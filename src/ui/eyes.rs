//! The bot's eyes: everything the agents actually know, straight from
//! the app's own state — guilds, channels, messages, reactions — never a
//! fresh Discord API fetch. What you see here is exactly what the bot
//! sees. And you can act AS the bot from here: react, reply, type, send.

use eframe::egui::{RichText, Ui};
use std::sync::Arc;

use crate::runtime::agent::AgentCore;
use crate::runtime::live::AgentStatus;

use super::theme::*;
use super::{App, widgets};

pub fn show(app: &mut App, ui: &mut Ui) {
    // Which agent's eyes? The first online one, else the first at all.
    if app.eyes_agent.is_none()
        || !app
            .config
            .agents
            .iter()
            .any(|a| Some(&a.id) == app.eyes_agent.as_ref())
    {
        app.eyes_agent = app.config.agents.first().map(|a| a.id.clone());
    }
    let Some(agent_id) = app.eyes_agent.clone() else {
        ui.label(RichText::new("No agents yet — add one first.").color(FAINT));
        return;
    };
    let Some(profile) = app.config.agents.iter().find(|a| a.id == agent_id).cloned() else {
        return;
    };
    let core = app.sup.core(&agent_id);

    ui.horizontal(|ui| {
        widgets::section(
            ui,
            "👁 Through the bot's eyes",
            "Exactly what the bot knows — no Discord API fetch. React, reply, and send as the bot.",
        );
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("agent:").size(12.0).color(WEAK));
        egui::ComboBox::from_id_salt("eyes_agent")
            .selected_text(profile.name.clone())
            .width(180.0)
            .show_ui(ui, |ui| {
                for agent in &app.config.agents {
                    if ui
                        .selectable_label(Some(&agent.id) == app.eyes_agent.as_ref(), &agent.name)
                        .clicked()
                    {
                        app.eyes_agent = Some(agent.id.clone());
                        app.eyes_guild = None;
                        app.eyes_channel = None;
                    }
                }
            });
        let online = app.sup.live.snapshot(&agent_id).status;
        match online {
            AgentStatus::Online => ui.label(RichText::new("● online").color(GREEN)),
            _ => ui.label(
                RichText::new(format!("● {online:?} (actions need the agent online)"))
                    .color(YELLOW),
            ),
        };
    });
    ui.add_space(6.0);

    let Some(core) = core else {
        ui.label(
            RichText::new("This agent isn't running — start it to see through its eyes.")
                .color(FAINT),
        );
        return;
    };

    ui.horizontal(|ui| {
        ui.set_min_height(ui.available_height() - 8.0);
        channels_panel(app, ui, &core);
        ui.separator();
        messages_panel(app, ui, &core);
    });
}

/// Left: every guild + channel the bot knows, from its world cache.
fn channels_panel(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>) {
    egui::ScrollArea::vertical()
        .id_salt("eyes_channels")
        .max_width(220.0)
        .show(ui, |ui| {
            let guilds: Vec<(u64, String, Vec<(u64, String, bool)>)> = core
                .world
                .guild_ids()
                .into_iter()
                .filter_map(|g| {
                    core.world.with_guild(g, |state| {
                        (
                            g,
                            state.name.clone(),
                            state
                                .channels
                                .values()
                                .map(|c| {
                                    (c.id.get(), c.name.clone().unwrap_or_default(), c.is_voice())
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                })
                .collect();
            if guilds.is_empty() {
                ui.label(RichText::new("No servers known yet.").color(FAINT));
            }
            for (guild_id, guild_name, channels) in guilds {
                ui.label(RichText::new(guild_name).strong().color(TEXT));
                for (channel_id, channel_name, is_voice) in channels {
                    let mark = if is_voice { "🔊 " } else { "# " };
                    let selected = app.eyes_channel == Some(channel_id);
                    if ui
                        .selectable_label(selected, format!("{mark}{channel_name}"))
                        .clicked()
                    {
                        app.eyes_guild = Some(guild_id);
                        app.eyes_channel = Some(channel_id);
                        app.eyes_reply_to = None;
                    }
                }
                ui.add_space(6.0);
            }
        });
}

/// Right: the messages the bot has archived for the selected channel —
/// authors, content, reactions — plus the composer to act as the bot.
fn messages_panel(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>) {
    let Some(channel_id) = app.eyes_channel else {
        ui.label(
            RichText::new(
                "Pick a channel on the left — what you'll see is the bot's own archive of it.",
            )
            .color(FAINT),
        );
        return;
    };
    let messages = core.hive.knowledge.channel(channel_id, |archive| {
        archive
            .messages
            .iter()
            .filter(|m| !m.deleted)
            .rev()
            .take(80)
            .map(|m| {
                (
                    m.id,
                    m.author.clone(),
                    m.author_id,
                    m.content.clone(),
                    m.reactions.clone(),
                    m.attachments
                        .iter()
                        .map(|a| a.name.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    });
    let my_id = core.world.my_id();

    let reply_to = app.eyes_reply_to;
    let agent_id = app.eyes_agent.clone().unwrap_or_default();

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} messages", messages.len()))
                .size(12.0)
                .color(WEAK),
        );
        if let Some(id) = reply_to {
            ui.label(
                RichText::new(format!("replying to {id}"))
                    .size(12.0)
                    .color(BLUE),
            );
            if ui.small_button("cancel reply").clicked() {
                app.eyes_reply_to = None;
            }
        }
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .id_salt("eyes_messages")
        .show(ui, |ui| {
            if messages.is_empty() {
                ui.label(RichText::new("The bot hasn't seen any messages here yet.").color(FAINT));
            }
            for (id, author, author_id, content, reactions, attachments) in messages.iter().rev() {
                let mine = *author_id == my_id;
                egui::Frame::new()
                    .fill(if mine {
                        egui::Color32::from_rgb(38, 42, 54)
                    } else {
                        egui::Color32::from_rgb(30, 33, 43)
                    })
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{author}{}",
                                    if mine { " (you)" } else { "" }
                                ))
                                .strong()
                                .color(if mine {
                                    BLUE
                                } else {
                                    TEXT
                                }),
                            );
                            ui.label(RichText::new(format!("· {id}")).size(11.0).color(FAINT));
                        });
                        if !content.trim().is_empty() {
                            ui.label(RichText::new(content).color(TEXT));
                        }
                        for file in attachments {
                            ui.label(RichText::new(format!("📎 {file}")).size(12.0).color(WEAK));
                        }
                        // Act as the bot, on this message.
                        ui.horizontal(|ui| {
                            for emoji in ["👍", "❤️", "😂", "🎉"] {
                                if ui
                                    .small_button(format!("{emoji}"))
                                    .on_hover_text("react as the bot")
                                    .clicked()
                                {
                                    react_as_bot(app, core, channel_id, *id, emoji);
                                }
                            }
                            let more = if app.eyes_react_for == Some(*id) {
                                "▾ any emoji"
                            } else {
                                "▸ any emoji"
                            };
                            if ui.small_button(more).clicked() {
                                app.eyes_react_for = if app.eyes_react_for == Some(*id) {
                                    None
                                } else {
                                    Some(*id)
                                };
                            }
                            if ui.small_button("↩ reply").clicked() {
                                app.eyes_reply_to = Some(*id);
                            }
                        });
                        if app.eyes_react_for == Some(*id) {
                            react_picker(app, ui, core, channel_id, *id);
                        }
                        if !reactions.is_empty() {
                            ui.horizontal(|ui| {
                                for (emoji, count) in reactions {
                                    ui.label(
                                        RichText::new(format!("{emoji} {count}"))
                                            .size(12.5)
                                            .color(WEAK),
                                    );
                                }
                            });
                        }
                    });
                ui.add_space(2.0);
            }
        });

    ui.separator();
    // The composer: speak as the bot in this channel.
    ui.horizontal(|ui| {
        let hint = if app.eyes_reply_to.is_some() {
            "Reply as the bot…"
        } else {
            "Send as the bot…"
        };
        let response = ui.add(
            egui::TextEdit::singleline(&mut app.eyes_input)
                .hint_text(hint)
                .desired_width(ui.available_width() - 200.0),
        );
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let _ = &agent_id;
        if (widgets::primary_button(ui, "Send").clicked() || enter)
            && !app.eyes_input.trim().is_empty()
        {
            send_as_bot(app, core, channel_id, app.eyes_input.trim().to_owned());
            app.eyes_input.clear();
            app.eyes_reply_to = None;
            response.request_focus();
        }
        if ui
            .button("Typing…")
            .on_hover_text("show the bot typing here")
            .clicked()
        {
            type_as_bot(app, core, channel_id);
        }
    });
}

/// Any-emoji picker: free text (unicode or `<:name:id>` / `name:id` emote
/// strings) plus every server emote the bot actually knows — its own
/// custom emotes show here as soon as it shares a server with them.
fn react_picker(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>, channel: u64, message: u64) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(24, 26, 35))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("react:").size(12.0).color(WEAK));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut app.eyes_emoji_input)
                        .hint_text("any emoji, or <:name:id> / name:id")
                        .desired_width(220.0),
                );
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let fire = (enter || ui.small_button("react").clicked())
                    && !app.eyes_emoji_input.trim().is_empty();
                if fire {
                    let emoji = app.eyes_emoji_input.trim().to_owned();
                    react_as_bot(app, core, channel, message, &emoji);
                    app.eyes_emoji_input.clear();
                    app.eyes_react_for = None;
                }
            });
            // Server emotes the bot knows (every guild it's in).
            let emotes: Vec<(String, String, bool)> = core
                .world
                .guild_ids()
                .into_iter()
                .filter_map(|g| {
                    core.world.with_guild(g, |state| {
                        state
                            .emojis
                            .iter()
                            .filter_map(|e| {
                                let name = e.name.clone()?;
                                let id = e.id?;
                                let animated = e.animated.unwrap_or(false);
                                let wire = format!(
                                    "{}:{name}:{}",
                                    if animated { "a" } else { "" },
                                    id.get()
                                );
                                Some((format!(":{name}:"), format!("<{wire}>"), animated))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .flatten()
                .collect();
            if !emotes.is_empty() {
                ui.label(
                    RichText::new("server emotes the bot knows:")
                        .size(11.5)
                        .color(FAINT),
                );
                ui.horizontal_wrapped(|ui| {
                    for (label, wire, _animated) in emotes.iter().take(60) {
                        if ui.small_button(label).on_hover_text(wire).clicked() {
                            react_as_bot(app, core, channel, message, wire);
                            app.eyes_react_for = None;
                        }
                    }
                });
            }
        });
}

fn react_as_bot(app: &mut App, core: &Arc<AgentCore>, channel: u64, message: u64, emoji: &str) {
    let core = Arc::clone(core);
    let channel = axis::prelude::ChannelId::new(channel);
    let message = axis::prelude::MessageId::new(message);
    let emoji = emoji.to_owned();
    app.sup.spawn(async move {
        if let Err(error) = core
            .http
            .add_reaction(channel, message, crate::tools::discord::parse_emoji(&emoji))
            .await
        {
            core.feed(
                crate::runtime::live::FeedKind::Error,
                format!(
                    "eyes: could not react: {}",
                    crate::tools::discord_error(error)
                ),
                None,
            );
        }
    });
}

fn send_as_bot(app: &mut App, core: &Arc<AgentCore>, channel: u64, text: String) {
    let core = Arc::clone(core);
    let channel = axis::prelude::ChannelId::new(channel);
    let reply = app.eyes_reply_to.map(axis::prelude::MessageId::new);
    app.sup.spawn(async move {
        let mut request = core
            .http
            .create_message(channel)
            .content(text)
            .allowed_mentions(axis::model::AllowedMentions::none());
        if let Some(reply) = reply {
            request = request.reply(reply);
        }
        match request.await {
            Ok(sent) => {
                // The bot's own words go into its archive like any gateway
                // message would — the eyes view stays truthful.
                core.hive.knowledge.channel_mut(channel.get(), |archive| {
                    archive.push(crate::memory::StoredMessage {
                        id: sent.id.get(),
                        author_id: core.world.my_id(),
                        author: core.name(),
                        author_bot: true,
                        content: sent.content,
                        ts: crate::memory::now_ms(),
                        reply_to: reply.map(|r| r.get()),
                        reply_author: None,
                        reply_snippet: None,
                        attachments: vec![],
                        deleted: false,
                        edited: false,
                        reactions: vec![],
                    });
                });
            }
            Err(error) => {
                core.feed(
                    crate::runtime::live::FeedKind::Error,
                    format!(
                        "eyes: could not send: {}",
                        crate::tools::discord_error(error)
                    ),
                    None,
                );
            }
        }
    });
}

fn type_as_bot(app: &mut App, core: &Arc<AgentCore>, channel: u64) {
    let core = Arc::clone(core);
    let channel = axis::prelude::ChannelId::new(channel);
    app.sup.spawn(async move {
        let _ = core.http.trigger_typing(channel).await;
    });
}
