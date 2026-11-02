//! Non-Discord tools: memory, directives, quiet mode, standing tasks,
//! coordination between agents, web wrappers, and utilities.

use axis::model::AllowedMentions;
use axis::prelude::{ChannelId, Permissions};

use serde_json::{Value, json};

use super::{
    Group, ToolCtx, ToolDef, arg, discord_error, flag, list, number, prop, required, schema,
};
use crate::config::{AccessLevel, Autonomy};
use crate::memory::{
    Directive, DirectiveScope, Memory, MemoryKind, NoChatRule, QuietRule, StandingTask, TaskTrigger, now_ms,
};
use crate::runtime::authority::PrincipalKind;
use crate::runtime::live::FeedKind;

macro_rules! def {
    ($name:literal, $group:ident, $autonomy:ident, guild: $guild:literal, core: $core:literal,
     keywords: [$($kw:literal),*], $description:literal, { $($pname:literal : ($pkind:literal, $pdesc:literal)),* $(,)? }, required: [$($req:literal),*]) => {
        ToolDef {
            name: $name,
            group: Group::$group,
            description: $description,
            params: schema(json!({ $($pname: prop($pkind, $pdesc)),* }), &[$($req),*]),
            requires: Permissions::empty(),
            autonomy: Autonomy::$autonomy,
            guild_only: $guild,
            core: $core,
            keywords: &[$($kw),*],
        }
    };
}

pub fn definitions() -> Vec<ToolDef> {
    vec![
        // --- memory ----------------------------------------------------------
        def!("remember", Memory, Conversational, guild: false, core: true,
            keywords: ["remember", "don't forget", "note", "my name is", "i like", "i prefer", "call me", "birthday"],
            "Save something worth remembering long-term (a fact, preference, event, or task). Use it whenever you learn something you'd want to know next week.",
            { "text": ("string", "What to remember, as a standalone sentence"), "about_user": ("string", "Who it's about (mention/ID/name), if a person"), "kind": ("string", "fact, preference, event, task, or note"), "importance": ("integer", "1 (trivia) to 5 (critical)") },
            required: ["text"]),
        def!("recall", Memory, Conversational, guild: false, core: true,
            keywords: ["remember", "recall", "what do you know", "did i tell", "last time", "forgot"],
            "Search your long-term memory.", { "query": ("string", "What to look for") }, required: ["query"]),
        def!("forget_memory", Memory, Conversational, guild: false, core: false,
            keywords: ["forget", "delete memory", "remove memory"],
            "Delete a memory by ID (people can delete memories about themselves; moderators any).",
            { "memory_id": ("integer", "The memory's ID (from recall)") }, required: ["memory_id"]),
        def!("note_about_person", Memory, Conversational, guild: true, core: false,
            keywords: ["is a", "works on", "likes", "they are", "she is", "he is"],
            "Update the shared notes all agents see about a person (personality, interests, how to talk to them). Replaces the old notes; keep it under 60 words.",
            { "user": ("string", "Who"), "notes": ("string", "The full updated notes") }, required: ["user", "notes"]),
        def!("set_directive", Memory, Conversational, guild: true, core: false,
            keywords: ["from now on", "always", "never", "rule", "don't ever", "stop doing", "make sure you", "you should"],
            "Record a standing instruction someone gives you. Scope 'me' (how to treat the requester — anyone), 'channel' (moderators), or 'server' (admins). Higher-ranked people's directives override lower ones. (Not for \"never chat/talk in #channel\" — that's stop_chatting_in.)",
            { "text": ("string", "The instruction, phrased as a rule"), "scope": ("string", "me, channel, or server"), "for_all_agents": ("boolean", "Apply to every agent, not just you") },
            required: ["text", "scope"]),
        def!("list_directives", Memory, Conversational, guild: true, core: false,
            keywords: ["rules", "directives", "instructions", "what were you told"],
            "List the standing directives that apply here, with who set them.", {}, required: []),
        def!("revoke_directive", Memory, Conversational, guild: true, core: false,
            keywords: ["cancel", "revoke", "ignore that rule", "you can stop", "remove the rule"],
            "Revoke a directive. Only its author or someone ranked at least as high may do it.",
            { "id": ("integer", "Directive ID") }, required: ["id"]),
        def!("create_command", Memory, Conversational, guild: true, core: false,
            keywords: ["slash command", "command", "commands", "make a command", "create a command", "add a command", "new command", "/"],
            "Make (or change) a slash command in this server. It either replies with fixed text, or has you carry out an instruction whenever someone uses it (does: run). Needs Manage Server. Registered with Discord within ~20s.",
            { "name": ("string", "Command name: lowercase letters, numbers, - or _ (no slash)"), "does": ("string", "reply = post the text as-is; run = you carry out the text as an instruction each time (use run for anything that needs looking up, deciding or doing)"), "text": ("string", "The reply, or the instruction to carry out. {user}, {server}, {channel}, {input} are filled in"), "description": ("string", "Short description shown in Discord"), "takes_input": ("boolean", "Add an optional input box ({input})"), "private": ("boolean", "Only the person who used it sees the answer") },
            required: ["name", "does", "text"]),
        def!("delete_command", Memory, Conversational, guild: true, core: false,
            keywords: ["delete the command", "remove the command", "remove command", "delete command", "get rid of the command"],
            "Delete a slash command made in this server. Needs Manage Server.",
            { "name": ("string", "Command name") }, required: ["name"]),
        def!("toggle_command", Memory, Conversational, guild: true, core: false,
            keywords: ["turn off", "turn on", "disable", "enable", "command", "commands", "slash"],
            "Turn a slash command off or on in this server — a built-in one (/rank, /leaderboard, /ask, …) or one made here. Needs Manage Server.",
            { "name": ("string", "Command name"), "enabled": ("boolean", "true = on, false = off") }, required: ["name", "enabled"]),
        def!("list_commands", Memory, Conversational, guild: true, core: false,
            keywords: ["commands", "slash commands", "what commands", "which commands", "command list"],
            "List your slash commands here: built-in ones (on/off), ones set in the app, and ones made in this server.", {}, required: []),
        def!("wait", Utilities, Conversational, guild: false, core: false,
            keywords: ["every", "seconds", "second", "wait", "pause", "delay", "then", "after", "one by one", "slowly", "apart"],
            "Wait a number of seconds before your next step (for \"every 5 seconds…\", \"wait 10s then…\", pacing a series of actions). Saying \"stop\" cuts it short.",
            { "seconds": ("integer", "How long, 1–120") }, required: ["seconds"]),
        def!("stop_chatting_in", Memory, Conversational, guild: true, core: false,
            keywords: ["never chat", "never talk", "never post", "never speak", "never reply", "never respond", "never message", "don't chat", "dont chat", "don't talk", "dont talk", "don't post", "dont post", "don't reply", "dont reply", "stay out", "not allowed to", "no chatting", "stop chatting", "stop talking in", "stop posting", "ever talk", "ever chat", "ever post", "ever speak", "ever reply", "talk in", "chat in", "post in", "speak in", "never", "do not chat", "do not talk", "do not post"],
            "Never chat in these channels again — a permanent rule until lifted: no replies, no chiming in, no posts there at all. Use when told to never/don't chat, talk or post in a channel (or several, or a whole category). Moderators set it.",
            { "channels": ("string", "Channel or category names, comma-separated ('this' for this one)"), "all_agents": ("boolean", "Every agent, not just you") },
            required: ["channels"]),
        def!("allow_chatting_in", Memory, Conversational, guild: true, core: false,
            keywords: ["can chat", "can talk", "can post", "allowed to talk", "allowed to chat", "chat here again", "talk here again", "unblock", "lift", "start chatting", "you may", "chat in", "talk in", "post in", "again"],
            "Lift a \"never chat here\" rule so you can chat in those channels again. Only someone ranked at least as high as whoever set it.",
            { "channels": ("string", "Channel or category names, comma-separated ('this' for this one, 'all' for every one)"), "all_agents": ("boolean", "For every agent") },
            required: ["channels"]),
        def!("go_quiet", Memory, Conversational, guild: false, core: false,
            keywords: ["shut up", "be quiet", "stop talking", "go away", "stop replying", "quiet", "silence", "leave me alone"],
            "Stop chatting for a while: scope 'me' (stop replying to the requester unless they @mention you) or 'channel' (moderators: stay quiet in this channel for some minutes unless someone ranked as high @mentions you). For \"never\" chat in a channel, use stop_chatting_in.",
            { "minutes": ("integer", "How long (default 60, max 10080)"), "scope": ("string", "me or channel"), "all_agents": ("boolean", "Silence every agent, not just you") },
            required: []),
        // --- standing tasks --------------------------------------------------
        def!("create_task", Memory, Conversational, guild: true, core: false,
            keywords: ["always look", "keep an eye", "watch", "every time", "whenever", "each new", "every day", "every hour", "monitor", "task", "duty", "remind", "check", "every morning", "every night", "every week", "every monday", "every friday", "weekdays", "daily", "weekly", "schedule", "scheduled", "at 9", "tomorrow at", "tonight at", "later today", "in an hour", "every 30 minutes"],
            "Take on a standing task you keep doing until told to stop (for GitHub repos use watch_github instead): react to every new forum post or message in a channel, greet new members, or run on a schedule — every N minutes/hours, every day at a time, on certain weekdays at a time, or once at a set time. Moderators can assign tasks.",
            { "instruction": ("string", "Exactly what to do each time, in the requester's words"), "watch": ("string", "new_posts (forum threads), new_messages, schedule (repeating), once (one time), or member_join"), "channel": ("string", "The channel to watch (or post to, for schedule/once/member_join)"), "every_minutes": ("integer", "Schedule: repeat every N minutes (min 15; 60 = hourly, 1440 = daily)"), "at": ("string", "Schedule/once: time of day, e.g. 09:00 or 9pm (local time)"), "days": ("string", "Schedule with a time: which days, e.g. mon,wed,fri or weekdays (default every day)"), "date": ("string", "Once: the date, YYYY-MM-DD (default today, or tomorrow if the time passed)"), "in_minutes": ("integer", "Once: run this many minutes from now"), "agent": ("string", "Assign to another agent by name (default: you)") },
            required: ["instruction", "watch", "channel"]),
        def!("watch_github", Memory, Conversational, guild: true, core: false,
            keywords: ["github", "repo", "repository", "release", "releases", "commit", "commits", "new version", "github.com", "watch for", "keep an eye on"],
            "Watch a GitHub repo: whenever it publishes a new release (or gets new commits), do the instruction with the details — e.g. post the link and sum up the highlights in a channel. Checked every few minutes. Moderators can set it up.",
            { "repo": ("string", "The repo: a github.com link or owner/name"), "watch": ("string", "releases (default) or commits"), "channel": ("string", "Where to post (default: this channel)"), "instruction": ("string", "What to do each time, in their words (default: post the link and sum up the highlights)"), "branch": ("string", "For commits: the branch (default: the repo's main branch)"), "post_latest_now": ("boolean", "Also handle the current latest release/commit right away") },
            required: ["repo"]),
        def!("stop_watching_github", Memory, Conversational, guild: true, core: false,
            keywords: ["stop watching", "unwatch", "stop following", "github", "repo", "no more releases", "stop posting releases"],
            "Stop watching a GitHub repo (its release/commit tasks are removed).",
            { "repo": ("string", "The repo (link or owner/name)"), "watch": ("string", "releases or commits (omit for both)") },
            required: ["repo"]),
        def!("list_tasks", Memory, Conversational, guild: true, core: false,
            keywords: ["tasks", "duties", "what are you doing", "what do you do", "jobs", "assignments", "your tasks", "watching", "what are you watching", "watch list", "following", "repos"],
            "List your standing tasks — including GitHub repos you watch and scheduled jobs — (or every agent's), with who assigned them and what you last did.",
            { "all_agents": ("boolean", "Include other agents' tasks") }, required: []),
        def!("update_task", Memory, Conversational, guild: true, core: false,
            keywords: ["change the task", "pause", "resume", "update task", "stop the task", "edit task"],
            "Change, pause, or resume a standing task. Only its assigner or someone ranked at least as high may.",
            { "id": ("integer", "Task ID"), "instruction": ("string", "New instruction"), "active": ("boolean", "false pauses, true resumes"), "every_minutes": ("integer", "New interval for scheduled tasks"), "at": ("string", "New time of day for scheduled tasks, e.g. 08:30"), "days": ("string", "New days, e.g. weekdays or mon,thu") },
            required: ["id"]),
        def!("delete_task", Memory, Conversational, guild: true, core: false,
            keywords: ["stop", "delete task", "remove task", "cancel task", "no longer"],
            "Permanently stop a standing task. Only its assigner or someone ranked at least as high may.",
            { "id": ("integer", "Task ID") }, required: ["id"]),
        // --- coordination ----------------------------------------------------
        def!("list_agents", Coordination, Conversational, guild: false, core: false,
            keywords: ["agents", "other bots", "who else", "team", "colleagues"],
            "List your fellow AI agents, their specialties, and whether they're online.", {}, required: []),
        def!("post_to_hq", Coordination, Helper, guild: true, core: false,
            keywords: ["hq", "coordinate", "tell the others", "team", "plan"],
            "Post in the private agent HQ channel to coordinate or plan with the other agents.",
            { "message": ("string", "What to say") }, required: ["message"]),
        def!("ask_agent", Coordination, Helper, guild: true, core: false,
            keywords: ["ask", "handoff", "hand off", "delegate", "can you get", "have them"],
            "Hand a request to another agent (posted in HQ, or here if there is no HQ). Use when another agent's specialty fits better.",
            { "agent": ("string", "Agent name"), "request": ("string", "What you need from them, with context") }, required: ["agent", "request"]),
        // --- utilities -------------------------------------------------------
        def!("get_time", Utilities, Conversational, guild: false, core: false,
            keywords: ["time", "date", "today", "day is it", "clock", "timezone"],
            "Current date and time (UTC, optionally at an offset).", { "utc_offset_hours": ("number", "e.g. -5 for EST") }, required: []),
        def!("calculate", Utilities, Conversational, guild: false, core: true,
            keywords: ["calculate", "math", "+", "*", "percent", "sqrt", "how much", "convert", "sum", "/"],
            "Evaluate a math expression exactly (+ - * / % ^, parentheses, sqrt, abs, ln, log, sin, cos, tan, round, floor, ceil, pi, e). Use it instead of mental math.",
            { "expression": ("string", "e.g. (12.5 * 4) ^ 2 / 3") }, required: ["expression"]),
        def!("roll_dice", Utilities, Conversational, guild: false, core: false,
            keywords: ["roll", "dice", "d20", "d6", "coin", "flip"],
            "Roll dice like 2d6+3, or 'coin' to flip a coin.", { "dice": ("string", "Dice notation") }, required: ["dice"]),
        def!("random_pick", Utilities, Conversational, guild: false, core: false,
            keywords: ["pick", "choose", "random", "decide", "which one"],
            "Pick randomly from options.", { "options": ("array", "The options") }, required: ["options"]),
        def!("set_reminder", Utilities, Conversational, guild: true, core: false,
            keywords: ["remind", "reminder", "in an hour", "tomorrow", "later", "don't let me forget", "alarm", "timer"],
            "Set a one-time reminder: at that time you'll ping them in this channel with the text. People can set reminders for themselves; moderators for others.",
            { "text": ("string", "What to remind about"), "in_minutes": ("integer", "Minutes from now"), "in_hours": ("number", "Hours from now"), "in_days": ("number", "Days from now"), "user": ("string", "Who to remind (default: the requester)") },
            required: ["text"]),
        def!("list_reminders", Utilities, Conversational, guild: true, core: false,
            keywords: ["reminders", "my reminders", "what did i ask you to remind"],
            "List pending reminders (the requester's, or all for moderators).", {}, required: []),
        def!("weather", Utilities, Conversational, guild: false, core: false,
            keywords: ["weather", "temperature", "rain", "forecast", "hot", "cold", "snow", "sunny", "degrees"],
            "Current weather and a 3-day forecast for a place.", { "location": ("string", "City or place name") }, required: ["location"]),
        def!("wikipedia", Utilities, Conversational, guild: false, core: false,
            keywords: ["wiki", "wikipedia", "who was", "what is", "history of", "tell me about", "explain"],
            "Look up a topic on Wikipedia and get its summary.", { "query": ("string", "Topic") }, required: ["query"]),
        def!("define_word", Utilities, Conversational, guild: false, core: false,
            keywords: ["define", "definition", "meaning of", "what does", "mean", "synonym"],
            "Dictionary definitions (and synonyms) of an English word.", { "word": ("string", "The word") }, required: ["word"]),
        def!("convert_currency", Utilities, Conversational, guild: false, core: false,
            keywords: ["usd", "eur", "gbp", "currency", "exchange rate", "dollars", "euros", "yen", "convert"],
            "Convert money between currencies at today's rate (ECB data).",
            { "amount": ("number", "Amount"), "from": ("string", "3-letter code, e.g. USD"), "to": ("string", "3-letter code, e.g. EUR") },
            required: ["amount", "from", "to"]),
        def!("create_invite", Utilities, Helper, guild: true, core: false,
            keywords: ["invite", "invite link", "join link"],
            "Create an invite link to the server (needs Create Invite).",
            { "channel": ("string", "Channel it lands in (default: this one)"), "hours": ("integer", "Hours until it expires (0 = never, max 168)"), "max_uses": ("integer", "0 = unlimited") },
            required: []),
        def!("server_activity", Utilities, Conversational, guild: true, core: false,
            keywords: ["activity", "most active", "stats", "statistics", "who talks", "busiest", "how active"],
            "Activity stats from recent history: busiest channels and most active people (last 24h and overall kept history).",
            { "channel": ("string", "Only this channel") }, required: []),
    ]
}

pub async fn dispatch(ctx: &ToolCtx, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "remember" => remember(ctx, args).await,
        "recall" => recall(ctx, args).await,
        "forget_memory" => forget_memory(ctx, args),
        "note_about_person" => note_about_person(ctx, args).await,
        "set_directive" => set_directive(ctx, args),
        "list_directives" => list_directives(ctx),
        "revoke_directive" => revoke_directive(ctx, args),
        "go_quiet" => go_quiet(ctx, args),
        "stop_chatting_in" => stop_chatting_in(ctx, args),
        "wait" => wait(ctx, args).await,
        "watch_github" => watch_github(ctx, args).await,
        "stop_watching_github" => stop_watching_github(ctx, args),
        "create_command" => create_command(ctx, args),
        "delete_command" => delete_command(ctx, args),
        "toggle_command" => toggle_command(ctx, args),
        "list_commands" => list_commands(ctx),
        "allow_chatting_in" => allow_chatting_in(ctx, args),
        "create_task" => create_task(ctx, args),
        "list_tasks" => list_tasks(ctx, args),
        "update_task" => update_task(ctx, args),
        "delete_task" => delete_task(ctx, args),
        "list_agents" => list_agents(ctx),
        "post_to_hq" => post_to_hq(ctx, args).await,
        "ask_agent" => ask_agent(ctx, args).await,
        "get_time" => get_time(args),
        "calculate" => calculate(&required(args, "expression")?).map(format_number),
        "roll_dice" => roll_dice(&required(args, "dice")?),
        "set_reminder" => set_reminder(ctx, args).await,
        "list_reminders" => list_reminders(ctx),
        "weather" => weather(&required(args, "location")?).await,
        "wikipedia" => wikipedia(&required(args, "query")?).await,
        "define_word" => define_word(&required(args, "word")?).await,
        "convert_currency" => convert_currency(args).await,
        "create_invite" => create_invite(ctx, args).await,
        "server_activity" => server_activity(ctx, args),
        "random_pick" => {
            let options = list(args, "options");
            if options.is_empty() {
                Err("no options given".to_owned())
            } else {
                Ok(format!(
                    "picked: {}",
                    options[fastrand::usize(..options.len())]
                ))
            }
        }
        other => Err(format!("there is no tool named {other}")),
    }
}

// --- web ----------------------------------------------------------------------------

pub async fn web_search(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let query = required(args, "query")?;
    let settings = ctx.core.hive.settings();
    let (results, answer) = super::web::search(&settings.search, &query).await?;
    ctx.core
        .feed(FeedKind::Tool, format!("searched the web: {query}"), None);
    if results.is_empty() && answer.is_none() {
        return Ok("no results".to_owned());
    }
    let mut out = String::new();
    if let Some(answer) = answer {
        out.push_str(&format!("Summary: {answer}\n\n"));
    }
    for (index, result) in results.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n   {}\n   {}\n",
            index + 1,
            result.title,
            result.url,
            crate::llm::truncate(&result.snippet, 300)
        ));
    }
    out.push_str("\n(Use fetch_url to read a result in full. Cite links when helpful.)");
    Ok(out)
}

pub async fn fetch_url(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let url = required(args, "url")?;
    let settings = ctx.core.hive.settings();
    let private_ok = settings.allow_private_fetch
        && (ctx.requester.kind == PrincipalKind::Operator
            || ctx.requester.level >= AccessLevel::Admin);
    // The whole page: long results become an outline to read in parts.
    let text = super::web::fetch(&url, private_ok, 1_500_000).await?;
    ctx.core.feed(FeedKind::Tool, format!("read {url}"), None);
    Ok(text)
}

// --- memory ---------------------------------------------------------------------

async fn resolve_person(ctx: &ToolCtx, query: &str) -> Result<(u64, String), String> {
    if ctx.guild_id.is_some() {
        let member = super::discord::resolve_member(ctx, query).await?;
        Ok((member.user_id, member.display_name().to_owned()))
    } else if matches!(query.to_lowercase().as_str(), "me" | "myself") || query.is_empty() {
        Ok((ctx.requester.user_id, ctx.requester.name.clone()))
    } else {
        crate::runtime::world::parse_snowflake(query)
            .map(|id| (id, query.to_owned()))
            .ok_or_else(|| "in DMs I can only note things about you".to_owned())
    }
}

fn opted_out(ctx: &ToolCtx, user_id: u64) -> bool {
    ctx.guild_id.is_some_and(|guild| {
        ctx.core.hive.knowledge.guild(guild, |k| {
            k.people.get(&user_id).is_some_and(|p| p.opted_out)
        })
    })
}

async fn remember(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let text = required(args, "text")?;
    let about = match arg(args, "about_user") {
        Some(query) => Some(resolve_person(ctx, &query).await?),
        None => None,
    };
    if let Some((user_id, name)) = &about {
        if opted_out(ctx, *user_id) {
            return Err(format!(
                "{name} asked agents not to keep memories about them"
            ));
        }
    }
    let importance = number(args, "importance").unwrap_or(3).clamp(1, 5) as u8;
    let memory = Memory {
        id: 0,
        text: crate::llm::truncate(&text, 600),
        kind: MemoryKind::parse(&arg(args, "kind").unwrap_or_default()),
        about_user: about.as_ref().map(|(id, _)| *id),
        about_name: about.as_ref().map(|(_, name)| name.clone()),
        guild_id: ctx.guild_id,
        source: if ctx.requester.kind == PrincipalKind::Human {
            format!("from {}", ctx.requester.name)
        } else {
            "own observation".to_owned()
        },
        created: now_ms(),
        last_used: 0,
        uses: 0,
        importance,
        embedding: Vec::new(),
    };
    let (id, new) = ctx.core.memory.add(memory);
    ctx.core.embed_memory_later(id);
    ctx.core
        .hive
        .live
        .stat(&ctx.core.id, |s| s.memories_saved += 1);
    ctx.core.feed(
        FeedKind::Memory,
        format!("remembered: {}", crate::llm::truncate(&text, 120)),
        None,
    );
    Ok(if new {
        format!("saved as memory #{id}")
    } else {
        format!("updated memory #{id}")
    })
}

async fn recall(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let query = required(args, "query")?;
    let embedding = ctx.core.embed_query(&query).await;
    let participants = [ctx.requester.user_id];
    let results =
        ctx.core
            .memory
            .search(&query, &participants, ctx.guild_id, embedding.as_deref(), 8);
    if results.is_empty() {
        return Ok("nothing relevant in memory".to_owned());
    }
    Ok(results
        .iter()
        .map(|m| {
            format!(
                "#{} [{}{}] {} ({})",
                m.id,
                m.kind.label(),
                m.about_name
                    .as_ref()
                    .map(|n| format!(", about {n}"))
                    .unwrap_or_default(),
                m.text,
                m.source
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn forget_memory(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let id = number(args, "memory_id").ok_or("missing memory_id")? as u64;
    let memory = ctx.core.memory.get(id).ok_or("no memory with that ID")?;
    let staff = ctx.requester.level >= AccessLevel::Moderator
        || ctx.requester.kind == PrincipalKind::Operator;
    let theirs = memory.about_user == Some(ctx.requester.user_id)
        || memory.source == format!("from {}", ctx.requester.name);
    if !staff && !theirs {
        return Err(
            "denied: people can only delete memories about themselves or that they told me"
                .to_owned(),
        );
    }
    ctx.core.memory.remove(id);
    ctx.core.feed(
        FeedKind::Memory,
        format!("forgot #{id}: {}", crate::llm::truncate(&memory.text, 80)),
        None,
    );
    Ok(format!("forgot memory #{id}"))
}

async fn note_about_person(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let (user_id, name) = resolve_person(ctx, &required(args, "user")?).await?;
    if opted_out(ctx, user_id) {
        return Err(format!("{name} asked agents not to keep notes about them"));
    }
    let notes = crate::llm::truncate(&required(args, "notes")?, 500);
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let person = k.people.entry(user_id).or_default();
        person.name = name.clone();
        person.notes = notes.clone();
        person.updated = now_ms();
    });
    ctx.core
        .feed(FeedKind::Memory, format!("updated notes on {name}"), None);
    Ok(format!("notes on {name} updated"))
}

fn set_directive(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    if ctx.autonomous {
        return Err("directives come from people, not from me".to_owned());
    }
    let text = crate::llm::truncate(&required(args, "text")?, 400);
    if crate::runtime::brain::channel_rule_request(&text) == Some("stop_chatting_in") {
        return Err("not saved: for \"never chat in a channel\" call stop_chatting_in with those channels (it actually keeps you out)".to_owned());
    }
    let scope = match arg(args, "scope")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "server" | "everywhere" | "global" => {
            let ok = ctx.requester.level >= AccessLevel::Admin
                || ctx
                    .requester
                    .permissions
                    .contains(Permissions::MANAGE_GUILD);
            if !ok {
                return Err(format!(
                    "denied: server-wide directives need an admin; {} can set one for themselves (scope 'me')",
                    ctx.requester.name
                ));
            }
            DirectiveScope::Server
        }
        "channel" | "here" => {
            let perms = ctx.requester_permissions_in(ctx.channel_id);
            if !perms.intersects(Permissions::MANAGE_CHANNELS | Permissions::MANAGE_MESSAGES) {
                return Err(format!(
                    "denied: channel directives need a moderator; {} can set one for themselves (scope 'me')",
                    ctx.requester.name
                ));
            }
            DirectiveScope::Channel(ctx.channel_id)
        }
        _ => DirectiveScope::Personal(ctx.requester.user_id),
    };
    let all = flag(args, "for_all_agents").unwrap_or(false);
    let id = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        k.next_directive += 1;
        let id = k.next_directive;
        k.directives.push(Directive {
            id,
            text: text.clone(),
            author_id: ctx.requester.user_id,
            author_name: ctx.requester.name.clone(),
            author_rank: ctx.requester.rank,
            author_level: ctx.requester.level,
            scope,
            target_agent: if all { None } else { Some(ctx.core.id.clone()) },
            created: now_ms(),
        });
        id
    });
    ctx.core.feed(
        FeedKind::Memory,
        format!("new directive #{id} from {}: {text}", ctx.requester.name),
        None,
    );
    Ok(format!("directive #{id} recorded"))
}

fn list_directives(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let lines = ctx.core.hive.knowledge.guild(guild_id, |k| {
        k.directives_for(&ctx.core.id, ctx.channel_id, &[ctx.requester.user_id])
            .iter()
            .map(|d| {
                format!(
                    "#{} [{} — {}] {}",
                    d.id,
                    d.author_name,
                    d.author_level.label(),
                    d.text
                )
            })
            .collect::<Vec<_>>()
    });
    let barred: Vec<String> = ctx.core.hive.knowledge.guild(guild_id, |k| {
        k.no_chat.iter().filter(|r| r.agent_id.as_deref().is_none_or(|a| a == ctx.core.id)).map(|r| (r.channel_id, r.setter_name.clone())).collect::<Vec<_>>()
    })
    .into_iter()
    .map(|(id, by)| format!("never chat in #{} [{by}]", ctx.core.world.with_guild(guild_id, |g| g.channel_name(id)).unwrap_or_default()))
    .collect();
    let lines: Vec<String> = lines.into_iter().chain(barred).collect();
    Ok(if lines.is_empty() {
        "no directives apply here".to_owned()
    } else {
        lines.join("\n")
    })
}

fn may_override(ctx: &ToolCtx, author_id: u64, author_rank: u32) -> bool {
    ctx.requester.kind == PrincipalKind::Operator
        || (!ctx.autonomous
            && (ctx.requester.user_id == author_id || ctx.requester.rank >= author_rank))
}

fn revoke_directive(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let id = number(args, "id").ok_or("missing id")? as u64;
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let index = k
            .directives
            .iter()
            .position(|d| d.id == id)
            .ok_or("no directive with that ID")?;
        let directive = &k.directives[index];
        if !may_override(ctx, directive.author_id, directive.author_rank) {
            return Err(format!(
                "denied: directive #{id} was set by {} ({}), who outranks {}",
                directive.author_name,
                directive.author_level.label(),
                ctx.requester.name
            ));
        }
        k.directives.remove(index);
        Ok(format!("directive #{id} revoked"))
    })
}

// --- pacing ------------------------------------------------------------------------------

async fn wait(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let seconds = number(args, "seconds").unwrap_or(5).clamp(1, 120);
    let started = now_ms();
    for _ in 0..seconds {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // "stop" while waiting ends it now.
        let stopped = ctx.core.interrupts.lock().expect("interrupts poisoned").get(&ctx.channel_id).is_some_and(|t| *t >= started);
        if stopped {
            return Ok("stopped waiting — they said stop".to_owned());
        }
    }
    Ok(format!("waited {seconds}s"))
}

// --- GitHub ------------------------------------------------------------------------------

async fn watch_github(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    if ctx.autonomous || ctx.requester.kind == PrincipalKind::Agent {
        return Err("standing tasks are assigned by people".to_owned());
    }
    let channel_id = super::discord::resolve_text_channel(ctx, arg(args, "channel"))?;
    if let Some(missing) = ctx.core.cant_post(guild_id, channel_id) {
        let name = ctx.core.world.with_guild(guild_id, |g| g.channel_name(channel_id)).unwrap_or_default();
        return Err(format!(
            "not set up: I can't post in #{name} — my role is missing {missing} there. Tell them that, so they can give my role that permission in the channel's settings (or pick another channel), then ask again."
        ));
    }
    let staff = ctx.requester.kind == PrincipalKind::Operator
        || ctx.requester.level >= AccessLevel::Moderator
        || ctx.requester_permissions_in(channel_id).intersects(Permissions::MANAGE_CHANNELS | Permissions::MANAGE_MESSAGES);
    if !staff {
        return Err(format!("denied: watching repos is set up by moderators; {} isn't one", ctx.requester.name));
    }
    let wanted = required(args, "repo")?;
    let repo = crate::runtime::github::parse_repo(&wanted)
        .or_else(|| crate::runtime::github::parse_repo(&request_text(ctx).split_whitespace().find(|w| w.contains("github.com/")).unwrap_or("")))
        .ok_or_else(|| format!("\"{wanted}\" isn't a GitHub repo — give a github.com link or owner/name"))?;
    let kind = match arg(args, "watch").unwrap_or_default().to_lowercase() {
        w if w.contains("commit") || w.contains("push") => "commits",
        _ => "releases",
    };
    let token = ctx.core.hive.settings().github_token;
    let now_repo = crate::runtime::github::look(&repo, arg(args, "branch").as_deref(), &token).await?;
    let repo = now_repo.full_name.clone();
    let branch = arg(args, "branch").unwrap_or_else(|| now_repo.default_branch.clone());
    // Only when they asked for the current one too (small models set this
    // on their own and then post it twice).
    let said = request_text(ctx).to_lowercase();
    let post_now = flag(args, "post_latest_now").unwrap_or(false)
        && ["latest", "current", "right now", "post it now", "the newest", "most recent", "last release", "last one"].iter().any(|p| said.contains(p));
    // Start from what's there now (or one step back, to handle the latest
    // right away).
    let last_seen = if kind == "commits" {
        match (post_now, now_repo.latest_commits.as_slice()) {
            (true, [_, second, ..]) => second.clone(),
            (_, [first, ..]) => first.clone(),
            _ => String::new(),
        }
    } else {
        match (&now_repo.latest_release, post_now) {
            (Some((id, ..)), true) => id.saturating_sub(1).to_string(),
            (Some((id, ..)), false) => id.to_string(),
            // No releases yet: everything from now on is new.
            (None, _) => "0".to_owned(),
        }
    };
    let instruction = crate::llm::truncate(
        &arg(args, "instruction").unwrap_or_else(|| {
            if kind == "commits" {
                "Post the new commits with their links and sum up what changed.".to_owned()
            } else {
                "Post the release link and a few short bullet points of its highlights.".to_owned()
            }
        }),
        1500,
    );
    let duplicate = ctx.core.hive.knowledge.guild(guild_id, |k| {
        k.tasks
            .iter()
            .find(|t| t.agent_id == ctx.core.id && matches!(&t.trigger, TaskTrigger::GitHub { repo: r, kind: kd, channel_id: c, .. } if r.eq_ignore_ascii_case(&repo) && kd == kind && *c == channel_id))
            .map(|t| t.id)
    });
    if let Some(id) = duplicate {
        return Ok(format!("already watching {repo} {kind} for that channel (task #{id})"));
    }
    let id = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        k.next_task += 1;
        let id = k.next_task;
        k.tasks.push(StandingTask {
            id,
            agent_id: ctx.core.id.clone(),
            instruction: instruction.clone(),
            trigger: TaskTrigger::GitHub {
                channel_id,
                repo: repo.clone(),
                kind: kind.to_owned(),
                branch: branch.clone(),
                last_seen,
                etag: String::new(),
                // Check soon when the latest should go out now.
                checked: if post_now { 0 } else { now_ms() },
            },
            author_id: ctx.requester.user_id,
            author_name: ctx.requester.name.clone(),
            author_rank: ctx.requester.rank,
            author_level: ctx.requester.level,
            created: now_ms(),
            active: true,
            runs: 0,
            last_run: now_ms(),
            last_result: String::new(),
            origin_channel: ctx.channel_id,
        });
        id
    });
    let channel = ctx.core.world.with_guild(guild_id, |g| g.channel_name(channel_id)).unwrap_or_default();
    let current = if kind == "commits" {
        format!("on branch {branch}")
    } else {
        match &now_repo.latest_release {
            Some((_, tag, name, url)) => format!("its latest release right now is {} ({url})", if name.is_empty() { tag } else { name }),
            None => "it has no releases yet".to_owned(),
        }
    };
    Ok(format!(
        "task #{id}: watching github.com/{repo} for new {kind} — {current}. Each time: \"{instruction}\" in #{channel}. Checked every 5 minutes{}. Stop with stop_watching_github; list_tasks shows everything being watched.\nNow just confirm this briefly — don't post or sum up any release yourself (the task does that with the real release notes{}).",
        if post_now { "; the latest one goes out within a minute" } else { "" },
        if post_now { ", starting with the latest within a minute" } else { " when the next one comes out" }
    ))
}

fn stop_watching_github(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let wanted = required(args, "repo")?;
    let repo = crate::runtime::github::parse_repo(&wanted).unwrap_or_else(|| wanted.trim().to_lowercase());
    let kind = arg(args, "watch").map(|w| if w.to_lowercase().contains("commit") { "commits" } else { "releases" });
    let (removed, kept) = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        k.tasks.retain(|t| {
            let TaskTrigger::GitHub { repo: r, kind: kd, .. } = &t.trigger else {
                return true;
            };
            // "llama.cpp" matches "ggml-org/llama.cpp" too.
            let same_repo = r.eq_ignore_ascii_case(&repo) || r.to_lowercase().ends_with(&format!("/{}", repo.to_lowercase()));
            if t.agent_id != ctx.core.id || !same_repo || kind.is_some_and(|w| w != kd) {
                return true;
            }
            if may_override(ctx, t.author_id, t.author_rank) {
                removed.push(format!("#{} ({r} {kd})", t.id));
                false
            } else {
                kept.push(format!("#{} (set by {})", t.id, t.author_name));
                true
            }
        });
        (removed, kept)
    });
    if removed.is_empty() && kept.is_empty() {
        return Err(format!("I'm not watching \"{wanted}\" here — list_tasks shows what I watch"));
    }
    let mut out = String::new();
    if !removed.is_empty() {
        out.push_str(&format!("stopped watching: {}", removed.join(", ")));
    }
    if !kept.is_empty() {
        out.push_str(&format!("\ndenied for {}: set by someone ranked higher than {}", kept.join(", "), ctx.requester.name));
    }
    Ok(out.trim().to_owned())
}

// --- reading schedules from the request itself ------------------------------------------

/// The message that asked for this (small models drop details; the
/// person's own words still have them).
fn request_text(ctx: &ToolCtx) -> String {
    ctx.message_id
        .and_then(|id| ctx.core.hive.knowledge.channel(ctx.channel_id, |a| a.get_mut(id).map(|m| m.content.clone())))
        .unwrap_or_default()
}

/// A time of day in free text: "at 9", "9am", "6:30 pm", "18:00", "noon".
pub fn find_time(text: &str) -> Option<u32> {
    let t = text.to_lowercase();
    let words: Vec<&str> = t.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).collect();
    for (i, w) in words.iter().enumerate() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric() && c != ':');
        if matches!(w, "noon" | "midnight") {
            return crate::memory::parse_time_of_day(w);
        }
        if !w.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let next = words.get(i + 1).map(|n| n.trim_matches(|c: char| !c.is_alphanumeric())).unwrap_or("");
        let candidate = if matches!(next, "am" | "pm") { format!("{w}{next}") } else { w.to_owned() };
        let clear = candidate.ends_with("am") || candidate.ends_with("pm") || candidate.contains(':')
            || (i > 0 && words[i - 1] == "at");
        if clear {
            if let Some(minute) = crate::memory::parse_time_of_day(&candidate) {
                return Some(minute);
            }
        }
    }
    None
}

/// "every 2 hours" / "every 30 minutes" / "hourly" → minutes.
pub fn find_interval(text: &str) -> Option<i64> {
    let t = text.to_lowercase();
    if t.contains("every half hour") {
        return Some(30);
    }
    if t.contains("every hour") || t.contains("hourly") {
        return Some(60);
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    words.windows(3).find_map(|w| {
        let n: i64 = w[1].parse().ok()?;
        let unit = match w[2] {
            u if u.starts_with("min") => 1,
            u if u.starts_with("hour") || u.starts_with("hr") => 60,
            u if u.starts_with("day") => 1440,
            u if u.starts_with("week") => 10_080,
            _ => return None,
        };
        (w[0] == "every").then_some(n * unit)
    })
}

/// "in 30 minutes" / "in 2 hours" → minutes from now.
pub fn find_in(text: &str) -> Option<i64> {
    let t = text.to_lowercase();
    let words: Vec<&str> = t.split_whitespace().collect();
    words.windows(3).find_map(|w| {
        let n: i64 = w[1].parse().ok()?;
        let unit = match w[2] {
            u if u.starts_with("min") => 1,
            u if u.starts_with("hour") || u.starts_with("hr") => 60,
            _ => return None,
        };
        (w[0] == "in").then_some(n * unit)
    })
}

// --- slash commands made in a server ---------------------------------------------------

fn may_manage_commands(ctx: &ToolCtx) -> Result<(), String> {
    if ctx.autonomous || ctx.requester.kind == PrincipalKind::Agent {
        return Err("commands are changed by people, not on my own".to_owned());
    }
    let ok = ctx.requester.kind == PrincipalKind::Operator
        || ctx.requester.level >= AccessLevel::Admin
        || ctx.requester.permissions.intersects(Permissions::MANAGE_GUILD | Permissions::ADMINISTRATOR);
    if ok {
        Ok(())
    } else {
        Err(format!("denied: changing slash commands needs Manage Server, which {} doesn't have", ctx.requester.name))
    }
}

fn command_name(args: &Value) -> Result<String, String> {
    let name = required(args, "name")?.trim().trim_start_matches('/').to_lowercase().replace(' ', "-");
    if crate::config::CustomCommand::valid_name(&name) {
        Ok(name)
    } else {
        Err(format!("\"{name}\" can't be a command name — 1–32 lowercase letters, numbers, - or _"))
    }
}

fn create_command(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    may_manage_commands(ctx)?;
    let name = command_name(args)?;
    if crate::runtime::commands::BUILT_IN.iter().any(|(n, _)| *n == name) {
        return Err(format!("/{name} is a built-in command — pick another name (toggle_command turns built-ins on/off)"));
    }
    let profile = ctx.core.profile();
    if profile.behavior.custom_commands.iter().any(|c| c.name == name && (c.servers.is_empty() || c.servers.contains(&guild_id))) {
        return Err(format!("/{name} is set in the Axis Agents app — change it there, or pick another name"));
    }
    let text = crate::llm::truncate(&required(args, "text")?, 1500);
    let mode = match arg(args, "does").map(|d| d.to_lowercase()).as_deref() {
        Some("run" | "do" | "agent" | "ai" | "instruction" | "execute" | "task") => crate::config::CommandMode::Agent,
        Some(_) => crate::config::CommandMode::Reply,
        None if sounds_like_instruction(&text) || sounds_like_instruction(&request_text(ctx)) => crate::config::CommandMode::Agent,
        None => crate::config::CommandMode::Reply,
    };
    let command = crate::memory::ServerCommand {
        name: name.clone(),
        description: arg(args, "description").unwrap_or_default().chars().take(100).collect(),
        text,
        mode,
        private: flag(args, "private").unwrap_or(false),
        takes_input: flag(args, "takes_input").unwrap_or(false),
        disabled: false,
        agent_id: ctx.core.id.clone(),
        author_id: ctx.requester.user_id,
        author_name: ctx.requester.name.clone(),
        created: now_ms(),
    };
    let replaced = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let before = k.commands.len();
        k.commands.retain(|c| !(c.name == name && c.agent_id == ctx.core.id));
        let replaced = k.commands.len() < before;
        k.commands.push(command);
        replaced
    });
    crate::runtime::commands::request_sync(&ctx.core);
    Ok(format!(
        "{} /{name} in this server ({}). Discord shows it within about 20 seconds (people may need to reload Discord to see it).",
        if replaced { "updated" } else { "made" },
        mode.label()
    ))
}

/// "look up today's news and post a summary" / "a command that makes you
/// …" — something to do each time, not text to post.
pub fn sounds_like_instruction(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    let starts = [
        "look up", "search", "find", "summarize", "summarise", "check", "get ", "fetch", "tell ", "give ", "pick", "choose", "roll",
        "write", "generate", "make ", "create", "translate", "list", "show", "read", "count", "ping", "analyze", "explain", "answer",
        "recommend", "suggest", "describe", "compare", "research",
    ];
    starts.iter().any(|s| t.starts_with(s))
        || ["makes you", "have you", "you look", "you search", "you find", "you check", "you summarize", "you post", "that looks up", "that searches", "that summarizes", "that checks"]
            .iter()
            .any(|p| t.contains(p))
}

fn delete_command(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    may_manage_commands(ctx)?;
    let name = command_name(args)?;
    let removed = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let before = k.commands.len();
        k.commands.retain(|c| !(c.name == name && c.agent_id == ctx.core.id));
        k.commands.len() < before
    });
    if !removed {
        if crate::runtime::commands::BUILT_IN.iter().any(|(n, _)| *n == name) {
            return Err(format!("/{name} is built in — it can be turned off here with toggle_command instead"));
        }
        if ctx.core.profile().behavior.custom_commands.iter().any(|c| c.name == name) {
            return Err(format!("/{name} is set in the Axis Agents app (Commands tab) — it has to be removed there"));
        }
        return Err(format!("there's no /{name} made in this server"));
    }
    crate::runtime::commands::request_sync(&ctx.core);
    Ok(format!("deleted /{name}; it disappears from Discord within about 20 seconds"))
}

fn toggle_command(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    may_manage_commands(ctx)?;
    let name = command_name(args)?;
    let on = flag(args, "enabled").ok_or("say whether to turn it on (true) or off (false)")?;
    if crate::runtime::commands::BUILT_IN.iter().any(|(n, _)| *n == name) {
        ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
            k.disabled_commands.retain(|d| *d != name);
            if !on {
                k.disabled_commands.push(name.clone());
            }
        });
        return Ok(format!(
            "/{name} is {} in this server{}",
            if on { "on" } else { "off" },
            if on { "" } else { " (it still shows in the list, but answers that it's turned off here)" }
        ));
    }
    let found = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        k.commands
            .iter_mut()
            .find(|c| c.name == name && c.agent_id == ctx.core.id)
            .map(|c| c.disabled = !on)
            .is_some()
    });
    if !found {
        return Err(format!("there's no /{name} here — list_commands shows them"));
    }
    crate::runtime::commands::request_sync(&ctx.core);
    Ok(format!("/{name} is {} (Discord updates within about 20 seconds)", if on { "on" } else { "off" }))
}

fn list_commands(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let profile = ctx.core.profile();
    if !profile.behavior.slash_commands {
        return Ok("slash commands are turned off in the app".to_owned());
    }
    let (off_here, made) = ctx.core.hive.knowledge.guild(guild_id, |k| (k.disabled_commands.clone(), k.commands.clone()));
    let mut out = String::from("Built in:\n");
    for (name, help) in crate::runtime::commands::BUILT_IN {
        let state = if profile.behavior.disabled_commands.iter().any(|d| d == name) {
            " (off everywhere — set in the app)"
        } else if off_here.iter().any(|d| d == name) {
            " (off in this server)"
        } else {
            ""
        };
        out.push_str(&format!("- /{name}: {help}{state}\n"));
    }
    let app: Vec<String> = profile
        .behavior
        .custom_commands
        .iter()
        .filter(|c| c.servers.is_empty() || c.servers.contains(&guild_id))
        .map(|c| format!("- /{} ({}): {}", c.name, c.mode.label(), crate::llm::truncate(&c.reply, 80)))
        .collect();
    if !app.is_empty() {
        out.push_str(&format!("Set in the app:\n{}\n", app.join("\n")));
    }
    let here: Vec<String> = made
        .iter()
        .filter(|c| c.agent_id == ctx.core.id)
        .map(|c| format!("- /{} ({}{}, made for {}): {}", c.name, c.mode.label(), if c.disabled { ", off" } else { "" }, c.author_name, crate::llm::truncate(&c.text, 80)))
        .collect();
    if !here.is_empty() {
        out.push_str(&format!("Made in this server:\n{}\n", here.join("\n")));
    }
    Ok(out)
}

/// Channel names in one argument: "general, memes and #art", "this".
/// A bare "this" when the request itself named other channels means
/// those (small models default to "this").
fn channel_ids(ctx: &ToolCtx, text: &str) -> (Vec<u64>, Vec<String>) {
    let only_here = text
        .split([',', ';'])
        .all(|p| matches!(p.trim().to_lowercase().as_str(), "this" | "here" | "current" | "this channel" | ""));
    let mut text = text.to_owned();
    if only_here {
        let request = ctx
            .message_id
            .and_then(|id| ctx.core.hive.knowledge.channel(ctx.channel_id, |a| a.get_mut(id).map(|m| m.content.clone())))
            .unwrap_or_default();
        let lower = request.to_lowercase();
        let means_here = [" here", "this channel", "this one", "in this"].iter().any(|w| lower.contains(w));
        if let Some(guild_id) = ctx.guild_id.filter(|_| !means_here) {
            let named = crate::runtime::brain::named_channels(&ctx.core, guild_id, &request);
            if !named.is_empty() {
                text = named.join(", ");
            }
        }
    }
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for part in text.split([',', ';', '\n']).flat_map(|p| p.split(" and ")) {
        let name = part.trim().trim_start_matches('#').trim();
        if name.is_empty() {
            continue;
        }
        // Text channels and categories first: "general" is #general, not
        // the General voice channel.
        let text_first = ctx
            .guild_id
            .and_then(|g| ctx.core.world.with_guild(g, |g| g.find_channel_where(name, |c| !c.is_voice()).map(|c| c.id.get())))
            .flatten();
        match text_first.ok_or(()).or_else(|_| super::discord::resolve_channel(ctx, Some(name.to_owned()))) {
            Ok(id) if !found.contains(&id) => found.push(id),
            Ok(_) => {}
            Err(_) => missing.push(name.to_owned()),
        }
    }
    (found, missing)
}

fn stop_chatting_in(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    if ctx.autonomous || ctx.requester.kind == PrincipalKind::Agent {
        return Err("only people can set where I don't chat".to_owned());
    }
    let all = flag(args, "all_agents").unwrap_or(false);
    let (channels, missing) = channel_ids(ctx, &required(args, "channels")?);
    let mut done = Vec::new();
    let mut denied = Vec::new();
    for id in channels {
        let name = ctx.core.world.with_guild(guild_id, |g| g.channel_name(id)).unwrap_or_default();
        let staff = ctx.requester.kind == PrincipalKind::Operator
            || ctx.requester.level >= AccessLevel::Moderator
            || ctx.requester_permissions_in(id).intersects(Permissions::MANAGE_MESSAGES | Permissions::MANAGE_CHANNELS);
        if !staff {
            denied.push(format!("#{name}"));
            continue;
        }
        let agent = if all { None } else { Some(ctx.core.id.clone()) };
        ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
            k.no_chat.retain(|r| !(r.channel_id == id && r.agent_id == agent));
            k.no_chat.push(NoChatRule {
                channel_id: id,
                agent_id: agent.clone(),
                setter_id: ctx.requester.user_id,
                setter_name: ctx.requester.name.clone(),
                setter_rank: ctx.requester.rank,
                created: now_ms(),
            });
        });
        done.push(format!("#{name}"));
    }
    if done.is_empty() {
        return Err(match (denied.is_empty(), missing.is_empty()) {
            (false, _) => format!("denied: only moderators can set where I never chat ({} can't for {})", ctx.requester.name, denied.join(", ")),
            _ => format!("no channel called {}", missing.join(", ")),
        });
    }
    ctx.core.feed(FeedKind::System, format!("never chatting in {}", done.join(", ")), None);
    let mut out = format!(
        "done: {} will never chat in {} (no replies or posts there) until someone ranked at least as high as {} lifts it with allow_chatting_in. Confirm briefly — after this reply you're silent there.",
        if all { "no agent" } else { "you" },
        done.join(", "),
        ctx.requester.name
    );
    if !denied.is_empty() {
        out.push_str(&format!("\nNot set (moderators only): {}", denied.join(", ")));
    }
    if !missing.is_empty() {
        out.push_str(&format!("\nNo such channel: {}", missing.join(", ")));
    }
    Ok(out)
}

fn allow_chatting_in(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let text = required(args, "channels")?;
    let every = matches!(text.trim().to_lowercase().as_str(), "all" | "every" | "everywhere" | "all channels");
    let (channels, missing) = if every { (Vec::new(), Vec::new()) } else { channel_ids(ctx, &text) };
    let all = flag(args, "all_agents").unwrap_or(false);
    let (lifted, kept) = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let mut lifted = Vec::new();
        let mut kept = Vec::new();
        k.no_chat.retain(|r| {
            let mine = all || r.agent_id.as_deref().is_none_or(|a| a == ctx.core.id);
            if !mine || !(every || channels.contains(&r.channel_id)) {
                return true;
            }
            if may_override(ctx, r.setter_id, r.setter_rank) {
                lifted.push(r.channel_id);
                false
            } else {
                kept.push((r.channel_id, r.setter_name.clone()));
                true
            }
        });
        (lifted, kept)
    });
    let name = |id: u64| format!("#{}", ctx.core.world.with_guild(guild_id, |g| g.channel_name(id)).unwrap_or_default());
    if lifted.is_empty() && kept.is_empty() {
        return Ok(if missing.is_empty() {
            "there was no \"never chat here\" rule for that — I can already chat there".to_owned()
        } else {
            format!("no channel called {}", missing.join(", "))
        });
    }
    let mut out = String::new();
    if !lifted.is_empty() {
        out.push_str(&format!("lifted: you can chat in {} again", lifted.iter().map(|id| name(*id)).collect::<Vec<_>>().join(", ")));
    }
    if !kept.is_empty() {
        out.push_str(&format!(
            "\ndenied for {}: set by someone ranked higher than {}",
            kept.iter().map(|(id, by)| format!("{} (by {by})", name(*id))).collect::<Vec<_>>().join(", "),
            ctx.requester.name
        ));
    }
    Ok(out.trim().to_owned())
}

fn go_quiet(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let minutes = number(args, "minutes").unwrap_or(60).clamp(1, 10_080);
    let channel_scope = arg(args, "scope").is_some_and(|s| s.eq_ignore_ascii_case("channel"));
    let all = flag(args, "all_agents").unwrap_or(false);
    let until = now_ms() + minutes * 60_000;
    let rule = if channel_scope {
        if !ctx
            .requester_permissions_in(ctx.channel_id)
            .contains(Permissions::MANAGE_MESSAGES)
            && ctx.requester.kind != PrincipalKind::Operator
        {
            return Err(format!(
                "denied: only moderators can quiet me for a whole channel; {} can ask me to leave them alone (scope 'me')",
                ctx.requester.name
            ));
        }
        QuietRule {
            channel_id: Some(ctx.channel_id),
            user_id: None,
            agent_id: if all { None } else { Some(ctx.core.id.clone()) },
            setter_rank: ctx.requester.rank,
            until,
        }
    } else {
        QuietRule {
            channel_id: None,
            user_id: Some(ctx.requester.user_id),
            agent_id: if all { None } else { Some(ctx.core.id.clone()) },
            setter_rank: ctx.requester.rank,
            until,
        }
    };
    match ctx.guild_id {
        Some(guild_id) => ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
            k.quiet.retain(|r| r.until > now_ms());
            k.quiet.push(rule);
        }),
        None => {
            return Err("in DMs, just stop messaging me — I only answer when you write".to_owned());
        }
    }
    ctx.core.feed(
        FeedKind::System,
        format!(
            "going quiet for {minutes}m ({})",
            if channel_scope {
                "channel"
            } else {
                "for one person"
            }
        ),
        None,
    );
    Ok(format!(
        "quiet for {minutes} minutes{}",
        if channel_scope {
            " in this channel"
        } else {
            " towards the requester (an @mention brings me back)"
        }
    ))
}

// --- standing tasks ----------------------------------------------------------------

fn create_task(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    if ctx.autonomous || ctx.requester.kind == PrincipalKind::Agent {
        return Err("standing tasks are assigned by people".to_owned());
    }
    let channel_id = super::discord::resolve_text_channel(ctx, arg(args, "channel"))?;
    if let Some(missing) = ctx.core.cant_post(guild_id, channel_id) {
        let name = ctx.core.world.with_guild(guild_id, |g| g.channel_name(channel_id)).unwrap_or_default();
        return Err(format!(
            "not set up: I can't post in #{name} — my role is missing {missing} there. Tell them that, so they can give my role that permission in the channel's settings (or pick another channel), then ask again."
        ));
    }
    let perms = ctx.requester_permissions_in(channel_id);
    let staff = ctx.requester.kind == PrincipalKind::Operator
        || ctx.requester.level >= AccessLevel::Moderator
        || perms.intersects(Permissions::MANAGE_CHANNELS | Permissions::MANAGE_MESSAGES);
    if !staff {
        return Err(format!(
            "denied: standing tasks are set by moderators; {} isn't one",
            ctx.requester.name
        ));
    }
    let channel = ctx.core.world.channel(guild_id, channel_id);
    // What the model left out, from the request's own words.
    let said = format!("{} {}", request_text(ctx), arg(args, "instruction").unwrap_or_default());
    let mut args = args.clone();
    if let Some(map) = args.as_object_mut() {
        let watch = map.get("watch").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let timed = matches!(watch.as_str(), "schedule" | "timer" | "every" | "daily" | "weekly" | "repeat" | "once" | "one_time" | "later" | "at");
        if timed && !map.contains_key("at") && !map.contains_key("in_minutes") {
            if let Some(minute) = find_time(&said) {
                map.insert("at".to_owned(), Value::String(format!("{:02}:{:02}", minute / 60, minute % 60)));
            } else if let (true, Some(minutes)) = (watch == "once" || watch == "later", find_in(&said)) {
                map.insert("in_minutes".to_owned(), Value::from(minutes));
            }
        }
        if timed && watch != "once" && !map.contains_key("days") {
            if let Some(days) = crate::memory::parse_days(&said).filter(|d| !d.is_empty()) {
                let names: Vec<&str> = days.iter().map(|d| ["mon", "tue", "wed", "thu", "fri", "sat", "sun"][*d as usize]).collect();
                map.insert("days".to_owned(), Value::String(names.join(",")));
            }
        }
        if timed && watch != "once" && !map.contains_key("every_minutes") && !map.contains_key("at") {
            if let Some(minutes) = find_interval(&said) {
                map.insert("every_minutes".to_owned(), Value::from(minutes));
            }
        }
        // "every day at 9" asked for as a one-time thing is still daily.
        if watch == "once" && find_interval(&said).is_none() && ["every day", "every morning", "daily", "every week"].iter().any(|p| said.to_lowercase().contains(p)) {
            map.insert("watch".to_owned(), Value::String("schedule".to_owned()));
        }
    }
    let args = &args;
    let trigger = match arg(args, "watch")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "new_posts" | "posts" | "forum" => {
            if channel.as_ref().is_some_and(|c| !c.is_forum()) {
                return Err(
                    "new_posts needs a forum channel; use new_messages for a regular channel"
                        .to_owned(),
                );
            }
            TaskTrigger::NewPost { channel_id }
        }
        "new_messages" | "messages" => TaskTrigger::NewMessage { channel_id },
        "schedule" | "timer" | "every" | "daily" | "weekly" | "repeat" => {
            let at_minute = match arg(args, "at") {
                Some(at) => Some(crate::memory::parse_time_of_day(&at).ok_or_else(|| format!("couldn't read the time \"{at}\" — use e.g. 09:00 or 9pm"))?),
                None => None,
            };
            let days = match arg(args, "days") {
                Some(d) => crate::memory::parse_days(&d).ok_or_else(|| format!("couldn't read the days \"{d}\" — use e.g. mon,wed,fri or weekdays"))?,
                None => Vec::new(),
            };
            if !days.is_empty() && at_minute.is_none() {
                return Err("give a time too (at), e.g. 09:00, for tasks on certain days".to_owned());
            }
            TaskTrigger::Schedule {
                channel_id,
                every_minutes: number(args, "every_minutes")
                    .unwrap_or(1440)
                    .clamp(15, 43_200) as u32,
                at_minute,
                days,
            }
        }
        "once" | "one_time" | "later" | "at" => {
            let now = now_ms();
            let at = match (number(args, "in_minutes"), arg(args, "at")) {
                (Some(minutes), _) => now + minutes.clamp(1, 525_600) * 60_000,
                (None, Some(at)) => {
                    let minute = crate::memory::parse_time_of_day(&at).ok_or_else(|| format!("couldn't read the time \"{at}\" — use e.g. 18:00 or 6pm"))?;
                    crate::memory::next_time_at(now, minute, arg(args, "date").as_deref())
                        .ok_or("couldn't read the date — use YYYY-MM-DD")?
                }
                (None, None) => return Err("say when: at (a time, optionally with date) or in_minutes".to_owned()),
            };
            if at <= now {
                return Err("that time has already passed".to_owned());
            }
            TaskTrigger::Once { channel_id, at }
        }
        "member_join" | "join" | "joins" => TaskTrigger::MemberJoin { channel_id },
        other => {
            return Err(format!(
                "unknown watch kind \"{other}\" — use new_posts, new_messages, schedule, once, or member_join"
            ));
        }
    };
    let agent_id = match arg(args, "agent") {
        Some(name) if !matches!(name.to_lowercase().as_str(), "you" | "me" | "self") => ctx
            .core
            .hive
            .peer_by_name(&name)
            .map(|p| p.id)
            .ok_or_else(|| format!("no agent named {name}"))?,
        _ => ctx.core.id.clone(),
    };
    let instruction = crate::llm::truncate(&required(args, "instruction")?, 1500);
    // Small models sometimes call this twice for one request.
    let duplicate = ctx.core.hive.knowledge.guild(guild_id, |k| {
        k.tasks
            .iter()
            .find(|t| t.agent_id == agent_id && t.trigger == trigger && now_ms() - t.created < 180_000)
            .map(|t| t.id)
    });
    if let Some(id) = duplicate {
        return Ok(format!("already set up as task #{id} — nothing more to do"));
    }
    let channel_label = channel
        .and_then(|c| c.name)
        .unwrap_or_else(|| channel_id.to_string());
    let description = trigger.describe(&channel_label);
    let id = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        k.next_task += 1;
        let id = k.next_task;
        k.tasks.push(StandingTask {
            id,
            agent_id: agent_id.clone(),
            instruction: instruction.clone(),
            trigger,
            author_id: ctx.requester.user_id,
            author_name: ctx.requester.name.clone(),
            author_rank: ctx.requester.rank,
            author_level: ctx.requester.level,
            created: now_ms(),
            active: true,
            runs: 0,
            last_run: now_ms(),
            last_result: String::new(),
            origin_channel: ctx.channel_id,
        });
        id
    });
    let assignee = ctx
        .core
        .hive
        .peer(&agent_id)
        .map(|p| p.name)
        .unwrap_or_else(|| ctx.core.name());
    ctx.core.feed(
        FeedKind::Coordination,
        format!(
            "task #{id} for {assignee} ({description}): {}",
            crate::llm::truncate(&instruction, 100)
        ),
        None,
    );
    Ok(format!("task #{id} created for {assignee}: {description}"))
}

pub fn describe_task(
    ctx_core: &crate::runtime::agent::AgentCore,
    guild_id: u64,
    task: &StandingTask,
) -> String {
    let channel = ctx_core
        .world
        .with_guild(guild_id, |g| g.channel_name(task.trigger.channel_id()))
        .unwrap_or_else(|| task.trigger.channel_id().to_string());
    let agent = ctx_core
        .hive
        .peer(&task.agent_id)
        .map(|p| p.name)
        .unwrap_or_else(|| "?".to_owned());
    format!(
        "#{} [{}] {} — {}: \"{}\" (assigned by {} — {}; ran {} time(s){})",
        task.id,
        if task.active { "active" } else { "paused" },
        agent,
        task.trigger.describe(&channel),
        task.instruction,
        task.author_name,
        task.author_level.label(),
        task.runs,
        if task.last_result.is_empty() {
            String::new()
        } else {
            format!("; last: {}", crate::llm::truncate(&task.last_result, 120))
        }
    )
}

fn list_tasks(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let all = flag(args, "all_agents").unwrap_or(false);
    let tasks = ctx.core.hive.knowledge.guild(guild_id, |k| k.tasks.clone());
    let lines: Vec<String> = tasks
        .iter()
        .filter(|t| all || t.agent_id == ctx.core.id)
        .map(|t| describe_task(&ctx.core, guild_id, t))
        .collect();
    Ok(if lines.is_empty() {
        "no standing tasks".to_owned()
    } else {
        lines.join("\n")
    })
}

fn update_task(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let id = number(args, "id").ok_or("missing id")? as u64;
    ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let task = k
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or("no task with that ID")?;
        if !may_override(ctx, task.author_id, task.author_rank) {
            return Err(format!(
                "denied: task #{id} was assigned by {} ({}), who outranks {}",
                task.author_name,
                task.author_level.label(),
                ctx.requester.name
            ));
        }
        let mut changes = Vec::new();
        if let Some(instruction) = arg(args, "instruction") {
            task.instruction = crate::llm::truncate(&instruction, 1500);
            changes.push("instruction updated");
        }
        if let Some(active) = flag(args, "active") {
            task.active = active;
            changes.push(if active { "resumed" } else { "paused" });
        }
        if let (Some(minutes), TaskTrigger::Schedule { every_minutes, .. }) =
            (number(args, "every_minutes"), &mut task.trigger)
        {
            *every_minutes = minutes.clamp(15, 43_200) as u32;
            changes.push("interval updated");
        }
        if let (Some(at), TaskTrigger::Schedule { at_minute, .. }) = (arg(args, "at"), &mut task.trigger) {
            *at_minute = Some(crate::memory::parse_time_of_day(&at).ok_or_else(|| format!("couldn't read the time \"{at}\""))?);
            changes.push("time updated");
        }
        if let (Some(d), TaskTrigger::Schedule { days, at_minute, .. }) = (arg(args, "days"), &mut task.trigger) {
            if at_minute.is_none() {
                return Err("give the task a time (at) before choosing days".to_owned());
            }
            *days = crate::memory::parse_days(&d).ok_or_else(|| format!("couldn't read the days \"{d}\""))?;
            changes.push("days updated");
        }
        if changes.is_empty() {
            return Err("nothing to change".to_owned());
        }
        Ok(format!("task #{id}: {}", changes.join(", ")))
    })
}

fn delete_task(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let id = number(args, "id").ok_or("missing id")? as u64;
    let result = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        let index = k
            .tasks
            .iter()
            .position(|t| t.id == id)
            .ok_or("no task with that ID")?;
        let task = &k.tasks[index];
        if !may_override(ctx, task.author_id, task.author_rank) {
            return Err(format!(
                "denied: task #{id} was assigned by {} ({}), who outranks {}",
                task.author_name,
                task.author_level.label(),
                ctx.requester.name
            ));
        }
        k.tasks.remove(index);
        Ok(format!("task #{id} stopped and removed"))
    });
    if result.is_ok() {
        ctx.core.feed(
            FeedKind::Coordination,
            format!("task #{id} removed by {}", ctx.requester.name),
            None,
        );
    }
    result
}

// --- coordination -------------------------------------------------------------------

fn list_agents(ctx: &ToolCtx) -> Result<String, String> {
    let lines: Vec<String> = ctx
        .core
        .hive
        .peers()
        .iter()
        .map(|p| {
            format!(
                "{}{} — {} ({}{})",
                p.name,
                if p.id == ctx.core.id { " (you)" } else { "" },
                p.specialty,
                if p.online { "online" } else { "offline" },
                if p.user_id != 0 {
                    format!(", <@{}>", p.user_id)
                } else {
                    String::new()
                }
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

async fn post_to_hq(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let message = required(args, "message")?;
    let hq = ctx
        .core
        .hive
        .knowledge
        .guild(guild_id, |k| k.hq.channel_id)
        .ok_or("there's no agent HQ channel in this server yet")?;
    for chunk in crate::runtime::brain::split_message(&message) {
        ctx.core
            .http
            .create_message(ChannelId::new(hq))
            .content(chunk)
            .allowed_mentions(AllowedMentions::users())
            .await
            .map_err(discord_error)?;
    }
    ctx.core.feed(
        FeedKind::Coordination,
        format!("HQ: {}", crate::llm::truncate(&message, 120)),
        None,
    );
    Ok("posted in HQ".to_owned())
}

async fn ask_agent(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let name = required(args, "agent")?;
    let peer = ctx
        .core
        .hive
        .peer_by_name(&name)
        .ok_or_else(|| format!("no agent named {name}"))?;
    if peer.id == ctx.core.id {
        return Err("that's you".to_owned());
    }
    if peer.user_id == 0 || !peer.online {
        return Err(format!("{} is offline right now", peer.name));
    }
    let request = required(args, "request")?;
    let hq = ctx.core.hive.knowledge.guild(guild_id, |k| k.hq.channel_id);
    let target = hq.unwrap_or(ctx.channel_id);
    let on_behalf = if ctx.requester.kind == PrincipalKind::Human {
        format!(
            "\n-# on behalf of {} in <#{}>",
            ctx.requester.name, ctx.channel_id
        )
    } else {
        String::new()
    };
    ctx.core
        .http
        .create_message(ChannelId::new(target))
        .content(crate::llm::truncate(
            &format!("<@{}> {request}{on_behalf}", peer.user_id),
            1990,
        ))
        .allowed_mentions(AllowedMentions::users())
        .await
        .map_err(discord_error)?;
    ctx.core.feed(
        FeedKind::Coordination,
        format!(
            "asked {}: {}",
            peer.name,
            crate::llm::truncate(&request, 100)
        ),
        None,
    );
    Ok(format!(
        "asked {} {}",
        peer.name,
        if hq.is_some() { "in HQ" } else { "here" }
    ))
}

// --- reminders --------------------------------------------------------------------

async fn set_reminder(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let text = crate::llm::truncate(&required(args, "text")?, 800);
    let minutes = number(args, "in_minutes").unwrap_or(0) as f64
        + args.get("in_hours").and_then(Value::as_f64).unwrap_or(0.0) * 60.0
        + args.get("in_days").and_then(Value::as_f64).unwrap_or(0.0) * 1440.0;
    if minutes < 1.0 {
        return Err("say when: in_minutes, in_hours, or in_days".to_owned());
    }
    if minutes > 60.0 * 24.0 * 365.0 {
        return Err("reminders can be at most a year out".to_owned());
    }
    let target = match arg(args, "user") {
        Some(user) if !matches!(user.to_lowercase().as_str(), "me" | "myself") => {
            let member = super::discord::resolve_member(ctx, &user).await?;
            if member.user_id != ctx.requester.user_id
                && ctx.requester.level < AccessLevel::Moderator
                && ctx.requester.kind != PrincipalKind::Operator
            {
                return Err("denied: only moderators can set reminders for other people".to_owned());
            }
            member.user_id
        }
        _ => ctx.requester.user_id,
    };
    let at = now_ms() + (minutes * 60_000.0) as i64;
    let id = ctx.core.hive.knowledge.guild_mut(guild_id, |k| {
        k.next_task += 1;
        let id = k.next_task;
        k.tasks.push(StandingTask {
            id,
            agent_id: ctx.core.id.clone(),
            instruction: text.clone(),
            trigger: TaskTrigger::Reminder {
                channel_id: ctx.channel_id,
                at,
                user_id: target,
            },
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
        });
        id
    });
    Ok(format!(
        "reminder #{id} set for <t:{}:f> (<t:{}:R>) in this channel",
        at / 1000,
        at / 1000
    ))
}

fn list_reminders(ctx: &ToolCtx) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let staff = ctx.requester.level >= AccessLevel::Moderator;
    let lines: Vec<String> = ctx.core.hive.knowledge.guild(guild_id, |k| {
        k.tasks
            .iter()
            .filter_map(|t| match t.trigger {
                TaskTrigger::Reminder { at, user_id, .. }
                    if staff || user_id == ctx.requester.user_id =>
                {
                    Some(format!(
                        "#{} <t:{}:R> for <@{user_id}>: {}",
                        t.id,
                        at / 1000,
                        t.instruction
                    ))
                }
                _ => None,
            })
            .collect()
    });
    Ok(if lines.is_empty() {
        "no pending reminders (cancel one with delete_task and its #id)".to_owned()
    } else {
        lines.join("\n")
    })
}

// --- free web APIs ------------------------------------------------------------------

async fn get_json(url: &str, query: &[(&str, String)]) -> Result<Value, String> {
    let response = crate::llm::http_client()
        .get(url)
        .query(query)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Err("not found".to_owned());
    }
    if !status.is_success() {
        return Err(format!("the service answered HTTP {status}"));
    }
    response.json().await.map_err(|e| e.to_string())
}

fn weather_text(code: i64) -> &'static str {
    match code {
        0 => "clear",
        1 => "mostly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "foggy",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing drizzle",
        61 | 63 | 65 => "rain",
        66 | 67 => "freezing rain",
        71 | 73 | 75 | 77 => "snow",
        80..=82 => "rain showers",
        85 | 86 => "snow showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm with hail",
        _ => "unknown",
    }
}

async fn weather(location: &str) -> Result<String, String> {
    let places = get_json(
        "https://geocoding-api.open-meteo.com/v1/search",
        &[("name", location.to_owned()), ("count", "1".to_owned())],
    )
    .await?;
    let place = places
        .pointer("/results/0")
        .ok_or_else(|| format!("I couldn't find a place called \"{location}\""))?;
    let lat = place.get("latitude").and_then(Value::as_f64).unwrap_or(0.0);
    let lon = place
        .get("longitude")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let name = format!(
        "{}, {}",
        place
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(location),
        place.get("country").and_then(Value::as_str).unwrap_or("")
    );
    let data = get_json(
        "https://api.open-meteo.com/v1/forecast",
        &[
            ("latitude", lat.to_string()),
            ("longitude", lon.to_string()),
            ("current", "temperature_2m,apparent_temperature,relative_humidity_2m,weather_code,wind_speed_10m".to_owned()),
            ("daily", "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max".to_owned()),
            ("timezone", "auto".to_owned()),
            ("forecast_days", "3".to_owned()),
        ],
    )
    .await?;
    let c = |key: &str| {
        data.pointer(&format!("/current/{key}"))
            .and_then(Value::as_f64)
            .unwrap_or(f64::NAN)
    };
    let f = |celsius: f64| celsius * 9.0 / 5.0 + 32.0;
    let mut out = format!(
        "{name} now: {}, {:.0}°C / {:.0}°F (feels {:.0}°C), humidity {:.0}%, wind {:.0} km/h\n",
        weather_text(c("weather_code") as i64),
        c("temperature_2m"),
        f(c("temperature_2m")),
        c("apparent_temperature"),
        c("relative_humidity_2m"),
        c("wind_speed_10m")
    );
    if let Some(days) = data.pointer("/daily/time").and_then(Value::as_array) {
        for (i, day) in days.iter().enumerate() {
            let get = |key: &str| {
                data.pointer(&format!("/daily/{key}/{i}"))
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN)
            };
            out.push_str(&format!(
                "{}: {}, {:.0}–{:.0}°C ({:.0}–{:.0}°F), rain chance {:.0}%\n",
                day.as_str().unwrap_or("?"),
                weather_text(get("weather_code") as i64),
                get("temperature_2m_min"),
                get("temperature_2m_max"),
                f(get("temperature_2m_min")),
                f(get("temperature_2m_max")),
                get("precipitation_probability_max")
            ));
        }
    }
    Ok(out)
}

async fn wikipedia(query: &str) -> Result<String, String> {
    let search = get_json(
        "https://en.wikipedia.org/w/rest.php/v1/search/title",
        &[("q", query.to_owned()), ("limit", "1".to_owned())],
    )
    .await?;
    let key = search
        .pointer("/pages/0/key")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("no Wikipedia article found for \"{query}\""))?;
    let summary = get_json(
        &format!("https://en.wikipedia.org/api/rest_v1/page/summary/{key}"),
        &[],
    )
    .await?;
    Ok(format!(
        "{}\n{}\n{}",
        summary.get("title").and_then(Value::as_str).unwrap_or(key),
        summary
            .get("extract")
            .and_then(Value::as_str)
            .unwrap_or("(no summary)"),
        summary
            .pointer("/content_urls/desktop/page")
            .and_then(Value::as_str)
            .unwrap_or("")
    ))
}

async fn define_word(word: &str) -> Result<String, String> {
    let word = word.trim().to_lowercase();
    let entries = get_json(
        &format!("https://api.dictionaryapi.dev/api/v2/entries/en/{word}"),
        &[],
    )
    .await
    .map_err(|_| format!("no dictionary entry for \"{word}\""))?;
    let mut out = Vec::new();
    for entry in entries.as_array().into_iter().flatten().take(1) {
        if let Some(phonetic) = entry.get("phonetic").and_then(Value::as_str) {
            out.push(format!("{word} {phonetic}"));
        }
        for meaning in entry
            .get("meanings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(4)
        {
            let part = meaning
                .get("partOfSpeech")
                .and_then(Value::as_str)
                .unwrap_or("");
            for definition in meaning
                .get("definitions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(2)
            {
                let text = definition
                    .get("definition")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let example = definition
                    .get("example")
                    .and_then(Value::as_str)
                    .map(|e| format!(" (e.g. \"{e}\")"))
                    .unwrap_or_default();
                out.push(format!("{part}: {text}{example}"));
            }
            let synonyms: Vec<&str> = meaning
                .get("synonyms")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(6)
                .collect();
            if !synonyms.is_empty() {
                out.push(format!("synonyms: {}", synonyms.join(", ")));
            }
        }
    }
    Ok(out.join("\n"))
}

async fn convert_currency(args: &Value) -> Result<String, String> {
    let amount = args
        .get("amount")
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or("missing amount")?;
    let from = required(args, "from")?.to_uppercase();
    let to = required(args, "to")?.to_uppercase();
    let data = get_json(
        "https://api.frankfurter.app/latest",
        &[
            ("amount", amount.to_string()),
            ("from", from.clone()),
            ("to", to.clone()),
        ],
    )
    .await
    .map_err(|_| format!("couldn't convert {from} to {to} (unknown currency code?)"))?;
    let value = data
        .pointer(&format!("/rates/{to}"))
        .and_then(Value::as_f64)
        .ok_or("no rate in the response")?;
    Ok(format!(
        "{amount} {from} = {value:.2} {to} (ECB rate of {})",
        data.get("date").and_then(Value::as_str).unwrap_or("today")
    ))
}

async fn create_invite(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let channel_id = super::discord::resolve_channel(ctx, arg(args, "channel"))?;
    ctx.require_in(channel_id, Permissions::CREATE_INSTANT_INVITE)?;
    let hours = number(args, "hours").unwrap_or(24).clamp(0, 168) as u32;
    let uses = number(args, "max_uses").unwrap_or(0).clamp(0, 100) as u32;
    let invite = ctx
        .core
        .http
        .create_invite(ChannelId::new(channel_id))
        .max_age(hours * 3600)
        .max_uses(uses)
        .reason(ctx.reason(None))
        .await
        .map_err(discord_error)?;
    Ok(format!(
        "{} ({}, {})",
        invite.url(),
        if hours == 0 {
            "never expires".to_owned()
        } else {
            format!("expires in {hours}h")
        },
        if uses == 0 {
            "unlimited uses".to_owned()
        } else {
            format!("{uses} uses")
        }
    ))
}

fn server_activity(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let channels: Vec<u64> = match arg(args, "channel") {
        Some(channel) => vec![super::discord::resolve_channel(ctx, Some(channel))?],
        None => ctx
            .core
            .world
            .with_guild(guild_id, |g| g.channels.keys().copied().collect())
            .unwrap_or_default(),
    };
    let day_ago = now_ms() - 86_400_000;
    let mut per_channel: Vec<(String, usize, usize)> = Vec::new();
    let mut per_person: std::collections::HashMap<String, (usize, usize)> = Default::default();
    for channel in channels {
        if ctx
            .require_in(
                channel,
                Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
            )
            .is_err()
        {
            continue;
        }
        let (name, total, today) = ctx.core.hive.knowledge.channel(channel, |archive| {
            let mut today = 0;
            for message in archive
                .messages
                .iter()
                .filter(|m| !m.deleted && !m.author_bot)
            {
                let entry = per_person.entry(message.author.clone()).or_insert((0, 0));
                entry.0 += 1;
                if message.ts >= day_ago {
                    entry.1 += 1;
                    today += 1;
                }
            }
            (archive.name.clone(), archive.messages.len(), today)
        });
        if total > 0 {
            per_channel.push((name, total, today));
        }
    }
    per_channel.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
    let mut people: Vec<_> = per_person.into_iter().collect();
    people.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(b.1.0.cmp(&a.1.0)));
    let mut out = String::from("Busiest channels (messages in the last 24h / kept history):\n");
    for (name, total, today) in per_channel.iter().take(10) {
        out.push_str(&format!("{name}: {today} / {total}\n"));
    }
    out.push_str("\nMost active people (24h / kept history):\n");
    for (name, (total, today)) in people.iter().take(10) {
        out.push_str(&format!("{name}: {today} / {total}\n"));
    }
    out.push_str("\n(Based on the recent history agents keep, up to 400 messages per channel.)");
    Ok(out)
}

// --- utilities ----------------------------------------------------------------------

fn get_time(args: &Value) -> Result<String, String> {
    let offset = args
        .get("utc_offset_hours")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(-14.0, 14.0);
    let millis = now_ms() + (offset * 3_600_000.0) as i64;
    let ts = axis::prelude::Timestamp::from_unix_millis(millis).map_err(|e| e.to_string())?;
    let dt = ts.as_datetime();
    Ok(format!(
        "{}, {:04}-{:02}-{:02} {:02}:{:02} (UTC{}{})",
        dt.weekday(),
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        if offset >= 0.0 { "+" } else { "" },
        offset
    ))
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value:.10}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// A small recursive-descent calculator.
pub fn calculate(expression: &str) -> Result<f64, String> {
    struct Parser<'a> {
        chars: std::iter::Peekable<std::str::Chars<'a>>,
    }
    impl Parser<'_> {
        fn skip(&mut self) {
            while self.chars.peek().is_some_and(|c| c.is_whitespace()) {
                self.chars.next();
            }
        }
        fn expr(&mut self) -> Result<f64, String> {
            let mut value = self.term()?;
            loop {
                self.skip();
                match self.chars.peek() {
                    Some('+') => {
                        self.chars.next();
                        value += self.term()?;
                    }
                    Some('-') => {
                        self.chars.next();
                        value -= self.term()?;
                    }
                    _ => return Ok(value),
                }
            }
        }
        fn term(&mut self) -> Result<f64, String> {
            let mut value = self.power()?;
            loop {
                self.skip();
                match self.chars.peek() {
                    Some('*') | Some('×') => {
                        self.chars.next();
                        value *= self.power()?;
                    }
                    Some('/') | Some('÷') => {
                        self.chars.next();
                        let divisor = self.power()?;
                        if divisor == 0.0 {
                            return Err("division by zero".to_owned());
                        }
                        value /= divisor;
                    }
                    Some('%') => {
                        self.chars.next();
                        value %= self.power()?;
                    }
                    _ => return Ok(value),
                }
            }
        }
        fn power(&mut self) -> Result<f64, String> {
            let base = self.unary()?;
            self.skip();
            if self.chars.peek() == Some(&'^') {
                self.chars.next();
                let exponent = self.power()?;
                return Ok(base.powf(exponent));
            }
            Ok(base)
        }
        fn unary(&mut self) -> Result<f64, String> {
            self.skip();
            match self.chars.peek() {
                Some('-') => {
                    self.chars.next();
                    Ok(-self.unary()?)
                }
                Some('+') => {
                    self.chars.next();
                    self.unary()
                }
                _ => self.atom(),
            }
        }
        fn atom(&mut self) -> Result<f64, String> {
            self.skip();
            match self.chars.peek().copied() {
                Some('(') => {
                    self.chars.next();
                    let value = self.expr()?;
                    self.skip();
                    if self.chars.next() != Some(')') {
                        return Err("missing )".to_owned());
                    }
                    Ok(value)
                }
                Some(c) if c.is_ascii_digit() || c == '.' => {
                    let mut number = String::new();
                    while let Some(&c) = self.chars.peek() {
                        if c.is_ascii_digit() || c == '.' || c == 'e' && !number.contains('e') {
                            number.push(c);
                            self.chars.next();
                        } else if c == ',' || c == '_' {
                            // Digit grouping: 1,000 or 1_000.
                            self.chars.next();
                        } else {
                            break;
                        }
                    }
                    number.parse().map_err(|_| format!("bad number {number}"))
                }
                Some(c) if c.is_alphabetic() || c == 'π' => {
                    let mut name = String::new();
                    while let Some(&c) = self.chars.peek() {
                        if c.is_alphanumeric() || c == 'π' {
                            name.push(c);
                            self.chars.next();
                        } else {
                            break;
                        }
                    }
                    match name.to_lowercase().as_str() {
                        "pi" | "π" => return Ok(std::f64::consts::PI),
                        "e" => return Ok(std::f64::consts::E),
                        _ => {}
                    }
                    let argument = self.atom()?;
                    Ok(match name.to_lowercase().as_str() {
                        "sqrt" => argument.sqrt(),
                        "abs" => argument.abs(),
                        "ln" => argument.ln(),
                        "log" => argument.log10(),
                        "sin" => argument.sin(),
                        "cos" => argument.cos(),
                        "tan" => argument.tan(),
                        "round" => argument.round(),
                        "floor" => argument.floor(),
                        "ceil" => argument.ceil(),
                        "exp" => argument.exp(),
                        other => return Err(format!("unknown function {other}")),
                    })
                }
                Some(c) => Err(format!("unexpected '{c}'")),
                None => Err("unexpected end".to_owned()),
            }
        }
    }
    let mut parser = Parser {
        chars: expression.chars().peekable(),
    };
    let value = parser.expr()?;
    parser.skip();
    if let Some(c) = parser.chars.next() {
        return Err(format!("unexpected '{c}'"));
    }
    if value.is_nan() || value.is_infinite() {
        return Err("the result is not a finite number".to_owned());
    }
    Ok(value)
}

fn roll_dice(spec: &str) -> Result<String, String> {
    let spec = spec.trim().to_lowercase().replace(' ', "");
    if spec == "coin" || spec == "flip" {
        return Ok(if fastrand::bool() {
            "heads".to_owned()
        } else {
            "tails".to_owned()
        });
    }
    let (dice, modifier) = match spec.find(['+', '-']) {
        Some(at) => (
            &spec[..at],
            spec[at..].parse::<i64>().map_err(|_| "bad modifier")?,
        ),
        None => (spec.as_str(), 0),
    };
    let (count, sides) = dice.split_once('d').ok_or("use notation like 2d6+1")?;
    let count: u32 = if count.is_empty() {
        1
    } else {
        count.parse().map_err(|_| "bad dice count")?
    };
    let sides: u32 = sides.parse().map_err(|_| "bad number of sides")?;
    if !(1..=100).contains(&count) || !(2..=1000).contains(&sides) {
        return Err("1-100 dice with 2-1000 sides".to_owned());
    }
    let rolls: Vec<u32> = (0..count).map(|_| fastrand::u32(1..=sides)).collect();
    let total: i64 = rolls.iter().map(|r| *r as i64).sum::<i64>() + modifier;
    Ok(format!("rolled {spec}: {:?} → {total}", rolls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_from_plain_words() {
        assert_eq!(find_time("every day at 9am post a quote"), Some(540));
        assert_eq!(find_time("every monday and friday at 6 pm remind everyone"), Some(18 * 60));
        assert_eq!(find_time("post it at 9"), Some(540));
        assert_eq!(find_time("every 2 hours check the news"), None);
        assert_eq!(find_time("tomorrow at 15:30 post the link"), Some(15 * 60 + 30));
        assert_eq!(find_interval("every 2 hours check the news"), Some(120));
        assert_eq!(find_interval("every 30 minutes"), Some(30));
        assert_eq!(find_interval("hourly updates"), Some(60));
        assert_eq!(find_in("in 45 minutes post it"), Some(45));
        assert!(sounds_like_instruction("Look up today's top tech news and post a summary"));
        assert!(sounds_like_instruction("a /news command that makes you look up the news"));
        assert!(!sounds_like_instruction("**Server Rules**: be nice, no spam"));
    }

    #[test]
    fn calculator_handles_precedence_and_functions() {
        assert_eq!(calculate("1 + 2 * 3").unwrap(), 7.0);
        assert_eq!(calculate("(1 + 2) * 3").unwrap(), 9.0);
        assert_eq!(calculate("2 ^ 3 ^ 2").unwrap(), 512.0);
        assert_eq!(calculate("-3 + 5").unwrap(), 2.0);
        assert_eq!(calculate("sqrt(16) + abs(-2)").unwrap(), 6.0);
        assert!((calculate("pi").unwrap() - std::f64::consts::PI).abs() < 1e-12);
        assert_eq!(calculate("1,000 * 3").unwrap(), 3000.0);
        assert!(calculate("1 / 0").is_err());
        assert!(calculate("2 +").is_err());
        assert_eq!(format_number(calculate("10 / 4").unwrap()), "2.5");
    }

    #[test]
    fn dice_roll_within_bounds() {
        for _ in 0..50 {
            let text = roll_dice("3d6+2").unwrap();
            let total: i64 = text.rsplit("→ ").next().unwrap().parse().unwrap();
            assert!((5..=20).contains(&total));
        }
        assert!(roll_dice("0d6").is_err());
        assert!(roll_dice("coin").is_ok());
    }
}
