//! Long-term memory and shared knowledge, persisted to disk.
//!
//! - [`MemoryStore`]: one per agent — facts, preferences, and notes the
//!   agent chose to remember, searchable by keywords and (optionally)
//!   embeddings.
//! - [`GuildKnowledge`]: shared by all agents in a server — culture notes,
//!   notes on people (with moderation strikes), standing directives with
//!   the rank of whoever gave them, quiet rules, the HQ channel, and the
//!   moderation log.
//! - [`ChannelArchive`]: shared per channel — the recent message history
//!   and a rolling summary of everything older (context compaction).
//!
//! Stores are in memory behind mutexes (never held across `.await`) and
//! flushed to disk by a background task when dirty.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Agent memories
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryKind {
    Fact,
    Preference,
    Event,
    Task,
    Note,
}

impl MemoryKind {
    pub fn parse(text: &str) -> Self {
        match text.trim().to_lowercase().as_str() {
            "preference" | "pref" | "likes" => MemoryKind::Preference,
            "event" => MemoryKind::Event,
            "task" | "todo" | "reminder" => MemoryKind::Task,
            "note" => MemoryKind::Note,
            _ => MemoryKind::Fact,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Event => "event",
            MemoryKind::Task => "task",
            MemoryKind::Note => "note",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: u64,
    pub text: String,
    pub kind: MemoryKind,
    #[serde(default)]
    pub about_user: Option<u64>,
    #[serde(default)]
    pub about_name: Option<String>,
    #[serde(default)]
    pub guild_id: Option<u64>,
    /// Who it came from ("told by Alice", "observed").
    #[serde(default)]
    pub source: String,
    pub created: i64,
    #[serde(default)]
    pub last_used: i64,
    #[serde(default)]
    pub uses: u32,
    /// 1–5.
    #[serde(default = "default_importance")]
    pub importance: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
}

fn default_importance() -> u8 {
    3
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoryFile {
    next_id: u64,
    memories: Vec<Memory>,
}

#[derive(Debug)]
pub struct MemoryStore {
    path: PathBuf,
    inner: Mutex<MemoryFile>,
    dirty: AtomicBool,
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "your", "with", "that", "this", "have", "has",
    "was", "were", "they", "them", "what", "when", "where", "who", "how", "why", "can", "could",
    "would", "should", "about", "just", "like", "from", "into", "will", "its", "it's", "his",
    "her", "she", "him", "our", "their", "there", "then", "than", "also", "any", "all", "some",
    "out", "get", "got", "does", "did", "doing", "been", "being", "i'm", "don't",
];

pub fn tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|word| word.trim_matches('\''))
        .filter(|word| word.chars().count() >= 3 && !STOPWORDS.contains(word))
        .map(|word| {
            // Light stemming so "games"/"gaming" meet "game".
            let word = word
                .strip_suffix("ing")
                .filter(|w| w.len() >= 3)
                .unwrap_or(word);
            let word = word
                .strip_suffix('s')
                .filter(|w| w.len() >= 3)
                .unwrap_or(word);
            word.to_owned()
        })
        .collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn similarity(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count() as f32;
    shared / (a.len().max(b.len()) as f32)
}

impl MemoryStore {
    pub fn load(path: PathBuf) -> Self {
        let file: MemoryFile = crate::paths::read_json(&path).unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(file),
            dirty: AtomicBool::new(false),
        }
    }

    fn mark(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Add a memory, or refresh a near-duplicate. Returns its ID and
    /// whether it was new.
    pub fn add(&self, mut memory: Memory) -> (u64, bool) {
        let mut inner = self.inner.lock().expect("memory poisoned");
        let new_tokens = tokens(&memory.text);
        if let Some(existing) = inner.memories.iter_mut().find(|m| {
            m.about_user == memory.about_user && similarity(&tokens(&m.text), &new_tokens) >= 0.7
        }) {
            existing.text = memory.text;
            existing.last_used = now_ms();
            existing.importance = existing.importance.max(memory.importance);
            if !memory.embedding.is_empty() {
                existing.embedding = memory.embedding;
            }
            let id = existing.id;
            drop(inner);
            self.mark();
            return (id, false);
        }
        inner.next_id += 1;
        memory.id = inner.next_id;
        let id = memory.id;
        inner.memories.push(memory);
        // Cap: drop the least valuable memories past 5000.
        if inner.memories.len() > 5000 {
            inner
                .memories
                .sort_by_key(|m| (m.importance, m.uses, m.last_used.max(m.created)));
            inner.memories.remove(0);
        }
        drop(inner);
        self.mark();
        (id, true)
    }

    pub fn remove(&self, id: u64) -> Option<Memory> {
        let mut inner = self.inner.lock().expect("memory poisoned");
        let index = inner.memories.iter().position(|m| m.id == id)?;
        let removed = inner.memories.remove(index);
        drop(inner);
        self.mark();
        Some(removed)
    }

    pub fn update_text(&self, id: u64, text: String) -> bool {
        let mut inner = self.inner.lock().expect("memory poisoned");
        let Some(memory) = inner.memories.iter_mut().find(|m| m.id == id) else {
            return false;
        };
        memory.text = text;
        memory.embedding.clear();
        drop(inner);
        self.mark();
        true
    }

    pub fn get(&self, id: u64) -> Option<Memory> {
        self.inner
            .lock()
            .expect("memory poisoned")
            .memories
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    pub fn all(&self) -> Vec<Memory> {
        self.inner.lock().expect("memory poisoned").memories.clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("memory poisoned").memories.len()
    }

    pub fn about_user(&self, user_id: u64) -> Vec<Memory> {
        self.inner
            .lock()
            .expect("memory poisoned")
            .memories
            .iter()
            .filter(|m| m.about_user == Some(user_id))
            .cloned()
            .collect()
    }

    pub fn forget_user(&self, user_id: u64) -> usize {
        let mut inner = self.inner.lock().expect("memory poisoned");
        let before = inner.memories.len();
        inner.memories.retain(|m| m.about_user != Some(user_id));
        let removed = before - inner.memories.len();
        drop(inner);
        if removed > 0 {
            self.mark();
        }
        removed
    }

    pub fn set_embedding(&self, id: u64, embedding: Vec<f32>) {
        let mut inner = self.inner.lock().expect("memory poisoned");
        if let Some(memory) = inner.memories.iter_mut().find(|m| m.id == id) {
            memory.embedding = embedding;
            drop(inner);
            self.mark();
        }
    }

    /// Memories without embeddings (for backfilling).
    pub fn missing_embeddings(&self, limit: usize) -> Vec<(u64, String)> {
        self.inner
            .lock()
            .expect("memory poisoned")
            .memories
            .iter()
            .filter(|m| m.embedding.is_empty())
            .take(limit)
            .map(|m| (m.id, m.text.clone()))
            .collect()
    }

    /// Rank memories for a query. Memories about the people in the
    /// conversation get a boost; so do important and recent ones.
    pub fn search(
        &self,
        query: &str,
        participants: &[u64],
        guild_id: Option<u64>,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Vec<Memory> {
        let mut inner = self.inner.lock().expect("memory poisoned");
        let total = inner.memories.len().max(1) as f32;
        let query_tokens = tokens(query);
        let mut document_frequency: HashMap<&str, usize> = HashMap::new();
        let memory_tokens: Vec<HashSet<String>> =
            inner.memories.iter().map(|m| tokens(&m.text)).collect();
        for set in &memory_tokens {
            for token in set {
                if query_tokens.contains(token) {
                    *document_frequency.entry(token.as_str()).or_insert(0) += 1;
                }
            }
        }
        let now = now_ms();
        let mut scored: Vec<(f32, usize)> = inner
            .memories
            .iter()
            .enumerate()
            .filter(|(_, m)| m.guild_id.is_none() || guild_id.is_none() || m.guild_id == guild_id)
            .map(|(index, memory)| {
                let mut score = 0.0f32;
                for token in query_tokens.intersection(&memory_tokens[index]) {
                    let df = *document_frequency.get(token.as_str()).unwrap_or(&1) as f32;
                    score += (1.0 + total / (1.0 + df)).ln();
                }
                score /= (memory_tokens[index].len().max(1) as f32).sqrt().max(1.0);
                if let (Some(q), false) = (query_embedding, memory.embedding.is_empty()) {
                    score += cosine(q, &memory.embedding).max(0.0) * 4.0;
                }
                let about_participant = memory
                    .about_user
                    .is_some_and(|user| participants.contains(&user));
                if about_participant {
                    score += 1.5;
                }
                if score > 0.0 {
                    let age_days = ((now - memory.last_used.max(memory.created)) as f32
                        / 86_400_000.0)
                        .max(0.0);
                    score += (memory.importance as f32) * 0.15;
                    score += 0.5 * (-age_days / 30.0).exp();
                }
                (score, index)
            })
            .filter(|(score, _)| *score > 0.35)
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let picked: Vec<usize> = scored.into_iter().take(limit).map(|(_, i)| i).collect();
        let mut results = Vec::with_capacity(picked.len());
        for index in picked {
            let memory = &mut inner.memories[index];
            memory.uses += 1;
            memory.last_used = now;
            results.push(memory.clone());
        }
        results
    }

    pub fn save_if_dirty(&self) {
        if self.dirty.swap(false, Ordering::Relaxed) {
            let snapshot = {
                let inner = self.inner.lock().expect("memory poisoned");
                serde_json::to_vec(&*inner)
            };
            match snapshot {
                Ok(bytes) => {
                    if let Err(error) = crate::paths::write_atomic(&self.path, &bytes) {
                        tracing::error!(%error, "memory save failed");
                        self.mark();
                    }
                }
                Err(error) => tracing::error!(%error, "memory serialization failed"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared guild knowledge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Strike {
    pub at: i64,
    pub reason: String,
    pub by: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonNote {
    pub name: String,
    pub notes: String,
    pub updated: i64,
    pub strikes: Vec<Strike>,
    /// The person asked agents not to keep notes about them.
    pub opted_out: bool,
}

impl PersonNote {
    pub fn recent_strikes(&self, window_ms: i64) -> usize {
        let cutoff = now_ms() - window_ms;
        self.strikes.iter().filter(|s| s.at >= cutoff).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectiveScope {
    Server,
    Channel(u64),
    /// How to treat one person (set by that person or above).
    Personal(u64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Directive {
    pub id: u64,
    pub text: String,
    pub author_id: u64,
    pub author_name: String,
    pub author_rank: u32,
    pub author_level: crate::config::AccessLevel,
    pub scope: DirectiveScope,
    /// Agent ID this applies to; `None` = every agent.
    #[serde(default)]
    pub target_agent: Option<String>,
    pub created: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuietRule {
    pub channel_id: Option<u64>,
    /// Quiet towards one person ("stop replying to me").
    pub user_id: Option<u64>,
    /// Agent ID; `None` = all agents.
    pub agent_id: Option<String>,
    pub setter_rank: u32,
    pub until: i64,
}

/// A slash command an agent made in a server when asked.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCommand {
    pub name: String,
    pub description: String,
    /// The reply, or the instruction (Agent mode).
    pub text: String,
    pub mode: crate::config::CommandMode,
    pub private: bool,
    pub takes_input: bool,
    /// Turned off (kept, but not registered).
    pub disabled: bool,
    pub agent_id: String,
    pub author_id: u64,
    pub author_name: String,
    pub created: i64,
}

/// A channel where the agent is told to be chill: minimal, brief, and it
/// cleans up after its own chatty stretches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChillRule {
    /// A channel, or a category (covers every channel in it).
    pub channel_id: u64,
    /// Agent ID; `None` = all agents.
    pub agent_id: Option<String>,
    pub setter_id: u64,
    pub setter_name: String,
}

/// "Never chat in #x": no replies, no chiming in, no posts there at all —
/// until someone ranked at least as high lifts it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoChatRule {
    /// A channel, or a category (covers every channel in it).
    pub channel_id: u64,
    /// Agent ID; `None` = all agents.
    pub agent_id: Option<String>,
    pub setter_id: u64,
    pub setter_name: String,
    pub setter_rank: u32,
    pub created: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HqRequest {
    pub channel_id: u64,
    pub message_id: u64,
    pub agent_id: String,
    pub asked_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HqState {
    pub channel_id: Option<u64>,
    pub category_id: Option<u64>,
    /// Where every agent action is logged.
    #[serde(default)]
    pub activity_channel_id: Option<u64>,
    /// Last "something happened here from outside" notice (no HQ yet).
    #[serde(default)]
    pub last_notice: i64,
    pub request: Option<HqRequest>,
    /// Don't ask again before this time (after a "no").
    pub declined_until: i64,
    /// Admins said: make it yourselves whenever you can.
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModAction {
    pub at: i64,
    pub agent: String,
    pub user_id: u64,
    pub user_name: String,
    pub action: String,
    pub reason: String,
    pub channel_id: u64,
}

/// What makes a standing task run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskTrigger {
    /// Every new post (thread) in a forum channel.
    NewPost { channel_id: u64 },
    /// Every new message from a person in a channel.
    NewMessage { channel_id: u64 },
    /// On a timer — every N minutes, or at a time of day (local time) on
    /// some weekdays; output goes to the channel.
    Schedule {
        channel_id: u64,
        every_minutes: u32,
        /// Minutes after local midnight ("every day at 09:00").
        #[serde(default)]
        at_minute: Option<u32>,
        /// Weekdays, 0 = Monday … 6 = Sunday; empty = every day.
        #[serde(default)]
        days: Vec<u8>,
    },
    /// Once, at a moment (then removed).
    Once { channel_id: u64, at: i64 },
    /// New releases or commits on a GitHub repo; output goes to the
    /// channel.
    GitHub {
        channel_id: u64,
        /// "owner/name".
        repo: String,
        /// "releases" or "commits".
        kind: String,
        /// For commits.
        #[serde(default)]
        branch: String,
        /// The newest release id / commit sha already handled.
        #[serde(default)]
        last_seen: String,
        #[serde(default)]
        etag: String,
        /// Last check (ms).
        #[serde(default)]
        checked: i64,
    },
    /// When someone joins the server; output goes to the channel.
    MemberJoin { channel_id: u64 },
    /// Every new ticket in the agent's ticket system (`channel_id` is the
    /// tickets category); runs inside the new ticket.
    NewTicket { channel_id: u64 },
    /// A one-time reminder for someone (sent as-is, then removed).
    Reminder {
        channel_id: u64,
        at: i64,
        user_id: u64,
    },
}

impl TaskTrigger {
    pub fn channel_id(&self) -> u64 {
        match self {
            TaskTrigger::NewPost { channel_id }
            | TaskTrigger::NewMessage { channel_id }
            | TaskTrigger::Schedule { channel_id, .. }
            | TaskTrigger::MemberJoin { channel_id }
            | TaskTrigger::NewTicket { channel_id }
            | TaskTrigger::Once { channel_id, .. }
            | TaskTrigger::GitHub { channel_id, .. }
            | TaskTrigger::Reminder { channel_id, .. } => *channel_id,
        }
    }

    pub fn describe(&self, channel_name: &str) -> String {
        match self {
            TaskTrigger::NewPost { .. } => format!("every new post in #{channel_name}"),
            TaskTrigger::NewMessage { .. } => format!("every new message in #{channel_name}"),
            TaskTrigger::Schedule {
                every_minutes,
                at_minute: Some(minute),
                days,
                ..
            } => {
                let when = if !days.is_empty() {
                    format!("every {}", day_names(days))
                } else if *every_minutes >= 2880 {
                    format!("every {} days", every_minutes / 1440)
                } else {
                    "every day".to_owned()
                };
                format!(
                    "{when} at {:02}:{:02} in #{channel_name}",
                    minute / 60,
                    minute % 60
                )
            }
            TaskTrigger::Once { at, .. } => {
                format!("once, at <t:{}:f> in #{channel_name}", at / 1000)
            }
            TaskTrigger::GitHub {
                repo, kind, branch, ..
            } => {
                if kind == "commits" {
                    format!("every new commit to github.com/{repo} ({branch}), in #{channel_name}")
                } else {
                    format!("every new release of github.com/{repo}, in #{channel_name}")
                }
            }
            TaskTrigger::Schedule { every_minutes, .. } => {
                if every_minutes % 1440 == 0 {
                    format!("every {} day(s) in #{channel_name}", every_minutes / 1440)
                } else if every_minutes % 60 == 0 {
                    format!("every {} hour(s) in #{channel_name}", every_minutes / 60)
                } else {
                    format!("every {every_minutes} minutes in #{channel_name}")
                }
            }
            TaskTrigger::MemberJoin { .. } => {
                format!("when someone joins (posting in #{channel_name})")
            }
            TaskTrigger::NewTicket { .. } => "every new ticket (inside the ticket)".to_owned(),
            TaskTrigger::Reminder { at, user_id, .. } => {
                format!(
                    "reminder for <@{user_id}> at <t:{}:f> in #{channel_name}",
                    at / 1000
                )
            }
        }
    }
}

const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// "Mon, Wed, Fri" / "weekdays" / "weekends".
pub fn day_names(days: &[u8]) -> String {
    let mut sorted = days.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    match sorted.as_slice() {
        [0, 1, 2, 3, 4] => "weekday".to_owned(),
        [5, 6] => "weekend day".to_owned(),
        _ => sorted
            .iter()
            .filter_map(|d| DAY_NAMES.get(*d as usize))
            .copied()
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// "9am", "21:30", "9:15 pm", "noon" → minutes after midnight.
pub fn parse_time_of_day(text: &str) -> Option<u32> {
    let t = text.trim().to_lowercase().replace('.', "");
    match t.as_str() {
        "noon" | "midday" => return Some(720),
        "midnight" => return Some(0),
        _ => {}
    }
    let (body, half) = if let Some(b) = t.strip_suffix("am") {
        (b.trim(), Some(false))
    } else if let Some(b) = t.strip_suffix("pm") {
        (b.trim(), Some(true))
    } else {
        (t.as_str(), None)
    };
    let mut parts = body.split([':', 'h']);
    let hour: u32 = parts.next()?.trim().parse().ok()?;
    let minute: u32 = match parts.next().map(str::trim) {
        Some("") | None => 0,
        Some(m) => m.parse().ok()?,
    };
    let hour = match half {
        Some(true) if hour < 12 => hour + 12,
        Some(false) if hour == 12 => 0,
        _ => hour,
    };
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

/// "mon, wed", "weekdays", "weekends", "every day" → weekday numbers
/// (empty = every day); `None` when nothing is recognizable.
pub fn parse_days(text: &str) -> Option<Vec<u8>> {
    let t = text.trim().to_lowercase();
    if t.is_empty() || ["every day", "everyday", "daily", "all", "each day"].contains(&t.as_str()) {
        return Some(Vec::new());
    }
    if t.contains("weekday") {
        return Some(vec![0, 1, 2, 3, 4]);
    }
    if t.contains("weekend") {
        return Some(vec![5, 6]);
    }
    let mut days: Vec<u8> = t
        .split(|c: char| !c.is_alphabetic())
        .filter_map(|w| {
            let w: String = w.chars().take(3).collect();
            DAY_NAMES
                .iter()
                .position(|d| d.to_lowercase() == w)
                .map(|i| i as u8)
        })
        .collect();
    days.sort_unstable();
    days.dedup();
    (!days.is_empty()).then_some(days)
}

/// Is a time-of-day schedule due? (At most once per day, after its time,
/// not more than 6 hours late.)
#[cfg(test)]
mod standing_task_tests {
    use super::*;

    #[test]
    fn old_task_json_defaults_scope() {
        let json = r#"{
            "id": 1, "agent_id": "a", "instruction": "x",
            "trigger": { "NewMessage": { "channel_id": 5 } },
            "author_id": 2, "author_name": "u", "author_rank": 0,
            "author_level": "Everyone", "created": 0, "active": true
        }"#;
        let task: StandingTask = serde_json::from_str(json).expect("old task loads");
        assert_eq!(task.scope, TaskScope::Origin);
    }
}

pub fn schedule_due(now: i64, last_run: i64, minute: u32, days: &[u8], every_minutes: u32) -> bool {
    use chrono::{Datelike, Local, TimeZone};
    let Some(local) = Local.timestamp_millis_opt(now).single() else {
        return false;
    };
    let today = local.date_naive();
    if !days.is_empty() && !days.contains(&(today.weekday().num_days_from_monday() as u8)) {
        return false;
    }
    let Some(target) = today
        .and_hms_opt(minute / 60, minute % 60, 0)
        .and_then(|t| Local.from_local_datetime(&t).earliest())
        .map(|t| t.timestamp_millis())
    else {
        return false;
    };
    // "Every 2 days at 9": also wait out the interval.
    let spaced = every_minutes < 2880 || now - last_run >= (every_minutes as i64 - 60) * 60_000;
    now >= target && last_run < target && now - target < 6 * 3_600_000 && spaced
}

/// The next moment a local time of day comes around (today, or tomorrow
/// if it already passed), or on a given date ("2026-11-05").
pub fn next_time_at(now: i64, minute: u32, date: Option<&str>) -> Option<i64> {
    use chrono::{Local, NaiveDate, TimeZone};
    let local = Local.timestamp_millis_opt(now).single()?;
    let day = match date {
        Some(d) => NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok()?,
        None => local.date_naive(),
    };
    let at = |day: NaiveDate| {
        day.and_hms_opt(minute / 60, minute % 60, 0)
            .and_then(|t| Local.from_local_datetime(&t).earliest())
            .map(|t| t.timestamp_millis())
    };
    let first = at(day)?;
    if date.is_none() && first <= now {
        return at(day.succ_opt()?);
    }
    Some(first)
}

/// Which channels a standing task watches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum TaskScope {
    /// Just the channel named by the trigger.
    #[default]
    Origin,
    /// A specific list of channels (the trigger channel plus these).
    Channels(Vec<u64>),
    /// Every channel in the server.
    Global,
}

/// A duty someone gave an agent that it keeps doing until told to stop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StandingTask {
    pub id: u64,
    pub agent_id: String,
    pub instruction: String,
    pub trigger: TaskTrigger,
    pub author_id: u64,
    pub author_name: String,
    pub author_rank: u32,
    pub author_level: crate::config::AccessLevel,
    pub created: i64,
    pub active: bool,
    #[serde(default)]
    pub runs: u32,
    #[serde(default)]
    pub last_run: i64,
    /// What the agent last did for this task (shown when asked).
    #[serde(default)]
    pub last_result: String,
    /// Where it was set up (problems are reported there).
    #[serde(default)]
    pub origin_channel: u64,
    /// Which channels the trigger watches (default: just its own).
    #[serde(default)]
    pub scope: TaskScope,
}

/// How much the agents may do without a human clicking a confirmation
/// button. Set per server with /mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationMode {
    /// Big or irreversible actions post Approve/Cancel buttons first.
    #[default]
    Standard,
    /// Actions run immediately; only mass operations still confirm.
    Autonomous,
}

impl OperationMode {
    pub fn label(self) -> &'static str {
        match self {
            OperationMode::Standard => "standard",
            OperationMode::Autonomous => "autonomous",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuildKnowledge {
    pub tasks: Vec<StandingTask>,
    pub next_task: u64,
    pub guild_id: u64,
    pub guild_name: String,
    pub culture: String,
    pub culture_updated: i64,
    pub rules: String,
    pub rules_updated: i64,
    pub people: BTreeMap<u64, PersonNote>,
    pub directives: Vec<Directive>,
    pub next_directive: u64,
    pub quiet: Vec<QuietRule>,
    /// Channels the agents never chat in.
    pub no_chat: Vec<NoChatRule>,
    /// Channels where the agents stay chill (extra brief, self-pruning).
    #[serde(default)]
    pub chill: Vec<ChillRule>,
    /// Slash commands agents made here.
    pub commands: Vec<ServerCommand>,
    /// Built-in commands turned off in this server.
    pub disabled_commands: Vec<String>,
    pub hq: HqState,
    pub messages_since_reflect: u32,
    pub mod_log: VecDeque<ModAction>,
    pub tickets: TicketSystem,
    pub giveaways: Vec<Giveaway>,
    #[serde(default)]
    pub votes: Vec<Vote>,
    /// Who may change the agents' server settings (set by the owner).
    pub settings_access: SettingsAccess,
    /// Whether big/irreversible actions need a confirmation button.
    pub operation_mode: OperationMode,
    pub leveling: Leveling,
    pub stats: ServerStats,
    /// Conversations being translated.
    pub translations: Vec<crate::runtime::translate::TranslationSession>,
    /// What other servers (and DMs) may do with this one.
    pub privacy: Privacy,
}

/// Cross-server privacy: what requests from outside this server (another
/// server, or a DM) may do here. All allowed by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Privacy {
    /// Find this server's members by name.
    pub member_search: bool,
    /// Post messages in this server's channels.
    pub outside_messages: bool,
    /// Search this server's audit log.
    pub audit_search: bool,
    /// DM this server's members (found through it).
    pub dms: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            member_search: true,
            outside_messages: true,
            audit_search: true,
            dms: true,
        }
    }
}

/// Who may change server settings. Empty = admins; once the owner names
/// roles or people, only they (and the owner) may.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsAccess {
    pub roles: Vec<u64>,
    pub users: Vec<u64>,
}

impl SettingsAccess {
    pub fn is_restricted(&self) -> bool {
        !self.roles.is_empty() || !self.users.is_empty()
    }
}

/// XP and levels for messages and voice time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Leveling {
    pub enabled: bool,
    pub paused: bool,
    pub xp_min: u32,
    pub xp_max: u32,
    /// Seconds between messages that earn XP.
    pub cooldown_secs: u32,
    /// XP per minute in voice with someone else.
    pub voice_xp_per_minute: u32,
    /// same, channel, dm, or off.
    pub announce: String,
    pub announce_channel: u64,
    /// {user}, {level}, {name}.
    pub message: String,
    pub rewards: Vec<LevelReward>,
    /// Keep lower reward roles when a higher one is earned.
    pub stack_rewards: bool,
    pub ignored_channels: Vec<u64>,
    pub card: RankCard,
    /// Personal rank cards (fall back to `card`), keyed by user id.
    #[serde(default)]
    pub card_overrides: HashMap<u64, RankCard>,
    /// Personal level-up messages ({user}, {name}, {level}), keyed by user id.
    #[serde(default)]
    pub message_overrides: HashMap<u64, String>,
    /// Personal announce targets (same, channel, dm, off), keyed by user id.
    #[serde(default)]
    pub announce_overrides: HashMap<u64, String>,
    pub members: BTreeMap<u64, LevelEntry>,
}

impl Default for Leveling {
    fn default() -> Self {
        Self {
            enabled: true,
            paused: false,
            xp_min: 15,
            xp_max: 25,
            cooldown_secs: 60,
            voice_xp_per_minute: 3,
            announce: "same".to_owned(),
            announce_channel: 0,
            message: "🎉 {user} reached **level {level}**!".to_owned(),
            rewards: Vec::new(),
            stack_rewards: true,
            ignored_channels: Vec::new(),
            card: RankCard::default(),
            card_overrides: HashMap::new(),
            message_overrides: HashMap::new(),
            announce_overrides: HashMap::new(),
            members: BTreeMap::new(),
        }
    }
}

impl Leveling {
    /// XP to go from `level` to the next (the familiar 5l² + 50l + 100).
    pub fn xp_for_next(level: u32) -> u64 {
        let l = level as u64;
        5 * l * l + 50 * l + 100
    }

    /// (level, XP into it, XP the level needs) for a total.
    pub fn progress(total: u64) -> (u32, u64, u64) {
        let mut level = 0;
        let mut left = total;
        while left >= Self::xp_for_next(level) {
            left -= Self::xp_for_next(level);
            level += 1;
        }
        (level, left, Self::xp_for_next(level))
    }

    /// 1-based position by XP.
    pub fn rank_of(&self, user_id: u64) -> Option<usize> {
        let mine = self.members.get(&user_id)?.xp;
        Some(self.members.values().filter(|e| e.xp > mine).count() + 1)
    }

    /// This person's rank card: their own if they designed one, else the
    /// server's.
    pub fn card_for(&self, user_id: u64) -> &RankCard {
        self.card_overrides.get(&user_id).unwrap_or(&self.card)
    }

    /// This person's level-up message: their own if they set one, else the
    /// server's.
    pub fn message_for(&self, user_id: u64) -> &str {
        self.message_overrides
            .get(&user_id)
            .map(String::as_str)
            .unwrap_or(&self.message)
    }

    /// Where this person's level-up goes (same, channel, dm, off): their
    /// own choice if they made one, else the server's.
    pub fn announce_for(&self, user_id: u64) -> &str {
        self.announce_overrides
            .get(&user_id)
            .map(String::as_str)
            .unwrap_or(&self.announce)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelReward {
    pub level: u32,
    pub role_id: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelEntry {
    pub xp: u64,
    pub messages: u64,
    pub voice_minutes: u64,
    pub last_xp: i64,
    pub name: String,
}

/// How rank cards look. Every part can be styled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RankCard {
    /// classic, mirrored, centered, or compact.
    pub layout: String,
    /// classic (dark panel), minimal, bold (accent stripe), or glass.
    pub style: String,
    pub background: String,
    /// A second color for a gradient ("" = solid).
    pub background2: String,
    /// horizontal, vertical, or diagonal.
    pub gradient: String,
    /// An image behind everything (cover-fit), optional.
    pub background_url: String,
    /// 1-100 (0 = fully opaque).
    pub image_opacity: u8,
    /// Blur radius for the image, 0-40.
    pub blur: u8,
    /// -100 to 100.
    pub image_brightness: i32,
    /// Darkening over the image (0-100).
    pub overlay: u8,
    /// The panel's opacity (0 = default).
    pub panel_opacity: u8,
    pub accent: String,
    pub text: String,
    pub muted: String,
    /// Per-element colors ("" = from text/accent/muted).
    pub name_color: String,
    pub rank_color: String,
    pub level_color: String,
    pub xp_color: String,
    pub bar_color: String,
    pub bar_track: String,
    /// A font name (inter, poppins, montserrat, bebas neue, oswald,
    /// orbitron, press start 2p, pacifico, lobster, nunito, playfair
    /// display, rubik, roboto mono, mono, default) or a .ttf URL.
    pub font: String,
    /// light, regular, or bold.
    pub font_weight: String,
    pub text_scale: f32,
    /// circle, rounded, or square.
    pub avatar_shape: String,
    pub avatar_ring: bool,
    pub ring_color: String,
    pub bar_height: u32,
    /// round or square.
    pub bar_shape: String,
    pub show_avatar: bool,
    pub show_rank: bool,
    pub show_level: bool,
    pub show_xp: bool,
    pub show_total: bool,
    pub show_bar: bool,
    /// below_bar, above_bar, with_xp, or top.
    pub total_position: String,
    pub rank_label: String,
    pub level_label: String,
    /// Rounded card corners in pixels.
    pub corner_radius: u32,
}

impl Default for RankCard {
    fn default() -> Self {
        Self {
            layout: "classic".to_owned(),
            style: "classic".to_owned(),
            background: "#1e1f22".to_owned(),
            background2: String::new(),
            gradient: "horizontal".to_owned(),
            background_url: String::new(),
            image_opacity: 100,
            blur: 0,
            image_brightness: 0,
            overlay: 45,
            panel_opacity: 0,
            accent: "#5865f2".to_owned(),
            text: "#ffffff".to_owned(),
            muted: "#b5bac1".to_owned(),
            name_color: String::new(),
            rank_color: String::new(),
            level_color: String::new(),
            xp_color: String::new(),
            bar_color: String::new(),
            bar_track: String::new(),
            font: "default".to_owned(),
            font_weight: "regular".to_owned(),
            text_scale: 1.0,
            avatar_shape: "circle".to_owned(),
            avatar_ring: true,
            ring_color: String::new(),
            bar_height: 0,
            bar_shape: "round".to_owned(),
            show_avatar: true,
            show_rank: true,
            show_level: true,
            show_xp: true,
            show_total: true,
            show_bar: true,
            total_position: "below_bar".to_owned(),
            rank_label: "RANK".to_owned(),
            level_label: "LEVEL".to_owned(),
            corner_radius: 0,
        }
    }
}

/// Server statistics collected over time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerStats {
    pub enabled: bool,
    pub paused: bool,
    pub track_members: bool,
    pub track_messages: bool,
    pub track_voice: bool,
    pub track_joins: bool,
    /// "2026-11-01" → that day.
    pub days: BTreeMap<String, DayStats>,
}

impl Default for ServerStats {
    fn default() -> Self {
        Self {
            enabled: true,
            paused: false,
            track_members: true,
            track_messages: true,
            track_voice: true,
            track_joins: true,
            days: BTreeMap::new(),
        }
    }
}

impl ServerStats {
    pub fn collecting(&self) -> bool {
        self.enabled && !self.paused
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DayStats {
    pub members: u64,
    pub joins: u32,
    pub leaves: u32,
    pub messages: u32,
    /// People who talked (or sat in voice) that day.
    pub active: Vec<u64>,
    pub voice_minutes: u32,
    pub channels: BTreeMap<u64, u32>,
    /// Messages by UTC hour.
    pub hours: Vec<u32>,
}

/// A giveaway with an Enter button; drawn when it ends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Giveaway {
    pub agent_id: String,
    pub channel_id: u64,
    pub message_id: u64,
    pub prize: String,
    pub host_id: u64,
    pub ends: i64,
    pub winners: u32,
    /// Only members with this role may enter (0 = anyone).
    pub required_role: u64,
    pub entrants: Vec<u64>,
    pub ended: bool,
    pub picked: Vec<u64>,
}

/// An interactive button-based vote or poll in a Discord channel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Vote {
    pub agent_id: String,
    pub channel_id: u64,
    pub message_id: u64,
    pub question: String,
    pub options: Vec<String>,
    pub host_id: u64,
    /// Unix timestamp in millis when this vote ends.
    pub ends: i64,
    /// Anonymous voting: votes are secret and only revealed when the vote ends.
    pub anonymous: bool,
    /// Map of user_id -> option_index (0-based).
    pub votes: HashMap<u64, usize>,
    pub ended: bool,
}

impl Vote {
    pub fn tally(&self) -> Vec<usize> {
        let mut counts = vec![0; self.options.len()];
        for &opt in self.votes.values() {
            if opt < counts.len() {
                counts[opt] += 1;
            }
        }
        counts
    }
}

/// A support-ticket system an agent set up (like Ticket Tool): a panel
/// with a button or a dropdown of ticket types, and a private channel per
/// ticket, with transcripts kept when tickets close.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketSystem {
    /// The agent that runs it (its bot owns the panel's components).
    pub agent_id: String,
    pub category_id: u64,
    pub panel_channel_id: u64,
    pub panel_message_id: u64,
    pub panel: TicketPanel,
    /// Roles that see every ticket.
    pub staff_roles: Vec<u64>,
    /// Where transcripts are posted when tickets close (0 = nowhere).
    pub log_channel_id: u64,
    /// DM the opener their transcript when their ticket closes.
    pub dm_transcripts: bool,
    /// The agent answers the opener in their ticket.
    pub auto_reply: bool,
    /// Open tickets one person may have at once.
    pub max_open: u32,
    pub next: u32,
    pub tickets: Vec<Ticket>,
}

/// How the panel looks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketPanel {
    pub title: String,
    pub message: String,
    /// Embed color.
    pub color: u32,
    pub button_label: String,
    pub button_emoji: String,
    /// primary, secondary, success, danger.
    pub button_style: String,
    /// With options, the panel shows a dropdown instead of a button.
    pub options: Vec<TicketOption>,
    /// What the ticket's first message says ({user}, {number}, {type}).
    pub welcome: String,
}

impl Default for TicketPanel {
    fn default() -> Self {
        Self {
            title: "Support".to_owned(),
            message: "Need help? Open a ticket — only you, staff, and I can see it.".to_owned(),
            color: 0x5865F2,
            button_label: "Open a ticket".to_owned(),
            button_emoji: "🎫".to_owned(),
            button_style: "primary".to_owned(),
            options: Vec::new(),
            welcome: "🎫 **Ticket #{number}** — {user}{type}".to_owned(),
        }
    }
}

/// A ticket type in the panel's dropdown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketOption {
    pub label: String,
    pub emoji: String,
    pub description: String,
    /// Extra instructions for the agent on tickets of this type.
    pub instruction: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ticket {
    pub number: u32,
    pub channel_id: u64,
    pub user_id: u64,
    pub user_name: String,
    /// The dropdown option it was opened with.
    pub kind: String,
    pub opened: i64,
    pub closed: Option<i64>,
    pub close_reason: String,
    pub closed_by: String,
    pub claimed_by: Option<u64>,
    /// Transcript files saved for it (one per close).
    pub transcripts: Vec<String>,
    /// A short summary, written when it closes.
    pub summary: String,
}

impl TicketSystem {
    pub fn is_set_up(&self) -> bool {
        self.category_id != 0
    }

    pub fn open_ticket_in(&self, channel_id: u64) -> Option<&Ticket> {
        self.tickets
            .iter()
            .find(|t| t.channel_id == channel_id && t.closed.is_none())
    }

    pub fn ticket_in(&self, channel_id: u64) -> Option<&Ticket> {
        self.tickets
            .iter()
            .rev()
            .find(|t| t.channel_id == channel_id)
    }

    pub fn by_number(&self, number: u32) -> Option<&Ticket> {
        self.tickets.iter().find(|t| t.number == number)
    }
}

impl GuildKnowledge {
    pub fn directives_for(
        &self,
        agent_id: &str,
        channel_id: u64,
        participants: &[u64],
    ) -> Vec<&Directive> {
        let mut out: Vec<&Directive> = self
            .directives
            .iter()
            .filter(|d| d.target_agent.as_deref().is_none_or(|a| a == agent_id))
            .filter(|d| match d.scope {
                DirectiveScope::Server => true,
                DirectiveScope::Channel(c) => c == channel_id,
                DirectiveScope::Personal(u) => participants.contains(&u),
            })
            .collect();
        out.sort_by(|a, b| b.author_rank.cmp(&a.author_rank));
        out
    }

    /// The active quiet rule that silences `agent_id` here, if any.
    pub fn quiet_rule(&self, agent_id: &str, channel_id: u64, user_id: u64) -> Option<&QuietRule> {
        let now = now_ms();
        self.quiet.iter().find(|rule| {
            rule.until > now
                && rule.agent_id.as_deref().is_none_or(|a| a == agent_id)
                && rule.channel_id.is_none_or(|c| c == channel_id)
                && rule.user_id.is_none_or(|u| u == user_id)
        })
    }

    pub fn log_mod_action(&mut self, action: ModAction) {
        self.mod_log.push_back(action);
        while self.mod_log.len() > 300 {
            self.mod_log.pop_front();
        }
    }
}

// ---------------------------------------------------------------------------
// Channel archives
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: u64,
}

impl AttachmentInfo {
    pub fn is_image(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("image/"))
            || [".png", ".jpg", ".jpeg", ".gif", ".webp"]
                .iter()
                .any(|ext| self.name.to_lowercase().ends_with(ext))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: u64,
    pub author_id: u64,
    pub author: String,
    #[serde(default)]
    pub author_bot: bool,
    pub content: String,
    pub ts: i64,
    #[serde(default)]
    pub reply_to: Option<u64>,
    #[serde(default)]
    pub reply_author: Option<String>,
    #[serde(default)]
    pub reply_snippet: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentInfo>,
    #[serde(default)]
    pub edited: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub reactions: Vec<(String, u32)>,
}

pub const ARCHIVE_CAP: usize = 400;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelArchive {
    pub channel_id: u64,
    pub guild_id: u64,
    pub name: String,
    /// Messages at or below this id were already folded by chill mode
    /// (so a burst is never compacted twice).
    #[serde(default)]
    pub chill_upto: u64,
    /// Rolling summary of everything up to `summary_upto`.
    pub summary: String,
    pub summary_upto: u64,
    pub messages: VecDeque<StoredMessage>,
}

impl ChannelArchive {
    pub fn push(&mut self, message: StoredMessage) -> bool {
        if self
            .messages
            .iter()
            .rev()
            .take(50)
            .any(|m| m.id == message.id)
        {
            return false;
        }
        // Keep order by ID (snowflakes are time-ordered).
        let position = self
            .messages
            .iter()
            .rposition(|m| m.id < message.id)
            .map(|p| p + 1)
            .unwrap_or(0);
        self.messages.insert(position, message);
        while self.messages.len() > ARCHIVE_CAP {
            self.messages.pop_front();
        }
        true
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut StoredMessage> {
        self.messages.iter_mut().rev().find(|m| m.id == id)
    }

    /// Messages newer than the summary that are not deleted.
    pub fn unsummarized(&self) -> impl Iterator<Item = &StoredMessage> {
        self.messages
            .iter()
            .filter(move |m| m.id > self.summary_upto && !m.deleted)
    }
}

// ---------------------------------------------------------------------------
// The shared store
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Knowledge {
    guilds: Mutex<HashMap<u64, GuildKnowledge>>,
    channels: Mutex<HashMap<u64, ChannelArchive>>,
    dirty_guilds: Mutex<HashSet<u64>>,
    dirty_channels: Mutex<HashSet<u64>>,
}

fn guild_path(guild_id: u64) -> PathBuf {
    crate::paths::shared_dir().join(format!("guild_{guild_id}.json"))
}

fn channel_path(channel_id: u64) -> PathBuf {
    crate::paths::shared_dir()
        .join("channels")
        .join(format!("{channel_id}.json"))
}

impl Knowledge {
    /// Read or modify a guild's knowledge (loaded lazily from disk).
    pub fn guild<R>(&self, guild_id: u64, f: impl FnOnce(&mut GuildKnowledge) -> R) -> R {
        let mut guilds = self.guilds.lock().expect("knowledge poisoned");
        let entry = guilds.entry(guild_id).or_insert_with(|| {
            crate::paths::read_json(&guild_path(guild_id)).unwrap_or(GuildKnowledge {
                guild_id,
                ..GuildKnowledge::default()
            })
        });
        f(entry)
    }

    /// Like [`Self::guild`], and marks it for saving.
    pub fn guild_mut<R>(&self, guild_id: u64, f: impl FnOnce(&mut GuildKnowledge) -> R) -> R {
        let result = self.guild(guild_id, f);
        self.dirty_guilds
            .lock()
            .expect("knowledge poisoned")
            .insert(guild_id);
        result
    }

    pub fn channel<R>(&self, channel_id: u64, f: impl FnOnce(&mut ChannelArchive) -> R) -> R {
        let mut channels = self.channels.lock().expect("knowledge poisoned");
        let entry = channels.entry(channel_id).or_insert_with(|| {
            crate::paths::read_json(&channel_path(channel_id)).unwrap_or(ChannelArchive {
                channel_id,
                ..ChannelArchive::default()
            })
        });
        f(entry)
    }

    pub fn channel_mut<R>(&self, channel_id: u64, f: impl FnOnce(&mut ChannelArchive) -> R) -> R {
        let result = self.channel(channel_id, f);
        self.dirty_channels
            .lock()
            .expect("knowledge poisoned")
            .insert(channel_id);
        result
    }

    pub fn known_guilds(&self) -> Vec<(u64, String)> {
        let mut out: Vec<(u64, String)> = self
            .guilds
            .lock()
            .expect("knowledge poisoned")
            .values()
            .map(|g| (g.guild_id, g.guild_name.clone()))
            .collect();
        // Include guilds saved on disk but not loaded yet.
        if let Ok(entries) = std::fs::read_dir(crate::paths::shared_dir()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name
                    .strip_prefix("guild_")
                    .and_then(|rest| rest.strip_suffix(".json"))
                    .and_then(|id| id.parse::<u64>().ok())
                {
                    if !out.iter().any(|(known, _)| *known == id) {
                        let label = self.guild(id, |g| g.guild_name.clone());
                        out.push((id, label));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    pub fn save_dirty(&self) {
        let guild_ids: Vec<u64> = self
            .dirty_guilds
            .lock()
            .expect("knowledge poisoned")
            .drain()
            .collect();
        for id in guild_ids {
            let bytes = self.guild(id, |g| serde_json::to_vec(g));
            if let Ok(bytes) = bytes {
                if let Err(error) = crate::paths::write_atomic(&guild_path(id), &bytes) {
                    tracing::error!(%error, "guild knowledge save failed");
                }
            }
        }
        let channel_ids: Vec<u64> = self
            .dirty_channels
            .lock()
            .expect("knowledge poisoned")
            .drain()
            .collect();
        for id in channel_ids {
            let bytes = self.channel(id, |c| serde_json::to_vec(c));
            if let Ok(bytes) = bytes {
                if let Err(error) = crate::paths::write_atomic(&channel_path(id), &bytes) {
                    tracing::error!(%error, "channel archive save failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_parsing_and_timing() {
        use chrono::{Local, TimeZone};
        assert_eq!(parse_time_of_day("9am"), Some(540));
        assert_eq!(parse_time_of_day("9:30 PM"), Some(21 * 60 + 30));
        assert_eq!(parse_time_of_day("12am"), Some(0));
        assert_eq!(parse_time_of_day("12pm"), Some(720));
        assert_eq!(parse_time_of_day("18:05"), Some(18 * 60 + 5));
        assert_eq!(parse_time_of_day("noon"), Some(720));
        assert_eq!(parse_time_of_day("25:00"), None);
        assert_eq!(parse_time_of_day("soon"), None);
        assert_eq!(parse_days("weekdays"), Some(vec![0, 1, 2, 3, 4]));
        assert_eq!(parse_days("monday and friday"), Some(vec![0, 4]));
        assert_eq!(parse_days("every day"), Some(Vec::new()));
        assert_eq!(parse_days("whenever"), None);
        assert_eq!(day_names(&[0, 4]), "Mon, Fri");

        // Monday 2026-11-02, local time.
        let at = |h: u32, m: u32| {
            Local
                .with_ymd_and_hms(2026, 11, 2, h, m, 0)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        let nine = 9 * 60;
        assert!(!schedule_due(at(8, 59), 0, nine, &[], 1440), "not yet");
        assert!(schedule_due(at(9, 0), at(7, 0), nine, &[], 1440), "on time");
        assert!(
            !schedule_due(at(9, 5), at(9, 0), nine, &[], 1440),
            "once a day"
        );
        assert!(
            !schedule_due(at(16, 0), at(7, 0), nine, &[], 1440),
            "too late (app was off)"
        );
        assert!(
            schedule_due(at(9, 1), at(7, 0), nine, &[0], 1440),
            "Mondays"
        );
        assert!(
            !schedule_due(at(9, 1), at(7, 0), nine, &[1, 2], 1440),
            "not Monday"
        );
        // Once: later today, or tomorrow when the time passed.
        assert_eq!(next_time_at(at(8, 0), nine, None), Some(at(9, 0)));
        assert_eq!(
            next_time_at(at(10, 0), nine, None),
            Some(at(9, 0) + 86_400_000)
        );
        assert!(next_time_at(at(10, 0), nine, Some("2026-11-05")).is_some());
    }

    fn memory(text: &str, about: Option<u64>) -> Memory {
        Memory {
            id: 0,
            text: text.to_owned(),
            kind: MemoryKind::Fact,
            about_user: about,
            about_name: None,
            guild_id: None,
            source: "test".to_owned(),
            created: now_ms(),
            last_used: 0,
            uses: 0,
            importance: 3,
            embedding: Vec::new(),
        }
    }

    fn store() -> MemoryStore {
        MemoryStore::load(std::env::temp_dir().join(format!("axis_mem_{}.json", fastrand::u64(..))))
    }

    #[test]
    fn search_finds_relevant_and_boosts_participants() {
        let store = store();
        store.add(memory("Alice loves playing chess on weekends", Some(1)));
        store.add(memory("The server movie night is every Friday", None));
        store.add(memory("Bob's favorite language is Rust", Some(2)));
        let results = store.search("who plays chess?", &[], None, None, 3);
        assert_eq!(results[0].about_user, Some(1));
        let results = store.search("favorite programming language", &[2], None, None, 3);
        assert!(results[0].text.contains("Rust"));
        assert!(store.search("zebra quantum", &[], None, None, 3).is_empty());
    }

    #[test]
    fn near_duplicates_merge() {
        let store = store();
        let (a, new_a) = store.add(memory("Carol prefers to be called CJ", Some(3)));
        let (b, new_b) = store.add(memory("Carol prefers to be called CJ now", Some(3)));
        assert!(new_a && !new_b);
        assert_eq!(a, b);
        assert_eq!(store.len(), 1);
        assert_eq!(store.forget_user(3), 1);
    }

    #[test]
    fn persistence_roundtrip() {
        let path = std::env::temp_dir().join(format!("axis_mem_rt_{}.json", fastrand::u64(..)));
        let store = MemoryStore::load(path.clone());
        store.add(memory("remember me", None));
        store.save_if_dirty();
        let reloaded = MemoryStore::load(path.clone());
        assert_eq!(reloaded.len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn archive_dedupes_and_orders() {
        let mut archive = ChannelArchive::default();
        let message = |id| StoredMessage {
            id,
            author_id: 1,
            author: "a".to_owned(),
            author_bot: false,
            content: format!("m{id}"),
            ts: 0,
            reply_to: None,
            reply_author: None,
            reply_snippet: None,
            attachments: Vec::new(),
            edited: false,
            deleted: false,
            reactions: Vec::new(),
        };
        assert!(archive.push(message(2)));
        assert!(archive.push(message(1)));
        assert!(!archive.push(message(2)));
        assert_eq!(
            archive.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn directives_sorted_by_rank_and_scoped() {
        let mut knowledge = GuildKnowledge::default();
        let directive = |id, rank, scope| Directive {
            id,
            text: format!("d{id}"),
            author_id: id,
            author_name: "x".to_owned(),
            author_rank: rank,
            author_level: crate::config::AccessLevel::Everyone,
            scope,
            target_agent: None,
            created: 0,
        };
        knowledge
            .directives
            .push(directive(1, 5, DirectiveScope::Server));
        knowledge
            .directives
            .push(directive(2, 50, DirectiveScope::Channel(9)));
        knowledge
            .directives
            .push(directive(3, 99, DirectiveScope::Channel(8)));
        knowledge
            .directives
            .push(directive(4, 1, DirectiveScope::Personal(7)));
        let applicable = knowledge.directives_for("a", 9, &[7]);
        assert_eq!(
            applicable.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![2, 1, 4]
        );
    }

    #[test]
    fn leveling_overrides_load_empty_from_old_json() {
        // Guild knowledge saved before personal overrides existed.
        let k: GuildKnowledge = serde_json::from_str(
            r#"{"guild_id": 1, "leveling": {
                "enabled": true, "xp_min": 10, "xp_max": 30, "cooldown_secs": 60,
                "voice_xp_per_minute": 3, "announce": "dm", "announce_channel": 0,
                "message": "gg {user}", "rewards": [], "stack_rewards": true,
                "ignored_channels": [], "card": {"style": "glass"},
                "members": {"123": {"xp": 500, "messages": 40, "voice_minutes": 2, "last_xp": 0, "name": "ann"}}
            }}"#,
        )
        .unwrap();
        let l = &k.leveling;
        assert_eq!(l.announce, "dm");
        assert_eq!(l.message, "gg {user}");
        assert_eq!(l.card.style, "glass");
        assert_eq!(l.members.get(&123).unwrap().xp, 500);
        assert!(l.card_overrides.is_empty());
        assert!(l.message_overrides.is_empty());
        assert!(l.announce_overrides.is_empty());
    }

    #[test]
    fn personal_leveling_helpers_fall_back_then_override() {
        let mut l = Leveling::default();
        l.message = "server {user}".to_owned();
        l.announce = "same".to_owned();
        l.card.style = "classic".to_owned();
        // Without an override: the server's everything.
        assert_eq!(l.message_for(7), "server {user}");
        assert_eq!(l.announce_for(7), "same");
        assert_eq!(l.card_for(7).style, "classic");
        // With one: only that person sees their own.
        l.message_overrides.insert(7, "mine {level}".to_owned());
        l.announce_overrides.insert(7, "off".to_owned());
        l.card_overrides.insert(
            7,
            RankCard {
                style: "glass".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(l.message_for(7), "mine {level}");
        assert_eq!(l.announce_for(7), "off");
        assert_eq!(l.card_for(7).style, "glass");
        // Everyone else still gets the server defaults.
        assert_eq!(l.message_for(8), "server {user}");
        assert_eq!(l.announce_for(8), "same");
        assert_eq!(l.card_for(8).style, "classic");
    }

    #[test]
    fn leveling_overrides_round_trip_through_json() {
        let mut l = Leveling::default();
        l.message_overrides.insert(42, "hi {name}".to_owned());
        l.announce_overrides.insert(42, "dm".to_owned());
        l.card_overrides.insert(
            42,
            RankCard {
                accent: "#ff0000".to_owned(),
                ..Default::default()
            },
        );
        let back: Leveling = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back.message_for(42), "hi {name}");
        assert_eq!(back.announce_for(42), "dm");
        assert_eq!(back.card_for(42).accent, "#ff0000");
    }
}
