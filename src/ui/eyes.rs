//! The bot's eyes: everything the agents actually know, straight from
//! the app's own state — never a fresh Discord API fetch. Laid out like
//! Discord: a server rail, a channel sidebar, and the chat view with a
//! composer — and you act AS the bot from here.

use eframe::egui::{Align, Color32, Layout, RichText, Ui};
use std::sync::Arc;

use super::App;
use super::theme::*;
use super::widgets;
use crate::runtime::agent::AgentCore;
use crate::runtime::live::AgentStatus;

/// One channel as the sidebar sees it.
struct Chan {
    id: u64,
    name: String,
    category: bool,
    voice: bool,
    forum: bool,
    thread: bool,
    parent: u64,
    pos: i32,
}

const CHAT_BG: Color32 = Color32::from_rgb(16, 18, 25);
const MESSAGE_OWN: Color32 = Color32::from_rgb(29, 33, 47);

pub fn show(app: &mut App, ui: &mut Ui) {
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

    // --- top bar: agent picker + status -------------------------------------
    egui::TopBottomPanel::top("eyes_top").show_inside(ui, |ui| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("👁 Through the bot's eyes")
                    .strong()
                    .color(TEXT),
            );
            ui.separator();
            egui::ComboBox::from_id_salt("eyes_agent")
                .selected_text(profile.name.clone())
                .width(170.0)
                .show_ui(ui, |ui| {
                    for agent in &app.config.agents {
                        if ui
                            .selectable_label(
                                Some(&agent.id) == app.eyes_agent.as_ref(),
                                &agent.name,
                            )
                            .clicked()
                        {
                            app.eyes_agent = Some(agent.id.clone());
                            app.eyes_guild = None;
                            app.eyes_channel = None;
                            app.eyes_reply_to = None;
                        }
                    }
                });
            let status = app.sup.live.snapshot(&agent_id).status;
            match status {
                AgentStatus::Online => ui.label(RichText::new("● online").color(GREEN)),
                other => ui.label(
                    RichText::new(format!("● {other:?} — start the agent to act as it"))
                        .color(YELLOW),
                ),
            };
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new("what the bot knows · act as the bot — no API fetch")
                        .size(11.5)
                        .color(FAINT),
                );
            });
        });
        ui.add_space(4.0);
    });

    let core = app.sup.core(&agent_id);
    let Some(core) = core else {
        ui.centered_and_justified(|ui| {
            ui.label(
                RichText::new("This agent isn't running — start it to see through its eyes.")
                    .color(FAINT),
            );
        });
        return;
    };

    // --- Discord layout: server rail → channel sidebar → chat ---------------
    egui::SidePanel::left("eyes_guilds")
        .exact_width(64.0)
        .frame(egui::Frame::new().fill(SIDEBAR))
        .show_inside(ui, |ui| {
            guild_rail(app, ui, &core);
        });
    egui::SidePanel::left("eyes_channels")
        .exact_width(190.0)
        .frame(egui::Frame::new().fill(PANEL))
        .show_inside(ui, |ui| {
            channel_list(app, ui, &core);
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(CHAT_BG))
        .show_inside(ui, |ui| {
            chat(app, ui, &core);
        });
}

/// The Discord server rail: one rounded icon per guild the bot knows.
fn guild_rail(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>) {
    ui.add_space(6.0);
    ui.vertical_centered(|ui| {
        let guilds: Vec<(u64, String)> = core
            .world
            .guild_ids()
            .into_iter()
            .filter_map(|g| core.world.with_guild(g, |s| (g, s.name.clone())))
            .collect();
        if guilds.is_empty() {
            ui.label(RichText::new("no\nservers").size(11.0).color(FAINT));
        }
        egui::ScrollArea::vertical()
            .id_salt("eyes_guild_scroll")
            .show(ui, |ui| {
                for (guild_id, name) in guilds {
                    let selected = app.eyes_guild == Some(guild_id);
                    // Discord-style: selected server = accent tile + left pill.
                    let color = if selected { ACCENT } else { dim(BLUE, 60) };
                    let response = widgets::avatar(ui, &name, color, 42.0);
                    if selected {
                        let rect = response.rect;
                        ui.painter().rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(rect.left() - 10.0, rect.center().y - 12.0),
                                egui::pos2(rect.left() - 5.0, rect.center().y + 12.0),
                            ),
                            egui::CornerRadius::same(3),
                            ACCENT,
                        );
                    }
                    if response.clicked() {
                        app.eyes_guild = Some(guild_id);
                        app.eyes_channel = None;
                        app.eyes_reply_to = None;
                    }
                    response.on_hover_text(&name);
                    ui.add_space(6.0);
                }
            });
    });
}

/// The channel sidebar for the selected guild, structured like Discord:
/// categories are collapsible sections holding their channels; forum
/// channels collapse into their posts (threads).
fn channel_list(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>) {
    let Some(guild_id) = app.eyes_guild else {
        ui.add_space(10.0);
        ui.label(
            RichText::new("Pick a server on the rail — only servers the bot is in appear.")
                .size(12.0)
                .color(FAINT),
        );
        return;
    };
    let Some(guild) = core.world.with_guild(guild_id, |s| {
        (
            s.name.clone(),
            s.channels
                .values()
                .map(|c| Chan {
                    id: c.id.get(),
                    name: c.name.clone().unwrap_or_default(),
                    category: matches!(c.r#type, axis::model::ChannelType::GuildCategory),
                    voice: c.is_voice(),
                    forum: c.is_forum(),
                    thread: c.is_thread(),
                    parent: c.parent_id.map(|p| p.get()).unwrap_or(0),
                    pos: c.position.unwrap_or(0),
                })
                .collect::<Vec<_>>(),
        )
    }) else {
        return;
    };
    ui.add_space(8.0);
    ui.label(RichText::new(guild.0).strong().color(TEXT));
    ui.add_space(4.0);
    let chans = guild.1;
    let sorted_cats = {
        let mut v: Vec<&Chan> = chans.iter().filter(|c| c.category).collect();
        v.sort_by_key(|c| c.pos);
        v
    };
    let top = {
        let mut v: Vec<&Chan> = chans
            .iter()
            .filter(|c| !c.category && !c.thread && c.parent == 0)
            .collect();
        v.sort_by_key(|c| c.pos);
        v
    };
    egui::ScrollArea::vertical()
        .id_salt("eyes_channel_scroll")
        .show(ui, |ui| {
            let text_top: Vec<&Chan> = top.iter().filter(|c| !c.voice).copied().collect();
            let voice_top: Vec<&Chan> = top.iter().filter(|c| c.voice).copied().collect();
            if !text_top.is_empty() {
                ui.label(RichText::new("TEXT CHANNELS").size(10.5).color(FAINT));
                for channel in &text_top {
                    channel_entry(app, ui, channel, &chans);
                }
                ui.add_space(6.0);
            }
            // Categories, each collapsible with its channels inside.
            for category in &sorted_cats {
                let children: Vec<&Chan> = {
                    let mut v: Vec<&Chan> = chans
                        .iter()
                        .filter(|c| !c.thread && c.parent == category.id)
                        .collect();
                    v.sort_by_key(|c| c.pos);
                    v
                };
                let contains_selected = children.iter().any(|c| app.eyes_channel == Some(c.id))
                    || app.eyes_channel == Some(category.id);
                egui::CollapsingHeader::new(
                    RichText::new(format!("▾ {}", category.name))
                        .size(11.0)
                        .color(WEAK),
                )
                .default_open(contains_selected)
                .id_salt(("eyes_cat", category.id))
                .show_unindented(ui, |ui| {
                    for channel in &children {
                        channel_entry(app, ui, channel, &chans);
                    }
                });
            }
            ui.add_space(6.0);
            if !voice_top.is_empty() {
                ui.label(RichText::new("VOICE CHANNELS").size(10.5).color(FAINT));
                for channel in &voice_top {
                    channel_entry(app, ui, channel, &chans);
                }
            }
            if text_top.is_empty() && voice_top.is_empty() && sorted_cats.is_empty() {
                ui.label(RichText::new("No channels known.").size(12.0).color(FAINT));
            }
        });
}

/// One channel row; forums collapse into their posts (threads).
fn channel_entry(app: &mut App, ui: &mut Ui, channel: &Chan, chans: &[Chan]) {
    let selected = app.eyes_channel == Some(channel.id);
    let mark = if channel.voice {
        "🔊"
    } else if channel.forum {
        "📋"
    } else {
        "#"
    };
    if channel.forum {
        let posts: Vec<&Chan> = chans
            .iter()
            .filter(|c| c.thread && c.parent == channel.id)
            .collect();
        let open = app
            .eyes_forum_open
            .map(|id| id == channel.id)
            .unwrap_or(false);
        let toggle_label = if open { "▾" } else { "▸" };
        ui.horizontal(|ui| {
            let button = egui::Button::new(
                RichText::new(format!("{mark}  {}", channel.name))
                    .size(13.0)
                    .color(if selected { TEXT } else { WEAK }),
            )
            .fill(if selected {
                CARD_HOVER
            } else {
                Color32::TRANSPARENT
            })
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(ui.available_width() - 22.0, 24.0));
            if ui.add(button).clicked() {
                app.eyes_channel = Some(channel.id);
                app.eyes_reply_to = None;
            }
            if ui
                .small_button(toggle_label)
                .on_hover_text("show/hide this forum's posts")
                .clicked()
            {
                app.eyes_forum_open = if open { None } else { Some(channel.id) };
            }
        });
        if open {
            for post in posts {
                let post_selected = app.eyes_channel == Some(post.id);
                let button = egui::Button::new(
                    RichText::new(format!("  🧵  {}", post.name))
                        .size(12.5)
                        .color(if post_selected { TEXT } else { FAINT }),
                )
                .fill(if post_selected {
                    CARD_HOVER
                } else {
                    Color32::TRANSPARENT
                })
                .corner_radius(egui::CornerRadius::same(6))
                .min_size(egui::vec2(ui.available_width(), 22.0));
                if ui.add(button).clicked() {
                    app.eyes_channel = Some(post.id);
                    app.eyes_reply_to = None;
                }
            }
        }
        return;
    }
    let button = egui::Button::new(
        RichText::new(format!("{mark}  {}", channel.name))
            .size(13.0)
            .color(if selected { TEXT } else { WEAK }),
    )
    .fill(if selected {
        CARD_HOVER
    } else {
        Color32::TRANSPARENT
    })
    .corner_radius(egui::CornerRadius::same(6))
    .min_size(egui::vec2(ui.available_width(), 24.0));
    if ui.add(button).clicked() {
        app.eyes_channel = Some(channel.id);
        app.eyes_reply_to = None;
    }
}

/// The chat: message history (the bot's own archive) + composer.
fn chat(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>) {
    let Some(channel_id) = app.eyes_channel else {
        ui.centered_and_justified(|ui| {
            ui.label(
                RichText::new(
                    "Pick a channel — you'll see exactly the messages the bot has archived, nothing fetched fresh.",
                )
                .color(FAINT),
            );
        });
        return;
    };
    let channel_name = app
        .eyes_guild
        .and_then(|g| {
            core.world.with_guild(g, |s| {
                s.channels
                    .get(&channel_id)
                    .and_then(|c| c.name.clone())
                    .unwrap_or_default()
            })
        })
        .unwrap_or_default();

    // composer pinned to the bottom, Discord-style
    egui::TopBottomPanel::bottom("eyes_composer")
        .frame(
            egui::Frame::new()
                .fill(CHAT_BG)
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show_inside(ui, |ui| composer(app, ui, core, channel_id));

    egui::TopBottomPanel::top("eyes_chat_head")
        .frame(
            egui::Frame::new()
                .fill(CHAT_BG)
                .inner_margin(egui::Margin::symmetric(12, 6)),
        )
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("# {channel_name}"))
                        .strong()
                        .color(TEXT),
                );
                if ui
                    .small_button("↻")
                    .on_hover_text(
                        "fetch the latest messages from Discord (API) into the bot's view",
                    )
                    .clicked()
                {
                    refresh_from_api(app, core, channel_id);
                }
                if let Some(id) = app.eyes_reply_to {
                    ui.label(
                        RichText::new(format!("· replying to {id}"))
                            .size(11.5)
                            .color(BLUE),
                    );
                    if ui.small_button("✕").clicked() {
                        app.eyes_reply_to = None;
                    }
                }
            });
            ui.separator();
        });

    let messages = core.hive.knowledge.channel(channel_id, |archive| {
        archive
            .messages
            .iter()
            .filter(|m| !m.deleted)
            .rev()
            .take(60)
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
                    m.ts,
                )
            })
            .collect::<Vec<_>>()
    });
    let my_id = core.world.my_id();

    egui::ScrollArea::vertical()
        .id_salt("eyes_chat_scroll")
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.add_space(4.0);
            if messages.is_empty() {
                ui.label(RichText::new("The bot hasn't seen any messages here yet.").color(FAINT));
            }
            // oldest first (the archive gave newest first)
            for (id, author, author_id, content, reactions, attachments, ts) in
                messages.iter().rev()
            {
                message_row(
                    app,
                    ui,
                    core,
                    channel_id,
                    my_id,
                    (
                        *id,
                        author,
                        *author_id,
                        content,
                        reactions,
                        attachments,
                        *ts,
                    ),
                );
            }
            ui.add_space(4.0);
        });
}

#[allow(clippy::too_many_arguments)]
fn message_row(
    app: &mut App,
    ui: &mut Ui,
    core: &Arc<AgentCore>,
    channel: u64,
    my_id: u64,
    message: (u64, &str, u64, &str, &[(String, u32)], &[String], i64),
) {
    let (id, author, author_id, content, reactions, attachments, ts) = message;
    let mine = author_id == my_id;
    let frame = if mine {
        egui::Frame::new()
            .fill(MESSAGE_OWN)
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(10, 6))
    } else {
        egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 6))
    };
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            // avatar + name + time (Discord message header)
            widgets::avatar(ui, author, if mine { ACCENT } else { dim(BLUE, 60) }, 26.0);
            ui.label(
                RichText::new(author)
                    .strong()
                    .color(if mine { ACCENT } else { TEXT }),
            );
            let time = crate::memory::now_ms() - ts;
            let ago = if time < 60_000 {
                "just now".to_owned()
            } else if time < 3_600_000 {
                format!("{}m ago", time / 60_000)
            } else {
                format!("{}h ago", time / 3_600_000)
            };
            ui.label(RichText::new(ago).size(11.0).color(FAINT));
            // actions on the right: react / reply
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button("↩")
                    .on_hover_text("reply to this as the bot")
                    .clicked()
                {
                    app.eyes_reply_to = Some(id);
                }
                let picker_open = app.eyes_react_for == Some(id);
                if ui
                    .small_button(if picker_open { "😀▾" } else { "😀" })
                    .on_hover_text("react with any emoji or server emote")
                    .clicked()
                {
                    app.eyes_react_for = if picker_open { None } else { Some(id) };
                }
                ui.menu_button("⋯", |ui| {
                    let guild = app.eyes_guild.unwrap_or_default();
                    let bot_may =
                        crate::runtime::authority::bot_channel_permissions(core, guild, channel)
                            .contains(axis::prelude::Permissions::MANAGE_MESSAGES);
                    if ui
                        .add_enabled(bot_may, egui::Button::new("🗑 delete message"))
                        .on_disabled_hover_text("the bot lacks Manage Messages here")
                        .clicked()
                    {
                        delete_message_as_bot(app, core, channel, id);
                        ui.close_menu();
                    }
                    if ui.button("🧷 copy message id").clicked() {
                        ui.ctx().copy_text(id.to_string());
                        ui.close_menu();
                    }
                });
            });
        });
        if !content.trim().is_empty() {
            ui.label(RichText::new(content).color(TEXT));
        }
        for file in attachments {
            ui.label(RichText::new(format!("📎 {file}")).size(12.0).color(WEAK));
        }
        if !reactions.is_empty() {
            ui.horizontal(|ui| {
                for (emoji, count) in reactions {
                    // Clicking a chip removes the BOT'S reaction (unreact).
                    if ui
                        .small_button(format!("{emoji} {count}"))
                        .on_hover_text("remove the bot's reaction")
                        .clicked()
                    {
                        unreact_as_bot(app, core, channel, id, emoji);
                    }
                }
            });
        }
        if app.eyes_react_for == Some(id) {
            react_picker(app, ui, core, channel, id);
        }
        ui.add_space(2.0);
    });
}

/// Any-emoji picker: free text (unicode or `<:name:id>` emote strings)
/// plus every server emote the bot actually knows.
fn react_picker(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>, channel: u64, message: u64) {
    egui::Frame::new()
        .fill(INPUT)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for emoji in ["👍", "❤️", "😂", "🎉", "😮", "😢"] {
                    if ui.small_button(emoji).clicked() {
                        react_as_bot(app, core, channel, message, emoji);
                        app.eyes_react_for = None;
                    }
                }
                ui.separator();
                let response = ui.add(
                    egui::TextEdit::singleline(&mut app.eyes_emoji_input)
                        .hint_text("any emoji or <:name:id>")
                        .desired_width(170.0),
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
            // Emotes from the SELECTED server — the one being viewed —
            // not a mix of every server the bot is in.
            let emotes: Vec<(String, String)> = app
                .eyes_guild
                .and_then(|g| {
                    core.world.with_guild(g, |state| {
                        state
                            .emojis
                            .iter()
                            .filter_map(|e| {
                                let name = e.name.clone()?;
                                let id = e.id?;
                                let animated = e.animated.unwrap_or(false);
                                Some((
                                    format!(":{name}:"),
                                    format!(
                                        "<{}:{name}:{}>",
                                        if animated { "a" } else { "" },
                                        id.get()
                                    ),
                                ))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            if !emotes.is_empty() {
                ui.label(
                    RichText::new("server emotes (this server):")
                        .size(11.0)
                        .color(FAINT),
                );
                ui.horizontal_wrapped(|ui| {
                    for (label, wire) in emotes.iter().take(80) {
                        if ui.small_button(label).on_hover_text(wire).clicked() {
                            react_as_bot(app, core, channel, message, wire);
                            app.eyes_react_for = None;
                        }
                    }
                });
            }
        });
}

fn composer(app: &mut App, ui: &mut Ui, core: &Arc<AgentCore>, channel: u64) {
    ui.horizontal(|ui| {
        let hint = if app.eyes_reply_to.is_some() {
            "Reply as the bot…  (Enter to send)"
        } else {
            "Message as the bot…  (Enter to send)"
        };
        let response = ui.add(
            egui::TextEdit::singleline(&mut app.eyes_input)
                .hint_text(hint)
                .desired_width(ui.available_width() - 220.0),
        );
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (widgets::primary_button(ui, "Send").clicked() || enter)
            && !app.eyes_input.trim().is_empty()
        {
            send_as_bot(app, core, channel, app.eyes_input.trim().to_owned());
            app.eyes_input.clear();
            app.eyes_reply_to = None;
            response.request_focus();
        }
        if ui
            .button("⌨ typing")
            .on_hover_text("show the bot typing in this channel")
            .clicked()
        {
            type_as_bot(app, core, channel);
        }
    });
}

/// Pull the latest messages from the Discord API into the bot's archive —
/// the eyes view stays "what the bot knows", it just knows more now.
fn refresh_from_api(app: &mut App, core: &Arc<AgentCore>, channel: u64) {
    let core = Arc::clone(core);
    let channel_id = axis::prelude::ChannelId::new(channel);
    app.sup.spawn(async move {
        match core
            .http
            .get_messages(channel_id)
            .limit(50)
            .await
        {
            Ok(page) => {
                let mut fresh = 0;
                core.hive.knowledge.channel_mut(channel, |archive| {
                    for message in page.iter().rev() {
                        let stored = crate::runtime::agent::to_stored(message);
                        if archive.messages.iter().any(|m| m.id == stored.id) {
                            // keep newest version of a known message
                            if let Some(existing) =
                                archive.messages.iter_mut().find(|m| m.id == stored.id)
                            {
                                *existing = stored;
                            }
                        } else {
                            archive.messages.push_back(stored);
                            fresh += 1;
                        }
                    }
                });
                tracing::info!(agent = %core.id, channel = channel, fresh, "eyes: refreshed from API");
            }
            Err(error) => {
                tracing::warn!(
                    agent = %core.id,
                    error = %crate::tools::discord_error(error),
                    "eyes: API refresh failed"
                );
            }
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
                // The bot's own words enter its archive — the eyes view
                // stays truthful.
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

/// Remove the bot's own reaction from a message.
fn unreact_as_bot(app: &mut App, core: &Arc<AgentCore>, channel: u64, message: u64, emoji: &str) {
    let core = Arc::clone(core);
    let channel = axis::prelude::ChannelId::new(channel);
    let message = axis::prelude::MessageId::new(message);
    let emoji = emoji.to_owned();
    app.sup.spawn(async move {
        if let Err(error) = core
            .http
            .delete_own_reaction(channel, message, crate::tools::discord::parse_emoji(&emoji))
            .await
        {
            core.feed(
                crate::runtime::live::FeedKind::Error,
                format!(
                    "eyes: could not remove reaction: {}",
                    crate::tools::discord_error(error)
                ),
                None,
            );
        }
    });
}

/// Delete a message as the bot (offered only where the bot has Manage
/// Messages).
fn delete_message_as_bot(app: &mut App, core: &Arc<AgentCore>, channel: u64, message: u64) {
    let core = Arc::clone(core);
    let channel = axis::prelude::ChannelId::new(channel);
    let message = axis::prelude::MessageId::new(message);
    app.sup.spawn(async move {
        match core.http.delete_message(channel, message).await {
            Ok(()) => {
                core.hive.knowledge.channel_mut(channel.get(), |archive| {
                    if let Some(stored) = archive.get_mut(message.get()) {
                        stored.deleted = true;
                    }
                });
            }
            Err(error) => {
                core.feed(
                    crate::runtime::live::FeedKind::Error,
                    format!(
                        "eyes: could not delete: {}",
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
