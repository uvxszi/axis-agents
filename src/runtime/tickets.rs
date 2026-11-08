//! Support tickets, Ticket Tool style but run by an agent: a panel with a
//! button (or a dropdown of ticket types); each ticket is a private
//! channel for the opener, staff, and the agent, with Close / Claim
//! buttons. Closing saves a transcript (and a short summary), posts it to
//! the log channel, optionally DMs it, and leaves a Reopen button.
//!
//! The setup lives in the guild's knowledge, so after a restart the agent
//! reconnects: component IDs are fixed, and every bot gets the presses of
//! the components it posted.

use std::path::PathBuf;
use std::sync::Arc;

use axis::model::{Interaction, InteractionData};
use axis::prelude::*;
use serde_json::{Value, json};

use super::agent::AgentCore;
use super::live::FeedKind;
use crate::memory::{Ticket, TicketOption, TicketSystem, now_ms};

pub const OPEN: &str = "axis_ticket:open";
pub const SELECT: &str = "axis_ticket:select";
pub const CLOSE: &str = "axis_ticket:close";
pub const CLAIM: &str = "axis_ticket:claim";
pub const REOPEN: &str = "axis_ticket:reopen";
pub const DELETE: &str = "axis_ticket:delete";

const VIEW: u64 = 1 << 10;
const SEND: u64 = 1 << 11;
const EMBED: u64 = 1 << 14;
const ATTACH: u64 = 1 << 15;
const HISTORY: u64 = 1 << 16;
const MANAGE_CHANNELS: u64 = 1 << 4;
const MANAGE_ROLES: u64 = 1 << 28;
const MANAGE_MESSAGES: u64 = 1 << 13;
const MEMBER: u64 = VIEW | SEND | HISTORY | ATTACH | EMBED;
const STAFF: u64 = MEMBER | MANAGE_MESSAGES;
const BOT: u64 = STAFF | MANAGE_CHANNELS | MANAGE_ROLES;

fn overwrite(id: u64, member: bool, allow: u64, deny: u64) -> Value {
    json!({ "id": id.to_string(), "type": u8::from(member), "allow": allow.to_string(), "deny": deny.to_string() })
}

fn api_error(error: AxisError) -> String {
    crate::tools::discord_error(error)
}

fn id_of(value: &Value) -> Option<u64> {
    value
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

pub fn system(core: &AgentCore, guild_id: u64) -> TicketSystem {
    core.hive.knowledge.guild(guild_id, |k| k.tickets.clone())
}

fn save(core: &AgentCore, guild_id: u64, edit: impl FnOnce(&mut TicketSystem)) {
    core.hive
        .knowledge
        .guild_mut(guild_id, |k| edit(&mut k.tickets));
}

/// Should the agent answer this author here because it's their ticket?
pub fn answers_in(core: &AgentCore, guild_id: u64, channel_id: u64, author: u64) -> bool {
    core.hive.knowledge.guild(guild_id, |k| {
        k.tickets.agent_id == core.id
            && k.tickets.auto_reply
            && k.tickets
                .open_ticket_in(channel_id)
                .is_some_and(|t| t.user_id == author)
    })
}

/// Find a ticket by "#12", "12", a channel, or the current channel.
pub fn find(system: &TicketSystem, query: Option<&str>, current_channel: u64) -> Option<Ticket> {
    match query.map(str::trim).filter(|q| !q.is_empty()) {
        None => system.ticket_in(current_channel).cloned(),
        Some(q) => {
            let digits = q
                .trim_start_matches(['#', ' '])
                .trim_start_matches("ticket")
                .trim_start_matches(['-', ' ', '#']);
            if let Ok(number) = digits.parse::<u32>() {
                if let Some(t) = system.by_number(number) {
                    return Some(t.clone());
                }
            }
            let id = crate::runtime::world::parse_snowflake(q)?;
            system.ticket_in(id).cloned()
        }
    }
}

// --- the panel ------------------------------------------------------------------------------

fn style_code(style: &str) -> u8 {
    match style.to_lowercase().as_str() {
        "secondary" | "grey" | "gray" => 2,
        "success" | "green" => 3,
        "danger" | "red" => 4,
        _ => 1,
    }
}

fn emoji_json(emoji: &str) -> Option<Value> {
    let emoji = emoji.trim();
    if emoji.is_empty() {
        return None;
    }
    // Custom emoji <:name:id> / <a:name:id>.
    if let Some(inner) = emoji.strip_prefix('<').and_then(|e| e.strip_suffix('>')) {
        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() == 3 {
            return Some(json!({ "name": parts[1], "id": parts[2], "animated": parts[0] == "a" }));
        }
    }
    Some(json!({ "name": emoji }))
}

pub fn panel_body(system: &TicketSystem) -> Value {
    let p = &system.panel;
    let component = if p.options.is_empty() {
        let mut button = json!({ "type": 2, "style": style_code(&p.button_style), "label": p.button_label, "custom_id": OPEN });
        if let Some(emoji) = emoji_json(&p.button_emoji) {
            button["emoji"] = emoji;
        }
        button
    } else {
        let options: Vec<Value> = p
            .options
            .iter()
            .enumerate()
            .take(25)
            .map(|(i, o)| {
                let mut option = json!({ "label": o.label, "value": i.to_string() });
                if !o.description.is_empty() {
                    option["description"] = json!(crate::llm::truncate(&o.description, 100));
                }
                if let Some(emoji) = emoji_json(&o.emoji) {
                    option["emoji"] = emoji;
                }
                option
            })
            .collect();
        json!({ "type": 3, "custom_id": SELECT, "placeholder": "Choose what you need help with", "options": options })
    };
    json!({
        "content": "",
        "embeds": [{ "title": p.title, "description": p.message, "color": p.color }],
        "components": [{ "type": 1, "components": [component] }],
        "allowed_mentions": { "parse": [] },
    })
}

/// Post or update the panel. Moving it removes the old one.
pub async fn render_panel(core: &AgentCore, guild_id: u64, channel_id: u64) -> Result<(), String> {
    let system = system(core, guild_id);
    let body = panel_body(&system);
    if system.panel_channel_id == channel_id && system.panel_message_id != 0 {
        let path = format!(
            "/channels/{channel_id}/messages/{}",
            system.panel_message_id
        );
        if core
            .http
            .raw("PATCH", &path, Some(body.clone()), None)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    let posted = core
        .http
        .raw(
            "POST",
            &format!("/channels/{channel_id}/messages"),
            Some(body),
            None,
        )
        .await
        .map_err(api_error)?;
    if system.panel_message_id != 0 && (system.panel_channel_id != channel_id) {
        let _ = core
            .http
            .raw(
                "DELETE",
                &format!(
                    "/channels/{}/messages/{}",
                    system.panel_channel_id, system.panel_message_id
                ),
                None,
                None,
            )
            .await;
    }
    let message_id = id_of(&posted).unwrap_or(0);
    save(core, guild_id, |t| {
        t.panel_channel_id = channel_id;
        t.panel_message_id = message_id;
    });
    Ok(())
}

// --- setup ---------------------------------------------------------------------------------------

/// Set up the ticket system, or reconnect to (and adjust) the existing one.
pub async fn setup(
    core: &Arc<AgentCore>,
    guild_id: u64,
    panel_channel: Option<u64>,
    category_name: &str,
    staff_roles: Vec<u64>,
    auto_reply: Option<bool>,
) -> Result<String, String> {
    let existing = system(core, guild_id);
    let me = core.world.my_id();
    let category_alive =
        existing.category_id != 0 && core.world.channel(guild_id, existing.category_id).is_some();
    let category_id = if category_alive {
        existing.category_id
    } else {
        let mut overwrites = vec![
            overwrite(guild_id, false, 0, VIEW),
            overwrite(me, true, BOT, 0),
        ];
        let roles = if staff_roles.is_empty() {
            existing.staff_roles.clone()
        } else {
            staff_roles.clone()
        };
        overwrites.extend(roles.iter().map(|r| overwrite(*r, false, STAFF, 0)));
        let created = core
            .http
            .raw(
                "POST",
                &format!("/guilds/{guild_id}/channels"),
                Some(json!({ "name": category_name, "type": 4, "permission_overwrites": overwrites })),
                Some("ticket system".to_owned()),
            )
            .await
            .map_err(api_error)?;
        id_of(&created).ok_or("Discord didn't return the new category")?
    };
    save(core, guild_id, |t| {
        t.agent_id = core.id.clone();
        t.category_id = category_id;
        if t.max_open == 0 {
            t.max_open = 1;
        }
        if let Some(auto) = auto_reply {
            t.auto_reply = auto;
        } else if !category_alive {
            t.auto_reply = true;
        }
    });
    if !staff_roles.is_empty() {
        set_staff(core, guild_id, &staff_roles, &[]).await?;
    }
    let panel_channel = panel_channel
        .or((existing.panel_channel_id != 0).then_some(existing.panel_channel_id))
        .ok_or("say which channel the ticket panel goes in")?;
    render_panel(core, guild_id, panel_channel).await?;
    let system = system(core, guild_id);
    let open = system.tickets.iter().filter(|t| t.closed.is_none()).count();
    core.feed(
        FeedKind::System,
        if category_alive {
            "reconnected to the ticket system"
        } else {
            "set up a ticket system"
        },
        Some(core.channel_label(Some(guild_id), panel_channel)),
    );
    Ok(format!(
        "{} — panel in <#{panel_channel}>, tickets in the category <#{category_id}>, staff: {}. {open} ticket(s) open.",
        if category_alive {
            "Ticket system updated (reconnected to the existing one)"
        } else {
            "Ticket system created"
        },
        if system.staff_roles.is_empty() {
            "none yet".to_owned()
        } else {
            system
                .staff_roles
                .iter()
                .map(|r| format!("<@&{r}>"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ))
}

/// Add/remove staff roles: they see every ticket (the category and each
/// open ticket are updated).
pub async fn set_staff(
    core: &AgentCore,
    guild_id: u64,
    add: &[u64],
    remove: &[u64],
) -> Result<(), String> {
    let system = system(core, guild_id);
    let mut channels = vec![system.category_id];
    channels.extend(
        system
            .tickets
            .iter()
            .filter(|t| t.closed.is_none())
            .map(|t| t.channel_id),
    );
    for channel in channels.into_iter().filter(|c| *c != 0) {
        for role in add {
            core.http
                .raw(
                    "PUT",
                    &format!("/channels/{channel}/permissions/{role}"),
                    Some(json!({ "type": 0, "allow": STAFF.to_string(), "deny": "0" })),
                    Some("ticket staff".to_owned()),
                )
                .await
                .map_err(api_error)?;
        }
        for role in remove {
            let _ = core
                .http
                .raw(
                    "DELETE",
                    &format!("/channels/{channel}/permissions/{role}"),
                    None,
                    Some("ticket staff".to_owned()),
                )
                .await;
        }
    }
    save(core, guild_id, |t| {
        for role in add {
            if !t.staff_roles.contains(role) {
                t.staff_roles.push(*role);
            }
        }
        t.staff_roles.retain(|r| !remove.contains(r));
    });
    Ok(())
}

// --- components -------------------------------------------------------------------------------

fn is_staff(core: &AgentCore, guild_id: u64, user_id: u64) -> bool {
    let roles = system(core, guild_id).staff_roles;
    core.world
        .with_guild(guild_id, |g| {
            let Some(member) = g.members.get(&user_id) else {
                return false;
            };
            let principal = super::authority::Principal::for_member(
                g,
                member,
                super::authority::PrincipalKind::Human,
            );
            principal.level >= crate::config::AccessLevel::Moderator
                || member.roles.iter().any(|r| roles.contains(r))
        })
        .unwrap_or(false)
}

/// A ticket component was used. Returns false when it isn't ours.
pub async fn on_component(core: &Arc<AgentCore>, interaction: &Interaction) -> bool {
    let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
        return false;
    };
    if !data.custom_id.starts_with("axis_ticket:") {
        return false;
    }
    let (Some(guild_id), Some(user)) = (
        interaction.guild_id.map(|g| g.get()),
        interaction
            .member
            .as_ref()
            .and_then(|m| m.user.clone())
            .or_else(|| interaction.user.clone()),
    ) else {
        return true;
    };
    // Creating channels and transcripts can take a moment: acknowledge now.
    let _ = core
        .http
        .defer_interaction(interaction.id, interaction.token.clone())
        .ephemeral(true)
        .await;
    let channel_id = interaction.channel_id.map(|c| c.get()).unwrap_or(0);
    let user_id = user.id.get();
    let name = user
        .global_name
        .clone()
        .unwrap_or_else(|| user.username.clone());
    let staff = is_staff(core, guild_id, user_id);
    let result = match data.custom_id.as_str() {
        OPEN => open_ticket(core, guild_id, user_id, &name, None)
            .await
            .map(|channel| format!("🎫 Your ticket is open: <#{channel}>")),
        SELECT => {
            let index: usize = data
                .values
                .as_ref()
                .and_then(|v| v.first())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let option = system(core, guild_id).panel.options.get(index).cloned();
            open_ticket(core, guild_id, user_id, &name, option.as_ref())
                .await
                .map(|channel| format!("🎫 Your ticket is open: <#{channel}>"))
        }
        CLOSE => {
            close_ticket(
                core,
                guild_id,
                channel_id,
                user_id,
                &name,
                staff,
                "closed with the button",
                false,
            )
            .await
        }
        CLAIM if staff => claim(core, guild_id, channel_id, user_id, &name).await,
        CLAIM => Err("only staff can claim tickets".to_owned()),
        REOPEN => reopen_ticket(core, guild_id, channel_id, user_id, &name, staff).await,
        DELETE if staff => {
            close_ticket(
                core, guild_id, channel_id, user_id, &name, true, "deleted", true,
            )
            .await
        }
        DELETE => Err("only staff can delete tickets".to_owned()),
        _ => Err("unknown button".to_owned()),
    };
    let content = match result {
        Ok(text) => text,
        Err(error) => format!("Couldn't do that: {error}"),
    };
    let _ = core
        .http
        .edit_followup(
            interaction.application_id,
            interaction.token.clone(),
            axis::rest::builders::ORIGINAL_RESPONSE,
            axis::rest::builders::FollowupEditFields::default().content(content),
        )
        .await;
    true
}

fn ticket_buttons(closed: bool) -> Value {
    if closed {
        json!([{ "type": 1, "components": [
            { "type": 2, "style": 3, "label": "Reopen", "emoji": { "name": "🔓" }, "custom_id": REOPEN },
            { "type": 2, "style": 4, "label": "Delete", "emoji": { "name": "🗑️" }, "custom_id": DELETE },
        ]}])
    } else {
        json!([{ "type": 1, "components": [
            { "type": 2, "style": 4, "label": "Close", "emoji": { "name": "🔒" }, "custom_id": CLOSE },
            { "type": 2, "style": 2, "label": "Claim", "emoji": { "name": "🙋" }, "custom_id": CLAIM },
        ]}])
    }
}

async fn post(core: &AgentCore, channel_id: u64, body: Value) -> Result<Value, String> {
    core.http
        .raw(
            "POST",
            &format!("/channels/{channel_id}/messages"),
            Some(body),
            None,
        )
        .await
        .map_err(api_error)
}

// --- lifecycle -------------------------------------------------------------------------------

/// Open a ticket for someone; returns the new channel.
pub async fn open_ticket(
    core: &Arc<AgentCore>,
    guild_id: u64,
    user_id: u64,
    user_name: &str,
    kind: Option<&TicketOption>,
) -> Result<u64, String> {
    let system = system(core, guild_id);
    if !system.is_set_up() {
        return Err("there's no ticket system here yet".to_owned());
    }
    let open: Vec<&Ticket> = system
        .tickets
        .iter()
        .filter(|t| t.user_id == user_id && t.closed.is_none())
        .collect();
    if open.len() as u32 >= system.max_open.max(1) {
        return Err(format!(
            "you already have an open ticket: <#{}>",
            open[0].channel_id
        ));
    }
    let number = system.next + 1;
    let me = core.world.my_id();
    let slug: String = user_name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(20)
        .collect();
    let mut overwrites = vec![
        overwrite(guild_id, false, 0, VIEW),
        overwrite(user_id, true, MEMBER, 0),
        overwrite(me, true, BOT, 0),
    ];
    overwrites.extend(
        system
            .staff_roles
            .iter()
            .map(|r| overwrite(*r, false, STAFF, 0)),
    );
    let kind_label = kind.map(|k| k.label.clone()).unwrap_or_default();
    let created = core
        .http
        .raw(
            "POST",
            &format!("/guilds/{guild_id}/channels"),
            Some(json!({
                "name": format!("ticket-{number:04}-{slug}"),
                "type": 0,
                "parent_id": system.category_id.to_string(),
                "topic": format!("Ticket #{number}{} — opened by {user_name}", if kind_label.is_empty() { String::new() } else { format!(" ({kind_label})") }),
                "permission_overwrites": overwrites,
            })),
            Some(format!("ticket #{number} for {user_name}")),
        )
        .await
        .map_err(api_error)?;
    let channel_id = id_of(&created).ok_or("Discord didn't return the ticket channel")?;
    if let Ok(channel) = serde_json::from_value::<axis::model::Channel>(created) {
        core.world.upsert_channel(&channel);
    }
    save(core, guild_id, |t| {
        t.next = number;
        t.tickets.push(Ticket {
            number,
            channel_id,
            user_id,
            user_name: user_name.to_owned(),
            kind: kind_label.clone(),
            opened: now_ms(),
            ..Default::default()
        });
    });
    let welcome = system
        .panel
        .welcome
        .replace("{user}", &format!("<@{user_id}>"))
        .replace("{number}", &number.to_string())
        .replace(
            "{type}",
            &if kind_label.is_empty() {
                String::new()
            } else {
                format!(" · {kind_label}")
            },
        );
    let _ = post(
        core,
        channel_id,
        json!({ "content": welcome, "components": ticket_buttons(false), "allowed_mentions": { "users": [user_id.to_string()] } }),
    )
    .await;
    core.feed(
        FeedKind::System,
        format!("opened ticket #{number} for {user_name}"),
        Some(core.channel_label(Some(guild_id), channel_id)),
    );
    let mut opener = format!("<@{user_id}> ({user_name})");
    if let Some(kind) = kind {
        opener.push_str(&format!(" — ticket type: {}", kind.label));
        if !kind.instruction.is_empty() {
            opener.push_str(&format!(". For this type: {}", kind.instruction));
        }
    }
    super::tasks::on_new_ticket(core, guild_id, channel_id, &opener);
    Ok(channel_id)
}

fn transcript_dir(guild_id: u64) -> PathBuf {
    crate::paths::data_dir()
        .join("tickets")
        .join(guild_id.to_string())
}

/// Read a transcript file saved for a ticket.
pub fn read_transcript(guild_id: u64, file: &str) -> Option<String> {
    std::fs::read_to_string(transcript_dir(guild_id).join(file)).ok()
}

/// Everything said in a ticket, oldest first, as plain text.
async fn build_transcript(
    core: &AgentCore,
    guild_id: u64,
    ticket: &Ticket,
    closer: &str,
    reason: &str,
) -> String {
    let mut messages: Vec<Value> = Vec::new();
    let mut before: Option<String> = None;
    for _ in 0..20 {
        let path = match &before {
            Some(b) => format!(
                "/channels/{}/messages?limit=100&before={b}",
                ticket.channel_id
            ),
            None => format!("/channels/{}/messages?limit=100", ticket.channel_id),
        };
        let Ok(Value::Array(page)) = core.http.raw("GET", &path, None, None).await else {
            break;
        };
        let len = page.len();
        before = page
            .last()
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        messages.extend(page);
        if len < 100 {
            break;
        }
    }
    messages.reverse();
    let fmt = |ms: i64| {
        Timestamp::from_unix_millis(ms)
            .map(|t| {
                t.to_string()
                    .chars()
                    .take(16)
                    .collect::<String>()
                    .replace('T', " ")
            })
            .unwrap_or_default()
    };
    let mut out = format!(
        "Ticket #{} — {}\nOpened by {} ({}) at {}\nClosed by {closer} at {}: {reason}\nServer: {}\n\n",
        ticket.number,
        if ticket.kind.is_empty() {
            "support"
        } else {
            &ticket.kind
        },
        ticket.user_name,
        ticket.user_id,
        fmt(ticket.opened),
        fmt(now_ms()),
        core.world
            .with_guild(guild_id, |g| g.name.clone())
            .unwrap_or_default(),
    );
    for m in &messages {
        let author = m
            .pointer("/author/global_name")
            .and_then(Value::as_str)
            .or_else(|| m.pointer("/author/username").and_then(Value::as_str))
            .unwrap_or("?");
        let time = m
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(16)
            .collect::<String>()
            .replace('T', " ");
        let mut line = m
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        for embed in m
            .get("embeds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for key in ["title", "description"] {
                if let Some(text) = embed.get(key).and_then(Value::as_str) {
                    line.push_str(&format!(" [{text}]"));
                }
            }
        }
        for file in m
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(url) = file.get("url").and_then(Value::as_str) {
                line.push_str(&format!(" [file: {url}]"));
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("[{time}] {author}: {line}\n"));
    }
    out
}

async fn summarize(core: &AgentCore, transcript: &str) -> String {
    let system = "Summarize a support ticket in two or three sentences: what the person needed, what was done, and how it ended. Plain text.";
    match super::brain::quick_completion(
        core,
        system,
        &crate::llm::truncate(transcript, 12_000),
        300,
        false,
    )
    .await
    {
        Ok(text) => crate::llm::text::split_think(&text).0.trim().to_owned(),
        Err(_) => String::new(),
    }
}

/// Close a ticket: save a transcript (posted to the log channel and,
/// if set, DMed to the opener), then lock it (Reopen/Delete buttons) or
/// delete it.
#[allow(clippy::too_many_arguments)]
pub async fn close_ticket(
    core: &Arc<AgentCore>,
    guild_id: u64,
    channel_id: u64,
    by: u64,
    by_name: &str,
    by_staff: bool,
    reason: &str,
    delete: bool,
) -> Result<String, String> {
    let system = system(core, guild_id);
    let Some(ticket) = system.ticket_in(channel_id).cloned() else {
        return Err("this isn't a ticket".to_owned());
    };
    if ticket.user_id != by && !by_staff {
        return Err("only the ticket's opener or staff can close it".to_owned());
    }
    if ticket.closed.is_some() && !delete {
        return Err(format!("ticket #{} is already closed", ticket.number));
    }

    let transcript = build_transcript(core, guild_id, &ticket, by_name, reason).await;
    let summary = summarize(core, &transcript).await;
    let file = format!(
        "ticket-{:04}-{}.txt",
        ticket.number,
        ticket.transcripts.len() + 1
    );
    let dir = transcript_dir(guild_id);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(&file), &transcript);
    let header = format!(
        "📄 **Ticket #{}** ({}) — opened by <@{}>, closed by {by_name}: {reason}{}",
        ticket.number,
        if ticket.kind.is_empty() {
            "support"
        } else {
            &ticket.kind
        },
        ticket.user_id,
        if summary.is_empty() {
            String::new()
        } else {
            format!("\n> {summary}")
        }
    );
    if system.log_channel_id != 0 {
        let _ = core
            .http
            .create_message(ChannelId::new(system.log_channel_id))
            .content(crate::llm::truncate(&header, 1900))
            .allowed_mentions(axis::model::AllowedMentions::none())
            .file(file.clone(), transcript.clone().into_bytes())
            .await;
    }
    if system.dm_transcripts {
        if let Ok(dm) = core.http.create_dm(UserId::new(ticket.user_id)).await {
            let _ = core
                .http
                .create_message(dm.id)
                .content(format!(
                    "Your ticket #{} was closed: {reason}. Here's the transcript.",
                    ticket.number
                ))
                .file(file.clone(), transcript.into_bytes())
                .await;
        }
    }
    save(core, guild_id, |t| {
        if let Some(stored) = t.tickets.iter_mut().find(|x| x.channel_id == channel_id) {
            stored.closed = Some(now_ms());
            stored.close_reason = reason.to_owned();
            stored.closed_by = by_name.to_owned();
            stored.transcripts.push(file.clone());
            if !summary.is_empty() {
                stored.summary = summary.clone();
            }
        }
    });

    if delete {
        core.http
            .raw(
                "DELETE",
                &format!("/channels/{channel_id}"),
                None,
                Some(format!("ticket #{} deleted", ticket.number)),
            )
            .await
            .map_err(api_error)?;
    } else if ticket.closed.is_none() {
        core.http
            .raw(
                "PUT",
                &format!("/channels/{channel_id}/permissions/{}", ticket.user_id),
                Some(json!({ "type": 1, "allow": "0", "deny": (VIEW | SEND).to_string() })),
                Some(format!("closed ticket #{}", ticket.number)),
            )
            .await
            .map_err(api_error)?;
        let _ = core
            .http
            .raw(
                "PATCH",
                &format!("/channels/{channel_id}"),
                Some(json!({ "name": format!("closed-{:04}", ticket.number) })),
                None,
            )
            .await;
        let _ = post(
            core,
            channel_id,
            json!({
                "content": format!("🔒 Ticket #{} closed by {by_name}: {reason}", ticket.number),
                "components": ticket_buttons(true),
                "allowed_mentions": { "parse": [] },
            }),
        )
        .await;
    }
    core.feed(
        FeedKind::System,
        format!("closed ticket #{} ({reason})", ticket.number),
        None,
    );
    Ok(format!(
        "closed ticket #{} ({}){}{}",
        ticket.number,
        ticket.user_name,
        if system.log_channel_id != 0 {
            format!("; transcript posted in <#{}>", system.log_channel_id)
        } else {
            "; transcript saved".to_owned()
        },
        if delete { "; channel deleted" } else { "" }
    ))
}

pub async fn reopen_ticket(
    core: &Arc<AgentCore>,
    guild_id: u64,
    channel_id: u64,
    by: u64,
    by_name: &str,
    by_staff: bool,
) -> Result<String, String> {
    let system = system(core, guild_id);
    let ticket = system
        .ticket_in(channel_id)
        .cloned()
        .ok_or("this isn't a ticket")?;
    if ticket.closed.is_none() {
        return Err(format!("ticket #{} is already open", ticket.number));
    }
    if ticket.user_id != by && !by_staff {
        return Err("only the ticket's opener or staff can reopen it".to_owned());
    }
    if core.world.channel(guild_id, channel_id).is_none() {
        return Err(format!(
            "ticket #{}'s channel was deleted — open a new one (its transcript is still saved)",
            ticket.number
        ));
    }
    core.http
        .raw(
            "PUT",
            &format!("/channels/{channel_id}/permissions/{}", ticket.user_id),
            Some(json!({ "type": 1, "allow": MEMBER.to_string(), "deny": "0" })),
            Some(format!("reopened ticket #{}", ticket.number)),
        )
        .await
        .map_err(api_error)?;
    let slug: String = ticket
        .user_name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(20)
        .collect();
    let _ = core
        .http
        .raw(
            "PATCH",
            &format!("/channels/{channel_id}"),
            Some(json!({ "name": format!("ticket-{:04}-{slug}", ticket.number) })),
            None,
        )
        .await;
    save(core, guild_id, |t| {
        if let Some(stored) = t.tickets.iter_mut().find(|x| x.channel_id == channel_id) {
            stored.closed = None;
        }
    });
    let _ = post(
        core,
        channel_id,
        json!({
            "content": format!("🔓 Ticket #{} reopened by {by_name} — <@{}>", ticket.number, ticket.user_id),
            "components": ticket_buttons(false),
            "allowed_mentions": { "users": [ticket.user_id.to_string()] },
        }),
    )
    .await;
    Ok(format!("reopened ticket #{}", ticket.number))
}

pub async fn claim(
    core: &AgentCore,
    guild_id: u64,
    channel_id: u64,
    staff_id: u64,
    staff_name: &str,
) -> Result<String, String> {
    let system = system(core, guild_id);
    let ticket = system
        .open_ticket_in(channel_id)
        .cloned()
        .ok_or("this isn't an open ticket")?;
    if let Some(other) = ticket.claimed_by.filter(|o| *o != staff_id) {
        return Err(format!("already claimed by <@{other}>"));
    }
    save(core, guild_id, |t| {
        if let Some(stored) = t.tickets.iter_mut().find(|x| x.channel_id == channel_id) {
            stored.claimed_by = Some(staff_id);
        }
    });
    let _ = post(
        core,
        channel_id,
        json!({ "content": format!("🙋 {staff_name} is handling this ticket."), "allowed_mentions": { "parse": [] } }),
    )
    .await;
    Ok(format!("you claimed ticket #{}", ticket.number))
}

pub async fn rename(core: &AgentCore, channel_id: u64, name: &str) -> Result<(), String> {
    core.http
        .raw(
            "PATCH",
            &format!("/channels/{channel_id}"),
            Some(json!({ "name": name })),
            Some("ticket renamed".to_owned()),
        )
        .await
        .map(|_| ())
        .map_err(api_error)
}

/// Give (or take away) a role's or member's access to one ticket.
pub async fn set_access(
    core: &AgentCore,
    channel_id: u64,
    id: u64,
    member: bool,
    grant: bool,
) -> Result<(), String> {
    let path = format!("/channels/{channel_id}/permissions/{id}");
    let result = if grant {
        core.http
            .raw(
                "PUT",
                &path,
                Some(json!({ "type": u8::from(member), "allow": MEMBER.to_string(), "deny": "0" })),
                Some("ticket access".to_owned()),
            )
            .await
    } else {
        core.http
            .raw("DELETE", &path, None, Some("ticket access".to_owned()))
            .await
    };
    result.map(|_| ()).map_err(api_error)
}

// --- searching --------------------------------------------------------------------------------

/// A ticket's full text: its saved transcripts, or (while open) what's
/// been said so far.
pub fn ticket_text(core: &AgentCore, guild_id: u64, ticket: &Ticket) -> String {
    let mut text = String::new();
    for file in &ticket.transcripts {
        if let Some(t) = read_transcript(guild_id, file) {
            text.push_str(&t);
            text.push('\n');
        }
    }
    if ticket.closed.is_none() {
        let lines: Vec<String> = core.hive.knowledge.channel(ticket.channel_id, |a| {
            a.messages
                .iter()
                .filter(|m| !m.deleted)
                .map(|m| format!("{}: {}", m.author, m.content))
                .collect()
        });
        text.push_str(&format!(
            "Ticket #{} (open) — {}\n{}",
            ticket.number,
            ticket.user_name,
            lines.join("\n")
        ));
    }
    text
}

/// Tickets mentioning all the query's words (or by that person), with a
/// snippet around the first match.
pub fn search(
    core: &AgentCore,
    guild_id: u64,
    query: &str,
    user: Option<u64>,
) -> Vec<(Ticket, String)> {
    let system = system(core, guild_id);
    let words: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(str::to_owned)
        .collect();
    let mut hits = Vec::new();
    for ticket in system.tickets.iter().rev() {
        if user.is_some_and(|u| ticket.user_id != u) {
            continue;
        }
        let text = format!(
            "{}\n{}",
            ticket.summary,
            ticket_text(core, guild_id, ticket)
        );
        let lower = text.to_lowercase();
        if !words.iter().all(|w| lower.contains(w.as_str())) {
            continue;
        }
        let at = words
            .first()
            .and_then(|w| lower.find(w.as_str()))
            .unwrap_or(0);
        let start = (at.saturating_sub(120)..=at)
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(0);
        let end = (at + 200).min(text.len());
        let end = (end..=text.len())
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(text.len());
        hits.push((ticket.clone(), text[start..end].replace('\n', " ")));
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_switches_to_a_dropdown_with_options() {
        let mut system = TicketSystem::default();
        let body = panel_body(&system);
        assert_eq!(body["components"][0]["components"][0]["type"], 2);
        system.panel.options.push(TicketOption {
            label: "Bug report".into(),
            emoji: "🐛".into(),
            ..Default::default()
        });
        let body = panel_body(&system);
        let menu = &body["components"][0]["components"][0];
        assert_eq!(menu["type"], 3);
        assert_eq!(menu["options"][0]["label"], "Bug report");
        assert_eq!(emoji_json("<a:spin:123>").unwrap()["id"], "123");
    }

    #[test]
    fn finds_tickets_by_number_or_channel() {
        let mut system = TicketSystem::default();
        system.tickets.push(Ticket {
            number: 7,
            channel_id: 555,
            ..Default::default()
        });
        assert_eq!(find(&system, Some("#7"), 0).unwrap().channel_id, 555);
        assert_eq!(find(&system, Some("ticket 7"), 0).unwrap().channel_id, 555);
        assert_eq!(find(&system, None, 555).unwrap().number, 7);
        assert!(find(&system, Some("#9"), 0).is_none());
    }
}
