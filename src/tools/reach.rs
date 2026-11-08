//! Working across every server the agent is in — from a DM or from
//! another server: finding people (and flagging look-alike names),
//! messaging them or a channel elsewhere, and searching audit logs.

use std::collections::BTreeMap;

use axis::model::AllowedMentions;
use axis::prelude::*;
use serde_json::Value;

use super::{ToolCtx, arg, discord_error, number, required};
use crate::config::AccessLevel;
use crate::runtime::authority::{Principal, PrincipalKind};
use crate::runtime::world::parse_snowflake;

fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Servers whose name matches (all of them for an empty query).
pub fn find_guilds(ctx: &ToolCtx, query: Option<&str>) -> Vec<(u64, String)> {
    let all: Vec<(u64, String)> = ctx
        .core
        .world
        .guild_ids()
        .into_iter()
        .filter_map(|g| ctx.core.world.with_guild(g, |s| (g, s.name.clone())))
        .collect();
    let Some(query) = query
        .map(squash)
        .filter(|q| !q.is_empty() && q != "here" && q != "this")
    else {
        return all;
    };
    let exact: Vec<_> = all
        .iter()
        .filter(|(_, n)| squash(n) == query)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let close: Vec<_> = all
        .iter()
        .filter(|(_, n)| {
            let n = squash(n);
            n.contains(&query)
                || query.contains(&n)
                || crate::runtime::voice::edit_distance(&n, &query) <= 2
        })
        .cloned()
        .collect();
    close
}

/// Which privacy switch a cross-server request needs.
#[derive(Clone, Copy)]
pub enum Reach {
    MemberSearch,
    Messages,
    Audit,
    Dms,
}

/// May a request from here (another server or a DM) do this in that
/// server? Requests inside the server itself always may.
pub fn reach_allowed(ctx: &ToolCtx, guild_id: u64, reach: Reach) -> bool {
    if ctx.guild_id == Some(guild_id) || ctx.requester.kind == PrincipalKind::Operator {
        return true;
    }
    let p = ctx
        .core
        .hive
        .knowledge
        .guild(guild_id, |k| k.privacy.clone());
    match reach {
        Reach::MemberSearch => p.member_search,
        Reach::Messages => p.outside_messages,
        Reach::Audit => p.audit_search,
        Reach::Dms => p.dms,
    }
}

/// Keep only servers that allow this from outside; say which were left out.
fn allowed(
    ctx: &ToolCtx,
    servers: Vec<(u64, String)>,
    reach: Reach,
) -> (Vec<(u64, String)>, Vec<String>) {
    let mut ok = Vec::new();
    let mut private = Vec::new();
    for (id, name) in servers {
        if reach_allowed(ctx, id, reach) {
            ok.push((id, name));
        } else {
            private.push(name);
        }
    }
    (ok, private)
}

/// Log in the HQs of other servers a request touched (they're told when
/// something reaches into them from outside).
fn log_outside(ctx: &ToolCtx, servers: &[(u64, String)], text: &str) {
    for (guild_id, _) in servers {
        if ctx.guild_id != Some(*guild_id) {
            crate::runtime::activity::log_outside(&ctx.core, *guild_id, text);
        }
    }
}

fn origin(ctx: &ToolCtx) -> String {
    match ctx.guild_id {
        Some(g) => format!(
            "{} from {}",
            ctx.requester.name,
            ctx.core
                .world
                .with_guild(g, |s| s.name.clone())
                .unwrap_or_else(|| "another server".to_owned())
        ),
        None => format!("{} in a DM", ctx.requester.name),
    }
}

/// The requester as a member of another server (their rank there).
pub fn principal_in(ctx: &ToolCtx, guild_id: u64) -> Principal {
    if ctx.requester.kind == PrincipalKind::Operator {
        return ctx.requester.clone();
    }
    ctx.core
        .world
        .with_guild(guild_id, |g| match g.members.get(&ctx.requester.user_id) {
            Some(member) => Principal::for_member(g, member, ctx.requester.kind),
            None => Principal::unknown_member(g, ctx.requester.user_id, ctx.requester.name.clone()),
        })
        .unwrap_or_else(|| ctx.requester.clone())
}

/// A person found in a server.
#[derive(Debug, Clone)]
pub struct Found {
    pub user_id: u64,
    pub username: String,
    pub display: String,
    pub servers: Vec<String>,
    pub score: u32,
    pub bot: bool,
}

fn score(query: &str, names: &[&str]) -> u32 {
    let q = squash(query);
    if q.is_empty() {
        return 0;
    }
    let mut best = 0;
    for name in names {
        let n = squash(name);
        if n.is_empty() {
            continue;
        }
        let s = if n == q {
            100
        } else if n.starts_with(&q) || q.starts_with(&n) && n.len() >= 4 {
            75
        } else if n.contains(&q) {
            60
        } else {
            let d = crate::runtime::voice::edit_distance(&n, &q);
            if d <= 1 && q.len() >= 4 {
                55
            } else if d <= 2 && q.len() >= 5 {
                40
            } else {
                0
            }
        };
        best = best.max(s);
    }
    best
}

/// Everyone matching a name across the agent's servers (or some of them),
/// best first. Asks Discord too, for members not cached.
pub async fn search_people(ctx: &ToolCtx, query: &str, servers: &[(u64, String)]) -> Vec<Found> {
    let mut found: BTreeMap<u64, Found> = BTreeMap::new();
    if let Some(id) = parse_snowflake(query) {
        for (guild_id, name) in servers {
            if let Some(m) = ctx.core.world.member(*guild_id, id) {
                found.entry(id).or_insert_with(|| Found {
                    user_id: id,
                    username: m.username.clone(),
                    display: m.display_name().to_owned(),
                    servers: vec![name.clone()],
                    score: 100,
                    bot: m.bot,
                });
            }
        }
    }
    let mut add = |user_id: u64,
                   username: &str,
                   display: &str,
                   nick: Option<&str>,
                   server: &str,
                   bot: bool| {
        let s = score(query, &[username, display, nick.unwrap_or("")]);
        if s == 0 {
            return;
        }
        let entry = found.entry(user_id).or_insert_with(|| Found {
            user_id,
            username: username.to_owned(),
            display: display.to_owned(),
            servers: Vec::new(),
            score: 0,
            bot,
        });
        entry.score = entry.score.max(s);
        if !entry.servers.iter().any(|x| x == server) {
            entry.servers.push(server.to_owned());
        }
    };
    for (guild_id, server) in servers {
        let cached: Vec<(u64, String, String, Option<String>, bool)> = ctx
            .core
            .world
            .with_guild(*guild_id, |g| {
                g.members
                    .values()
                    .map(|m| {
                        (
                            m.user_id,
                            m.username.clone(),
                            m.global_name.clone().unwrap_or_default(),
                            m.nick.clone(),
                            m.bot,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (id, username, global, nick, bot) in &cached {
            add(*id, username, global, nick.as_deref(), server, *bot);
        }
        // Discord's own search finds members we haven't cached.
        if let Ok(members) = ctx
            .core
            .http
            .search_members(GuildId::new(*guild_id), query.trim_start_matches('@'))
            .limit(10)
            .await
        {
            for member in members {
                ctx.core.world.upsert_member(*guild_id, &member);
                if let Some(user) = &member.user {
                    let global = user.global_name.clone().unwrap_or_default();
                    add(
                        user.id.get(),
                        &user.username,
                        &global,
                        member.nick.as_deref(),
                        server,
                        user.bot.unwrap_or(false),
                    );
                }
            }
        }
    }
    let mut list: Vec<Found> = found.into_values().collect();
    list.sort_by(|a, b| b.score.cmp(&a.score).then(a.username.cmp(&b.username)));
    list
}

fn describe(f: &Found) -> String {
    let display =
        (!f.display.trim().is_empty() && f.display != f.username).then_some(f.display.as_str());
    format!(
        "{}{} — in {}",
        super::person_label(None, display, &f.username, f.user_id),
        if f.bot { " [bot]" } else { "" },
        f.servers.join(", ")
    )
}

pub async fn find_people(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let query = required(args, "name")?;
    let servers = find_guilds(ctx, arg(args, "server").as_deref());
    if servers.is_empty() {
        return Err(format!(
            "I'm not in a server called \"{}\"",
            arg(args, "server").unwrap_or_default()
        ));
    }
    let (servers, private) = allowed(ctx, servers, Reach::MemberSearch);
    if servers.is_empty() {
        return Err(format!(
            "{} keep their member list private from outside searches",
            private.join(", ")
        ));
    }
    let found = search_people(ctx, &query, &servers).await;
    let touched: Vec<(u64, String)> = servers
        .iter()
        .filter(|(_, name)| found.iter().any(|f| f.servers.contains(name)))
        .cloned()
        .collect();
    log_outside(
        ctx,
        &touched,
        &format!("searched members for “{query}” (asked by {})", origin(ctx)),
    );
    if found.is_empty() {
        return Ok(format!(
            "nobody called \"{query}\" in {}",
            servers
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let strong: Vec<&Found> = found.iter().filter(|f| f.score >= 55).collect();
    let mut out = String::new();
    if !private.is_empty() {
        out.push_str(&format!(
            "(Not searched — private from outside: {})\n",
            private.join(", ")
        ));
    }
    if strong.len() > 1 {
        out.push_str("Several people have names like that — say which one you mean:\n");
    }
    for f in found.iter().take(12) {
        out.push_str(&format!("- {}\n", describe(f)));
    }
    Ok(out)
}

/// One person by name for an action: the clear best match, or an error
/// listing look-alikes.
async fn one_person(
    ctx: &ToolCtx,
    query: &str,
    servers: &[(u64, String)],
) -> Result<Found, String> {
    if matches!(query.trim().to_lowercase().as_str(), "me" | "myself") {
        return Ok(Found {
            user_id: ctx.requester.user_id,
            username: ctx.requester.name.clone(),
            display: ctx.requester.name.clone(),
            servers: Vec::new(),
            score: 100,
            bot: false,
        });
    }
    let found = search_people(ctx, query, servers).await;
    let Some(best) = found.first().cloned() else {
        return Err(format!(
            "I couldn't find anyone called \"{query}\" in {}",
            servers
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let rivals: Vec<&Found> = found
        .iter()
        .skip(1)
        .filter(|f| f.score >= best.score.saturating_sub(10) && f.score >= 55)
        .collect();
    if best.score < 55 || !rivals.is_empty() {
        let list: Vec<String> = found
            .iter()
            .take(6)
            .map(|f| format!("- {}", describe(f)))
            .collect();
        return Err(format!(
            "not sure who \"{query}\" is — these are close:\n{}\nAsk which one (or use their @username or ID).",
            list.join("\n")
        ));
    }
    Ok(best)
}

pub async fn send_dm(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let content = required(args, "content")?;
    let query = required(args, "user")?;
    let servers = match arg(args, "server") {
        Some(s) => find_guilds(ctx, Some(&s)),
        None => match ctx.guild_id {
            Some(g) => find_guilds(ctx, None)
                .into_iter()
                .filter(|(id, _)| *id == g)
                .collect(),
            None => find_guilds(ctx, None),
        },
    };
    let (servers, private) = allowed(ctx, servers, Reach::Dms);
    if servers.is_empty() {
        return Err(format!(
            "{} don't allow DMs to their members from outside",
            private.join(", ")
        ));
    }
    // A numbered pick from find_users resolves first (exact ID, no
    // transcription).
    let target = match super::users::resolve(ctx.channel_id, &query, false)? {
        Some(picked) => {
            let user = ctx
                .guild_id
                .and_then(|g| ctx.core.world.member(g, picked[0]));
            Found {
                user_id: picked[0],
                username: user
                    .as_ref()
                    .map(|m| m.username.clone())
                    .unwrap_or_else(|| format!("user {}", picked[0])),
                display: user
                    .and_then(|m| m.nick.clone())
                    .unwrap_or_else(|| format!("user {}", picked[0])),
                servers: servers.iter().map(|(_, n)| n.clone()).collect(),
                score: 100,
                bot: false,
            }
        }
        None => one_person(ctx, &query, &servers).await?,
    };
    // AAv2: DMs are not server actions — no server permission applies.
    // Anyone who shares a server with the target may ask (Discord itself
    // blocks DMs the recipient has closed).
    let is_self = target.user_id == ctx.requester.user_id;
    let shares_server = ctx.requester.kind == PrincipalKind::Operator
        || ctx.acting_as_agent()
        || servers
            .iter()
            .any(|(g, _)| ctx.core.world.member(*g, target.user_id).is_some());
    if !is_self && !shares_server {
        return Err(
            "denied: I can only DM people who share a server with the person asking".to_owned(),
        );
    }
    let dm = ctx
        .core
        .http
        .create_dm(UserId::new(target.user_id))
        .await
        .map_err(discord_error)?;
    let text = if is_self || ctx.acting_as_agent() {
        content
    } else {
        format!(
            "{content}\n-# sent at the request of {}",
            ctx.requester.name
        )
    };
    for chunk in crate::runtime::brain::split_message(&text) {
        ctx.core
            .http
            .create_message(dm.id)
            .content(chunk)
            .allowed_mentions(AllowedMentions::none())
            .await
            .map_err(|e| {
                let text = discord_error(e);
                format!("{text} (they may have DMs from server members turned off)")
            })?;
    }
    // Logged where they're a member (and where the requester is, if
    // it came from a DM).
    let note = format!("DMed {} (asked by {})", target.display, origin(ctx));
    let theirs: Vec<(u64, String)> = servers
        .iter()
        .filter(|(g, _)| ctx.core.world.member(*g, target.user_id).is_some())
        .cloned()
        .collect();
    for (guild, _) in &theirs {
        if ctx.guild_id == Some(*guild) {
            crate::runtime::activity::log(&ctx.core, *guild, &note);
        }
    }
    log_outside(ctx, &theirs, &note);
    Ok(format!("sent {} a DM", describe(&target)))
}

/// Post in a channel of any server the agent is in.
pub async fn send_to_server(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let content = required(args, "content")?;
    let servers = find_guilds(ctx, Some(&required(args, "server")?));
    let (guild_id, server) = match servers.as_slice() {
        [one] => one.clone(),
        [] => return Err("I'm not in a server by that name".to_owned()),
        many => {
            return Err(format!(
                "several servers match: {} — which one?",
                many.iter()
                    .map(|(_, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    };
    if !reach_allowed(ctx, guild_id, Reach::Messages) {
        return Err(format!(
            "{server} doesn't allow messages from outside the server"
        ));
    }
    let query = required(args, "channel")?;
    let channel_id = ctx
        .core
        .world
        .with_guild(guild_id, |g| {
            g.find_channel_where(&query, |c| {
                !c.is_voice() || c.r#type == axis::model::ChannelType::GuildVoice
            })
            .map(|c| c.id.get())
        })
        .flatten()
        .ok_or_else(|| format!("no channel \"{query}\" in {server}"))?;
    if let Some(rule) = ctx.core.barred(guild_id, channel_id) {
        return Err(format!(
            "not sent: I never chat in #{} on {server} ({} set that rule)",
            query.trim_start_matches('#'),
            rule.setter_name
        ));
    }
    // The requester's own permissions in that server apply.
    let who = principal_in(ctx, guild_id);
    let allowed = who.kind == PrincipalKind::Operator
        || ctx.acting_as_agent()
        || ctx
            .core
            .world
            .with_guild(guild_id, |g| {
                who.channel_permissions(Some(g), channel_id)
                    .contains(Permissions::SEND_MESSAGES)
            })
            .unwrap_or(false);
    if !allowed {
        return Err(format!(
            "denied: {} can't post in #{} on {server}",
            ctx.requester.name,
            query.trim_start_matches('#')
        ));
    }
    let sent = ctx
        .core
        .http
        .create_message(ChannelId::new(channel_id))
        .content(crate::llm::truncate(&content, 1990))
        .allowed_mentions(AllowedMentions::none())
        .await
        .map_err(discord_error)?;
    let note = format!("posted in <#{channel_id}> (asked by {})", origin(ctx));
    if ctx.guild_id == Some(guild_id) {
        crate::runtime::activity::log(&ctx.core, guild_id, &note);
    } else {
        crate::runtime::activity::log_outside(&ctx.core, guild_id, &note);
    }
    Ok(format!(
        "posted in #{} on {server} (message {})",
        query.trim_start_matches('#'),
        sent.id
    ))
}

fn action_name(code: u64) -> &'static str {
    match code {
        1 => "server updated",
        10 => "channel created",
        11 => "channel updated",
        12 => "channel deleted",
        13 => "permission overwrite created",
        14 => "permission overwrite updated",
        15 => "permission overwrite deleted",
        20 => "member kicked",
        21 => "members pruned",
        22 => "member banned",
        23 => "member unbanned",
        24 => "member updated (nickname/timeout/mute)",
        25 => "member roles changed",
        26 => "member moved in voice",
        27 => "member disconnected from voice",
        28 => "bot added",
        30 => "role created",
        31 => "role updated",
        32 => "role deleted",
        40 => "invite created",
        41 => "invite updated",
        42 => "invite deleted",
        50 => "webhook created",
        51 => "webhook updated",
        52 => "webhook deleted",
        60 => "emoji created",
        61 => "emoji updated",
        62 => "emoji deleted",
        72 => "message deleted",
        73 => "messages bulk-deleted",
        74 => "message pinned",
        75 => "message unpinned",
        80 => "integration created",
        81 => "integration updated",
        82 => "integration deleted",
        100 => "event created",
        101 => "event updated",
        102 => "event deleted",
        110 => "thread created",
        111 => "thread updated",
        112 => "thread deleted",
        121 => "command permissions updated",
        140 => "automod rule created",
        141 => "automod rule updated",
        142 => "automod rule deleted",
        143 => "automod blocked a message",
        144 => "automod flagged a message",
        145 => "automod timed someone out",
        192 => "voice channel status set",
        193 => "voice channel status cleared",
        _ => "other change",
    }
}

fn action_code(name: &str) -> Option<u64> {
    let n = name.to_lowercase();
    // Most specific first ("unban" before "ban", "role created" before "role").
    const PAIRS: [(&str, u64); 34] = [
        ("unban", 23),
        ("ban", 22),
        ("kick", 20),
        ("prune", 21),
        ("timeout", 24),
        ("timed out", 24),
        ("mute", 24),
        ("nick", 24),
        ("rename", 24),
        ("role creat", 30),
        ("new role", 30),
        ("role delet", 32),
        ("role updat", 31),
        ("role edit", 31),
        ("role", 25),
        ("bulk", 73),
        ("purge", 73),
        ("message delet", 72),
        ("deleted message", 72),
        ("channel creat", 10),
        ("new channel", 10),
        ("channel delet", 12),
        ("channel", 11),
        ("permission", 14),
        ("invite", 40),
        ("emoji", 60),
        ("sticker", 90),
        ("webhook", 50),
        ("bot", 28),
        ("thread", 110),
        ("unpin", 75),
        ("pin", 74),
        ("event", 100),
        ("automod", 143),
    ];
    PAIRS.iter().find(|(k, _)| n.contains(k)).map(|(_, c)| *c)
}

fn ago(ms: i64) -> String {
    let mins = (crate::memory::now_ms() as i64 - ms).max(0) / 60_000;
    match mins {
        0 => "just now".to_owned(),
        1..=59 => format!("{mins}m ago"),
        60..=1439 => format!("{}h ago", mins / 60),
        _ => format!("{}d ago", mins / 1440),
    }
}

/// One change of an audit entry, readable ("added role Mod",
/// "nick: bob → bobby").
fn describe_change(change: &Value) -> String {
    let key = change.get("key").and_then(Value::as_str).unwrap_or("?");
    let names = |v: Option<&Value>| -> String {
        v.and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let plain = |v: Option<&Value>| -> String {
        match v {
            None | Some(Value::Null) => "∅".to_owned(),
            Some(Value::String(s)) => crate::llm::truncate(s, 50),
            Some(other) => crate::llm::truncate(&other.to_string(), 50),
        }
    };
    match key {
        "$add" => format!("added role {}", names(change.get("new_value"))),
        "$remove" => format!("removed role {}", names(change.get("new_value"))),
        "communication_disabled_until" => match change.get("new_value").and_then(Value::as_str) {
            Some(until) => format!(
                "timed out until {}",
                until.chars().take(16).collect::<String>().replace('T', " ")
            ),
            None => "timeout removed".to_owned(),
        },
        _ => format!(
            "{key}: {} → {}",
            plain(change.get("old_value")),
            plain(change.get("new_value"))
        ),
    }
}

/// Search audit logs in one or more servers: who did what, to whom, why.
pub async fn search_audit_logs(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    // This server by default; a named one; or every one (from DMs, or
    // when asked).
    let all = find_guilds(ctx, arg(args, "server").as_deref());
    if all.is_empty() {
        return Err(format!(
            "I'm not in a server called \"{}\" — list_servers shows where I am",
            arg(args, "server").unwrap_or_default()
        ));
    }
    let everywhere = super::flag(args, "all_servers").unwrap_or(false);
    let servers: Vec<(u64, String)> = match (arg(args, "server"), ctx.guild_id) {
        (None, Some(g)) if !everywhere => all.into_iter().filter(|(id, _)| *id == g).collect(),
        _ => all,
    };
    let (servers, private) = allowed(ctx, servers, Reach::Audit);
    let text = arg(args, "text").unwrap_or_default().to_lowercase();
    // A kind of action Discord can filter by; otherwise match its name.
    let action_arg = arg(args, "action").filter(|a| {
        !a.trim().is_empty()
            && !matches!(
                a.trim().to_lowercase().as_str(),
                "all" | "any" | "everything"
            )
    });
    let action = action_arg.as_deref().and_then(action_code);
    let action_words: Vec<String> = match (&action_arg, action) {
        (Some(a), None) => a
            .to_lowercase()
            .split_whitespace()
            .map(|w| w.chars().take(5).collect())
            .collect(),
        _ => Vec::new(),
    };
    let target = arg(args, "target").map(|t| squash(t.trim_start_matches('@')));
    let limit = number(args, "limit").unwrap_or(25).clamp(1, 60) as usize;
    let mut lines = Vec::new();
    let mut skipped = Vec::new();
    for (guild_id, server) in &servers {
        let who = principal_in(ctx, *guild_id);
        let can = who.kind == PrincipalKind::Operator
            || who
                .permissions
                .intersects(Permissions::VIEW_AUDIT_LOG | Permissions::ADMINISTRATOR)
            || who.level >= AccessLevel::Admin;
        if !can {
            skipped.push(format!("{server} (you can't view its audit log)"));
            continue;
        }
        // "by": a person Discord can filter on, or else a name to match.
        let mut by_id = None;
        let mut by_name = None;
        if let Some(user) = arg(args, "by") {
            match one_person(ctx, &user, &[(*guild_id, server.clone())]).await {
                Ok(found) => by_id = Some(found.user_id),
                Err(_) => by_name = Some(squash(user.trim_start_matches('@'))),
            }
        }
        let filtering =
            !text.is_empty() || target.is_some() || by_name.is_some() || !action_words.is_empty();
        let mut before: Option<String> = None;
        // Without extra filters one page holds the newest; with them, look
        // further back.
        for _ in 0..if filtering { 5 } else { 1 } {
            let mut path = format!("/guilds/{guild_id}/audit-logs?limit=100");
            if let Some(id) = by_id {
                path.push_str(&format!("&user_id={id}"));
            }
            if let Some(code) = action {
                path.push_str(&format!("&action_type={code}"));
            }
            if let Some(b) = &before {
                path.push_str(&format!("&before={b}"));
            }
            let log = match ctx.core.http.raw("GET", &path, None, None).await {
                Ok(log) => log,
                Err(error) => {
                    if lines.is_empty() && before.is_none() {
                        skipped.push(format!(
                            "{server} (I can't read its audit log: {})",
                            discord_error(error)
                        ));
                    }
                    break;
                }
            };
            let users: BTreeMap<String, String> = log
                .get("users")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|u| {
                    let username = u.get("username").and_then(Value::as_str)?;
                    let label = match u
                        .get("global_name")
                        .and_then(Value::as_str)
                        .filter(|g| *g != username)
                    {
                        Some(global) => format!("{global} (@{username})"),
                        None => format!("@{username}"),
                    };
                    Some((u.get("id")?.as_str()?.to_owned(), label))
                })
                .collect();
            let name = |id: Option<&str>| -> String {
                match id {
                    Some(id) => users
                        .get(id)
                        .cloned()
                        .or_else(|| {
                            id.parse::<u64>().ok().and_then(|i| {
                                ctx.core
                                    .world
                                    .with_guild(*guild_id, |g| {
                                        g.channels
                                            .get(&i)
                                            .and_then(|c| c.name.clone())
                                            .map(|n| format!("#{n}"))
                                            .or_else(|| {
                                                g.roles.get(&i).map(|r| format!("@{}", r.name))
                                            })
                                    })
                                    .flatten()
                            })
                        })
                        .unwrap_or_else(|| id.to_owned()),
                    None => "-".to_owned(),
                }
            };
            let entries: Vec<&Value> = log
                .get("audit_log_entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .collect();
            for entry in &entries {
                let code = entry
                    .get("action_type")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let ms = ((id >> 22) + 1_420_070_400_000) as i64;
                let when = Timestamp::from_unix_millis(ms)
                    .map(|t| {
                        t.to_string()
                            .chars()
                            .take(16)
                            .collect::<String>()
                            .replace('T', " ")
                    })
                    .unwrap_or_default();
                let actor = name(entry.get("user_id").and_then(Value::as_str));
                let mut target_name = name(entry.get("target_id").and_then(Value::as_str));
                let options = entry.get("options");
                if let Some(channel) = options
                    .and_then(|o| o.get("channel_id"))
                    .and_then(Value::as_str)
                {
                    target_name.push_str(&format!(" in {}", name(Some(channel))));
                }
                if let Some(count) = options.and_then(|o| o.get("count")).and_then(Value::as_str) {
                    target_name.push_str(&format!(" (×{count})"));
                }
                let changes: Vec<String> = entry
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(5)
                    .map(describe_change)
                    .collect();
                let line = format!(
                    "{when} UTC ({}) [{server}] {} — by {} — on {}{}{}",
                    ago(ms),
                    action_name(code),
                    actor,
                    target_name,
                    entry
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(|r| format!(" — reason: {r}"))
                        .unwrap_or_default(),
                    if changes.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", changes.join("; "))
                    }
                );
                let keep = (text.is_empty() || line.to_lowercase().contains(&text))
                    && target
                        .as_ref()
                        .is_none_or(|t| squash(&target_name).contains(t.as_str()))
                    && by_name
                        .as_ref()
                        .is_none_or(|b| squash(&actor).contains(b.as_str()))
                    && (action_words.is_empty()
                        || action_words
                            .iter()
                            .any(|w| action_name(code).contains(w.as_str())));
                if keep {
                    lines.push(line);
                }
                if lines.len() >= limit {
                    break;
                }
            }
            before = entries
                .last()
                .and_then(|e| e.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if entries.len() < 100 || lines.len() >= limit || before.is_none() {
                break;
            }
        }
    }
    let searched: Vec<(u64, String)> = servers
        .iter()
        .filter(|(_, n)| !skipped.iter().any(|s| s.starts_with(n.as_str())))
        .cloned()
        .collect();
    log_outside(
        ctx,
        &searched,
        &format!("searched the audit log (asked by {})", origin(ctx)),
    );
    for name in private {
        skipped.push(format!("{name} (private from outside)"));
    }
    let mut out = if lines.is_empty() {
        format!(
            "no matching audit log entries in {}{}",
            searched
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            if action.is_some() || filtering_note(args) {
                " — try fewer filters"
            } else {
                ""
            }
        )
    } else {
        format!(
            "Audit log, newest first ({} entr{}):\n{}",
            lines.len(),
            if lines.len() == 1 { "y" } else { "ies" },
            lines.join("\n")
        )
    };
    if !skipped.is_empty() {
        out.push_str(&format!("\n(Not searched: {})", skipped.join(", ")));
    }
    Ok(out)
}

fn filtering_note(args: &Value) -> bool {
    ["by", "target", "text"]
        .iter()
        .any(|k| arg(args, k).is_some())
}

/// Every member of another server (or this one), straight from Discord,
/// optionally filtered by role or name. Long lists come back as an
/// outline to read in parts.
pub async fn list_server_members(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let servers = find_guilds(ctx, arg(args, "server").as_deref());
    let (guild_id, server) = match servers.as_slice() {
        [one] => one.clone(),
        [] => {
            return Err(format!(
                "I'm not in a server called \"{}\" — list_servers shows where I am",
                arg(args, "server").unwrap_or_default()
            ));
        }
        many => {
            return Err(format!(
                "several servers match: {} — which one?",
                many.iter()
                    .map(|(_, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    };
    if !reach_allowed(ctx, guild_id, Reach::MemberSearch) {
        return Err(format!(
            "{server} keeps its member list private from outside"
        ));
    }
    // Page through the whole list.
    let mut after = 0u64;
    let mut members = Vec::new();
    for _ in 0..20 {
        let page = ctx
            .core
            .http
            .raw(
                "GET",
                &format!("/guilds/{guild_id}/members?limit=1000&after={after}"),
                None,
                None,
            )
            .await
            .map_err(discord_error)?;
        let Value::Array(page) = page else { break };
        let len = page.len();
        for m in page {
            if let Ok(member) = serde_json::from_value::<axis::model::Member>(m) {
                if let Some(user) = &member.user {
                    after = after.max(user.id.get());
                }
                ctx.core.world.upsert_member(guild_id, &member);
                members.push(member);
            }
        }
        if len < 1000 {
            break;
        }
    }
    let role = match arg(args, "role") {
        Some(r) => Some(
            ctx.core
                .world
                .with_guild(guild_id, |g| g.find_role(&r).map(|x| x.id.get()))
                .flatten()
                .ok_or_else(|| format!("no role \"{r}\" in {server}"))?,
        ),
        None => None,
    };
    let query = arg(args, "query").map(|q| squash(&q));
    let role_names = |ids: &[RoleId]| -> String {
        ctx.core
            .world
            .with_guild(guild_id, |g| {
                ids.iter()
                    .filter_map(|r| g.roles.get(&r.get()).map(|x| x.name.clone()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let lines: Vec<String> = members
        .iter()
        .filter(|m| role.is_none_or(|r| m.roles.iter().any(|x| x.get() == r)))
        .filter_map(|m| {
            let user = m.user.as_ref()?;
            let display = m
                .nick
                .clone()
                .or_else(|| user.global_name.clone())
                .unwrap_or_else(|| user.username.clone());
            if let Some(q) = &query {
                if !squash(&display).contains(q.as_str())
                    && !squash(&user.username).contains(q.as_str())
                {
                    return None;
                }
            }
            let roles = role_names(&m.roles);
            Some(format!(
                "- {}{}{}",
                super::person_label(
                    m.nick.as_deref(),
                    user.global_name.as_deref(),
                    &user.username,
                    user.id.get()
                ),
                if user.bot.unwrap_or(false) {
                    " [bot]"
                } else {
                    ""
                },
                if roles.is_empty() {
                    String::new()
                } else {
                    format!(" — {roles}")
                }
            ))
        })
        .collect();
    if ctx.guild_id != Some(guild_id) {
        crate::runtime::activity::log_outside(
            &ctx.core,
            guild_id,
            &format!("listed members (asked by {})", origin(ctx)),
        );
    }
    Ok(format!(
        "{} member(s) in {server}{}:
{}",
        lines.len(),
        if role.is_some() || query.is_some() {
            " matching"
        } else {
            ""
        },
        lines.join(
            "
"
        )
    ))
}

/// The servers the agent is in, for "which servers are you in".
pub async fn list_servers(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    let lines: Vec<String> = find_guilds(ctx, None)
        .into_iter()
        .map(|(id, name)| {
            let members = ctx
                .core
                .world
                .with_guild(id, |g| g.member_count.max(g.members.len() as u64))
                .unwrap_or(0);
            let you = ctx.core.world.member(id, ctx.requester.user_id).is_some();
            format!(
                "- {name} (ID {id}) — {members} members{}",
                if you { ", you're in it" } else { "" }
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        "I'm not in any servers".to_owned()
    } else {
        lines.join("\n")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_actions() {
        assert_eq!(action_code("unban"), Some(23));
        assert_eq!(action_code("bans"), Some(22));
        assert_eq!(action_code("role created"), Some(30));
        assert_eq!(action_code("role added"), Some(25));
        assert_eq!(action_code("channel deleted"), Some(12));
        assert_eq!(action_code("timeouts"), Some(24));
        assert_eq!(action_code("delete"), None);
        let change =
            serde_json::json!({ "key": "$add", "new_value": [{ "name": "Mod", "id": "1" }] });
        assert_eq!(describe_change(&change), "added role Mod");
    }

    #[test]
    fn names_score_by_closeness() {
        assert_eq!(score("susie", &["Susie", "", ""]), 100);
        assert_eq!(score("susie", &["susie_123", "", ""]), 75);
        assert!(score("susie", &["Susy", "", ""]) > 0);
        assert_eq!(score("susie", &["bob", "Robert", ""]), 0);
        assert_eq!(action_code("who banned bob"), Some(22));
    }
}
