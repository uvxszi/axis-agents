//! The activity log: every action an agent takes in a server, one small
//! line each, in an "agent-activity" channel inside Agent HQ (made the
//! first time it's needed, private like HQ).

use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};

use super::agent::AgentCore;

/// Tools that only look things up; they aren't logged.
pub fn is_read_only(tool: &str) -> bool {
    tool.starts_with("get_")
        || tool.starts_with("list_")
        || tool.starts_with("read_")
        || tool.starts_with("search_")
        || tool.starts_with("find_")
        || tool.starts_with("draft_")
        || matches!(
            tool,
            "web_search" | "fetch_url" | "recall" | "calculate" | "get_time" | "roll_dice" | "random_pick" | "look_at_image"
                | "image_search" | "voice_overview" | "server_stats" | "leaderboard" | "ticket_transcript" | "make_plan"
                | "step_done" | "draw_image" | "make_chart" | "edit_image" | "attach_image" | "get_avatar" | "remember"
                | "forget_memory" | "list_voices" | "show_settings" | "server_activity" | "ask_agent"
        )
}

fn creating() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(Default::default)
}

/// The activity channel, made under the HQ category when there is one.
async fn channel(core: &AgentCore, guild_id: u64) -> Option<u64> {
    let _guard = creating().lock().await;
    let (existing, category) = core.hive.knowledge.guild(guild_id, |k| (k.hq.activity_channel_id, k.hq.category_id));
    if let Some(id) = existing.filter(|id| core.world.channel(guild_id, *id).is_some()) {
        return Some(id);
    }
    let category = category.filter(|c| core.world.channel(guild_id, *c).is_some())?;
    // Created inside the category, it takes the category's (private)
    // permissions.
    let created: Value = core
        .http
        .raw(
            "POST",
            &format!("/guilds/{guild_id}/channels"),
            Some(json!({
                "name": "agent-activity",
                "type": 0,
                "parent_id": category.to_string(),
                "topic": "Everything the AI agents do, as it happens.",
            })),
            Some("activity log for the AI agents".to_owned()),
        )
        .await
        .ok()?;
    let id: u64 = created.get("id")?.as_str()?.parse().ok()?;
    if let Ok(channel) = serde_json::from_value::<axis::model::Channel>(created) {
        core.world.upsert_channel(&channel);
    }
    core.hive.knowledge.guild_mut(guild_id, |k| k.hq.activity_channel_id = Some(id));
    Some(id)
}

/// Something reached into this server from outside (a DM or another
/// server): log it in HQ — or, without an HQ, tell the server's leaders
/// at most once a day (and offer to set HQ up).
pub fn log_outside(core: &Arc<AgentCore>, guild_id: u64, text: &str) {
    let has_hq = core.hive.knowledge.guild(guild_id, |k| k.hq.category_id.is_some());
    if has_hq {
        log(core, guild_id, &format!("{text} — from outside this server"));
        return;
    }
    let now = crate::memory::now_ms();
    let due = core.hive.knowledge.guild_mut(guild_id, |k| {
        if now - k.hq.last_notice < 24 * 3_600_000 {
            return false;
        }
        k.hq.last_notice = now;
        true
    });
    if !due {
        return;
    }
    let core = Arc::clone(core);
    let text = text.to_owned();
    tokio::spawn(async move {
        let posted = super::hq::notify_leaders(
            &core,
            guild_id,
            &format!(
                "Heads-up: I was used on this server from outside it — {text}. I log things like this in a private **Agent HQ** channel, but this server doesn't have one. An admin can reply **yes** to this message and I'll set one up (or turn off outside access with /setup → Privacy)."
            ),
        )
        .await;
        // A "yes" to this notice works like a yes to the HQ request.
        if let Some((channel_id, message_id)) = posted {
            core.hive.knowledge.guild_mut(guild_id, |k| {
                if k.hq.request.is_none() {
                    k.hq.request = Some(crate::memory::HqRequest {
                        channel_id,
                        message_id,
                        agent_id: core.id.clone(),
                        asked_at: crate::memory::now_ms(),
                    });
                }
            });
        }
    });
}

/// Log one action in a server's activity channel (in the background).
pub fn log(core: &Arc<AgentCore>, guild_id: u64, text: &str) {
    let core = Arc::clone(core);
    let line = format!("-# 🤖 **{}** · {}", core.name(), crate::llm::truncate(&text.replace('\n', " "), 400));
    tokio::spawn(async move {
        if let Some(channel_id) = channel(&core, guild_id).await {
            let _ = core
                .http
                .raw(
                    "POST",
                    &format!("/channels/{channel_id}/messages"),
                    Some(json!({ "content": line, "allowed_mentions": { "parse": [] }, "flags": 4096 })),
                    None,
                )
                .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_are_not_logged() {
        assert!(is_read_only("get_member"));
        assert!(is_read_only("search_audit_logs"));
        assert!(is_read_only("web_search"));
        assert!(!is_read_only("ban_member"));
        assert!(!is_read_only("send_message"));
        assert!(!is_read_only("design_rank_card"));
    }
}
