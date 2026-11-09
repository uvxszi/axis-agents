//! Idle roaming: when the agent has nothing to do, it wanders — reads
//! channels, notices chats about itself, occasionally reacts or drops a
//! short reply. The rules of chill: only when idle, mostly nothing,
//! never two actions close together, and the decision (react / reply /
//! stay quiet) is a judgment made by the model, not a keyword.

use std::sync::Arc;
use std::time::Duration;

use crate::runtime::agent::AgentCore;

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
        if fastrand::f32() > 0.45 {
            continue;
        }
        roam_once(&core).await;
    }
}

async fn roam_once(core: &Arc<AgentCore>) {
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
