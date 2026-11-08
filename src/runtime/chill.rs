//! Chill mode: channels where the agent stays extra brief and cleans up
//! after its own chatty stretches.
//!
//! When a conversation burst in a chill channel has gone quiet, the agent
//! looks back at **its own** messages in that burst, deletes the ones that
//! didn't contribute, and replaces the last one with a one-line summary of
//! what mattered. Human messages are never touched — only the agent's own
//! message ids are ever candidates for deletion.
//!
//! Detection is software; judgment is one cheap background-model call:
//! - trigger: >= 4 of the agent's own messages since the last fold, the
//!   newest of them >= 5 minutes old, and nothing newer from anyone;
//! - the call returns `{keep: [ids], summary: "one short line"}`;
//! - the agent deletes its non-kept messages, then edits its last kept
//!   message to the summary (or posts the summary when none were kept).
//!
//! A verdict that fails to parse deletes nothing: "could not decide" is
//! always safer than guessing.

use std::sync::Arc;

use serde_json::json;

use super::agent::AgentCore;
use super::brain;
use super::live::FeedKind;
use crate::memory::{StoredMessage, now_ms};

/// Quiet time after a burst before compaction kicks in.
const QUIET_MS: i64 = 5 * 60_000;
/// The agent's own messages in the stretch required to bother.
const MIN_OWN: usize = 4;
/// How far back a stretch can reach.
const WINDOW: usize = 80;
/// Cap on the summary line.
const SUMMARY_MAX: usize = 200;

/// Check every chill channel for a quiet burst to fold. Called from the
/// scheduler's slow tick; `begin_job` keeps it single-flight.
pub async fn tick(core: &Arc<AgentCore>) {
    for guild_id in core.world.guild_ids() {
        let chill_channels: Vec<u64> = core.hive.knowledge.guild(guild_id, |k| {
            k.chill
                .iter()
                .filter(|r| r.agent_id.as_deref().is_none_or(|a| a == core.id))
                .map(|r| r.channel_id)
                .collect()
        });
        for channel in chill_channels {
            let Some(_guard) = core.hive.begin_job(channel, "chill") else {
                continue;
            };
            if let Err(error) = compact(core, guild_id, channel).await {
                // "skip" is the quiet not-this-time; anything else is real.
                if error != "skip" {
                    core.feed(
                        FeedKind::Error,
                        format!("chill cleanup in channel {channel}: {error}"),
                        None,
                    );
                }
            }
        }
    }
}

/// Fold one channel's quiet burst. `Err("skip")` when there's nothing to
/// do.
async fn compact(core: &Arc<AgentCore>, guild_id: u64, channel_id: u64) -> Result<(), String> {
    let me = core.world.my_id();
    let (stretch, upto, last_ts) = core.hive.knowledge.channel(channel_id, |archive| {
        let recent: Vec<_> = archive.messages.iter().rev().take(WINDOW).collect();
        let last_ts = recent.first().map(|m| m.ts).unwrap_or(0);
        let own: Vec<StoredMessage> = recent
            .iter()
            .filter(|m| m.author_id == me && m.id > archive.chill_upto)
            .map(|m| (*m).clone())
            .collect();
        let upto = own.iter().map(|m| m.id).max().unwrap_or(archive.chill_upto);
        (own, upto, last_ts)
    });
    if stretch.len() < MIN_OWN {
        return Err("skip".to_owned());
    }
    if now_ms() - last_ts < QUIET_MS {
        return Err("skip".to_owned()); // the conversation is still live
    }

    // The conversation around the stretch, for judgment. Lines carry
    // message ids so the verdict can name what to keep.
    let context: Vec<String> = core.hive.knowledge.channel(channel_id, |archive| {
        archive
            .messages
            .iter()
            .rev()
            .take(WINDOW)
            .rev()
            .map(|m| {
                format!(
                    "{} [id {}]: {}",
                    if m.author_id == me { "YOU" } else { &m.author },
                    m.id,
                    crate::llm::truncate(&m.content, 200)
                )
            })
            .collect()
    });

    let answer = brain::quick_completion(
        core,
        "You decide which of an assistant's own Discord messages earned their place in a channel where it was asked to stay CHILL (minimal, brief). Reply with JSON only.",
        &format!(
            "Conversation below. Lines marked YOU are the assistant's own messages with their ids. Keep only its messages that genuinely contributed (answers, results, links people needed); drop chatter, filler, restatements. Also write one summary line (max {SUMMARY_MAX} chars) of only what mattered in the exchange.\nReply with ONLY: {{\"keep\": [your ids worth keeping, may be empty], \"summary\": \"...\"}}\n\n{}",
            context.join("\n")
        ),
        400,
        true,
    )
    .await?;

    // Unparseable verdict = keep everything, change nothing.
    let Some(verdict) = brain::extract_json(&answer) else {
        return Ok(());
    };
    let keep: Vec<u64> = verdict
        .get("keep")
        .and_then(|ids| ids.as_array())
        .map(|ids| {
            ids.iter()
                .filter_map(|id| {
                    id.as_u64()
                        .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut summary = verdict
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_owned();
    // Never trust the length; clip in code.
    summary = crate::llm::truncate(&summary, SUMMARY_MAX);

    // Delete our own messages that didn't earn their place. The id list
    // comes from `stretch`, which only ever contains the agent's own
    // messages — human messages are structurally not deletable here.
    let mut deleted = 0u32;
    for message in &stretch {
        if keep.contains(&message.id) {
            continue;
        }
        if core
            .http
            .delete_message(
                axis::prelude::ChannelId::new(channel_id),
                axis::prelude::MessageId::new(message.id),
            )
            .await
            .is_ok()
        {
            deleted += 1;
        }
    }
    // The summary lands on the last kept message when there is one,
    // otherwise as one fresh line.
    if let Some(last) = stretch.iter().filter(|m| keep.contains(&m.id)).next_back() {
        let _ = core
            .http
            .raw(
                "PATCH",
                &format!("/channels/{channel_id}/messages/{}", last.id),
                Some(json!({ "content": summary, "allowed_mentions": { "parse": [] } })),
                None,
            )
            .await;
    } else if !summary.trim().is_empty() {
        let _ = core
            .http
            .raw(
                "POST",
                &format!("/channels/{channel_id}/messages"),
                Some(json!({ "content": summary, "allowed_mentions": { "parse": [] } })),
                None,
            )
            .await;
    }
    // This burst is folded either way: advance the watermark.
    core.hive.knowledge.channel_mut(channel_id, |archive| {
        archive.chill_upto = archive.chill_upto.max(upto)
    });
    if deleted > 0 || !summary.is_empty() {
        core.feed(
            FeedKind::Moderation,
            format!(
                "chill cleanup in channel {channel_id}: folded {deleted} of its own {} messages",
                stretch.len()
            ),
            None,
        );
        let _ = guild_id;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(id: u64, author: u64, content: &str) -> StoredMessage {
        StoredMessage {
            id,
            author_id: author,
            author: format!("u{author}"),
            author_bot: author == 7,
            content: content.to_owned(),
            ts: now_ms(),
            reply_to: None,
            reply_author: None,
            reply_snippet: None,
            attachments: Vec::new(),
            deleted: false,
            edited: false,
            reactions: Vec::new(),
        }
    }

    #[test]
    fn stretch_selection_takes_only_own_new_messages() {
        // Mirrors the selection closure: own messages above the watermark.
        let me = 7u64;
        let archive_chill_upto = 100u64;
        let recent = vec![
            stored(101, me, "a"),
            stored(102, 3, "human"),
            stored(103, me, "b"),
            stored(99, me, "already folded"),
        ];
        let own: Vec<&StoredMessage> = recent
            .iter()
            .filter(|m| m.author_id == me && m.id > archive_chill_upto)
            .collect();
        assert_eq!(own.len(), 2);
        assert!(own.iter().all(|m| m.id != 99));
    }

    #[test]
    fn verdict_ids_accept_numbers_and_strings() {
        let value = brain::extract_json(r#"{"keep": [3, "abc", "12", null], "summary": "x"}"#)
            .expect("parses");
        let keep: Vec<u64> = value["keep"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|id| {
                id.as_u64()
                    .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
            })
            .collect();
        assert_eq!(keep, [3, 12]);
    }

    #[test]
    fn unparseable_verdict_is_none_so_nothing_deletes() {
        assert!(brain::extract_json("I could not decide").is_none());
    }
}
