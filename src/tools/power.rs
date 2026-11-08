//! Everything else a member can do in Discord: clone channels, channel
//! permissions, voice channel status, roles, bans, invites, events,
//! emojis, server settings, threads — plus tickets, and a raw API escape
//! hatch for admins.

use axis::prelude::{Permissions, Timestamp};
use serde_json::{Value, json};

use super::discord::{resolve_channel, resolve_member};
use super::{ToolCtx, arg, discord_error, flag, number, required};
use crate::config::AccessLevel;
use crate::memory::{StandingTask, TaskTrigger, now_ms};
use crate::runtime::authority::PrincipalKind;
use crate::runtime::live::FeedKind;
use crate::runtime::tickets;

fn label(ctx: &ToolCtx, channel_id: u64) -> String {
    ctx.core.channel_label(ctx.guild_id, channel_id)
}

fn require(ctx: &ToolCtx, channel_id: u64, perms: Permissions) -> Result<(), String> {
    ctx.require_in(channel_id, perms)
}

/// Guild-wide permission check (for things that aren't per channel).
fn require_guild_perm(ctx: &ToolCtx, perms: Permissions) -> Result<u64, String> {
    let guild_id = ctx.require_guild()?;
    if ctx.requester.kind == PrincipalKind::Operator || ctx.acting_as_agent() {
        return Ok(guild_id);
    }
    if ctx.requester.permissions.contains(perms)
        || ctx
            .requester
            .permissions
            .contains(Permissions::ADMINISTRATOR)
    {
        Ok(guild_id)
    } else {
        Err(format!(
            "denied: {} doesn't have the {perms:?} permission",
            ctx.requester.name
        ))
    }
}

fn id_of(value: &Value) -> Option<u64> {
    value
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

fn find_role(ctx: &ToolCtx, guild_id: u64, query: &str) -> Result<(u64, String, i32), String> {
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            g.find_role(query)
                .map(|r| (r.id.get(), r.name.clone(), r.position))
        })
        .flatten()
        .ok_or_else(|| format!("no role matching \"{query}\""))
}

/// "send messages, view channel" → bits.
fn permission_bits(list: &str) -> Result<u64, String> {
    let mut bits = 0u64;
    for item in list
        .split([',', ';', '|'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let name = item
            .to_uppercase()
            .replace([' ', '-'], "_")
            .replace("MESSAGE_HISTORY", "READ_MESSAGE_HISTORY")
            .replace("READ_READ_", "READ_");
        let name = match name.as_str() {
            "VIEW" | "SEE" | "READ" | "READ_MESSAGES" => "VIEW_CHANNEL".to_owned(),
            "SEND" | "TALK" | "CHAT" | "WRITE" => "SEND_MESSAGES".to_owned(),
            "ADMIN" => "ADMINISTRATOR".to_owned(),
            "MANAGE_PERMISSIONS" => "MANAGE_ROLES".to_owned(),
            "REACT" | "REACTIONS" => "ADD_REACTIONS".to_owned(),
            "ATTACH" | "UPLOAD" | "FILES" => "ATTACH_FILES".to_owned(),
            "VOICE_STATUS" => "SET_VOICE_CHANNEL_STATUS".to_owned(),
            _ => name,
        };
        let flag = Permissions::from_name(&name)
            .ok_or_else(|| format!("unknown permission \"{item}\""))?;
        bits |= flag.bits();
    }
    Ok(bits)
}

// --- channels ------------------------------------------------------------------------

pub async fn clone_channel(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let source = resolve_channel(ctx, arg(args, "channel"))?;
    require(ctx, source, Permissions::MANAGE_CHANNELS)?;
    let original = ctx
        .core
        .http
        .raw("GET", &format!("/channels/{source}"), None, None)
        .await
        .map_err(discord_error)?;
    let name = arg(args, "name").unwrap_or_else(|| {
        format!(
            "{}-copy",
            original
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("channel")
        )
    });
    let mut body = json!({ "name": name });
    for key in [
        "type",
        "topic",
        "nsfw",
        "rate_limit_per_user",
        "bitrate",
        "user_limit",
        "parent_id",
        "permission_overwrites",
        "rtc_region",
        "video_quality_mode",
        "default_auto_archive_duration",
        "default_reaction_emoji",
        "available_tags",
        "default_sort_order",
        "default_forum_layout",
        "default_thread_rate_limit_per_user",
        "flags",
    ] {
        if let Some(value) = original.get(key).filter(|v| !v.is_null()) {
            body[key] = value.clone();
        }
    }
    // Forum tags can't keep their old IDs.
    if let Some(tags) = body.get_mut("available_tags").and_then(Value::as_array_mut) {
        for tag in tags {
            if let Some(tag) = tag.as_object_mut() {
                tag.remove("id");
            }
        }
    }
    if let Some(position) = original.get("position").and_then(Value::as_i64) {
        body["position"] = json!(position + 1);
    }
    let created = ctx
        .core
        .http
        .raw(
            "POST",
            &format!("/guilds/{guild_id}/channels"),
            Some(body),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    let id = id_of(&created).ok_or("Discord didn't return the new channel")?;
    if let Ok(channel) = serde_json::from_value::<axis::model::Channel>(created) {
        ctx.core.world.upsert_channel(&channel);
    }
    ctx.core.feed(
        FeedKind::Tool,
        format!("cloned #{} as <#{id}>", label(ctx, source)),
        None,
    );
    Ok(format!(
        "cloned {} into <#{id}> — same type, category, topic, settings, and permissions (messages aren't copied)",
        label(ctx, source)
    ))
}

/// Reorder: put a channel or category above/below another, at the top or
/// bottom, or into another category.
pub async fn move_channel(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    use axis::model::ChannelType;
    let guild_id = ctx.require_guild()?;
    let target = resolve_channel(ctx, arg(args, "channel"))?;
    require(ctx, target, Permissions::MANAGE_CHANNELS)?;
    // (id, is category, parent, position)
    let all: Vec<(u64, bool, Option<u64>, i32)> = ctx
        .core
        .world
        .with_guild(guild_id, |g| {
            g.channels
                .values()
                .filter(|c| !c.is_thread())
                .map(|c| {
                    (
                        c.id.get(),
                        c.r#type == ChannelType::GuildCategory,
                        c.parent_id.map(|p| p.get()),
                        c.position.unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let info = |id: u64| all.iter().find(|c| c.0 == id).copied();
    let (_, is_category, old_parent, _) = info(target).ok_or("I don't know that channel")?;

    let mut parent = old_parent;
    if let Some(query) = arg(args, "category") {
        if is_category {
            return Err("categories can't go inside other categories".to_owned());
        }
        parent = if matches!(
            query.to_lowercase().as_str(),
            "none" | "no category" | "nothing" | "top level"
        ) {
            None
        } else {
            let id = resolve_channel(ctx, Some(query.clone()))?;
            if !info(id).is_some_and(|c| c.1) {
                return Err(format!("{} isn't a category", label(ctx, id)));
            }
            Some(id)
        };
    }
    let resolve_anchor = |key: &str| -> Result<Option<u64>, String> {
        arg(args, key)
            .map(|q| resolve_channel(ctx, Some(q)))
            .transpose()
    };
    let above = resolve_anchor("above")?;
    let below = resolve_anchor("below")?;
    let anchor = above.or(below);
    if let Some(anchor) = anchor {
        let (_, anchor_is_category, anchor_parent, _) =
            info(anchor).ok_or("I don't know the channel to put it next to")?;
        if is_category && !anchor_is_category {
            return Err("a category can only go above or below another category".to_owned());
        }
        if !is_category && arg(args, "category").is_none() {
            parent = if anchor_is_category {
                Some(anchor)
            } else {
                anchor_parent
            };
        }
    }

    // Siblings in their current order, without the one being moved.
    let mut order: Vec<(u64, i32)> = all
        .iter()
        .filter(|c| c.0 != target && c.1 == is_category && (is_category || c.2 == parent))
        .map(|c| (c.0, c.3))
        .collect();
    order.sort_by_key(|(id, position)| (*position, *id));
    let mut order: Vec<u64> = order.into_iter().map(|(id, _)| id).collect();
    let place = arg(args, "position").unwrap_or_default().to_lowercase();
    let index = match (above, below) {
        (Some(a), _) if order.contains(&a) => order.iter().position(|id| *id == a).unwrap_or(0),
        (_, Some(b)) if order.contains(&b) => order
            .iter()
            .position(|id| *id == b)
            .map_or(order.len(), |i| i + 1),
        // Into a category named as the anchor: its top.
        _ if anchor.is_some() => 0,
        _ if place == "top" || place == "first" => 0,
        _ => order.len(),
    };
    order.insert(index.min(order.len()), target);

    let body: Vec<Value> = order
        .iter()
        .enumerate()
        .map(|(position, id)| {
            let mut entry = json!({ "id": id.to_string(), "position": position });
            if *id == target && parent != old_parent {
                entry["parent_id"] = parent.map_or(Value::Null, |p| json!(p.to_string()));
                entry["lock_permissions"] = json!(flag(args, "sync_permissions").unwrap_or(false));
            }
            entry
        })
        .collect();
    ctx.core
        .http
        .raw(
            "PATCH",
            &format!("/guilds/{guild_id}/channels"),
            Some(Value::Array(body)),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    let place = match (above, below) {
        (Some(a), _) => format!("above {}", label(ctx, a)),
        (_, Some(b)) => format!("below {}", label(ctx, b)),
        _ if index == 0 => "to the top".to_owned(),
        _ => "to the bottom".to_owned(),
    };
    let moved_into = if parent != old_parent {
        match parent {
            Some(p) => format!(" in {}", label(ctx, p)),
            None => " (no category)".to_owned(),
        }
    } else {
        String::new()
    };
    Ok(format!("moved {} {place}{moved_into}", label(ctx, target)))
}

pub async fn set_voice_status(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let channel_id = match arg(args, "channel") {
        Some(q) => resolve_channel(ctx, Some(q))?,
        None => ctx
            .core
            .voice
            .channel(guild_id)
            .or_else(|| {
                ctx.core
                    .world
                    .with_guild(guild_id, |g| g.voice.get(&ctx.requester.user_id).copied())
                    .flatten()
            })
            .ok_or("say which voice channel")?,
    };
    if !ctx
        .core
        .world
        .channel(guild_id, channel_id)
        .is_some_and(|c| c.is_voice())
    {
        return Err(format!("{} isn't a voice channel", label(ctx, channel_id)));
    }
    let perms = ctx.requester_permissions_in(channel_id);
    if !ctx.acting_as_agent()
        && !perms.intersects(Permissions::SET_VOICE_CHANNEL_STATUS | Permissions::MANAGE_CHANNELS)
    {
        return Err(format!(
            "denied: {} can't set that channel's status",
            ctx.requester.name
        ));
    }
    let status = arg(args, "status").unwrap_or_default();
    ctx.core
        .http
        .raw(
            "PUT",
            &format!("/channels/{channel_id}/voice-status"),
            Some(json!({ "status": crate::llm::truncate(&status, 500) })),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(|e| {
            let text = discord_error(e);
            format!("{text} (bots can only set the status of a voice channel they're in, unless they can manage it)")
        })?;
    Ok(if status.is_empty() {
        format!("cleared the status of {}", label(ctx, channel_id))
    } else {
        format!(
            "set the status of {} to \"{status}\"",
            label(ctx, channel_id)
        )
    })
}

pub async fn set_channel_permissions(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    require(ctx, channel_id, Permissions::MANAGE_ROLES)?;
    let target = required(args, "target")?;
    let (id, is_member, name) =
        if matches!(target.to_lowercase().as_str(), "everyone" | "@everyone") {
            (guild_id, false, "@everyone".to_owned())
        } else if let Ok((id, name, _)) = find_role(ctx, guild_id, &target) {
            (id, false, format!("@{name}"))
        } else {
            let member = resolve_member(ctx, &target).await?;
            (member.user_id, true, member.display_name().to_owned())
        };
    let path = format!("/channels/{channel_id}/permissions/{id}");
    if flag(args, "reset").unwrap_or(false) {
        ctx.core
            .http
            .raw("DELETE", &path, None, Some(ctx.reason(None)))
            .await
            .map_err(discord_error)?;
        return Ok(format!(
            "removed {name}'s special permissions in {}",
            label(ctx, channel_id)
        ));
    }
    let allow = permission_bits(&arg(args, "allow").unwrap_or_default())?;
    let deny = permission_bits(&arg(args, "deny").unwrap_or_default())?;
    if allow == 0 && deny == 0 {
        return Err("say which permissions to allow and/or deny (e.g. allow: \"view channel, send messages\")".to_owned());
    }
    ctx.core
        .http
        .raw(
            "PUT",
            &path,
            Some(json!({ "type": u8::from(is_member), "allow": allow.to_string(), "deny": deny.to_string() })),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "{name} in {}: allowed {:?}, denied {:?}",
        label(ctx, channel_id),
        Permissions::from_bits_truncate(allow),
        Permissions::from_bits_truncate(deny)
    ))
}

pub async fn edit_thread(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let thread = resolve_channel(ctx, arg(args, "thread"))?;
    require(ctx, thread, Permissions::MANAGE_THREADS)?;
    let mut body = json!({});
    if let Some(name) = arg(args, "name") {
        body["name"] = json!(name);
    }
    if let Some(archived) = flag(args, "archived") {
        body["archived"] = json!(archived);
    }
    if let Some(locked) = flag(args, "locked") {
        body["locked"] = json!(locked);
    }
    if let Some(tags) = arg(args, "tags") {
        // Forum tags by name.
        let parent = ctx
            .core
            .world
            .channel(ctx.guild_id.unwrap_or(0), thread)
            .and_then(|c| c.parent_id.map(|p| p.get()))
            .ok_or("that isn't a forum post")?;
        let forum = ctx
            .core
            .http
            .raw("GET", &format!("/channels/{parent}"), None, None)
            .await
            .map_err(discord_error)?;
        let wanted: Vec<String> = tags.split(',').map(|t| t.trim().to_lowercase()).collect();
        let ids: Vec<String> = forum
            .get("available_tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|t| {
                t.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| wanted.contains(&n.to_lowercase()))
            })
            .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        body["applied_tags"] = json!(ids);
    }
    ctx.core
        .http
        .raw(
            "PATCH",
            &format!("/channels/{thread}"),
            Some(body),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!("updated {}", label(ctx, thread)))
}

// --- messages -----------------------------------------------------------------------------

fn message_target(ctx: &ToolCtx, args: &Value) -> Result<(u64, u64), String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    let message_id = arg(args, "message_id")
        .and_then(|m| m.trim().parse().ok())
        .or(ctx.message_id.filter(|_| arg(args, "channel").is_none()))
        .ok_or("which message? give its message_id")?;
    Ok((channel_id, message_id))
}

pub async fn unpin_message(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let (channel_id, message_id) = message_target(ctx, args)?;
    require(ctx, channel_id, Permissions::MANAGE_MESSAGES)?;
    ctx.core
        .http
        .raw(
            "DELETE",
            &format!("/channels/{channel_id}/pins/{message_id}"),
            None,
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "unpinned that message in {}",
        label(ctx, channel_id)
    ))
}

pub async fn publish_message(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let (channel_id, message_id) = message_target(ctx, args)?;
    require(ctx, channel_id, Permissions::SEND_MESSAGES)?;
    ctx.core
        .http
        .raw(
            "POST",
            &format!("/channels/{channel_id}/messages/{message_id}/crosspost"),
            None,
            None,
        )
        .await
        .map_err(discord_error)?;
    Ok("published it to the servers following this announcement channel".to_owned())
}

pub async fn end_poll(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let (channel_id, message_id) = message_target(ctx, args)?;
    ctx.core
        .http
        .raw(
            "POST",
            &format!("/channels/{channel_id}/polls/{message_id}/expire"),
            None,
            None,
        )
        .await
        .map_err(discord_error)?;
    Ok("ended the poll — results are final".to_owned())
}

// --- roles & members ------------------------------------------------------------------------

pub async fn edit_role(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_ROLES)?;
    let (role_id, name, position) = find_role(ctx, guild_id, &required(args, "role")?)?;
    if !ctx.acting_as_agent()
        && ctx.requester.kind != PrincipalKind::Operator
        && position.max(0) as u32 >= ctx.requester.rank
    {
        return Err(format!(
            "denied: @{name} is at or above {}'s highest role",
            ctx.requester.name
        ));
    }
    let current = ctx
        .core
        .world
        .with_guild(guild_id, |g| {
            g.roles.get(&role_id).map(|r| r.permissions.bits())
        })
        .flatten()
        .unwrap_or(0);
    let mut body = json!({});
    if let Some(new_name) = arg(args, "name") {
        body["name"] = json!(new_name);
    }
    if let Some(color) = arg(args, "color") {
        let hex = color.trim().trim_start_matches('#');
        body["color"] = json!(
            u32::from_str_radix(hex, 16)
                .map_err(|_| format!("color should be hex like #ff8800, not {color}"))?
        );
    }
    if let Some(hoist) = flag(args, "hoist") {
        body["hoist"] = json!(hoist);
    }
    if let Some(mentionable) = flag(args, "mentionable") {
        body["mentionable"] = json!(mentionable);
    }
    let add = permission_bits(&arg(args, "add_permissions").unwrap_or_default())?;
    let remove = permission_bits(&arg(args, "remove_permissions").unwrap_or_default())?;
    if add != 0 || remove != 0 {
        body["permissions"] = json!(((current | add) & !remove).to_string());
    }
    ctx.core
        .http
        .raw(
            "PATCH",
            &format!("/guilds/{guild_id}/roles/{role_id}"),
            Some(body),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!("updated @{name}"))
}

pub async fn delete_role(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_ROLES)?;
    let (role_id, name, position) = find_role(ctx, guild_id, &required(args, "role")?)?;
    if !ctx.acting_as_agent()
        && ctx.requester.kind != PrincipalKind::Operator
        && position.max(0) as u32 >= ctx.requester.rank
    {
        return Err(format!(
            "denied: @{name} is at or above {}'s highest role",
            ctx.requester.name
        ));
    }
    ctx.core
        .http
        .raw(
            "DELETE",
            &format!("/guilds/{guild_id}/roles/{role_id}"),
            None,
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!("deleted @{name}"))
}

pub async fn list_bans(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::BAN_MEMBERS)?;
    let bans = ctx
        .core
        .http
        .raw(
            "GET",
            &format!("/guilds/{guild_id}/bans?limit=100"),
            None,
            None,
        )
        .await
        .map_err(discord_error)?;
    let lines: Vec<String> = bans
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| {
            format!(
                "- {} ({}): {}",
                b.pointer("/user/username")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                b.pointer("/user/id").and_then(Value::as_str).unwrap_or("?"),
                b.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason")
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        "nobody is banned".to_owned()
    } else {
        lines.join("\n")
    })
}

pub async fn unban_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::BAN_MEMBERS)?;
    let query = required(args, "user")?;
    let user_id = match query.trim().parse::<u64>() {
        Ok(id) => id,
        Err(_) => {
            let bans = ctx
                .core
                .http
                .raw(
                    "GET",
                    &format!("/guilds/{guild_id}/bans?limit=1000"),
                    None,
                    None,
                )
                .await
                .map_err(discord_error)?;
            let wanted = query.trim_start_matches('@').to_lowercase();
            bans.as_array()
                .into_iter()
                .flatten()
                .find(|b| {
                    b.pointer("/user/username")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.to_lowercase() == wanted)
                        || b.pointer("/user/global_name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| n.to_lowercase() == wanted)
                })
                .and_then(|b| {
                    b.pointer("/user/id")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse().ok())
                })
                .ok_or_else(|| format!("no banned user named {query}"))?
        }
    };
    ctx.core
        .http
        .raw(
            "DELETE",
            &format!("/guilds/{guild_id}/bans/{user_id}"),
            None,
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!("unbanned <@{user_id}>"))
}

// --- server -------------------------------------------------------------------------------

pub async fn edit_server(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_GUILD)?;
    let mut body = json!({});
    if let Some(name) = arg(args, "name") {
        body["name"] = json!(name);
    }
    if let Some(description) = arg(args, "description") {
        body["description"] = json!(description);
    }
    for (key, field) in [
        ("system_channel", "system_channel_id"),
        ("afk_channel", "afk_channel_id"),
        ("rules_channel", "rules_channel_id"),
    ] {
        if let Some(query) = arg(args, key) {
            body[field] = json!(resolve_channel(ctx, Some(query))?.to_string());
        }
    }
    if let Some(timeout) = number(args, "afk_timeout_minutes") {
        body["afk_timeout"] = json!(
            [1i64, 5, 15, 30, 60]
                .into_iter()
                .min_by_key(|m| (m - timeout).abs())
                .unwrap_or(5)
                * 60
        );
    }
    if let Some(level) = number(args, "verification_level") {
        body["verification_level"] = json!(level.clamp(0, 4));
    }
    if body.as_object().is_some_and(|o| o.is_empty()) {
        return Err("say what to change (name, description, system_channel, afk_channel, afk_timeout_minutes, verification_level)".to_owned());
    }
    ctx.core
        .http
        .raw(
            "PATCH",
            &format!("/guilds/{guild_id}"),
            Some(body),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok("updated the server settings".to_owned())
}

/// "in 2 hours", "tomorrow", or an ISO time.
fn parse_time(text: &str) -> Result<Timestamp, String> {
    let t = text.trim().to_lowercase();
    let now = now_ms();
    let relative = |ms: i64| Timestamp::from_unix_millis(now + ms);
    if let Some(rest) = t.strip_prefix("in ") {
        let mut parts = rest.split_whitespace();
        let amount: f64 = parts.next().and_then(|n| n.parse().ok()).unwrap_or(1.0);
        let unit = parts.next().unwrap_or("hours");
        let ms = match unit.trim_end_matches('s') {
            "minute" | "min" => 60_000.0,
            "hour" | "hr" | "h" => 3_600_000.0,
            "day" | "d" => 86_400_000.0,
            "week" => 604_800_000.0,
            _ => return Err(format!("can't read the time \"{text}\"")),
        };
        return relative((amount * ms) as i64).map_err(|e| e.to_string());
    }
    if t == "tomorrow" {
        return relative(86_400_000).map_err(|e| e.to_string());
    }
    text.trim().parse::<Timestamp>().map_err(|_| {
        format!(
            "can't read the time \"{text}\" — use \"in 2 hours\" or ISO like 2026-11-02T20:00:00Z"
        )
    })
}

pub async fn create_event(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::CREATE_EVENTS)?;
    let start = parse_time(&required(args, "start")?)?;
    let minutes = number(args, "duration_minutes")
        .unwrap_or(60)
        .clamp(5, 10_080);
    let end = Timestamp::from_unix_millis(start.as_unix_millis() + minutes * 60_000)
        .map_err(|e| e.to_string())?;
    let mut body = json!({
        "name": required(args, "name")?,
        "description": arg(args, "description").unwrap_or_default(),
        "scheduled_start_time": start.to_string(),
        "scheduled_end_time": end.to_string(),
        "privacy_level": 2,
    });
    match arg(args, "channel")
        .map(|q| resolve_channel(ctx, Some(q)))
        .transpose()?
    {
        Some(channel)
            if ctx
                .core
                .world
                .channel(guild_id, channel)
                .is_some_and(|c| c.is_voice()) =>
        {
            body["entity_type"] = json!(2);
            body["channel_id"] = json!(channel.to_string());
        }
        _ => {
            body["entity_type"] = json!(3);
            body["entity_metadata"] = json!({ "location": arg(args, "location").unwrap_or_else(|| "Discord".to_owned()) });
        }
    }
    let event = ctx
        .core
        .http
        .raw(
            "POST",
            &format!("/guilds/{guild_id}/scheduled-events"),
            Some(body),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "created the event \"{}\" starting <t:{}:F> (event ID {})",
        event.get("name").and_then(Value::as_str).unwrap_or("?"),
        start.as_unix_millis() / 1000,
        event.get("id").and_then(Value::as_str).unwrap_or("?")
    ))
}

pub async fn list_events(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let events = ctx
        .core
        .http
        .raw(
            "GET",
            &format!("/guilds/{guild_id}/scheduled-events?with_user_count=true"),
            None,
            None,
        )
        .await
        .map_err(discord_error)?;
    let lines: Vec<String> = events
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            format!(
                "- {} (ID {}): starts {} · {} interested",
                e.get("name").and_then(Value::as_str).unwrap_or("?"),
                e.get("id").and_then(Value::as_str).unwrap_or("?"),
                e.get("scheduled_start_time")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                e.get("user_count").and_then(Value::as_u64).unwrap_or(0)
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        "no scheduled events".to_owned()
    } else {
        lines.join("\n")
    })
}

pub async fn delete_event(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_EVENTS)?;
    let query = required(args, "event")?;
    let id = match query.trim().parse::<u64>() {
        Ok(id) => id,
        Err(_) => {
            let events = ctx
                .core
                .http
                .raw(
                    "GET",
                    &format!("/guilds/{guild_id}/scheduled-events"),
                    None,
                    None,
                )
                .await
                .map_err(discord_error)?;
            events
                .as_array()
                .into_iter()
                .flatten()
                .find(|e| {
                    e.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.to_lowercase().contains(&query.to_lowercase()))
                })
                .and_then(id_of)
                .ok_or_else(|| format!("no event matching \"{query}\""))?
        }
    };
    ctx.core
        .http
        .raw(
            "DELETE",
            &format!("/guilds/{guild_id}/scheduled-events/{id}"),
            None,
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok("deleted the event".to_owned())
}

pub async fn create_emoji(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::CREATE_GUILD_EXPRESSIONS)?;
    let name: String = required(args, "name")?
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .take(32)
        .collect();
    let source = required(args, "image")?;
    let url = if source.starts_with("http") {
        source.clone()
    } else {
        ctx.inputs
            .iter()
            .find(|i| i.name.eq_ignore_ascii_case(&source))
            .map(|i| i.url.clone())
            .ok_or_else(|| {
                format!("give an image URL (or the name of an image in the chat), not \"{source}\"")
            })?
    };
    let bytes = super::web::fetch_bytes(&url, false, 256 * 1024)
        .await
        .map_err(|e| format!("couldn't download the image (emojis must be under 256 KB): {e}"))?;
    let mime = match bytes.get(..4) {
        Some([0x89, b'P', b'N', b'G']) => "image/png",
        Some([b'G', b'I', b'F', _]) => "image/gif",
        Some([b'R', b'I', b'F', b'F']) => "image/webp",
        _ => "image/jpeg",
    };
    let emoji = ctx
        .core
        .http
        .raw(
            "POST",
            &format!("/guilds/{guild_id}/emojis"),
            Some(json!({ "name": name, "image": format!("data:{mime};base64,{}", crate::runtime::brain::base64(&bytes)) })),
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    let id = emoji.get("id").and_then(Value::as_str).unwrap_or("?");
    let animated = emoji
        .get("animated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(format!(
        "added the emoji <{}:{name}:{id}>",
        if animated { "a" } else { "" }
    ))
}

pub async fn delete_emoji(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_GUILD_EXPRESSIONS)?;
    let query = required(args, "emoji")?;
    let wanted = query
        .trim_matches([':', '<', '>'])
        .rsplit(':')
        .nth(1)
        .unwrap_or(query.trim_matches(':'))
        .to_lowercase();
    let id = ctx
        .core
        .world
        .with_guild(guild_id, |g| {
            g.emojis
                .iter()
                .find(|e| {
                    e.name
                        .as_deref()
                        .is_some_and(|n| n.to_lowercase() == wanted)
                        || e.id.map(|i| i.to_string()) == Some(query.clone())
                })
                .and_then(|e| e.id.map(|i| i.get()))
        })
        .flatten()
        .ok_or_else(|| format!("no emoji named {query}"))?;
    ctx.core
        .http
        .raw(
            "DELETE",
            &format!("/guilds/{guild_id}/emojis/{id}"),
            None,
            Some(ctx.reason(None)),
        )
        .await
        .map_err(discord_error)?;
    Ok(format!("deleted :{wanted}:"))
}

// --- tickets ------------------------------------------------------------------------------

/// A text channel by name — so "ticket support" means the channel, not a
/// category with the same name.
fn resolve_text_channel(ctx: &ToolCtx, query: Option<String>) -> Result<u64, String> {
    use axis::model::ChannelType;
    if let (Some(q), Some(guild_id)) = (&query, ctx.guild_id) {
        let found = ctx
            .core
            .world
            .with_guild(guild_id, |g| {
                g.find_channel_where(q, |c| {
                    matches!(
                        c.r#type,
                        ChannelType::GuildText | ChannelType::GuildAnnouncement
                    )
                })
                .map(|c| c.id.get())
            })
            .flatten();
        if let Some(id) = found {
            return Ok(id);
        }
    }
    resolve_channel(ctx, query)
}

fn staff_check(ctx: &ToolCtx, channel_id: u64) -> bool {
    ctx.requester.kind == PrincipalKind::Operator
        || ctx.acting_as_agent()
        || ctx.requester.level >= AccessLevel::Moderator
        || ctx
            .requester_permissions_in(channel_id)
            .contains(Permissions::MANAGE_MESSAGES)
        || tickets::system(&ctx.core, ctx.guild_id.unwrap_or(0))
            .staff_roles
            .iter()
            .any(|r| ctx.requester.roles.contains(r))
}

fn ticket_for(ctx: &ToolCtx, args: &Value) -> Result<crate::memory::Ticket, String> {
    let guild_id = ctx.require_guild()?;
    let system = tickets::system(&ctx.core, guild_id);
    tickets::find(&system, arg(args, "ticket").as_deref(), ctx.channel_id).ok_or_else(|| {
        "which ticket? give its number (like #3), or ask inside the ticket".to_owned()
    })
}

fn roles_list(ctx: &ToolCtx, guild_id: u64, list: Option<String>) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    for role in list
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        out.push(find_role(ctx, guild_id, role)?.0);
    }
    Ok(out)
}

pub async fn setup_tickets(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    ensure_settings(ctx)?;
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_CHANNELS)?;
    let existing = tickets::system(&ctx.core, guild_id);
    // Updating an existing system keeps its panel where it is unless told
    // otherwise; a new one goes where it was asked for.
    let panel_channel = match arg(args, "panel_channel") {
        Some(q) => Some(resolve_text_channel(ctx, Some(q))?),
        None if existing.is_set_up() => None,
        None if existing.open_ticket_in(ctx.channel_id).is_some() => {
            return Err("say which channel the panel goes in (not inside a ticket)".to_owned());
        }
        None => Some(ctx.channel_id),
    };
    let staff = roles_list(ctx, guild_id, arg(args, "staff_roles"))?;
    let result = tickets::setup(
        &ctx.core,
        guild_id,
        panel_channel,
        &arg(args, "category_name").unwrap_or_else(|| "Tickets".to_owned()),
        staff,
        flag(args, "auto_reply"),
    )
    .await?;
    // What to do with each new ticket is a standing task, so it can be
    // listed, changed, or stopped like any other.
    let category = tickets::system(&ctx.core, guild_id).category_id;
    let given = arg(args, "instruction");
    let task_id = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        if let Some(task) = k
            .tasks
            .iter_mut()
            .find(|t| t.agent_id == ctx.core.id && t.active && matches!(t.trigger, TaskTrigger::NewTicket { .. }))
        {
            if let Some(instruction) = &given {
                task.instruction = instruction.clone();
            }
            task.trigger = TaskTrigger::NewTicket { channel_id: category };
            return task.id;
        }
        k.next_task += 1;
        let id = k.next_task;
        k.tasks.push(StandingTask {
            id,
            agent_id: ctx.core.id.clone(),
            instruction: given.clone().unwrap_or_else(|| {
                "Greet them by name, ask what they need help with, and help them. If it needs staff, say a staff member will follow up.".to_owned()
            }),
            trigger: TaskTrigger::NewTicket { channel_id: category },
            author_id: ctx.requester.user_id,
            author_name: ctx.requester.name.clone(),
            author_rank: ctx.requester.rank,
            author_level: ctx.requester.level,
            created: now_ms(),
            active: true,
            runs: 0,
            last_run: 0,
            last_result: String::new(),
            origin_channel: ctx.channel_id,
            scope: crate::memory::TaskScope::Origin,
        });
        id
    });
    Ok(format!(
        "{result} New tickets run standing task #{task_id}."
    ))
}

pub async fn edit_ticket_panel(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    ensure_settings(ctx)?;
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_CHANNELS)?;
    let system = tickets::system(&ctx.core, guild_id);
    if !system.is_set_up() {
        return Err("there's no ticket system yet — set one up first".to_owned());
    }
    let target = match arg(args, "panel_channel") {
        Some(q) => resolve_text_channel(ctx, Some(q))?,
        None => system.panel_channel_id,
    };
    let color = match arg(args, "color") {
        Some(c) => Some(
            u32::from_str_radix(c.trim().trim_start_matches('#'), 16)
                .map_err(|_| format!("color should be hex like #5865F2, not {c}"))?,
        ),
        None => None,
    };
    // "Bug report | 🐛 | Something's broken; Billing | 💳 | Payments"
    let options = arg(args, "options").map(|text| {
        text.split([';', '\n'])
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .map(|o| {
                let parts: Vec<&str> = o.split('|').map(str::trim).collect();
                crate::memory::TicketOption {
                    label: parts
                        .first()
                        .copied()
                        .unwrap_or("Support")
                        .chars()
                        .take(100)
                        .collect(),
                    emoji: parts.get(1).copied().unwrap_or("").to_owned(),
                    description: parts.get(2).copied().unwrap_or("").to_owned(),
                    instruction: parts.get(3).copied().unwrap_or("").to_owned(),
                }
            })
            .collect::<Vec<_>>()
    });
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let p = &mut k.tickets.panel;
        if let Some(v) = arg(args, "title") {
            p.title = v;
        }
        if let Some(v) = arg(args, "message") {
            p.message = v;
        }
        if let Some(v) = color {
            p.color = v;
        }
        if let Some(v) = arg(args, "button_label") {
            p.button_label = v;
        }
        if let Some(v) = arg(args, "button_emoji") {
            p.button_emoji = v;
        }
        if let Some(v) = arg(args, "button_color") {
            p.button_style = v;
        }
        if let Some(v) = arg(args, "welcome") {
            p.welcome = v;
        }
        if let Some(v) = options.clone() {
            p.options = v;
        }
    });
    tickets::render_panel(&ctx.core, guild_id, target).await?;
    let panel = tickets::system(&ctx.core, guild_id).panel;
    Ok(format!(
        "panel updated in <#{target}>: \"{}\" with {}",
        panel.title,
        if panel.options.is_empty() {
            format!("a \"{}\" button", panel.button_label)
        } else {
            format!(
                "a dropdown of {}",
                panel
                    .options
                    .iter()
                    .map(|o| o.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    ))
}

pub async fn ticket_settings(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    ensure_settings(ctx)?;
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_CHANNELS)?;
    if !tickets::system(&ctx.core, guild_id).is_set_up() {
        return Err("there's no ticket system yet — set one up first".to_owned());
    }
    let add = roles_list(ctx, guild_id, arg(args, "add_staff_roles"))?;
    let remove = roles_list(ctx, guild_id, arg(args, "remove_staff_roles"))?;
    if !add.is_empty() || !remove.is_empty() {
        tickets::set_staff(&ctx.core, guild_id, &add, &remove).await?;
    }
    let log = match arg(args, "log_channel") {
        Some(q) if matches!(q.to_lowercase().as_str(), "none" | "off") => Some(0),
        Some(q) => Some(resolve_text_channel(ctx, Some(q))?),
        None => None,
    };
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let t = &mut k.tickets;
        if let Some(v) = log {
            t.log_channel_id = v;
        }
        if let Some(v) = flag(args, "dm_transcripts") {
            t.dm_transcripts = v;
        }
        if let Some(v) = flag(args, "auto_reply") {
            t.auto_reply = v;
        }
        if let Some(v) = number(args, "max_open_per_user") {
            t.max_open = v.clamp(1, 10) as u32;
        }
    });
    let t = tickets::system(&ctx.core, guild_id);
    Ok(format!(
        "ticket settings: staff {} · transcripts to {} · DM transcripts {} · I answer in tickets: {} · max open per person {}",
        if t.staff_roles.is_empty() {
            "none".to_owned()
        } else {
            t.staff_roles
                .iter()
                .map(|r| format!("<@&{r}>"))
                .collect::<Vec<_>>()
                .join(", ")
        },
        if t.log_channel_id == 0 {
            "nowhere".to_owned()
        } else {
            format!("<#{}>", t.log_channel_id)
        },
        if t.dm_transcripts { "on" } else { "off" },
        if t.auto_reply { "yes" } else { "no" },
        t.max_open.max(1),
    ))
}

pub async fn ticket_access(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let ticket = ticket_for(ctx, args)?;
    if !staff_check(ctx, ticket.channel_id) && ctx.requester.user_id != ticket.user_id {
        return Err("only staff or the ticket's opener can change who's in it".to_owned());
    }
    let grant = !flag(args, "remove").unwrap_or(false);
    let who = arg(args, "who")
        .or_else(|| arg(args, "user"))
        .ok_or("say who (a member or a role)")?;
    let (id, member, name) = if let Ok((id, name, _)) = find_role(ctx, guild_id, &who) {
        (id, false, format!("@{name}"))
    } else {
        let m = resolve_member(ctx, &who).await?;
        (m.user_id, true, m.display_name().to_owned())
    };
    tickets::set_access(&ctx.core, ticket.channel_id, id, member, grant).await?;
    Ok(format!(
        "{} {name} {} ticket #{}",
        if grant { "gave" } else { "removed" },
        if grant { "access to" } else { "from" },
        ticket.number
    ))
}

pub async fn close_ticket(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let ticket = ticket_for(ctx, args)?;
    tickets::close_ticket(
        &ctx.core,
        guild_id,
        ticket.channel_id,
        ctx.requester.user_id,
        &ctx.requester.name,
        staff_check(ctx, ticket.channel_id),
        &arg(args, "reason").unwrap_or_else(|| "resolved".to_owned()),
        flag(args, "delete").unwrap_or(false),
    )
    .await
}

pub async fn reopen_ticket(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let ticket = ticket_for(ctx, args)?;
    tickets::reopen_ticket(
        &ctx.core,
        guild_id,
        ticket.channel_id,
        ctx.requester.user_id,
        &ctx.requester.name,
        staff_check(ctx, ticket.channel_id),
    )
    .await
}

pub async fn claim_ticket(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let ticket = ticket_for(ctx, args)?;
    if !staff_check(ctx, ticket.channel_id) {
        return Err("only staff can claim tickets".to_owned());
    }
    tickets::claim(
        &ctx.core,
        guild_id,
        ticket.channel_id,
        ctx.requester.user_id,
        &ctx.requester.name,
    )
    .await
}

pub async fn rename_ticket(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let ticket = ticket_for(ctx, args)?;
    if !staff_check(ctx, ticket.channel_id) {
        return Err("only staff can rename tickets".to_owned());
    }
    let name = required(args, "name")?;
    tickets::rename(&ctx.core, ticket.channel_id, &name).await?;
    Ok(format!("renamed ticket #{} to {name}", ticket.number))
}

pub async fn open_ticket(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let member = match arg(args, "user") {
        Some(user) => resolve_member(ctx, &user).await?,
        None => resolve_member(ctx, &ctx.requester.user_id.to_string()).await?,
    };
    if member.user_id != ctx.requester.user_id && !staff_check(ctx, ctx.channel_id) {
        return Err("denied: only staff can open tickets for other people".to_owned());
    }
    let system = tickets::system(&ctx.core, guild_id);
    let kind = arg(args, "type").and_then(|t| {
        system
            .panel
            .options
            .iter()
            .find(|o| o.label.to_lowercase().contains(&t.to_lowercase()))
            .cloned()
    });
    let channel = tickets::open_ticket(
        &ctx.core,
        guild_id,
        member.user_id,
        member.display_name(),
        kind.as_ref(),
    )
    .await?;
    Ok(format!(
        "opened a ticket for {}: <#{channel}>",
        member.display_name()
    ))
}

pub async fn list_tickets(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let system = tickets::system(&ctx.core, guild_id);
    if !system.is_set_up() {
        return Ok("there's no ticket system here — set one up with setup_tickets".to_owned());
    }
    let all = flag(args, "include_closed").unwrap_or(false);
    let user = match arg(args, "user") {
        Some(u) => Some(resolve_member(ctx, &u).await?.user_id),
        None => None,
    };
    let lines: Vec<String> = system
        .tickets
        .iter()
        .rev()
        .filter(|t| all || t.closed.is_none())
        .filter(|t| user.is_none_or(|u| t.user_id == u))
        .take(30)
        .map(|t| {
            format!(
                "- #{} {}{} — {} (<#{}>), opened <t:{}:R>{}{}",
                t.number,
                if t.closed.is_some() { "closed" } else { "open" },
                if t.kind.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", t.kind)
                },
                t.user_name,
                t.channel_id,
                t.opened / 1000,
                t.claimed_by
                    .map(|c| format!(", claimed by <@{c}>"))
                    .unwrap_or_default(),
                if t.summary.is_empty() {
                    String::new()
                } else {
                    format!("\n  {}", crate::llm::truncate(&t.summary, 200))
                }
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        "no matching tickets".to_owned()
    } else {
        format!(
            "Tickets (panel in <#{}>):\n{}",
            system.panel_channel_id,
            lines.join("\n")
        )
    })
}

pub async fn ticket_transcript(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let ticket = ticket_for(ctx, args)?;
    if !staff_check(ctx, ticket.channel_id) && ctx.requester.user_id != ticket.user_id {
        return Err("only staff or the ticket's opener can read its transcript".to_owned());
    }
    let text = tickets::ticket_text(&ctx.core, guild_id, &ticket);
    if text.trim().is_empty() {
        return Ok(format!(
            "ticket #{} has no saved transcript yet",
            ticket.number
        ));
    }
    Ok(format!(
        "{}{}",
        if ticket.summary.is_empty() {
            String::new()
        } else {
            format!("Summary: {}\n\n", ticket.summary)
        },
        crate::llm::truncate(&text, 6000)
    ))
}

pub async fn search_tickets(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    if !staff_check(ctx, ctx.channel_id) {
        return Err("only staff can search tickets".to_owned());
    }
    let user = match arg(args, "user") {
        Some(u) => Some(resolve_member(ctx, &u).await?.user_id),
        None => None,
    };
    let query = arg(args, "query").unwrap_or_default();
    let hits = tickets::search(&ctx.core, guild_id, &query, user);
    if hits.is_empty() {
        return Ok(format!("no tickets mention \"{query}\""));
    }
    let lines: Vec<String> = hits
        .iter()
        .take(15)
        .map(|(t, snippet)| {
            format!(
                "- #{} ({}, {}): {}",
                t.number,
                t.user_name,
                if t.closed.is_some() { "closed" } else { "open" },
                crate::llm::truncate(snippet, 300)
            )
        })
        .collect();
    Ok(format!(
        "{} ticket(s) match{}. Use ticket_transcript with a number to read one in full.\n{}",
        hits.len(),
        if query.is_empty() {
            String::new()
        } else {
            format!(" \"{query}\"")
        },
        lines.join("\n")
    ))
}

// --- role menus & giveaways ---------------------------------------------------------------

pub async fn create_role_menu(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    use crate::runtime::community::{MenuRole, role_menu_body};
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_ROLES)?;
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    // "Gamer | 🎮 | plays games; Artist | 🎨"
    let mut roles = Vec::new();
    for item in required(args, "roles")?
        .split([';', '\n'])
        .map(str::trim)
        .filter(|i| !i.is_empty())
    {
        let parts: Vec<&str> = item.split('|').map(str::trim).collect();
        // An emoji written next to the role name ("Gamer 🎮") is the emoji.
        let inline_emoji: String = parts[0]
            .chars()
            .filter(|c| !c.is_alphanumeric() && !c.is_whitespace() && !"-_'&".contains(*c))
            .collect();
        let (role_id, name, position) = find_role(ctx, guild_id, parts[0])?;
        if !ctx.acting_as_agent()
            && ctx.requester.kind != PrincipalKind::Operator
            && position.max(0) as u32 >= ctx.requester.rank
        {
            return Err(format!(
                "denied: @{name} is at or above {}'s highest role",
                ctx.requester.name
            ));
        }
        roles.push(MenuRole {
            role_id,
            label: name,
            emoji: parts
                .get(1)
                .filter(|e| !e.is_empty())
                .map(|e| (*e).to_owned())
                .unwrap_or(inline_emoji),
            description: parts.get(2).copied().unwrap_or("").to_owned(),
        });
    }
    if roles.is_empty() {
        return Err("list the roles, like \"Gamer | 🎮; Artist | 🎨\"".to_owned());
    }
    let dropdown = arg(args, "style")
        .is_some_and(|s| s.to_lowercase().contains("drop") || s.to_lowercase().contains("menu"))
        || roles.len() > 25;
    let body = role_menu_body(
        &arg(args, "title").unwrap_or_else(|| "Pick your roles".to_owned()),
        &arg(args, "message").unwrap_or_else(|| {
            if dropdown {
                "Choose from the menu below.".to_owned()
            } else {
                "Press a button to get or remove a role.".to_owned()
            }
        }),
        &roles,
        dropdown,
        0x5865F2,
    );
    ctx.core
        .http
        .raw(
            "POST",
            &format!("/channels/{channel_id}/messages"),
            Some(body),
            None,
        )
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "posted a role menu in {} with {} ({}) — members pick roles themselves",
        label(ctx, channel_id),
        roles
            .iter()
            .map(|r| format!("@{}", r.label))
            .collect::<Vec<_>>()
            .join(", "),
        if dropdown { "dropdown" } else { "buttons" }
    ))
}

pub async fn start_giveaway(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(
        ctx,
        Permissions::MANAGE_GUILD | Permissions::MANAGE_MESSAGES,
    )
    .or_else(|_| require_guild_perm(ctx, Permissions::MANAGE_MESSAGES))?;
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    let ends = parse_time(&format!(
        "{}",
        arg(args, "ends").unwrap_or_else(|| "in 1 day".to_owned())
    ))?;
    let required_role = match arg(args, "required_role") {
        Some(r) => find_role(ctx, guild_id, &r)?.0,
        None => 0,
    };
    let g = crate::runtime::community::start_giveaway(
        &ctx.core,
        guild_id,
        crate::memory::Giveaway {
            channel_id,
            prize: required(args, "prize")?,
            host_id: ctx.requester.user_id,
            ends: ends.as_unix_millis(),
            winners: number(args, "winners").unwrap_or(1).clamp(1, 20) as u32,
            required_role,
            ..Default::default()
        },
    )
    .await?;
    Ok(format!(
        "giveaway for \"{}\" started in {} — ends <t:{}:R>, {} winner(s) (message {})",
        g.prize,
        label(ctx, channel_id),
        g.ends / 1000,
        g.winners,
        g.message_id
    ))
}

fn giveaway_message(
    ctx: &ToolCtx,
    guild_id: u64,
    args: &Value,
    ended: bool,
) -> Result<u64, String> {
    let query = arg(args, "giveaway").unwrap_or_default().to_lowercase();
    ctx.core
        .hive
        .knowledge
        .guild(guild_id, |k| {
            k.giveaways
                .iter()
                .rev()
                .filter(|g| g.ended == ended)
                .find(|g| {
                    query.is_empty()
                        || g.prize.to_lowercase().contains(&query)
                        || g.message_id.to_string() == query
                })
                .map(|g| g.message_id)
        })
        .ok_or_else(|| {
            format!(
                "no {} giveaway matching \"{query}\"",
                if ended { "ended" } else { "running" }
            )
        })
}

pub async fn end_giveaway(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = require_guild_perm(ctx, Permissions::MANAGE_MESSAGES)?;
    let reroll = flag(args, "reroll").unwrap_or(false);
    let message = giveaway_message(ctx, guild_id, args, reroll)?;
    let winners = crate::runtime::community::finish(&ctx.core, guild_id, message, reroll).await?;
    Ok(if winners.is_empty() {
        "ended — nobody entered".to_owned()
    } else {
        format!(
            "{} — winner(s): {}",
            if reroll { "rerolled" } else { "ended" },
            winners
                .iter()
                .map(|w| format!("<@{w}>"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

pub async fn list_giveaways(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let lines: Vec<String> = ctx.core.hive.knowledge.guild(guild_id, |k| {
        k.giveaways
            .iter()
            .rev()
            .take(15)
            .map(|g| {
                format!(
                    "- {} in <#{}>: {} entries, {}",
                    g.prize,
                    g.channel_id,
                    g.entrants.len(),
                    if g.ended {
                        format!(
                            "ended (winners: {})",
                            g.picked
                                .iter()
                                .map(|w| format!("<@{w}>"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    } else {
                        format!("ends <t:{}:R>", g.ends / 1000)
                    }
                )
            })
            .collect()
    });
    Ok(if lines.is_empty() {
        "no giveaways yet".to_owned()
    } else {
        lines.join("\n")
    })
}

// --- settings, levels & stats ---------------------------------------------------------------

/// Settings changes: the owner, or whoever the owner allowed (admins
/// when the owner hasn't restricted it).
fn ensure_settings(ctx: &ToolCtx) -> Result<u64, String> {
    let guild_id = ctx.require_guild()?;
    if ctx.acting_as_agent()
        || crate::runtime::levels::can_manage(&ctx.core, guild_id, &ctx.requester)
    {
        Ok(guild_id)
    } else {
        Err(format!(
            "denied: {} isn't allowed to change these settings (the owner decides who can)",
            ctx.requester.name
        ))
    }
}

pub async fn get_rank(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let user_id = match arg(args, "user") {
        Some(u) => resolve_member(ctx, &u).await?.user_id,
        None => ctx.requester.user_id,
    };
    let (png, summary) = crate::runtime::levels::rank_card(&ctx.core, guild_id, user_id).await?;
    let name = ctx.media.lock().expect("media poisoned").add(png, true);
    Ok(format!(
        "{summary}. Attached the rank card ({name}) to your reply."
    ))
}

pub async fn leaderboard(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    Ok(crate::runtime::levels::leaderboard(
        &ctx.core,
        guild_id,
        number(args, "count").unwrap_or(10) as usize,
    ))
}

pub async fn server_stats(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    Ok(crate::runtime::levels::stats_report(
        &ctx.core,
        guild_id,
        number(args, "days").unwrap_or(7) as usize,
    ))
}

pub async fn leveling_settings(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ensure_settings(ctx)?;
    let announce_channel = match arg(args, "announce_channel") {
        Some(q) => Some(resolve_text_channel(ctx, Some(q))?),
        None => None,
    };
    let mut add_rewards = Vec::new();
    for item in arg(args, "add_rewards")
        .unwrap_or_default()
        .split([';', ',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (level, role) = item
            .split_once([':', '='])
            .ok_or_else(|| format!("rewards look like \"10: Regular\", not \"{item}\""))?;
        let level: u32 = level
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .map_err(|_| format!("bad level in \"{item}\""))?;
        add_rewards.push(crate::memory::LevelReward {
            level,
            role_id: find_role(ctx, guild_id, role.trim())?.0,
        });
    }
    let remove_levels: Vec<u32> = arg(args, "remove_rewards")
        .unwrap_or_default()
        .split([',', ' '])
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    let ignore = match arg(args, "ignore_channel") {
        Some(q) => Some(resolve_channel(ctx, Some(q))?),
        None => None,
    };
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let l = &mut k.leveling;
        if let Some(v) = flag(args, "enabled") {
            l.enabled = v;
        }
        if let Some(v) = flag(args, "paused") {
            l.paused = v;
        }
        if let Some(v) = number(args, "xp_min") {
            l.xp_min = v.clamp(0, 1000) as u32;
        }
        if let Some(v) = number(args, "xp_max") {
            l.xp_max = v.clamp(0, 1000) as u32;
        }
        if let Some(v) = number(args, "cooldown_seconds") {
            l.cooldown_secs = v.clamp(0, 3600) as u32;
        }
        if let Some(v) = number(args, "voice_xp_per_minute") {
            l.voice_xp_per_minute = v.clamp(0, 100) as u32;
        }
        if let Some(v) = arg(args, "announce") {
            l.announce = v.to_lowercase();
        }
        if let Some(v) = announce_channel {
            l.announce_channel = v;
            l.announce = "channel".to_owned();
        }
        if let Some(v) = arg(args, "message") {
            l.message = v;
        }
        if let Some(v) = flag(args, "stack_rewards") {
            l.stack_rewards = v;
        }
        l.rewards.retain(|r| !remove_levels.contains(&r.level));
        for reward in &add_rewards {
            l.rewards.retain(|r| r.level != reward.level);
            l.rewards.push(*reward);
        }
        l.rewards.sort_by_key(|r| r.level);
        if let Some(channel) = ignore {
            if !l.ignored_channels.contains(&channel) {
                l.ignored_channels.push(channel);
            }
        }
    });
    let l = ctx
        .core
        .hive
        .knowledge
        .guild(guild_id, |k| k.leveling.clone());
    Ok(format!(
        "leveling {}{}: {}–{} XP per message every {}s, {} voice XP/min, level-ups {}, rewards: {}",
        if l.enabled { "on" } else { "off" },
        if l.paused { " (paused)" } else { "" },
        l.xp_min,
        l.xp_max,
        l.cooldown_secs,
        l.voice_xp_per_minute,
        match l.announce.as_str() {
            "channel" => format!("in <#{}>", l.announce_channel),
            "dm" => "by DM".to_owned(),
            "off" => "off".to_owned(),
            _ => "in the same channel".to_owned(),
        },
        if l.rewards.is_empty() {
            "none".to_owned()
        } else {
            l.rewards
                .iter()
                .map(|r| format!("{} → <@&{}>", r.level, r.role_id))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ))
}

pub async fn set_xp(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ensure_settings(ctx)?;
    let member = resolve_member(ctx, &required(args, "user")?).await?;
    let level = number(args, "level");
    let set = number(args, "xp");
    let add = number(args, "add_xp");
    let xp = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let entry = k.leveling.members.entry(member.user_id).or_default();
        entry.name = member.display_name().to_owned();
        if let Some(level) = level {
            entry.xp = (0..level.clamp(0, 500) as u32)
                .map(crate::memory::Leveling::xp_for_next)
                .sum();
        }
        if let Some(v) = set {
            entry.xp = v.max(0) as u64;
        }
        if let Some(v) = add {
            entry.xp = (entry.xp as i64 + v).max(0) as u64;
        }
        entry.xp
    });
    let (level, _, _) = crate::memory::Leveling::progress(xp);
    Ok(format!(
        "{} now has {xp} XP (level {level})",
        member.display_name()
    ))
}

/// Apply design_rank_card's styling arguments to one card. Everything is
/// parsed before anything is written, and it's pure (no Discord), so the
/// server's card and a person's own go through the same tested path.
fn apply_card_design(c: &mut crate::memory::RankCard, args: &Value) -> Result<(), String> {
    let hex = |key: &str| -> Result<Option<String>, String> {
        match arg(args, key) {
            Some(v) if v.trim().is_empty() || v.trim() == "default" => Ok(Some(String::new())),
            Some(v) => {
                let v = v.trim().to_owned();
                let ok = v.starts_with('#')
                    && (v.len() == 7)
                    && u32::from_str_radix(&v[1..], 16).is_ok();
                if ok {
                    Ok(Some(v))
                } else {
                    Err(format!("{key} should be a hex color like #5865f2, not {v}"))
                }
            }
            None => Ok(None),
        }
    };
    let mut colors = Vec::new();
    for key in [
        "background",
        "background2",
        "accent",
        "text",
        "muted",
        "name_color",
        "rank_color",
        "level_color",
        "xp_color",
        "bar_color",
        "bar_track",
        "ring_color",
    ] {
        colors.push((key, hex(key)?));
    }
    let pick = |key: &str, allowed: &[&str]| -> Result<Option<String>, String> {
        match arg(args, key) {
            Some(v) => {
                let v = v.trim().to_lowercase().replace([' ', '-'], "_");
                if allowed.is_empty() || allowed.contains(&v.as_str()) {
                    Ok(Some(v))
                } else {
                    Err(format!("{key} must be one of {}", allowed.join(", ")))
                }
            }
            None => Ok(None),
        }
    };
    let layout = pick("layout", &crate::tools::rankcard::LAYOUTS)?;
    let style = pick("style", &crate::tools::rankcard::STYLES)?;
    let gradient = pick("gradient", &["horizontal", "vertical", "diagonal"])?;
    let avatar_shape = pick("avatar_shape", &["circle", "rounded", "square"])?;
    let bar_shape = pick("bar_shape", &["round", "square"])?;
    let total_position = pick(
        "total_position",
        &["below_bar", "above_bar", "with_xp", "top"],
    )?;
    let weight = pick("font_weight", &["light", "regular", "bold"])?;
    let hide: Vec<String> = arg(args, "hide")
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    let show: Vec<String> = arg(args, "show")
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    for (key, value) in &colors {
        let Some(value) = value.clone() else { continue };
        let slot = match *key {
            "background" => &mut c.background,
            "background2" => &mut c.background2,
            "accent" => &mut c.accent,
            "text" => &mut c.text,
            "muted" => &mut c.muted,
            "name_color" => &mut c.name_color,
            "rank_color" => &mut c.rank_color,
            "level_color" => &mut c.level_color,
            "xp_color" => &mut c.xp_color,
            "bar_color" => &mut c.bar_color,
            "bar_track" => &mut c.bar_track,
            _ => &mut c.ring_color,
        };
        if !value.is_empty() || !matches!(*key, "background" | "accent" | "text" | "muted") {
            *slot = value;
        }
    }
    if let Some(v) = layout.clone() {
        c.layout = v;
    }
    if let Some(v) = style.clone() {
        c.style = v;
    }
    if let Some(v) = gradient.clone() {
        c.gradient = v;
    }
    if let Some(v) = avatar_shape.clone() {
        c.avatar_shape = v;
    }
    if let Some(v) = bar_shape.clone() {
        c.bar_shape = v;
    }
    if let Some(v) = total_position.clone() {
        c.total_position = v;
    }
    if let Some(v) = weight.clone() {
        c.font_weight = v;
    }
    if let Some(v) = arg(args, "font") {
        c.font = v;
    }
    if let Some(v) = arg(args, "background_image") {
        c.background_url = v;
    }
    if let Some(v) = number(args, "image_opacity") {
        c.image_opacity = v.clamp(1, 100) as u8;
    }
    if let Some(v) = number(args, "blur") {
        c.blur = v.clamp(0, 40) as u8;
    }
    if let Some(v) = number(args, "image_brightness") {
        c.image_brightness = v.clamp(-100, 100) as i32;
    }
    if let Some(v) = number(args, "darken") {
        c.overlay = v.clamp(0, 100) as u8;
    }
    if let Some(v) = number(args, "panel_opacity") {
        c.panel_opacity = v.clamp(0, 100) as u8;
    }
    if let Some(v) = number(args, "bar_height") {
        c.bar_height = v.clamp(4, 80) as u32;
    }
    if let Some(v) = number(args, "corner_radius") {
        c.corner_radius = v.clamp(0, 140) as u32;
    }
    if let Some(v) = args.get("text_scale").and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }) {
        c.text_scale = (v as f32).clamp(0.5, 2.0);
    }
    if let Some(v) = flag(args, "avatar_ring") {
        c.avatar_ring = v;
    }
    if let Some(v) = arg(args, "rank_label") {
        c.rank_label = v;
    }
    if let Some(v) = arg(args, "level_label") {
        c.level_label = v;
    }
    for (list, value) in [(&hide, false), (&show, true)] {
        for part in list.iter() {
            match part.as_str() {
                "avatar" => c.show_avatar = value,
                "rank" => c.show_rank = value,
                "level" => c.show_level = value,
                "xp" => c.show_xp = value,
                "total" | "total_xp" | "xp_total" => c.show_total = value,
                "bar" | "progress" | "progress_bar" => c.show_bar = value,
                _ => {}
            }
        }
    }
    Ok(())
}

/// Where a design_rank_card call lands: the server's card, or (by default)
/// the requester's own copy-on-write override, which starts from whatever
/// card they'd see now. A personal reset drops their override — back to
/// the server's design. Pure, so the routing is testable without Discord.
fn apply_design(
    l: &mut crate::memory::Leveling,
    user_id: u64,
    server_scope: bool,
    reset: bool,
    args: &Value,
) -> Result<(), String> {
    if server_scope {
        if reset {
            l.card = Default::default();
        }
        apply_card_design(&mut l.card, args)
    } else {
        if reset {
            l.card_overrides.remove(&user_id);
        }
        let styling = args.as_object().is_some_and(|o| {
            o.keys()
                .any(|k| !matches!(k.as_str(), "scope" | "reset" | "look"))
        });
        if styling {
            let mut card = l.card_for(user_id).clone();
            apply_card_design(&mut card, args)?;
            l.card_overrides.insert(user_id, card);
        }
        Ok(())
    }
}

pub async fn design_rank_card(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    // Personal by default: this designs the requester's own card, which
    // needs no settings permission. scope=server changes everyone's and
    // is settings-gated.
    let guild_id = ctx.require_guild()?;
    let server_scope = matches!(
        arg(args, "scope")
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "server" | "server_wide" | "serverwide" | "guild" | "everyone" | "all"
    );
    if server_scope {
        ensure_settings(ctx)?;
    }
    if let Some(url) = arg(args, "background_image").filter(|u| !u.trim().is_empty()) {
        if !super::web::is_live_image(url.trim()).await {
            return Err(format!(
                "{url} isn't a working image link — find a real one with image_search first (never make up links)"
            ));
        }
    }
    let user_id = ctx.requester.user_id;
    let reset = flag(args, "reset").unwrap_or(false);
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        apply_design(&mut k.leveling, user_id, server_scope, reset, args)
    })?;
    let (png, _) = crate::runtime::levels::rank_card(&ctx.core, guild_id, user_id).await?;
    // Look at the result when asked to (or to check readability).
    let review = if flag(args, "look").unwrap_or(false) {
        match crate::tools::rankcard::look(
            &ctx.core,
            &png,
            "This is a Discord rank card I just designed. Describe how it looks and anything hard to read, clashing, cramped, or off-center.",
        )
        .await
        {
            Ok(text) => format!("\nWhat it looks like: {text}"),
            Err(error) => format!("\n(couldn't look at it: {error})"),
        }
    } else {
        String::new()
    };
    let name = ctx
        .media
        .lock()
        .expect("media poisoned")
        .add_replacing("rank card", png);
    let c = ctx
        .core
        .hive
        .knowledge
        .guild(guild_id, |k| k.leveling.card.clone());
    let color_keys = [
        "background",
        "background2",
        "accent",
        "text",
        "muted",
        "name_color",
        "rank_color",
        "level_color",
        "xp_color",
        "bar_color",
        "bar_track",
        "ring_color",
    ];
    let colors_changed = color_keys
        .iter()
        .filter(|key| arg(args, key).is_some_and(|v| !v.trim().is_empty()))
        .count();
    let tip = if c.background_url.is_empty()
        && colors_changed >= 3
        && arg(args, "background_image").is_none()
    {
        " No background image yet — for a themed card, find a fitting one with image_search and call design_rank_card again with background_image (it's what makes a theme look finished)."
    } else {
        ""
    };
    Ok(format!(
        "rank card updated — layout {}, style {}, font {} ({}), background {}{}{}, accent {}, text {}. Preview ({name}) attached to your reply.{review}{tip}",
        c.layout,
        c.style,
        c.font,
        c.font_weight,
        c.background,
        if c.background2.is_empty() {
            String::new()
        } else {
            format!("→{} gradient", c.background2)
        },
        if c.background_url.is_empty() {
            String::new()
        } else {
            format!(
                ", image (opacity {}%, blur {}, brightness {})",
                c.image_opacity, c.blur, c.image_brightness
            )
        },
        c.accent,
        c.text
    ))
}

pub async fn stats_settings(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ensure_settings(ctx)?;
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let s = &mut k.stats;
        if let Some(v) = flag(args, "enabled") {
            s.enabled = v;
        }
        if let Some(v) = flag(args, "paused") {
            s.paused = v;
        }
        if let Some(v) = flag(args, "track_members") {
            s.track_members = v;
        }
        if let Some(v) = flag(args, "track_messages") {
            s.track_messages = v;
        }
        if let Some(v) = flag(args, "track_voice") {
            s.track_voice = v;
        }
        if let Some(v) = flag(args, "track_joins") {
            s.track_joins = v;
        }
        if flag(args, "clear_data").unwrap_or(false) {
            s.days.clear();
        }
    });
    let s = ctx.core.hive.knowledge.guild(guild_id, |k| k.stats.clone());
    Ok(format!(
        "statistics {}{}: members {}, messages {}, voice {}, joins {} · {} day(s) kept",
        if s.enabled { "on" } else { "off" },
        if s.paused { " (paused)" } else { "" },
        s.track_members,
        s.track_messages,
        s.track_voice,
        s.track_joins,
        s.days.len()
    ))
}

pub async fn settings_access(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let owner =
        ctx.requester.kind == PrincipalKind::Operator || ctx.requester.level >= AccessLevel::Owner;
    if !owner
        && (arg(args, "roles").is_some()
            || arg(args, "users").is_some()
            || flag(args, "reset").unwrap_or(false))
    {
        return Err("denied: only the server owner decides who can change settings".to_owned());
    }
    if flag(args, "reset").unwrap_or(false) {
        ctx.core
            .hive
            .knowledge
            .guild_mut(guild_id, |k| k.settings_access = Default::default());
    }
    if let Some(list) = arg(args, "roles") {
        let roles = roles_list(ctx, guild_id, Some(list))?;
        ctx.core
            .hive
            .knowledge
            .guild_mut(guild_id, |k| k.settings_access.roles = roles.clone());
    }
    if let Some(list) = arg(args, "users") {
        let mut users = Vec::new();
        for who in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            users.push(resolve_member(ctx, who).await?.user_id);
        }
        ctx.core
            .hive
            .knowledge
            .guild_mut(guild_id, |k| k.settings_access.users = users.clone());
    }
    let a = ctx
        .core
        .hive
        .knowledge
        .guild(guild_id, |k| k.settings_access.clone());
    Ok(if a.is_restricted() {
        format!(
            "settings can be changed by the owner, {}{}",
            a.roles
                .iter()
                .map(|r| format!("<@&{r}>"))
                .collect::<Vec<_>>()
                .join(", "),
            if a.users.is_empty() {
                String::new()
            } else {
                format!(
                    ", {}",
                    a.users
                        .iter()
                        .map(|u| format!("<@{u}>"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        )
    } else {
        "settings can be changed by admins and the owner (not restricted)".to_owned()
    })
}

pub async fn show_settings(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let section = arg(args, "section")
        .unwrap_or_else(|| "overview".to_owned())
        .to_lowercase();
    let section = crate::runtime::setup::SECTIONS
        .iter()
        .find(|(id, label, _)| section.contains(id) || label.to_lowercase().contains(&section))
        .map(|(id, _, _)| *id)
        .unwrap_or("overview");
    let can_edit = crate::runtime::levels::can_manage(&ctx.core, guild_id, &ctx.requester);
    let owner =
        ctx.requester.level >= AccessLevel::Owner || ctx.requester.kind == PrincipalKind::Operator;
    let body = crate::runtime::setup::render(&ctx.core, guild_id, section, can_edit, owner);
    ctx.core
        .http
        .raw(
            "POST",
            &format!("/channels/{}/messages", ctx.channel_id),
            Some(body),
            None,
        )
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "posted the {section} settings panel here{} — don't repeat its contents, just point to it",
        if can_edit {
            " with buttons to change them"
        } else {
            " (view only for them)"
        }
    ))
}

// --- translation ---------------------------------------------------------------------------

pub async fn start_translating(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    use crate::runtime::translate::{TranslationSession, guess_language, start};
    let guild_id = ctx.require_guild()?;
    let mut users = Vec::new();
    for who in arg(args, "users")
        .unwrap_or_default()
        .split([',', '&'])
        .map(str::trim)
        .filter(|s| !s.is_empty() && !matches!(s.to_lowercase().as_str(), "everyone" | "all"))
    {
        users.push(resolve_member(ctx, who).await?);
    }
    // Known languages: given ("uvx: English, Pablo: Spanish"), else guessed
    // from what they wrote recently.
    let mut languages = std::collections::BTreeMap::new();
    for pair in arg(args, "languages").unwrap_or_default().split([',', ';']) {
        if let Some((who, language)) = pair.split_once(':') {
            if let Ok(member) = resolve_member(ctx, who.trim()).await {
                languages.insert(member.user_id, language.trim().to_owned());
            }
        }
    }
    let ids: Vec<u64> = users.iter().map(|m| m.user_id).collect();
    let recent: Vec<(u64, String)> = ctx.core.hive.knowledge.channel(ctx.channel_id, |a| {
        a.messages
            .iter()
            .rev()
            .take(30)
            .map(|m| (m.author_id, m.content.clone()))
            .collect()
    });
    for (author, text) in recent {
        if (ids.is_empty() || ids.contains(&author)) && !languages.contains_key(&author) {
            if let Some(language) = guess_language(&text) {
                languages.insert(author, language.to_owned());
            }
        }
    }
    let target = arg(args, "language").unwrap_or_default();
    let session = start(
        &ctx.core,
        guild_id,
        TranslationSession {
            channel_id: ctx.channel_id,
            users: ids,
            target: target.clone(),
            languages,
            started_by: ctx.requester.user_id,
            idle_minutes: number(args, "stop_after_quiet_minutes")
                .unwrap_or(15)
                .clamp(1, 240) as u32,
            ..Default::default()
        },
    );
    let who = if users.is_empty() {
        "everyone here".to_owned()
    } else {
        users
            .iter()
            .map(|m| m.display_name().to_owned())
            .collect::<Vec<_>>()
            .join(" and ")
    };
    let known = session
        .languages
        .iter()
        .map(|(u, l)| format!("<@{u}>: {l}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "translating {who} {} from now on, until someone asks you to stop or it's quiet for {} minutes{}. \
         Don't translate in your own replies — every new message gets translated automatically.",
        if target.is_empty() {
            "into each other's languages".to_owned()
        } else {
            format!("into {target}")
        },
        session.idle_minutes,
        if known.is_empty() {
            String::new()
        } else {
            format!(" (languages so far: {known})")
        }
    ))
}

pub async fn stop_translating(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    if crate::runtime::translate::stop(&ctx.core, guild_id, ctx.channel_id) {
        Ok("stopped translating here".to_owned())
    } else {
        Ok("I wasn't translating here".to_owned())
    }
}

// --- the bot's own profile ---------------------------------------------------------------------

pub async fn set_my_avatar(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    if !(ctx.requester.kind == PrincipalKind::Operator || ctx.requester.level >= AccessLevel::Admin)
    {
        return Err("denied: only admins can change my profile picture".to_owned());
    }
    let source = required(args, "image")?;
    // An image made in this conversation, else a URL / image in the chat.
    let made = ctx
        .media
        .lock()
        .expect("media poisoned")
        .get(&source)
        .cloned();
    let bytes = match made {
        Some(bytes) => bytes,
        None => {
            let url = if source.starts_with("http") {
                source.clone()
            } else {
                ctx.inputs
                    .iter()
                    .find(|i| i.name.eq_ignore_ascii_case(&source))
                    .map(|i| i.url.clone())
                    .ok_or_else(|| {
                        format!("give an image URL or the name of an image here, not \"{source}\"")
                    })?
            };
            super::web::fetch_bytes(&url, false, 10_000_000).await?
        }
    };
    // Square, 512px, PNG.
    let image =
        image::load_from_memory(&bytes).map_err(|e| format!("not an image I can read: {e}"))?;
    let side = image.width().min(image.height());
    let square = image.crop_imm(
        (image.width() - side) / 2,
        (image.height() - side) / 2,
        side,
        side,
    );
    let square = square.resize_exact(512, 512, image::imageops::FilterType::Lanczos3);
    let mut png = Vec::new();
    square
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    ctx.core
        .http
        .raw(
            "PATCH",
            "/users/@me",
            Some(json!({ "avatar": format!("data:image/png;base64,{}", crate::runtime::brain::base64(&png)) })),
            None,
        )
        .await
        .map_err(|e| {
            let text = discord_error(e);
            format!("{text} (Discord limits how often a bot can change its avatar)")
        })?;
    let name = ctx.media.lock().expect("media poisoned").add(png, true);
    Ok(format!(
        "changed my profile picture (cropped to a square); preview {name} attached"
    ))
}

// --- writing big files ------------------------------------------------------------------------

pub async fn privacy_settings(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ensure_settings(ctx)?;
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let p = &mut k.privacy;
        if let Some(v) = flag(args, "member_search") {
            p.member_search = v;
        }
        if let Some(v) = flag(args, "dms") {
            p.dms = v;
        }
        if let Some(v) = flag(args, "outside_messages") {
            p.outside_messages = v;
        }
        if let Some(v) = flag(args, "audit_search") {
            p.audit_search = v;
        }
    });
    let p = ctx
        .core
        .hive
        .knowledge
        .guild(guild_id, |k| k.privacy.clone());
    let yn = |b: bool| if b { "allowed" } else { "blocked" };
    Ok(format!(
        "from other servers and DMs: member search {}, DMs to members {}, posting in channels {}, audit log search {}",
        yn(p.member_search),
        yn(p.dms),
        yn(p.outside_messages),
        yn(p.audit_search)
    ))
}

// --- anything else -------------------------------------------------------------------------

pub async fn discord_api(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let allowed =
        ctx.requester.kind == PrincipalKind::Operator || ctx.requester.level >= AccessLevel::Admin;
    if !allowed || ctx.autonomous {
        return Err(format!(
            "denied: the raw Discord API is only for server admins (asked by {})",
            ctx.requester.name
        ));
    }
    let method = required(args, "method")?.to_uppercase();
    let path = required(args, "path")?;
    // Only this server: every guild ID must be this one, every channel
    // ID one of its channels.
    let segments: Vec<&str> = path.split(['/', '?']).collect();
    for pair in segments.windows(2) {
        let id = pair[1].parse::<u64>().ok();
        match (pair[0], id) {
            ("guilds", Some(id)) if id != guild_id => {
                return Err("denied: that's another server".to_owned());
            }
            ("channels", Some(id)) if ctx.core.world.channel(guild_id, id).is_none() => {
                return Err(
                    "denied: that channel isn't in this server (or I can't see it)".to_owned(),
                );
            }
            _ => {}
        }
    }
    if method == "DELETE"
        && path
            .trim_end_matches('/')
            .ends_with(&format!("guilds/{guild_id}"))
    {
        return Err("denied: not deleting the server".to_owned());
    }
    let body = match arg(args, "body") {
        Some(text) if !text.trim().is_empty() => Some(
            serde_json::from_str::<Value>(&text)
                .map_err(|e| format!("body isn't valid JSON: {e}"))?,
        ),
        _ => None,
    };
    let result = ctx
        .core
        .http
        .raw(&method, &path, body, Some(ctx.reason(arg(args, "reason"))))
        .await
        .map_err(discord_error)?;
    ctx.core.feed(
        FeedKind::Tool,
        format!("Discord API: {method} {path}"),
        None,
    );
    Ok(match result {
        Value::Null => format!("{method} {path} → done"),
        other => crate::llm::truncate(&other.to_string(), 3000),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_names_parse_loosely() {
        let bits = permission_bits("view channel, send messages, read message history").unwrap();
        assert_eq!(
            bits,
            (Permissions::VIEW_CHANNEL
                | Permissions::SEND_MESSAGES
                | Permissions::READ_MESSAGE_HISTORY)
                .bits()
        );
        assert_eq!(
            permission_bits("SEND").unwrap(),
            Permissions::SEND_MESSAGES.bits()
        );
        assert_eq!(
            permission_bits("manage-permissions").unwrap(),
            Permissions::MANAGE_ROLES.bits()
        );
        assert!(permission_bits("fly").is_err());
        assert_eq!(permission_bits("").unwrap(), 0);
    }

    #[test]
    fn times_parse() {
        let now = now_ms();
        let in_two = parse_time("in 2 hours").unwrap().as_unix_millis();
        assert!((in_two - now - 7_200_000).abs() < 5_000);
        assert!(parse_time("2026-11-02T20:00:00Z").is_ok());
        assert!(parse_time("whenever").is_err());
    }
}
