//! Confirmation buttons for big and irreversible actions.
//!
//! In standard mode an action the model asked for that is big or
//! irreversible (deleting a channel, purging over a hundred messages,
//! banning someone, …) does not run right away: the agent posts a message
//! with Approve / Cancel buttons, stores the pending action, and only the
//! person who asked — or a moderator above them — can make it run by
//! clicking. In autonomous mode only mass operations still confirm.
//!
//! The model is told the action is queued; it never learns how to skip
//! the gate because there is nothing to skip: the gate is in this code.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::config::AccessLevel;
use crate::runtime::agent::AgentCore;
use crate::runtime::authority::Principal;

use axis::model::{Interaction, InteractionData, User};

/// How long a confirmation stays clickable.
const EXPIRY: Duration = Duration::from_secs(5 * 60);

/// The pieces of a confirmation request, before it gets an id.
pub struct Pending<'a> {
    pub tool: &'a str,
    pub args: Value,
    pub requester: Principal,
    pub guild_id: Option<u64>,
    pub channel_id: u64,
}

pub struct PendingAction {
    pub tool: String,
    pub args: Value,
    /// Who asked for the action (whose permissions it was checked against).
    pub requester: Principal,
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub confirm_message_id: u64,
    pub label: String,
    pub created: Instant,
}

#[derive(Default)]
pub struct Confirmations {
    pending: Mutex<HashMap<u64, PendingAction>>,
    next_id: AtomicU64,
}

impl Confirmations {
    /// Queue an action and post its Approve/Cancel message. Returns the
    /// text the model gets as the tool result.
    pub async fn request(&self, core: &Arc<AgentCore>, req: Pending<'_>, label: String) -> String {
        let (tool, args, requester, guild_id, channel_id) = (
            req.tool,
            req.args,
            req.requester,
            req.guild_id,
            req.channel_id,
        );
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let body = json!({
            "content": format!(
                "**Confirm?** {label} — <@{}>, click within 5 minutes.",
                requester.user_id,
            ),
            "allowed_mentions": { "parse": [] },
            "components": [{
                "type": 1,
                "components": [
                    { "type": 2, "style": 3, "label": "Approve", "custom_id": format!("axis_confirm:yes:{id}") },
                    { "type": 2, "style": 4, "label": "Cancel",  "custom_id": format!("axis_confirm:no:{id}") },
                ],
            }],
        });
        let message = core
            .http
            .raw(
                "POST",
                &format!("/channels/{channel_id}/messages"),
                Some(body),
                None,
            )
            .await;
        let confirm_message_id = message
            .ok()
            .and_then(|value| value.get("id").cloned())
            .and_then(|id| id.as_str().map(str::to_owned))
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or_default();
        if confirm_message_id == 0 {
            return format!(
                "error: could not post the confirmation buttons for {label}; the action was NOT run. Tell them to try again or use the command directly."
            );
        }
        self.pending.lock().expect("confirmations poisoned").insert(
            id,
            PendingAction {
                tool: tool.to_owned(),
                args,
                requester,
                guild_id,
                channel_id,
                confirm_message_id,
                label: label.clone(),
                created: Instant::now(),
            },
        );
        // Drop expired entries while we're here.
        self.pending
            .lock()
            .expect("confirmations poisoned")
            .retain(|_, action| action.created.elapsed() < EXPIRY);
        format!(
            "confirmation pending: {label}. Approve/Cancel buttons were posted for them (or a moderator) to click; the action runs only then. Do NOT claim it happened. Continue with anything else that does not depend on it."
        )
    }

    /// Handle an `axis_confirm:` button. Returns true when consumed.
    pub async fn on_component(core: &Arc<AgentCore>, interaction: &Interaction) -> bool {
        let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
            return false;
        };
        let Some(rest) = data.custom_id.strip_prefix("axis_confirm:") else {
            return false;
        };
        let Some((verdict, id_text)) = rest.split_once(':') else {
            return false;
        };
        let Ok(id) = id_text.parse::<u64>() else {
            return false;
        };

        let (action, expired) = {
            let hub = core
                .hive
                .confirmations
                .pending
                .lock()
                .expect("confirmations poisoned");
            match hub.get(&id) {
                Some(action) => (
                    Some((
                        action.tool.clone(),
                        action.args.clone(),
                        action.requester.clone(),
                        action.label.clone(),
                        action.channel_id,
                        action.confirm_message_id,
                    )),
                    action.created.elapsed() >= EXPIRY,
                ),
                None => (None, true),
            }
        };
        let respond = |kind: u8, content: String| {
            let core = Arc::clone(core);
            let interaction = interaction.clone();
            async move {
                let _ = core
                    .http
                    .raw(
                        "POST",
                        &format!("/interactions/{}/{}/callback", interaction.id, interaction.token),
                        Some(json!({ "type": kind, "data": { "content": content, "flags": 64, "allowed_mentions": { "parse": [] } } })),
                        None,
                    )
                    .await;
            }
        };

        let action_guild = {
            let hub = core
                .hive
                .confirmations
                .pending
                .lock()
                .expect("confirmations poisoned");
            hub.get(&id).and_then(|a| a.guild_id)
        };
        let Some((tool, args, requester, label, channel_id, confirm_message_id)) = action else {
            respond(4, "That confirmation already ran or expired.".to_owned()).await;
            return true;
        };
        if expired {
            core.hive
                .confirmations
                .pending
                .lock()
                .expect("confirmations poisoned")
                .remove(&id);
            respond(
                4,
                "That confirmation expired (5 minutes). Ask again.".to_owned(),
            )
            .await;
            return true;
        }

        let Some(user) = interaction
            .member
            .as_ref()
            .and_then(|m| m.user.as_ref())
            .or(interaction.user.as_ref())
        else {
            respond(4, "Could not tell who clicked.".to_owned()).await;
            return true;
        };
        let clicker = core.principal_for(interaction.guild_id.map(|g| g.get()), user);

        if verdict == "no" {
            // Anyone involved may cancel; moderators too.
            let may =
                clicker.user_id == requester.user_id || clicker.level >= AccessLevel::Moderator;
            if !may {
                respond(
                    4,
                    "Only the person who asked (or a moderator) can cancel.".to_owned(),
                )
                .await;
                return true;
            }
            core.hive
                .confirmations
                .pending
                .lock()
                .expect("confirmations poisoned")
                .remove(&id);
            respond(4, format!("Cancelled: {label}.")).await;
            edit_message(
                core,
                channel_id,
                confirm_message_id,
                &format!("~~{label}~~ — cancelled."),
            )
            .await;
            return true;
        }
        if verdict != "yes" {
            return false;
        }

        // Approve: the clicker must be the requester, or a moderator.
        let may = clicker.user_id == requester.user_id || clicker.level >= AccessLevel::Moderator;
        if !may {
            respond(
                4,
                "Only the person who asked (or a moderator) can approve this.".to_owned(),
            )
            .await;
            return true;
        }

        // Take it out of the map so double-clicks can't run it twice.
        core.hive
            .confirmations
            .pending
            .lock()
            .expect("confirmations poisoned")
            .remove(&id);

        respond(5, json_decode_placeholder()).await; // acknowledge, then edit below.
        let by = if clicker.user_id == requester.user_id {
            requester.name.clone()
        } else {
            format!("{} (moderator)", clicker.name)
        };
        let result = crate::tools::run_confirmed(core, &tool, args, requester, channel_id).await;
        let outcome_text = match result {
            Ok(text) => format!("✅ {label} — done (approved by {by}).\n-# {text}"),
            Err(error) => format!("⚠️ {label} — failed (approved by {by}).\n-# {error}"),
        };
        edit_message(core, channel_id, confirm_message_id, &outcome_text).await;
        let logged_guild = interaction
            .guild_id
            .map(|g| g.get())
            .or(action_guild)
            .unwrap_or_default();
        crate::runtime::activity::log(
            core,
            logged_guild,
            &format!("{label} (guild {logged_guild}) — approved by {by}"),
        );
        true
    }
}

fn json_decode_placeholder() -> String {
    "…".to_owned()
}

async fn edit_message(core: &Arc<AgentCore>, channel_id: u64, message_id: u64, content: &str) {
    let _ = core
        .http
        .raw(
            "PATCH",
            &format!("/channels/{channel_id}/messages/{message_id}"),
            Some(json!({ "content": content, "components": [], "allowed_mentions": { "parse": [] } })),
            None,
        )
        .await;
}

/// Whether a tool call needs a confirmation button first, and the short
/// label to show. `None` means: run it now.
pub fn gate(
    _core: &Arc<AgentCore>,
    _guild_id: Option<u64>,
    tool: &str,
    args: &Value,
    autonomous_mode: bool,
) -> Option<String> {
    let count = args
        .get("count")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            args.get("count")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
        });
    let purge_all = count
        .as_deref()
        .map(|c| c.trim().eq_ignore_ascii_case("all"))
        .unwrap_or(false);
    let purge_count = count
        .as_deref()
        .and_then(|c| c.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let is_mass = purge_all || purge_count > 100;
    let delete = tool.starts_with("delete_")
        || tool == "close_ticket" && args.get("delete").and_then(Value::as_bool).unwrap_or(false);

    // Mass operations confirm in every mode.
    if is_mass && tool == "purge_messages" {
        return Some(format!(
            "purge {} in this server",
            if purge_all {
                "the whole channel (up to 10 000 messages)".to_owned()
            } else {
                format!("{purge_count} messages")
            }
        ));
    }
    if autonomous_mode {
        return None;
    }

    // Standard mode: irreversible and big actions confirm.
    let label = match tool {
        "delete_channel" => Some("delete this channel".to_owned()),
        "delete_role" => Some("delete a role".to_owned()),
        "delete_emoji" => Some("delete an emoji".to_owned()),
        "delete_event" => Some("delete a server event".to_owned()),
        "ban_member" => Some("ban a member".to_owned()),
        "edit_server" => Some("change server settings".to_owned()),
        "purge_messages" => Some(format!("purge {purge_count} messages")),
        "discord_api" => args
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_uppercase)
            .filter(|m| m != "GET")
            .map(|m| format!("raw Discord API call ({m})")),
        "close_ticket" if args.get("delete").and_then(Value::as_bool).unwrap_or(false) => {
            Some("close and delete a ticket".to_owned())
        }
        _ if delete => Some(format!("run {tool}")),
        _ => None,
    };
    label.or_else(|| {
        if tool == "purge_messages" {
            Some(format!("purge {purge_count} messages"))
        } else {
            None
        }
    })
}

/// The server's operation mode (standard by default, /mode to change).
pub fn mode_of(core: &Arc<AgentCore>, guild_id: Option<u64>) -> crate::memory::OperationMode {
    guild_id
        .map(|g| core.hive.knowledge.guild(g, |k| k.operation_mode))
        .unwrap_or_default()
}

/// Unused for now but keeps the User import honest for future approval
/// receipts.
#[allow(dead_code)]
fn user_id_of(user: &User) -> u64 {
    user.id.get()
}
