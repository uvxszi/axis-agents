//! Watching for rule-breaking.
//!
//! Cheap heuristics run on every message (spam, floods, mass mentions,
//! invite links, blocked words, scam patterns). Clear-cut hits are acted
//! on; borderline ones (shouting, hostile language) go to the model for a
//! judgement that weighs the server's rules *and its culture* — friendly
//! banter in a server that jokes that way is not harassment.
//!
//! Staff, the owner, and fellow agents are never moderated. Only one agent
//! handles a given message (hive claim). Acting requires the agent's
//! autonomy to be at least "Moderator"; below that it only reports.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use axis::model::{AllowedMentions, Message};
use axis::prelude::{ChannelId, MessageId, Permissions};
use serde_json::json;

use super::agent::AgentCore;
use super::brain;
use super::hive::ClaimKind;
use super::live::FeedKind;
use crate::config::{AccessLevel, Autonomy};
use crate::memory::now_ms;
use crate::tools::{self, ToolCtx};

#[derive(Debug, Clone, PartialEq)]
pub enum Flag {
    /// Remove it: the rule is unambiguous.
    Hard(String),
    /// Ask the model.
    Soft(String),
}

const SCAM_PHRASES: &[&str] = &[
    "free nitro",
    "discord nitro for free",
    "steam gift",
    "claim your nitro",
    "airdrop",
    "nitro giveaway",
    "@everyone free",
    "gift for you",
    "click here to claim",
];
const HOSTILE_PHRASES: &[&str] = &[
    "kill yourself",
    "kys",
    "go die",
    "neck yourself",
    "retard",
    "i will find you",
    "dox",
];

fn fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.trim().to_lowercase().hash(&mut hasher);
    hasher.finish()
}

/// Run the heuristics. Pure except for the per-user history it records.
pub fn evaluate(core: &AgentCore, guild_id: u64, message: &Message) -> Option<Flag> {
    let config = core.profile().behavior.moderation;
    let text = message.content.to_lowercase();
    let author = message.author.id.get();

    // History for flood/duplicate detection.
    let (recent, duplicates) = {
        let mut history = core.recent_by_user.lock().expect("history poisoned");
        let entries = history.entry((guild_id, author)).or_default();
        let now = now_ms();
        entries.retain(|(at, _)| now - at < 60_000);
        let print = fingerprint(&message.content);
        entries.push((now, print));
        let recent = entries.iter().filter(|(at, _)| now - at < 8_000).count();
        let duplicates = if message.content.trim().len() >= 3 {
            entries.iter().filter(|(_, p)| *p == print).count()
        } else {
            0
        };
        (recent as u32, duplicates as u32)
    };

    for word in &config.blocked_words {
        let word = word.trim().to_lowercase();
        if !word.is_empty() && super::agent::mentions_word(&text, &word) {
            return Some(Flag::Hard(format!("used a blocked word (\"{word}\")")));
        }
    }
    let distinct_mentions = {
        let mut ids: Vec<u64> = message.mentions.iter().map(|u| u.id.get()).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len() as u32 + message.mention_roles.len() as u32
    };
    if config.max_mentions > 0 && distinct_mentions >= config.max_mentions {
        return Some(Flag::Hard(format!(
            "mass-mentioned {distinct_mentions} people/roles"
        )));
    }
    if config.flood_messages > 0 && recent >= config.flood_messages {
        return Some(Flag::Hard(format!(
            "flooded the chat ({recent} messages in 8 seconds)"
        )));
    }
    if config.duplicate_messages > 0 && duplicates >= config.duplicate_messages {
        return Some(Flag::Hard(format!(
            "spammed the same message {duplicates} times"
        )));
    }
    let has_link = text.contains("http://") || text.contains("https://");
    if config.block_invites
        && (text.contains("discord.gg/")
            || text.contains("discord.com/invite/")
            || text.contains("discordapp.com/invite/"))
    {
        return Some(Flag::Hard("posted a server invite link".to_owned()));
    }
    if has_link && SCAM_PHRASES.iter().any(|p| text.contains(p)) {
        return Some(Flag::Hard("posted a likely scam/phishing link".to_owned()));
    }
    if HOSTILE_PHRASES
        .iter()
        .any(|p| super::agent::mentions_word(&text, p))
    {
        return Some(Flag::Soft("possibly hostile language".to_owned()));
    }
    let letters: Vec<char> = message
        .content
        .chars()
        .filter(|c| c.is_alphabetic())
        .collect();
    if letters.len() >= 25 {
        let upper = letters.iter().filter(|c| c.is_uppercase()).count();
        if upper * 100 / letters.len() >= 85 {
            return Some(Flag::Soft("all-caps shouting".to_owned()));
        }
    }
    if text.contains("@everyone") || text.contains("@here") {
        return Some(Flag::Soft("tried to ping everyone".to_owned()));
    }
    None
}

fn exempt(core: &AgentCore, guild_id: u64, message: &Message) -> bool {
    if message.author.bot.unwrap_or(false)
        || core.hive.peer_by_user(message.author.id.get()).is_some()
    {
        return true;
    }
    let principal = core.principal_for(Some(guild_id), &message.author);
    principal.level >= AccessLevel::Moderator
        || core
            .world
            .with_guild(guild_id, |g| {
                principal.channel_permissions(Some(g), message.channel_id.get())
            })
            .is_some_and(|perms| perms.contains(Permissions::MANAGE_MESSAGES))
}

/// Screen a message. Returns `true` when it is being removed (so the
/// agent shouldn't also answer it).
pub fn screen(core: &Arc<AgentCore>, guild_id: u64, message: &Message) -> bool {
    let profile = core.profile();
    if !profile.behavior.moderation.enabled || exempt(core, guild_id, message) {
        return false;
    }
    let Some(flag) = evaluate(core, guild_id, message) else {
        return false;
    };
    if !core
        .hive
        .claim(message.id.get(), ClaimKind::Moderation, &core.id)
    {
        return matches!(flag, Flag::Hard(_));
    }
    let core = Arc::clone(core);
    let message = message.clone();
    let hard = matches!(flag, Flag::Hard(_));
    tokio::spawn(async move {
        match flag {
            Flag::Hard(reason) => act(&core, guild_id, &message, "delete", &reason).await,
            Flag::Soft(reason) => {
                if profile.behavior.moderation.llm_review {
                    review(&core, guild_id, &message, &reason).await;
                }
            }
        }
    });
    hard
}

/// Ask the model whether a borderline message breaks the rules.
async fn review(core: &Arc<AgentCore>, guild_id: u64, message: &Message, flag: &str) {
    let profile = core.profile();
    let (rules, culture) = core
        .hive
        .knowledge
        .guild(guild_id, |k| (k.rules.clone(), k.culture.clone()));
    let context: Vec<String> = core
        .hive
        .knowledge
        .channel(message.channel_id.get(), |archive| {
            archive
                .messages
                .iter()
                .rev()
                .take(10)
                .rev()
                .map(|m| format!("{}: {}", m.author, crate::llm::truncate(&m.content, 200)))
                .collect()
        });
    let system = "You are a fair, level-headed Discord moderator. Judge one message against the server's rules and culture. Most borderline messages are fine; only act on real violations. Reply with JSON only.";
    let user = format!(
        "Server rules:\n{}\n{}\n\nServer culture:\n{}\n\nRecent conversation:\n{}\n\nFlagged message from {} (flag: {flag}):\n\"{}\"\n\nReply as JSON: {{\"violation\": true|false, \"action\": \"none\"|\"warn\"|\"delete\"|\"timeout\", \"reason\": \"short reason shown to the user\"}}",
        if rules.is_empty() {
            "(no rules channel found)"
        } else {
            &rules
        },
        profile.behavior.moderation.extra_rules,
        if culture.is_empty() {
            "(unknown yet)"
        } else {
            &culture
        },
        context.join("\n"),
        super::agent::display_name(message),
        message.content
    );
    let Ok(reply) = brain::quick_completion(core, system, &user, 300, true).await else {
        return;
    };
    let (reply, _) = crate::llm::text::split_think(&reply);
    let Some(verdict) = brain::extract_json(&reply) else {
        return;
    };
    if verdict.get("violation").and_then(|v| v.as_bool()) != Some(true) {
        return;
    }
    let action = verdict
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_owned();
    let reason = verdict
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or(flag)
        .to_owned();
    if action != "none" {
        act(core, guild_id, message, &action, &reason).await;
    }
}

async fn act(core: &Arc<AgentCore>, guild_id: u64, message: &Message, action: &str, reason: &str) {
    let profile = core.profile();
    let channel_label = core.channel_label(Some(guild_id), message.channel_id.get());
    let who = super::agent::display_name(message);
    if profile.behavior.autonomy < Autonomy::Moderator {
        core.feed(
            FeedKind::Moderation,
            format!("flagged {who}: {reason} (not acting — autonomy below Moderator)"),
            Some(channel_label),
        );
        core.post_mod_log(
            guild_id,
            &format!(
                "🚩 Flagged a message from <@{}> in <#{}>: {reason}\n> {}\n-# I'm not allowed to act on my own — a moderator should take a look.",
                message.author.id,
                message.channel_id,
                crate::llm::truncate(&message.content, 300)
            ),
        )
        .await;
        return;
    }
    let ctx = ToolCtx {
        core: Arc::clone(core),
        guild_id: Some(guild_id),
        channel_id: message.channel_id.get(),
        message_id: Some(message.id.get()),
        requester: core.self_principal(Some(guild_id)),
        autonomous: true,
        replies_here: false,
        media: Default::default(),
        wants_file_attached: false,
        inputs: Vec::new(),
    };
    let Ok(target) = tools::discord::resolve_member(&ctx, &message.author.id.to_string()).await
    else {
        return;
    };
    let delete = matches!(action, "delete" | "timeout");
    if delete {
        let _ = core
            .http
            .delete_message(message.channel_id, MessageId::new(message.id.get()))
            .reason(ctx.reason(Some(reason.to_owned())))
            .await;
    }
    let strikes = tools::discord::add_strike(&ctx, guild_id, &target, reason);
    let config = profile.behavior.moderation;
    let recent = core.hive.knowledge.guild(guild_id, |k| {
        k.people
            .get(&target.user_id)
            .map(|p| p.recent_strikes(24 * 3600 * 1000))
            .unwrap_or(0)
    });
    let escalate = action == "timeout"
        || (config.strikes_before_timeout > 0 && recent as u32 >= config.strikes_before_timeout);
    let mut summary = format!(
        "{} {who}: {reason}",
        if delete {
            "removed a message from"
        } else {
            "warned"
        }
    );
    if escalate {
        let minutes = config.timeout_minutes.clamp(1, 60);
        let args =
            json!({ "user": target.user_id.to_string(), "minutes": minutes, "reason": reason });
        if tools::discord::timeout_member(&ctx, &args).await.is_ok() {
            summary.push_str(&format!(
                " — timed out for {minutes}m ({recent} strikes today)"
            ));
        }
    } else {
        core.live().stat(&core.id, |s| s.mod_actions += 1);
    }
    // A short, self-deleting notice so people know why.
    let notice = format!(
        "-# 🛡 <@{}>, {} — {reason}. (strike {strikes})",
        target.user_id,
        if delete {
            "your message was removed"
        } else {
            "heads up"
        }
    );
    if let Ok(sent) = core
        .http
        .create_message(ChannelId::new(message.channel_id.get()))
        .content(notice)
        .allowed_mentions(AllowedMentions::users())
        .await
    {
        let core = Arc::clone(core);
        let channel = message.channel_id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(25)).await;
            let _ = core.http.delete_message(channel, sent.id).await;
        });
    }
    core.feed(FeedKind::Moderation, summary.clone(), Some(channel_label));
    core.post_mod_log(
        guild_id,
        &format!(
            "🛡 {summary}\n> {}\n-# in <#{}> · automatic",
            crate::llm::truncate(&message.content, 300),
            message.channel_id
        ),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_ignore_case_and_whitespace() {
        assert_eq!(fingerprint("Buy NOW "), fingerprint("buy now"));
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }
}
