//! One agent: its Discord connection, its view of the world, and the
//! decision of when to speak.
//!
//! Gateway handlers stay fast — they update caches and route messages.
//! Anything slow (LLM calls, moderation reviews, reflection) runs on its
//! own task. Each channel gets a worker that debounces bursts of
//! messages, so three quick lines from one person get one answer.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axis::model::{AllowedMentions, Message};
use axis::prelude::*;
use tokio::sync::{mpsc, watch};

use super::authority::{Principal, PrincipalKind};
use super::brain::{self, Job, Outlet, Reason};
use super::hive::{ClaimKind, Hive, Peer};
use super::live::{AgentStatus, FeedKind, Live};
use super::world::World;
use crate::config::AgentProfile;
use crate::llm::{Endpoint, ModelInfo};
use crate::memory::{AttachmentInfo, MemoryStore, StoredMessage, TaskTrigger};

pub struct AgentCore {
    pub id: String,
    profile: RwLock<AgentProfile>,
    pub http: Arc<Http>,
    pub world: World,
    pub memory: Arc<MemoryStore>,
    pub hive: Arc<Hive>,
    pub messenger: Mutex<Option<ShardMessenger>>,
    /// The status line Discord is showing (set on connect and by Apply).
    pub shown_status: Mutex<String>,
    /// (channel, user) → last time we talked with them there.
    pub conversations: Mutex<HashMap<(u64, u64), Instant>>,
    workers: Mutex<HashMap<u64, mpsc::UnboundedSender<(Message, Reason)>>>,
    pub paused: AtomicBool,
    /// Learned characters per token (EMA of observed usage).
    pub chars_per_token: Mutex<f32>,
    /// Slash command syncing (rate-limited, on request).
    pub command_sync: super::commands::SyncState,
    /// Extra output tokens for models whose thinking eats the reply budget
    /// (learned when one runs out of room mid-thought; reset per model).
    pub thinking_room: std::sync::atomic::AtomicU32,
    pub model_info: RwLock<ModelInfo>,
    /// The model rejected native tools; use the text protocol.
    pub text_tools: AtomicBool,
    /// The model rejected images.
    pub no_vision: AtomicBool,
    pub application_id: AtomicU64,
    /// Per-guild: (user → recent message fingerprints) for spam checks.
    pub recent_by_user: Mutex<HashMap<(u64, u64), Vec<(i64, u64)>>>,
    pub commands: Mutex<Option<Arc<axis_commands::CommandRegistry>>>,
    pub offline: bool,
    /// Voice connections.
    pub voice: super::voice::VoiceHub,
    /// Long tool results, readable a section at a time.
    pub docs: crate::tools::docs::Library,
    /// channel → when someone said "stop" to the agent there.
    pub interrupts: Mutex<HashMap<u64, i64>>,
    /// channel → jobs running there now.
    pub running: Mutex<HashMap<u64, u32>>,
    /// channel → who the job running there is working for (people's
    /// follow-ups go into that job instead of queueing behind it).
    pub running_for: Mutex<HashMap<u64, u64>>,
    /// channel → messages that arrived for the running job.
    pub steering: Mutex<HashMap<u64, Vec<(Message, Reason)>>>,
    /// channel → a thinking message posted the moment a message for the
    /// agent arrived (the job picks it up).
    pub early: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<Option<MessageId>>>>>,
    /// message → its pages, for long answers with page buttons.
    pub pages: Mutex<HashMap<u64, Vec<String>>>,
}

impl AgentCore {
    /// A "never chat here" rule covering this channel (directly, or via
    /// its category or a thread's parent) for this agent.
    pub fn barred(&self, guild_id: u64, channel_id: u64) -> Option<crate::memory::NoChatRule> {
        let parent = self
            .world
            .channel(guild_id, channel_id)
            .and_then(|c| c.parent_id.map(|p| p.get()));
        let grandparent = parent
            .and_then(|p| self.world.channel(guild_id, p))
            .and_then(|c| c.parent_id.map(|p| p.get()));
        let covers = [Some(channel_id), parent, grandparent];
        self.hive.knowledge.guild(guild_id, |k| {
            k.no_chat
                .iter()
                .find(|r| {
                    r.agent_id.as_deref().is_none_or(|a| a == self.id)
                        && covers.contains(&Some(r.channel_id))
                })
                .cloned()
        })
    }

    /// What the bot itself is missing to post in a channel ("Send
    /// Messages"), or None when it can (or its roles aren't known yet).
    pub fn cant_post(&self, guild_id: u64, channel_id: u64) -> Option<String> {
        use axis::prelude::Permissions;
        let me = self.world.my_id();
        self.world
            .with_guild(guild_id, |g| {
                let roles = g.members.get(&me)?.roles.clone();
                let channel = g.channels.get(&channel_id)?;
                let have = super::authority::channel_permissions(g, me, &roles, channel_id);
                let send = if channel.is_thread() {
                    Permissions::SEND_MESSAGES_IN_THREADS
                } else {
                    Permissions::SEND_MESSAGES
                };
                let mut missing = Vec::new();
                if !have.contains(Permissions::VIEW_CHANNEL) {
                    missing.push("View Channel");
                }
                if !have.contains(send) {
                    missing.push(if channel.is_thread() {
                        "Send Messages in Threads"
                    } else {
                        "Send Messages"
                    });
                }
                (!missing.is_empty()).then(|| missing.join(" and "))
            })
            .flatten()
    }

    /// Same, when only the channel is known.
    pub fn barred_anywhere(&self, channel_id: u64) -> Option<crate::memory::NoChatRule> {
        let guild_id = self.world.guild_of_channel(channel_id)?;
        self.barred(guild_id, channel_id)
    }

    pub fn new(
        profile: AgentProfile,
        hive: Arc<Hive>,
        memory: Arc<MemoryStore>,
        http: Arc<Http>,
        offline: bool,
    ) -> Arc<Self> {
        // What Discord will show once it connects (the presence sent with
        // Identify).
        let shown = status_line(&profile);
        Arc::new(Self {
            id: profile.id.clone(),
            profile: RwLock::new(profile),
            http,
            world: World::default(),
            memory,
            hive,
            messenger: Mutex::new(None),
            shown_status: Mutex::new(shown),
            conversations: Mutex::new(HashMap::new()),
            workers: Mutex::new(HashMap::new()),
            paused: AtomicBool::new(false),
            chars_per_token: Mutex::new(3.6),
            command_sync: Default::default(),
            thinking_room: std::sync::atomic::AtomicU32::new(0),
            model_info: RwLock::new(ModelInfo::default()),
            text_tools: AtomicBool::new(false),
            no_vision: AtomicBool::new(false),
            application_id: AtomicU64::new(0),
            recent_by_user: Mutex::new(HashMap::new()),
            commands: Mutex::new(None),
            offline,
            voice: Default::default(),
            docs: Default::default(),
            interrupts: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            running_for: Mutex::new(HashMap::new()),
            steering: Mutex::new(HashMap::new()),
            early: Mutex::new(HashMap::new()),
            pages: Mutex::new(HashMap::new()),
        })
    }

    pub fn profile(&self) -> AgentProfile {
        self.profile.read().expect("profile poisoned").clone()
    }

    pub fn set_profile(&self, profile: AgentProfile) {
        let model_changed = {
            let current = self.profile.read().expect("profile poisoned");
            current.model != profile.model
        };
        *self.profile.write().expect("profile poisoned") = profile;
        if model_changed {
            self.text_tools.store(false, Ordering::Relaxed);
            self.no_vision.store(false, Ordering::Relaxed);
            self.thinking_room.store(0, Ordering::Relaxed);
            *self.model_info.write().expect("model info poisoned") = ModelInfo::default();
        }
    }

    pub fn name(&self) -> String {
        self.profile.read().expect("profile poisoned").name.clone()
    }

    pub fn live(&self) -> &Live {
        &self.hive.live
    }

    pub fn feed(&self, kind: FeedKind, text: impl Into<String>, channel: Option<String>) {
        self.hive
            .live
            .feed(&self.id, &self.name(), kind, text, channel);
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::from_config(&self.profile().model)
    }

    pub fn channel_label(&self, guild_id: Option<u64>, channel_id: u64) -> String {
        match guild_id {
            Some(guild_id) => self
                .world
                .with_guild(guild_id, |g| format!("#{}", g.channel_name(channel_id)))
                .unwrap_or_else(|| format!("#{channel_id}")),
            None => "DM".to_owned(),
        }
    }

    /// The principal for a message author.
    pub fn principal_for(&self, guild_id: Option<u64>, user: &User) -> Principal {
        let kind = if self.hive.peer_by_user(user.id.get()).is_some() {
            PrincipalKind::Agent
        } else {
            PrincipalKind::Human
        };
        let name = user
            .global_name
            .clone()
            .unwrap_or_else(|| user.username.clone());
        let mut principal = match guild_id {
            Some(guild_id) => self
                .world
                .with_guild(guild_id, |g| match g.members.get(&user.id.get()) {
                    Some(member) => Principal::for_member(g, member, kind),
                    None => Principal::unknown_member(g, user.id.get(), name.clone()),
                })
                .unwrap_or_else(|| Principal::dm_user(user.id.get(), name.clone())),
            None => Principal::dm_user(user.id.get(), name),
        };
        principal.kind = kind;
        principal
    }

    /// The agent itself, acting on its own initiative.
    pub fn self_principal(&self, guild_id: Option<u64>) -> Principal {
        let me = self.world.me();
        let mut principal = match (guild_id, &me) {
            (Some(guild_id), Some(me)) => self
                .world
                .with_guild(guild_id, |g| match g.members.get(&me.id.get()) {
                    Some(member) => Principal::for_member(g, member, PrincipalKind::Agent),
                    None => Principal::unknown_member(g, me.id.get(), self.name()),
                })
                .unwrap_or_else(|| Principal::dm_user(me.id.get(), self.name())),
            _ => Principal::dm_user(self.world.my_id(), self.name()),
        };
        principal.kind = PrincipalKind::Agent;
        principal.name = self.name();
        principal
    }

    pub async fn post_mod_log(&self, guild_id: u64, line: &str) {
        let log_name = self.profile().behavior.moderation.log_channel;
        let log_channel = if log_name.trim().is_empty() {
            None
        } else {
            self.world
                .with_guild(guild_id, |g| g.find_channel(&log_name).map(|c| c.id.get()))
                .flatten()
        };
        let target =
            log_channel.or_else(|| self.hive.knowledge.guild(guild_id, |k| k.hq.channel_id));
        if let Some(channel) = target {
            let _ = self
                .http
                .create_message(ChannelId::new(channel))
                .content(crate::llm::truncate(line, 1990))
                .allowed_mentions(AllowedMentions::none())
                .await;
        }
    }

    /// Compute and store an embedding for a memory in the background.
    pub fn embed_memory_later(self: &Arc<Self>, id: u64) {
        let model = self.profile().model.embedding_model;
        if model.trim().is_empty() {
            return;
        }
        let core = Arc::clone(self);
        tokio::spawn(async move {
            if let Some(memory) = core.memory.get(id) {
                if let Ok(mut vectors) = core.endpoint().embed(&model, &[memory.text]).await {
                    if let Some(vector) = vectors.pop() {
                        core.memory.set_embedding(id, vector);
                    }
                }
            }
        });
    }

    pub async fn embed_query(&self, query: &str) -> Option<Vec<f32>> {
        let model = self.profile().model.embedding_model;
        if model.trim().is_empty() {
            return None;
        }
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            self.endpoint().embed(&model, &[query.to_owned()]),
        )
        .await;
        match result {
            Ok(Ok(mut vectors)) => vectors.pop(),
            _ => None,
        }
    }

    pub fn note_conversation(&self, channel_id: u64, user_id: u64) {
        self.conversations
            .lock()
            .expect("conversations poisoned")
            .insert((channel_id, user_id), Instant::now());
    }

    fn in_conversation(&self, channel_id: u64, user_id: u64, window: Duration) -> bool {
        let mut conversations = self.conversations.lock().expect("conversations poisoned");
        conversations.retain(|_, at| at.elapsed() < Duration::from_secs(3600));
        conversations
            .get(&(channel_id, user_id))
            .is_some_and(|at| at.elapsed() < window)
    }

    /// Model info: configured values win over detected ones.
    pub fn context_window(&self) -> u32 {
        let configured = self.profile().model.context_window;
        if configured > 0 {
            return configured;
        }
        self.model_info
            .read()
            .expect("model info poisoned")
            .context_window
            .unwrap_or(8192)
            .max(2048)
    }

    pub fn vision_enabled(&self) -> bool {
        self.profile().model.vision && !self.no_vision.load(Ordering::Relaxed)
    }

    pub async fn refresh_model_info(&self) {
        let profile = self.profile();
        let info = self.endpoint().model_info(&profile.model.model).await;
        // The main model being able to see IS vision, by default: when the
        // endpoint reports image support and no separate vision model is
        // configured, switch the agent's vision on automatically (the
        // toggle stays available to turn it off).
        if info.vision == Some(true)
            && !profile.model.vision
            && profile.model.vision_model.trim().is_empty()
        {
            let mut patched = profile.clone();
            patched.model.vision = true;
            self.set_profile(patched.clone());
            let id = self.id.clone();
            self.live()
                .profile_patches
                .lock()
                .expect("live poisoned")
                .push((
                    id,
                    Box::new(move |p: &mut crate::config::AgentProfile| {
                        p.model.vision = true;
                    }),
                ));
            tracing::info!(agent = %self.id, "main model supports vision — enabled it by default");
        }
        self.live()
            .agent(&self.id, |live| live.model_info = Some(info.clone()));
        *self.model_info.write().expect("model info poisoned") = info;
    }
}

// ---------------------------------------------------------------------------
// Archive conversion
// ---------------------------------------------------------------------------

pub fn display_name(message: &Message) -> String {
    message
        .member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .or_else(|| message.author.global_name.clone())
        .unwrap_or_else(|| message.author.username.clone())
}

pub fn to_stored(message: &Message) -> StoredMessage {
    let reply = message.referenced_message.as_deref();
    StoredMessage {
        id: message.id.get(),
        author_id: message.author.id.get(),
        author: display_name(message),
        author_bot: message.author.bot.unwrap_or(false),
        content: message.full_text(),
        ts: message.timestamp.as_unix_millis(),
        reply_to: message
            .message_reference
            .and_then(|r| r.message_id)
            .map(|id| id.get()),
        reply_author: reply.map(display_name),
        reply_snippet: reply.map(|r| crate::llm::truncate(&r.content, 120)),
        attachments: message
            .attachments
            .iter()
            .map(|a| AttachmentInfo {
                name: a.filename.clone(),
                url: a.url.clone(),
                content_type: a.content_type.clone(),
                size: a.size,
            })
            .chain(message.media_urls().into_iter().map(|url| AttachmentInfo {
                name: "embedded image".to_owned(),
                url: super::vision::animated_variant(&url).unwrap_or(url),
                content_type: Some("image/*".to_owned()),
                size: 0,
            }))
            // Stickers show in history as images too.
            .chain(message.sticker_items.iter().flatten().filter_map(|s| {
                Some(AttachmentInfo {
                    name: format!("sticker {}", s.name),
                    url: s.image_url()?,
                    content_type: Some("image/png".to_owned()),
                    size: 0,
                })
            }))
            .collect(),
        edited: message.edited_timestamp.is_some(),
        deleted: false,
        reactions: Vec::new(),
    }
}

/// Whether `word` appears in `text` as a whole word (so "nova" matches
/// "hey Nova," and "nova's" but not "casanova").
pub fn mentions_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    let mut start = 0;
    while let Some(found) = lower[start..].find(word) {
        let at = start + found;
        let before = lower[..at].chars().next_back();
        let after = lower[at + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        if boundary(before) && boundary(after) {
            return true;
        }
        start = at + word.len().max(1);
        if start >= lower.len() {
            break;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Routing incoming messages
// ---------------------------------------------------------------------------

/// Why a message reached the agent.
/// Does this text call the agent by name, honoring the configured
/// position? `Anywhere` matches the name as any whole word; `Start`
/// requires it as the first or second word.
pub fn named_in(profile: &crate::config::AgentProfile, text: &str) -> bool {
    match profile.behavior.name_position {
        crate::config::NamePosition::Anywhere => profile
            .trigger_words()
            .iter()
            .any(|word| mentions_word(text, word)),
        crate::config::NamePosition::Start => {
            let words: Vec<&str> = text.split_whitespace().take(2).collect();
            profile.trigger_words().iter().any(|word| {
                words.iter().any(|text| {
                    let text = text.trim_start_matches('@');
                    text.trim_matches(|c: char| !c.is_alphanumeric())
                        .eq_ignore_ascii_case(word)
                })
            })
        }
    }
}

pub fn classify(core: &Arc<AgentCore>, message: &Message) -> Option<Reason> {
    let profile = core.profile();
    let me = core.world.my_id();
    let author = message.author.id.get();
    let guild_id = message.guild_id.map(|g| g.get());
    let channel_id = message.channel_id.get();
    let author_agent = core.hive.peer_by_user(author);
    // An @mention of the bot, or of the bot's own managed role.
    let mentioned = message.mentions.iter().any(|u| u.id.get() == me)
        || guild_id.is_some_and(|g| {
            !message.mention_roles.is_empty()
                && core
                    .world
                    .with_guild(g, |guild| {
                        guild.members.get(&me).is_some_and(|m| {
                            message.mention_roles.iter().any(|role| {
                                m.roles.contains(&role.get())
                                    && guild.roles.get(&role.get()).is_some_and(|r| r.managed)
                            })
                        })
                    })
                    .unwrap_or(false)
        });
    let replied_to_me = message
        .referenced_message
        .as_ref()
        .is_some_and(|r| r.author.id.get() == me);
    let named = named_in(&profile, &message.content);
    let hq = guild_id.and_then(|g| core.hive.knowledge.guild(g, |k| k.hq.channel_id));
    let in_hq = hq == Some(channel_id);
    let settings = core.hive.settings();

    // "Never chat in #x": nothing there gets an answer — except someone
    // ranked high enough asking to lift the rule. Being called there gets
    // a 🔇 reaction, so they know it was seen.
    if let Some(rule) = guild_id.and_then(|g| core.barred(g, channel_id)) {
        let addressed = (mentioned || replied_to_me || named) && author_agent.is_none();
        let lifts = addressed
            && asks_to_lift(&message.content)
            && core.principal_for(guild_id, &message.author).rank >= rule.setter_rank;
        if lifts {
            return Some(Reason::Direct);
        }
        if addressed {
            let core = Arc::clone(core);
            let path = format!(
                "/channels/{channel_id}/messages/{}/reactions/%F0%9F%94%87/@me",
                message.id
            );
            tokio::spawn(async move {
                let _ = core.http.raw("PUT", &path, None, None).await;
            });
        }
        return None;
    }

    if author_agent.is_some() {
        // Fellow agents get answers only when they address us.
        if !(mentioned || replied_to_me || (in_hq && named)) {
            return None;
        }
        // Loop guard: agents can't bounce off each other forever, in
        // public or in HQ (where planning gets a longer leash).
        let limit = if in_hq {
            settings.max_agent_chain * 2
        } else {
            settings.max_agent_chain
        };
        if core.hive.agent_chain(channel_id) >= limit {
            return None;
        }
        return Some(if in_hq { Reason::Hq } else { Reason::Direct });
    }
    if message.author.bot.unwrap_or(false) {
        if !profile.behavior.reply_to_other_bots || !(mentioned || replied_to_me) {
            return None;
        }
        if core.hive.agent_chain(channel_id) >= settings.max_agent_chain {
            return None;
        }
    }
    let Some(guild_id) = guild_id else {
        return profile.behavior.respond_in_dms.then_some(Reason::Direct);
    };

    // Channel allow/deny lists (threads inherit their parent's).
    let parent = core
        .world
        .channel(guild_id, channel_id)
        .and_then(|c| c.parent_id.map(|p| p.get()));
    let listed =
        |list: &[u64]| list.contains(&channel_id) || parent.is_some_and(|p| list.contains(&p));
    if listed(&profile.behavior.ignored_channels) {
        return None;
    }
    if !profile.behavior.allowed_channels.is_empty()
        && !listed(&profile.behavior.allowed_channels)
        && !in_hq
    {
        return None;
    }

    let direct = (mentioned && profile.behavior.respond_to_mentions)
        || (replied_to_me && profile.behavior.respond_to_replies)
        || (named && profile.behavior.respond_to_name);

    // Quiet rules: a direct call from someone ranked at least as high as
    // whoever silenced us (or the person who asked us to leave them alone)
    // breaks through; everything else stays quiet.
    let quiet = core.hive.knowledge.guild(guild_id, |k| {
        k.quiet_rule(&core.id, channel_id, author).cloned()
    });
    if let Some(rule) = quiet {
        let requester = core.principal_for(Some(guild_id), &message.author);
        let lifts = direct && (requester.rank >= rule.setter_rank || rule.user_id == Some(author));
        if !lifts {
            return None;
        }
        if rule.user_id == Some(author) {
            core.hive.knowledge.guild_mut(guild_id, |k| {
                k.quiet
                    .retain(|r| !(r.user_id == Some(author) && r.agent_id == rule.agent_id))
            });
        }
    }

    if in_hq && (direct || named) {
        return Some(Reason::Hq);
    }
    if direct {
        return Some(Reason::Direct);
    }
    // Someone writing in their own ticket is talking to the agent.
    if super::tickets::answers_in(core, guild_id, channel_id, author) {
        return Some(Reason::Direct);
    }
    if in_hq {
        return None;
    }
    let window = Duration::from_secs(profile.behavior.conversation_window_secs);
    if core.in_conversation(channel_id, author, window) && !addresses_someone_else(core, message) {
        return Some(Reason::Conversation);
    }
    if chilled(core, guild_id, channel_id) {
        // Chill channels: no chiming in. If it got here without a direct
        // address, stay quiet.
        return None;
    }
    if profile.behavior.chattiness > 0 && message.content.split_whitespace().count() >= 3 {
        let mut chance = profile.behavior.chattiness as f32 / 100.0;
        if message.content.contains('?') {
            chance *= 1.6;
        }
        let specialty_hit = profile
            .specialty
            .split([',', ' ', '&', '/'])
            .filter(|w| w.len() >= 5)
            .any(|w| mentions_word(&message.content, &w.to_lowercase()));
        if specialty_hit {
            chance *= 2.0;
        }
        if addresses_someone_else(core, message) {
            chance *= 0.2;
        }
        if fastrand::f32() < chance.min(0.9) {
            return Some(Reason::Ambient);
        }
    }
    None
}

fn addresses_someone_else(core: &AgentCore, message: &Message) -> bool {
    let me = core.world.my_id();
    if message.mentions.iter().any(|u| u.id.get() != me) {
        return true;
    }
    if message
        .referenced_message
        .as_ref()
        .is_some_and(|r| r.author.id.get() != me)
    {
        return true;
    }
    core.hive
        .peers()
        .iter()
        .filter(|p| p.id != core.id)
        .any(|p| {
            std::iter::once(&p.name)
                .chain(p.aliases.iter())
                .any(|name| mentions_word(&message.content, &name.to_lowercase()))
        })
}

/// The gateway's MESSAGE_CREATE path.
pub fn handle_message(core: &Arc<AgentCore>, message: Message) {
    let guild_id = message.guild_id.map(|g| g.get());
    let channel_id = message.channel_id.get();
    let me = core.world.my_id();
    let author = message.author.id.get();
    match guild_id {
        Some(guild_id) => {
            core.world
                .upsert_author(guild_id, &message.author, message.member.as_ref())
        }
        None => {
            if author != me {
                core.world.note_dm(channel_id, author);
            }
        }
    }

    // An agent's live voice transcript: already in the voice history.
    if core.hive.peer_by_user(author).is_some()
        && super::voice::is_transcript_line(&message.content)
    {
        return;
    }

    // Archive once per message across all agents.
    let label = core.channel_label(guild_id, channel_id);
    let is_new = core.hive.knowledge.channel_mut(channel_id, |archive| {
        archive.guild_id = guild_id.unwrap_or(0);
        archive.name = label.clone();
        archive.push(to_stored(&message))
    });
    let author_is_agent = core.hive.peer_by_user(author).is_some();
    if is_new {
        core.hive.note_speaker(channel_id, author_is_agent);
        if let Some(guild_id) = guild_id {
            core.hive
                .knowledge
                .guild_mut(guild_id, |k| k.messages_since_reflect += 1);
            // XP and statistics, counted once per message.
            if !author_is_agent {
                super::levels::on_message(core, guild_id, &message);
            }
        }
    }
    if author == me {
        return;
    }
    core.live().stat(&core.id, |s| s.messages_seen += 1);
    if core.paused.load(Ordering::Relaxed) || core.offline {
        return;
    }

    let mut task_handles = false;
    if let Some(guild_id) = guild_id {
        let human = !message.author.bot.unwrap_or(false);
        if human {
            if super::moderation::screen(core, guild_id, &message) {
                return; // removed as rule-breaking; don't answer it
            }
            task_handles = super::tasks::on_message(core, guild_id, &message);
            super::hq::on_message(core, guild_id, &message);
        }
        super::reflect::maybe_reflect(core, guild_id);
    }

    // Live translation covers this conversation: translate, and only
    // answer when talked to directly ("stop translating").
    let translating = match guild_id {
        Some(guild_id) => match super::translate::on_message(core, guild_id, &message) {
            super::translate::Handled::Stopped => return,
            handled => handled == super::translate::Handled::Translating,
        },
        None => false,
    };
    let reason = classify(core, &message);
    if matches!(reason, Some(Reason::Direct | Reason::Conversation))
        && brain::is_interrupt(&message.content)
    {
        let busy = core
            .running
            .lock()
            .expect("running poisoned")
            .get(&channel_id)
            .copied()
            .unwrap_or(0)
            > 0;
        if busy {
            core.interrupts
                .lock()
                .expect("interrupts poisoned")
                .insert(channel_id, crate::memory::now_ms());
            return;
        }
    }
    // The person the running job works for adds something ("no, do all
    // the clones first"): it goes into that job, which adjusts.
    if let Some(r @ (Reason::Direct | Reason::Conversation)) = reason {
        let working_for = core
            .running_for
            .lock()
            .expect("running poisoned")
            .get(&channel_id)
            .copied();
        if working_for == Some(author) && !author_is_agent {
            core.steering
                .lock()
                .expect("steering poisoned")
                .entry(channel_id)
                .or_default()
                .push((message.clone(), r));
            let core = Arc::clone(core);
            let path = format!(
                "/channels/{channel_id}/messages/{}/reactions/%F0%9F%91%80/@me",
                message.id
            );
            tokio::spawn(async move {
                let _ = core.http.raw("PUT", &path, None, None).await;
            });
            return;
        }
    }
    if !translating && reason.is_none_or(|r| r == Reason::Ambient) {
        if let Some(guild_id) = guild_id {
            super::translate::maybe_offer(core, guild_id, &message);
        }
    }
    if let Some(reason) = reason {
        // A standing task already covers this message; only a direct
        // call gets a separate answer.
        if task_handles && reason < Reason::Hq {
            return;
        }
        if translating && reason != Reason::Direct && reason != Reason::Hq {
            return;
        }
        if reason == Reason::Direct || reason == Reason::Hq {
            core.note_conversation(channel_id, author);
        }
        if matches!(reason, Reason::Direct | Reason::Hq | Reason::Conversation) {
            early_thinking(core, channel_id);
        }
        enqueue(core, message, reason);
    }
}

/// "you can chat here again" and the like.
pub fn asks_to_lift(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "can chat",
        "can talk",
        "can post",
        "can speak",
        "can reply",
        "can respond",
        "allowed to",
        "chat here",
        "talk here",
        "unblock",
        "lift",
        "start chatting",
        "start talking",
        "unmute",
        "come back",
        "you may",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// Show "thinking…" the moment a message for the agent arrives, before
/// the burst wait and context gathering; the job takes it over (and
/// deletes it if it decides not to answer).
fn early_thinking(core: &Arc<AgentCore>, channel_id: u64) {
    let profile = core.profile();
    if !profile.behavior.thinking_style.uses_placeholder() {
        return;
    }
    let slot = Arc::new(tokio::sync::Mutex::new(None));
    let Ok(guard) = Arc::clone(&slot).try_lock_owned() else {
        return;
    };
    {
        let mut early = core.early.lock().expect("early poisoned");
        if early.contains_key(&channel_id) {
            return;
        }
        early.insert(channel_id, Arc::clone(&slot));
    }
    let core = Arc::clone(core);
    let size = if profile.behavior.small_thinking {
        "-# "
    } else {
        ""
    };
    // Icon only — the configured status icon (the operator's chess
    // piece); no words, no elapsed counter.
    let text = format!("{size}{}", profile.behavior.emojis.thinking.trim());
    tokio::spawn(async move {
        let mut guard = guard;
        if let Ok(message) = core
            .http
            .create_message(ChannelId::new(channel_id))
            .content(text.clone())
            .allowed_mentions(AllowedMentions::none())
            // No SUPPRESS_NOTIFICATIONS: the icon alone mentions nobody,
            // and leaving the flag off lets the final edit-in-place
            // (which adds the answer and its mentions) push-notify.
            .await
        {
            // What this message shows, so later no-op edits of the same
            // icon text can be skipped (no pointless "(edited)" tag).
            core.live().remember_sent(message.id.get(), &text);
            *guard = Some(message.id);
        }
        drop(guard);
        // Nobody picked it up (the message was dropped after all): remove it.
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        let stale = {
            let mut early = core.early.lock().expect("early poisoned");
            match early.get(&channel_id) {
                Some(current) if Arc::ptr_eq(current, &slot) => early.remove(&channel_id),
                _ => None,
            }
        };
        if let Some(slot) = stale {
            if let Some(id) = *slot.lock().await {
                let _ = core
                    .http
                    .delete_message(ChannelId::new(channel_id), id)
                    .await;
                core.live().forget_sent(id.get());
            }
        }
    });
}

pub(crate) fn enqueue(core: &Arc<AgentCore>, message: Message, reason: Reason) {
    let channel_id = message.channel_id.get();
    let mut workers = core.workers.lock().expect("workers poisoned");
    let item = (message, reason);
    let item = match workers.get(&channel_id) {
        Some(sender) => match sender.send(item) {
            Ok(()) => return,
            Err(mpsc::error::SendError(item)) => item,
        },
        None => item,
    };
    let (sender, receiver) = mpsc::unbounded_channel();
    let _ = sender.send(item);
    workers.insert(channel_id, sender);
    let core = Arc::clone(core);
    tokio::spawn(channel_worker(core, channel_id, receiver));
}

/// Collects bursts of messages in one channel and answers them together.
async fn channel_worker(
    core: Arc<AgentCore>,
    channel_id: u64,
    mut receiver: mpsc::UnboundedReceiver<(Message, Reason)>,
) {
    loop {
        let first = match tokio::time::timeout(Duration::from_secs(300), receiver.recv()).await {
            Ok(Some(message)) => message,
            _ => break,
        };
        let debounce = Duration::from_millis(core.profile().behavior.debounce_ms.clamp(0, 8000));
        let started = Instant::now();
        let mut batch = vec![first];
        loop {
            let since_start = started.elapsed();
            let cap = Duration::from_millis(5000);
            if since_start >= cap {
                break;
            }
            let wait = debounce.min(cap - since_start);
            match tokio::time::timeout(wait, receiver.recv()).await {
                Ok(Some(message)) => batch.push(message),
                _ => break,
            }
        }
        let reason = batch
            .iter()
            .map(|(_, reason)| *reason)
            .max()
            .unwrap_or(Reason::Ambient);
        let last = batch.last().expect("batch is not empty").0.clone();
        let guild_id = last.guild_id.map(|g| g.get());
        if reason == Reason::Ambient
            && !core
                .hive
                .claim(last.id.get(), ClaimKind::AmbientReply, &core.id)
        {
            core.live().stat(&core.id, |s| s.skipped += 1);
            continue;
        }
        // The requester is the last person who addressed us directly
        // (or simply the last speaker).
        let requester_message = batch
            .iter()
            .rev()
            .find(|(_, reason)| *reason >= Reason::Conversation)
            .map(|(message, _)| message)
            .unwrap_or(&last);
        let requester = core.principal_for(guild_id, &requester_message.author);
        let job = Job {
            guild_id,
            channel_id,
            triggers: batch.into_iter().map(|(message, _)| message).collect(),
            reason,
            requester,
            task: None,
            outlet: Outlet::Channel,
            question: None,
        };
        brain::run(&core, job).await;
    }
    core.workers
        .lock()
        .expect("workers poisoned")
        .remove(&channel_id);
}

// ---------------------------------------------------------------------------
// Running an agent on Discord
// ---------------------------------------------------------------------------

pub fn peer_from(profile: &AgentProfile) -> Peer {
    Peer {
        id: profile.id.clone(),
        name: profile.name.clone(),
        aliases: profile.aliases.clone(),
        specialty: profile.specialty.clone(),
        user_id: 0,
        guilds: Vec::new(),
        online: false,
        moderates: profile.behavior.moderation.enabled,
        learns: profile.behavior.learn_culture,
    }
}

/// The line under the bot's name: the status text, or its specialty.
pub fn status_line(profile: &AgentProfile) -> String {
    if profile.status_text.trim().is_empty() {
        format!("✨ {}", crate::llm::truncate(&profile.specialty, 100))
    } else {
        profile.status_text.trim().to_owned()
    }
}

pub fn presence_for(profile: &AgentProfile) -> Presence {
    Presence::online().with_activity(Activity::custom(status_line(profile)))
}

pub fn intents() -> GatewayIntents {
    GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_MODERATION
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::DIRECT_MESSAGE_REACTIONS
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MESSAGE_POLLS
}

/// Connect and run until `shutdown` flips. Updates live status.
pub async fn run(core: Arc<AgentCore>, token: String, mut shutdown: watch::Receiver<bool>) {
    let profile = core.profile();
    let live = Arc::clone(&core.hive.live);
    let run_id = fastrand::u64(1..);
    live.agent(&core.id, |l| {
        l.status = AgentStatus::Starting;
        l.started_at = Some(Instant::now());
        l.last_error = None;
        l.paused = false;
        l.run_id = run_id;
    });
    let current = |live: &Live| live.snapshot(&core.id).run_id == run_id;
    core.hive.upsert_peer(peer_from(&profile));
    {
        let core = Arc::clone(&core);
        tokio::spawn(async move { core.refresh_model_info().await });
    }

    let mut client = match Client::builder(Token::new(token))
        .intents(intents())
        .presence(presence_for(&profile))
        .large_threshold(250)
        .build()
        .await
    {
        Ok(client) => client,
        Err(error) => {
            let text = match &error {
                AxisError::Auth(_) => {
                    "the bot token was rejected — copy it again from the Developer Portal"
                        .to_owned()
                }
                other => other.to_string(),
            };
            if current(&live) {
                live.agent(&core.id, |l| {
                    l.status = AgentStatus::Error(text.clone());
                    l.last_error = Some(text.clone());
                });
            }
            core.feed(FeedKind::Error, format!("could not connect: {text}"), None);
            return;
        }
    };
    *core.messenger.lock().expect("messenger poisoned") = Some(client.messenger());

    register_handlers(&core, &mut client);

    let ticker = {
        let core = Arc::clone(&core);
        tokio::spawn(async move { super::tasks::scheduler(core).await })
    };

    let start = client.start();
    tokio::pin!(start);
    let outcome = tokio::select! {
        result = &mut start => result,
        _ = shutdown.changed() => {
            client.shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut start).await;
            Ok(())
        }
    };
    ticker.abort();
    let still_current = current(&live);
    if still_current {
        core.hive.update_peer(&core.id, |p| p.online = false);
    }
    *core.messenger.lock().expect("messenger poisoned") = None;
    core.voice.shutdown();
    core.live().agent(&core.id, |l| l.voice_channels.clear());
    let status = match outcome {
        Ok(()) => AgentStatus::Stopped,
        Err(error) => {
            let text = match &error {
                AxisError::Gateway(GatewayError::Closed { code, .. }) => match code.code() {
                    4004 => "Discord rejected the token (4004)".to_owned(),
                    4014 => "privileged intents are not enabled — in the Developer Portal → Bot, turn on Server Members and Message Content intents (4014)".to_owned(),
                    other => format!("gateway closed ({other})"),
                },
                other => other.to_string(),
            };
            core.feed(FeedKind::Error, format!("disconnected: {text}"), None);
            AgentStatus::Error(text)
        }
    };
    if still_current {
        live.agent(&core.id, |l| {
            l.status = status;
            l.thoughts.clear();
        });
    }
}

fn register_handlers(core: &Arc<AgentCore>, client: &mut Client) {
    let c = Arc::clone(core);
    client.on_ready(move |_ctx, ready| {
        let core = Arc::clone(&c);
        async move {
            core.world.set_me(ready.user.clone());
            if let Some(app) = &ready.application {
                core.application_id.store(app.id.get(), Ordering::Relaxed);
            }
            core.hive.update_peer(&core.id, |p| {
                p.user_id = ready.user.id.get();
                p.online = true;
                p.guilds = ready.guilds.iter().map(|g| g.id.get()).collect();
            });
            core.live().agent(&core.id, |l| {
                l.status = AgentStatus::Online;
                l.bot_user = Some((ready.user.id.get(), ready.user.username.clone()));
                l.application_id = ready.application.as_ref().map(|a| a.id.get());
            });
            core.feed(
                FeedKind::System,
                format!("online as @{}", ready.user.username),
                None,
            );
            // Only touches Discord when the commands changed.
            super::commands::request_sync(&core);
            Ok(())
        }
    });

    let c = Arc::clone(core);
    client.on_guild_create(move |_ctx, create| {
        let core = Arc::clone(&c);
        async move {
            let guild_id = create.guild.id.get();
            core.world.apply_guild_create(&create);
            core.hive.update_peer(&core.id, |p| {
                if !p.guilds.contains(&guild_id) {
                    p.guilds.push(guild_id);
                }
            });
            core.hive
                .knowledge
                .guild_mut(guild_id, |k| k.guild_name = create.guild.name.clone());
            core.live()
                .agent(&core.id, |l| l.guilds = core.world.guild_names());
            let cached = create.members.as_ref().map(|m| m.len()).unwrap_or(0) as u64;
            if create.member_count.unwrap_or(0) > cached {
                if let Some(messenger) = core.messenger.lock().expect("messenger poisoned").clone()
                {
                    let _ = messenger.request_guild_members(create.guild.id, "", 0, None);
                }
            }
            let setup = Arc::clone(&core);
            let threads: Vec<ChannelId> = create
                .threads
                .iter()
                .flatten()
                .map(|t| t.id)
                .take(100)
                .collect();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                super::hq::ensure(&setup, guild_id).await;
                super::reflect::load_rules(&setup, guild_id).await;
                // Join active threads so their messages reach us.
                for thread in threads {
                    let _ = setup.http.join_thread(thread).await;
                }
            });
            Ok(())
        }
    });

    let c = Arc::clone(core);
    client.on_guild_update(move |_ctx, guild| {
        c.world.apply_guild_update(&guild);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_guild_delete(move |_ctx, guild| {
        if guild.unavailable != Some(true) {
            c.world.remove_guild(guild.id.get());
            c.hive
                .update_peer(&c.id, |p| p.guilds.retain(|g| *g != guild.id.get()));
            c.live().agent(&c.id, |l| l.guilds = c.world.guild_names());
        }
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_channel_create(move |_ctx, channel| {
        c.world.upsert_channel(&channel);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_channel_update(move |_ctx, channel| {
        c.world.upsert_channel(&channel);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_channel_delete(move |_ctx, channel| {
        c.world.remove_channel(&channel);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_thread_create(move |_ctx, thread| {
        c.world.upsert_channel(&thread);
        // Join new threads so their messages reach us.
        let core = Arc::clone(&c);
        tokio::spawn(async move {
            let _ = core.http.join_thread(thread.id).await;
            // A forum post's first message can arrive before we joined;
            // fetch it so "watch new posts" tasks never miss one.
            if let (Some(guild_id), Some(parent)) = (thread.guild_id, thread.parent_id) {
                let watched = core.hive.knowledge.guild(guild_id.get(), |k| {
                    k.tasks.iter().any(|t| {
                        t.agent_id == core.id
                            && t.active
                            && t.trigger
                                == TaskTrigger::NewPost {
                                    channel_id: parent.get(),
                                }
                    })
                });
                if watched {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Ok(starter) = core
                        .http
                        .get_message(thread.id, MessageId::new(thread.id.get()))
                        .await
                    {
                        if !starter.author.bot.unwrap_or(false) {
                            super::tasks::on_message(&core, guild_id.get(), &starter);
                        }
                    }
                }
            }
        });
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_thread_update(move |_ctx, thread| {
        c.world.upsert_channel(&thread);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_thread_delete(move |_ctx, thread| {
        c.world.remove_channel(&thread);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_role_create(move |_ctx, change| {
        c.world.upsert_role(change.guild_id.get(), &change.role);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_role_update(move |_ctx, change| {
        c.world.upsert_role(change.guild_id.get(), &change.role);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_role_delete(move |_ctx, change| {
        c.world
            .remove_role(change.guild_id.get(), change.role_id.get());
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_member_add(move |_ctx, add| {
        c.world.upsert_member(add.guild_id.get(), &add.member);
        super::tasks::on_member_join(&c, add.guild_id.get(), &add.member);
        if let Some(user) = &add.member.user {
            super::levels::on_join(&c, add.guild_id.get(), user.id.get(), true);
        }
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_member_update(move |_ctx, update| {
        c.world.upsert_member(update.guild_id.get(), &update.member);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_member_remove(move |_ctx, remove| {
        c.world
            .remove_member(remove.guild_id.get(), remove.user.id.get());
        super::levels::on_join(&c, remove.guild_id.get(), remove.user.id.get(), false);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_guild_members_chunk(move |_ctx, chunk| {
        for member in &chunk.members {
            c.world.upsert_member(chunk.guild_id.get(), member);
        }
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_voice_state_update(move |_ctx, voice| {
        c.world.apply_voice(&voice);
        super::voice::on_state(&c, &voice);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_voice_server_update(move |_ctx, update| {
        super::voice::on_server(&c, &update);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_message_create(move |_ctx, message| {
        handle_message(&c, message);
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_message_update(move |_ctx, update| {
        if let Some(content) = update.content.clone() {
            c.hive
                .knowledge
                .channel_mut(update.channel_id.get(), |archive| {
                    if let Some(stored) = archive.get_mut(update.id.get()) {
                        stored.content = content;
                        stored.edited = true;
                    }
                });
        }
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_message_delete(move |_ctx, delete| {
        c.hive
            .knowledge
            .channel_mut(delete.channel_id.get(), |archive| {
                if let Some(stored) = archive.get_mut(delete.id.get()) {
                    stored.deleted = true;
                }
            });
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_reaction_add(move |_ctx, reaction| {
        let name = reaction.emoji.name.clone().unwrap_or_default();
        c.hive
            .knowledge
            .channel_mut(reaction.channel_id.get(), |archive| {
                if let Some(stored) = archive.get_mut(reaction.message_id.get()) {
                    match stored.reactions.iter_mut().find(|(e, _)| *e == name) {
                        Some((_, count)) => *count += 1,
                        None => stored.reactions.push((name, 1)),
                    }
                }
            });
        async { Ok(()) }
    });
    let c = Arc::clone(core);
    client.on_interaction_create(move |_ctx, interaction| {
        let core = Arc::clone(&c);
        async move {
            super::commands::handle(&core, interaction).await;
            Ok(())
        }
    });
    let c = Arc::clone(core);
    client.on_any(move |_ctx, event| {
        match event {
            Event::Reconnecting { .. } => c
                .live()
                .agent(&c.id, |l| l.status = AgentStatus::Reconnecting),
            Event::Resumed => c.live().agent(&c.id, |l| l.status = AgentStatus::Online),
            _ => {}
        }
        async { Ok(()) }
    });
}

/// Is the HQ channel / a channel a task watches?
pub fn task_channel_matches(trigger: &TaskTrigger, channel_id: u64, parent: Option<u64>) -> bool {
    match trigger {
        TaskTrigger::NewPost { channel_id: forum } => parent == Some(*forum),
        TaskTrigger::NewMessage {
            channel_id: watched,
        } => *watched == channel_id || parent == Some(*watched),
        _ => false,
    }
}

/// Whether a task's trigger fires in this channel, honoring its scope:
/// Origin = just the trigger channel (or its threads), Channels = the
/// listed channels (plus the trigger channel and their threads), Global =
/// anywhere in the server.
pub fn task_in_scope(
    task: &crate::memory::StandingTask,
    channel_id: u64,
    parent: Option<u64>,
) -> bool {
    match &task.scope {
        crate::memory::TaskScope::Origin => task_channel_matches(&task.trigger, channel_id, parent),
        crate::memory::TaskScope::Channels(extra) => {
            let watched = task.trigger.channel_id();
            extra.contains(&channel_id)
                || (parent.is_some_and(|p| extra.contains(&p)))
                || watched == channel_id
                || parent == Some(watched)
        }
        crate::memory::TaskScope::Global => matches!(
            task.trigger,
            crate::memory::TaskTrigger::NewMessage { .. }
                | crate::memory::TaskTrigger::NewPost { .. }
        ),
    }
}

/// Is this channel chill for this agent (extra brief, self-pruning)?
pub fn chilled(core: &Arc<AgentCore>, guild_id: u64, channel_id: u64) -> bool {
    let parent = core
        .world
        .channel(guild_id, channel_id)
        .and_then(|c| c.parent_id.map(|p| p.get()));
    core.hive.knowledge.guild(guild_id, |k| {
        k.chill.iter().any(|rule| {
            rule.agent_id.as_deref().is_none_or(|a| a == core.id)
                && (rule.channel_id == channel_id || Some(rule.channel_id) == parent)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_scope_matching() {
        use crate::memory::{StandingTask, TaskScope, TaskTrigger};
        let base = |scope: TaskScope, trigger: TaskTrigger| StandingTask {
            id: 1,
            agent_id: "a".to_owned(),
            instruction: String::new(),
            trigger,
            author_id: 1,
            author_name: "u".to_owned(),
            author_rank: 0,
            author_level: crate::config::AccessLevel::Everyone,
            created: 0,
            active: true,
            runs: 0,
            last_run: 0,
            last_result: String::new(),
            origin_channel: 0,
            scope,
        };
        let watch = |channel: u64| TaskTrigger::NewMessage {
            channel_id: channel,
        };
        // Origin: only its own channel (and threads of it).
        let t = base(TaskScope::Origin, watch(10));
        assert!(task_in_scope(&t, 10, None));
        assert!(task_in_scope(&t, 11, Some(10)));
        assert!(!task_in_scope(&t, 12, None));
        // Channels: the listed ones plus the trigger channel.
        let t = base(TaskScope::Channels(vec![20, 21]), watch(10));
        assert!(task_in_scope(&t, 10, None));
        assert!(task_in_scope(&t, 20, None));
        assert!(task_in_scope(&t, 30, Some(21)));
        assert!(!task_in_scope(&t, 22, None));
        // Global: anywhere.
        let t = base(TaskScope::Global, watch(10));
        assert!(task_in_scope(&t, 99, None));
        assert!(task_in_scope(&t, 98, Some(97)));
    }

    #[test]
    fn name_position_start_only_counts_first_two_words() {
        use crate::config::{AgentProfile, BehaviorConfig, NamePosition};
        fn profile(position: NamePosition) -> AgentProfile {
            AgentProfile {
                name: "magi".to_owned(),
                behavior: BehaviorConfig {
                    name_position: position,
                    ..BehaviorConfig::default()
                },
                ..AgentProfile::default()
            }
        }
        // Start mode: first word.
        assert!(named_in(&profile(NamePosition::Start), "magi help me"));
        // Second word ("hey magi, ...").
        assert!(named_in(
            &profile(NamePosition::Start),
            "hey magi what's 2+2"
        ));
        // Anywhere mode: deep in the sentence.
        assert!(named_in(
            &profile(NamePosition::Anywhere),
            "I was telling magi about it"
        ));
        // Start mode: deep in the sentence does NOT count.
        assert!(!named_in(
            &profile(NamePosition::Start),
            "I was telling magi about it"
        ));
        // Possessive second word still fine ("hey magi's").
        assert!(named_in(&profile(NamePosition::Start), "hey Magi, look"));
    }

    #[test]
    fn whole_word_name_matching() {
        assert!(mentions_word("hey nova, you there?", "nova"));
        assert!(mentions_word("Nova's take on this", "nova"));
        assert!(mentions_word("@nova", "nova"));
        assert!(mentions_word("NOVA", "nova"));
        assert!(!mentions_word("casanova is here", "nova"));
        assert!(!mentions_word("novas", "nova"));
        assert!(mentions_word("thanks vex!", "vex"));
        assert!(!mentions_word("", "nova"));
    }

    #[test]
    fn task_channel_matching() {
        assert!(task_channel_matches(
            &TaskTrigger::NewPost { channel_id: 5 },
            99,
            Some(5)
        ));
        assert!(!task_channel_matches(
            &TaskTrigger::NewPost { channel_id: 5 },
            5,
            None
        ));
        assert!(task_channel_matches(
            &TaskTrigger::NewMessage { channel_id: 5 },
            5,
            None
        ));
    }
}
