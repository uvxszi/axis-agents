//! Axis Agents — a small desktop app that runs an AI-powered Discord bot
//! locally on the Axis library.

mod agent;
mod bot;
mod commands_model;
mod config;
mod llm;

use std::{collections::VecDeque, path::PathBuf};

use bot::{BotCommand, BotEvent, BotHandle, Status};
use commands_model::{CommandBehavior, OptionKind, UserCommand, UserOption};

fn main() -> eframe::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("axis=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1160.0, 760.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Axis Agents",
        native,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Nav {
    Connection,
    Agent,
    Commands,
    Log,
    Messages,
    Send,
}

impl Nav {
    const ALL: [Nav; 6] = [
        Nav::Connection,
        Nav::Agent,
        Nav::Commands,
        Nav::Log,
        Nav::Messages,
        Nav::Send,
    ];
    fn label(self) -> &'static str {
        match self {
            Nav::Connection => "🔌  Connection",
            Nav::Agent => "🤖  AI Agent",
            Nav::Commands => "⌨️  Commands",
            Nav::Log => "📜  Log",
            Nav::Messages => "💬  Messages",
            Nav::Send => "✉️  Send message",
        }
    }
}

struct MessageLine {
    author: String,
    bot: bool,
    channel_id: u64,
    content: String,
}

struct App {
    bot: BotHandle,
    config_path: PathBuf,
    config: config::AppConfig,
    token_input: String,
    status: Status,
    log: VecDeque<String>,
    messages: VecDeque<MessageLine>,
    nav: Nav,
    autoscroll: bool,
    editor: UserCommand,
    editing: Option<usize>,
    editor_error: Option<String>,
    models: Vec<String>,
    models_status: Option<String>,
    send_channel: String,
    send_content: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "noto_emoji_full".to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                "../assets/NotoEmoji.ttf"
            ))),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("noto_emoji_full".to_owned());
        }
        cc.egui_ctx.set_fonts(fonts);
        cc.egui_ctx.set_theme(egui::Theme::Dark);

        let config_path = config::default_path();
        let mut config = config::load(&config_path);
        config.seed_built_ins();
        let token_input = if config.save_token {
            config.token.clone()
        } else {
            String::new()
        };
        let editor = config
            .commands
            .first()
            .cloned()
            .unwrap_or_else(|| UserCommand {
                name: String::new(),
                description: String::new(),
                reply: String::new(),
                options: Vec::new(),
                behavior: CommandBehavior::Reply,
            });

        // Push the initial agent config to the shared state.
        // This happens implicitly because bot.update_agent is called below.
        let bot = BotHandle::spawn();
        bot.update_agent(config.agent.clone());

        Self {
            bot,
            config_path,
            token_input,
            status: Status::Offline,
            log: VecDeque::new(),
            messages: VecDeque::new(),
            nav: Nav::Connection,
            autoscroll: true,
            editor,
            editing: None,
            editor_error: None,
            models: Vec::new(),
            models_status: None,
            send_channel: String::new(),
            send_content: String::new(),
            config,
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        let stamp = axis::timestamp::Timestamp::now_utc().as_datetime();
        self.log.push_back(format!(
            "{:02}:{:02}:{:02}  {}",
            stamp.hour(),
            stamp.minute(),
            stamp.second(),
            line.into()
        ));
        while self.log.len() > 800 {
            self.log.pop_front();
        }
    }

    fn poll_bot(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.bot.events.try_recv() {
            match event {
                BotEvent::Status(status) => {
                    self.push_log(match &status {
                        Status::Offline => "offline".to_owned(),
                        Status::Connecting => "connecting…".to_owned(),
                        Status::Connected(name) => format!("connected as {name}"),
                        Status::Resumed => "session resumed".to_owned(),
                        Status::Reconnecting(code) => format!("reconnecting ({code:?})…"),
                        Status::Failed(reason) => format!("FAILED: {reason}"),
                        Status::Disconnected(reason) => format!("disconnected: {reason}"),
                    });
                    self.status = status;
                }
                BotEvent::Log(line) => self.push_log(line),
                BotEvent::ActionFailed(line) => self.push_log(format!("ERROR: {line}")),
                BotEvent::Message {
                    author,
                    bot,
                    channel_id,
                    content,
                } => {
                    self.messages.push_back(MessageLine {
                        author,
                        bot,
                        channel_id,
                        content,
                    });
                    while self.messages.len() > 200 {
                        self.messages.pop_front();
                    }
                }
                BotEvent::Interaction { command, user } => {
                    self.push_log(format!("/{command} invoked by {user}"));
                }
                BotEvent::CommandsSynced(names) => {
                    self.push_log(format!("commands synced: {}", names.join(", ")));
                }
                BotEvent::ModelsFetched(models) => {
                    self.push_log(format!("fetched {} models", models.len()));
                    self.models_status =
                        Some(format!("Found {} models — pick one below.", models.len()));
                    self.models = models;
                }
            }
        }
        if self.bot.is_running() {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    fn save_config(&mut self) {
        if let Err(error) = config::save(&self.config_path, &self.config) {
            self.push_log(format!("ERROR: {error}"));
        }
    }

    fn status_label(&self) -> (String, egui::Color32) {
        match &self.status {
            Status::Offline => ("offline".to_owned(), egui::Color32::GRAY),
            Status::Connecting => ("connecting…".to_owned(), egui::Color32::YELLOW),
            Status::Connected(name) => (format!("online as {name}"), egui::Color32::GREEN),
            Status::Resumed => ("online (resumed)".to_owned(), egui::Color32::GREEN),
            Status::Reconnecting(code) => {
                (format!("reconnecting ({code:?})…"), egui::Color32::YELLOW)
            }
            Status::Failed(reason) => (format!("failed: {reason}"), egui::Color32::RED),
            Status::Disconnected(reason) => {
                (format!("disconnected: {reason}"), egui::Color32::GRAY)
            }
        }
    }

    fn nav_panel(&mut self, ctx: &egui::Context) {
        let (label, color) = self.status_label();
        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(190.0)
            .show(ctx, |ui| {
                ui.add_space(10.0);
                ui.heading("Axis Agents");
                ui.weak("powered by the Axis library");
                ui.add_space(8.0);
                ui.separator();
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                    ui.add_space(6.0);
                    for nav in Nav::ALL {
                        ui.selectable_value(&mut self.nav, nav, nav.label());
                        ui.add_space(2.0);
                    }
                });
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.separator();
                    ui.add_space(6.0);
                    ui.colored_label(color, label);
                    ui.weak(format!("v{}", env!("CARGO_PKG_VERSION")));
                });
            });
    }

    fn connection_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connection");
        ui.add_space(6.0);
        ui.label("Paste your bot token (Developer Portal → Bot → Reset Token).");
        ui.add_space(6.0);

        egui::Grid::new("connection_grid")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("Bot token");
                ui.add(
                    egui::TextEdit::singleline(&mut self.token_input)
                        .password(true)
                        .hint_text("MTIzNDU2Nzg5…")
                        .desired_width(420.0),
                );
                ui.end_row();
                ui.label("Application ID");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.config.application_id)
                            .hint_text("numeric app id")
                            .desired_width(420.0),
                    )
                    .changed()
                {
                    self.save_config();
                }
                ui.end_row();
            });

        if ui
            .checkbox(
                &mut self.config.save_token,
                "Remember token on disk (plain text next to this exe)",
            )
            .changed()
        {
            if self.config.save_token {
                self.config.token = self.token_input.trim().to_owned();
            } else {
                self.config.token.clear();
            }
            self.save_config();
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let running = self.bot.is_running();
            if !running && ui.button("▶  Connect").clicked() {
                let trimmed = self.token_input.trim().to_owned();
                if trimmed.is_empty() {
                    self.push_log("ERROR: paste a bot token first");
                } else if let Ok(application_id) = self.config.application_id.trim().parse::<u64>()
                {
                    self.config.token = if self.config.save_token {
                        trimmed.clone()
                    } else {
                        String::new()
                    };
                    self.save_config();
                    self.bot.send(BotCommand::Connect {
                        token: trimmed,
                        application_id,
                        commands: self.config.commands.clone(),
                    });
                } else {
                    self.push_log("ERROR: application ID must be numeric");
                }
            }
            if running && ui.button("⏹  Disconnect").clicked() {
                self.bot.send(BotCommand::Disconnect);
            }
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(4.0);
        ui.strong("Setup checklist");
        ui.label("•  Create the app in the Developer Portal and invite it with the `bot` + `applications.commands` scopes.");
        ui.label("•  Enable the Message Content intent (Bot → Privileged Gateway Intents).");
        ui.label("•  Enable any other privileged intents your automations need.");
    }

    fn agent_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("AI Agent");
        ui.add_space(4.0);

        if ui
            .checkbox(
                &mut self.config.agent.enabled,
                "Enable the AI agent (responds to @mentions and its name)",
            )
            .changed()
        {
            self.bot.update_agent(self.config.agent.clone());
            self.save_config();
        }

        ui.add_space(8.0);
        ui.separator();
        ui.strong("Provider");
        ui.add_space(4.0);

        let mut provider_changed = false;
        egui::Grid::new("provider_grid")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("Provider");
                if egui::ComboBox::from_id_salt("provider")
                    .selected_text(self.config.agent.provider.label())
                    .show_ui(ui, |ui| {
                        for provider in llm::Provider::ALL {
                            ui.selectable_value(
                                &mut self.config.agent.provider,
                                provider,
                                provider.label(),
                            );
                        }
                    })
                    .response
                    .changed()
                {
                    self.config.agent.base_url =
                        self.config.agent.provider.default_base_url().to_owned();
                    self.config.agent.model = self.config.agent.provider.default_model().to_owned();
                    self.models.clear();
                    self.models_status = None;
                    provider_changed = true;
                }
                ui.end_row();

                ui.label("Base URL");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.config.agent.base_url)
                            .hint_text("http://localhost:1234/v1")
                            .desired_width(420.0),
                    )
                    .changed()
                {
                    provider_changed = true;
                }
                ui.end_row();

                ui.label("API key");
                ui.add(
                    egui::TextEdit::singleline(&mut self.config.agent.api_key)
                        .password(true)
                        .hint_text("empty for LM Studio")
                        .desired_width(420.0),
                );
                ui.end_row();

                ui.label("Model");
                if self.models.is_empty() {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.agent.model)
                            .hint_text("model id")
                            .desired_width(420.0),
                    );
                } else {
                    egui::ComboBox::from_id_salt("model_choice")
                        .selected_text(self.config.agent.model.clone())
                        .show_ui(ui, |ui| {
                            for model in &self.models {
                                ui.selectable_value(
                                    &mut self.config.agent.model,
                                    model.clone(),
                                    model,
                                );
                            }
                        });
                }
                ui.end_row();
            });

        ui.horizontal(|ui| {
            if ui.button("🔄  Fetch model list").clicked() {
                self.bot.send(BotCommand::FetchModels {
                    base_url: self.config.agent.base_url.clone(),
                    api_key: self.config.agent.api_key.clone(),
                });
                self.models_status = Some("fetching…".to_owned());
            }
            if let Some(status) = &self.models_status {
                ui.weak(status);
            }
        });
        if provider_changed {
            self.bot.update_agent(self.config.agent.clone());
            self.save_config();
        }

        ui.add_space(8.0);
        ui.separator();
        ui.strong("Behavior");
        ui.add_space(4.0);

        egui::Grid::new("agent_behavior_grid")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("Agent name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.config.agent.name)
                        .hint_text("e.g. magi — used as the trigger word")
                        .desired_width(300.0),
                );
                ui.end_row();

                ui.label("Temperature");
                let old_temperature = self.config.agent.temperature;
                ui.add(
                    egui::Slider::new(&mut self.config.agent.temperature, 0.0..=2.0)
                        .text("0 = focused, 2 = creative"),
                );
                if (old_temperature - self.config.agent.temperature).abs() > f32::EPSILON {
                    self.bot.update_agent(self.config.agent.clone());
                    self.save_config();
                }
                ui.end_row();

                ui.label("Max tool rounds");
                let mut iterations = self.config.agent.max_iterations;
                ui.add(egui::DragValue::new(&mut iterations).range(1..=12));
                ui.end_row();

                ui.label("Context budget");
                let mut budget = self.config.agent.context_budget;
                ui.add(
                    egui::DragValue::new(&mut budget)
                        .range(500..=32000)
                        .suffix(" tokens"),
                );
                ui.end_row();

                ui.label("Conversation timeout");
                let mut timeout = self.config.agent.conversation_timeout_secs;
                ui.add(
                    egui::DragValue::new(&mut timeout)
                        .range(30..=3600)
                        .suffix("s"),
                );
                ui.end_row();

                ui.label("Max concurrent");
                let mut concurrent = self.config.agent.max_concurrent;
                ui.add(
                    egui::DragValue::new(&mut concurrent)
                        .range(1..=8)
                        .suffix(" LLM calls"),
                );
                ui.end_row();
            });

        // Save behavior settings when any changed (check periodically).
        self.bot.update_agent(self.config.agent.clone());
        self.save_config();

        if ui
            .checkbox(
                &mut self.config.agent.mention_trigger,
                "Summon the agent by @mentioning the bot",
            )
            .changed()
        {
            self.bot.update_agent(self.config.agent.clone());
            self.save_config();
        }

        ui.add_space(8.0);
        ui.strong("Standing instructions (the agent's persona and rules)");
        ui.weak("The agent always enforces Discord permissions on top of these — it can never act beyond what the invoking user may do.");
        if ui
            .add(
                egui::TextEdit::multiline(&mut self.config.agent.instructions)
                    .hint_text("e.g. You are the moderator helper of this server. Be concise.")
                    .desired_rows(6)
                    .desired_width(f32::INFINITY),
            )
            .lost_focus()
        {
            self.bot.update_agent(self.config.agent.clone());
            self.save_config();
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("🧹  Clear conversation memory").clicked() {
                self.bot.agent().reset_memory();
                self.push_log("agent conversation memory cleared");
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.strong("MCP-style tools (permission-gated to the invoking user)");
        ui.weak("Every tool checks the invoking user's real Discord permissions in code before it runs.");
        for definition in agent::definitions() {
            let name = definition["function"]["name"].as_str().unwrap_or("?");
            let description = definition["function"]["description"].as_str().unwrap_or("");
            ui.label(format!("•  {name} — {description}"));
        }
    }

    fn commands_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Commands");
        ui.weak(
            "Built-ins are seeded and fully editable. Add new commands or modify existing ones.",
        );

        ui.add_space(4.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut delete_index: Option<usize> = None;
            for (index, command) in self.config.commands.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("/{}", command.name));
                    ui.weak(&command.description);
                    ui.weak(format!("({})", command.behavior.label()));
                    if ui.small_button("edit").clicked() {
                        self.editing = Some(index);
                        self.editor = command.clone();
                        self.editor_error = None;
                    }
                    if ui.small_button("✕").clicked() {
                        delete_index = Some(index);
                    }
                });
            }
            if let Some(index) = delete_index {
                self.config.commands.remove(index);
                self.save_config();
                self.push_log("command removed (re-sync to apply on Discord)");
            }

            ui.add_space(8.0);
            ui.separator();
            ui.strong(if self.editing.is_some() {
                "Edit command"
            } else {
                "New command"
            });

            egui::Grid::new("command_editor")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.editor.name)
                            .hint_text("lowercase, e.g. greet")
                            .desired_width(200.0),
                    );
                    ui.end_row();
                    ui.label("Description");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.editor.description)
                            .hint_text("Shown in Discord")
                            .desired_width(260.0),
                    );
                    ui.end_row();
                    ui.label("Behavior");
                    egui::ComboBox::from_id_salt("behavior")
                        .selected_text(self.editor.behavior.label())
                        .show_ui(ui, |ui| {
                            for behavior in CommandBehavior::ALL {
                                ui.selectable_value(
                                    &mut self.editor.behavior,
                                    behavior,
                                    behavior.label(),
                                );
                            }
                        });
                    ui.end_row();
                    match self.editor.behavior {
                        CommandBehavior::Reply | CommandBehavior::Roll => {
                            ui.label(if self.editor.behavior == CommandBehavior::Roll {
                                "Reply (use {roll})"
                            } else {
                                "Reply (use {option} and {user})"
                            });
                            ui.add(
                                egui::TextEdit::singleline(&mut self.editor.reply)
                                    .desired_width(320.0),
                            );
                            ui.end_row();
                        }
                        CommandBehavior::Ping => {
                            ui.label("Reply");
                            ui.weak("(unused — ping measures latency)");
                            ui.end_row();
                        }
                    }
                });

            ui.weak("Options (optional):");
            let mut add_option: Option<OptionKind> = None;
            let mut remove_option: Option<usize> = None;
            for (index, option) in self.editor.options.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut option.name)
                            .hint_text("name")
                            .desired_width(90.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut option.description)
                            .hint_text("description")
                            .desired_width(160.0),
                    );
                    egui::ComboBox::from_id_salt(format!("option-kind-{index}"))
                        .selected_text(option.kind.label())
                        .show_ui(ui, |ui| {
                            for kind in OptionKind::ALL {
                                ui.selectable_value(&mut option.kind, kind, kind.label());
                            }
                        });
                    ui.checkbox(&mut option.required, "required");
                    if ui.small_button("✕").clicked() {
                        remove_option = Some(index);
                    }
                });
            }
            ui.horizontal(|ui| {
                for kind in OptionKind::ALL {
                    if ui.small_button(format!("+ {}", kind.label())).clicked() {
                        add_option = Some(kind);
                    }
                }
            });
            if let Some(kind) = add_option {
                self.editor.options.push(UserOption {
                    name: String::new(),
                    description: String::new(),
                    kind,
                    required: false,
                });
            }
            if let Some(index) = remove_option {
                self.editor.options.remove(index);
            }

            ui.add_space(4.0);
            if let Some(error) = &self.editor_error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.horizontal(|ui| {
                if ui
                    .button(if self.editing.is_some() {
                        "Save changes"
                    } else {
                        "Add command"
                    })
                    .clicked()
                {
                    self.save_editor_command();
                }
                if self.editing.is_some() && ui.button("Cancel").clicked() {
                    self.editing = None;
                    self.editor_error = None;
                }
            });

            ui.add_space(8.0);
            if ui.button("↺  Restore default commands").clicked() {
                self.config.commands = commands_model::default_commands();
                self.save_config();
                self.push_log("default commands restored");
            }

            ui.add_space(8.0);
            let running = self.bot.is_running();
            if ui
                .add_enabled(running, egui::Button::new("⇅  Sync to Discord (global)"))
                .clicked()
            {
                let (registry, validation) = commands_model::build_registry(&self.config.commands);
                match validation {
                    Ok(()) => {
                        self.bot.update_registry(registry);
                        self.bot.send(BotCommand::SyncCommands);
                    }
                    Err(error) => self.push_log(format!("ERROR: {error}")),
                }
            }
            if !running {
                ui.weak("Connect first to sync.");
            }
        });
    }

    fn save_editor_command(&mut self) {
        let command = self.editor.clone();
        if !commands_model::valid_command_name(&command.name) {
            self.editor_error = Some(
                "Name must be 1-32 chars: lowercase letters, digits, hyphens, underscores"
                    .to_owned(),
            );
            return;
        }
        if !commands_model::valid_description(&command.description) {
            self.editor_error = Some("Description must be 1-100 characters".to_owned());
            return;
        }
        if self
            .config
            .commands
            .iter()
            .enumerate()
            .any(|(index, existing)| existing.name == command.name && self.editing != Some(index))
        {
            self.editor_error = Some(format!("A command named /{} already exists", command.name));
            return;
        }
        match self.editing.take() {
            Some(index) => self.config.commands[index] = command,
            None => self.config.commands.push(command),
        }
        self.editor_error = None;
        self.editor = UserCommand {
            name: String::new(),
            description: String::new(),
            reply: String::new(),
            options: Vec::new(),
            behavior: CommandBehavior::Reply,
        };
        self.save_config();
        self.push_log("command saved (use Sync to update Discord)");
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .stick_to_bottom(self.autoscroll)
            .show(ui, |ui| {
                for line in &self.log {
                    ui.monospace(line);
                }
            });
    }

    fn messages_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .stick_to_bottom(self.autoscroll)
            .show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.weak("No messages seen yet (requires the MESSAGE_CONTENT intent).");
                }
                for message in &self.messages {
                    ui.horizontal(|ui| {
                        if message.bot {
                            ui.weak(format!("[bot] {}", message.author));
                        } else {
                            ui.strong(&message.author);
                        }
                        ui.weak(format!("in {}", message.channel_id));
                    });
                    ui.label(&message.content);
                    ui.separator();
                }
            });
    }

    fn send_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label("Send a message through the bot. Copy a channel ID in Discord via right-click → Copy Channel ID (needs Developer Mode).");
        ui.add_space(6.0);
        egui::Grid::new("send_grid")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("Channel ID");
                let channel_response = ui.add(
                    egui::TextEdit::singleline(&mut self.send_channel)
                        .hint_text("e.g. 123456789012345678")
                        .desired_width(280.0),
                );
                ui.end_row();
                ui.label("Content");
                let content_response = ui.add(
                    egui::TextEdit::singleline(&mut self.send_content)
                        .hint_text("message text")
                        .desired_width(480.0),
                );
                ui.end_row();
                let enter_pressed = (channel_response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    || (content_response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                if enter_pressed {
                    self.send_message();
                }
            });
        ui.add_space(4.0);
        if ui.button("✉  Send").clicked() {
            self.send_message();
        }
        if !self.bot.is_running() {
            ui.weak("Connect first to send.");
        }
    }

    fn send_message(&mut self) {
        match self.send_channel.trim().parse::<u64>() {
            Ok(channel_id) if !self.send_content.trim().is_empty() => {
                self.bot.send(BotCommand::SendMessage {
                    channel_id,
                    content: self.send_content.trim().to_owned(),
                });
                self.send_content.clear();
            }
            Ok(_) => self.push_log("ERROR: message text is empty"),
            Err(_) => self.push_log("ERROR: channel ID must be numeric"),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_bot(ctx);
        self.nav_panel(ctx);
        egui::CentralPanel::default().show(ctx, |ui| match self.nav {
            Nav::Connection => self.connection_panel(ui),
            Nav::Agent => self.agent_panel(ui),
            Nav::Commands => self.commands_panel(ui),
            Nav::Log => {
                ui.horizontal(|ui| {
                    ui.heading("Log");
                    ui.checkbox(&mut self.autoscroll, "autoscroll");
                });
                ui.separator();
                self.log_panel(ui);
            }
            Nav::Messages => {
                ui.horizontal(|ui| {
                    ui.heading("Messages");
                    ui.checkbox(&mut self.autoscroll, "autoscroll");
                });
                ui.separator();
                self.messages_panel(ui);
            }
            Nav::Send => {
                ui.heading("Send message");
                ui.separator();
                self.send_panel(ui);
            }
        });
    }
}
