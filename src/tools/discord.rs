//! Discord tools: reading the server, messaging, reactions, polls,
//! threads and forums, voice, moderation, roles, and channels.

use axis::model::{
    AllowedMentions, ChannelType, Emoji, Message, OverwriteType, PermissionOverwrite, PollCreate,
};
use axis::prelude::{
    ChannelId, EmojiId, GenericId, GuildId, MessageId, Permissions, RoleId, Timestamp, UserId,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use super::{ToolCtx, arg, discord_error, flag, list, number, required};
use crate::memory::{ModAction, Strike, now_ms};
use crate::runtime::authority::{self, PrincipalKind};
use crate::runtime::live::FeedKind;
use crate::runtime::world::{MemberInfo, World, parse_snowflake};

// --- resolution helpers -----------------------------------------------------

pub async fn resolve_member(ctx: &ToolCtx, query: &str) -> Result<MemberInfo, String> {
    let guild_id = ctx.require_guild()?;
    // Hierarchy checks need the bot's own roles; fetch them if missing.
    let me = ctx.core.world.my_id();
    if me != 0 && ctx.core.world.member(guild_id, me).is_none() {
        if let Ok(member) = ctx
            .core
            .http
            .get_member(GuildId::new(guild_id), UserId::new(me))
            .await
        {
            ctx.core.world.upsert_member(guild_id, &member);
        }
    }
    let query = query.trim();
    // A numbered pick from the last find_users list resolves to an exact
    // ID before anything else — the model never transcribes IDs.
    if let Some(picked) = super::users::resolve(ctx.channel_id, query, false)? {
        if let Some(member) = ctx.core.world.member(guild_id, picked[0]) {
            return Ok(member);
        }
        if let Ok(member) = ctx
            .core
            .http
            .get_member(GuildId::new(guild_id), UserId::new(picked[0]))
            .await
        {
            ctx.core.world.upsert_member(guild_id, &member);
            return Ok(ctx.core.world.member(guild_id, picked[0]).unwrap());
        }
        // A bare ID (not a member here): the minimum the tools need.
        let username = format!("user {}", picked[0]);
        return Ok(MemberInfo {
            user_id: picked[0],
            username: username.clone(),
            global_name: None,
            nick: None,
            roles: Vec::new(),
            bot: false,
            joined_at: None,
            timeout_until: None,
            avatar: None,
            guild_avatar: None,
        });
    }
    let lower = query.to_lowercase();
    if matches!(lower.as_str(), "me" | "myself" | "requester" | "i") && ctx.requester.user_id != 0 {
        if let Some(member) = ctx.core.world.member(guild_id, ctx.requester.user_id) {
            return Ok(member);
        }
    }
    if let Some(found) = ctx
        .core
        .world
        .with_guild(guild_id, |g| g.find_member(query).cloned())
        .flatten()
    {
        return Ok(found);
    }
    // Not cached: ask Discord.
    let fetched = if let Some(id) = parse_snowflake(query) {
        ctx.core
            .http
            .get_member(GuildId::new(guild_id), UserId::new(id))
            .await
            .ok()
    } else {
        ctx.core
            .http
            .search_members(GuildId::new(guild_id), query.trim_start_matches('@'))
            .limit(5)
            .await
            .ok()
            .and_then(|members| members.into_iter().next())
    };
    match fetched {
        Some(member) => {
            ctx.core.world.upsert_member(guild_id, &member);
            let id = member.user.as_ref().map(|u| u.id.get()).unwrap_or(0);
            ctx.core
                .world
                .member(guild_id, id)
                .ok_or_else(|| format!("could not find a member matching \"{query}\""))
        }
        None => Err(format!("could not find a member matching \"{query}\"")),
    }
}

/// Like [`resolve_channel`], but "general" means #general rather than the
/// General voice channel when both exist.
pub fn resolve_text_channel(ctx: &ToolCtx, query: Option<String>) -> Result<u64, String> {
    if let (Some(q), Some(guild_id)) = (query.as_deref(), ctx.guild_id) {
        if !matches!(q.to_lowercase().as_str(), "this" | "here" | "current") {
            let text = ctx
                .core
                .world
                .with_guild(guild_id, |g| {
                    g.find_channel_where(q, |c| !c.is_voice())
                        .map(|c| c.id.get())
                })
                .flatten();
            if let Some(id) = text {
                return Ok(id);
            }
        }
    }
    resolve_channel(ctx, query)
}

pub fn resolve_channel(ctx: &ToolCtx, query: Option<String>) -> Result<u64, String> {
    let Some(query) =
        query.filter(|q| !matches!(q.to_lowercase().as_str(), "this" | "here" | "current"))
    else {
        return Ok(ctx.channel_id);
    };
    if let Some(guild_id) = ctx.guild_id {
        if let Some(id) = ctx
            .core
            .world
            .with_guild(guild_id, |g| g.find_channel(&query).map(|c| c.id.get()))
            .flatten()
        {
            return Ok(id);
        }
    }
    let id = parse_snowflake(&query).ok_or_else(|| {
        format!(
            "could not find a channel matching \"{query}\" — use the channel's name (e.g. general)"
        )
    })?;
    // An ID that isn't a channel in this server is usually a guess.
    if let Some(guild_id) = ctx.guild_id {
        let known = ctx
            .core
            .world
            .with_guild(guild_id, |g| g.channels.contains_key(&id))
            .unwrap_or(true);
        if !known {
            return Err(format!(
                "there's no channel with ID {id} in this server that I can see — use the channel's name instead of an ID"
            ));
        }
    }
    Ok(id)
}

fn resolve_message_id(ctx: &ToolCtx, args: &Value, key: &str) -> Result<u64, String> {
    match arg(args, key) {
        Some(text) if !matches!(text.to_lowercase().as_str(), "this" | "that" | "current") => text
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("\"{text}\" is not a message ID")),
        _ => ctx
            .message_id
            .ok_or_else(|| "which message? give a message ID".to_owned()),
    }
}

fn channel_name(ctx: &ToolCtx, channel_id: u64) -> String {
    ctx.guild_id
        .and_then(|g| ctx.core.world.with_guild(g, |g| g.channel_name(channel_id)))
        .unwrap_or_else(|| channel_id.to_string())
}

/// Hierarchy checks before acting on a member.
fn check_target(ctx: &ToolCtx, target: &MemberInfo) -> Result<(), String> {
    let guild_id = ctx.require_guild()?;
    let my_id = ctx.core.world.my_id();
    if target.user_id == my_id {
        return Err("I can't do that to myself".to_owned());
    }
    if ctx.acting_as_agent() && ctx.core.hive.peer_by_user(target.user_id).is_some() {
        return Err("I don't take moderation actions against my fellow agents".to_owned());
    }
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            if !ctx.acting_as_agent() && ctx.requester.kind == PrincipalKind::Human {
                if target.user_id == ctx.requester.user_id {
                    return Ok(());
                }
                if !authority::outranks(g, ctx.requester.user_id, &ctx.requester.roles, target) {
                    return Err(format!(
                        "denied: {} does not outrank {} (their highest role is equal or higher)",
                        ctx.requester.name,
                        target.display_name()
                    ));
                }
            }
            let my_roles = g.members.get(&my_id).map(|m| m.roles.clone()).unwrap_or_default();
            if !authority::outranks(g, my_id, &my_roles, target) {
                return Err(format!(
                    "my highest role isn't above {}'s, so Discord won't let me — move my role higher",
                    target.display_name()
                ));
            }
            Ok(())
        })
        .unwrap_or(Ok(()))
}

fn record_mod_action(ctx: &ToolCtx, target: &MemberInfo, action: &str, reason: &str) {
    let Some(guild_id) = ctx.guild_id else {
        return;
    };
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        k.log_mod_action(ModAction {
            at: now_ms(),
            agent: ctx.core.name(),
            user_id: target.user_id,
            user_name: target.display_name().to_owned(),
            action: action.to_owned(),
            reason: reason.to_owned(),
            channel_id: ctx.channel_id,
        })
    });
    ctx.core
        .hive
        .live
        .stat(&ctx.core.id, |s| s.mod_actions += 1);
    ctx.core.feed(
        FeedKind::Moderation,
        format!("{action} {} — {reason}", target.display_name()),
        Some(channel_name(ctx, ctx.channel_id)),
    );
    let core = ctx.core.clone();
    let line = format!(
        "🛡 **{action}** <@{}> ({}) — {reason}\n-# by {} in <#{}>",
        target.user_id,
        target.display_name(),
        if ctx.autonomous {
            core.name()
        } else {
            format!("{} via {}", ctx.requester.name, core.name())
        },
        ctx.channel_id
    );
    tokio::spawn(async move { core.post_mod_log(guild_id, &line).await });
}

fn format_time(ts: Timestamp) -> String {
    let dt = ts.as_datetime();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

/// Render a message for the model. `guild_id` comes from the caller's
/// context: REST-fetched messages don't carry one, but the jump link needs
/// it (gateway messages' own guild_id wins when present).
pub fn format_message(guild_id: Option<u64>, message: &Message) -> String {
    let author = message
        .member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .or_else(|| message.author.global_name.clone())
        .unwrap_or_else(|| message.author.username.clone());
    let mut line = format!(
        "[{} | id {}] {}{}: {}",
        format_time(message.timestamp),
        message.id,
        author,
        if message.author.bot.unwrap_or(false) {
            " (bot)"
        } else {
            ""
        },
        message.content
    );
    // A ready-to-copy jump link: the model pastes this verbatim, never
    // builds one from ids.
    if let Some(guild_id) = message.guild_id.map(|g| g.get()).or(guild_id) {
        let url = format!(
            "https://discord.com/channels/{}/{}/{}",
            guild_id, message.channel_id, message.id
        );
        remember_link(message.id.get(), &url);
        line.push_str(&format!(" [link: {url}]"));
    }
    for attachment in &message.attachments {
        line.push_str(&format!(" [attachment: {}]", attachment.filename));
    }
    for embed in &message.embeds {
        if let Some(title) = &embed.title {
            line.push_str(&format!(" [embed: {title}]"));
        }
    }
    if let Some(reactions) = &message.reactions {
        let rendered: Vec<String> = reactions
            .iter()
            .map(|r| format!("{}×{}", r.emoji.name.as_deref().unwrap_or("?"), r.count))
            .collect();
        if !rendered.is_empty() {
            line.push_str(&format!(" (reactions: {})", rendered.join(" ")));
        }
    }
    line
}

pub fn parse_emoji(text: &str) -> Emoji {
    let text = text.trim();
    // <:name:id> or <a:name:id> or name:id
    let inner = text.trim_start_matches('<').trim_end_matches('>');
    let (animated, inner) = match inner.strip_prefix("a:") {
        Some(rest) => (true, rest),
        None => (false, inner.trim_start_matches(':')),
    };
    if let Some((name, id)) = inner.rsplit_once(':') {
        if let Ok(id) = id.parse::<u64>() {
            return Emoji {
                id: Some(EmojiId::new(id)),
                name: Some(name.to_owned()),
                animated: Some(animated),
            };
        }
    }
    Emoji {
        id: None,
        name: Some(text.trim_matches(':').to_owned()),
        animated: None,
    }
}

// --- server info --------------------------------------------------------------

pub async fn get_server_info(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            let owner = g
                .members
                .get(&g.owner_id)
                .map(|m| m.display_name().to_owned())
                .unwrap_or_else(|| g.owner_id.to_string());
            let text_channels = g.channels.values().filter(|c| c.is_text_based() && !c.is_thread()).count();
            let voice = g.channels.values().filter(|c| c.is_voice()).count();
            let mut out = format!(
                "Server \"{}\" (ID {}). Owner: {owner}. Members: ~{}. Channels: {text_channels} text, {voice} voice, {} roles.",
                g.name,
                g.id,
                g.member_count.max(g.members.len() as u64),
                g.roles.len().saturating_sub(1)
            );
            if let Some(description) = &g.description {
                out.push_str(&format!("\nDescription: {description}"));
            }
            if let Some(rules) = g.rules_channel {
                out.push_str(&format!("\nRules channel: #{}", g.channel_name(rules)));
            }
            if !g.features.is_empty() {
                out.push_str(&format!("\nFeatures: {}", g.features.join(", ")));
            }
            if !g.emojis.is_empty() {
                let emojis: Vec<String> = g
                    .emojis
                    .iter()
                    .take(30)
                    .filter_map(|e| Some(format!("<{}:{}:{}>", if e.animated == Some(true) { "a" } else { "" }, e.name.as_ref()?, e.id?)))
                    .collect();
                out.push_str(&format!("\nCustom emojis: {}", emojis.join(" ")));
            }
            out
        })
        .ok_or_else(|| "I don't have this server cached yet".to_owned())
}

pub async fn list_channels(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            let mut lines = Vec::new();
            for channel in g.sorted_channels() {
                if channel.is_thread() {
                    continue;
                }
                let visible = ctx
                    .requester
                    .channel_permissions(Some(g), channel.id.get())
                    .contains(Permissions::VIEW_CHANNEL);
                if !visible && !ctx.acting_as_agent() {
                    continue;
                }
                let category = channel
                    .parent_id
                    .map(|p| format!(" in {}", g.channel_name(p.get())))
                    .unwrap_or_default();
                let topic = channel
                    .topic
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(|t| format!(" — {}", crate::llm::truncate(t, 80)))
                    .unwrap_or_default();
                lines.push(format!(
                    "<#{}> {} ({}{category}){topic}",
                    channel.id,
                    channel.name.as_deref().unwrap_or("?"),
                    World::channel_kind_label(channel.r#type)
                ));
            }
            lines.join("\n")
        })
        .ok_or_else(|| "server not cached".to_owned())
}

pub async fn list_roles(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            g.sorted_roles()
                .iter()
                .map(|role| {
                    let count = g
                        .members
                        .values()
                        .filter(|m| m.roles.contains(&role.id.get()))
                        .count();
                    let notable: Vec<&str> = if role.permissions.is_administrator() {
                        vec!["Administrator"]
                    } else {
                        role.permissions
                            .names()
                            .filter(|n| {
                                ["Kick", "Ban", "Manage", "Moderate", "Mention Everyone"]
                                    .iter()
                                    .any(|k| n.contains(k))
                            })
                            .take(4)
                            .collect()
                    };
                    format!(
                        "{} (ID {}, {count} cached members{}{})",
                        role.name,
                        role.id,
                        if role.managed {
                            ", bot/integration role"
                        } else {
                            ""
                        },
                        if notable.is_empty() {
                            String::new()
                        } else {
                            format!(", {}", notable.join(", "))
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .ok_or_else(|| "server not cached".to_owned())
}

pub async fn get_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let member = resolve_member(ctx, &required(args, "user")?).await?;
    let guild_id = ctx.require_guild()?;
    let agent = ctx.core.hive.peer_by_user(member.user_id);
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            let roles: Vec<String> = {
                let mut roles: Vec<_> =
                    member.roles.iter().filter_map(|r| g.roles.get(r)).collect();
                roles.sort_by(|a, b| b.position.cmp(&a.position));
                roles.iter().map(|r| r.name.clone()).collect()
            };
            let principal = authority::Principal::for_member(g, &member, PrincipalKind::Human);
            let mut out = format!(
                "<@{}> — {}, rank: {}{}",
                member.user_id,
                super::person_label(
                    member.nick.as_deref(),
                    member.global_name.as_deref(),
                    &member.username,
                    member.user_id
                ),
                principal.describe_rank(),
                if member.bot { ", bot" } else { "" }
            );
            if let Some(peer) = agent {
                out.push_str(&format!(
                    " — this is {}, one of your fellow AI agents ({})",
                    peer.name, peer.specialty
                ));
            }
            out.push_str(&format!(
                "\nRoles: {}",
                if roles.is_empty() {
                    "none".to_owned()
                } else {
                    roles.join(", ")
                }
            ));
            if let Some(joined) = member.joined_at {
                out.push_str(&format!("\nJoined: {}", format_time(joined)));
            }
            out.push_str(&format!(
                "\nAccount created: {}",
                format_time(UserId::new(member.user_id).created_at())
            ));
            if member.is_timed_out() {
                if let Some(until) = member.timeout_until {
                    out.push_str(&format!("\nTimed out until {}", format_time(until)));
                }
            }
            if let Some(voice) = g.voice.get(&member.user_id) {
                out.push_str(&format!("\nIn voice: {}", g.channel_name(*voice)));
            }
            out
        })
        .ok_or_else(|| "server not cached".to_owned())
}

pub async fn list_members(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let limit = number(args, "limit").unwrap_or(25).clamp(1, 100) as usize;
    let query = arg(args, "query").map(|q| q.to_lowercase());
    if let Some(q) = &query {
        // Make sure Discord's view is included, not just the cache.
        if let Ok(found) = ctx
            .core
            .http
            .search_members(GuildId::new(guild_id), q.clone())
            .limit(limit as u16)
            .await
        {
            for member in &found {
                ctx.core.world.upsert_member(guild_id, member);
            }
        }
    }
    let role_filter = match arg(args, "role") {
        Some(role) => Some(
            ctx.core
                .world
                .with_guild(guild_id, |g| g.find_role(&role).map(|r| r.id.get()))
                .flatten()
                .ok_or_else(|| format!("no role matches \"{role}\""))?,
        ),
        None => None,
    };
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            let mut members: Vec<&MemberInfo> = g
                .members
                .values()
                .filter(|m| role_filter.is_none_or(|r| m.roles.contains(&r)))
                .filter(|m| {
                    query.as_ref().is_none_or(|q| {
                        m.username.to_lowercase().contains(q)
                            || m.display_name().to_lowercase().contains(q)
                    })
                })
                .collect();
            members.sort_by_key(|m| std::cmp::Reverse(authority::top_position(g, &m.roles)));
            let total = members.len();
            let lines: Vec<String> = members
                .iter()
                .take(limit)
                .map(|m| {
                    format!(
                        "- {}{}{}",
                        super::person_label(
                            m.nick.as_deref(),
                            m.global_name.as_deref(),
                            &m.username,
                            m.user_id
                        ),
                        if m.bot { " [bot]" } else { "" },
                        g.top_role(m)
                            .map(|r| format!(" — {}", r.name))
                            .unwrap_or_default()
                    )
                })
                .collect();
            format!("{total} matching cached member(s):\n{}", lines.join("\n"))
        })
        .ok_or_else(|| "server not cached".to_owned())
}

pub async fn read_messages(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    // Forums hold posts (threads), not messages.
    if let Some(guild_id) = ctx.guild_id {
        if ctx
            .core
            .world
            .channel(guild_id, channel_id)
            .is_some_and(|c| c.is_forum())
        {
            let forum_args = serde_json::json!({ "forum": channel_id.to_string() });
            return list_forum_posts(ctx, &forum_args).await;
        }
    }
    ctx.require_in(
        channel_id,
        Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
    )?;
    let limit = number(args, "limit").unwrap_or(30).clamp(1, 100) as u8;
    let mut request = ctx
        .core
        .http
        .get_messages(ChannelId::new(channel_id))
        .limit(limit);
    if let Some(before) = arg(args, "before_message_id").and_then(|b| b.parse::<u64>().ok()) {
        request = request.before(MessageId::new(before));
    }
    let mut messages = request.await.map_err(discord_error)?;
    // The bot's own status messages (the work icon and its progress
    // edits) are for the room, not for the model — reading them back
    // makes it talk about its own "thinking".
    let me = ctx.core.world.my_id();
    messages.retain(|m| !(m.author.id.get() == me && is_status_content(&m.content)));
    messages.reverse();
    if messages.is_empty() {
        return Ok("no messages".to_owned());
    }
    let guild = ctx.guild_id;
    Ok(messages
        .iter()
        .map(|m| format_message(guild, m))
        .collect::<Vec<_>>()
        .join(
            "
",
        ))
}

/// Is this the content of one of the bot's own status messages? The bare
/// work icon (often alone), possibly with short progress lines under it —
/// never real prose.
pub(crate) fn is_status_content(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    lines.iter().all(|line| {
        let line = line.trim().trim_start_matches("-#").trim();
        line.chars().count() <= 48 && !line.ends_with(['.', '!', '?'])
    }) && lines.len() <= 4
}

/// Message jump links the model was shown (read_messages/search), by
/// message id — the canonical form, built by the app.
fn shown_links() -> &'static Mutex<HashMap<u64, String>> {
    static LINKS: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();
    LINKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_link(message_id: u64, url: &str) {
    let mut links = shown_links().lock().expect("links poisoned");
    links.insert(message_id, url.to_owned());
    // A bounded cache: the most recent links are the relevant ones.
    if links.len() > 400 {
        let oldest: Vec<u64> = links.keys().copied().take(100).collect();
        for id in oldest {
            links.remove(&id);
        }
    }
}

/// Small models re-compose jump links from what they remember ("…/Probe
/// Server/general/303") instead of pasting the one they were given. Any
/// discord.com/channels URL in a reply whose trailing message id is known
/// is rewritten to the canonical URL the app showed; unknown ones are left
/// untouched.
pub(crate) fn repair_links(text: &str) -> String {
    const PREFIX: &str = "https://discord.com/channels/";
    if !text.contains(PREFIX) {
        return text.to_owned();
    }
    let links = shown_links().lock().expect("links poisoned").clone();
    if links.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start + PREFIX.len()..];
        // The URL runs until whitespace or a closing bracket/paren.
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '|'))
            .unwrap_or(after.len());
        let candidate = &after[..end];
        let last_number = candidate.rsplit('/').next().unwrap_or("");
        match last_number
            .parse::<u64>()
            .ok()
            .and_then(|id| links.get(&id))
        {
            Some(canonical) => out.push_str(canonical),
            // Unknown or partial: paste it back as-is rather than break it.
            None => out.push_str(&rest[start..start + PREFIX.len() + end]),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

pub async fn search_messages(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let query = required(args, "query")?.to_lowercase();
    let author = arg(args, "author").map(|a| a.to_lowercase());
    let channels: Vec<u64> = match arg(args, "channel") {
        Some(channel) => vec![resolve_channel(ctx, Some(channel))?],
        None => ctx
            .core
            .world
            .with_guild(guild_id, |g| g.channels.keys().copied().collect())
            .unwrap_or_default(),
    };
    let mut hits = Vec::new();
    for channel_id in channels {
        if ctx
            .require_in(
                channel_id,
                Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
            )
            .is_err()
        {
            continue;
        }
        let name = channel_name(ctx, channel_id);
        ctx.core.hive.knowledge.channel(channel_id, |archive| {
            for message in archive.messages.iter().filter(|m| !m.deleted) {
                let content = message.content.to_lowercase();
                let author_ok = author.as_ref().is_none_or(|a| {
                    message.author.to_lowercase().contains(a) || message.author_id.to_string() == *a
                });
                if author_ok && query.split_whitespace().all(|word| content.contains(word)) {
                    let link = Some(guild_id).map(|g| {
                        let url = format!(
                            "https://discord.com/channels/{g}/{channel_id}/{}",
                            message.id
                        );
                        remember_link(message.id, &url);
                        format!(" [link: {url}]")
                    });
                    hits.push((
                        message.id,
                        format!(
                            "#{name} [id {}] {}: {}{link}",
                            message.id,
                            message.author,
                            crate::llm::truncate(&message.content, 300),
                            link = link.unwrap_or_default()
                        ),
                    ));
                }
            }
        });
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    if hits.is_empty() {
        return Ok("no matching messages in the recent history I keep".to_owned());
    }
    Ok(hits
        .into_iter()
        .take(25)
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n"))
}

pub async fn get_pinned(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    ctx.require_in(
        channel_id,
        Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
    )?;
    let pins = ctx
        .core
        .http
        .get_pinned_messages(ChannelId::new(channel_id))
        .await
        .map_err(discord_error)?;
    if pins.items.is_empty() {
        return Ok("no pinned messages".to_owned());
    }
    let guild = ctx.guild_id;
    Ok(pins
        .items
        .iter()
        .map(|p| format_message(guild, &p.message))
        .collect::<Vec<_>>()
        .join("\n"))
}

pub async fn list_threads(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let active = ctx
        .core
        .http
        .get_active_threads(GuildId::new(guild_id))
        .await
        .map_err(discord_error)?;
    let lines: Vec<String> = active
        .threads
        .iter()
        .filter(|t| {
            ctx.acting_as_agent()
                || ctx
                    .requester_permissions_in(t.parent_id.map(|p| p.get()).unwrap_or(t.id.get()))
                    .contains(Permissions::VIEW_CHANNEL)
        })
        .map(|t| {
            ctx.core.world.upsert_channel(t);
            format!(
                "<#{}> \"{}\" in #{} ({} messages)",
                t.id,
                t.name.as_deref().unwrap_or("?"),
                t.parent_id
                    .map(|p| channel_name(ctx, p.get()))
                    .unwrap_or_default(),
                t.message_count.unwrap_or(0)
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        "no active threads".to_owned()
    } else {
        lines.join("\n")
    })
}

pub async fn voice_overview(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            let mut by_channel: std::collections::BTreeMap<String, Vec<String>> =
                Default::default();
            for (user, channel) in &g.voice {
                let name = g
                    .members
                    .get(user)
                    .map(|m| m.display_name().to_owned())
                    .unwrap_or_else(|| user.to_string());
                by_channel
                    .entry(g.channel_name(*channel))
                    .or_default()
                    .push(name);
            }
            if by_channel.is_empty() {
                return "nobody is in voice right now".to_owned();
            }
            by_channel
                .into_iter()
                .map(|(channel, people)| format!("🔊 {channel}: {}", people.join(", ")))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .ok_or_else(|| "server not cached".to_owned())
}

// --- messaging ------------------------------------------------------------------

fn mentions_for(ctx: &ToolCtx, channel_id: u64) -> AllowedMentions {
    let perms = ctx.requester_permissions_in(channel_id);
    if !ctx.acting_as_agent() && perms.contains(Permissions::MENTION_EVERYONE) {
        AllowedMentions {
            parse: vec![
                "users".to_owned(),
                "roles".to_owned(),
                "everyone".to_owned(),
            ],
            ..AllowedMentions::default()
        }
    } else {
        AllowedMentions::users()
    }
}

pub async fn send_message(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_text_channel(ctx, arg(args, "channel"))?;
    if ctx.replies_here && channel_id == ctx.channel_id {
        return Err(
            "not sent: your final answer is already posted in this channel — write the message as your reply instead"
                .to_owned(),
        );
    }
    let content = required(args, "content")?;
    if let Some(rule) = ctx.guild_id.and_then(|g| ctx.core.barred(g, channel_id)) {
        return Err(format!(
            "not sent: you never chat in that channel ({} set that rule) — it has to be lifted first (allow_chatting_in)",
            rule.setter_name
        ));
    }
    ctx.require_in(
        channel_id,
        Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
    )?;
    let mut sent_ids = Vec::new();
    for (index, chunk) in crate::runtime::brain::split_message(&content)
        .into_iter()
        .enumerate()
    {
        let mut request = ctx
            .core
            .http
            .create_message(ChannelId::new(channel_id))
            .content(chunk)
            .allowed_mentions(mentions_for(ctx, channel_id));
        if index == 0 {
            if let Some(reply) = arg(args, "reply_to").and_then(|r| r.parse::<u64>().ok()) {
                request = request.reply(MessageId::new(reply));
            }
        }
        let message = request.await.map_err(discord_error)?;
        sent_ids.push(message.id.to_string());
    }
    Ok(format!(
        "sent to #{} (message {})",
        channel_name(ctx, channel_id),
        sent_ids.join(", ")
    ))
}

pub async fn edit_my_message(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    let message_id = resolve_message_id(ctx, args, "message_id")?;
    let content = required(args, "content")?;
    let message = ctx
        .core
        .http
        .get_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .await
        .map_err(discord_error)?;
    if message.author.id.get() != ctx.core.world.my_id() {
        return Err("that's not my message — I can only edit my own".to_owned());
    }
    ctx.core
        .http
        .edit_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .content(crate::llm::truncate(&content, 1990))
        .allowed_mentions(AllowedMentions::users())
        .await
        .map_err(discord_error)?;
    Ok("edited".to_owned())
}

/// The agent's own most recent message in a channel (or its threads /
/// forum posts), or anywhere in the server — optionally containing text.
fn find_my_message(
    ctx: &ToolCtx,
    channel: Option<u64>,
    contains: Option<&str>,
) -> Option<(u64, u64, String)> {
    let me = ctx.core.world.my_id();
    let guild_id = ctx.guild_id?;
    let channels: Vec<u64> = ctx
        .core
        .world
        .with_guild(guild_id, |g| {
            g.channels
                .values()
                .filter(|c| match channel {
                    Some(wanted) => {
                        c.id.get() == wanted || c.parent_id.map(|p| p.get()) == Some(wanted)
                    }
                    None => true,
                })
                .map(|c| c.id.get())
                .collect()
        })
        .unwrap_or_default();
    let needle = contains.map(str::to_lowercase);
    let mut best: Option<(u64, u64, String)> = None;
    for channel_id in channels {
        let found = ctx.core.hive.knowledge.channel(channel_id, |archive| {
            archive
                .messages
                .iter()
                .rev()
                .filter(|m| m.author_id == me && !m.deleted)
                .find(|m| {
                    needle
                        .as_ref()
                        .is_none_or(|n| m.content.to_lowercase().contains(n))
                })
                .map(|m| (m.id, m.content.clone()))
        });
        if let Some((id, content)) = found {
            if best.as_ref().is_none_or(|(best_id, _, _)| id > *best_id) {
                best = Some((id, channel_id, content));
            }
        }
    }
    best
}

pub async fn delete_my_message(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel = match arg(args, "channel") {
        Some(query) => Some(resolve_channel(ctx, Some(query))?),
        None => None,
    };
    let (channel_id, message_id) = match arg(args, "message_id") {
        Some(_) => (
            channel.unwrap_or(ctx.channel_id),
            resolve_message_id(ctx, args, "message_id")?,
        ),
        None => {
            let contains = arg(args, "contains");
            let (id, channel_id, _) = find_my_message(ctx, channel, contains.as_deref())
                .ok_or_else(|| {
                    format!(
                        "I can't find a recent message of mine{}{} in the history I keep",
                        channel
                            .map(|c| format!(" in #{}", channel_name(ctx, c)))
                            .unwrap_or_default(),
                        contains
                            .as_ref()
                            .map(|c| format!(" containing \"{c}\""))
                            .unwrap_or_default()
                    )
                })?;
            (channel_id, id)
        }
    };
    let message = ctx
        .core
        .http
        .get_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .await
        .map_err(discord_error)?;
    if message.author.id.get() != ctx.core.world.my_id() {
        return Err("that's not my message".to_owned());
    }
    ctx.core
        .http
        .delete_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "deleted my message in #{}: “{}”",
        channel_name(ctx, channel_id),
        crate::llm::truncate(&message.full_text(), 80)
    ))
}

pub async fn pin_message(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    let message_id = resolve_message_id(ctx, args, "message_id")?;
    if !ctx.acting_as_agent() {
        let perms = ctx.requester_permissions_in(channel_id);
        if !perms.intersects(Permissions::PIN_MESSAGES | Permissions::MANAGE_MESSAGES) {
            return Err(format!(
                "denied: {} can't pin messages here",
                ctx.requester.name
            ));
        }
    }
    let (channel, message) = (ChannelId::new(channel_id), MessageId::new(message_id));
    if flag(args, "unpin").unwrap_or(false) {
        ctx.core
            .http
            .unpin_message(channel, message)
            .await
            .map_err(discord_error)?;
        Ok("unpinned".to_owned())
    } else {
        ctx.core
            .http
            .pin_message(channel, message)
            .await
            .map_err(discord_error)?;
        Ok("pinned".to_owned())
    }
}

/// Describe a message for tool results ("Alice's message “hi”"), from the
/// archive when possible, so the model can't misreport what it acted on.
fn describe_message(ctx: &ToolCtx, channel_id: u64, message_id: u64) -> String {
    let stored = ctx
        .core
        .hive
        .knowledge
        .channel(channel_id, |archive| archive.get_mut(message_id).cloned());
    let base = match stored {
        Some(m) => format!(
            "{}'s message “{}”",
            m.author,
            crate::llm::truncate(&m.content, 60)
        ),
        None => format!("message {message_id}"),
    };
    let current = if Some(message_id) == ctx.message_id {
        " (the message you are answering)"
    } else {
        ""
    };
    format!("{base}{current} in #{}", channel_name(ctx, channel_id))
}

pub async fn react(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    let emoji_text = required(args, "emoji")?;
    // Another channel without a message ID: a thread or forum post means
    // its first message (which shares the thread's ID). Never fall back to
    // the message being answered — that silently reacts to the wrong thing.
    let message_id = match arg(args, "message_id") {
        Some(_) => resolve_message_id(ctx, args, "message_id")?,
        None if channel_id != ctx.channel_id => {
            let is_thread = ctx
                .guild_id
                .and_then(|g| ctx.core.world.channel(g, channel_id))
                .is_some_and(|c| c.is_thread());
            if is_thread {
                channel_id
            } else {
                return Err(format!(
                    "which message in #{}? pass message_id (get IDs with read_messages)",
                    channel_name(ctx, channel_id)
                ));
            }
        }
        None => ctx.message_id.ok_or("which message? pass message_id")?,
    };
    if channel_id != ctx.channel_id {
        ctx.require_in(channel_id, Permissions::ADD_REACTIONS)?;
    }
    let emoji = parse_emoji(&emoji_text);
    let target = describe_message(ctx, channel_id, message_id);
    let (channel, message) = (ChannelId::new(channel_id), MessageId::new(message_id));
    if flag(args, "remove").unwrap_or(false) {
        ctx.core
            .http
            .delete_own_reaction(channel, message, emoji)
            .await
            .map_err(discord_error)?;
        Ok(format!("removed {emoji_text} from {target}"))
    } else {
        ctx.core
            .http
            .add_reaction(channel, message, emoji)
            .await
            .map_err(discord_error)?;
        Ok(format!("reacted {emoji_text} to {target}"))
    }
}

/// Channel flag: the forum requires a tag on every post.
const REQUIRE_TAG: u64 = 1 << 4;
/// Channel flag: the post is pinned in its forum.
const PINNED: u64 = 1 << 1;

/// A forum's posts with their opening messages, plus the forum's post
/// guidelines — everything needed to review posts in one call.
pub async fn list_forum_posts(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let forum_id = resolve_channel(ctx, arg(args, "forum").or_else(|| arg(args, "channel")))?;
    let forum = ctx
        .core
        .world
        .channel(guild_id, forum_id)
        .ok_or("I don't know that channel")?;
    if !forum.is_forum() {
        return Err(format!(
            "#{} is not a forum — use read_messages for regular channels",
            forum.name.unwrap_or_default()
        ));
    }
    ctx.require_in(
        forum_id,
        Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
    )?;
    let limit = number(args, "limit").unwrap_or(10).clamp(1, 25) as usize;
    let active = ctx
        .core
        .http
        .get_active_threads(GuildId::new(guild_id))
        .await
        .map_err(discord_error)?;
    let mut posts: Vec<_> = active
        .threads
        .into_iter()
        .filter(|t| t.parent_id.map(|p| p.get()) == Some(forum_id))
        .collect();
    posts.sort_by(|a, b| b.id.get().cmp(&a.id.get()));
    let tag_name = |id: GenericId| {
        forum
            .available_tags
            .iter()
            .flatten()
            .find(|t| t.id == id)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let mut out = format!(
        "Forum #{} — {} open post(s).\nPost guidelines: {}\nAvailable tags: {}{}\n",
        forum.name.as_deref().unwrap_or("?"),
        posts.len(),
        forum
            .topic
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("(none set in the forum settings — check the pinned post)"),
        {
            let tags: Vec<String> = forum
                .available_tags
                .iter()
                .flatten()
                .map(|t| t.name.clone())
                .collect();
            if tags.is_empty() {
                "none".to_owned()
            } else {
                tags.join(", ")
            }
        },
        if forum.flags.unwrap_or(0) & REQUIRE_TAG != 0 {
            " (every post must have a tag)"
        } else {
            ""
        }
    );
    for post in posts.into_iter().take(limit) {
        ctx.core.world.upsert_channel(&post);
        let starter = ctx
            .core
            .http
            .get_message(post.id, MessageId::new(post.id.get()))
            .await
            .ok();
        let tags: Vec<String> = post
            .applied_tags
            .iter()
            .flatten()
            .map(|id| tag_name(*id))
            .collect();
        out.push_str(&format!(
            "\n### Post “{}” — post id {}{}\nTags: {}\nReplies: {}\n",
            post.name.as_deref().unwrap_or("?"),
            post.id,
            if post.flags.unwrap_or(0) & PINNED != 0 {
                " (PINNED — usually the guidelines post)"
            } else {
                ""
            },
            if tags.is_empty() {
                "none".to_owned()
            } else {
                tags.join(", ")
            },
            post.message_count.unwrap_or(0)
        ));
        match starter {
            Some(message) => {
                ctx.core
                    .hive
                    .knowledge
                    .channel_mut(post.id.get(), |archive| {
                        archive.push(crate::runtime::agent::to_stored(&message));
                    });
                out.push_str(&format!(
                    "Author: {} (<@{}>)\nOpening message: {}{}\n",
                    crate::runtime::agent::display_name(&message),
                    message.author.id,
                    crate::llm::truncate(&message.content, 700),
                    message
                        .attachments
                        .iter()
                        .map(|a| format!(" [attachment: {}]", a.filename))
                        .collect::<String>()
                ));
            }
            None => {
                out.push_str("Opening message: (could not be read — it may have been deleted)\n")
            }
        }
    }
    out.push_str(
        "\nTo reply inside a post: send_message with channel = the post id. \
         To react to a post: react with channel = the post id (no message_id needed).",
    );
    Ok(out)
}

pub async fn create_poll(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    ctx.require_in(
        channel_id,
        Permissions::SEND_MESSAGES | Permissions::SEND_POLLS,
    )?;
    let question = required(args, "question")?;
    let answers: Vec<String> = list(args, "answers")
        .into_iter()
        .map(|a| a.chars().take(55).collect())
        .take(10)
        .collect();
    if answers.len() < 2 {
        return Err("a poll needs at least 2 answers".to_owned());
    }
    let hours = number(args, "hours").unwrap_or(24).clamp(1, 768) as u32;
    let mut poll = PollCreate::new(
        question.chars().take(300).collect::<String>(),
        answers,
        hours,
    );
    poll.allow_multiselect = flag(args, "multiple").unwrap_or(false);
    let message = ctx
        .core
        .http
        .create_message(ChannelId::new(channel_id))
        .poll(poll)
        .await
        .map_err(discord_error)?;
    Ok(format!("poll posted (message {}) for {hours}h", message.id))
}

// --- threads & forums ----------------------------------------------------------

pub async fn create_thread(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    let name: String = required(args, "name")?.chars().take(100).collect();
    let private = flag(args, "private").unwrap_or(false);
    ctx.require_in(
        channel_id,
        if private {
            Permissions::CREATE_PRIVATE_THREADS
        } else {
            Permissions::CREATE_PUBLIC_THREADS
        },
    )?;
    let from = arg(args, "from_message_id");
    let standalone = private
        || from
            .as_deref()
            .is_some_and(|f| f.eq_ignore_ascii_case("none"));
    let thread = if standalone {
        ctx.core
            .http
            .start_thread(ChannelId::new(channel_id), name)
            .private(private)
            .auto_archive_duration(1440)
            .await
            .map_err(discord_error)?
    } else {
        let message_id = resolve_message_id(ctx, args, "from_message_id")?;
        ctx.core
            .http
            .start_thread_from_message(ChannelId::new(channel_id), MessageId::new(message_id), name)
            .auto_archive_duration(1440)
            .await
            .map_err(discord_error)?
    };
    ctx.core.world.upsert_channel(&thread);
    if private && ctx.requester.user_id != 0 && ctx.requester.kind == PrincipalKind::Human {
        let _ = ctx
            .core
            .http
            .add_thread_member(thread.id, UserId::new(ctx.requester.user_id))
            .await;
    }
    if let Some(first) = arg(args, "first_message") {
        ctx.core
            .http
            .create_message(thread.id)
            .content(crate::llm::truncate(&first, 1990))
            .allowed_mentions(AllowedMentions::users())
            .await
            .map_err(discord_error)?;
    }
    Ok(format!(
        "thread created: <#{}> (ID {})",
        thread.id, thread.id
    ))
}

pub async fn create_forum_post(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let forum_id = resolve_channel(ctx, Some(required(args, "forum")?))?;
    let forum = ctx
        .core
        .world
        .channel(guild_id, forum_id)
        .ok_or("I don't know that channel")?;
    if !forum.is_forum() {
        return Err(format!(
            "#{} is not a forum channel",
            forum.name.unwrap_or_default()
        ));
    }
    ctx.require_in(forum_id, Permissions::SEND_MESSAGES)?;
    let wanted = list(args, "tags");
    let tags: Vec<GenericId> = forum
        .available_tags
        .iter()
        .flatten()
        .filter(|tag| wanted.iter().any(|w| w.eq_ignore_ascii_case(&tag.name)))
        .map(|tag| tag.id)
        .collect();
    let post = ctx
        .core
        .http
        .create_forum_post(
            ChannelId::new(forum_id),
            required(args, "title")?
                .chars()
                .take(100)
                .collect::<String>(),
        )
        .content(crate::llm::truncate(&required(args, "content")?, 1990))
        .applied_tags(tags)
        .allowed_mentions(AllowedMentions::users())
        .await
        .map_err(discord_error)?;
    ctx.core.world.upsert_channel(&post);
    Ok(format!("posted: <#{}>", post.id))
}

pub async fn archive_thread(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let thread_id = resolve_channel(ctx, arg(args, "thread"))?;
    let lock = flag(args, "lock").unwrap_or(false);
    let mut request = ctx
        .core
        .http
        .edit_channel(ChannelId::new(thread_id))
        .archived(true)
        .reason(ctx.reason(None));
    if lock {
        request = request.locked(true);
    }
    request.await.map_err(discord_error)?;
    Ok(format!(
        "thread archived{}",
        if lock { " and locked" } else { "" }
    ))
}

pub async fn add_to_thread(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let thread_id = resolve_channel(ctx, arg(args, "thread"))?;
    let member = resolve_member(ctx, &required(args, "user")?).await?;
    ctx.core
        .http
        .add_thread_member(ChannelId::new(thread_id), UserId::new(member.user_id))
        .await
        .map_err(discord_error)?;
    Ok(format!("added {} to the thread", member.display_name()))
}

// --- voice ------------------------------------------------------------------------

fn voice_self_service(ctx: &ToolCtx, target: &MemberInfo) -> bool {
    !ctx.acting_as_agent() && target.user_id == ctx.requester.user_id
}

pub async fn join_voice(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let is_voice = |id: u64| {
        ctx.core
            .world
            .channel(guild_id, id)
            .is_some_and(|c| c.is_voice())
    };
    // The named channel; else this channel when it's a voice channel's
    // chat ("join" typed there); else the requester's voice channel.
    let named = arg(args, "channel")
        .and_then(|q| resolve_channel(ctx, Some(q)).ok())
        .filter(|id| is_voice(*id));
    let requesters = ctx
        .core
        .world
        .with_guild(guild_id, |g| g.voice.get(&ctx.requester.user_id).copied())
        .flatten();
    let channel_id = named
        .or_else(|| is_voice(ctx.channel_id).then_some(ctx.channel_id))
        .or(requesters)
        .ok_or_else(|| match arg(args, "channel") {
            Some(query) => format!("there's no voice channel matching \"{query}\" — use a voice channel's name"),
            None => format!(
                "{} isn't in a voice channel and this isn't a voice channel's chat — say which voice channel to join",
                ctx.requester.name
            ),
        })?;
    let channel = ctx
        .core
        .world
        .channel(guild_id, channel_id)
        .ok_or("unknown channel")?;
    ctx.require_in(channel_id, Permissions::CONNECT)?;
    let me = ctx.core.self_principal(Some(guild_id));
    let my_permissions = ctx
        .core
        .world
        .with_guild(guild_id, |g| me.channel_permissions(Some(g), channel_id))
        .unwrap_or_else(Permissions::all);
    if !my_permissions.contains(Permissions::CONNECT | Permissions::SPEAK) {
        return Err(format!(
            "I don't have permission to connect and speak in #{}",
            channel.name.clone().unwrap_or_default()
        ));
    }
    crate::runtime::voice::join(&ctx.core, guild_id, channel_id, Some(ctx.channel_id)).await?;
    let profile = ctx.core.profile();
    Ok(format!(
        "joined voice channel #{} — {}",
        channel.name.unwrap_or_default(),
        if profile.voice.listen {
            "I'll hear people there and answer out loud (and in its text chat when that's better)"
        } else {
            "listening is off in my settings, so I can only speak"
        }
    ))
}

pub async fn list_voices(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    use crate::runtime::speech;
    let profile = ctx.core.profile();
    let mut out = format!(
        "Current voice: {}\n\nKokoro voices (most natural):\n",
        speech::current_voice(&profile.voice)
    );
    for (id, description) in speech::KOKORO_VOICES {
        out.push_str(&format!("- {id}: {description}\n"));
    }
    out.push_str("\nPiper voices:\n");
    for (id, description) in speech::PIPER_VOICES {
        out.push_str(&format!("- {id}: {description}\n"));
    }
    out.push_str("\nPeople can pick one by ID or describe what they want (e.g. \"british male\").");
    Ok(out)
}

pub async fn set_voice(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    use crate::config::TtsEngine;
    use crate::runtime::speech;
    let choice = match arg(args, "voice") {
        Some(query) => Some(speech::find_voice(&query).ok_or_else(|| {
            format!("no voice matches \"{query}\" — call list_voices and pick an ID")
        })?),
        None => None,
    };
    let speed = args
        .get("speed")
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
        .map(|s| s.clamp(0.5, 2.0) as f32);
    if choice.is_none() && speed.is_none() {
        return Err("say which voice (an ID or a description) or what speed".to_owned());
    }
    let apply = move |profile: &mut crate::config::AgentProfile| {
        if let Some((engine, id, _)) = choice {
            profile.voice.tts.engine = engine;
            match engine {
                TtsEngine::Kokoro => profile.voice.tts.kokoro_voice = id.to_owned(),
                _ => profile.voice.tts.piper_voice = id.to_owned(),
            }
        }
        if let Some(speed) = speed {
            profile.voice.tts.speed = speed;
        }
    };
    // Now, and saved in the app's settings.
    let mut profile = ctx.core.profile();
    apply(&mut profile);
    let now = speech::current_voice(&profile.voice);
    ctx.core.set_profile(profile);
    ctx.core
        .live()
        .profile_patches
        .lock()
        .expect("live poisoned")
        .push((ctx.core.id.clone(), Box::new(apply)));
    ctx.core.live().repaint();
    ctx.core
        .feed(FeedKind::System, format!("switched voice to {now}"), None);
    Ok(format!("now speaking with {now}"))
}

pub async fn leave_voice(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let channel = ctx
        .core
        .voice
        .channel(guild_id)
        .ok_or("I'm not in a voice channel")?;
    crate::runtime::voice::leave(&ctx.core, guild_id)?;
    Ok(format!("left #{}", channel_name(ctx, channel)))
}

pub async fn say_in_voice(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let text = required(args, "text")?;
    crate::runtime::voice::say(&ctx.core, guild_id, &text).await?;
    Ok("said it out loud".to_owned())
}

pub async fn move_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    let destination = resolve_channel(ctx, Some(required(args, "channel")?))?;
    let channel = ctx
        .core
        .world
        .channel(guild_id, destination)
        .ok_or("unknown channel")?;
    if !channel.is_voice() {
        return Err(format!(
            "#{} is not a voice channel",
            channel.name.unwrap_or_default()
        ));
    }
    let in_voice = ctx
        .core
        .world
        .with_guild(guild_id, |g| g.voice.contains_key(&target.user_id))
        .unwrap_or(false);
    if !in_voice {
        return Err(format!(
            "{} isn't in a voice channel (Discord can only move people who are)",
            target.display_name()
        ));
    }
    if voice_self_service(ctx, &target) {
        ctx.require_in(destination, Permissions::CONNECT)?;
    } else {
        if !ctx.acting_as_agent()
            && !ctx
                .requester_permissions_in(destination)
                .contains(Permissions::MOVE_MEMBERS)
        {
            return Err(format!(
                "denied: {} can't move other people",
                ctx.requester.name
            ));
        }
        check_target(ctx, &target)?;
    }
    ctx.core
        .http
        .edit_member(GuildId::new(guild_id), UserId::new(target.user_id))
        .move_to_voice(ChannelId::new(destination))
        .reason(ctx.reason(None))
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "moved {} to 🔊 {}",
        target.display_name(),
        channel.name.unwrap_or_default()
    ))
}

pub async fn disconnect_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    if !voice_self_service(ctx, &target) {
        if !ctx.acting_as_agent()
            && !ctx
                .requester
                .permissions
                .contains(Permissions::MOVE_MEMBERS)
        {
            return Err(format!(
                "denied: {} can't disconnect other people",
                ctx.requester.name
            ));
        }
        check_target(ctx, &target)?;
    }
    ctx.core
        .http
        .edit_member(GuildId::new(guild_id), UserId::new(target.user_id))
        .disconnect_voice()
        .reason(ctx.reason(None))
        .await
        .map_err(discord_error)?;
    Ok(format!("disconnected {} from voice", target.display_name()))
}

pub async fn voice_mute(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    check_target(ctx, &target)?;
    let mute = flag(args, "mute");
    let deafen = flag(args, "deafen");
    if mute.is_none() && deafen.is_none() {
        return Err("say whether to mute and/or deafen".to_owned());
    }
    if !ctx.acting_as_agent()
        && deafen.is_some()
        && !ctx
            .requester
            .permissions
            .contains(Permissions::DEAFEN_MEMBERS)
    {
        return Err(format!(
            "denied: {} lacks Deafen Members",
            ctx.requester.name
        ));
    }
    let mut request = ctx
        .core
        .http
        .edit_member(GuildId::new(guild_id), UserId::new(target.user_id))
        .reason(ctx.reason(None));
    if let Some(mute) = mute {
        request = request.mute(mute);
    }
    if let Some(deafen) = deafen {
        request = request.deaf(deafen);
    }
    request.await.map_err(discord_error)?;
    Ok(format!("updated {}'s voice state", target.display_name()))
}

// --- moderation -------------------------------------------------------------------

pub async fn delete_message(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    let message_id = resolve_message_id(ctx, args, "message_id")?;
    let message = ctx
        .core
        .http
        .get_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .await
        .map_err(discord_error)?;
    let own_message = message.author.id.get() == ctx.requester.user_id;
    if !own_message && !ctx.acting_as_agent() {
        ctx.require_in(channel_id, Permissions::MANAGE_MESSAGES)?;
    }
    if !own_message && ctx.guild_id.is_some() && message.author.id.get() != ctx.core.world.my_id() {
        if let Ok(target) = resolve_member(ctx, &message.author.id.to_string()).await {
            check_target(ctx, &target)?;
        }
    }
    let reason = arg(args, "reason");
    ctx.core
        .http
        .delete_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .reason(ctx.reason(reason.clone()))
        .await
        .map_err(discord_error)?;
    if !own_message && ctx.guild_id.is_some() {
        if let Ok(target) = resolve_member(ctx, &message.author.id.to_string()).await {
            record_mod_action(
                ctx,
                &target,
                "deleted a message from",
                &reason.unwrap_or_else(|| "rule violation".to_owned()),
            );
        }
    }
    Ok("message deleted".to_owned())
}

pub async fn purge_messages(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    ctx.require_in(channel_id, Permissions::MANAGE_MESSAGES)?;
    let everything = arg(args, "count").is_some_and(|c| c.trim().eq_ignore_ascii_case("all"))
        || flag(args, "all") == Some(true);
    let limit = if everything {
        10_000
    } else {
        number(args, "count").unwrap_or(10).clamp(1, 1000) as usize
    };
    let user = match arg(args, "user") {
        Some(user) => Some(resolve_member(ctx, &user).await?),
        None => None,
    };
    if let Some(target) = &user {
        check_target(ctx, target)?;
    }
    let channel = ChannelId::new(channel_id);
    // Bulk delete only takes messages under 14 days old (with a margin).
    let cutoff = Timestamp::now_utc().as_unix_millis() - (14 * 24 * 3600 - 600) * 1000;
    let my_id = ctx.core.world.my_id();
    let request = ctx.message_id;
    let (mut recent, mut old) = (Vec::new(), Vec::new());
    let mut before: Option<MessageId> = None;
    let mut scanned = 0usize;
    let mut more = false;
    // Page back through history, 100 at a time.
    while scanned < limit {
        let page = (limit - scanned).min(100);
        let mut fetch = ctx.core.http.get_messages(channel).limit(page as u8);
        if let Some(cursor) = before {
            fetch = fetch.before(cursor);
        }
        let messages = fetch.await.map_err(discord_error)?;
        scanned += messages.len();
        before = messages.iter().map(|m| m.id).min();
        for m in &messages {
            if user
                .as_ref()
                .is_some_and(|u| m.author.id.get() != u.user_id)
            {
                continue;
            }
            if user.is_none() {
                // Keep the request (the answer replies to it) and this
                // reply's own thinking message.
                if Some(m.id.get()) == request {
                    continue;
                }
                if m.author.id.get() == my_id && request.is_some_and(|r| m.id.get() > r) {
                    continue;
                }
            }
            if m.id.created_at().as_unix_millis() > cutoff {
                recent.push(m.id);
            } else {
                old.push(m.id);
            }
        }
        if messages.len() < page {
            more = false;
            break;
        }
        more = true;
    }

    let mut deleted = 0usize;
    for batch in recent.chunks(100) {
        match batch {
            [one] => ctx
                .core
                .http
                .delete_message(channel, *one)
                .await
                .map_err(discord_error)?,
            _ => ctx
                .core
                .http
                .bulk_delete_messages(channel, batch.to_vec())
                .await
                .map_err(discord_error)?,
        }
        deleted += batch.len();
    }

    // Older messages must go one at a time; Discord paces those slowly,
    // so they finish in the background.
    let background = old.len();
    if background > 0 {
        let core = std::sync::Arc::clone(&ctx.core);
        let label = channel_name(ctx, channel_id);
        tokio::spawn(async move {
            let mut done = 0usize;
            for id in old {
                if core.http.delete_message(channel, id).await.is_ok() {
                    done += 1;
                }
            }
            core.feed(
                FeedKind::Moderation,
                format!("finished deleting {done} older messages"),
                Some(label),
            );
        });
    }

    let reason = arg(args, "reason").unwrap_or_else(|| "cleanup".to_owned());
    let total = deleted + background;
    if let Some(target) = &user {
        record_mod_action(
            ctx,
            target,
            &format!("purged {total} messages from"),
            &reason,
        );
    } else {
        ctx.core.feed(
            FeedKind::Moderation,
            format!("purged {total} messages — {reason}"),
            Some(channel_name(ctx, channel_id)),
        );
    }
    if total == 0 {
        return Ok("nothing to delete — no matching messages".to_owned());
    }
    let mut out = format!(
        "deleted {deleted} message(s) in #{}",
        channel_name(ctx, channel_id)
    );
    if background > 0 {
        out.push_str(&format!(
            "; {background} more are older than 14 days, which Discord only lets bots delete one by one — those are being deleted in the background now (roughly {} s)",
            background.div_ceil(2)
        ));
    }
    if more {
        out.push_str(&format!(
            "; stopped after scanning {scanned} messages — run it again to keep going"
        ));
    }
    Ok(out)
}

pub async fn warn_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    check_target(ctx, &target)?;
    let reason = required(args, "reason")?;
    let strikes = add_strike(ctx, guild_id, &target, &reason);
    ctx.core
        .http
        .create_message(ChannelId::new(ctx.channel_id))
        .content(format!(
            "⚠️ <@{}>, this is a warning: {reason}\n-# strike {strikes} in the last 30 days",
            target.user_id
        ))
        .allowed_mentions(AllowedMentions::users())
        .await
        .map_err(discord_error)?;
    record_mod_action(ctx, &target, "warned", &reason);
    Ok(format!(
        "warned {} (strike {strikes} in 30 days)",
        target.display_name()
    ))
}

pub fn add_strike(ctx: &ToolCtx, guild_id: u64, target: &MemberInfo, reason: &str) -> usize {
    let by = if ctx.autonomous {
        ctx.core.name()
    } else {
        ctx.requester.name.clone()
    };
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let note = k.people.entry(target.user_id).or_default();
        note.name = target.display_name().to_owned();
        note.strikes.push(Strike {
            at: now_ms(),
            reason: reason.to_owned(),
            by,
        });
        note.recent_strikes(30 * 24 * 3600 * 1000)
    })
}

pub async fn timeout_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    check_target(ctx, &target)?;
    let minutes = number(args, "minutes").unwrap_or(10).clamp(0, 40_320);
    if ctx.autonomous && minutes > 60 {
        return Err(
            "on my own judgement I only time out for up to 60 minutes; ask a moderator for longer"
                .to_owned(),
        );
    }
    let reason = arg(args, "reason");
    let request = ctx
        .core
        .http
        .edit_member(GuildId::new(guild_id), UserId::new(target.user_id))
        .reason(ctx.reason(reason.clone()));
    if minutes == 0 {
        request.clear_timeout().await.map_err(discord_error)?;
        record_mod_action(
            ctx,
            &target,
            "lifted the timeout of",
            &reason.unwrap_or_default(),
        );
        return Ok(format!("lifted {}'s timeout", target.display_name()));
    }
    let until =
        Timestamp::from_unix_millis(Timestamp::now_utc().as_unix_millis() + minutes * 60_000)
            .map_err(|e| e.to_string())?;
    request.timeout_until(until).await.map_err(discord_error)?;
    record_mod_action(
        ctx,
        &target,
        &format!("timed out ({minutes}m)"),
        &reason.unwrap_or_else(|| "no reason".to_owned()),
    );
    Ok(format!(
        "timed out {} for {minutes} minutes",
        target.display_name()
    ))
}

pub async fn kick_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    check_target(ctx, &target)?;
    let reason = required(args, "reason")?;
    ctx.core
        .http
        .kick_member(GuildId::new(guild_id), UserId::new(target.user_id))
        .reason(ctx.reason(Some(reason.clone())))
        .await
        .map_err(discord_error)?;
    record_mod_action(ctx, &target, "kicked", &reason);
    Ok(format!("kicked {}", target.display_name()))
}

pub async fn ban_member(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let query = required(args, "user")?;
    let reason = arg(args, "reason").unwrap_or_else(|| "no reason given".to_owned());
    if flag(args, "unban").unwrap_or(false) {
        let user_id = parse_snowflake(&query).ok_or("to unban, give the user's ID")?;
        ctx.core
            .http
            .unban_member(GuildId::new(guild_id), UserId::new(user_id))
            .await
            .map_err(discord_error)?;
        ctx.core.feed(
            FeedKind::Moderation,
            format!("unbanned {user_id} — {reason}"),
            None,
        );
        return Ok(format!("unbanned {user_id}"));
    }
    let (user_id, label) = match resolve_member(ctx, &query).await {
        Ok(target) => {
            check_target(ctx, &target)?;
            record_mod_action(ctx, &target, "banned", &reason);
            (target.user_id, target.display_name().to_owned())
        }
        Err(_) => {
            let id =
                parse_snowflake(&query).ok_or_else(|| format!("no member matches \"{query}\""))?;
            (id, id.to_string())
        }
    };
    let hours = number(args, "delete_message_hours")
        .unwrap_or(0)
        .clamp(0, 168) as u32;
    let mut request = ctx
        .core
        .http
        .ban_member(GuildId::new(guild_id), UserId::new(user_id))
        .reason(ctx.reason(Some(reason)));
    if hours > 0 {
        request = request.delete_message_seconds(hours * 3600);
    }
    request.await.map_err(discord_error)?;
    Ok(format!("banned {label}"))
}

pub async fn set_slowmode(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    ctx.require_in(channel_id, Permissions::MANAGE_CHANNELS)?;
    let seconds = number(args, "seconds").unwrap_or(0).clamp(0, 21_600) as u32;
    ctx.core
        .http
        .edit_channel(ChannelId::new(channel_id))
        .rate_limit_per_user(seconds)
        .reason(ctx.reason(None))
        .await
        .map_err(discord_error)?;
    Ok(if seconds == 0 {
        "slowmode off".to_owned()
    } else {
        format!("slowmode set to {seconds}s")
    })
}

pub async fn lock_channel(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    ctx.require_in(
        channel_id,
        Permissions::MANAGE_ROLES | Permissions::MANAGE_CHANNELS,
    )?;
    let locked = flag(args, "locked").unwrap_or(true);
    let channel = ctx
        .core
        .world
        .channel(guild_id, channel_id)
        .ok_or("unknown channel")?;
    let existing = channel
        .permission_overwrites
        .iter()
        .flatten()
        .find(|o| o.r#type == OverwriteType::Role && o.id.get() == guild_id)
        .cloned();
    let (allow, deny) = existing
        .map(|o| (o.allow, o.deny))
        .unwrap_or((Permissions::empty(), Permissions::empty()));
    let talk = Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::ADD_REACTIONS;
    let (allow, deny) = if locked {
        (allow & !talk, deny | talk)
    } else {
        (allow, deny & !talk)
    };
    ctx.core
        .http
        .edit_channel_permission(
            ChannelId::new(channel_id),
            GenericId::new(guild_id),
            OverwriteType::Role,
        )
        .allow(allow)
        .deny(deny)
        .await
        .map_err(discord_error)?;
    let reason = arg(args, "reason").unwrap_or_default();
    ctx.core.feed(
        FeedKind::Moderation,
        format!(
            "{} #{} {reason}",
            if locked { "locked" } else { "unlocked" },
            channel_name(ctx, channel_id)
        ),
        None,
    );
    Ok(format!(
        "#{} is now {}",
        channel_name(ctx, channel_id),
        if locked { "locked" } else { "unlocked" }
    ))
}

pub async fn get_strikes(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    let (strikes, actions) = ctx.core.hive.knowledge.guild(guild_id, |k| {
        let strikes: Vec<String> = k
            .people
            .get(&target.user_id)
            .map(|p| {
                p.strikes
                    .iter()
                    .rev()
                    .take(15)
                    .map(|s| {
                        format!(
                            "- {} by {}: {}",
                            format_time(
                                Timestamp::from_unix_millis(s.at)
                                    .unwrap_or_else(|_| Timestamp::now_utc())
                            ),
                            s.by,
                            s.reason
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let actions: Vec<String> = k
            .mod_log
            .iter()
            .rev()
            .filter(|a| a.user_id == target.user_id)
            .take(15)
            .map(|a| format!("- {} {} — {}", a.agent, a.action, a.reason))
            .collect();
        (strikes, actions)
    });
    Ok(format!(
        "{}: {} strike(s)\n{}\nActions:\n{}",
        target.display_name(),
        strikes.len(),
        strikes.join("\n"),
        if actions.is_empty() {
            "none".to_owned()
        } else {
            actions.join("\n")
        }
    ))
}

// --- roles ------------------------------------------------------------------------

pub async fn change_role(ctx: &ToolCtx, args: &Value, add: bool) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let target = resolve_member(ctx, &required(args, "user")?).await?;
    let role_query = required(args, "role")?;
    let role = ctx
        .core
        .world
        .with_guild(guild_id, |g| g.find_role(&role_query).cloned())
        .flatten()
        .ok_or_else(|| format!("no role matches \"{role_query}\""))?;
    let my_id = ctx.core.world.my_id();
    ctx.core
        .world
        .with_guild(guild_id, |g| {
            if !ctx.acting_as_agent()
                && ctx.requester.kind == PrincipalKind::Human
                && !authority::can_manage_role(g, ctx.requester.user_id, &ctx.requester.roles, role.id.get())
            {
                return Err(format!("denied: {} is above {}'s highest role", role.name, ctx.requester.name));
            }
            let my_roles = g.members.get(&my_id).map(|m| m.roles.clone()).unwrap_or_default();
            if !authority::can_manage_role(g, my_id, &my_roles, role.id.get()) {
                return Err(format!("{} is above my highest role (or managed by an integration), so I can't assign it", role.name));
            }
            Ok(())
        })
        .unwrap_or(Ok(()))?;
    if role.id.get() == guild_id {
        return Err("everyone already has @everyone".to_owned());
    }
    let mut roles: Vec<RoleId> = target.roles.iter().map(|r| RoleId::new(*r)).collect();
    if add {
        if roles.contains(&role.id) {
            return Ok(format!(
                "{} already has {}",
                target.display_name(),
                role.name
            ));
        }
        roles.push(role.id);
    } else {
        roles.retain(|r| *r != role.id);
    }
    let updated = ctx
        .core
        .http
        .edit_member(GuildId::new(guild_id), UserId::new(target.user_id))
        .roles(roles)
        .reason(ctx.reason(None))
        .await
        .map_err(discord_error)?;
    ctx.core.world.upsert_member(guild_id, &updated);
    Ok(format!(
        "{} {} {}",
        if add { "gave" } else { "removed" },
        role.name,
        if add {
            format!("to {}", target.display_name())
        } else {
            format!("from {}", target.display_name())
        }
    ))
}

pub async fn create_role(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let name = required(args, "name")?;
    let mut request = ctx
        .core
        .http
        .create_role(GuildId::new(guild_id))
        .name(name.clone());
    if let Some(color) =
        arg(args, "color").and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok())
    {
        request = request.color(color);
    }
    if let Some(mentionable) = flag(args, "mentionable") {
        request = request.mentionable(mentionable);
    }
    if let Some(hoist) = flag(args, "hoist") {
        request = request.hoist(hoist);
    }
    let role = request.await.map_err(discord_error)?;
    ctx.core.world.upsert_role(guild_id, &role);
    Ok(format!("created role {} (ID {})", role.name, role.id))
}

// --- channels ---------------------------------------------------------------------

pub async fn create_channel(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let name = required(args, "name")?;
    let kind = match arg(args, "kind")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "voice" | "vc" => ChannelType::GuildVoice,
        "category" => ChannelType::GuildCategory,
        "forum" => ChannelType::GuildForum,
        "announcement" | "news" => ChannelType::GuildAnnouncement,
        "stage" => ChannelType::GuildStageVoice,
        _ => ChannelType::GuildText,
    };
    let mut request = ctx
        .core
        .http
        .create_guild_channel(GuildId::new(guild_id), name)
        .kind(kind)
        .reason(ctx.reason(None));
    if let Some(category) = arg(args, "category") {
        let parent = resolve_channel(ctx, Some(category))?;
        request = request.parent_id(ChannelId::new(parent));
    }
    if let Some(topic) = arg(args, "topic") {
        request = request.topic(topic);
    }
    if flag(args, "private").unwrap_or(false) {
        let mut overwrites = vec![
            PermissionOverwrite {
                id: GenericId::new(guild_id),
                r#type: OverwriteType::Role,
                allow: Permissions::empty(),
                deny: Permissions::VIEW_CHANNEL,
            },
            PermissionOverwrite {
                id: GenericId::new(ctx.core.world.my_id()),
                r#type: OverwriteType::Member,
                allow: Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
                deny: Permissions::empty(),
            },
        ];
        if ctx.requester.kind == PrincipalKind::Human && ctx.requester.user_id != 0 {
            overwrites.push(PermissionOverwrite {
                id: GenericId::new(ctx.requester.user_id),
                r#type: OverwriteType::Member,
                allow: Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
                deny: Permissions::empty(),
            });
        }
        request = request.permission_overwrites(overwrites);
    }
    let channel = request.await.map_err(discord_error)?;
    ctx.core.world.upsert_channel(&channel);
    Ok(format!(
        "created <#{}> ({})",
        channel.id,
        World::channel_kind_label(channel.r#type)
    ))
}

pub async fn edit_channel(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, arg(args, "channel"))?;
    ctx.require_in(channel_id, Permissions::MANAGE_CHANNELS)?;
    let mut request = ctx
        .core
        .http
        .edit_channel(ChannelId::new(channel_id))
        .reason(ctx.reason(None));
    let name = arg(args, "name");
    let topic = arg(args, "topic");
    if name.is_none() && topic.is_none() {
        return Err("nothing to change — give a name or topic".to_owned());
    }
    if let Some(name) = name {
        request = request.name(name);
    }
    if let Some(topic) = topic {
        request = request.topic(topic);
    }
    let channel = request.await.map_err(discord_error)?;
    ctx.core.world.upsert_channel(&channel);
    Ok(format!("updated <#{}>", channel.id))
}

pub async fn delete_channel(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = resolve_channel(ctx, Some(required(args, "channel")?))?;
    ctx.require_in(channel_id, Permissions::MANAGE_CHANNELS)?;
    if ctx.autonomous {
        return Err("I won't delete channels on my own — a person has to ask".to_owned());
    }
    let name = channel_name(ctx, channel_id);
    let channel = ctx
        .core
        .http
        .delete_channel(ChannelId::new(channel_id))
        .await
        .map_err(discord_error)?;
    ctx.core.world.remove_channel(&channel);
    ctx.core
        .feed(FeedKind::Moderation, format!("deleted #{name}"), None);
    Ok(format!("deleted #{name}"))
}

pub async fn set_nickname(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let who = required(args, "user")?;
    let nick = arg(args, "nick").unwrap_or_default();
    let nick: String = nick.chars().take(32).collect();
    if matches!(
        who.to_lowercase().as_str(),
        "me-bot" | "yourself" | "self" | "bot"
    ) {
        if !ctx.acting_as_agent()
            && !ctx
                .requester
                .permissions
                .contains(Permissions::MANAGE_NICKNAMES)
        {
            return Err(format!(
                "denied: {} can't change my nickname",
                ctx.requester.name
            ));
        }
        let request = ctx.core.http.edit_current_member(GuildId::new(guild_id));
        let request = if nick.is_empty() {
            request.clear_nick()
        } else {
            request.nick(nick.clone())
        };
        request.await.map_err(discord_error)?;
        return Ok("updated my nickname".to_owned());
    }
    let target = resolve_member(ctx, &who).await?;
    let is_self = target.user_id == ctx.requester.user_id && !ctx.acting_as_agent();
    if is_self {
        if !ctx
            .requester
            .permissions
            .intersects(Permissions::CHANGE_NICKNAME | Permissions::MANAGE_NICKNAMES)
        {
            return Err("denied: you can't change your nickname here".to_owned());
        }
    } else {
        if !ctx.acting_as_agent()
            && !ctx
                .requester
                .permissions
                .contains(Permissions::MANAGE_NICKNAMES)
        {
            return Err(format!(
                "denied: {} lacks Manage Nicknames",
                ctx.requester.name
            ));
        }
        check_target(ctx, &target)?;
    }
    let request = ctx
        .core
        .http
        .edit_member(GuildId::new(guild_id), UserId::new(target.user_id))
        .reason(ctx.reason(None));
    let request = if nick.is_empty() {
        request.clear_nick()
    } else {
        request.nick(nick)
    };
    let updated = request.await.map_err(discord_error)?;
    ctx.core.world.upsert_member(guild_id, &updated);
    Ok(format!("updated {}'s nickname", target.display_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emojis_parse() {
        let custom = parse_emoji("<:pepe:123456>");
        assert_eq!(custom.id.map(|i| i.get()), Some(123_456));
        assert_eq!(custom.name.as_deref(), Some("pepe"));
        let animated = parse_emoji("<a:dance:42>");
        assert_eq!(animated.animated, Some(true));
        let unicode = parse_emoji("👍");
        assert_eq!(unicode.id, None);
        assert_eq!(unicode.name.as_deref(), Some("👍"));
    }
}
