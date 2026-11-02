//! One agent's page.

use egui::{RichText, TextEdit, Ui};

use super::theme::{self, ACCENT, FAINT, TEXT, WEAK};
use super::widgets::{self, avatar, card, form_row, pill, section, stat, status_color, toggle_row};
use super::{AgentTab, App};
use crate::config::{AgentProfile, Autonomy, PromptStyle, ReplyLength, ThinkingStyle, ToolMode};
use crate::llm::ProviderKind;
use crate::memory::{Memory, MemoryKind, now_ms};
use crate::runtime::live::{AgentStatus, PlaygroundRole, Probe};

pub fn show(app: &mut App, ui: &mut Ui, id: &str) {
    let profile = app.config.agents[app.agent_index(id).expect("checked")].clone();
    let live = app.sup.live.snapshot(id);
    let accent = theme::rgb(profile.accent);
    let running = app.sup.is_running(id);

    // Header.
    ui.horizontal(|ui| {
        avatar(ui, &profile.name, accent, 54.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(&profile.name).size(24.0).strong().color(TEXT));
            ui.horizontal(|ui| {
                pill(
                    ui,
                    &if live.paused {
                        "paused".to_owned()
                    } else {
                        crate::llm::truncate(&live.status.label(), 60)
                    },
                    status_color(&live.status, live.paused),
                );
                if let Some((_, username)) = &live.bot_user {
                    ui.label(RichText::new(format!("@{username}")).color(WEAK));
                }
                ui.label(RichText::new(crate::llm::truncate(&profile.specialty, 60)).color(FAINT));
            });
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if app.confirm_delete.as_deref() == Some(id) {
                if widgets::danger_button(ui, "Yes, delete").clicked() {
                    app.confirm_delete = None;
                    app.delete_agent(id);
                    return;
                }
                if ui.button("Cancel").clicked() {
                    app.confirm_delete = None;
                }
            } else if ui
                .add(egui::Button::new(RichText::new("🗑").color(WEAK)).frame(false))
                .on_hover_text("Delete agent")
                .clicked()
            {
                app.confirm_delete = Some(id.to_owned());
            }
            if running {
                if widgets::ghost_button(ui, "⏹  Stop").clicked() {
                    app.sup.stop(id);
                }
                let label = if live.paused {
                    "▶  Resume"
                } else {
                    "⏸  Pause"
                };
                if widgets::ghost_button(ui, label)
                    .on_hover_text("Paused agents stay online but don't respond")
                    .clicked()
                {
                    app.sup.set_paused(id, !live.paused);
                }
            } else if widgets::primary_button(ui, "▶  Start").clicked() {
                app.flush_now();
                let profile = app.config.agents[app.agent_index(id).expect("agent")].clone();
                app.sup.start(&profile);
            }
        });
    });
    ui.add_space(8.0);
    widgets::tab_bar(
        ui,
        &mut app.agent_tab,
        &[
            (AgentTab::Overview, "Overview"),
            (AgentTab::Discord, "Discord"),
            (AgentTab::Persona, "Persona"),
            (AgentTab::Model, "Model"),
            (AgentTab::Behavior, "Behavior"),
            (AgentTab::Tools, "Tools"),
            (AgentTab::Commands, "Commands"),
            (AgentTab::Voice, "Voice"),
            (AgentTab::Memory, "Memory"),
            (AgentTab::Tasks, "Tasks"),
            (AgentTab::Console, "Console"),
            (AgentTab::Activity, "Activity"),
        ],
    );

    let tab = app.agent_tab;
    if tab == AgentTab::Console {
        console(app, ui, &profile);
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(("agent_tab", id, tab as u8))
        .auto_shrink([false, false])
        .show(ui, |ui| match tab {
            AgentTab::Overview => overview(app, ui, &profile),
            AgentTab::Discord => app.edit_agent(id, |app, p| discord(app, ui, p)),
            AgentTab::Persona => {
                let mut apply = false;
                app.edit_agent(id, |app, p| apply = persona(app, ui, p));
                if apply {
                    app.flush(true);
                    if app.sup.apply_status(id) {
                        app.toast("Status updated on Discord", false);
                    }
                }
            }
            AgentTab::Model => app.edit_agent(id, |app, p| model(app, ui, p)),
            AgentTab::Behavior => app.edit_agent(id, |app, p| behavior(app, ui, p)),
            AgentTab::Tools => app.edit_agent(id, |app, p| tools(app, ui, p)),
            AgentTab::Commands => {
                let mut apply = false;
                app.edit_agent(id, |app, p| apply = commands(app, ui, p));
                if apply {
                    // Hand the latest settings to the agent, then register.
                    app.flush(true);
                    if app.sup.apply_commands(id) {
                        app.toast("Applying slash commands…", false);
                    }
                }
            }
            AgentTab::Voice => app.edit_agent(id, |app, p| voice(app, ui, p)),
            AgentTab::Memory => memory(app, ui, &profile),
            AgentTab::Tasks => tasks(app, ui, &profile),
            AgentTab::Activity => super::dashboard::activity(app, ui, Some(id)),
            AgentTab::Console => {}
        });
}

impl App {
    pub fn flush_now(&mut self) {
        self.flush(true);
    }
}

// --- overview -------------------------------------------------------------------

fn overview(app: &mut App, ui: &mut Ui, profile: &AgentProfile) {
    let live = app.sup.live.snapshot(&profile.id);
    let checks = [
        (
            !profile.token.trim().is_empty(),
            "Bot token added",
            AgentTab::Discord,
        ),
        (
            !profile.model.model.trim().is_empty(),
            "Model selected",
            AgentTab::Model,
        ),
        (
            matches!(live.status, AgentStatus::Online),
            "Connected to Discord",
            AgentTab::Overview,
        ),
        (
            !live.guilds.is_empty(),
            "Invited to a server",
            AgentTab::Discord,
        ),
    ];
    if checks.iter().any(|(ok, _, _)| !ok) {
        card(ui, |ui| {
            section(ui, "Setup", "A few things before this agent can work.");
            for (ok, label, tab) in checks {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if ok { "✅" } else { "⬜" }).size(15.0));
                    ui.label(RichText::new(label).color(if ok { WEAK } else { TEXT }));
                    if !ok && tab != AgentTab::Overview && ui.small_button("fix ›").clicked() {
                        app.agent_tab = tab;
                    }
                });
            }
            if let AgentStatus::Error(error) = &live.status {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("⚠ {error}")).color(theme::RED));
            }
        });
        ui.add_space(8.0);
    }

    if let Some(per) = live
        .model_info
        .as_ref()
        .and_then(|info| info.context_window.filter(|_| info.parallel.is_some()))
        .filter(|per| *per < 8192)
    {
        card(ui, |ui| {
            ui.label(
                RichText::new(format!(
                    "⚠ LM Studio gives this model only {per} tokens per request — the agent will forget the conversation almost immediately and can't fit its tools."
                ))
                .color(theme::YELLOW),
            );
            if ui.button("Fix it on the Model tab").clicked() {
                app.agent_tab = AgentTab::Model;
            }
        });
        ui.add_space(8.0);
    }
    if !live.voice_channels.is_empty() {
        card(ui, |ui| {
            ui.label(
                RichText::new(format!("🔊 In voice: {}", live.voice_channels.join(", ")))
                    .strong()
                    .color(theme::GREEN),
            );
            for (who, said) in live.voice_heard.iter().rev().take(4) {
                ui.label(RichText::new(format!("{who}: {said}")).color(WEAK));
            }
        });
        ui.add_space(8.0);
    }
    card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let uptime = live
                .started_at
                .filter(|_| live.status.is_running())
                .map(|t| {
                    let s = t.elapsed().as_secs();
                    format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
                })
                .unwrap_or_else(|| "—".to_owned());
            for (label, value) in [
                ("uptime", uptime),
                (
                    "messages seen",
                    widgets::compact_number(live.stats.messages_seen),
                ),
                ("replies", widgets::compact_number(live.stats.replies)),
                ("stayed quiet", widgets::compact_number(live.stats.skipped)),
                ("tool calls", widgets::compact_number(live.stats.tool_calls)),
                ("model calls", widgets::compact_number(live.stats.llm_calls)),
                ("tokens in", widgets::compact_number(live.stats.tokens_in)),
                ("tokens out", widgets::compact_number(live.stats.tokens_out)),
                (
                    "mod actions",
                    widgets::compact_number(live.stats.mod_actions),
                ),
                ("errors", widgets::compact_number(live.stats.errors)),
            ] {
                stat(ui, label, value);
                ui.add_space(14.0);
            }
        });
        if let Some(info) = &live.model_info {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "Model: {} · context {} · vision {} · tools {}",
                    profile.model.model,
                    info.context_window
                        .map(|c| format!("{c} tokens"))
                        .unwrap_or_else(|| "unknown".to_owned()),
                    info.vision
                        .map(|v| if v { "yes" } else { "no" })
                        .unwrap_or("?"),
                    info.tools
                        .map(|v| if v { "yes" } else { "no" })
                        .unwrap_or("?"),
                ))
                .size(12.5)
                .color(WEAK),
            );
        }
        if let Some(error) = &live.last_error {
            ui.label(
                RichText::new(format!("Last error: {error}"))
                    .size(12.5)
                    .color(theme::RED),
            );
        }
    });
    ui.add_space(8.0);

    card(ui, |ui| {
        section(
            ui,
            "💭 Live thinking",
            "What this agent is working on right now.",
        );
        if live.thoughts.is_empty() {
            ui.label(RichText::new("Idle.").color(FAINT));
        }
        for thought in &live.thoughts {
            ui.label(
                RichText::new(format!(
                    "{} · {} · {}s",
                    thought.channel,
                    thought.phase,
                    thought.started.elapsed().as_secs()
                ))
                .strong()
                .color(ACCENT),
            );
            ui.label(RichText::new(&thought.about).size(12.5).color(WEAK));
            for step in &thought.steps {
                ui.label(
                    RichText::new(format!("🔧 {step}"))
                        .size(12.5)
                        .color(theme::BLUE),
                );
            }
            if !thought.reasoning.trim().is_empty() {
                egui::CollapsingHeader::new(RichText::new("reasoning").size(12.0).color(WEAK))
                    .id_salt(("reasoning", thought.id))
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(thought.reasoning.trim())
                                .italics()
                                .size(12.5)
                                .color(WEAK),
                        );
                    });
            }
            if !thought.answer.trim().is_empty() {
                ui.label(RichText::new(thought.answer.trim()).color(TEXT));
            }
            ui.separator();
        }
    });
    ui.add_space(8.0);

    if !live.guilds.is_empty() {
        card(ui, |ui| {
            section(ui, "Servers", "");
            for (guild_id, name) in &live.guilds {
                let (hq, tasks) = app.sup.hive.knowledge.guild(*guild_id, |k| {
                    (
                        k.hq.channel_id,
                        k.tasks.iter().filter(|t| t.agent_id == profile.id).count(),
                    )
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name).strong().color(TEXT));
                    if hq.is_some() {
                        pill(ui, "HQ ready", theme::GREEN);
                    } else {
                        pill(ui, "no HQ yet", FAINT);
                    }
                    if tasks > 0 {
                        pill(ui, &format!("{tasks} task(s)"), ACCENT);
                    }
                });
            }
        });
    }
}

// --- discord ---------------------------------------------------------------------

/// The bot's application ID is encoded in the token's first segment.
pub fn application_id_from_token(token: &str) -> Option<u64> {
    let first = token.trim().trim_start_matches("Bot ").split('.').next()?;
    let mut bits: u32 = 0;
    let mut count = 0;
    let mut out = Vec::new();
    for c in first.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            '=' => break,
            _ => return None,
        };
        bits = (bits << 6) | value;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    String::from_utf8(out).ok()?.parse().ok()
}

fn recommended_permissions() -> u64 {
    use axis::prelude::Permissions as P;
    (P::VIEW_CHANNEL
        | P::SEND_MESSAGES
        | P::SEND_MESSAGES_IN_THREADS
        | P::CREATE_PUBLIC_THREADS
        | P::CREATE_PRIVATE_THREADS
        | P::MANAGE_THREADS
        | P::EMBED_LINKS
        | P::ATTACH_FILES
        | P::READ_MESSAGE_HISTORY
        | P::ADD_REACTIONS
        | P::USE_EXTERNAL_EMOJIS
        | P::MANAGE_MESSAGES
        | P::PIN_MESSAGES
        | P::MODERATE_MEMBERS
        | P::KICK_MEMBERS
        | P::BAN_MEMBERS
        | P::MANAGE_ROLES
        | P::MANAGE_CHANNELS
        | P::MOVE_MEMBERS
        | P::MUTE_MEMBERS
        | P::DEAFEN_MEMBERS
        | P::MANAGE_NICKNAMES
        | P::CHANGE_NICKNAME
        | P::VIEW_AUDIT_LOG
        | P::SEND_POLLS
        | P::CONNECT)
        .bits()
}

fn discord(app: &mut App, ui: &mut Ui, p: &mut AgentProfile) {
    card(ui, |ui| {
        section(
            ui,
            "Bot token",
            "Each agent is its own Discord bot. The token is saved in your data folder so agents can run 24/7.",
        );
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut p.token)
                    .password(!app.show_token)
                    .hint_text("paste the bot token")
                    .desired_width(460.0),
            );
            if ui
                .button(if app.show_token {
                    "🙈 Hide"
                } else {
                    "👁 Show"
                })
                .clicked()
            {
                app.show_token = !app.show_token;
            }
        });
        toggle_row(
            ui,
            &mut p.autostart,
            "Start automatically",
            "Connect this agent when the app opens.",
        );
        ui.add_space(4.0);
        egui::CollapsingHeader::new("How to get a token")
            .default_open(p.token.is_empty())
            .show(ui, |ui| {
                for line in [
                    "1. Open discord.com/developers/applications › New Application (name it after the agent).",
                    "2. Bot › Reset Token › copy it here.",
                    "3. Bot › Privileged Gateway Intents: turn on SERVER MEMBERS and MESSAGE CONTENT.",
                    "4. Optional: set the bot's avatar there too — it's what people will see.",
                ] {
                    ui.label(RichText::new(line).color(WEAK));
                }
                if ui.link("Open the Developer Portal").clicked() {
                    widgets::open_external("https://discord.com/developers/applications");
                }
            });
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(
            ui,
            "Invite to a server",
            "Open the link while signed in as a server admin.",
        );
        let live = app.sup.live.snapshot(&p.id);
        let app_id = live
            .application_id
            .or_else(|| application_id_from_token(&p.token));
        match app_id {
            Some(app_id) => {
                let key = format!("{}:admin_invite", p.id);
                let mut admin = app.buffers.get(&key).map(|v| v == "1").unwrap_or(false);
                if toggle_row(
                    ui,
                    &mut admin,
                    "Administrator permission",
                    "Simplest; otherwise a recommended set of moderation and community permissions is requested.",
                ) {
                    app.buffers
                        .insert(key, if admin { "1" } else { "0" }.to_owned());
                }
                let permissions = if admin { 8 } else { recommended_permissions() };
                let url = format!(
                    "https://discord.com/oauth2/authorize?client_id={app_id}&scope=bot%20applications.commands&permissions={permissions}"
                );
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "Open invite link").clicked() {
                        widgets::open_external(&url);
                    }
                    if ui.button("📋 Copy").clicked() {
                        ui.ctx().copy_text(url.clone());
                        app.toast("Invite link copied", false);
                    }
                });
                ui.label(RichText::new("Tip: put the agent's role near the top of the role list so it can moderate below it.").size(12.0).color(FAINT));
            }
            None => {
                ui.label(RichText::new("Paste a valid token first.").color(FAINT));
            }
        }
    });
}

// --- persona ---------------------------------------------------------------------

const TEMPLATES: &[(&str, &str, &str)] = &[
    (
        "Friendly regular",
        "general help and conversation",
        "You are warm, witty, and genuinely helpful. You talk like a long-time member of the server: casual, a bit playful, never preachy. You remember people and bring up things they told you.",
    ),
    (
        "Moderator",
        "keeping the server safe and the rules fair",
        "You are a calm, fair moderator. You de-escalate first, explain rules kindly, and only act when it's clearly warranted. You keep notes on repeat issues and coordinate with the other agents in HQ. You never lecture people who are just joking around.",
    ),
    (
        "Hype & games",
        "fun, games, events, and keeping chat lively",
        "You are energetic and funny. You run little games, polls, and trivia, celebrate people's wins, and keep chat lively without being annoying. You read the room and tone it down when things are serious.",
    ),
    (
        "Tech expert",
        "programming, tech support, and research",
        "You are a sharp, patient engineer. You give precise, practical answers with code when useful, ask one clarifying question when needed, and search the web for anything version-specific or recent.",
    ),
    (
        "Concierge",
        "welcoming newcomers and answering questions about the server",
        "You are the server's welcoming host. You greet newcomers, explain where things are and how the server works, point people to the right channels, and make everyone feel included.",
    ),
];

fn persona(app: &mut App, ui: &mut Ui, p: &mut AgentProfile) -> bool {
    let mut apply = false;
    let shown = app.sup.shown_status(&p.id);
    card(ui, |ui| {
        section(
            ui,
            "Identity",
            "How the agent sees itself and how others find it.",
        );
        form_row(
            ui,
            "Name",
            "Also a trigger word: saying it in chat summons the agent.",
            |ui| {
                ui.add(TextEdit::singleline(&mut p.name).desired_width(260.0));
            },
        );
        form_row(
            ui,
            "Nicknames",
            "Other names that summon it, comma-separated.",
            |ui| {
                let mut aliases = p.aliases.join(", ");
                if ui
                    .add(
                        TextEdit::singleline(&mut aliases)
                            .hint_text("e.g. nov, novabot")
                            .desired_width(360.0),
                    )
                    .changed()
                {
                    p.aliases = aliases
                        .split(',')
                        .map(|a| a.trim().to_owned())
                        .filter(|a| !a.is_empty())
                        .collect();
                }
            },
        );
        form_row(
            ui,
            "Specialty",
            "One line other agents see, to hand off requests.",
            |ui| {
                ui.add(TextEdit::singleline(&mut p.specialty).desired_width(420.0));
            },
        );
        form_row(
            ui,
            "Status line",
            "Shown under the bot's name (empty = specialty). Discord hides it while the bot is offline.",
            |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut p.status_text)
                            .hint_text("e.g. ✨ ask me anything")
                            .desired_width(300.0),
                    );
                    match &shown {
                        Some(shown) => {
                            let pending = *shown != crate::runtime::agent::status_line(p);
                            if ui
                                .add_enabled(pending, egui::Button::new("Apply"))
                                .on_hover_text("Show it on Discord now")
                                .clicked()
                            {
                                apply = true;
                            }
                            if pending {
                                ui.label(RichText::new("● not applied").color(egui::Color32::from_rgb(230, 160, 60)));
                            }
                        }
                        None => {
                            ui.label(RichText::new("applies when the agent starts").color(WEAK));
                        }
                    }
                });
            },
        );
        toggle_row(
            ui,
            &mut p.status_in_profile,
            "Also put it in the bot's About Me",
            "The profile's About Me stays visible while the bot is offline (updated when you press Apply).",
        );
        form_row(ui, "Color", "", |ui| {
            ui.color_edit_button_srgb(&mut p.accent);
        });
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        ui.horizontal(|ui| {
            section(ui, "Personality & instructions", "The agent's voice, values, and standing orders. Discord permissions are always enforced on top.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("templates")
                    .selected_text("Templates…")
                    .show_ui(ui, |ui| {
                        for (name, specialty, persona) in TEMPLATES {
                            if ui.selectable_label(false, *name).clicked() {
                                p.persona = persona.to_string();
                                p.specialty = specialty.to_string();
                            }
                        }
                    });
            });
        });
        ui.add(
            TextEdit::multiline(&mut p.persona)
                .desired_rows(12)
                .desired_width(f32::INFINITY)
                .hint_text("Who is this agent? How does it talk? What does it care about?"),
        );
    });
    apply
}

fn model(app: &mut App, ui: &mut Ui, p: &mut AgentProfile) {
    card(ui, |ui| {
        section(
            ui,
            "Provider",
            "Any OpenAI-compatible server works, plus Anthropic natively.",
        );
        form_row(ui, "Provider", "", |ui| {
            let before = p.model.provider;
            egui::ComboBox::from_id_salt("provider")
                .selected_text(p.model.provider.label())
                .width(280.0)
                .show_ui(ui, |ui| {
                    for kind in ProviderKind::ALL {
                        ui.selectable_value(&mut p.model.provider, kind, kind.label());
                    }
                });
            if p.model.provider != before {
                p.model.base_url = p.model.provider.default_base_url().to_owned();
                p.model.model = p.model.provider.default_model().to_owned();
                p.model.context_window = 0;
            }
        });
        form_row(ui, "Base URL", "", |ui| {
            ui.add(TextEdit::singleline(&mut p.model.base_url).desired_width(420.0));
        });
        if p.model.provider.needs_key()
            || !p.model.api_key.is_empty()
            || p.model.provider == ProviderKind::Custom
        {
            form_row(ui, "API key", "Stored in your data folder.", |ui| {
                ui.add(
                    TextEdit::singleline(&mut p.model.api_key)
                        .password(true)
                        .desired_width(420.0),
                );
            });
        }
        let models_key = format!("{}#models", p.id);
        let probe = app
            .sup
            .live
            .probes
            .lock()
            .expect("live poisoned")
            .get(&models_key)
            .cloned();
        form_row(ui, "Model", "", |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut p.model.model)
                        .hint_text("model id")
                        .desired_width(300.0),
                );
                if ui.button("🔄 Fetch list").clicked() {
                    app.sup.fetch_models(&p.id, &p.model);
                }
            });
            match &probe {
                Some(Probe::Models(models)) => {
                    egui::ComboBox::from_id_salt("model_pick")
                        .selected_text(format!("{} models available — pick one", models.len()))
                        .width(420.0)
                        .show_ui(ui, |ui| {
                            for model in models {
                                if ui
                                    .selectable_label(p.model.model == *model, model)
                                    .clicked()
                                {
                                    p.model.model = model.clone();
                                    p.model.context_window = 0;
                                }
                            }
                        });
                }
                Some(Probe::Pending) => {
                    ui.label(RichText::new("fetching…").color(WEAK));
                }
                Some(Probe::Failed(error)) => {
                    ui.label(RichText::new(error).color(theme::RED));
                }
                _ => {}
            }
        });
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(
            ui,
            "Capabilities",
            "Detected from the provider where possible. Context management uses the context window to fit history, memories, and tools.",
        );
        let info_key = format!("{}#info", p.id);
        let info = app
            .sup
            .live
            .probes
            .lock()
            .expect("live poisoned")
            .get(&info_key)
            .cloned();
        form_row(ui, "Context window", "0 = detect automatically.", |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut p.model.context_window)
                        .range(0..=2_000_000)
                        .speed(256)
                        .suffix(" tokens"),
                );
                if ui.button("🔍 Detect").clicked() {
                    app.sup.detect_model(&p.id, &p.model);
                }
            });
            match info {
                Some(Probe::Info(info)) => {
                    ui.label(
                        RichText::new(format!(
                            "detected: context {} · vision {} · tools {}",
                            info.context_window
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "unknown (8192 assumed)".to_owned()),
                            info.vision
                                .map(|v| if v { "yes" } else { "no" })
                                .unwrap_or("?"),
                            info.tools
                                .map(|v| if v { "yes" } else { "no" })
                                .unwrap_or("?"),
                        ))
                        .size(12.5)
                        .color(WEAK),
                    );
                    if let Some(true) = info.vision {
                        if !p.model.vision && ui.small_button("enable vision").clicked() {
                            p.model.vision = true;
                        }
                    }
                }
                Some(Probe::Pending) => {
                    ui.label(RichText::new("detecting…").color(WEAK));
                }
                _ => {}
            }
        });
        if p.model.provider == ProviderKind::LmStudio {
            lmstudio_room(app, ui, p);
        }
        form_row(ui, "Max reply tokens", "", |ui| {
            ui.add(
                egui::DragValue::new(&mut p.model.max_output_tokens)
                    .range(128..=32_000)
                    .speed(16),
            );
        });
        if p.model.provider == ProviderKind::LmStudio {
            form_row(
                ui,
                "Thinking budget",
                "Most tokens a thinking model may think before answering; the answer still gets its full max reply tokens. 0 = automatic (same as max reply tokens).",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut p.model.thinking_budget)
                            .range(0..=32_000)
                            .speed(16)
                            .custom_formatter(|v, _| if v == 0.0 { "auto".to_owned() } else { format!("{v:.0}") }),
                    );
                },
            );
        }
        form_row(
            ui,
            "Temperature",
            "Lower = focused, higher = creative.",
            |ui| {
                ui.add(egui::Slider::new(&mut p.model.temperature, 0.0..=1.6));
            },
        );
        form_row(
            ui,
            "Thinking",
            "For reasoning models (Qwen 3.5, DeepSeek R1…), per kind of work. Off is several times faster; Low/Medium can run out of room on small models. If a model thinks until it runs out of room, the agent retries with thinking off.",
            |ui| {
                egui::Grid::new("thinking_modes").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    let t = &mut p.model.thinking;
                    for (label, id, mode) in [
                        ("replies", "think_replies", &mut t.replies),
                        ("chiming in", "think_ambient", &mut t.chiming_in),
                        ("standing tasks", "think_tasks", &mut t.tasks),
                        ("looking at images", "think_vision", &mut t.vision),
                        ("background work", "think_background", &mut t.background),
                        ("voice chat", "think_voice", &mut t.voice),
                    ] {
                        ui.label(RichText::new(label).color(WEAK));
                        egui::ComboBox::from_id_salt(id)
                            .selected_text(mode.label())
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                for option in crate::config::ReasoningMode::ALL {
                                    ui.selectable_value(mode, option, option.label());
                                }
                            });
                        ui.end_row();
                    }
                });
            },
        );
        form_row(
            ui,
            "Tool calling",
            "Small local models often do better with the text protocol.",
            |ui| {
                egui::ComboBox::from_id_salt("tool_mode")
                    .selected_text(p.model.tool_mode.label())
                    .width(300.0)
                    .show_ui(ui, |ui| {
                        for mode in ToolMode::ALL {
                            ui.selectable_value(&mut p.model.tool_mode, mode, mode.label());
                        }
                    });
            },
        );
        form_row(
            ui,
            "Prompt size",
            "Compact prompts and fewer tools for small models.",
            |ui| {
                egui::ComboBox::from_id_salt("prompt_style")
                    .selected_text(p.model.prompt_style.label())
                    .width(300.0)
                    .show_ui(ui, |ui| {
                        for style in PromptStyle::ALL {
                            ui.selectable_value(&mut p.model.prompt_style, style, style.label());
                        }
                    });
            },
        );
        toggle_row(
            ui,
            &mut p.model.streaming,
            "Stream responses",
            "Needed for the live thinking message.",
        );
        toggle_row(
            ui,
            &mut p.model.vision,
            "Send images",
            "Only for multimodal models; turns itself off if the model refuses.",
        );
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(ui, "Advanced", "");
        form_row(
            ui,
            "Background model",
            "Cheaper model for summaries, culture notes, and moderation reviews (empty = same model).",
            |ui| {
                ui.add(TextEdit::singleline(&mut p.model.background_model).desired_width(320.0));
            },
        );
        let vision_key = format!("{}#models", p.id);
        let fetched = app
            .sup
            .live
            .probes
            .lock()
            .expect("live poisoned")
            .get(&vision_key)
            .cloned();
        form_row(
            ui,
            "Vision model",
            "Optional small vision model on the same server (e.g. a Qwen2.5-VL 3B or Gemma 3 4B in LM Studio). It looks at images and describes them, so the main model doesn't need vision.",
            |ui| {
                ui.add(
                    TextEdit::singleline(&mut p.model.vision_model)
                        .hint_text("empty = main model sees images itself (if Send images is on)")
                        .desired_width(320.0),
                );
                if let Some(Probe::Models(models)) = &fetched {
                    let vision: Vec<&String> = models
                        .iter()
                        .filter(|m| {
                            let m = m.to_lowercase();
                            [
                                "vl",
                                "vision",
                                "llava",
                                "gemma-3",
                                "moondream",
                                "smolvlm",
                                "pixtral",
                                "minicpm-v",
                            ]
                            .iter()
                            .any(|k| m.contains(k))
                        })
                        .collect();
                    if !vision.is_empty() {
                        egui::ComboBox::from_id_salt("vision_pick")
                            .selected_text("pick a vision model…")
                            .width(320.0)
                            .show_ui(ui, |ui| {
                                for model in vision {
                                    if ui
                                        .selectable_label(p.model.vision_model == *model, model)
                                        .clicked()
                                    {
                                        p.model.vision_model = model.clone();
                                    }
                                }
                            });
                    }
                }
            },
        );
        form_row(
            ui,
            "Embedding model",
            "Optional: semantic memory search (e.g. text-embedding-nomic-embed-text-v1.5 in LM Studio).",
            |ui| {
                ui.add(TextEdit::singleline(&mut p.model.embedding_model).desired_width(320.0));
            },
        );
        form_row(ui, "Request timeout", "", |ui| {
            ui.add(
                egui::DragValue::new(&mut p.model.request_timeout_secs)
                    .range(10..=900)
                    .suffix(" s"),
            );
        });
    });
}

// --- voice -----------------------------------------------------------------------

use crate::runtime::speech::{KOKORO_VOICES, PIPER_VOICES};

const WHISPER_MODELS: [(&str, &str); 8] = [
    ("tiny.en", "tiny.en — fastest, English"),
    ("base.en", "base.en — fast, English"),
    ("small.en", "small.en — accurate, English"),
    ("tiny", "tiny — fastest, any language"),
    ("base", "base — fast, any language"),
    ("small", "small — accurate, any language"),
    ("medium", "medium — slow, very accurate"),
    ("large-v3", "large-v3 — needs a strong GPU"),
];

fn probe_line(ui: &mut Ui, probe: Option<Probe>) {
    match probe {
        Some(Probe::Pending) => {
            ui.label(RichText::new("working…").color(WEAK));
        }
        Some(Probe::Text(text)) => {
            ui.label(RichText::new(text).color(theme::GREEN));
        }
        Some(Probe::Failed(error)) => {
            ui.label(RichText::new(error).color(theme::RED));
        }
        _ => {}
    }
}

fn voice(app: &mut App, ui: &mut Ui, p: &mut AgentProfile) {
    let live = app.sup.live.snapshot(&p.id);
    let probe = |app: &App, key: &str| app.sup.live.probes.lock().expect("live poisoned").get(key).cloned();
    let check_key = format!("{}#voice_check", p.id);
    let install_key = format!("{}#voice_install", p.id);
    let preview_key = format!("{}#voice_preview", p.id);
    let voices_key = format!("{}#voice_list", p.id);

    card(ui, |ui| {
        section(
            ui,
            "Voice channels",
            "Ask the agent to join a voice channel — \"join the gaming vc\" — and it hops in, listens, and talks back. It leaves when asked (or when everyone's gone).",
        );
        if live.voice_channels.is_empty() {
            ui.label(RichText::new("Not in a voice channel right now.").color(WEAK));
        } else {
            ui.label(
                RichText::new(format!("🔊 In {}", live.voice_channels.join(", ")))
                    .strong()
                    .color(theme::GREEN),
            );
            for (who, said) in live.voice_heard.iter().rev().take(6) {
                ui.label(RichText::new(format!("{who}: {said}")).color(WEAK));
            }
        }
        ui.add_space(6.0);
        let v = &mut p.voice;
        toggle_row(ui, &mut v.listen, "Listen and answer", "Hear what people say in the call and reply out loud.");
        form_row(ui, "Answers when", "", |ui| {
            egui::ComboBox::from_id_salt("voice_reply")
                .selected_text(v.reply.label())
                .width(300.0)
                .show_ui(ui, |ui| {
                    for option in crate::config::VoiceReply::ALL {
                        ui.selectable_value(&mut v.reply, option, option.label());
                    }
                });
        });
        form_row(ui, "Wait after a pause", "How long someone must be quiet before the agent treats it as the end of what they said. Raise it if it cuts you off.", |ui| {
            ui.add(egui::Slider::new(&mut v.pause_ms, 600..=3000).step_by(100.0).suffix(" ms"));
        });
        toggle_row(ui, &mut v.interruptible, "Can be interrupted", "Stop talking when someone talks over it.");
        toggle_row(
            ui,
            &mut v.live_transcript,
            "Live transcript in the call's chat",
            "Posts what it hears and says as small, silent lines (-#), so people reading along can follow.",
        );
        toggle_row(ui, &mut v.post_transcript, "Also post replies in the call's text chat", "Full-size messages, in addition to speaking.");
        toggle_row(ui, &mut v.leave_when_alone, "Leave when nobody's left", "After 30 seconds alone in the channel.");
    });
    ui.add_space(8.0);

    card(ui, |ui| {
        section(ui, "Hearing", "Speech-to-text turns what people say into text for the model.");
        let stt = &mut p.voice.stt;
        form_row(ui, "Engine", "", |ui| {
            egui::ComboBox::from_id_salt("stt_engine")
                .selected_text(stt.engine.label())
                .width(300.0)
                .show_ui(ui, |ui| {
                    for option in crate::config::SttEngine::ALL {
                        ui.selectable_value(&mut stt.engine, option, option.label());
                    }
                });
        });
        match stt.engine {
            crate::config::SttEngine::LocalWhisper => {
                form_row(ui, "Model", "Downloaded on first use. Runs on your GPU when it can, otherwise the CPU.", |ui| {
                    let current = WHISPER_MODELS
                        .iter()
                        .find(|(id, _)| *id == stt.whisper_model)
                        .map(|(_, label)| (*label).to_owned())
                        .unwrap_or_else(|| stt.whisper_model.clone());
                    egui::ComboBox::from_id_salt("whisper_model")
                        .selected_text(current)
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            for (id, label) in WHISPER_MODELS {
                                ui.selectable_value(&mut stt.whisper_model, id.to_owned(), label);
                            }
                        });
                });
                form_row(ui, "Language", "Code like en or de; empty detects it.", |ui| {
                    ui.add(TextEdit::singleline(&mut stt.language).desired_width(80.0));
                });
            }
            crate::config::SttEngine::OpenAi => {
                form_row(ui, "Base URL", "OpenAI, Groq (https://api.groq.com/openai/v1), or a local Whisper server.", |ui| {
                    ui.add(TextEdit::singleline(&mut stt.base_url).desired_width(320.0));
                });
                form_row(ui, "API key", "", |ui| {
                    ui.add(TextEdit::singleline(&mut stt.api_key).password(true).desired_width(320.0));
                });
                form_row(ui, "Model", "", |ui| {
                    ui.add(TextEdit::singleline(&mut stt.model).desired_width(200.0));
                });
            }
        }
    });
    ui.add_space(8.0);

    card(ui, |ui| {
        section(ui, "Speaking", "Text-to-speech gives the agent its voice.");
        let tts = &mut p.voice.tts;
        form_row(ui, "Engine", "", |ui| {
            egui::ComboBox::from_id_salt("tts_engine")
                .selected_text(tts.engine.label())
                .width(300.0)
                .show_ui(ui, |ui| {
                    for option in crate::config::TtsEngine::ALL {
                        ui.selectable_value(&mut tts.engine, option, option.label());
                    }
                });
        });
        match tts.engine {
            crate::config::TtsEngine::Kokoro => {
                form_row(ui, "Voice", "The model (~120 MB) downloads the first time it speaks.", |ui| {
                    let current = KOKORO_VOICES
                        .iter()
                        .find(|(id, _)| *id == tts.kokoro_voice)
                        .map(|(_, label)| (*label).to_owned())
                        .unwrap_or_else(|| tts.kokoro_voice.clone());
                    egui::ComboBox::from_id_salt("kokoro_voice")
                        .selected_text(current)
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            for (id, label) in KOKORO_VOICES {
                                ui.selectable_value(&mut tts.kokoro_voice, id.to_owned(), label);
                            }
                        });
                });
            }
            crate::config::TtsEngine::Piper => {
                form_row(ui, "Voice", "Each voice is a ~60 MB download the first time it's used. Any Piper voice name works.", |ui| {
                    let current = PIPER_VOICES
                        .iter()
                        .find(|(id, _)| *id == tts.piper_voice)
                        .map(|(_, label)| (*label).to_owned())
                        .unwrap_or_else(|| tts.piper_voice.clone());
                    ui.vertical(|ui| {
                        egui::ComboBox::from_id_salt("piper_voice")
                            .selected_text(current)
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for (id, label) in PIPER_VOICES {
                                    ui.selectable_value(&mut tts.piper_voice, id.to_owned(), label);
                                }
                            });
                        ui.add(TextEdit::singleline(&mut tts.piper_voice).desired_width(300.0));
                    });
                });
            }
            crate::config::TtsEngine::Windows => {
                let voices = match probe(app, &voices_key) {
                    Some(Probe::Models(list)) => list,
                    _ => Vec::new(),
                };
                form_row(ui, "Voice", "", |ui| {
                    ui.horizontal(|ui| {
                        let shown = if tts.windows_voice.is_empty() { "System default".to_owned() } else { tts.windows_voice.clone() };
                        egui::ComboBox::from_id_salt("windows_voice")
                            .selected_text(shown)
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut tts.windows_voice, String::new(), "System default");
                                for name in &voices {
                                    ui.selectable_value(&mut tts.windows_voice, name.clone(), name);
                                }
                            });
                        if voices.is_empty() && ui.button("Load voices").clicked() {
                            app.sup.load_windows_voices(voices_key.clone());
                        }
                    });
                });
            }
            crate::config::TtsEngine::OpenAi => {
                form_row(ui, "Base URL", "OpenAI, or a local server like Kokoro-FastAPI (http://localhost:8880/v1).", |ui| {
                    ui.add(TextEdit::singleline(&mut tts.base_url).desired_width(320.0));
                });
                form_row(ui, "API key", "", |ui| {
                    ui.add(TextEdit::singleline(&mut tts.api_key).password(true).desired_width(320.0));
                });
                form_row(ui, "Model", "", |ui| {
                    ui.add(TextEdit::singleline(&mut tts.model).desired_width(200.0));
                });
                form_row(ui, "Voice", "", |ui| {
                    ui.add(TextEdit::singleline(&mut tts.voice).desired_width(200.0));
                });
            }
        }
        form_row(ui, "Speed", "", |ui| {
            ui.add(egui::Slider::new(&mut tts.speed, 0.5..=2.0).step_by(0.05));
        });
        form_row(ui, "Volume", "", |ui| {
            ui.add(egui::Slider::new(&mut tts.volume, 0.1..=2.0).step_by(0.05));
        });
        let text_key = format!("{}:voice_preview_text", p.id);
        let mut text = app
            .buffers
            .get(&text_key)
            .cloned()
            .unwrap_or_else(|| format!("Hey, I'm {}. Want me to hop in the call?", p.name));
        form_row(ui, "Try it", "Plays on this PC's speakers.", |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    if ui.add(TextEdit::singleline(&mut text).desired_width(280.0)).changed() {
                        app.buffers.insert(text_key.clone(), text.clone());
                    }
                    if ui.button("▶ Preview").clicked() {
                        let config = p.voice.clone();
                        let text = text.clone();
                        app.sup.voice_task(preview_key.clone(), async move {
                            let started = std::time::Instant::now();
                            crate::runtime::speech::preview(&config, &text).await?;
                            Ok(format!("played ({:.1}s)", started.elapsed().as_secs_f32()))
                        });
                    }
                });
                probe_line(ui, probe(app, &preview_key));
            });
        });
    });
    ui.add_space(8.0);

    let local = p.voice.stt.engine == crate::config::SttEngine::LocalWhisper
        || matches!(p.voice.tts.engine, crate::config::TtsEngine::Piper | crate::config::TtsEngine::Kokoro);
    if local {
        card(ui, |ui| {
            section(
                ui,
                "Local engines",
                "Whisper (hearing), Kokoro and Piper (speaking) run in Python on this PC: faster-whisper, kokoro-onnx and piper-tts.",
            );
            form_row(ui, "Python", "The python command or a full path to python.exe.", |ui| {
                ui.add(TextEdit::singleline(&mut p.voice.python).desired_width(260.0));
            });
            ui.horizontal(|ui| {
                if ui.button("Check").clicked() {
                    let python = p.voice.python.clone();
                    app.sup.voice_task(check_key.clone(), async move {
                        let status = crate::runtime::speech::check_local(&python).await;
                        if let Some(error) = status.error {
                            return Err(error);
                        }
                        let mark = |ok: bool| if ok { "✔" } else { "✖ missing" };
                        Ok(format!(
                            "Python {} · faster-whisper {} · kokoro-onnx {} · piper-tts {}",
                            status.python.unwrap_or_default(),
                            mark(status.whisper),
                            mark(status.kokoro),
                            mark(status.piper)
                        ))
                    });
                }
                if ui
                    .button("Install / update")
                    .on_hover_text("Runs: python -m pip install --upgrade faster-whisper kokoro-onnx piper-tts")
                    .clicked()
                {
                    let python = p.voice.python.clone();
                    app.sup.voice_task(install_key.clone(), async move {
                        crate::runtime::speech::install_local(&python).await
                    });
                }
            });
            probe_line(ui, probe(app, &check_key));
            probe_line(ui, probe(app, &install_key));
        });
    }
}

/// How much room LM Studio gives this model per request, with a button
/// to reload it with more.
fn lmstudio_room(app: &mut App, ui: &mut Ui, p: &AgentProfile) {
    let info = app.sup.live.snapshot(&p.id).model_info;
    let reload_key = format!("{}#reload", p.id);
    let reload = app.sup.live.probes.lock().expect("live poisoned").get(&reload_key).cloned();
    form_row(
        ui,
        "LM Studio room",
        "Each request gets the loaded context divided by the parallel slots. Under ~16k the agent forgets quickly and sees fewer tools.",
        |ui| {
            if let Some(info) = &info {
                if let (Some(loaded), Some(parallel)) = (info.loaded_context, info.parallel) {
                    let per = loaded / parallel.max(1);
                    ui.label(
                        RichText::new(format!("loaded with {loaded} tokens ÷ {parallel} parallel = {per} per request"))
                            .color(if per < 16_384 { theme::YELLOW } else { WEAK }),
                    );
                }
            }
            let ctx_key = format!("{}:lms_ctx", p.id);
            let par_key = format!("{}:lms_par", p.id);
            let mut context: u32 = app.buffers.get(&ctx_key).and_then(|v| v.parse().ok()).unwrap_or(32_768);
            let mut parallel: u32 = app.buffers.get(&par_key).and_then(|v| v.parse().ok()).unwrap_or(2);
            ui.horizontal(|ui| {
                ui.label("context");
                if ui.add(egui::DragValue::new(&mut context).range(4096..=262_144).speed(1024)).changed() {
                    app.buffers.insert(ctx_key.clone(), context.to_string());
                }
                ui.label("parallel");
                if ui.add(egui::DragValue::new(&mut parallel).range(1..=8)).changed() {
                    app.buffers.insert(par_key.clone(), parallel.to_string());
                }
                if ui.button("⟳ Reload in LM Studio").on_hover_text("Unloads and reloads the model with these settings (uses more VRAM for more context)").clicked() {
                    app.sup.reload_lmstudio(&p.id, &p.model, context, parallel);
                }
            });
            match reload {
                Some(Probe::Pending) => {
                    ui.label(RichText::new("reloading…").color(WEAK));
                }
                Some(Probe::Failed(error)) => {
                    ui.label(RichText::new(error).color(theme::RED));
                }
                Some(Probe::Info(info)) => {
                    ui.label(
                        RichText::new(format!(
                            "reloaded — {} tokens per request",
                            info.context_window.map(|c| c.to_string()).unwrap_or_else(|| "?".to_owned())
                        ))
                        .color(theme::GREEN),
                    );
                }
                _ => {}
            }
        },
    );
}

// --- behavior --------------------------------------------------------------------

fn id_list(app: &mut App, ui: &mut Ui, key: String, values: &mut Vec<u64>, hint: &str) {
    let text = app.buffers.entry(key).or_insert_with(|| {
        values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    });
    if ui
        .add(
            TextEdit::singleline(text)
                .hint_text(hint)
                .desired_width(420.0),
        )
        .changed()
    {
        *values = text
            .split([',', ' ', '\n'])
            .filter_map(|part| {
                crate::runtime::world::parse_snowflake(part).or_else(|| part.trim().parse().ok())
            })
            .collect();
    }
}

fn behavior(app: &mut App, ui: &mut Ui, p: &mut AgentProfile) {
    let b = &mut p.behavior;
    card(ui, |ui| {
        section(
            ui,
            "When to speak",
            "Agents answer when called and otherwise read the room.",
        );
        toggle_row(
            ui,
            &mut b.respond_to_mentions,
            "@mentions",
            "Always answer when @mentioned.",
        );
        toggle_row(
            ui,
            &mut b.respond_to_name,
            "Its name in any message",
            "\"hey nova…\", \"what does Nova think\" — reads the surrounding context and answers.",
        );
        toggle_row(ui, &mut b.respond_to_replies, "Replies to its messages", "");
        toggle_row(ui, &mut b.respond_in_dms, "Direct messages", "");
        form_row(
            ui,
            "Chattiness",
            "How often it joins conversations nobody asked it into.",
            |ui| {
                ui.add(egui::Slider::new(&mut b.chattiness, 0..=100).suffix("%"));
            },
        );
        form_row(
            ui,
            "Conversation window",
            "After talking with someone, keep listening to them without a new mention.",
            |ui| {
                ui.add(
                    egui::DragValue::new(&mut b.conversation_window_secs)
                        .range(0..=3600)
                        .suffix(" s"),
                );
            },
        );
        form_row(
            ui,
            "Wait for follow-ups",
            "Rapid messages are answered together.",
            |ui| {
                ui.add(
                    egui::DragValue::new(&mut b.debounce_ms)
                        .range(0..=8000)
                        .speed(50)
                        .suffix(" ms"),
                );
            },
        );
        toggle_row(
            ui,
            &mut b.reply_to_other_bots,
            "Answer other bots",
            "Bots outside this app (agents always handle each other, with a loop guard).",
        );
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(ui, "How it replies", "");
        form_row(
            ui,
            "While thinking",
            "Discord's typing indicator fades after ~10s; a thinking message doesn't.",
            |ui| {
                egui::ComboBox::from_id_salt("thinking")
                    .selected_text(b.thinking_style.label())
                    .width(360.0)
                    .show_ui(ui, |ui| {
                        for style in ThinkingStyle::ALL {
                            ui.selectable_value(&mut b.thinking_style, style, style.label());
                        }
                    });
            },
        );
        toggle_row(
            ui,
            &mut b.paginate,
            "Page long answers",
            "Long replies get ◀ ▶ buttons instead of several messages (people can still ask for it all at once).",
        );
        toggle_row(
            ui,
            &mut b.small_thinking,
            "Small thinking text",
            "Show the thinking message in Discord's small subtext (-#) instead of normal size.",
        );
        toggle_row(
            ui,
            &mut b.show_footer,
            "\"thought for 3.2s\" footer",
            "A small line under replies.",
        );
        form_row(
            ui,
            "Working reaction",
            "React with this while working (empty = none).",
            |ui| {
                ui.add(
                    TextEdit::singleline(&mut b.working_reaction)
                        .hint_text("e.g. 👀")
                        .desired_width(80.0),
                );
            },
        );
        form_row(
            ui,
            "Thinking emojis",
            "Paste custom emojis as <a:name:id> (from the bot's own emoji list in the Developer Portal; they only work for this bot).",
            |ui| {
                egui::Grid::new("thinking_emojis")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        for (label, value) in [
                            ("thinking", &mut b.emojis.thinking),
                            ("working (tools)", &mut b.emojis.working),
                            ("reading / recalling", &mut b.emojis.reading),
                            ("footer", &mut b.emojis.thought),
                        ] {
                            ui.label(RichText::new(label).color(WEAK));
                            ui.add(TextEdit::singleline(value).desired_width(300.0));
                            ui.end_row();
                        }
                    });
            },
        );
        form_row(ui, "Reply length", "", |ui| {
            egui::ComboBox::from_id_salt("length")
                .selected_text(b.reply_length.label())
                .show_ui(ui, |ui| {
                    for length in ReplyLength::ALL {
                        ui.selectable_value(&mut b.reply_length, length, length.label());
                    }
                });
        });
        form_row(ui, "Max tool steps", "Per reply.", |ui| {
            ui.add(egui::DragValue::new(&mut b.max_tool_rounds).range(1..=24));
        });
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(
            ui,
            "Initiative",
            "What it does without being asked. Requests from people are always limited by their own permissions.",
        );
        form_row(ui, "Autonomy", "", |ui| {
            egui::ComboBox::from_id_salt("autonomy")
                .selected_text(b.autonomy.label())
                .width(360.0)
                .show_ui(ui, |ui| {
                    for level in Autonomy::ALL {
                        ui.selectable_value(&mut b.autonomy, level, level.label());
                    }
                });
        });
        toggle_row(
            ui,
            &mut b.learn_culture,
            "Learn the server's culture",
            "Keeps shared notes on tone, in-jokes, norms, and people — all agents read them.",
        );
        if b.learn_culture {
            form_row(ui, "Reflect every", "", |ui| {
                ui.add(
                    egui::DragValue::new(&mut b.reflect_every_messages)
                        .range(30..=2000)
                        .suffix(" messages"),
                );
            });
        }
        toggle_row(
            ui,
            &mut b.use_hq,
            "Use the private agent HQ",
            "Finds it, or asks an admin to create one.",
        );
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(
            ui,
            "🛡 Moderation",
            "Heuristics on every message; borderline cases are judged by the model against the rules and the server's culture.",
        );
        let m = &mut b.moderation;
        toggle_row(
            ui,
            &mut m.enabled,
            "Watch for rule-breaking",
            "Staff and agents are never moderated. Acting needs autonomy \"Moderator\" or higher; below that it only reports.",
        );
        if m.enabled {
            toggle_row(
                ui,
                &mut m.llm_review,
                "Model review for borderline messages",
                "",
            );
            toggle_row(ui, &mut m.block_invites, "Remove server invite links", "");
            form_row(ui, "Mass mention at", "", |ui| {
                ui.add(
                    egui::DragValue::new(&mut m.max_mentions)
                        .range(0..=50)
                        .suffix(" mentions"),
                );
            });
            form_row(ui, "Flood at", "", |ui| {
                ui.add(
                    egui::DragValue::new(&mut m.flood_messages)
                        .range(0..=30)
                        .suffix(" msgs / 8s"),
                );
            });
            form_row(ui, "Duplicate spam at", "", |ui| {
                ui.add(
                    egui::DragValue::new(&mut m.duplicate_messages)
                        .range(0..=20)
                        .suffix(" repeats / 60s"),
                );
            });
            form_row(ui, "Timeout after", "", |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut m.strikes_before_timeout)
                            .range(0..=20)
                            .suffix(" strikes / 24h"),
                    );
                    ui.label("for");
                    ui.add(
                        egui::DragValue::new(&mut m.timeout_minutes)
                            .range(1..=60)
                            .suffix(" min"),
                    );
                });
            });
            form_row(
                ui,
                "Log channel",
                "Channel name for the mod log (HQ if not found).",
                |ui| {
                    ui.add(TextEdit::singleline(&mut m.log_channel).desired_width(200.0));
                },
            );
            form_row(ui, "Blocked words", "One per line; always removed.", |ui| {
                let mut words = m.blocked_words.join("\n");
                if ui
                    .add(
                        TextEdit::multiline(&mut words)
                            .desired_rows(3)
                            .desired_width(320.0),
                    )
                    .changed()
                {
                    m.blocked_words = words
                        .lines()
                        .map(|w| w.trim().to_owned())
                        .filter(|w| !w.is_empty())
                        .collect();
                }
            });
            form_row(
                ui,
                "Extra rules",
                "Beyond the server's rules channel.",
                |ui| {
                    ui.add(
                        TextEdit::multiline(&mut m.extra_rules)
                            .desired_rows(3)
                            .desired_width(420.0),
                    );
                },
            );
        }
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(ui, "Channels", "Paste channel IDs or #mentions.");
        form_row(ui, "Only in", "Empty = everywhere it can see.", |ui| {
            id_list(
                app,
                ui,
                format!("{}:allowed", p.id),
                &mut p.behavior.allowed_channels,
                "all channels",
            );
        });
        form_row(ui, "Never in", "", |ui| {
            id_list(
                app,
                ui,
                format!("{}:ignored", p.id),
                &mut p.behavior.ignored_channels,
                "none",
            );
        });
    });
}

// --- commands ----------------------------------------------------------------------

fn commands(app: &mut App, ui: &mut Ui, p: &mut AgentProfile) -> bool {
    let mut apply = false;
    // Apply bar: nothing reaches Discord until Apply is pressed.
    card(ui, |ui| {
        let state = app.sup.commands_state(&p.id);
        let current = crate::runtime::commands::fingerprint(p);
        ui.horizontal(|ui| {
            match &state {
                None => {
                    ui.label(RichText::new("Start the agent to apply commands to Discord.").color(WEAK));
                }
                Some((applied, status)) => {
                    let pending = applied.as_deref() != Some(current.as_str());
                    if pending {
                        ui.label(RichText::new("● Changes not applied to Discord yet").color(egui::Color32::from_rgb(230, 160, 60)));
                    } else {
                        ui.label(RichText::new("✓ Discord has these commands").color(egui::Color32::from_rgb(90, 190, 120)));
                    }
                    if !status.is_empty() {
                        ui.label(RichText::new(format!("· {status}")).color(WEAK));
                    }
                    if ui
                        .add_enabled(pending, egui::Button::new("Apply to Discord"))
                        .on_hover_text("Registers the commands now. Unchanged ones aren't touched, and it waits out Discord's rate limits (at most once every 20 seconds).")
                        .clicked()
                    {
                        apply = true;
                    }
                }
            }
        });
    });
    ui.add_space(8.0);
    let guilds = app.sup.live.snapshot(&p.id).guilds;
    let b = &mut p.behavior;
    card(ui, |ui| {
        section(ui, "⌨ Slash commands", "Agents can also turn these off per server, and make their own commands in a server, when someone with Manage Server asks.");
        toggle_row(ui, &mut b.slash_commands, "Slash commands", "Turn off to remove them all from Discord (after Apply).");
        if b.slash_commands {
            ui.add_space(4.0);
            egui::Grid::new("built_in_commands").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                for (name, help) in crate::runtime::commands::BUILT_IN {
                    let mut on = !b.disabled_commands.iter().any(|d| d == name);
                    if ui.checkbox(&mut on, format!("/{name}")).changed() {
                        b.disabled_commands.retain(|d| d != name);
                        if !on {
                            b.disabled_commands.push(name.to_owned());
                        }
                    }
                    ui.label(RichText::new(help).color(WEAK));
                    ui.end_row();
                }
            });
        }
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(
            ui,
            "Custom commands",
            "Reply with your text, or have the AI do what the text says (e.g. \"summarize the last 50 messages here\"). {user} pings whoever used it; {server}, {channel} and {input} are filled in.",
        );
        let mut remove = None;
        for (index, c) in b.custom_commands.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                ui.horizontal(|ui| {
                    ui.label("/");
                    let mut name = c.name.clone();
                    if ui.add(TextEdit::singleline(&mut name).hint_text("name").desired_width(140.0)).changed() {
                        c.name = name.to_lowercase().replace(' ', "-");
                    }
                    ui.add(TextEdit::singleline(&mut c.description).hint_text("description").desired_width(260.0));
                    if ui.small_button("🗑").on_hover_text("Remove").clicked() {
                        remove = Some(index);
                    }
                });
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("mode")
                        .selected_text(c.mode.label())
                        .show_ui(ui, |ui| {
                            for mode in crate::config::CommandMode::ALL {
                                ui.selectable_value(&mut c.mode, mode, mode.label());
                            }
                        });
                    ui.checkbox(&mut c.takes_input, "takes input").on_hover_text("Adds an optional \"input\" box to the command ({input}).");
                    ui.checkbox(&mut c.private, "only they see it");
                });
                let hint = match c.mode {
                    crate::config::CommandMode::Reply => "Reply text",
                    crate::config::CommandMode::Agent => "What the AI should do, e.g. \"post today's top tech news\"",
                };
                ui.add(TextEdit::multiline(&mut c.reply).hint_text(hint).desired_rows(2).desired_width(560.0));
                // Where it's registered.
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Servers:").color(WEAK));
                    let mut everywhere = c.servers.is_empty();
                    if ui.checkbox(&mut everywhere, "all").changed() && everywhere {
                        c.servers.clear();
                    }
                    for (guild_id, guild_name) in &guilds {
                        let mut on = c.servers.contains(guild_id);
                        if ui.checkbox(&mut on, guild_name.as_str()).changed() {
                            c.servers.retain(|g| g != guild_id);
                            if on {
                                c.servers.push(*guild_id);
                            }
                        }
                    }
                    if guilds.is_empty() && !c.servers.is_empty() {
                        ui.label(RichText::new(format!("{} server(s) — start the agent to see names", c.servers.len())).color(WEAK));
                    }
                });
                let built_in = crate::runtime::commands::BUILT_IN.iter().any(|(n, _)| *n == c.name);
                let problem = if !crate::config::CustomCommand::valid_name(&c.name) {
                    Some("Names: 1–32 lowercase letters, numbers, - or _.")
                } else if built_in {
                    Some("That name is taken by a built-in command.")
                } else if c.reply.trim().is_empty() {
                    Some("Needs text.")
                } else {
                    None
                };
                if let Some(problem) = problem {
                    ui.label(RichText::new(format!("⚠ {problem} (skipped until fixed)")).color(egui::Color32::from_rgb(230, 160, 60)));
                }
                ui.add_space(6.0);
            });
        }
        if let Some(index) = remove {
            b.custom_commands.remove(index);
        }
        if ui.button("➕ Add a command").clicked() {
            b.custom_commands.push(crate::config::CustomCommand {
                name: format!("command{}", b.custom_commands.len() + 1),
                ..Default::default()
            });
        }
    });
    // Commands agents made in servers.
    let made: Vec<(u64, String, Vec<crate::memory::ServerCommand>)> = app
        .sup
        .hive
        .knowledge
        .known_guilds()
        .into_iter()
        .map(|(g, name)| (g, name, app.sup.hive.knowledge.guild(g, |k| k.commands.iter().filter(|c| c.agent_id == p.id).cloned().collect::<Vec<_>>())))
        .filter(|(_, _, list)| !list.is_empty())
        .collect();
    if !made.is_empty() {
        ui.add_space(8.0);
        card(ui, |ui| {
            section(ui, "Made in servers", "Commands this agent made when asked. They apply on their own (rate-limited).");
            let mut changed = false;
            for (guild_id, guild_name, list) in made {
                ui.label(RichText::new(guild_name).strong());
                for c in list {
                    ui.horizontal(|ui| {
                        ui.label(format!("/{}", c.name));
                        ui.label(RichText::new(format!("{} · by {}", c.mode.label(), c.author_name)).color(WEAK));
                        let label = if c.disabled { "turn on" } else { "turn off" };
                        if ui.small_button(label).clicked() {
                            app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                                if let Some(x) = k.commands.iter_mut().find(|x| x.name == c.name && x.agent_id == c.agent_id) {
                                    x.disabled = !x.disabled;
                                }
                            });
                            changed = true;
                        }
                        if ui.small_button("🗑").on_hover_text("Delete").clicked() {
                            app.sup.hive.knowledge.guild_mut(guild_id, |k| k.commands.retain(|x| !(x.name == c.name && x.agent_id == c.agent_id)));
                            changed = true;
                        }
                    });
                }
            }
            if changed {
                apply = true;
            }
        });
    }
    apply
}

// --- tools -----------------------------------------------------------------------

fn tools(app: &mut App, ui: &mut Ui, p: &mut AgentProfile) {
    card(ui, |ui| {
        section(
            ui,
            "Tool groups",
            "Every tool also checks the requester's real Discord permissions and the role hierarchy when it runs.",
        );
        let t = &mut p.tools;
        let groups: [(&mut bool, &str, &str); 16] = [
            (
                &mut t.server_info,
                "Server awareness",
                "server info, channels, roles, members, read & search history, pins, threads, voice, audit log",
            ),
            (
                &mut t.messaging,
                "Messaging",
                "post in other channels, DMs, edit/delete own messages, pin",
            ),
            (&mut t.reactions, "Reactions", "react to messages"),
            (
                &mut t.threads,
                "Threads & forums",
                "start threads, create forum posts, archive, add members",
            ),
            (&mut t.polls, "Polls", "native Discord polls"),
            (
                &mut t.voice,
                "Voice",
                "join and talk in voice, move people, disconnect, mute/deafen, voice channel status",
            ),
            (
                &mut t.moderation,
                "Moderation",
                "delete, purge, warn, timeout, kick, ban, slowmode, lock channels, strike history",
            ),
            (
                &mut t.roles,
                "Roles",
                "give/remove/create/edit/delete roles (below its own role)",
            ),
            (
                &mut t.channels,
                "Channels",
                "create, clone, edit, delete channels; permissions; tickets; events; emojis; server settings; raw Discord API (admins only)",
            ),
            (
                &mut t.web_search,
                "Web search",
                "search the internet (configure the provider in Settings)",
            ),
            (
                &mut t.web_fetch,
                "Read web pages",
                "fetch links people share (private network addresses blocked)",
            ),
            (
                &mut t.memory,
                "Memory & tasks",
                "remember/recall, notes on people, directives, quiet mode, standing tasks",
            ),
            (
                &mut t.coordination,
                "Team coordination",
                "post in HQ, hand requests to other agents",
            ),
            (
                &mut t.utilities,
                "Utilities",
                "calculator, time, dice, reminders, weather, Wikipedia, dictionary, currency, invites, activity stats",
            ),
            (
                &mut t.images,
                "Images",
                "profile pictures, web image search, drawing, charts, edits (crop, filters, meme captions…), looking at images",
            ),
            (
                &mut t.mcp,
                "MCP servers",
                "tools from MCP servers configured in Settings",
            ),
        ];
        for (flag, label, help) in groups {
            toggle_row(ui, flag, label, help);
        }
    });
    ui.add_space(8.0);
    card(ui, |ui| {
        section(ui, "MCP tools available", "");
        let tools = app.sup.hive.mcp.tools();
        if tools.is_empty() {
            ui.label(
                RichText::new("No MCP servers connected — add them in Settings › MCP.")
                    .color(FAINT),
            );
        }
        for tool in tools {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&tool.exposed_name).monospace().color(TEXT));
                pill(ui, tool.access.label(), theme::YELLOW);
                ui.label(
                    RichText::new(crate::llm::truncate(&tool.description, 120))
                        .size(12.0)
                        .color(WEAK),
                );
            });
        }
    });
}

// --- memory ----------------------------------------------------------------------

fn memory(app: &mut App, ui: &mut Ui, profile: &AgentProfile) {
    let store = app.sup.memory(&profile.id);
    card(ui, |ui| {
        section(
            ui,
            "🧠 Long-term memory",
            "What this agent chose to remember. It recalls relevant memories in every conversation.",
        );
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut app.new_memory)
                    .hint_text("Teach it something directly…")
                    .desired_width(480.0),
            );
            if widgets::primary_button(ui, "Add").clicked() && !app.new_memory.trim().is_empty() {
                let (id, _) = store.add(Memory {
                    id: 0,
                    text: app.new_memory.trim().to_owned(),
                    kind: MemoryKind::Fact,
                    about_user: None,
                    about_name: None,
                    guild_id: None,
                    source: "from the operator".to_owned(),
                    created: now_ms(),
                    last_used: 0,
                    uses: 0,
                    importance: 4,
                    embedding: Vec::new(),
                });
                app.new_memory.clear();
                if let Some(core) = app.sup.core(&profile.id) {
                    core.embed_memory_later(id);
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(
                TextEdit::singleline(&mut app.memory_query)
                    .hint_text("filter")
                    .desired_width(300.0),
            );
            ui.label(RichText::new(format!("{} memories", store.len())).color(WEAK));
        });
    });
    ui.add_space(8.0);
    let query = app.memory_query.to_lowercase();
    let mut memories = store.all();
    memories.sort_by(|a, b| b.created.cmp(&a.created));
    for memory in memories.iter().filter(|m| {
        query.is_empty()
            || m.text.to_lowercase().contains(&query)
            || m.about_name
                .as_deref()
                .is_some_and(|n| n.to_lowercase().contains(&query))
    }) {
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                pill(ui, memory.kind.label(), ACCENT);
                if let Some(name) = &memory.about_name {
                    pill(ui, &format!("about {name}"), theme::BLUE);
                }
                pill(ui, &"★".repeat(memory.importance as usize), theme::YELLOW);
                ui.label(
                    RichText::new(format!(
                        "#{} · {} · {} · used {}×",
                        memory.id,
                        memory.source,
                        widgets::ago(memory.created),
                        memory.uses
                    ))
                    .size(11.5)
                    .color(FAINT),
                );
            });
            match &mut app.editing_memory {
                Some((id, text)) if *id == memory.id => {
                    ui.add(
                        TextEdit::multiline(text)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    );
                    let mut finished = false;
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            store.update_text(memory.id, text.clone());
                            finished = true;
                        } else if ui.button("Cancel").clicked() {
                            finished = true;
                        }
                    });
                    if finished {
                        app.editing_memory = None;
                    }
                }
                _ => {
                    ui.label(RichText::new(&memory.text).color(TEXT));
                    ui.horizontal(|ui| {
                        if ui.small_button("edit").clicked() {
                            app.editing_memory = Some((memory.id, memory.text.clone()));
                        }
                        if ui.small_button("forget").clicked() {
                            store.remove(memory.id);
                        }
                    });
                }
            }
        });
        ui.add_space(4.0);
    }
}

// --- tasks -----------------------------------------------------------------------

pub fn task_list(app: &mut App, ui: &mut Ui, agent: Option<&str>) {
    let guilds = app.sup.hive.knowledge.known_guilds();
    let mut any = false;
    for (guild_id, guild_name) in guilds {
        let tasks = app.sup.hive.knowledge.guild(guild_id, |k| k.tasks.clone());
        let tasks: Vec<_> = tasks
            .into_iter()
            .filter(|t| agent.is_none_or(|a| t.agent_id == a))
            .collect();
        if tasks.is_empty() {
            continue;
        }
        any = true;
        ui.label(
            RichText::new(if guild_name.is_empty() {
                guild_id.to_string()
            } else {
                guild_name
            })
            .strong()
            .color(WEAK),
        );
        for task in tasks {
            let core_name = app
                .config
                .agents
                .iter()
                .find(|a| a.id == task.agent_id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "?".to_owned());
            card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("#{}", task.id))
                            .strong()
                            .color(ACCENT),
                    );
                    pill(
                        ui,
                        if task.active { "active" } else { "paused" },
                        if task.active { theme::GREEN } else { FAINT },
                    );
                    let channel = app
                        .sup
                        .hive
                        .knowledge
                        .channel(task.trigger.channel_id(), |c| c.name.clone());
                    ui.label(
                        RichText::new(task.trigger.describe(channel.trim_start_matches('#')))
                            .color(TEXT),
                    );
                    if agent.is_none() {
                        pill(ui, &core_name, theme::BLUE);
                    }
                });
                match &mut app.editing_task {
                    Some((g, id, text)) if *g == guild_id && *id == task.id => {
                        ui.add(
                            TextEdit::multiline(text)
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );
                        let mut saved: Option<String> = None;
                        let mut finished = false;
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                saved = Some(text.clone());
                                finished = true;
                            } else if ui.button("Cancel").clicked() {
                                finished = true;
                            }
                        });
                        if let Some(new_text) = saved {
                            app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                                if let Some(t) = k.tasks.iter_mut().find(|t| t.id == task.id) {
                                    t.instruction = new_text;
                                }
                            });
                        }
                        if finished {
                            app.editing_task = None;
                        }
                    }
                    _ => {
                        ui.label(RichText::new(format!("“{}”", task.instruction)).color(TEXT));
                    }
                }
                ui.label(
                    RichText::new(format!(
                        "assigned by {} ({}) · ran {}× {}",
                        task.author_name,
                        task.author_level.label().to_lowercase(),
                        task.runs,
                        if task.last_result.is_empty() {
                            String::new()
                        } else {
                            format!("· last: {}", crate::llm::truncate(&task.last_result, 100))
                        }
                    ))
                    .size(12.0)
                    .color(FAINT),
                );
                ui.horizontal(|ui| {
                    if ui
                        .small_button(if task.active {
                            "⏸ pause"
                        } else {
                            "▶ resume"
                        })
                        .clicked()
                    {
                        app.sup.hive.knowledge.guild_mut(guild_id, |k| {
                            if let Some(t) = k.tasks.iter_mut().find(|t| t.id == task.id) {
                                t.active = !t.active;
                            }
                        });
                    }
                    if ui.small_button("✏ edit").clicked() {
                        app.editing_task = Some((guild_id, task.id, task.instruction.clone()));
                    }
                    if ui.small_button("🗑 delete").clicked() {
                        app.sup
                            .hive
                            .knowledge
                            .guild_mut(guild_id, |k| k.tasks.retain(|t| t.id != task.id));
                    }
                });
            });
            ui.add_space(4.0);
        }
    }
    if !any {
        ui.label(RichText::new("No standing tasks yet. Moderators give them in chat — e.g. “Nova, always look at new posts in #showcase; if a post doesn't follow the pinned guidelines, tell them what to edit.” Ask the agent “what are your tasks?” any time.").color(FAINT));
    }
}

fn tasks(app: &mut App, ui: &mut Ui, profile: &AgentProfile) {
    card(ui, |ui| {
        section(
            ui,
            "📋 Standing tasks",
            "Duties this agent keeps doing until told to stop. From the app you act as the operator (you can change anything); in Discord only the assigner or someone ranked at least as high can.",
        );
    });
    ui.add_space(8.0);
    task_list(app, ui, Some(&profile.id));
}

// --- console ---------------------------------------------------------------------

fn console(app: &mut App, ui: &mut Ui, profile: &AgentProfile) {
    let state = app
        .sup
        .live
        .playground
        .lock()
        .expect("live poisoned")
        .get(&profile.id)
        .cloned()
        .unwrap_or_default();
    ui.label(RichText::new("Talk to the agent directly, without Discord — the same brain, memory, web, and utility tools. You have full authority here.").color(WEAK));
    ui.add_space(4.0);
    let input_height = 70.0;
    egui::ScrollArea::vertical()
        .id_salt(("console", &profile.id))
        .stick_to_bottom(true)
        .max_height((ui.available_height() - input_height).max(120.0))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (role, text) in &state.messages {
                match role {
                    PlaygroundRole::User => {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                            bubble(ui, text, theme::dim(ACCENT, 70), ui.available_width() * 0.7);
                        });
                    }
                    PlaygroundRole::Agent => {
                        ui.horizontal_top(|ui| {
                            avatar(ui, &profile.name, theme::rgb(profile.accent), 26.0);
                            bubble(ui, text, theme::CARD, ui.available_width() * 0.8);
                        });
                    }
                    PlaygroundRole::Note => {
                        ui.label(RichText::new(text).size(11.5).color(FAINT));
                    }
                }
                ui.add_space(4.0);
            }
            if let Some(thought) = &state.thought {
                ui.horizontal_top(|ui| {
                    avatar(ui, &profile.name, theme::rgb(profile.accent), 26.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "💭 thinking… {}s",
                                thought.started.elapsed().as_secs()
                            ))
                            .color(ACCENT),
                        );
                        for step in &thought.steps {
                            ui.label(
                                RichText::new(format!("🔧 {step}"))
                                    .size(12.5)
                                    .color(theme::BLUE),
                            );
                        }
                        if !thought.reasoning.trim().is_empty() {
                            ui.label(
                                RichText::new(crate::llm::truncate(thought.reasoning.trim(), 600))
                                    .italics()
                                    .size(12.5)
                                    .color(WEAK),
                            );
                        }
                        if !thought.answer.is_empty() {
                            ui.label(RichText::new(&thought.answer).color(TEXT));
                        }
                    });
                });
            } else if state.busy {
                ui.label(RichText::new("💭 thinking…").color(ACCENT));
            }
        });
    ui.separator();
    ui.horizontal(|ui| {
        let response = ui.add(
            TextEdit::singleline(&mut app.console_input)
                .hint_text(format!("Message {}…", profile.name))
                .desired_width(ui.available_width() - 150.0),
        );
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let send = widgets::primary_button(ui, "Send").clicked() || enter;
        if send && !app.console_input.trim().is_empty() && !state.busy {
            let text = std::mem::take(&mut app.console_input);
            app.sup.console_send(profile, text.trim().to_owned());
            response.request_focus();
        }
        if ui.button("Clear").clicked() {
            app.sup.console_clear(&profile.id);
        }
    });
}

fn bubble(ui: &mut Ui, text: &str, fill: egui::Color32, max_width: f32) {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_max_width(max_width);
            ui.label(RichText::new(text).color(TEXT));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_id_decodes_from_token() {
        // "80351110224678912" base64-encoded, as Discord tokens begin.
        assert_eq!(
            application_id_from_token("ODAzNTExMTAyMjQ2Nzg5MTI.Gxyz.abc"),
            Some(80_351_110_224_678_912)
        );
        assert_eq!(application_id_from_token("not a token!"), None);
    }
}
