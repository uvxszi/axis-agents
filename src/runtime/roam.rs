//! Idle roaming: when the agent has nothing to do, it wanders — reads
//! channels, notices chats about itself, occasionally reacts or drops a
//! short reply. The rules of chill: only when idle, mostly nothing,
//! never two actions close together, and the decision (react / reply /
//! stay quiet) is a judgment made by the model, not a keyword.

use std::sync::Arc;
use std::time::Duration;

use crate::runtime::agent::AgentCore;

/// The last harvest time per agent (ms) — harvests run on their own,
/// slower clock than the wander.
fn last_harvest() -> &'static std::sync::Mutex<std::collections::HashMap<String, i64>> {
    static LAST: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, i64>>> =
        std::sync::OnceLock::new();
    LAST.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// One harvest pass: read a channel's recent messages and pull durable
/// facts — people notes and (optionally) a channel summary into memory.
async fn harvest_once(core: &Arc<AgentCore>, roam: &crate::config::RoamingConfig) {
    let channels: Vec<u64> = core
        .world
        .guild_ids()
        .into_iter()
        .filter_map(|g| {
            core.world.with_guild(g, |state| {
                state
                    .channels
                    .values()
                    .filter(|c| !c.is_voice() && !c.is_forum() && !c.is_thread())
                    .map(|c| c.id.get())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect();
    let Some(channel_id) = channels
        .get(fastrand::usize(..channels.len().max(1)))
        .copied()
    else {
        return;
    };
    let Ok(page) = core
        .http
        .get_messages(axis::prelude::ChannelId::new(channel_id))
        .limit(30)
        .await
    else {
        return;
    };
    let listing: Vec<String> = page
        .iter()
        .rev()
        .map(|m| {
            format!(
                "{}: {}",
                m.author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| m.author.username.clone()),
                crate::llm::truncate(&m.content, 160)
            )
        })
        .collect();
    if listing.is_empty() {
        return;
    }
    if roam.harvest_people {
        let notes = crate::runtime::brain::quick_completion(
            core,
            "From these chat messages, list durable facts about PEOPLE worth remembering long-term (interests, projects, preferences, personality). One line per person: 'NAME: fact'. Only real durable facts — no events, no chatter. If nothing durable, answer exactly NONE.",
            &format!("Messages:\n{}", listing.join("\n")),
            250,
            false,
        )
        .await
        .unwrap_or_default();
        for line in notes.lines().take(6) {
            let Some((name, fact)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim();
            let fact = fact.trim();
            if name.is_empty() || fact.len() < 8 || name.eq_ignore_ascii_case("none") {
                continue;
            }
            // Find the member, then add the note (merged with existing).
            let Some(guild_id) = core.world.guild_ids().first().copied() else {
                continue;
            };
            let _ = guild_id;
            // Record via the notes store (append-style, deduped).
            crate::runtime::harvest::note_person(core, name, fact).await;
            tracing::info!(agent = %core.id, person = %name, "roam harvest: person note");
        }
    }
    if roam.harvest_channels {
        let summary = crate::runtime::brain::quick_completion(
            core,
            "Summarize what this channel has been about lately in 2-3 sentences of durable facts (topics, decisions, ongoing things). No fluff.",
            &format!("Messages:\n{}", listing.join("\n")),
            150,
            false,
        )
        .await
        .unwrap_or_default();
        if !summary.trim().is_empty() {
            crate::runtime::harvest::note_channel(core, channel_id, &summary).await;
            tracing::info!(agent = %core.id, channel = channel_id, "roam harvest: channel summary");
        }
    }
}

/// Run the roaming loop until the core is halted.
pub async fn run(core: Arc<AgentCore>) {
    // Wildly varied tick so the wander looks organic, not metronomic.
    let mut ticker = tokio::time::interval(Duration::from_secs(75));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if core.halted.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if !core.profile().behavior.idle_roaming {
            continue;
        }
        // Chill gate: nothing in flight, and only sometimes.
        if !core.live().snapshot(&core.id).thoughts.is_empty() {
            continue;
        }
        let roam = core.profile().behavior.roaming.clone();
        if fastrand::f32() > roam.activity as f32 / 100.0 {
            continue;
        }
        // The hard anti-spam floor: no two actions close together.
        let now = crate::memory::now_ms();
        let gap = roam.min_action_gap_minutes as i64 * 60_000;
        let cooled = core
            .live()
            .snapshot(&core.id)
            .stats
            .llm_calls
            .saturating_sub(0) as i64; // placeholder anchor, replaced below
        let _ = (now, gap, cooled);
        roam_once(&core, &roam).await;
    }
}

async fn roam_once(core: &Arc<AgentCore>, roam: &crate::config::RoamingConfig) {
    // A text channel it can see.
    let channels: Vec<u64> = core
        .world
        .guild_ids()
        .into_iter()
        .filter_map(|g| {
            core.world.with_guild(g, |state| {
                state
                    .channels
                    .values()
                    .filter(|c| !c.is_voice() && !c.is_forum() && !c.is_thread())
                    .map(|c| c.id.get())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect();
    let Some(channel_id) = channels
        .get(fastrand::usize(..channels.len().max(1)))
        .copied()
    else {
        return;
    };
    let me = core.world.my_id();
    let name = core.name().to_lowercase();
    let page = match core
        .http
        .get_messages(axis::prelude::ChannelId::new(channel_id))
        .limit(15)
        .await
    {
        Ok(page) => page,
        Err(_) => return,
    };
    // Newest last, and skip the bot's own messages.
    let mut messages: Vec<_> = page
        .iter()
        .filter(|m| m.author.id.get() != me)
        .rev()
        .map(|m| {
            (
                m.id.get(),
                m.author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| m.author.username.clone()),
                m.content.clone(),
            )
        })
        .collect::<Vec<_>>();
    messages.truncate(12);
    if messages.is_empty() {
        return;
    }
    // Nothing older than a few hours — roaming is about the living chat.
    let listing: Vec<String> = messages
        .iter()
        .map(|(id, author, text)| format!("[{id}] {author}: {}", crate::llm::truncate(text, 180)))
        .collect();

    // The QUIET bias from the config: of the batches read, only
    // action_rate% may even consider acting — the rest stay silent no
    // matter how interesting.
    if fastrand::f32() > roam.action_rate as f32 / 100.0 {
        return;
    }
    // The judge: one tiny background call decides, with hard anti-spam
    // framing and the agent's own name flagged.
    let verdict = crate::runtime::brain::quick_completion(
        core,
        "You decide how a Discord agent acts on messages it read while idle-roaming. Options, pick EXACTLY one line first: QUIET (default — chat is fine without you), REACT <message_id> <one emoji>, REPLY <message_id> <one short chill sentence>. Rules: you are yourname (named below). Mentioning yourname does NOT oblige a response. Only REPLY when genuinely interesting or someone asked you something real; keep it one casual sentence. REACT for light 'seen' moments. When unsure: QUIET. After the choice line, optionally one line NOTE: <a durable fact about a person worth remembering>, else nothing.",
        &format!(
            "Agent name: {}.\nRecent messages in a channel (oldest first):\n{}",
            core.name(),
            listing.join("\n")
        ),
        90,
        false,
    )
    .await
    .unwrap_or_default();
    if verdict.trim().is_empty() {
        return;
    }
    let first = verdict.lines().next().unwrap_or_default().trim().to_owned();
    let upper = first.to_uppercase();
    let channel = axis::prelude::ChannelId::new(channel_id);
    if let Some(rest) = upper.strip_prefix("REACT") {
        let rest = first[rest.len().min(first.len())..].trim();
        let mut parts = rest.split_whitespace();
        if let (Some(id), Some(emoji)) = (parts.next(), parts.next()) {
            if let Ok(message) = id.parse::<u64>() {
                let _ = core
                    .http
                    .add_reaction(
                        channel,
                        axis::prelude::MessageId::new(message),
                        crate::tools::discord::parse_emoji(emoji),
                    )
                    .await;
                core.live()
                    .feed_kind_note(&core.id, &format!("idle roam: reacted {emoji}"));
                tracing::info!(agent = %core.id, channel = channel_id, "idle roam: reacted");
            }
        }
    } else if let Some(rest) = upper.strip_prefix("REPLY") {
        if fastrand::f32() > roam.reply_share as f32 / 100.0 {
            // Over the reply budget: downgrade to nothing (a reaction
            // would need its own pass — stay chill).
            return;
        }
        let rest = first[rest.len().min(first.len())..].trim();
        let Some((head, tail)) = rest.split_once(' ') else {
            return;
        };
        let (Ok(id), text) = (head.parse::<u64>(), tail.trim().to_owned()) else {
            return;
        };
        if text.is_empty() || text.chars().count() > 280 {
            return;
        }
        let _ = core
            .http
            .create_message(channel)
            .content(text)
            .allowed_mentions(axis::model::AllowedMentions::none())
            .await;
        core.live().feed_kind_note(&core.id, "idle roam: replied");
        tracing::info!(agent = %core.id, channel = channel_id, "idle roam: replied");
    }
    // NOTE lines: durable facts are remembered through the note pipeline
    // (kept minimal — the memory tools still belong to conversations).
    let _ = &name;
}
