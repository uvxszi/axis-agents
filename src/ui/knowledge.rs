//! What the agents know about each server, shared by all of them.

use egui::{RichText, TextEdit, Ui};

use super::theme::{self, ACCENT, FAINT, TEXT, WEAK};
use super::widgets::{self, card, pill, section};
use super::{App, KnowledgeTab};
use crate::memory::{DirectiveScope, now_ms};

pub fn show(app: &mut App, ui: &mut Ui) {
    widgets::page_header(
        ui,
        "Knowledge",
        "What your agents have learned and been told, per server. Everything here is shared by all agents.",
    );
    let guilds = app.sup.hive.knowledge.known_guilds();
    if guilds.is_empty() {
        card(ui, |ui| {
            ui.label(
                RichText::new("No servers yet — start an agent and invite it somewhere.")
                    .color(FAINT),
            );
        });
        return;
    }
    if app
        .knowledge_guild
        .is_none_or(|g| !guilds.iter().any(|(id, _)| *id == g))
    {
        app.knowledge_guild = guilds.first().map(|(id, _)| *id);
    }
    let guild_id = app.knowledge_guild.expect("set above");
    ui.horizontal(|ui| {
        ui.label("Server");
        egui::ComboBox::from_id_salt("knowledge_guild")
            .selected_text(
                guilds
                    .iter()
                    .find(|(id, _)| *id == guild_id)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default(),
            )
            .width(300.0)
            .show_ui(ui, |ui| {
                for (id, name) in &guilds {
                    ui.selectable_value(
                        &mut app.knowledge_guild,
                        Some(*id),
                        if name.is_empty() {
                            id.to_string()
                        } else {
                            name.clone()
                        },
                    );
                }
            });
        let hq = app.sup.hive.knowledge.guild(guild_id, |k| k.hq.clone());
        match hq.channel_id {
            Some(channel) => pill(
                ui,
                &format!(
                    "HQ: #{}",
                    app.sup
                        .hive
                        .knowledge
                        .channel(channel, |c| c.name.trim_start_matches('#').to_owned())
                ),
                theme::GREEN,
            ),
            None if hq.request.is_some() => {
                pill(ui, "HQ: waiting for an admin's answer", theme::YELLOW)
            }
            None if hq.declined_until > now_ms() => pill(ui, "HQ: declined by admins", FAINT),
            None => pill(ui, "HQ: not set up", FAINT),
        };
    });
    ui.add_space(6.0);
    widgets::tab_bar(
        ui,
        &mut app.knowledge_tab,
        &[
            (KnowledgeTab::Culture, "Culture & rules"),
            (KnowledgeTab::People, "People"),
            (KnowledgeTab::Directives, "Directives"),
            (KnowledgeTab::Tasks, "Tasks"),
            (KnowledgeTab::Moderation, "Moderation log"),
            (KnowledgeTab::Channels, "Channel memory"),
        ],
    );
    egui::ScrollArea::vertical()
        .id_salt(("knowledge", app.knowledge_tab as u8))
        .auto_shrink([false, false])
        .show(ui, |ui| match app.knowledge_tab {
            KnowledgeTab::Culture => culture(app, ui, guild_id),
            KnowledgeTab::People => people(app, ui, guild_id),
            KnowledgeTab::Directives => directives(app, ui, guild_id),
            KnowledgeTab::Tasks => super::agent_page::task_list(app, ui, None),
            KnowledgeTab::Moderation => moderation(app, ui, guild_id),
            KnowledgeTab::Channels => channels(app, ui, guild_id),
        });
}

fn culture(app: &mut App, ui: &mut Ui, guild_id: u64) {
    let (mut culture, updated, rules, pending) = app.sup.hive.knowledge.guild(guild_id, |k| {
        (
            k.culture.clone(),
            k.culture_updated,
            k.rules.clone(),
            k.messages_since_reflect,
        )
    });
    card(ui, |ui| {
        section(
            ui,
            "Server culture",
            &format!(
                "Learned by watching the server{}. {} new messages since the last reflection. You can edit it.",
                if updated > 0 {
                    format!(", last updated {}", widgets::ago(updated))
                } else {
                    String::new()
                },
                pending
            ),
        );
        if ui
            .add(
                TextEdit::multiline(&mut culture)
                    .desired_rows(8)
                    .desired_width(f32::INFINITY)
                    .hint_text("Nothing learned yet — agents write this after watching a while."),
            )
            .changed()
        {
            app.sup
                .hive
                .knowledge
                .guild_mut(guild_id, |k| k.culture = culture.clone());
        }
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(
            ui,
            "Server rules",
            "Read from the rules channel (refreshed twice a day).",
        );
        ui.label(
            RichText::new(if rules.is_empty() {
                "No rules channel found."
            } else {
                &rules
            })
            .color(if rules.is_empty() { FAINT } else { TEXT }),
        );
    });
}

fn people(app: &mut App, ui: &mut Ui, guild_id: u64) {
    let people = app.sup.hive.knowledge.guild(guild_id, |k| k.people.clone());
    if people.is_empty() {
        card(ui, |ui| {
            ui.label(RichText::new("No notes on people yet.").color(FAINT));
        });
    }
    let mut people: Vec<_> = people.into_iter().collect();
    people.sort_by(|a, b| b.1.updated.cmp(&a.1.updated));
    for (user_id, person) in people {
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(if person.name.is_empty() {
                        user_id.to_string()
                    } else {
                        person.name.clone()
                    })
                    .strong()
                    .color(TEXT),
                );
                ui.label(RichText::new(user_id.to_string()).size(11.0).color(FAINT));
                let strikes = person.recent_strikes(30 * 86_400_000);
                if strikes > 0 {
                    pill(ui, &format!("{strikes} strike(s) / 30d"), theme::YELLOW);
                }
                if person.opted_out {
                    pill(ui, "opted out of notes", FAINT);
                }
            });
            let mut notes = person.notes.clone();
            if ui
                .add(
                    TextEdit::multiline(&mut notes)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                )
                .changed()
            {
                app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                    if let Some(p) = k.people.get_mut(&user_id) {
                        p.notes = notes.clone();
                    }
                });
            }
            if !person.strikes.is_empty() {
                egui::CollapsingHeader::new(
                    RichText::new(format!("{} strike(s)", person.strikes.len())).size(12.0),
                )
                .id_salt(("strikes", user_id))
                .show(ui, |ui| {
                    for strike in person.strikes.iter().rev() {
                        ui.label(
                            RichText::new(format!(
                                "{} — {} (by {})",
                                widgets::ago(strike.at),
                                strike.reason,
                                strike.by
                            ))
                            .size(12.5)
                            .color(WEAK),
                        );
                    }
                    if ui.small_button("clear strikes").clicked() {
                        app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                            if let Some(p) = k.people.get_mut(&user_id) {
                                p.strikes.clear();
                            }
                        });
                    }
                });
            }
            if ui.small_button("forget this person").clicked() {
                app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                    k.people.remove(&user_id);
                });
            }
        });
        ui.add_space(4.0);
    }
}

fn directives(app: &mut App, ui: &mut Ui, guild_id: u64) {
    let (directives, quiet) = app
        .sup
        .hive
        .knowledge
        .guild(guild_id, |k| (k.directives.clone(), k.quiet.clone()));
    card(ui, |ui| {
        section(
            ui,
            "Standing directives",
            "Instructions people gave the agents, highest rank first. A lower-ranked person can't override or revoke a higher-ranked person's directive.",
        );
        if directives.is_empty() {
            ui.label(
                RichText::new("None yet. People set these in chat (\"Nova, from now on…\").")
                    .color(FAINT),
            );
        }
        let mut sorted = directives.clone();
        sorted.sort_by(|a, b| b.author_rank.cmp(&a.author_rank));
        for directive in sorted {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("#{}", directive.id))
                        .strong()
                        .color(ACCENT),
                );
                pill(ui, directive.author_level.label(), theme::BLUE);
                pill(
                    ui,
                    &match directive.scope {
                        DirectiveScope::Server => "server-wide".to_owned(),
                        DirectiveScope::Channel(c) => format!(
                            "in {}",
                            app.sup.hive.knowledge.channel(c, |a| a.name.clone())
                        ),
                        DirectiveScope::Personal(_) => "personal".to_owned(),
                    },
                    FAINT,
                );
                let target = directive
                    .target_agent
                    .as_ref()
                    .and_then(|id| app.config.agents.iter().find(|a| &a.id == id))
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "all agents".to_owned());
                ui.label(
                    RichText::new(format!(
                        "for {target} · from {} · {}",
                        directive.author_name,
                        widgets::ago(directive.created)
                    ))
                    .size(12.0)
                    .color(WEAK),
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(&directive.text).color(TEXT));
                if ui.small_button("revoke").clicked() {
                    app.sup
                        .hive
                        .knowledge
                        .guild_mut(guild_id, |k| k.directives.retain(|d| d.id != directive.id));
                }
            });
            ui.separator();
        }
    });
    ui.add_space(8.0);
    let active: Vec<_> = quiet.into_iter().filter(|q| q.until > now_ms()).collect();
    card(ui, |ui| {
        section(
            ui,
            "Quiet rules",
            "Agents told to stay quiet in a channel or towards a person.",
        );
        if active.is_empty() {
            ui.label(RichText::new("None active.").color(FAINT));
        }
        for rule in &active {
            ui.label(
                RichText::new(format!(
                    "{} · {} · ends in {} min",
                    rule.channel_id
                        .map(|c| app.sup.hive.knowledge.channel(c, |a| a.name.clone()))
                        .unwrap_or_else(|| "everywhere".to_owned()),
                    rule.user_id
                        .map(|u| format!("towards user {u}"))
                        .unwrap_or_else(|| "for everyone".to_owned()),
                    (rule.until - now_ms()) / 60_000
                ))
                .color(TEXT),
            );
        }
        if !active.is_empty() && ui.button("Lift all").clicked() {
            app.sup
                .hive
                .knowledge
                .guild_mut(guild_id, |k| k.quiet.clear());
        }
    });
    ui.add_space(8.0);
    // "Never chat in #x" — the harshest rule, and the one a confused
    // agent can set on itself. It must always be visible and liftable.
    let no_chat = app
        .sup
        .hive
        .knowledge
        .guild(guild_id, |k| k.no_chat.clone());
    card(ui, |ui| {
        section(
            ui,
            "No-chat rules",
            "Channels where an agent never speaks at all — set in chat, by a person or by the agent itself.",
        );
        if no_chat.is_empty() {
            ui.label(RichText::new("None.").color(FAINT));
        }
        for rule in &no_chat {
            let channel = app
                .sup
                .hive
                .knowledge
                .channel(rule.channel_id, |a| a.name.clone());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "never chats in #{channel} · set by {} · for {}",
                        rule.setter_name,
                        rule.agent_id.as_deref().unwrap_or("all agents")
                    ))
                    .color(TEXT),
                );
                if ui.small_button("lift").clicked() {
                    app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                        k.no_chat.retain(|r| {
                            !(r.channel_id == rule.channel_id && r.agent_id == rule.agent_id)
                        })
                    });
                }
            });
        }
    });
    ui.add_space(8.0);
    let chill = app.sup.hive.knowledge.guild(guild_id, |k| k.chill.clone());
    card(ui, |ui| {
        section(
            ui,
            "Chill channels",
            "Extra-brief mode with self-pruning — set with \"be chill in #x\".",
        );
        if chill.is_empty() {
            ui.label(RichText::new("None.").color(FAINT));
        }
        for rule in &chill {
            let channel = app
                .sup
                .hive
                .knowledge
                .channel(rule.channel_id, |a| a.name.clone());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "chill in #{channel} · set by {} · for {}",
                        rule.setter_name,
                        rule.agent_id.as_deref().unwrap_or("all agents")
                    ))
                    .color(TEXT),
                );
                if ui.small_button("lift").clicked() {
                    app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                        k.chill.retain(|r| {
                            !(r.channel_id == rule.channel_id && r.agent_id == rule.agent_id)
                        })
                    });
                }
            });
        }
    });
}

fn moderation(app: &mut App, ui: &mut Ui, guild_id: u64) {
    let log = app
        .sup
        .hive
        .knowledge
        .guild(guild_id, |k| k.mod_log.clone());
    card(ui, |ui| {
        section(
            ui,
            "🛡 Moderation log",
            "Every action an agent took, newest first.",
        );
        if log.is_empty() {
            ui.label(RichText::new("Nothing yet.").color(FAINT));
        }
        for action in log.iter().rev() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(widgets::ago(action.at))
                        .size(11.5)
                        .color(FAINT),
                );
                ui.label(RichText::new(&action.agent).strong().color(ACCENT));
                ui.label(
                    RichText::new(format!("{} {}", action.action, action.user_name)).color(TEXT),
                );
                ui.label(RichText::new(&action.reason).color(WEAK));
            });
        }
    });
}

fn channels(app: &mut App, ui: &mut Ui, guild_id: u64) {
    let channels: Vec<u64> = app
        .config
        .agents
        .iter()
        .filter_map(|a| app.sup.core(&a.id))
        .flat_map(|core| {
            core.world
                .with_guild(guild_id, |g| g.channels.keys().copied().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    card(ui, |ui| {
        section(
            ui,
            "Channel memory",
            "Recent history agents keep per channel, and the rolling summary older messages are compacted into.",
        );
        if channels.is_empty() {
            ui.label(
                RichText::new("Start an agent in this server to see its channels.").color(FAINT),
            );
        }
        for channel in channels {
            let (name, count, summary) = app.sup.hive.knowledge.channel(channel, |c| {
                (c.name.clone(), c.messages.len(), c.summary.clone())
            });
            if count == 0 && summary.is_empty() {
                continue;
            }
            egui::CollapsingHeader::new(
                RichText::new(format!("{name} — {count} messages kept")).color(TEXT),
            )
            .id_salt(("channel", channel))
            .show(ui, |ui| {
                if summary.is_empty() {
                    ui.label(
                        RichText::new("No summary yet (made once history gets long).").color(FAINT),
                    );
                } else {
                    ui.label(RichText::new(&summary).color(WEAK));
                }
                if ui.small_button("clear this channel's memory").clicked() {
                    app.sup.hive.knowledge.channel_mut(channel, |c| {
                        c.messages.clear();
                        c.summary.clear();
                        c.summary_upto = 0;
                    });
                }
            });
        }
    });
}
