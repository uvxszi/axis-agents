//! The few slash commands people need. Everything else happens in
//! conversation — agents are talked to, not operated.
//!
//! - `/ask` — a private answer only you see
//! - `/memories` — what this agent remembers about you
//! - `/forget` — erase it (and optionally opt out of notes)
//! - `/quiet` — quiet the agent here (moderators) or towards you
//! - `/tasks` — this agent's standing tasks
//! - `/status` — model, uptime, activity
//! - `/setup` — everything it runs here, with buttons and forms
//! - `/rank`, `/leaderboard` — levels
//! - `/ping` — latency
//!
//! Each can be turned off in the app (Commands tab), which also holds
//! custom commands that reply with fixed text.

use std::sync::{Arc, atomic::Ordering};

use axis::model::{Interaction, InteractionData};
use axis::prelude::*;
use axis_commands::{CommandOption, CommandRegistry, Invocation};

use super::agent::AgentCore;
use super::brain::{self, Job, Outlet, Reason};
use crate::config::{CommandMode, CustomCommand};
use crate::memory::{QuietRule, now_ms};

fn requester_user(invocation: &Invocation) -> Option<User> {
    invocation
        .interaction
        .member
        .as_ref()
        .and_then(|m| m.user.clone())
        .or_else(|| invocation.interaction.user.clone())
}

/// The built-in commands, for the app's Commands tab.
pub const BUILT_IN: [(&str, &str); 10] = [
    ("ask", "a private answer only you see"),
    ("memories", "what it remembers about you"),
    ("forget", "erase that (or opt out of notes)"),
    ("quiet", "quiet it here, or towards you"),
    ("tasks", "its standing tasks"),
    ("setup", "everything it runs in a server, with buttons"),
    ("rank", "your level and rank card"),
    ("leaderboard", "top members by XP"),
    ("status", "model, uptime, activity"),
    ("ping", "gateway and API latency"),
];

/// Fill {user}, {server}, {channel}, {input} in a custom command's text.
fn fill(core: &AgentCore, template: &str, interaction: &Interaction, input: Option<&str>) -> String {
    let user = interaction.member.as_ref().and_then(|m| m.user.clone()).or_else(|| interaction.user.clone());
    let server = interaction
        .guild_id
        .and_then(|g| core.world.with_guild(g.get(), |s| s.name.clone()))
        .unwrap_or_else(|| "DMs".to_owned());
    let channel = interaction.channel_id.map(|c| format!("<#{c}>")).unwrap_or_default();
    template
        .replace("{user}", &user.map(|u| format!("<@{}>", u.id)).unwrap_or_default())
        .replace("{server}", &server)
        .replace("{channel}", &channel)
        .replace("{input}", input.unwrap_or(""))
}

/// A custom command as it runs: set in the app, or made in a server.
#[derive(Debug, Clone, PartialEq)]
pub struct Custom {
    pub name: String,
    pub description: String,
    pub text: String,
    pub mode: CommandMode,
    pub private: bool,
    pub takes_input: bool,
}

fn usable(name: &str, text: &str, taken: &[String]) -> bool {
    CustomCommand::valid_name(name)
        && !BUILT_IN.iter().any(|(n, _)| *n == name)
        && !text.trim().is_empty()
        && !taken.iter().any(|t| t == name)
}

/// The custom commands registered globally (`None`) or in one server.
pub fn customs(core: &AgentCore, guild: Option<u64>) -> Vec<Custom> {
    let profile = core.profile();
    if !profile.behavior.slash_commands {
        return Vec::new();
    }
    // A server's own commands can't reuse a global command's name.
    let mut taken: Vec<String> = match guild {
        Some(_) => customs(core, None).into_iter().map(|c| c.name).collect(),
        None => Vec::new(),
    };
    let mut out = Vec::new();
    for c in &profile.behavior.custom_commands {
        let here = match guild {
            None => c.servers.is_empty(),
            Some(g) => c.servers.contains(&g),
        };
        let name = c.name.trim().to_lowercase();
        if here && usable(&name, &c.reply, &taken) {
            taken.push(name.clone());
            out.push(Custom {
                name,
                description: c.description.clone(),
                text: c.reply.clone(),
                mode: c.mode,
                private: c.private,
                takes_input: c.takes_input,
            });
        }
    }
    if let Some(g) = guild {
        let made = core.hive.knowledge.guild(g, |k| {
            k.commands.iter().filter(|c| c.agent_id == core.id && !c.disabled).cloned().collect::<Vec<_>>()
        });
        for c in made {
            if usable(&c.name, &c.text, &taken) {
                taken.push(c.name.clone());
                out.push(Custom {
                    name: c.name,
                    description: c.description,
                    text: c.text,
                    mode: c.mode,
                    private: c.private,
                    takes_input: c.takes_input,
                });
            }
        }
    }
    out
}

/// A custom command as Discord wants it.
fn custom_json(c: &Custom) -> serde_json::Value {
    let description = match c.description.trim() {
        "" => match c.mode {
            CommandMode::Reply => "A custom command".to_owned(),
            CommandMode::Agent => "Ask the agent to do this".to_owned(),
        },
        d => d.chars().take(100).collect(),
    };
    let mut command = serde_json::json!({ "name": c.name, "description": description, "type": 1 });
    if c.takes_input {
        command["options"] = serde_json::json!([{ "type": 3, "name": "input", "description": "Details to go with it", "required": false }]);
    }
    command
}

fn build(core: &Arc<AgentCore>) -> CommandRegistry {
    let mut registry = CommandRegistry::new();
    let name = core.name();

    let c = Arc::clone(core);
    registry.command(
        "ask",
        format!("Ask {name} something privately"),
        move |cmd| {
            cmd.option(
                CommandOption::string("question", "What do you want to ask?")
                    .required()
                    .finish(),
            );
            cmd.handler(move |invocation| {
                let core = Arc::clone(&c);
                async move {
                    let question: String = invocation.arg("question")?;
                    invocation.defer(true).await?;
                    let Some(user) = requester_user(&invocation) else {
                        return Ok(());
                    };
                    let guild_id = invocation.interaction.guild_id.map(|g| g.get());
                    if let Some(guild_id) = guild_id {
                        if let Some(member) = &invocation.interaction.member {
                            let mut member = member.clone();
                            member.user = Some(user.clone());
                            core.world.upsert_member(guild_id, &member);
                        }
                    }
                    let job = Job {
                        guild_id,
                        channel_id: invocation
                            .interaction
                            .channel_id
                            .map(|c| c.get())
                            .unwrap_or(0),
                        triggers: Vec::new(),
                        reason: Reason::Ask,
                        requester: core.principal_for(guild_id, &user),
                        task: None,
                        outlet: Outlet::Interaction {
                            application_id: invocation.interaction.application_id.get(),
                            token: invocation.interaction.token.clone(),
                        },
                        question: Some(question),
                    };
                    tokio::spawn(async move { brain::run(&core, job).await });
                    Ok(())
                }
            });
        },
    );

    let c = Arc::clone(core);
    registry.command(
        "memories",
        format!("See what {name} remembers about you"),
        move |cmd| {
            cmd.handler(move |invocation| {
                let core = Arc::clone(&c);
                async move {
                    let Some(user) = requester_user(&invocation) else {
                        return Ok(());
                    };
                    let memories = core.memory.about_user(user.id.get());
                    let note = invocation.interaction.guild_id.and_then(|g| {
                        core.hive
                            .knowledge
                            .guild(g.get(), |k| k.people.get(&user.id.get()).cloned())
                    });
                    let mut text = String::new();
                    if let Some(note) = &note {
                        if !note.notes.is_empty() {
                            text.push_str(&format!(
                                "**Shared notes (all agents):** {}\n",
                                note.notes
                            ));
                        }
                        if note.opted_out {
                            text.push_str("-# You've opted out of notes.\n");
                        }
                    }
                    if memories.is_empty() {
                        text.push_str(&format!("{} has no saved memories about you.", core.name()));
                    } else {
                        text.push_str(&format!("**{} remembers:**\n", core.name()));
                        for memory in memories.iter().rev().take(25) {
                            text.push_str(&format!("• #{} {}\n", memory.id, memory.text));
                        }
                    }
                    invocation
                        .reply()
                        .content(crate::llm::truncate(&text, 1990))
                        .ephemeral(true)
                        .await?;
                    Ok(())
                }
            });
        },
    );

    let c = Arc::clone(core);
    registry.command(
        "forget",
        format!("Make {name} forget what it knows about you"),
        move |cmd| {
            cmd.option(
                CommandOption::boolean(
                    "stop_noting",
                    "Also stop all agents from keeping notes about you",
                )
                .finish(),
            );
            cmd.handler(move |invocation| {
                let core = Arc::clone(&c);
                async move {
                    let Some(user) = requester_user(&invocation) else {
                        return Ok(());
                    };
                    let opt_out = invocation.opt::<bool>("stop_noting")?.unwrap_or(false);
                    let removed = core.memory.forget_user(user.id.get());
                    if let Some(guild) = invocation.interaction.guild_id {
                        core.hive.knowledge.guild_mut(guild.get(), |k| {
                            let note = k.people.entry(user.id.get()).or_default();
                            note.notes.clear();
                            if opt_out {
                                note.opted_out = true;
                            }
                        });
                    }
                    invocation
                        .reply()
                        .content(format!(
                            "Done — forgot {removed} memor{} about you{}.",
                            if removed == 1 { "y" } else { "ies" },
                            if opt_out {
                                ", and agents won't keep notes on you anymore"
                            } else {
                                ""
                            }
                        ))
                        .ephemeral(true)
                        .await?;
                    Ok(())
                }
            });
        },
    );

    let c = Arc::clone(core);
    registry.command("quiet", format!("Ask {name} to stay quiet for a while"), move |cmd| {
        cmd.option(CommandOption::integer("minutes", "How long (default 60)").min_value(1.0).max_value(10_080.0).finish());
        cmd.handler(move |invocation| {
            let core = Arc::clone(&c);
            async move {
                let Some(user) = requester_user(&invocation) else {
                    return Ok(());
                };
                let Some(guild) = invocation.interaction.guild_id.map(|g| g.get()) else {
                    invocation.reply().content("In DMs I only answer when you write to me.").ephemeral(true).await?;
                    return Ok(());
                };
                let minutes = invocation.opt::<i64>("minutes")?.unwrap_or(60).clamp(1, 10_080);
                let channel = invocation.interaction.channel_id.map(|c| c.get()).unwrap_or(0);
                let principal = core.principal_for(Some(guild), &user);
                let is_mod = invocation
                    .interaction
                    .member
                    .as_ref()
                    .and_then(|m| m.permissions)
                    .is_some_and(|p| p.contains(Permissions::MANAGE_MESSAGES) || p.is_administrator());
                let rule = QuietRule {
                    channel_id: is_mod.then_some(channel),
                    user_id: (!is_mod).then_some(user.id.get()),
                    agent_id: Some(core.id.clone()),
                    setter_rank: principal.rank,
                    until: now_ms() + minutes * 60_000,
                };
                core.hive.knowledge.guild_mut(guild, |k| k.quiet.push(rule));
                invocation
                    .reply()
                    .content(if is_mod {
                        format!("🤫 I'll stay quiet in this channel for {minutes} minutes (people ranked at least as high as you can still @mention me).")
                    } else {
                        format!("🤫 I'll leave you be for {minutes} minutes — @mention me if you want me back sooner.")
                    })
                    .ephemeral(true)
                    .await?;
                Ok(())
            }
        });
    });

    let c = Arc::clone(core);
    registry.command("tasks", format!("See {name}'s standing tasks"), move |cmd| {
        cmd.handler(move |invocation| {
            let core = Arc::clone(&c);
            async move {
                let text = match invocation.interaction.guild_id.map(|g| g.get()) {
                    Some(guild) => {
                        let tasks = core.hive.knowledge.guild(guild, |k| k.tasks.clone());
                        let lines: Vec<String> = tasks
                            .iter()
                            .filter(|t| t.agent_id == core.id)
                            .map(|t| format!("• {}", crate::tools::extra::describe_task(&core, guild, t)))
                            .collect();
                        if lines.is_empty() {
                            format!("{} has no standing tasks here. Moderators can give one in chat, e.g. \"{}, watch #suggestions and …\"", core.name(), core.name())
                        } else {
                            format!("**{}'s tasks**\n{}\n-# Ask me in chat to change, pause, or stop one.", core.name(), lines.join("\n"))
                        }
                    }
                    None => "Tasks live in servers.".to_owned(),
                };
                invocation.reply().content(crate::llm::truncate(&text, 1990)).ephemeral(true).await?;
                Ok(())
            }
        });
    });

    let c = Arc::clone(core);
    registry.command("setup", format!("See and change what {name} runs in this server"), move |cmd| {
        cmd.option(
            CommandOption::string("section", "Which part")
                .choice("Overview", "overview")
                .choice("Leveling", "leveling")
                .choice("Rank cards", "card")
                .choice("Statistics", "stats")
                .choice("Tickets", "tickets")
                .choice("Who can change settings", "access")
                .choice("This agent", "agent")
                .finish(),
        );
        cmd.handler(move |invocation| {
            let core = Arc::clone(&c);
            async move {
                let Some(guild_id) = invocation.interaction.guild_id.map(|g| g.get()) else {
                    invocation.reply().content("Setup is per server.").ephemeral(true).await?;
                    return Ok(());
                };
                let Some(user) = requester_user(&invocation) else { return Ok(()) };
                if let Some(member) = &invocation.interaction.member {
                    let mut member = member.clone();
                    member.user = Some(user.clone());
                    core.world.upsert_member(guild_id, &member);
                }
                let section: String = invocation.opt("section")?.unwrap_or_else(|| "overview".to_owned());
                let who = core.principal_for(Some(guild_id), &user);
                let can_edit = super::levels::can_manage(&core, guild_id, &who);
                let is_owner = who.level >= crate::config::AccessLevel::Owner;
                let mut data = super::setup::render(&core, guild_id, &section, can_edit, is_owner);
                data["flags"] = serde_json::json!(64);
                let interaction = &invocation.interaction;
                let _ = core
                    .http
                    .raw(
                        "POST",
                        &format!("/interactions/{}/{}/callback", interaction.id, interaction.token),
                        Some(serde_json::json!({ "type": 4, "data": data })),
                        None,
                    )
                    .await;
                Ok(())
            }
        });
    });

    let c = Arc::clone(core);
    registry.command("rank", "Your level and rank card (or someone else's)", move |cmd| {
        cmd.option(CommandOption::user("user", "Whose card").finish());
        cmd.handler(move |invocation| {
            let core = Arc::clone(&c);
            async move {
                let Some(guild_id) = invocation.interaction.guild_id.map(|g| g.get()) else {
                    invocation.reply().content("Levels are per server.").ephemeral(true).await?;
                    return Ok(());
                };
                invocation.defer(false).await?;
                let target: Option<String> = invocation.opt("user")?;
                let user_id = target
                    .and_then(|t| t.parse().ok())
                    .or_else(|| requester_user(&invocation).map(|u| u.id.get()))
                    .unwrap_or(0);
                let data = axis::model::InteractionMessageCallbackData::default();
                let interaction = &invocation.interaction;
                match super::levels::rank_card(&core, guild_id, user_id).await {
                    Ok((png, _)) => {
                        let _ = core.http.create_followup(interaction.application_id, interaction.token.clone(), data).file("rank.png", png).await;
                    }
                    Err(error) => {
                        let mut data = data;
                        data.content = Some(format!("Couldn't draw the card: {error}"));
                        let _ = core.http.create_followup(interaction.application_id, interaction.token.clone(), data).await;
                    }
                }
                Ok(())
            }
        });
    });

    let c = Arc::clone(core);
    registry.command("leaderboard", "Top members by XP", move |cmd| {
        cmd.handler(move |invocation| {
            let core = Arc::clone(&c);
            async move {
                let text = match invocation.interaction.guild_id.map(|g| g.get()) {
                    Some(guild_id) => format!("**🏆 Leaderboard**\n{}", super::levels::leaderboard(&core, guild_id, 10)),
                    None => "Levels are per server.".to_owned(),
                };
                // Names shown, nobody pinged.
                let interaction = &invocation.interaction;
                let _ = core
                    .http
                    .raw(
                        "POST",
                        &format!("/interactions/{}/{}/callback", interaction.id, interaction.token),
                        Some(serde_json::json!({ "type": 4, "data": {
                            "content": crate::llm::truncate(&text, 1990),
                            "allowed_mentions": { "parse": [] },
                        } })),
                        None,
                    )
                    .await;
                Ok(())
            }
        });
    });

    let c = Arc::clone(core);
    registry.command("status", format!("How {name} is doing"), move |cmd| {
        cmd.handler(move |invocation| {
            let core = Arc::clone(&c);
            async move {
                let live = core.live().snapshot(&core.id);
                let profile = core.profile();
                let uptime = live
                    .started_at
                    .map(|t| {
                        let secs = t.elapsed().as_secs();
                        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
                    })
                    .unwrap_or_default();
                let text = format!(
                    "**{}** — {}\n• Model: `{}` ({}, {} token context)\n• Up {uptime} · {} replies · {} tool calls · {} memories\n• Tokens: {} in / {} out{}",
                    profile.name,
                    profile.specialty,
                    profile.model.model,
                    profile.model.provider.label(),
                    core.context_window(),
                    live.stats.replies,
                    live.stats.tool_calls,
                    core.memory.len(),
                    live.stats.tokens_in,
                    live.stats.tokens_out,
                    if core.paused.load(Ordering::Relaxed) { "\n• ⏸ paused by the operator" } else { "" }
                );
                invocation.reply().content(text).ephemeral(true).await?;
                Ok(())
            }
        });
    });
    let c = Arc::clone(core);
    registry.command("ping", "Check the latency", move |cmd| {
        cmd.handler(move |invocation| {
            let core = Arc::clone(&c);
            async move {
                let started = std::time::Instant::now();
                let api = invocation.http.get_gateway().await.map(|_| started.elapsed().as_millis());
                let gateway = core
                    .messenger
                    .lock()
                    .expect("messenger poisoned")
                    .as_ref()
                    .and_then(|m| m.latency())
                    .map(|d| format!("{}ms", d.as_millis()))
                    .unwrap_or_else(|| "measuring…".to_owned());
                let api = api.map(|ms| format!("{ms}ms")).unwrap_or_else(|_| "unreachable".to_owned());
                invocation
                    .reply()
                    .content(format!("🏓 Pong! Gateway: **{gateway}** · API: **{api}**"))
                    .await?;
                Ok(())
            }
        });
    });

    registry
}

/// The parts of a command Discord stores that matter for "did it change",
/// in one comparable shape (Discord adds ids, versions and defaults).
fn normalize(command: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Value, json};
    fn option(o: &Value) -> Value {
        json!({
            "type": o.get("type").cloned().unwrap_or(Value::Null),
            "name": o.get("name").cloned().unwrap_or(Value::Null),
            "description": o.get("description").cloned().unwrap_or(Value::Null),
            "required": o.get("required").and_then(Value::as_bool).unwrap_or(false),
            "choices": o.get("choices").and_then(Value::as_array).map(|c| c.iter().map(|c| json!([c.get("name"), c.get("value")])).collect::<Vec<_>>()).unwrap_or_default(),
            "options": o.get("options").and_then(Value::as_array).map(|os| os.iter().map(option).collect::<Vec<_>>()).unwrap_or_default(),
            // 1 and 1.0 are the same limit.
            "min": o.get("min_value").and_then(Value::as_f64),
            "max": o.get("max_value").and_then(Value::as_f64),
            "min_length": o.get("min_length").and_then(Value::as_u64),
            "max_length": o.get("max_length").and_then(Value::as_u64),
        })
    }
    let perms = match command.get("default_member_permissions") {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Number(n)) => Value::String(n.to_string()),
        _ => Value::Null,
    };
    json!({
        "name": command.get("name").cloned().unwrap_or(Value::Null),
        "type": command.get("type").and_then(Value::as_u64).unwrap_or(1),
        "description": command.get("description").cloned().unwrap_or(Value::Null),
        "options": command.get("options").and_then(Value::as_array).map(|os| os.iter().map(option).collect::<Vec<_>>()).unwrap_or_default(),
        "default_member_permissions": perms,
    })
}

/// Same commands (in any order)?
pub fn same_commands(wanted: &[serde_json::Value], registered: &[serde_json::Value]) -> bool {
    let sorted = |list: &[serde_json::Value]| {
        let mut list: Vec<serde_json::Value> = list.iter().map(normalize).collect();
        list.sort_by_key(|c| c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_owned());
        list
    };
    sorted(wanted) == sorted(registered)
}

/// Command syncing: only on request (startup, the app's Apply button, an
/// agent making a command), at most once per [`GAP`], and only touching
/// Discord where something changed.
#[derive(Default)]
pub struct SyncState {
    pending: std::sync::atomic::AtomicBool,
    last: std::sync::Mutex<Option<std::time::Instant>>,
    running: tokio::sync::Mutex<()>,
    /// The command settings at the last sync (see [`fingerprint`]).
    pub applied: std::sync::Mutex<Option<String>>,
    /// What the last sync did, for the app.
    pub status: std::sync::Mutex<String>,
}

const GAP: std::time::Duration = std::time::Duration::from_secs(20);

/// The command settings, to tell whether the app has unapplied changes.
pub fn fingerprint(profile: &crate::config::AgentProfile) -> String {
    let b = &profile.behavior;
    serde_json::to_string(&(b.slash_commands, &b.disabled_commands, &b.custom_commands)).unwrap_or_default()
}

fn set_status(core: &AgentCore, text: impl Into<String>) {
    *core.command_sync.status.lock().expect("status poisoned") = text.into();
}

/// Bring Discord's commands in line with the settings — soon, but never
/// more than once per [`GAP`] (requests in between fold into one sync).
pub fn request_sync(core: &Arc<AgentCore>) {
    if core.command_sync.pending.swap(true, Ordering::SeqCst) {
        return;
    }
    let core = Arc::clone(core);
    tokio::spawn(async move {
        let wait = core
            .command_sync
            .last
            .lock()
            .expect("sync poisoned")
            .map(|t| GAP.saturating_sub(t.elapsed()))
            .unwrap_or_default();
        if !wait.is_zero() {
            set_status(&core, format!("applying in {}s (Discord rate limits)", wait.as_secs().max(1)));
            tokio::time::sleep(wait).await;
        }
        core.command_sync.pending.store(false, Ordering::SeqCst);
        sync(&core).await;
    });
}

/// Replace the commands at `path` when they differ from `wanted`.
async fn put_if_changed(core: &AgentCore, path: &str, wanted: Vec<serde_json::Value>) -> Result<bool, String> {
    match core.http.raw("GET", path, None, None).await {
        Ok(serde_json::Value::Array(registered)) if same_commands(&wanted, &registered) => return Ok(false),
        // Can't read them (e.g. no access to that server's commands) and
        // nothing to add: leave it alone.
        Err(_) if wanted.is_empty() => return Ok(false),
        _ => {}
    }
    core.http
        .raw("PUT", path, Some(serde_json::Value::Array(wanted)), None)
        .await
        .map(|_| true)
        .map_err(|e| e.to_string())
}

async fn sync(core: &Arc<AgentCore>) {
    let _one_at_a_time = core.command_sync.running.lock().await;
    let app_id = core.application_id.load(Ordering::Relaxed);
    if app_id == 0 {
        return;
    }
    let profile = core.profile();
    let enabled = profile.behavior.slash_commands;
    let registry = build(core);
    if let Err(error) = registry.validate() {
        tracing::error!(%error, "command declarations are invalid");
        set_status(core, format!("invalid commands: {error}"));
        return;
    }
    set_status(core, "applying…");
    let disabled = &profile.behavior.disabled_commands;
    let mut global: Vec<serde_json::Value> = if enabled {
        registry
            .to_fields()
            .iter()
            .filter(|f| f.name.as_deref().is_none_or(|n| !disabled.iter().any(|d| d == n)))
            .filter_map(|f| serde_json::to_value(f).ok())
            .collect()
    } else {
        Vec::new()
    };
    global.extend(customs(core, None).iter().map(custom_json));
    let mut updated = Vec::new();
    let mut errors = Vec::new();
    match put_if_changed(core, &format!("/applications/{app_id}/commands"), global).await {
        Ok(true) => updated.push("global".to_owned()),
        Ok(false) => {}
        Err(error) => errors.push(format!("global: {error}")),
    }
    for guild_id in core.world.guild_ids() {
        let wanted: Vec<serde_json::Value> = customs(core, Some(guild_id)).iter().map(custom_json).collect();
        let name = core.world.with_guild(guild_id, |g| g.name.clone()).unwrap_or_else(|| guild_id.to_string());
        match put_if_changed(core, &format!("/applications/{app_id}/guilds/{guild_id}/commands"), wanted).await {
            Ok(true) => updated.push(name),
            Ok(false) => {}
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    *core.command_sync.last.lock().expect("sync poisoned") = Some(std::time::Instant::now());
    *core.commands.lock().expect("commands poisoned") = Some(Arc::new(registry));
    *core.command_sync.applied.lock().expect("sync poisoned") = Some(fingerprint(&profile));
    let status = if !errors.is_empty() {
        format!("⚠ {}", errors.join("; "))
    } else if updated.is_empty() {
        "up to date".to_owned()
    } else {
        format!("applied ({})", updated.join(", "))
    };
    tracing::info!(%status, "slash commands");
    set_status(core, status);
}

/// Run a custom command.
async fn run_custom(core: &Arc<AgentCore>, interaction: &Interaction, custom: &Custom, input: Option<String>) {
    let callback = format!("/interactions/{}/{}/callback", interaction.id, interaction.token);
    let flags = if custom.private { 64 } else { 0 };
    let text = fill(core, &custom.text, interaction, input.as_deref());
    match custom.mode {
        CommandMode::Reply => {
            let _ = core
                .http
                .raw(
                    "POST",
                    &callback,
                    Some(serde_json::json!({ "type": 4, "data": {
                        "content": crate::llm::truncate(&text, 1990),
                        "flags": flags,
                        "allowed_mentions": { "parse": ["users"] },
                    }})),
                    None,
                )
                .await;
        }
        CommandMode::Agent => {
            // "Thinking…" while the agent works; its answer replaces it.
            let _ = core
                .http
                .raw("POST", &callback, Some(serde_json::json!({ "type": 5, "data": { "flags": flags } })), None)
                .await;
            let Some(user) = interaction.member.as_ref().and_then(|m| m.user.clone()).or_else(|| interaction.user.clone()) else {
                return;
            };
            let guild_id = interaction.guild_id.map(|g| g.get());
            if let (Some(guild_id), Some(member)) = (guild_id, &interaction.member) {
                let mut member = member.clone();
                member.user = Some(user.clone());
                core.world.upsert_member(guild_id, &member);
            }
            let mut question = format!(
                "{} used your /{} command, which means: {text}\nDo that now (with your tools when needed) and reply with the result.",
                user.global_name.clone().unwrap_or_else(|| user.username.clone()),
                custom.name
            );
            if let Some(input) = input.filter(|i| !i.trim().is_empty()) {
                question.push_str(&format!("\nWhat they typed with it: {input}"));
            }
            let job = Job {
                guild_id,
                channel_id: interaction.channel_id.map(|c| c.get()).unwrap_or(0),
                triggers: Vec::new(),
                reason: Reason::Ask,
                requester: core.principal_for(guild_id, &user),
                task: None,
                outlet: Outlet::Interaction {
                    application_id: interaction.application_id.get(),
                    token: interaction.token.clone(),
                },
                question: Some(question),
            };
            let core = Arc::clone(core);
            tokio::spawn(async move { brain::run(&core, job).await });
        }
    }
}

pub async fn handle(core: &Arc<AgentCore>, interaction: Interaction) {
    // Page buttons on long answers.
    if super::pages::on_component(core, &interaction).await {
        return;
    }
    // /setup menus and forms.
    if super::setup::on_component(core, &interaction).await {
        return;
    }
    // Ticket panel and ticket buttons.
    if super::tickets::on_component(core, &interaction).await {
        return;
    }
    // Role menus and giveaways.
    if super::community::on_component(core, &interaction).await {
        return;
    }
    if let Some(InteractionData::ApplicationCommand(data)) = &interaction.data {
        let guild = interaction.guild_id.map(|g| g.get());
        // A built-in turned off in this server.
        if let Some(g) = guild {
            let off = BUILT_IN.iter().any(|(n, _)| *n == data.name)
                && core.hive.knowledge.guild(g, |k| k.disabled_commands.iter().any(|d| *d == data.name));
            if off {
                let _ = core
                    .http
                    .raw(
                        "POST",
                        &format!("/interactions/{}/{}/callback", interaction.id, interaction.token),
                        Some(serde_json::json!({ "type": 4, "data": { "content": format!("/{} is turned off in this server.", data.name), "flags": 64 } })),
                        None,
                    )
                    .await;
                return;
            }
        }
        let custom = guild
            .and_then(|g| customs(core, Some(g)).into_iter().find(|c| c.name == data.name))
            .or_else(|| customs(core, None).into_iter().find(|c| c.name == data.name));
        if let Some(custom) = custom {
            let input = data
                .options
                .iter()
                .find(|o| o.name == "input")
                .and_then(|o| o.value.as_ref())
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            run_custom(core, &interaction, &custom, input).await;
            return;
        }
    }
    let registry = core.commands.lock().expect("commands poisoned").clone();
    let Some(registry) = registry else {
        return;
    };
    if let Err(error) = registry.handle(Arc::clone(&core.http), interaction).await {
        tracing::warn!(%error, "slash command failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unchanged_commands_are_recognized() {
        let wanted = vec![
            json!({ "name": "ping", "description": "Check the latency" }),
            json!({ "name": "rank", "description": "Your level", "options": [{ "type": 6, "name": "user", "description": "Whose" }] }),
        ];
        // What Discord returns: ids, versions, defaults, another order.
        let registered = vec![
            json!({ "id": "2", "application_id": "9", "version": "5", "type": 1, "name": "rank", "description": "Your level", "default_member_permissions": null, "dm_permission": true, "nsfw": false, "options": [{ "type": 6, "name": "user", "description": "Whose", "required": false }] }),
            json!({ "id": "1", "application_id": "9", "version": "4", "type": 1, "name": "ping", "description": "Check the latency", "default_member_permissions": null }),
        ];
        assert!(same_commands(&wanted, &registered));
        let changed = vec![wanted[0].clone(), json!({ "name": "rank", "description": "Your level and card" })];
        assert!(!same_commands(&changed, &registered));
        assert!(!same_commands(&wanted[..1], &registered));
        let a = vec![json!({ "name": "quiet", "description": "q", "options": [{ "type": 4, "name": "m", "description": "d", "min_value": 1.0, "max_value": 10080.0 }] })];
        let b = vec![json!({ "name": "quiet", "type": 1, "description": "q", "options": [{ "type": 4, "name": "m", "description": "d", "min_value": 1, "max_value": 10080 }] })];
        assert!(same_commands(&a, &b));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Compare what the app would register with what Discord has now
    /// (read-only; needs the agent's token in the app's config).
    #[tokio::test]
    #[ignore]
    async fn lab_registered_commands_match() {
        let config = crate::config::load(&crate::paths::config_path()).expect("config");
        let profile = config.agents[0].clone();
        let http = Arc::new(axis::rest::Http::builder(Token::new(profile.token.clone())).build().expect("http"));
        let me = http.raw("GET", "/applications/@me", None, None).await.expect("application");
        let app_id = me["id"].as_str().expect("id").to_owned();
        let hive = super::super::hive::Hive::new(Arc::new(super::super::live::Live::default()), config.settings.clone());
        let memory = Arc::new(crate::memory::MemoryStore::load(std::env::temp_dir().join("axis-lab-commands.json")));
        let core = AgentCore::new(profile, hive, memory, http, true);
        let wanted: Vec<serde_json::Value> = build(&core).to_fields().iter().map(|f| serde_json::to_value(f).unwrap()).collect();
        let registered = core.http.raw("GET", &format!("/applications/{app_id}/commands"), None, None).await.expect("fetch");
        let registered = registered.as_array().cloned().unwrap_or_default();
        let names = |l: &[serde_json::Value]| l.iter().filter_map(|c| c["name"].as_str().map(str::to_owned)).collect::<Vec<_>>();
        println!("wanted: {:?}\nregistered: {:?}\nsame: {}", names(&wanted), names(&registered), same_commands(&wanted, &registered));
        let without_ping: Vec<_> = wanted.iter().filter(|c| c["name"] != "ping").cloned().collect();
        println!("same apart from the new /ping: {}", same_commands(&without_ping, &registered));
        for w in &without_ping {
            if let Some(r) = registered.iter().find(|r| r["name"] == w["name"]) {
                let (a, b) = (normalize(w), normalize(r));
                if a != b {
                    println!("DIFF {}
  ours:    {a}
  discord: {b}", w["name"]);
                }
            }
        }
    }
}
