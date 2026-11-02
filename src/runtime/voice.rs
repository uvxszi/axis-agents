//! Voice channels: joining, hearing people, and talking back.
//!
//! Joining is a handshake with the main gateway: the agent asks to join
//! (op 4), Discord answers with the agent's own voice state (session ID)
//! and a voice server (token, endpoint); with both, `axis-voice` opens
//! the connection. Each connection gets a listener that cuts everyone's
//! audio into utterances, transcribes them, and — when they're talking to
//! the agent — runs the brain and speaks the answer.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use axis::prelude::*;
use axis_voice::{VoiceConnection, VoiceEvent, VoiceInfo};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::agent::AgentCore;
use super::authority::{Principal, PrincipalKind};
use super::brain::{self, Job, Outlet, Reason};
use super::live::FeedKind;
use super::speech;
use crate::config::VoiceReply;
use crate::memory::StoredMessage;

/// Loudness (RMS) above which a frame counts as speech.
const SPEECH_RMS: f64 = 450.0;
/// Longest single utterance.
const MAX_UTTERANCE: Duration = Duration::from_secs(25);
/// Shortest speech worth transcribing (20 ms frames).
const MIN_VOICED_FRAMES: usize = 12;
/// Talking over the agent this long stops it.
const BARGE_IN_FRAMES: usize = 25;
/// An answer within this long keeps the conversation going.
const CONVERSATION_WINDOW: Duration = Duration::from_secs(60);

/// One agent's voice connections, one per guild.
#[derive(Default)]
pub struct VoiceHub {
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    /// guild → the agent's voice session ID.
    session_ids: HashMap<u64, String>,
    /// guild → (token, endpoint).
    servers: HashMap<u64, (String, String)>,
    /// guild → the channel the agent is in or joining.
    channels: HashMap<u64, u64>,
    waiters: HashMap<u64, Vec<oneshot::Sender<Result<(), String>>>>,
    sessions: HashMap<u64, Arc<Session>>,
    connecting: HashSet<u64>,
    /// guild → the text channel the agent was asked to join from.
    origins: HashMap<u64, u64>,
}

pub struct Session {
    pub guild_id: u64,
    pub channel_id: u64,
    /// The text channel the agent was asked to join from.
    pub origin: Option<u64>,
    conn: VoiceConnection,
    /// user → when the agent last answered them here.
    answered: Mutex<HashMap<u64, Instant>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Keeps lines in order when several are being prepared at once.
    speaking: tokio::sync::Mutex<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        for task in self.tasks.lock().expect("voice poisoned").drain(..) {
            task.abort();
        }
    }
}

impl VoiceHub {
    /// The voice channel the agent is connected to in a guild.
    pub fn channel(&self, guild_id: u64) -> Option<u64> {
        self.session(guild_id).map(|s| s.channel_id)
    }

    /// The text channel the agent was invited from.
    pub fn origin(&self, guild_id: u64) -> Option<u64> {
        self.session(guild_id).and_then(|s| s.origin)
    }

    fn session(&self, guild_id: u64) -> Option<Arc<Session>> {
        self.state.lock().expect("voice poisoned").sessions.get(&guild_id).cloned()
    }

    fn fail_waiters(state: &mut HubState, guild_id: u64, error: &str) {
        for waiter in state.waiters.remove(&guild_id).unwrap_or_default() {
            let _ = waiter.send(Err(error.to_owned()));
        }
    }

    /// Hang up everywhere (agent stopping).
    pub fn shutdown(&self) {
        let mut state = self.state.lock().expect("voice poisoned");
        state.sessions.clear();
        state.channels.clear();
        let guilds: Vec<u64> = state.waiters.keys().copied().collect();
        for guild in guilds {
            Self::fail_waiters(&mut state, guild, "the agent stopped");
        }
    }
}

fn messenger(core: &AgentCore) -> Option<axis::gateway::ShardMessenger> {
    core.messenger.lock().expect("messenger poisoned").clone()
}

fn publish(core: &AgentCore) {
    let state = core.voice.state.lock().expect("voice poisoned");
    let channels: Vec<String> = state
        .sessions
        .values()
        .map(|s| core.channel_label(Some(s.guild_id), s.channel_id))
        .collect();
    drop(state);
    core.live().agent(&core.id, |l| l.voice_channels = channels.clone());
}

// --- gateway events ----------------------------------------------------------------

/// The agent's own voice state changed (joined, moved, left).
pub fn on_state(core: &Arc<AgentCore>, voice: &axis::model::VoiceState) {
    if voice.user_id.get() != core.world.my_id() {
        return;
    }
    let Some(guild_id) = voice.guild_id.map(|g| g.get()) else {
        return;
    };
    {
        let mut state = core.voice.state.lock().expect("voice poisoned");
        match voice.channel_id.map(|c| c.get()) {
            Some(channel_id) => {
                let changed = state.session_ids.get(&guild_id) != Some(&voice.session_id)
                    || state.sessions.get(&guild_id).is_some_and(|s| s.channel_id != channel_id);
                state.session_ids.insert(guild_id, voice.session_id.clone());
                state.channels.insert(guild_id, channel_id);
                if changed {
                    // Moved (or a new session): reconnect for the new channel.
                    state.sessions.remove(&guild_id);
                }
            }
            None => {
                state.session_ids.remove(&guild_id);
                state.servers.remove(&guild_id);
                state.channels.remove(&guild_id);
                if state.sessions.remove(&guild_id).is_some() {
                    core.feed(FeedKind::System, "left voice", None);
                }
                VoiceHub::fail_waiters(&mut state, guild_id, "disconnected from voice");
            }
        }
    }
    publish(core);
    try_connect(core, guild_id);
}

/// Discord assigned (or reassigned) a voice server.
pub fn on_server(core: &Arc<AgentCore>, update: &axis::model::VoiceServerUpdate) {
    let guild_id = update.guild_id.get();
    let Some(endpoint) = update.endpoint.clone() else {
        return;
    };
    {
        let mut state = core.voice.state.lock().expect("voice poisoned");
        state.servers.insert(guild_id, (update.token.clone(), endpoint));
        // A new server means a new connection.
        state.sessions.remove(&guild_id);
    }
    try_connect(core, guild_id);
}

fn try_connect(core: &Arc<AgentCore>, guild_id: u64) {
    let info = {
        let mut state = core.voice.state.lock().expect("voice poisoned");
        if state.sessions.contains_key(&guild_id) || state.connecting.contains(&guild_id) {
            return;
        }
        let (Some(channel_id), Some(session_id), Some((token, endpoint))) = (
            state.channels.get(&guild_id).copied(),
            state.session_ids.get(&guild_id).cloned(),
            state.servers.get(&guild_id).cloned(),
        ) else {
            return;
        };
        state.connecting.insert(guild_id);
        VoiceInfo {
            endpoint,
            token,
            session_id,
            guild_id,
            channel_id,
            user_id: core.world.my_id(),
        }
    };
    let core = Arc::clone(core);
    tokio::spawn(async move {
        let result = VoiceConnection::connect(info.clone()).await;
        let mut state = core.voice.state.lock().expect("voice poisoned");
        state.connecting.remove(&guild_id);
        match result {
            Ok((conn, events)) => {
                // Superseded while connecting (moved or left)?
                if state.channels.get(&guild_id) != Some(&info.channel_id) {
                    return;
                }
                let session = Arc::new(Session {
                    guild_id,
                    channel_id: info.channel_id,
                    origin: state.origins.get(&guild_id).copied(),
                    conn,
                    answered: Mutex::new(HashMap::new()),
                    tasks: Mutex::new(Vec::new()),
                    speaking: tokio::sync::Mutex::new(()),
                });
                let listener = tokio::spawn(listen(Arc::clone(&core), Arc::downgrade(&session), events));
                session.tasks.lock().expect("voice poisoned").push(listener);
                state.sessions.insert(guild_id, session);
                for waiter in state.waiters.remove(&guild_id).unwrap_or_default() {
                    let _ = waiter.send(Ok(()));
                }
                drop(state);
                core.feed(
                    FeedKind::System,
                    "joined voice",
                    Some(core.channel_label(Some(guild_id), info.channel_id)),
                );
                publish(&core);
            }
            Err(error) => {
                // join() retries with a fresh session.
                let message = format!("couldn't connect to voice: {error}");
                VoiceHub::fail_waiters(&mut state, guild_id, &message);
                drop(state);
                tracing::warn!(%message);
            }
        }
    });
}

// --- actions -------------------------------------------------------------------------

/// Join a voice channel (or move there). Resolves once connected;
/// retries with a fresh voice session when Discord's voice gateway hiccups.
pub async fn join(core: &Arc<AgentCore>, guild_id: u64, channel_id: u64, origin: Option<u64>) -> Result<(), String> {
    if let Some(origin) = origin {
        core.voice.state.lock().expect("voice poisoned").origins.insert(guild_id, origin);
    }
    if core.voice.channel(guild_id) == Some(channel_id) {
        return Ok(());
    }
    let mut last_error = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            // Start over: leave, forget the old session, and ask again.
            if let Some(messenger) = messenger(core) {
                let _ = messenger.update_voice_state(GuildId::new(guild_id), None, false, false);
            }
            tokio::time::sleep(Duration::from_millis(1500 * attempt)).await;
            let mut state = core.voice.state.lock().expect("voice poisoned");
            state.session_ids.remove(&guild_id);
            state.servers.remove(&guild_id);
            state.sessions.remove(&guild_id);
            state.channels.remove(&guild_id);
        }
        match join_once(core, guild_id, channel_id).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(attempt, %error, "voice join failed");
                last_error = error;
            }
        }
    }
    core.feed(FeedKind::Error, format!("couldn't join voice: {last_error}"), None);
    Err(format!("{last_error} (tried 3 times)"))
}

async fn join_once(core: &Arc<AgentCore>, guild_id: u64, channel_id: u64) -> Result<(), String> {
    let messenger = messenger(core).ok_or("not connected to Discord")?;
    let (tx, rx) = oneshot::channel();
    {
        let mut state = core.voice.state.lock().expect("voice poisoned");
        state.waiters.entry(guild_id).or_default().push(tx);
        // Details from an earlier connection would pair a new session
        // with an old token (4006); wait for fresh ones.
        if state.channels.get(&guild_id) != Some(&channel_id) {
            state.servers.remove(&guild_id);
        }
    }
    messenger
        .update_voice_state(GuildId::new(guild_id), Some(ChannelId::new(channel_id)), false, false)
        .map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_secs(15), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("voice join was cancelled".to_owned()),
        Err(_) => Err("Discord didn't let me into the voice channel (no response in 15s)".to_owned()),
    }
}

/// Leave voice in a guild.
pub fn leave(core: &Arc<AgentCore>, guild_id: u64) -> Result<(), String> {
    if let Some(messenger) = messenger(core) {
        messenger
            .update_voice_state(GuildId::new(guild_id), None, false, false)
            .map_err(|e| e.to_string())?;
    }
    {
        let mut state = core.voice.state.lock().expect("voice poisoned");
        state.sessions.remove(&guild_id);
        state.channels.remove(&guild_id);
        state.servers.remove(&guild_id);
        state.session_ids.remove(&guild_id);
    }
    publish(core);
    Ok(())
}

/// Say something in the voice channel the agent is in. Long text is
/// spoken sentence by sentence, starting as soon as the first is ready.
pub async fn say(core: &Arc<AgentCore>, guild_id: u64, text: &str) -> Result<(), String> {
    speak(core, guild_id, text, true).await
}

/// Say a short status line ("One sec, checking the web") without keeping
/// it in the conversation history.
pub async fn say_quietly(core: &Arc<AgentCore>, guild_id: u64, text: &str) -> Result<(), String> {
    speak(core, guild_id, text, false).await
}

/// What the agent says while a slow tool runs, so the call isn't silent.
pub fn progress_line(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "web_search" | "news_search" => "One sec, checking the web.",
        "fetch_url" | "read_url" => "Give me a sec, I'm reading that page.",
        "image_search" => "Looking for an image.",
        "read_messages" | "search_messages" | "list_forum_posts" | "read_forum_post" => "Let me look through the chat.",
        "recall" => "Let me think back.",
        "describe_image" | "look_at_image" => "Let me take a look.",
        "purge_messages" => "On it, cleaning up.",
        "send_message" => "Sending it now.",
        _ => return None,
    })
}

async fn speak(core: &Arc<AgentCore>, guild_id: u64, text: &str, keep: bool) -> Result<(), String> {
    let session = core.voice.session(guild_id).ok_or("I'm not in a voice channel")?;
    let spoken = spoken_text(core, guild_id, text);
    if spoken.is_empty() {
        return Ok(());
    }
    let profile = core.profile();
    if keep {
        archive(core, session.channel_id, core.world.my_id(), &profile.name, true, &spoken);
        if profile.voice.live_transcript {
            transcript_line(core, session.channel_id, "🔊", &profile.name, &spoken);
        }
    }
    let order = session.speaking.lock().await;
    let stops = session.conn.stop_count();
    let mut playing = Vec::new();
    for chunk in sentence_chunks(&spoken) {
        if session.conn.stop_count() != stops {
            break; // interrupted
        }
        let audio = speech::synthesize(&profile.voice, &chunk).await;
        if let Some(notice) = speech::take_notice() {
            core.feed(FeedKind::Error, notice, None);
        }
        let audio = audio?;
        let session = Arc::clone(&session);
        playing.push(tokio::spawn(async move { session.conn.play(audio).await }));
    }
    drop(order);
    for handle in playing {
        let _ = handle.await;
    }
    Ok(())
}

/// Split speech into pieces of a sentence or more (short sentences are
/// joined so the voice keeps its flow).
fn sentence_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        let end = matches!(c, '.' | '!' | '?' | ';' | '\n') && chars.peek().is_none_or(|n| n.is_whitespace());
        if end && current.trim().len() >= 40 {
            chunks.push(current.trim().to_owned());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_owned());
    }
    chunks
}

/// The brain's answer to someone in voice: spoken, and whatever it marked
/// for the call's text chat posted there.
pub async fn reply(core: &Arc<AgentCore>, guild_id: u64, speaker: u64, text: &str, files: &[(String, Vec<u8>)]) {
    let Some(session) = core.voice.session(guild_id) else {
        return;
    };
    let profile = core.profile();
    let (mut spoken, mut chat) = split_chat(text);
    // A chat part that just repeats what's said is noise.
    if chat.as_deref().is_some_and(|c| mostly_same(c, &spoken)) {
        chat = None;
    }
    // Too long (or full of links) to listen to: it belongs in the chat,
    // with a short line said out loud.
    if spoken.len() > 400 || spoken.matches("http").count() > 1 {
        let full = spoken.clone();
        chat = Some(match chat.take() {
            Some(extra) => format!("{full}\n{extra}"),
            None => full,
        });
        spoken = String::new();
    }
    // Never answer someone in a call only in writing.
    if spoken.trim().is_empty() {
        if let Some(written) = chat.take() {
            let plain = !written.contains("http") && !written.contains("```") && written.len() <= 300;
            if plain {
                // Short and speakable: just say it.
                spoken = written;
            } else {
                let first_line = written
                    .lines()
                    .map(|l| l.trim().trim_start_matches(['-', '*', '•', '#', ' ']).trim_end_matches(':').trim())
                    .find(|l| !l.is_empty())
                    .map(speech::speakable)
                    .unwrap_or_default();
                spoken = if first_line.is_empty() || first_line.len() > 120 {
                    "I put it in the chat.".to_owned()
                } else {
                    format!("{first_line} — it's in the chat.")
                };
                chat = Some(written);
            }
        }
    }
    let posted = match (&chat, profile.voice.post_transcript) {
        (Some(chat), true) if !spoken.trim().is_empty() => Some(format!("🔊 {}\n{chat}", spoken.trim())),
        (Some(chat), _) => Some(chat.clone()),
        // The live transcript already shows what it says.
        (None, true) if !spoken.trim().is_empty() && !profile.voice.live_transcript => {
            Some(format!("🔊 {}", spoken.trim()))
        }
        _ => None,
    };
    // The voice channel's chat may be off limits; speaking still works.
    let chat_ok = core.barred(guild_id, session.channel_id).is_none();
    if chat_ok && (posted.is_some() || !files.is_empty()) {
        let posted = posted.map(|p| super::brain::link_mentions(core, guild_id, &p)).unwrap_or_default();
        let mut request = core
            .http
            .create_message(ChannelId::new(session.channel_id))
            .content(crate::llm::truncate(&posted, 1990))
            .allowed_mentions(axis::model::AllowedMentions::users());
        for (name, bytes) in files {
            request = request.file(name.clone(), bytes.clone());
        }
        if let Err(error) = request.await {
            core.feed(FeedKind::Error, format!("couldn't post in the call's chat: {error}"), None);
        }
    }
    if spoken.trim().is_empty() {
        session.answered.lock().expect("voice poisoned").insert(speaker, Instant::now());
        return;
    }
    match say(core, guild_id, &spoken).await {
        Ok(()) => {
            session.answered.lock().expect("voice poisoned").insert(speaker, Instant::now());
            core.live().stat(&core.id, |s| s.replies += 1);
            core.feed(
                FeedKind::Reply,
                format!("said: {}", crate::llm::truncate(&spoken, 120)),
                Some(core.channel_label(Some(guild_id), session.channel_id)),
            );
        }
        Err(error) => core.feed(FeedKind::Error, format!("couldn't speak: {error}"), None),
    }
}

/// Do two texts say mostly the same words?
fn mostly_same(a: &str, b: &str) -> bool {
    let words = |t: &str| -> std::collections::HashSet<String> {
        t.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .map(str::to_lowercase)
            .collect()
    };
    let (a, b) = (words(a), words(b));
    if a.is_empty() {
        return true;
    }
    a.intersection(&b).count() * 10 >= a.len() * 6
}

fn marker_line<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let trimmed = line.trim_start().trim_start_matches(['*', '_']);
    let head = trimmed.get(..marker.len())?;
    head.eq_ignore_ascii_case(marker)
        .then(|| trimmed[marker.len()..].trim_start_matches(['*', '_']).trim_start())
}

/// A voice reply: the part to say, and (after a `CHAT:` line) the part to
/// post in the call's text chat.
pub fn split_chat(text: &str) -> (String, Option<String>) {
    let mut spoken = Vec::new();
    let mut chat: Option<Vec<&str>> = None;
    for line in text.lines() {
        if let Some(chat) = chat.as_mut() {
            chat.push(line);
        } else if let Some(rest) = marker_line(line, "CHAT:") {
            chat = Some(vec![rest]);
        } else if let Some(at) = line.find("CHAT:") {
            // Written mid-sentence: split there.
            spoken.push(&line[..at]);
            chat = Some(vec![line[at + 5..].trim_start()]);
        } else {
            spoken.push(line);
        }
    }
    let chat = chat
        .map(|lines| lines.join("\n").trim().to_owned())
        .filter(|c| !c.is_empty());
    (spoken.join("\n").trim().to_owned(), chat)
}

/// A text reply in the call's chat: `SAY:` lines are spoken in the call,
/// the rest is posted.
pub fn split_say(text: &str) -> (String, Vec<String>) {
    let mut kept = Vec::new();
    let mut said = Vec::new();
    for line in text.lines() {
        match marker_line(line, "SAY:") {
            Some(rest) if !rest.is_empty() => said.push(rest.to_owned()),
            Some(_) => {}
            None => kept.push(line),
        }
    }
    (kept.join("\n").trim().to_owned(), said)
}

/// People in the call (display names), for the prompt.
pub fn people_names(core: &AgentCore, guild_id: u64, channel_id: u64) -> Vec<String> {
    people_in(core, guild_id, channel_id)
        .into_iter()
        .filter_map(|u| {
            core.world
                .with_guild(guild_id, |g| g.members.get(&u).map(|m| m.display_name().to_owned()))
                .flatten()
        })
        .collect()
}

/// Mentions and channel links as names, then stripped of markup.
fn spoken_text(core: &AgentCore, guild_id: u64, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find('>') else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let inner = &tail[1..end];
        let id = inner.trim_start_matches(['@', '!', '#', '&']).parse::<u64>().ok();
        let name = id.and_then(|id| {
            core.world
                .with_guild(guild_id, |g| {
                    if inner.starts_with('#') {
                        g.channels.get(&id).and_then(|c| c.name.clone())
                    } else if inner.starts_with("@&") {
                        g.roles.get(&id).map(|r| r.name.clone())
                    } else {
                        g.members.get(&id).map(|m| m.display_name().to_owned())
                    }
                })
                .flatten()
        });
        match name {
            Some(name) => out.push_str(&name),
            // Custom emoji and the like are dropped.
            None if id.is_some() || inner.contains(':') => {}
            None => out.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    speech::speakable(&out)
}

/// Marks the agent's transcript lines (not archived as messages: the
/// voice history already has them).
pub fn is_transcript_line(content: &str) -> bool {
    content.starts_with("-# 🎙 ") || content.starts_with("-# 🔊 ")
}

/// Post one line of the live transcript: small (`-#`) and silent.
fn transcript_line(core: &Arc<AgentCore>, channel_id: u64, icon: &str, who: &str, text: &str) {
    if core.barred_anywhere(channel_id).is_some() {
        return;
    }
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let content = format!(
        "-# {icon} **{}**: {}",
        who.replace(['*', '_', '`'], ""),
        crate::llm::truncate(&line, 1800)
    );
    let core = Arc::clone(core);
    tokio::spawn(async move {
        let _ = core
            .http
            .create_message(ChannelId::new(channel_id))
            .content(content)
            .flags(axis::model::MessageFlags::SUPPRESS_NOTIFICATIONS)
            .allowed_mentions(axis::model::AllowedMentions::none())
            .await;
    });
}

/// Keep what's said in voice in the channel's history, so the agent
/// remembers voice conversations like text ones.
fn archive(core: &AgentCore, channel_id: u64, author_id: u64, author: &str, bot: bool, text: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let id = (((ms - 1_420_070_400_000).max(0) as u64) << 22) | fastrand::u64(..1 << 22);
    core.hive.knowledge.channel_mut(channel_id, |archive| {
        archive.push(StoredMessage {
            id,
            author_id,
            author: author.to_owned(),
            author_bot: bot,
            content: format!("🎙 (said in voice) {text}"),
            ts: ms,
            reply_to: None,
            reply_author: None,
            reply_snippet: None,
            attachments: Vec::new(),
            edited: false,
            deleted: false,
            reactions: Vec::new(),
        });
    });
}

// --- listening -----------------------------------------------------------------------

struct Utterance {
    user_id: Option<u64>,
    audio: Vec<i16>,
    voiced: usize,
    started: Instant,
    last_voice: Instant,
}

fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    (pcm.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
}

/// Is this user someone the agent should listen to (not a bot)?
fn is_person(core: &AgentCore, guild_id: u64, user_id: u64) -> bool {
    core.hive.peer_by_user(user_id).is_none()
        && core
            .world
            .with_guild(guild_id, |g| g.members.get(&user_id).is_none_or(|m| !m.bot))
            .unwrap_or(true)
}

/// People (not bots) in a voice channel, besides the agent.
fn people_in(core: &AgentCore, guild_id: u64, channel_id: u64) -> Vec<u64> {
    let me = core.world.my_id();
    let users: Vec<u64> = core
        .world
        .with_guild(guild_id, |g| {
            g.voice
                .iter()
                .filter(|(user, channel)| **channel == channel_id && **user != me)
                .map(|(user, _)| *user)
                .collect()
        })
        .unwrap_or_default();
    users.into_iter().filter(|u| is_person(core, guild_id, *u)).collect()
}

async fn listen(core: Arc<AgentCore>, session: Weak<Session>, mut events: mpsc::UnboundedReceiver<VoiceEvent>) {
    let (heard_tx, heard_rx) = mpsc::unbounded_channel::<(u64, Vec<i16>)>();
    let responder = tokio::spawn(respond(Arc::clone(&core), session.clone(), heard_rx));
    let mut speech: HashMap<u32, Utterance> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut alone_since: Option<Instant> = None;
    let mut last_alone_check = Instant::now();
    let (guild_id, channel_id) = match session.upgrade() {
        Some(s) => (s.guild_id, s.channel_id),
        None => return,
    };
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                let Some(session) = session.upgrade() else { break };
                match event {
                    VoiceEvent::Audio { user_id, ssrc, pcm } => {
                        let profile = core.profile();
                        if !profile.voice.listen || core.paused.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        if user_id.is_some_and(|u| !is_person(&core, guild_id, u)) {
                            continue;
                        }
                        let now = Instant::now();
                        let loud = rms(&pcm) > SPEECH_RMS;
                        let entry = speech.entry(ssrc).or_insert_with(|| Utterance {
                            user_id,
                            audio: Vec::new(),
                            voiced: 0,
                            started: now,
                            last_voice: now,
                        });
                        entry.user_id = entry.user_id.or(user_id);
                        if loud {
                            if entry.voiced == 0 {
                                entry.started = now;
                            }
                            entry.voiced += 1;
                            entry.last_voice = now;
                        }
                        if entry.voiced > 0 {
                            entry.audio.extend(axis_voice::to_mono(&pcm, 16_000));
                        }
                        if profile.voice.interruptible
                            && entry.voiced >= BARGE_IN_FRAMES
                            && session.conn.is_playing()
                        {
                            session.conn.stop();
                        }
                    }
                    VoiceEvent::Closed { reason, code } => {
                        let still_current = core
                            .voice
                            .session(guild_id)
                            .is_some_and(|s| Arc::ptr_eq(&s, &session));
                        drop(session);
                        if still_current {
                            core.voice.state.lock().expect("voice poisoned").sessions.remove(&guild_id);
                            publish(&core);
                            // 4014: removed from the channel (kicked, or the
                            // channel was deleted) — don't force back in.
                            if code != Some(4014) {
                                core.feed(FeedKind::System, format!("voice dropped ({reason}); reconnecting"), None);
                                let core = Arc::clone(&core);
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    let origin = core.voice.state.lock().expect("voice poisoned").origins.get(&guild_id).copied();
                                    if let Err(error) = join(&core, guild_id, channel_id, origin).await {
                                        core.feed(FeedKind::Error, format!("couldn't rejoin voice: {error}"), None);
                                    }
                                });
                            }
                        }
                        break;
                    }
                    VoiceEvent::Speaking { .. } | VoiceEvent::UserLeft { .. } => {}
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                let end_of_speech = Duration::from_millis(core.profile().voice.pause_ms.clamp(400, 5000) as u64);
                let done: Vec<u32> = speech
                    .iter()
                    .filter(|(_, u)| {
                        (u.voiced > 0 && now.duration_since(u.last_voice) > end_of_speech)
                            || (u.voiced == 0 && now.duration_since(u.started) > end_of_speech)
                            || now.duration_since(u.started) > MAX_UTTERANCE
                    })
                    .map(|(ssrc, _)| *ssrc)
                    .collect();
                for ssrc in done {
                    let Some(utterance) = speech.remove(&ssrc) else { continue };
                    let user_id = utterance.user_id.or_else(|| session.upgrade().and_then(|s| s.conn.user_for(ssrc)));
                    if let (Some(user), true) = (user_id, utterance.voiced >= MIN_VOICED_FRAMES) {
                        let _ = heard_tx.send((user, utterance.audio));
                    }
                }
                if now.duration_since(last_alone_check) > Duration::from_secs(5) {
                    last_alone_check = now;
                    if core.profile().voice.leave_when_alone && people_in(&core, guild_id, channel_id).is_empty() {
                        let since = *alone_since.get_or_insert(now);
                        if now.duration_since(since) > Duration::from_secs(30) {
                            core.feed(FeedKind::System, "left voice — nobody's there", None);
                            let _ = leave(&core, guild_id);
                            break;
                        }
                    } else {
                        alone_since = None;
                    }
                }
            }
        }
    }
    responder.abort();
}

/// Recent lines in the call, as context for speech recognition.
fn recent_speech(core: &AgentCore, channel_id: u64) -> String {
    let lines: Vec<String> = core.hive.knowledge.channel(channel_id, |archive| {
        archive
            .messages
            .iter()
            .rev()
            .filter(|m| !m.deleted)
            .take(4)
            .map(|m| m.content.trim_start_matches("🎙 (said in voice) ").to_owned())
            .collect()
    });
    let mut text: String = lines.into_iter().rev().collect::<Vec<_>>().join(" ");
    if text.len() > 400 {
        let cut = text.len() - 400;
        let cut = (cut..text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
        text = text[cut..].to_owned();
    }
    text
}

/// Transcribe what people said and answer. Everything said while the
/// agent was busy becomes one turn — no backlog of stale answers.
async fn respond(core: Arc<AgentCore>, session: Weak<Session>, mut heard: mpsc::UnboundedReceiver<(u64, Vec<i16>)>) {
    let mut reported_error = false;
    while let Some(first) = heard.recv().await {
        let mut batch = vec![first];
        while let Ok(more) = heard.try_recv() {
            batch.push(more);
        }
        let Some(current) = session.upgrade() else { return };
        let (guild_id, channel_id) = (current.guild_id, current.channel_id);
        drop(current);
        let profile = core.profile();
        let mut names = vec![profile.name.clone()];
        names.extend(profile.aliases.iter().cloned());

        // (speaker, name, what they said), oldest first.
        let mut said: Vec<(u64, String, String)> = Vec::new();
        for (user_id, audio) in batch {
            let context = recent_speech(&core, channel_id);
            let text = match speech::transcribe(&profile.voice, &audio, &names, &context).await {
                Ok(text) => {
                    reported_error = false;
                    text
                }
                Err(error) => {
                    if !reported_error {
                        core.feed(FeedKind::Error, format!("couldn't transcribe voice: {error}"), None);
                        reported_error = true;
                    }
                    continue;
                }
            };
            if text.is_empty() {
                continue;
            }
            let name = core
                .world
                .with_guild(guild_id, |g| g.members.get(&user_id).map(|m| m.display_name().to_owned()))
                .flatten()
                .unwrap_or_else(|| "someone".to_owned());
            archive(&core, channel_id, user_id, &name, false, &text);
            if profile.voice.live_transcript {
                transcript_line(&core, channel_id, "🎙", &name, &text);
            }
            core.live().agent(&core.id, |l| {
                l.voice_heard.push((name.clone(), text.clone()));
                if l.voice_heard.len() > 12 {
                    l.voice_heard.remove(0);
                }
            });
            said.push((user_id, name, text));
        }
        core.live().repaint();
        // Answer the latest speaker, taking everything they said together
        // (a sentence split by a pause, or "no, I meant…" said while busy).
        let Some((user_id, name, _)) = said.last().cloned() else { continue };
        let text = said
            .iter()
            .filter(|(u, _, _)| *u == user_id)
            .map(|(_, _, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        // Talking to the agent?
        let named = heard_name(&text, &names);
        let Some(current) = session.upgrade() else { return };
        let recent = current
            .answered
            .lock()
            .expect("voice poisoned")
            .get(&user_id)
            .is_some_and(|t| t.elapsed() < CONVERSATION_WINDOW);
        drop(current);
        let one_on_one = people_in(&core, guild_id, channel_id).len() <= 1;
        let reason = match profile.voice.reply {
            _ if named => Reason::Direct,
            VoiceReply::Conversation if one_on_one => Reason::Direct,
            VoiceReply::Conversation if recent => Reason::Conversation,
            _ => continue,
        };
        // Asked to do something: say so right away instead of going quiet.
        if brain::looks_like_request(&text) {
            const ACKS: [&str; 5] = ["On it.", "Sure, one sec.", "Okay, give me a moment.", "Got it, working on it.", "Alright, on it."];
            let ack = ACKS[fastrand::usize(..ACKS.len())];
            let core = Arc::clone(&core);
            tokio::spawn(async move {
                let _ = say_quietly(&core, guild_id, ack).await;
            });
        }
        let requester = core
            .world
            .with_guild(guild_id, |g| match g.members.get(&user_id) {
                Some(member) => Principal::for_member(g, member, PrincipalKind::Human),
                None => Principal::unknown_member(g, user_id, name.clone()),
            })
            .unwrap_or_else(|| Principal::dm_user(user_id, name.clone()));
        let job = Job {
            guild_id: Some(guild_id),
            channel_id,
            triggers: Vec::new(),
            reason,
            requester,
            task: None,
            outlet: Outlet::Voice { guild_id, speaker: user_id },
            question: Some(text),
        };
        brain::run(&core, job).await;
    }
}

/// Did they say the agent's name? Speech recognition misspells names
/// ("Maggie" for "Magi"), so close matches count.
pub fn heard_name(text: &str, names: &[String]) -> bool {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    names.iter().any(|name| {
        let name = name.to_lowercase();
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        if name.contains(' ') {
            return text.to_lowercase().contains(name);
        }
        words.iter().any(|word| {
            word == name
                || (name.len() >= 4 && edit_distance(word, name) <= 1)
                || (name.len() >= 3
                    && word.chars().next() == name.chars().next()
                    && word.len().abs_diff(name.len()) <= 2
                    && skeleton(word) == skeleton(name)
                    && skeleton(name).len() >= 2)
        })
    })
}

/// Consonants without repeats: "maggie" and "magi" are both "mg".
fn skeleton(word: &str) -> String {
    let mut out = String::new();
    for c in word.chars().filter(|c| !"aeiouy".contains(*c)) {
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row.push((prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_chat_is_noise() {
        assert!(mostly_same(
            "Not much! Just here and ready to help with whatever you need.",
            "Not much! Just here and ready to help with whatever you need. How can I assist?"
        ));
        assert!(!mostly_same("https://example.com/cow.jpg", "Here's a cow."));
    }

    #[test]
    fn chunks_speech_by_sentence() {
        let chunks = sentence_chunks("Hi. Sure thing, I moved you to the gaming channel. Anything else you need from me today?");
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert!(chunks[0].starts_with("Hi. Sure thing"));
        assert_eq!(sentence_chunks("ok"), vec!["ok".to_owned()]);
    }

    #[test]
    fn splits_spoken_and_chat_parts() {
        let (spoken, chat) = split_chat("Sure, here's the link.\nCHAT: https://example.com\n- item");
        assert_eq!(spoken, "Sure, here's the link.");
        assert_eq!(chat.as_deref(), Some("https://example.com\n- item"));
        let (spoken, chat) = split_chat("Just talking.");
        assert_eq!((spoken.as_str(), chat), ("Just talking.", None));
        let (text, said) = split_say("Here you go:\n**SAY:** I posted it in chat\n- a");
        assert_eq!(text, "Here you go:\n- a");
        assert_eq!(said, vec!["I posted it in chat".to_owned()]);
    }

    #[test]
    fn hears_names_through_misspellings() {
        let names = vec!["Magi".to_owned()];
        assert!(heard_name("hey magi what's up", &names));
        assert!(heard_name("Maggie, can you hear me?", &names));
        assert!(heard_name("Magie are you there", &names));
        assert!(!heard_name("I made a game yesterday", &names));
        assert!(!heard_name("what is this", &names));
        let names = vec!["Nova".to_owned()];
        assert!(heard_name("nova!", &names));
        assert!(!heard_name("casanova is here", &names));
    }
}
