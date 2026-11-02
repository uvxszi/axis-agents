//! Learning the server by being in it.
//!
//! Every message is archived. Every N messages, one agent (elected per
//! server) reads a sample of recent conversation across channels and
//! updates the shared notes: the server's culture (tone, humor, slang,
//! norms, recurring topics) and short notes on the people it sees most.
//! Agents read these notes in every prompt, so they fit in.

use std::sync::Arc;

use axis::prelude::ChannelId;

use super::agent::AgentCore;
use super::brain::{extract_json, quick_completion};
use super::live::FeedKind;
use crate::memory::now_ms;

fn elected(core: &AgentCore, guild_id: u64) -> bool {
    core.hive
        .peers_in_guild(guild_id)
        .into_iter()
        .find(|p| p.learns)
        .is_some_and(|p| p.id == core.id)
}

pub fn maybe_reflect(core: &Arc<AgentCore>, guild_id: u64) {
    let profile = core.profile();
    if !profile.behavior.learn_culture {
        return;
    }
    let threshold = profile.behavior.reflect_every_messages.max(30);
    let due = core
        .hive
        .knowledge
        .guild(guild_id, |k| k.messages_since_reflect >= threshold);
    if !due || !elected(core, guild_id) {
        return;
    }
    let Some(guard) = core.hive.begin_job(guild_id, "reflect") else {
        return;
    };
    let core = Arc::clone(core);
    tokio::spawn(async move {
        let _guard = guard;
        reflect(&core, guild_id).await;
    });
}

async fn reflect(core: &Arc<AgentCore>, guild_id: u64) {
    let channels: Vec<u64> = core
        .world
        .with_guild(guild_id, |g| g.channels.keys().copied().collect())
        .unwrap_or_default();
    let hq = core.hive.knowledge.guild(guild_id, |k| k.hq.channel_id);
    let mut sample: Vec<(i64, String)> = Vec::new();
    let mut counts: std::collections::HashMap<u64, (String, usize)> = Default::default();
    for channel_id in channels {
        if Some(channel_id) == hq {
            continue;
        }
        let name = core.channel_label(Some(guild_id), channel_id);
        core.hive.knowledge.channel(channel_id, |archive| {
            for message in archive.messages.iter().rev().take(60) {
                if message.deleted || message.author_bot {
                    continue;
                }
                let entry = counts
                    .entry(message.author_id)
                    .or_insert((message.author.clone(), 0));
                entry.1 += 1;
                sample.push((
                    message.ts,
                    format!(
                        "{name} {} (<@{}>): {}",
                        message.author,
                        message.author_id,
                        crate::llm::truncate(&message.content, 240)
                    ),
                ));
            }
        });
    }
    if sample.len() < 20 {
        return;
    }
    sample.sort_by_key(|(ts, _)| *ts);
    let lines: Vec<String> = sample
        .into_iter()
        .rev()
        .take(220)
        .map(|(_, l)| l)
        .rev()
        .collect();
    let mut regulars: Vec<(u64, (String, usize))> = counts.into_iter().collect();
    regulars.sort_by(|a, b| b.1.1.cmp(&a.1.1));
    let (culture, existing_people) = core.hive.knowledge.guild(guild_id, |k| {
        let people: Vec<String> = regulars
            .iter()
            .take(12)
            .filter_map(|(id, _)| {
                k.people
                    .get(id)
                    .filter(|p| !p.notes.is_empty() && !p.opted_out)
                    .map(|p| format!("<@{id}> {}: {}", p.name, p.notes))
            })
            .collect();
        (k.culture.clone(), people)
    });
    let system = "You are an anthropologist embedded in a Discord community, writing private field notes for a team of AI agents so they can fit in naturally. Be specific and concrete; never invent things that aren't in the messages.";
    let user = format!(
        "Current culture notes:\n{}\n\nCurrent notes on people:\n{}\n\nRecent messages across channels:\n{}\n\nUpdate the notes. Reply with JSON only:\n{{\"culture\": \"<=180 words: overall vibe and tone, humor style, slang/in-jokes, what's normal vs frowned upon, recurring topics, what each main channel is used for\", \"people\": [{{\"user_id\": \"<id>\", \"notes\": \"<=35 words: personality, interests, how they like to be talked to\"}}]}}\nInclude up to 8 of the most active people.",
        if culture.is_empty() {
            "(none yet)"
        } else {
            &culture
        },
        if existing_people.is_empty() {
            "(none yet)".to_owned()
        } else {
            existing_people.join("\n")
        },
        crate::llm::truncate(&lines.join("\n"), 28_000)
    );
    let reply = match quick_completion(core, system, &user, 1200, true).await {
        Ok(reply) => reply,
        Err(error) => {
            tracing::debug!(%error, "reflection failed");
            return;
        }
    };
    let (reply, _) = crate::llm::text::split_think(&reply);
    let Some(notes) = extract_json(&reply) else {
        return;
    };
    let new_culture = notes
        .get("culture")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    let mut updated_people = 0;
    core.hive.knowledge.guild_mut(guild_id, |k| {
        if !new_culture.is_empty() {
            k.culture = crate::llm::truncate(&new_culture, 1600);
            k.culture_updated = now_ms();
        }
        if let Some(people) = notes.get("people").and_then(|v| v.as_array()) {
            for person in people {
                let id = person
                    .get("user_id")
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    })
                    .and_then(|s| super::world::parse_snowflake(&s).or_else(|| s.parse().ok()));
                let note = person
                    .get("notes")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let (Some(id), false) = (id, note.is_empty()) else {
                    continue;
                };
                let name = regulars
                    .iter()
                    .find(|(u, _)| *u == id)
                    .map(|(_, (n, _))| n.clone());
                let entry = k.people.entry(id).or_default();
                if entry.opted_out {
                    continue;
                }
                if let Some(name) = name {
                    entry.name = name;
                }
                entry.notes = crate::llm::truncate(note, 400);
                entry.updated = now_ms();
                updated_people += 1;
            }
        }
        k.messages_since_reflect = 0;
    });
    core.feed(
        FeedKind::Learning,
        format!("updated the server's culture notes and {updated_people} people notes"),
        None,
    );
}

/// Read the rules channel into shared knowledge (refreshed twice a day).
pub async fn load_rules(core: &Arc<AgentCore>, guild_id: u64) {
    let stale = core
        .hive
        .knowledge
        .guild(guild_id, |k| now_ms() - k.rules_updated > 12 * 3600 * 1000);
    if !stale {
        return;
    }
    let channel = core
        .world
        .with_guild(guild_id, |g| {
            g.rules_channel.or_else(|| {
                g.channels
                    .values()
                    .find(|c| c.name.as_deref().is_some_and(|n| n.contains("rules")))
                    .map(|c| c.id.get())
            })
        })
        .flatten();
    let Some(channel) = channel else {
        return;
    };
    let Ok(mut messages) = core
        .http
        .get_messages(ChannelId::new(channel))
        .limit(20)
        .await
    else {
        return;
    };
    messages.reverse();
    let mut text = String::new();
    for message in &messages {
        text.push_str(&message.content);
        for embed in &message.embeds {
            if let Some(title) = &embed.title {
                text.push_str(&format!("\n{title}"));
            }
            if let Some(description) = &embed.description {
                text.push_str(&format!("\n{description}"));
            }
            for field in embed.fields.iter().flatten() {
                text.push_str(&format!("\n{}: {}", field.name, field.value));
            }
        }
        text.push('\n');
    }
    if text.trim().is_empty() {
        return;
    }
    core.hive.knowledge.guild_mut(guild_id, |k| {
        k.rules = crate::llm::truncate(text.trim(), 3000);
        k.rules_updated = now_ms();
    });
    core.feed(FeedKind::Learning, "read the server rules", None);
}
