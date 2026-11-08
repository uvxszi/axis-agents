//! `/setup`: everything the agents run in this server, shown as sections
//! with buttons and forms — leveling, rank cards, statistics, tickets,
//! who may change settings, and how the agent itself behaves. People who
//! may not change settings see it read-only.

use std::sync::Arc;

use axis::model::{Interaction, InteractionData, User};
use serde_json::{Value, json};

use super::agent::AgentCore;
use super::authority::Principal;
use super::levels;
use crate::config::AccessLevel;
use crate::memory::{GuildKnowledge, LevelReward};

const PREFIX: &str = "axis_setup:";
pub const SECTIONS: [(&str, &str, &str); 8] = [
    ("overview", "Overview", "📋"),
    ("leveling", "Leveling", "⭐"),
    ("card", "Rank cards", "🎴"),
    ("stats", "Statistics", "📈"),
    ("tickets", "Tickets", "🎫"),
    ("access", "Who can change settings", "🔐"),
    ("privacy", "Privacy across servers", "🛡️"),
    ("agent", "This agent", "🤖"),
];

fn on_off(value: bool) -> &'static str {
    if value { "✅ on" } else { "⛔ off" }
}

fn button(action: &str, label: &str, style: u8) -> Value {
    json!({ "type": 2, "style": style, "label": label, "custom_id": format!("{PREFIX}{action}") })
}

fn toggle(action: &str, label: &str, on: bool) -> Value {
    button(
        action,
        &format!("{label}: {}", if on { "on" } else { "off" }),
        if on { 3 } else { 2 },
    )
}

fn roles_text(roles: &[u64]) -> String {
    if roles.is_empty() {
        "none".to_owned()
    } else {
        roles
            .iter()
            .map(|r| format!("<@&{r}>"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The panel for a section: an embed, a section picker, and (for people
/// allowed to change things) buttons.
pub fn render(
    core: &AgentCore,
    guild_id: u64,
    section: &str,
    can_edit: bool,
    is_owner: bool,
) -> Value {
    let k: GuildKnowledge = core.hive.knowledge.guild(guild_id, |k| k.clone());
    let profile = core.profile();
    let l = &k.leveling;
    let s = &k.stats;
    let t = &k.tickets;
    let access = if k.settings_access.is_restricted() {
        format!(
            "Only the owner, {}{}",
            roles_text(&k.settings_access.roles),
            if k.settings_access.users.is_empty() {
                String::new()
            } else {
                format!(
                    ", {}",
                    k.settings_access
                        .users
                        .iter()
                        .map(|u| format!("<@{u}>"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        )
    } else {
        "Admins and the owner".to_owned()
    };
    let (title, fields, mut rows): (&str, Vec<(String, String)>, Vec<Value>) = match section {
        "leveling" => (
            "⭐ Leveling",
            vec![
                ("Status".into(), format!("{}{}", on_off(l.enabled), if l.paused { " (paused)" } else { "" })),
                ("XP per message".into(), format!("{}–{} every {}s", l.xp_min, l.xp_max, l.cooldown_secs)),
                ("Voice XP".into(), format!("{} per minute with others", l.voice_xp_per_minute)),
                ("Level-up messages".into(), match l.announce.as_str() {
                    "channel" if l.announce_channel != 0 => format!("in <#{}>", l.announce_channel),
                    "dm" => "by DM".into(),
                    "off" => "off".into(),
                    _ => "in the same channel".into(),
                }),
                ("Message".into(), l.message.clone()),
                ("Role rewards".into(), if l.rewards.is_empty() {
                    "none".into()
                } else {
                    l.rewards.iter().map(|r| format!("lvl {} → <@&{}>", r.level, r.role_id)).collect::<Vec<_>>().join("\n")
                }),
                ("Members ranked".into(), l.members.len().to_string()),
            ],
            vec![
                json!({ "type": 1, "components": [
                    toggle("lvl_enable", "Leveling", l.enabled),
                    button("lvl_pause", if l.paused { "Resume" } else { "Pause" }, 2),
                    button("lvl_announce", "Level-up messages: next", 2),
                    button("lvl_here", "Announce in this channel", 2),
                ]}),
                json!({ "type": 1, "components": [
                    button("lvl_edit", "Edit XP & message…", 1),
                    button("lvl_rewards", "Edit role rewards…", 1),
                    toggle("lvl_stack", "Keep lower rewards", l.stack_rewards),
                ]}),
            ],
        ),
        "card" => (
            "🎴 Rank cards",
            vec![
                ("Layout & style".into(), format!("{} · {} · font {} ({})", l.card.layout, l.card.style, l.card.font, l.card.font_weight)),
                ("Colors".into(), format!("background {} · accent {} · text {} · muted {}", l.card.background, l.card.accent, l.card.text, l.card.muted)),
                ("Background image".into(), if l.card.background_url.is_empty() { "none".into() } else { format!("{} (darkened {}%)", l.card.background_url, l.card.overlay) }),
            ],
            vec![json!({ "type": 1, "components": [
                button("card_edit", "Edit design…", 1),
                button("card_style", "Style: next", 2),
                button("card_preview", "Preview my card", 3),
                button("card_reset", "Reset", 4),
            ]})],
        ),
        "stats" => {
            let days = s.days.len();
            (
                "📈 Statistics",
                vec![
                    ("Collecting".into(), format!("{}{}", on_off(s.enabled), if s.paused { " (paused)" } else { "" })),
                    ("Tracking".into(), format!(
                        "members {} · messages {} · voice {} · joins/leaves {}",
                        on_off(s.track_members), on_off(s.track_messages), on_off(s.track_voice), on_off(s.track_joins)
                    )),
                    ("Last 7 days".into(), crate::llm::truncate(levels::stats_report(core, guild_id, 7).lines().next().unwrap_or(""), 900)),
                    ("History".into(), format!("{days} day(s) kept (up to 120)")),
                ],
                vec![
                    json!({ "type": 1, "components": [
                        toggle("st_enable", "Collecting", s.enabled),
                        button("st_pause", if s.paused { "Resume" } else { "Pause" }, 2),
                        button("st_clear", "Delete collected data", 4),
                    ]}),
                    json!({ "type": 1, "components": [
                        toggle("st_members", "Members", s.track_members),
                        toggle("st_messages", "Messages", s.track_messages),
                        toggle("st_voice", "Voice", s.track_voice),
                        toggle("st_joins", "Joins", s.track_joins),
                    ]}),
                ],
            )
        }
        "tickets" => (
            "🎫 Tickets",
            if t.is_set_up() {
                vec![
                    ("Panel".into(), format!("<#{}> — \"{}\"", t.panel_channel_id, t.panel.title)),
                    ("Ticket types".into(), if t.panel.options.is_empty() { "one button".into() } else { t.panel.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>().join(", ") }),
                    ("Staff roles".into(), roles_text(&t.staff_roles)),
                    ("Transcripts".into(), format!(
                        "{} · DM to opener {}",
                        if t.log_channel_id == 0 { "not posted".to_owned() } else { format!("posted in <#{}>", t.log_channel_id) },
                        on_off(t.dm_transcripts)
                    )),
                    ("Tickets".into(), format!("{} open, {} total", t.tickets.iter().filter(|x| x.closed.is_none()).count(), t.tickets.len())),
                    ("Agent answers in tickets".into(), on_off(t.auto_reply).to_owned()),
                ]
            } else {
                vec![("Not set up".into(), "Ask the agent: \"set up a ticket system in #support\".".into())]
            },
            if t.is_set_up() {
                vec![json!({ "type": 1, "components": [
                    toggle("tk_reply", "Agent answers", t.auto_reply),
                    toggle("tk_dm", "DM transcripts", t.dm_transcripts),
                    button("tk_log", "Post transcripts in this channel", 2),
                ]})]
            } else {
                Vec::new()
            },
        ),
        "access" => (
            "🔐 Who can change settings",
            vec![
                ("Allowed".into(), access.clone()),
                ("How it works".into(), "By default admins can change these settings. The server owner can limit it to chosen roles and people — then other admins can't.".into()),
            ],
            if is_owner {
                vec![json!({ "type": 1, "components": [
                    button("ac_edit", "Choose roles & people…", 1),
                    button("ac_reset", "Back to admins", 2),
                ]})]
            } else {
                Vec::new()
            },
        ),
        "privacy" => {
            let p = &k.privacy;
            (
                "🛡️ Privacy across servers",
                vec![
                    ("From other servers and DMs".into(), format!(
                        "member search {} · DMs to members {} · posting in channels {} · audit log search {}",
                        on_off(p.member_search), on_off(p.dms), on_off(p.outside_messages), on_off(p.audit_search)
                    )),
                    ("What this covers".into(), "Requests that come from another server or a DM. Everything asked inside this server works as usual, and outside actions here are logged in Agent HQ.".into()),
                ],
                vec![json!({ "type": 1, "components": [
                    toggle("pv_search", "Member search", p.member_search),
                    toggle("pv_dms", "DMs", p.dms),
                    toggle("pv_messages", "Posting", p.outside_messages),
                    toggle("pv_audit", "Audit search", p.audit_search),
                ]})],
            )
        }
        "agent" => (
            "🤖 This agent",
            vec![
                ("Name".into(), profile.name.clone()),
                ("Model".into(), profile.model.model.clone()),
                ("Thinking message".into(), format!("{} · {}", profile.behavior.thinking_style.label(), if profile.behavior.small_thinking { "small text" } else { "normal text" })),
                ("\"thought for\" footer".into(), on_off(profile.behavior.show_footer).to_owned()),
                ("Voice".into(), format!("answers out loud: {} · voice: {}", on_off(profile.voice.listen), crate::runtime::speech::current_voice(&profile.voice))),
                ("More".into(), "Everything else is in the Axis Agents app.".into()),
            ],
            vec![json!({ "type": 1, "components": [
                toggle("ag_small", "Small thinking text", profile.behavior.small_thinking),
                toggle("ag_footer", "Footer", profile.behavior.show_footer),
                toggle("ag_listen", "Answer in voice", profile.voice.listen),
            ]})],
        ),
        _ => (
            "📋 Setup",
            vec![
                ("⭐ Leveling".into(), format!("{}{} · {} ranked · {} reward(s)", on_off(l.enabled), if l.paused { " (paused)" } else { "" }, l.members.len(), l.rewards.len())),
                ("📈 Statistics".into(), format!("{}{} · {} day(s) of history", on_off(s.enabled), if s.paused { " (paused)" } else { "" }, s.days.len())),
                ("🎫 Tickets".into(), if t.is_set_up() { format!("panel in <#{}>, {} open", t.panel_channel_id, t.tickets.iter().filter(|x| x.closed.is_none()).count()) } else { "not set up".into() }),
                ("🎉 Giveaways".into(), format!("{} running", k.giveaways.iter().filter(|g| !g.ended).count())),
                ("📌 Standing tasks".into(), format!("{} active", k.tasks.iter().filter(|t| t.active).count())),
                ("🔐 Settings access".into(), access.clone()),
                ("🛡️ Privacy".into(), format!(
                    "from outside: search {} · DMs {} · posting {} · audit {}",
                    on_off(k.privacy.member_search), on_off(k.privacy.dms), on_off(k.privacy.outside_messages), on_off(k.privacy.audit_search)
                )),
            ],
            Vec::new(),
        ),
    };
    if !can_edit {
        rows.clear();
    }
    let options: Vec<Value> = SECTIONS
        .iter()
        .map(|(id, label, emoji)| json!({ "label": label, "value": id, "emoji": { "name": emoji }, "default": *id == section }))
        .collect();
    rows.insert(0, json!({ "type": 1, "components": [{ "type": 3, "custom_id": format!("{PREFIX}section"), "options": options }] }));
    let embed_fields: Vec<Value> = fields
        .into_iter()
        .map(|(name, value)| json!({ "name": name, "value": crate::llm::truncate(if value.is_empty() { "—" } else { &value }, 1000), "inline": false }))
        .collect();
    json!({
        "embeds": [{
            "title": title,
            "color": 0x5865F2,
            "fields": embed_fields,
            "footer": { "text": if can_edit { format!("{} · changes save right away", profile.name) } else { "View only — you can't change these settings".to_owned() } },
        }],
        "components": rows,
        "allowed_mentions": { "parse": [] },
    })
}

async fn callback(core: &AgentCore, interaction: &Interaction, kind: u8, data: Value) {
    let _ = core
        .http
        .raw(
            "POST",
            &format!(
                "/interactions/{}/{}/callback",
                interaction.id, interaction.token
            ),
            Some(json!({ "type": kind, "data": data })),
            None,
        )
        .await;
}

fn modal(id: &str, title: &str, inputs: &[(&str, &str, String, bool)]) -> Value {
    let rows: Vec<Value> = inputs
        .iter()
        .map(|(field, label, value, long)| {
            json!({ "type": 1, "components": [{
                "type": 4, "custom_id": field, "label": label, "style": if *long { 2 } else { 1 },
                "value": value, "required": false, "max_length": if *long { 1000 } else { 200 }
            }]})
        })
        .collect();
    json!({ "custom_id": format!("{PREFIX}m:{id}"), "title": title, "components": rows })
}

fn modal_values(components: &Value) -> Vec<(String, String)> {
    components
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| {
            row.get("components")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|c| {
            Some((
                c.get("custom_id")?.as_str()?.to_owned(),
                c.get("value")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn user_of(interaction: &Interaction) -> Option<User> {
    interaction
        .member
        .as_ref()
        .and_then(|m| m.user.clone())
        .or_else(|| interaction.user.clone())
}

/// The section a panel message shows, from its select menu.
fn current_section(interaction: &Interaction) -> String {
    interaction
        .message
        .as_ref()
        .and_then(|m| m.components.first().cloned())
        .and_then(|row| {
            row.pointer("/components/0/options")
                .and_then(Value::as_array)
                .cloned()
        })
        .and_then(|options| {
            options
                .iter()
                .find(|o| o.get("default").and_then(Value::as_bool) == Some(true))
                .and_then(|o| o.get("value").and_then(Value::as_str).map(str::to_owned))
        })
        .unwrap_or_else(|| "overview".to_owned())
}

fn roles_from(core: &AgentCore, guild_id: u64, text: &str) -> (Vec<u64>, Vec<String>) {
    let mut ids = Vec::new();
    let mut unknown = Vec::new();
    for item in text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match core
            .world
            .with_guild(guild_id, |g| g.find_role(item).map(|r| r.id.get()))
            .flatten()
        {
            Some(id) => ids.push(id),
            None => unknown.push(item.to_owned()),
        }
    }
    (ids, unknown)
}

/// A `/setup` component or form. Returns false when it isn't ours.
pub async fn on_component(core: &Arc<AgentCore>, interaction: &Interaction) -> bool {
    let (custom_id, values, modal_components) = match &interaction.data {
        Some(InteractionData::MessageComponent(d)) => (
            d.custom_id.clone(),
            d.values.clone().unwrap_or_default(),
            None,
        ),
        Some(InteractionData::ModalSubmit(d)) => {
            (d.custom_id.clone(), Vec::new(), Some(d.components.clone()))
        }
        _ => return false,
    };
    let Some(action) = custom_id.strip_prefix(PREFIX) else {
        return false;
    };
    let (Some(guild_id), Some(user)) =
        (interaction.guild_id.map(|g| g.get()), user_of(interaction))
    else {
        return true;
    };
    if let Some(member) = &interaction.member {
        let mut member = member.clone();
        member.user = Some(user.clone());
        core.world.upsert_member(guild_id, &member);
    }
    let who: Principal = core.principal_for(Some(guild_id), &user);
    let can_edit = levels::can_manage(core, guild_id, &who);
    let is_owner =
        who.level >= AccessLevel::Owner || who.kind == super::authority::PrincipalKind::Operator;
    let mut section = current_section(interaction);
    if action == "section" {
        section = values
            .first()
            .cloned()
            .unwrap_or_else(|| "overview".to_owned());
        callback(
            core,
            interaction,
            7,
            render(core, guild_id, &section, can_edit, is_owner),
        )
        .await;
        return true;
    }
    if !can_edit || (action.starts_with("ac_") || action == "m:access") && !is_owner {
        callback(
            core,
            interaction,
            4,
            json!({ "content": "You can't change these settings — ask the server owner.", "flags": 64 }),
        )
        .await;
        return true;
    }
    let channel_id = interaction.channel_id.map(|c| c.get()).unwrap_or(0);
    let mut note: Option<String> = None;
    let edit = |f: &dyn Fn(&mut GuildKnowledge)| core.hive.knowledge.guild_mut(guild_id, |k| f(k));
    match action {
        // Forms open in a pop-up.
        "lvl_edit" => {
            let l = core.hive.knowledge.guild(guild_id, |k| k.leveling.clone());
            callback(
                core,
                interaction,
                9,
                modal(
                    "lvl",
                    "Leveling",
                    &[
                        (
                            "xp_min",
                            "Least XP per message",
                            l.xp_min.to_string(),
                            false,
                        ),
                        ("xp_max", "Most XP per message", l.xp_max.to_string(), false),
                        (
                            "cooldown",
                            "Seconds between XP messages",
                            l.cooldown_secs.to_string(),
                            false,
                        ),
                        (
                            "voice",
                            "Voice XP per minute (0 = none)",
                            l.voice_xp_per_minute.to_string(),
                            false,
                        ),
                        (
                            "message",
                            "Level-up message ({user} {level} {name})",
                            l.message.clone(),
                            true,
                        ),
                    ],
                ),
            )
            .await;
            return true;
        }
        "lvl_rewards" => {
            let rewards = core
                .hive
                .knowledge
                .guild(guild_id, |k| k.leveling.rewards.clone());
            let text = rewards
                .iter()
                .map(|r| {
                    let name = core
                        .world
                        .with_guild(guild_id, |g| {
                            g.roles.get(&r.role_id).map(|x| x.name.clone())
                        })
                        .flatten()
                        .unwrap_or_else(|| r.role_id.to_string());
                    format!("{}: {name}", r.level)
                })
                .collect::<Vec<_>>()
                .join("\n");
            callback(
                core,
                interaction,
                9,
                modal(
                    "rewards",
                    "Role rewards",
                    &[("rewards", "One per line: level: role name", text, true)],
                ),
            )
            .await;
            return true;
        }
        "card_edit" => {
            let c = core
                .hive
                .knowledge
                .guild(guild_id, |k| k.leveling.card.clone());
            callback(
                core,
                interaction,
                9,
                modal(
                    "card",
                    "Rank card design",
                    &[
                        ("background", "Background color (#hex)", c.background, false),
                        (
                            "background_url",
                            "Background image URL (optional)",
                            c.background_url,
                            false,
                        ),
                        ("accent", "Accent color (#hex)", c.accent, false),
                        ("text", "Text color (#hex)", c.text, false),
                        ("muted", "Secondary text color (#hex)", c.muted, false),
                    ],
                ),
            )
            .await;
            return true;
        }
        "ac_edit" => {
            let a = core
                .hive
                .knowledge
                .guild(guild_id, |k| k.settings_access.clone());
            let roles = a
                .roles
                .iter()
                .filter_map(|r| {
                    core.world
                        .with_guild(guild_id, |g| g.roles.get(r).map(|x| x.name.clone()))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join(", ");
            let users = a
                .users
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            callback(
                core,
                interaction,
                9,
                modal(
                    "access",
                    "Who can change settings",
                    &[
                        ("roles", "Roles (names, comma-separated)", roles, false),
                        ("users", "People (user IDs, comma-separated)", users, false),
                    ],
                ),
            )
            .await;
            return true;
        }
        "card_preview" => {
            let _ = core
                .http
                .raw(
                    "POST",
                    &format!(
                        "/interactions/{}/{}/callback",
                        interaction.id, interaction.token
                    ),
                    Some(json!({ "type": 5, "data": { "flags": 64 } })),
                    None,
                )
                .await;
            let data = axis::model::InteractionMessageCallbackData {
                flags: Some(axis::model::MessageFlags::EPHEMERAL),
                ..Default::default()
            };
            match levels::rank_card(core, guild_id, user.id.get()).await {
                Ok((png, _)) => {
                    let _ = core
                        .http
                        .create_followup(
                            interaction.application_id,
                            interaction.token.clone(),
                            data,
                        )
                        .file("rank.png", png)
                        .await;
                }
                Err(e) => {
                    let mut data = data;
                    data.content = Some(format!("Couldn't draw the card: {e}"));
                    let _ = core
                        .http
                        .create_followup(
                            interaction.application_id,
                            interaction.token.clone(),
                            data,
                        )
                        .await;
                }
            }
            return true;
        }
        // Form results.
        "m:lvl" => {
            let values = modal_values(modal_components.as_ref().unwrap_or(&Value::Null));
            let get = |k: &str| {
                values
                    .iter()
                    .find(|(f, _)| f == k)
                    .map(|(_, v)| v.trim().to_owned())
            };
            edit(&|k| {
                let l = &mut k.leveling;
                if let Some(v) = get("xp_min").and_then(|v| v.parse().ok()) {
                    l.xp_min = v;
                }
                if let Some(v) = get("xp_max").and_then(|v| v.parse().ok()) {
                    l.xp_max = v;
                }
                if let Some(v) = get("cooldown").and_then(|v| v.parse().ok()) {
                    l.cooldown_secs = v;
                }
                if let Some(v) = get("voice").and_then(|v| v.parse().ok()) {
                    l.voice_xp_per_minute = v;
                }
                if let Some(v) = get("message").filter(|v| !v.is_empty()) {
                    l.message = v;
                }
            });
            section = "leveling".into();
        }
        "m:rewards" => {
            let values = modal_values(modal_components.as_ref().unwrap_or(&Value::Null));
            let text = values.first().map(|(_, v)| v.clone()).unwrap_or_default();
            let mut rewards = Vec::new();
            let mut unknown = Vec::new();
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let (level, role) = line.split_once([':', '=', '-']).unwrap_or((line, ""));
                let level = level
                    .trim()
                    .trim_start_matches(|c: char| !c.is_ascii_digit())
                    .parse::<u32>();
                let (ids, missing) = roles_from(core, guild_id, role);
                match (level, ids.first()) {
                    (Ok(level), Some(role_id)) => rewards.push(LevelReward {
                        level,
                        role_id: *role_id,
                    }),
                    _ => unknown.push(line.to_owned()),
                }
                unknown.extend(missing);
            }
            rewards.sort_by_key(|r| r.level);
            edit(&|k| k.leveling.rewards = rewards.clone());
            if !unknown.is_empty() {
                note = Some(format!("Skipped: {}", unknown.join(", ")));
            }
            section = "leveling".into();
        }
        "m:card" => {
            let values = modal_values(modal_components.as_ref().unwrap_or(&Value::Null));
            let get = |k: &str| {
                values
                    .iter()
                    .find(|(f, _)| f == k)
                    .map(|(_, v)| v.trim().to_owned())
            };
            edit(&|k| {
                let c = &mut k.leveling.card;
                if let Some(v) = get("background").filter(|v| !v.is_empty()) {
                    c.background = v;
                }
                if let Some(v) = get("background_url") {
                    c.background_url = v;
                }
                if let Some(v) = get("accent").filter(|v| !v.is_empty()) {
                    c.accent = v;
                }
                if let Some(v) = get("text").filter(|v| !v.is_empty()) {
                    c.text = v;
                }
                if let Some(v) = get("muted").filter(|v| !v.is_empty()) {
                    c.muted = v;
                }
            });
            section = "card".into();
        }
        "m:access" => {
            let values = modal_values(modal_components.as_ref().unwrap_or(&Value::Null));
            let get = |k: &str| {
                values
                    .iter()
                    .find(|(f, _)| f == k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            let (roles, unknown) = roles_from(core, guild_id, &get("roles"));
            let users: Vec<u64> = get("users")
                .split([',', ' ', '\n'])
                .filter_map(|u| crate::runtime::world::parse_snowflake(u.trim()))
                .collect();
            edit(&|k| {
                k.settings_access.roles = roles.clone();
                k.settings_access.users = users.clone();
            });
            if !unknown.is_empty() {
                note = Some(format!("Unknown roles skipped: {}", unknown.join(", ")));
            }
            section = "access".into();
        }
        // Buttons.
        "lvl_enable" => edit(&|k| k.leveling.enabled = !k.leveling.enabled),
        "lvl_pause" => edit(&|k| k.leveling.paused = !k.leveling.paused),
        "lvl_stack" => edit(&|k| k.leveling.stack_rewards = !k.leveling.stack_rewards),
        "lvl_announce" => edit(&|k| {
            k.leveling.announce = match k.leveling.announce.as_str() {
                "same" => "channel",
                "channel" => "dm",
                "dm" => "off",
                _ => "same",
            }
            .to_owned();
            if k.leveling.announce == "channel" && k.leveling.announce_channel == 0 {
                k.leveling.announce_channel = channel_id;
            }
        }),
        "lvl_here" => edit(&|k| {
            k.leveling.announce = "channel".to_owned();
            k.leveling.announce_channel = channel_id;
        }),
        "card_style" => edit(&|k| {
            k.leveling.card.style = match k.leveling.card.style.as_str() {
                "classic" => "minimal",
                "minimal" => "bold",
                _ => "classic",
            }
            .to_owned();
        }),
        "card_reset" => edit(&|k| k.leveling.card = Default::default()),
        "st_enable" => edit(&|k| k.stats.enabled = !k.stats.enabled),
        "st_pause" => edit(&|k| k.stats.paused = !k.stats.paused),
        "st_members" => edit(&|k| k.stats.track_members = !k.stats.track_members),
        "st_messages" => edit(&|k| k.stats.track_messages = !k.stats.track_messages),
        "st_voice" => edit(&|k| k.stats.track_voice = !k.stats.track_voice),
        "st_joins" => edit(&|k| k.stats.track_joins = !k.stats.track_joins),
        "st_clear" => edit(&|k| k.stats.days.clear()),
        "tk_reply" => edit(&|k| k.tickets.auto_reply = !k.tickets.auto_reply),
        "tk_dm" => edit(&|k| k.tickets.dm_transcripts = !k.tickets.dm_transcripts),
        "tk_log" => edit(&|k| k.tickets.log_channel_id = channel_id),
        "ac_reset" => edit(&|k| k.settings_access = Default::default()),
        "pv_search" => edit(&|k| k.privacy.member_search = !k.privacy.member_search),
        "pv_dms" => edit(&|k| k.privacy.dms = !k.privacy.dms),
        "pv_messages" => edit(&|k| k.privacy.outside_messages = !k.privacy.outside_messages),
        "pv_audit" => edit(&|k| k.privacy.audit_search = !k.privacy.audit_search),
        "ag_small" | "ag_footer" | "ag_listen" => {
            let action = action.to_owned();
            let apply = move |p: &mut crate::config::AgentProfile| match action.as_str() {
                "ag_small" => p.behavior.small_thinking = !p.behavior.small_thinking,
                "ag_footer" => p.behavior.show_footer = !p.behavior.show_footer,
                _ => p.voice.listen = !p.voice.listen,
            };
            let mut profile = core.profile();
            apply(&mut profile);
            core.set_profile(profile);
            core.live()
                .profile_patches
                .lock()
                .expect("live poisoned")
                .push((core.id.clone(), Box::new(apply)));
            core.live().repaint();
        }
        _ => {}
    }
    let mut body = render(core, guild_id, &section, can_edit, is_owner);
    if let Some(note) = note {
        body["content"] = json!(note);
    }
    callback(core, interaction, 7, body).await;
    true
}
