//! Mission control: every agent at a glance, live thinking, activity.

use egui::{RichText, Ui};

use super::theme::{self, ACCENT, FAINT, TEXT, WEAK};
use super::widgets::{self, avatar, card, clickable_card, pill, stat, status_color};
use super::{AgentTab, App, Page};
use crate::runtime::live::FeedKind;

pub fn show(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            widgets::page_header(
                ui,
                "Mission control",
                "Your agents, what they're doing, and what they've done.",
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if !app.config.agents.is_empty() {
                if widgets::ghost_button(ui, "⏹  Stop all").clicked() {
                    app.sup.stop_all();
                }
                if widgets::primary_button(ui, "▶  Start all").clicked() {
                    let mut missing = Vec::new();
                    for agent in app.config.agents.clone() {
                        if agent.token.trim().is_empty() {
                            missing.push(agent.name.clone());
                        } else if !app.sup.is_running(&agent.id) {
                            app.sup.start(&agent);
                        }
                    }
                    if !missing.is_empty() {
                        app.toast(format!("No token yet for: {}", missing.join(", ")), true);
                    }
                }
            }
        });
    });

    if app.config.agents.is_empty() {
        onboarding(app, ui);
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Agent cards in a responsive grid.
            let width = ui.available_width();
            let columns = ((width / 330.0).floor() as usize).clamp(1, 4);
            let agents = app.config.agents.clone();
            for row in agents.chunks(columns) {
                ui.columns(columns, |cols| {
                    for (index, agent) in row.iter().enumerate() {
                        let ui = &mut cols[index];
                        let live = app.sup.live.snapshot(&agent.id);
                        let accent = theme::rgb(agent.accent);
                        let response = clickable_card(ui, accent, |ui| {
                            ui.horizontal(|ui| {
                                avatar(ui, &agent.name, accent, 40.0);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(
                                        RichText::new(&agent.name).size(16.0).strong().color(TEXT),
                                    );
                                    ui.label(
                                        RichText::new(crate::llm::truncate(&agent.specialty, 40))
                                            .size(12.0)
                                            .color(WEAK),
                                    );
                                });
                            });
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                let color = status_color(&live.status, live.paused);
                                pill(
                                    ui,
                                    &if live.paused {
                                        "paused".to_owned()
                                    } else {
                                        crate::llm::truncate(&live.status.label(), 30)
                                    },
                                    color,
                                );
                                let model = if agent.model.model.is_empty() {
                                    "no model".to_owned()
                                } else {
                                    crate::llm::truncate(&agent.model.model, 26)
                                };
                                pill(ui, &model, theme::BLUE);
                                if agent.behavior.moderation.enabled {
                                    pill(ui, "🛡 mod", theme::YELLOW);
                                }
                            });
                            ui.add_space(4.0);
                            match live.thoughts.first() {
                                Some(thought) => {
                                    ui.label(
                                        RichText::new(format!(
                                            "💭 {} · {}",
                                            thought.channel,
                                            crate::llm::truncate(&thought.about, 50)
                                        ))
                                        .size(12.5)
                                        .color(ACCENT),
                                    );
                                }
                                None => {
                                    let guilds = live.guilds.len();
                                    ui.label(
                                        RichText::new(if guilds > 0 {
                                            format!(
                                                "In {guilds} server{}",
                                                if guilds == 1 { "" } else { "s" }
                                            )
                                        } else {
                                            "Not in any server yet".to_owned()
                                        })
                                        .size(12.5)
                                        .color(FAINT),
                                    );
                                }
                            }
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                stat(ui, "replies", widgets::compact_number(live.stats.replies));
                                ui.add_space(10.0);
                                stat(ui, "tools", widgets::compact_number(live.stats.tool_calls));
                                ui.add_space(10.0);
                                stat(
                                    ui,
                                    "tokens",
                                    widgets::compact_number(
                                        live.stats.tokens_in + live.stats.tokens_out,
                                    ),
                                );
                                ui.add_space(10.0);
                                stat(
                                    ui,
                                    "memories",
                                    widgets::compact_number(live.memory_count as u64),
                                );
                            });
                        });
                        if response.clicked() {
                            app.page = Page::Agent(agent.id.clone());
                            app.agent_tab = AgentTab::Overview;
                        }
                    }
                });
                ui.add_space(8.0);
            }

            ui.add_space(8.0);
            ui.columns(2, |cols| {
                thinking_now(app, &mut cols[0]);
                activity(app, &mut cols[1], None);
            });
            ui.add_space(8.0);
            process_log(app, ui);
        });
}

fn thinking_now(app: &App, ui: &mut Ui) {
    card(ui, |ui| {
        widgets::section(
            ui,
            "💭 Thinking right now",
            "Live reasoning and tool use, as it happens.",
        );
        let mut any = false;
        for agent in &app.config.agents {
            for thought in app.sup.live.snapshot(&agent.id).thoughts {
                any = true;
                ui.horizontal(|ui| {
                    avatar(ui, &agent.name, theme::rgb(agent.accent), 22.0);
                    ui.label(RichText::new(&agent.name).strong().color(TEXT));
                    ui.label(
                        RichText::new(format!(
                            "in {} · {}s",
                            thought.channel,
                            thought.started.elapsed().as_secs()
                        ))
                        .size(12.0)
                        .color(WEAK),
                    );
                });
                ui.label(
                    RichText::new(crate::llm::truncate(&thought.about, 120))
                        .size(12.5)
                        .color(FAINT),
                );
                for step in thought.steps.iter().rev().take(3).rev() {
                    ui.label(
                        RichText::new(format!("🔧 {step}"))
                            .size(12.5)
                            .color(theme::BLUE),
                    );
                }
                if !thought.reasoning.trim().is_empty() {
                    let tail: String = {
                        let chars: Vec<char> = thought.reasoning.trim().chars().collect();
                        chars[chars.len().saturating_sub(260)..].iter().collect()
                    };
                    ui.label(
                        RichText::new(format!("“…{tail}”"))
                            .italics()
                            .size(12.5)
                            .color(WEAK),
                    );
                }
                if !thought.answer.trim().is_empty() {
                    ui.label(
                        RichText::new(crate::llm::truncate(thought.answer.trim(), 300))
                            .size(13.0)
                            .color(TEXT),
                    );
                }
                ui.separator();
            }
        }
        if !any {
            ui.label(RichText::new("Nobody is thinking right now.").color(FAINT));
        }
    });
}

/// The full process history: every job the agents ran, kept after it
/// finished — what it was about, the steps it took, its reasoning and
/// answer. Nothing disappears when a job completes.
pub fn process_log(app: &App, ui: &mut Ui) {
    let log = app.sup.live.process_log(60);
    card(ui, |ui| {
        widgets::section(
            ui,
            "📜 Process log",
            "Every job, kept after it finishes — steps, reasoning, and the answer.",
        );
        if log.is_empty() {
            ui.label(RichText::new("Nothing yet.").color(FAINT));
            return;
        }
        for entry in &log {
            egui::CollapsingHeader::new(
                RichText::new(format!(
                    "{} · #{} · {}",
                    entry.agent_id,
                    entry.channel,
                    crate::llm::truncate(&entry.about, 80)
                ))
                .size(12.5)
                .color(TEXT),
            )
            .id_salt(("process", entry.id))
            .show_unindented(ui, |ui| {
                let duration = entry
                    .ended_ms
                    .map(|end| format!("{}s", (end - entry.started_ms) / 1000))
                    .unwrap_or_else(|| "running".to_owned());
                ui.label(
                    RichText::new(format!(
                        "{} · {} step{}",
                        duration,
                        entry.steps.len(),
                        if entry.steps.len() == 1 { "" } else { "s" }
                    ))
                    .size(12.0)
                    .color(WEAK),
                );
                for (n, step) in entry.steps.iter().enumerate() {
                    ui.label(
                        RichText::new(format!("{}. {}", n + 1, step))
                            .size(12.5)
                            .color(theme::BLUE),
                    );
                }
                if !entry.reasoning.trim().is_empty() {
                    ui.label(
                        RichText::new(crate::llm::truncate(entry.reasoning.trim(), 500))
                            .italics()
                            .size(12.5)
                            .color(WEAK),
                    );
                }
                if !entry.answer.trim().is_empty() {
                    ui.separator();
                    ui.label(
                        RichText::new(crate::llm::truncate(entry.answer.trim(), 700))
                            .size(13.0)
                            .color(TEXT),
                    );
                }
            });
        }
    });
}

pub fn activity(app: &mut App, ui: &mut Ui, agent: Option<&str>) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            widgets::section(ui, "Activity", "");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("clear").clicked() {
                    app.sup.live.clear_feed();
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            let kinds = [
                (None, "All"),
                (Some(FeedKind::Reply), "💬 Replies"),
                (Some(FeedKind::Tool), "🔧 Tools"),
                (Some(FeedKind::Moderation), "🛡 Moderation"),
                (Some(FeedKind::Memory), "🧠 Memory"),
                (Some(FeedKind::Coordination), "🤝 Team"),
                (Some(FeedKind::Learning), "📚 Learning"),
                (Some(FeedKind::Error), "⚠ Errors"),
            ];
            for (kind, label) in kinds {
                let selected = app.feed_kind == kind;
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).size(12.0))
                            .selected(selected)
                            .small(),
                    )
                    .clicked()
                {
                    app.feed_kind = kind;
                }
            }
        });
        ui.add_space(4.0);
        let items = app.sup.live.feed_snapshot(agent, 300);
        egui::ScrollArea::vertical()
            .id_salt(("feed", agent))
            .max_height(420.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let mut shown = 0;
                for item in items
                    .iter()
                    .filter(|i| app.feed_kind.is_none_or(|k| k == i.kind))
                {
                    shown += 1;
                    let color = match item.kind {
                        FeedKind::Error => theme::RED,
                        FeedKind::Moderation => theme::YELLOW,
                        FeedKind::Memory | FeedKind::Learning => theme::GREEN,
                        FeedKind::Coordination => ACCENT,
                        _ => TEXT,
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(item.kind.icon()).color(color));
                        if agent.is_none() {
                            ui.label(RichText::new(&item.agent_name).strong().color(TEXT));
                        }
                        if let Some(channel) = &item.channel {
                            ui.label(RichText::new(channel).size(12.0).color(WEAK));
                        }
                        ui.label(RichText::new(widgets::ago(item.at)).size(11.0).color(FAINT));
                    });
                    ui.label(RichText::new(&item.text).size(13.0).color(
                        if item.kind == FeedKind::Error {
                            theme::RED
                        } else {
                            theme::TEXT
                        },
                    ));
                    ui.add_space(4.0);
                }
                if shown == 0 {
                    ui.label(RichText::new("Nothing yet.").color(FAINT));
                }
            });
    });
}

fn onboarding(app: &mut App, ui: &mut Ui) {
    card(ui, |ui| {
        ui.label(
            RichText::new("Let's build your agent team")
                .size(20.0)
                .strong()
                .color(TEXT),
        );
        ui.add_space(6.0);
        let steps = [
            (
                "1",
                "Create a bot for each agent",
                "discord.com/developers › New Application › Bot › Reset Token. Turn on the Server Members and Message Content intents.",
            ),
            (
                "2",
                "Run a model",
                "Start LM Studio's local server (or use OpenAI, Anthropic, OpenRouter, Groq, Gemini, Ollama…).",
            ),
            (
                "3",
                "Create an agent here",
                "Give it a name, a personality, its token, and a model.",
            ),
            (
                "4",
                "Invite it",
                "Use the invite link on the agent's Discord tab, then press Start.",
            ),
        ];
        for (number, title, body) in steps {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(number).size(18.0).strong().color(ACCENT));
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).strong().color(TEXT));
                    ui.label(RichText::new(body).color(WEAK));
                });
            });
            ui.add_space(4.0);
        }
        ui.add_space(8.0);
        if widgets::primary_button(ui, "➕  Create your first agent").clicked() {
            app.new_agent();
        }
    });
}
