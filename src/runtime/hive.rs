//! The hive: what all agents share.
//!
//! - **Peers**: which agents exist, their Discord user IDs, specialties,
//!   and which servers they are in — so each agent knows its teammates
//!   and never mistakes one for an ordinary bot.
//! - **Claims**: the first agent to claim a message for a purpose
//!   (ambient reply, moderation) wins, so a server full of agents does not
//!   answer the same chatter five times or double-punish a spammer.
//! - **Floors**: agents speaking in the same channel take turns, so the
//!   second one reads the first one's reply before answering.
//! - **Endpoint limits**: agents on the same LLM server share a
//!   concurrency limit (a local LM Studio serves one or two at a time).
//! - **Loop guard**: consecutive agent-to-agent messages in public
//!   channels are capped.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::live::Live;
use crate::config::GlobalSettings;
use crate::memory::Knowledge;
use crate::tools::mcp::McpManager;

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub specialty: String,
    pub user_id: u64,
    pub guilds: Vec<u64>,
    pub online: bool,
    pub moderates: bool,
    pub learns: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaimKind {
    AmbientReply,
    Moderation,
    HqSetup,
    TaskRun,
    Stats,
}

pub struct Hive {
    pub knowledge: Knowledge,
    pub live: Arc<Live>,
    pub settings: RwLock<GlobalSettings>,
    pub mcp: Arc<McpManager>,
    peers: RwLock<HashMap<String, Peer>>,
    claims: Mutex<HashMap<(u64, ClaimKind), (String, Instant)>>,
    floors: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    limits: Mutex<HashMap<String, (u32, Arc<Semaphore>)>>,
    agent_chain: Mutex<HashMap<u64, u32>>,
    busy: Mutex<HashSet<(u64, &'static str)>>,
}

impl Hive {
    pub fn new(live: Arc<Live>, settings: GlobalSettings) -> Arc<Self> {
        Arc::new(Self {
            knowledge: Knowledge::default(),
            live,
            mcp: Arc::new(McpManager::default()),
            settings: RwLock::new(settings),
            peers: RwLock::new(HashMap::new()),
            claims: Mutex::new(HashMap::new()),
            floors: Mutex::new(HashMap::new()),
            limits: Mutex::new(HashMap::new()),
            agent_chain: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
        })
    }

    pub fn settings(&self) -> GlobalSettings {
        self.settings.read().expect("settings poisoned").clone()
    }

    // --- peers ---------------------------------------------------------------

    pub fn upsert_peer(&self, peer: Peer) {
        self.peers
            .write()
            .expect("peers poisoned")
            .insert(peer.id.clone(), peer);
    }

    pub fn update_peer(&self, id: &str, f: impl FnOnce(&mut Peer)) {
        if let Some(peer) = self.peers.write().expect("peers poisoned").get_mut(id) {
            f(peer);
        }
    }

    pub fn remove_peer(&self, id: &str) {
        self.peers.write().expect("peers poisoned").remove(id);
    }

    pub fn peers(&self) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self
            .peers
            .read()
            .expect("peers poisoned")
            .values()
            .cloned()
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        peers
    }

    pub fn peer(&self, id: &str) -> Option<Peer> {
        self.peers.read().expect("peers poisoned").get(id).cloned()
    }

    /// The agent behind a Discord user ID, if it is one of ours.
    pub fn peer_by_user(&self, user_id: u64) -> Option<Peer> {
        if user_id == 0 {
            return None;
        }
        self.peers
            .read()
            .expect("peers poisoned")
            .values()
            .find(|p| p.user_id == user_id)
            .cloned()
    }

    pub fn peer_by_name(&self, name: &str) -> Option<Peer> {
        let name = name.trim().trim_start_matches('@').to_lowercase();
        self.peers
            .read()
            .expect("peers poisoned")
            .values()
            .find(|p| {
                p.name.to_lowercase() == name || p.aliases.iter().any(|a| a.to_lowercase() == name)
            })
            .cloned()
    }

    /// Online peers in a guild, ordered by user ID (a stable election
    /// order for "who does the shared chore").
    pub fn peers_in_guild(&self, guild_id: u64) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self
            .peers
            .read()
            .expect("peers poisoned")
            .values()
            .filter(|p| p.online && p.guilds.contains(&guild_id))
            .cloned()
            .collect();
        peers.sort_by_key(|p| p.user_id);
        peers
    }

    // --- claims --------------------------------------------------------------

    /// First caller wins; later callers for the same message/kind get
    /// `false`. The winning agent may re-claim (idempotent).
    pub fn claim(&self, message_id: u64, kind: ClaimKind, agent_id: &str) -> bool {
        let mut claims = self.claims.lock().expect("claims poisoned");
        if claims.len() > 4000 {
            claims.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(900));
        }
        match claims.get(&(message_id, kind)) {
            Some((owner, _)) => owner == agent_id,
            None => {
                claims.insert((message_id, kind), (agent_id.to_owned(), Instant::now()));
                true
            }
        }
    }

    /// Claim something exactly once, even for the same agent: for work
    /// that may be triggered twice (a forum post seen by the gateway and
    /// by a fetch).
    pub fn claim_once(&self, key: u64, kind: ClaimKind) -> bool {
        let mut claims = self.claims.lock().expect("claims poisoned");
        if claims.contains_key(&(key, kind)) {
            return false;
        }
        claims.insert((key, kind), (String::new(), Instant::now()));
        true
    }

    // --- floors --------------------------------------------------------------

    pub fn floor(&self, channel_id: u64) -> Arc<tokio::sync::Mutex<()>> {
        let mut floors = self.floors.lock().expect("floors poisoned");
        Arc::clone(
            floors
                .entry(channel_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    // --- endpoint limits -----------------------------------------------------

    pub async fn endpoint_permit(&self, key: &str) -> Option<OwnedSemaphorePermit> {
        let wanted = self.settings().endpoint_parallelism.clamp(1, 32);
        let semaphore = {
            let mut limits = self.limits.lock().expect("limits poisoned");
            let entry = limits
                .entry(key.to_owned())
                .or_insert_with(|| (wanted, Arc::new(Semaphore::new(wanted as usize))));
            if entry.0 != wanted {
                *entry = (wanted, Arc::new(Semaphore::new(wanted as usize)));
            }
            Arc::clone(&entry.1)
        };
        semaphore.acquire_owned().await.ok()
    }

    // --- loop guard ----------------------------------------------------------

    /// Record who spoke last in a channel; returns the run of consecutive
    /// agent messages.
    pub fn note_speaker(&self, channel_id: u64, is_agent: bool) -> u32 {
        let mut chain = self.agent_chain.lock().expect("chain poisoned");
        let count = chain.entry(channel_id).or_insert(0);
        if is_agent {
            *count += 1;
        } else {
            *count = 0;
        }
        *count
    }

    pub fn agent_chain(&self, channel_id: u64) -> u32 {
        self.agent_chain
            .lock()
            .expect("chain poisoned")
            .get(&channel_id)
            .copied()
            .unwrap_or(0)
    }

    // --- single-flight background jobs -------------------------------------

    /// Mark a background job (e.g. summarizing a channel) as running;
    /// `None` when another agent is already on it. The guard clears it.
    pub fn begin_job(self: &Arc<Self>, key: u64, job: &'static str) -> Option<JobGuard> {
        let mut busy = self.busy.lock().expect("busy poisoned");
        if busy.insert((key, job)) {
            Some(JobGuard {
                hive: Arc::clone(self),
                key: (key, job),
            })
        } else {
            None
        }
    }
}

pub struct JobGuard {
    hive: Arc<Hive>,
    key: (u64, &'static str),
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.hive
            .busy
            .lock()
            .expect("busy poisoned")
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hive() -> Arc<Hive> {
        Hive::new(Arc::new(Live::default()), GlobalSettings::default())
    }

    #[test]
    fn first_claim_wins() {
        let hive = hive();
        assert!(hive.claim(1, ClaimKind::AmbientReply, "a"));
        assert!(!hive.claim(1, ClaimKind::AmbientReply, "b"));
        assert!(hive.claim(1, ClaimKind::AmbientReply, "a"));
        assert!(hive.claim(1, ClaimKind::Moderation, "b"));
    }

    #[test]
    fn claim_once_is_once_even_for_the_same_agent() {
        let hive = hive();
        assert!(hive.claim_once(7, ClaimKind::TaskRun));
        assert!(!hive.claim_once(7, ClaimKind::TaskRun));
    }

    #[test]
    fn agent_chain_resets_on_humans() {
        let hive = hive();
        assert_eq!(hive.note_speaker(5, true), 1);
        assert_eq!(hive.note_speaker(5, true), 2);
        assert_eq!(hive.note_speaker(5, false), 0);
    }

    #[test]
    fn jobs_are_single_flight() {
        let hive = hive();
        let guard = hive.begin_job(9, "summary");
        assert!(guard.is_some());
        assert!(hive.begin_job(9, "summary").is_none());
        drop(guard);
        assert!(hive.begin_job(9, "summary").is_some());
    }
}
