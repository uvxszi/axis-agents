//! Live translation: an agent translates a conversation as it happens —
//! one person into a language, or two people who don't share one into
//! each other's — until asked to stop or the conversation goes quiet.
//! When it notices people writing past each other in different languages,
//! it can offer (if it's chatty enough to chime in at all).

use std::collections::BTreeMap;
use std::sync::Arc;

use axis::model::Message;
use serde::{Deserialize, Serialize};

use super::agent::AgentCore;
use super::brain::{self, Job, Outlet, Reason};
use super::hive::ClaimKind;
use super::live::FeedKind;
use crate::memory::now_ms;

/// A conversation being translated.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslationSession {
    pub agent_id: String,
    pub channel_id: u64,
    /// Whose messages to translate (empty = everyone's).
    pub users: Vec<u64>,
    /// Translate into this language; empty = between the participants'
    /// own languages.
    pub target: String,
    /// The language each person writes in.
    pub languages: BTreeMap<u64, String>,
    pub started_by: u64,
    pub started: i64,
    pub last_activity: i64,
    /// Stop after this long without messages.
    pub idle_minutes: u32,
}

/// A rough guess at a message's language, from its script or common words.
pub fn guess_language(text: &str) -> Option<&'static str> {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() < 4 {
        return None;
    }
    let share = |test: fn(char) -> bool| letters.iter().filter(|c| test(**c)).count() * 2 > letters.len();
    if share(|c| ('\u{3040}'..='\u{30FF}').contains(&c)) {
        return Some("Japanese");
    }
    if share(|c| ('\u{AC00}'..='\u{D7AF}').contains(&c) || ('\u{1100}'..='\u{11FF}').contains(&c)) {
        return Some("Korean");
    }
    if share(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)) {
        return Some("Chinese");
    }
    if share(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
        return Some(if text.contains('і') || text.contains('ї') || text.contains('є') { "Ukrainian" } else { "Russian" });
    }
    if share(|c| ('\u{0600}'..='\u{06FF}').contains(&c)) {
        return Some("Arabic");
    }
    if share(|c| ('\u{0900}'..='\u{097F}').contains(&c)) {
        return Some("Hindi");
    }
    if share(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c)) {
        return Some("Thai");
    }
    if share(|c| ('\u{0370}'..='\u{03FF}').contains(&c)) {
        return Some("Greek");
    }
    if share(|c| ('\u{0590}'..='\u{05FF}').contains(&c)) {
        return Some("Hebrew");
    }
    // Latin script: count each language's most common words.
    const WORDS: [(&str, &[&str]); 10] = [
        ("English", &["the", "and", "is", "you", "it", "that", "what", "to", "are", "this", "i", "my", "do", "have", "not", "with"]),
        ("Spanish", &["el", "la", "que", "de", "y", "es", "no", "en", "los", "por", "qué", "como", "pero", "tengo", "eres", "estoy", "hola", "gracias", "muy"]),
        ("French", &["le", "la", "et", "est", "je", "tu", "les", "des", "pas", "que", "une", "vous", "c'est", "bonjour", "merci", "oui", "avec", "mais"]),
        ("German", &["der", "die", "das", "und", "ist", "ich", "nicht", "du", "ein", "eine", "mit", "auf", "wie", "danke", "ja", "aber", "was"]),
        ("Portuguese", &["o", "a", "que", "não", "de", "é", "um", "uma", "você", "eu", "com", "obrigado", "olá", "tudo", "bem", "mas", "isso"]),
        ("Italian", &["il", "che", "di", "non", "è", "un", "una", "sono", "per", "ciao", "grazie", "come", "anche", "questo", "ma", "io"]),
        ("Dutch", &["de", "het", "een", "en", "is", "ik", "niet", "je", "dat", "wat", "van", "hoe", "dank", "maar", "ook"]),
        ("Turkish", &["ve", "bir", "bu", "ne", "ben", "sen", "değil", "için", "nasıl", "evet", "hayır", "teşekkürler", "merhaba", "çok"]),
        ("Polish", &["i", "nie", "to", "jest", "się", "że", "na", "jak", "co", "tak", "ale", "dziękuję", "cześć", "mam"]),
        ("Indonesian", &["dan", "yang", "di", "ini", "itu", "tidak", "saya", "kamu", "apa", "ada", "terima", "kasih", "aku", "juga"]),
    ];
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    if words.len() < 2 {
        return None;
    }
    let mut best: Option<(&str, usize)> = None;
    let mut second = 0;
    for (language, common) in WORDS {
        let hits = words.iter().filter(|w| common.contains(&w.as_str())).count();
        match best {
            Some((_, top)) if hits > top => {
                second = top;
                best = Some((language, hits));
            }
            Some(_) => second = second.max(hits),
            None => best = Some((language, hits)),
        }
    }
    // Confident only with a clear winner.
    best.filter(|(_, hits)| *hits >= 2 && *hits > second).map(|(language, _)| language)
}

pub fn session_in(core: &AgentCore, guild_id: u64, channel_id: u64) -> Option<TranslationSession> {
    core.hive.knowledge.guild(guild_id, |k| {
        k.translations
            .iter()
            .find(|s| s.channel_id == channel_id && s.agent_id == core.id)
            .cloned()
    })
}

pub fn start(core: &AgentCore, guild_id: u64, mut session: TranslationSession) -> TranslationSession {
    session.agent_id = core.id.clone();
    session.started = now_ms();
    session.last_activity = now_ms();
    if session.idle_minutes == 0 {
        session.idle_minutes = 15;
    }
    core.hive.knowledge.guild_mut(guild_id, |k| {
        k.translations.retain(|s| !(s.channel_id == session.channel_id && s.agent_id == core.id));
        k.translations.push(session.clone());
    });
    core.feed(FeedKind::System, "started translating", Some(core.channel_label(Some(guild_id), session.channel_id)));
    session
}

pub fn stop(core: &AgentCore, guild_id: u64, channel_id: u64) -> bool {
    core.hive.knowledge.guild_mut(guild_id, |k| {
        let before = k.translations.len();
        k.translations.retain(|s| !(s.channel_id == channel_id && s.agent_id == core.id));
        before != k.translations.len()
    })
}

/// What a translation session did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// No session here.
    No,
    /// Translated (or skipped as not a participant's).
    Translating,
    /// Someone asked to stop; it stopped and said so.
    Stopped,
}

fn asks_to_stop(text: &str) -> bool {
    let t = text.to_lowercase();
    ["stop translat", "no more translat", "don't translate", "dont translate", "quit translat", "end translat", "deja de traducir", "para de traducir", "arrête de traduire"]
        .iter()
        .any(|p| t.contains(p))
}

/// A new message: translate it if this channel is being translated.
pub fn on_message(core: &Arc<AgentCore>, guild_id: u64, message: &Message) -> Handled {
    if message.author.bot.unwrap_or(false) {
        return Handled::No;
    }
    let channel_id = message.channel_id.get();
    let Some(session) = session_in(core, guild_id, channel_id) else {
        return Handled::No;
    };
    if asks_to_stop(&message.content) {
        stop(core, guild_id, channel_id);
        let core = Arc::clone(core);
        tokio::spawn(async move {
            let _ = core
                .http
                .raw(
                    "POST",
                    &format!("/channels/{channel_id}/messages"),
                    Some(serde_json::json!({ "content": "-# 🌐 Stopped translating.", "allowed_mentions": { "parse": [] } })),
                    None,
                )
                .await;
        });
        return Handled::Stopped;
    }
    let author = message.author.id.get();
    if !session.users.is_empty() && !session.users.contains(&author) {
        return Handled::Translating;
    }
    let text = message.content.trim().to_owned();
    if text.is_empty() {
        return Handled::Translating;
    }
    core.hive.knowledge.guild_mut(guild_id, |k| {
        if let Some(s) = k.translations.iter_mut().find(|s| s.channel_id == channel_id && s.agent_id == core.id) {
            s.last_activity = now_ms();
            if !s.users.is_empty() || s.target.is_empty() {
                if !s.users.contains(&author) && s.target.is_empty() {
                    s.users.push(author);
                }
            }
        }
    });
    let core = Arc::clone(core);
    let message = message.clone();
    tokio::spawn(async move {
        if let Err(error) = translate_message(&core, guild_id, &session, &message, &text).await {
            tracing::warn!(%error, "translation failed");
        }
    });
    Handled::Translating
}

#[derive(Deserialize)]
struct Translated {
    #[serde(default)]
    language: String,
    #[serde(default)]
    translations: BTreeMap<String, String>,
}

async fn translate_message(core: &AgentCore, guild_id: u64, session: &TranslationSession, message: &Message, text: &str) -> Result<(), String> {
    // Never post in a channel it was told never to chat in.
    if core.barred(guild_id, session.channel_id).is_some() {
        return Ok(());
    }
    let author = message.author.id.get();
    let guessed = guess_language(text).map(str::to_owned);
    let own = session.languages.get(&author).cloned().or(guessed.clone());
    // Into the fixed language, or into everyone else's.
    let mut targets: Vec<String> = if session.target.trim().is_empty() {
        let mut others: Vec<String> = session
            .languages
            .iter()
            .filter(|(user, _)| **user != author)
            .map(|(_, language)| language.clone())
            .collect();
        others.sort();
        others.dedup();
        if others.is_empty() {
            others.push("English".to_owned());
        }
        others
    } else {
        vec![session.target.trim().to_owned()]
    };
    targets.retain(|t| own.as_deref().is_none_or(|o| !o.eq_ignore_ascii_case(t)));
    let (language, translations) = translate_text(core, text, &targets).await?;
    let language = if language.is_empty() { own.unwrap_or_default() } else { language };
    if !language.is_empty() {
        core.hive.knowledge.guild_mut(guild_id, |k| {
            if let Some(s) = k.translations.iter_mut().find(|s| s.channel_id == session.channel_id && s.agent_id == core.id) {
                s.languages.insert(author, language.clone());
            }
        });
    }
    let lines: Vec<(String, String)> = translations
        .into_iter()
        .filter(|(target, t)| !t.trim().is_empty() && !target.eq_ignore_ascii_case(&language) && t.trim() != text.trim())
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    let body = if lines.len() == 1 {
        let (target, translation) = &lines[0];
        format!("{translation}\n-# 🌐 {language} → {target}")
    } else {
        let mut out = lines.iter().map(|(t, tr)| format!("**{t}:** {tr}")).collect::<Vec<_>>().join("\n");
        out.push_str(&format!("\n-# 🌐 from {language}"));
        out
    };

    let _ = core
        .http
        .raw(
            "POST",
            &format!("/channels/{}/messages", session.channel_id),
            Some(serde_json::json!({
                "content": crate::llm::truncate(&body, 1990),
                "message_reference": { "message_id": message.id.to_string(), "fail_if_not_exists": false },
                "allowed_mentions": { "parse": [], "replied_user": false },
            })),
            None,
        )
        .await;
    Ok(())
}

/// Translate text into each target language (skipping ones it's already
/// in). Returns the language it's written in and the translations.
pub async fn translate_text(core: &AgentCore, text: &str, targets: &[String]) -> Result<(String, Vec<(String, String)>), String> {
    let system = format!(
        "You translate chat messages. Reply with JSON only: {{\"language\": \"<the language the message is written in, in English>\", \"translations\": {{\"<target language>\": \"<translation>\"}}}}.          Translate into: {}. Leave out any target the message is already written in. Keep the tone, slang, names, and emojis; translate meaning, not word for word; add nothing.",
        if targets.is_empty() { "(none — just report the language)".to_owned() } else { targets.join(", ") }
    );
    let raw = brain::quick_completion(core, &system, text, 600, true).await?;
    let raw = crate::llm::text::split_think(&raw).0;
    let start = raw.find('{').ok_or("no JSON in the translation")?;
    let end = raw.rfind('}').ok_or("no JSON in the translation")?;
    let parsed: Translated = serde_json::from_str(&raw[start..=end]).map_err(|e| e.to_string())?;
    Ok((parsed.language.trim().to_owned(), parsed.translations.into_iter().collect()))
}

/// End sessions whose conversation went quiet (called every minute).
pub async fn tick(core: &AgentCore) {
    let now = now_ms();
    for guild_id in core.world.guild_ids() {
        let ended: Vec<u64> = core.hive.knowledge.guild_mut(guild_id, |k| {
            let mut ended = Vec::new();
            k.translations.retain(|s| {
                let idle = s.agent_id == core.id && now - s.last_activity > s.idle_minutes.max(1) as i64 * 60_000;
                if idle {
                    ended.push(s.channel_id);
                }
                !idle
            });
            ended
        });
        for channel_id in ended {
            let _ = core
                .http
                .raw(
                    "POST",
                    &format!("/channels/{channel_id}/messages"),
                    Some(serde_json::json!({ "content": "-# 🌐 Stopped translating — the conversation went quiet.", "allowed_mentions": { "parse": [] } })),
                    None,
                )
                .await;
        }
    }
}

/// People writing past each other in different languages? Then let the
/// agent decide (as when chiming in) whether to offer translating.
pub fn maybe_offer(core: &Arc<AgentCore>, guild_id: u64, message: &Message) {
    let profile = core.profile();
    if profile.behavior.chattiness == 0 || message.author.bot.unwrap_or(false) {
        return;
    }
    let channel_id = message.channel_id.get();
    let Some(mine) = guess_language(&message.content) else { return };
    let author = message.author.id.get();
    let since = now_ms() - 10 * 60_000;
    // The other people talking here recently, and their languages.
    let others: Vec<(u64, String, &'static str)> = core.hive.knowledge.channel(channel_id, |archive| {
        let mut seen: Vec<(u64, String, &'static str)> = Vec::new();
        for m in archive.messages.iter().rev().take(12) {
            if m.ts < since || m.author_bot || m.author_id == author || seen.iter().any(|(u, _, _)| *u == m.author_id) {
                continue;
            }
            if let Some(language) = guess_language(&m.content) {
                seen.push((m.author_id, m.author.clone(), language));
            }
        }
        seen
    });
    let Some((other, other_name, theirs)) = others.into_iter().find(|(_, _, l)| *l != mine) else {
        return;
    };
    // At most one offer per channel per half hour, across all agents.
    let key = channel_id ^ ((now_ms() / 1_800_000) as u64).wrapping_mul(0x51_7CC1_B727_220A);
    if !core.hive.claim_once(key, ClaimKind::AmbientReply) {
        return;
    }
    let name = super::agent::display_name(message);
    let hint = format!(
        "{name} is writing in {mine} and {other_name} in {theirs} — they may not understand each other. \
         If translating would help them, call start_translating for both (users: \"{name}, {other_name}\", no fixed language) and tell them in one short line in both languages that you'll translate (and that they can ask you to stop). \
         If they clearly understand each other, reply exactly NO_REPLY."
    );
    let job = Job {
        guild_id: Some(guild_id),
        channel_id,
        triggers: vec![message.clone()],
        reason: Reason::Ambient,
        requester: core.principal_for(Some(guild_id), &message.author),
        task: None,
        outlet: Outlet::Channel,
        question: Some(hint),
    };
    let _ = other;
    let core = Arc::clone(core);
    tokio::spawn(async move { brain::run(&core, job).await });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_common_languages() {
        assert_eq!(guess_language("hey what are you doing tonight"), Some("English"));
        assert_eq!(guess_language("hola, qué tal? estoy muy bien gracias"), Some("Spanish"));
        assert_eq!(guess_language("bonjour, c'est vraiment pas mal je pense"), Some("French"));
        assert_eq!(guess_language("ich weiß nicht was das ist"), Some("German"));
        assert_eq!(guess_language("привет как дела"), Some("Russian"));
        assert_eq!(guess_language("こんにちは、元気ですか"), Some("Japanese"));
        assert_eq!(guess_language("lol"), None);
        assert_eq!(guess_language("ok ok"), None);
    }
}
