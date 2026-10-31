//! Axis Bot Studio — a small desktop app that runs an Axis Discord bot
//! locally: paste your token, edit commands, watch the gateway live.
//!
//! Layout: connection controls on top, the command editor on the right,
//! and tabbed central views (log / messages / send message) in the middle.

mod bot;
mod commands_model;
mod config;

use std::{collections::VecDeque, path::PathBuf};

use bot::{BotCommand, BotEvent, BotHandle, Status};
use commands_model::{OptionKind, UserCommand, UserOption};

fn main() -> eframe::Result<()> {
    // Library logs (reconnects, rate limits) go to stderr via tracing;
    // the GUI keeps its own on-screen log.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("axis=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([860.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Axis Bot Studio",
        native,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tab {
    Log,
    Messages,
    Send,
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

    token: String,
    status: Status,
    log: VecDeque<String>,
    messages: VecDeque<MessageLine>,

    tab: Tab,
    autoscroll: bool,

    // Command editor state.
    editor: UserCommand,
    editing: Option<usize>,
    editor_error: Option<String>,

    // Send-message panel state.
    send_channel: String,
    send_content: String,
}

impl App {
    fn new() -> Self {
        let config_path = config::default_path();
        let config = config::load(&config_path);
        let editor = config
            .commands
            .first()
            .cloned()
            .unwrap_or_else(|| UserCommand {
                name: String::new(),
                description: String::new(),
                reply: String::new(),
                options: Vec::new(),
            });
        Self {
            bot: BotHandle::spawn(),
            config_path,
            config,
            token: String::new(),
            status: Status::Offline,
            log: VecDeque::new(),
            messages: VecDeque::new(),
            tab: Tab::Log,
            autoscroll: true,
            editor,
            editing: None,
            editor_error: None,
            send_channel: String::new(),
            send_content: String::new(),
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        let stamp = axis::timestamp::Timestamp::now_utc().as_datetime();
        self.log.push_back(format!(
            "{:02}:{:02}:{:02} {}",
            stamp.hour(),
            stamp.minute(),
            stamp.second(),
            line.into()
        ));
        while self.log.len() > 500 {
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
                        Status::Reconnecting(code) => {
                            format!("reconnecting (close code {code:?})…")
                        }
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
                    self.push_log(format!("commands synced to Discord: {}", names.join(", ")));
                }
            }
        }

        // Keep polling while connected so gateway traffic shows up.
        if self.bot.is_running() {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    fn save_config(&mut self) {
        if let Err(error) = config::save(&self.config_path, &self.config) {
            self.push_log(format!("ERROR: {error}"));
        }
    }

    // ----- UI sections -----

    fn connection_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Axis Bot Studio");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (label, color) = match &self.status {
                    Status::Offline => ("offline".to_owned(), egui::Color32::GRAY),
                    Status::Connecting => ("connecting…".to_owned(), egui::Color32::YELLOW),
                    Status::Connected(name) => (format!("online as {name}"), egui::Color32::GREEN),
                    Status::Resumed => ("online (resumed)".to_owned(), egui::Color32::GREEN),
                    Status::Reconnecting(_) => ("reconnecting…".to_owned(), egui::Color32::YELLOW),
                    Status::Failed(_) => ("failed".to_owned(), egui::Color32::RED),
                    Status::Disconnected(_) => ("disconnected".to_owned(), egui::Color32::GRAY),
                };
                ui.colored_label(color, label);
            });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Token:");
            ui.add(
                egui::TextEdit::singleline(&mut self.token)
                    .password(true)
                    .hint_text("paste your bot token (never saved to disk)")
                    .desired_width(340.0),
            );

            ui.label("Application ID:");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.config.application_id)
                        .hint_text("numeric app id")
                        .desired_width(140.0),
                )
                .lost_focus()
            {
                self.save_config();
            }

            let running = self.bot.is_running();
            if !running && ui.button("▶ Connect").clicked() {
                let trimmed = self.token.trim().to_owned();
                if trimmed.is_empty() {
                    self.push_log("ERROR: paste a bot token first");
                } else if let Ok(application_id) = self.config.application_id.trim().parse::<u64>()
                {
                    self.bot.send(BotCommand::Connect {
                        token: trimmed,
                        application_id,
                    });
                } else {
                    self.push_log(
                        "ERROR: application ID must be numeric (Developer Portal → General Information)",
                    );
                }
            }
            if running && ui.button("⏹ Disconnect").clicked() {
                self.bot.send(BotCommand::Disconnect);
            }
        });
        ui.separator();
    }

    fn commands_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Commands");
        ui.weak("Built-ins: /ping, /echo, /roll");

        ui.add_space(4.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut delete_index: Option<usize> = None;
            for (index, command) in self.config.commands.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("/{}", command.name));
                    ui.weak(&command.description);
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
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.editor.name)
                            .hint_text("lowercase, e.g. greet")
                            .desired_width(180.0),
                    );
                    ui.end_row();

                    ui.label("Description");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.editor.description)
                            .hint_text("Shown in Discord")
                            .desired_width(180.0),
                    );
                    ui.end_row();

                    ui.label("Reply");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.editor.reply)
                            .hint_text("Supports {option_name} and {user}")
                            .desired_width(220.0),
                    );
                    ui.end_row();
                });

            ui.weak("Options (optional):");
            let mut add_option: Option<OptionKind> = None;
            let mut remove_option: Option<usize> = None;
            for (index, option) in self.editor.options.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut option.name)
                            .hint_text("name")
                            .desired_width(70.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut option.description)
                            .hint_text("description")
                            .desired_width(110.0),
                    );
                    egui::ComboBox::from_id_salt(format!("kind-{index}"))
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
                    self.editor = UserCommand {
                        name: String::new(),
                        description: String::new(),
                        reply: String::new(),
                        options: Vec::new(),
                    };
                }
            });

            ui.add_space(8.0);
            let running = self.bot.is_running();
            if ui
                .add_enabled(running, egui::Button::new("⇅ Sync to Discord (global)"))
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
        };
        self.save_config();
        self.push_log("command saved (use Sync to update Discord)");
    }

    fn central_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Log, "Log");
                ui.selectable_value(&mut self.tab, Tab::Messages, "Messages");
                ui.selectable_value(&mut self.tab, Tab::Send, "Send message");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.autoscroll, "autoscroll");
                });
            });
            ui.separator();

            match self.tab {
                Tab::Log => {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(self.autoscroll)
                        .show(ui, |ui| {
                            for line in &self.log {
                                ui.monospace(line);
                            }
                        });
                }
                Tab::Messages => {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(self.autoscroll)
                        .show(ui, |ui| {
                            if self.messages.is_empty() {
                                ui.weak(
                                    "No messages seen yet (requires the MESSAGE_CONTENT intent).",
                                );
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
                Tab::Send => {
                    ui.add_space(4.0);
                    ui.label(
                        "Send a message through the bot. Copy a channel ID in Discord via \
                         right-click → Copy Channel ID (needs Developer Mode enabled).",
                    );
                    ui.add_space(4.0);
                    egui::Grid::new("send_grid")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Channel ID");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.send_channel)
                                    .hint_text("e.g. 123456789012345678")
                                    .desired_width(260.0),
                            );
                            ui.end_row();
                            ui.label("Content");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.send_content)
                                    .hint_text("message text")
                                    .desired_width(420.0),
                            );
                            ui.end_row();
                        });
                    ui.add_space(4.0);
                    if ui.button("Send").clicked() {
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
                    if !self.bot.is_running() {
                        ui.weak("Connect first to send.");
                    }
                }
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_bot(ctx);

        egui::TopBottomPanel::top("connection").show(ctx, |ui| {
            self.connection_panel(ui);
        });

        egui::SidePanel::right("commands")
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| {
                self.commands_panel(ui);
            });

        self.central_panel(ctx);
    }
}
