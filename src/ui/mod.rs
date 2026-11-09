//! The desktop app: sidebar navigation, pages, saving.
//!
//! Edits are applied to a copy of the profile, then written back and
//! marked dirty; dirty agents are pushed to the runtime (live, without a
//! restart unless the token changed) and the config is saved, debounced,
//! about a second after the last edit.

mod agent_page;
mod dashboard;
mod eyes;
mod knowledge;
mod settings;
pub mod theme;
pub mod widgets;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};

use egui::{CornerRadius, Margin, RichText, Stroke};

use crate::config::{self, AgentProfile, AppConfig};
use crate::runtime::Supervisor;
use crate::runtime::live::FeedKind;
use theme::{ACCENT, SIDEBAR, TEXT, WEAK};
use widgets::{avatar, status_color, status_dot};

#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Dashboard,
    Agent(String),
    Eyes,
    Knowledge,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTab {
    Overview,
    Discord,
    Persona,
    Model,
    Behavior,
    Tools,
    Commands,
    Voice,
    Memory,
    Tasks,
    Console,
    Activity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeTab {
    Culture,
    People,
    Directives,
    Tasks,
    Moderation,
    Channels,
}

pub struct Toast {
    at: Instant,
    text: String,
    error: bool,
}

pub struct App {
    pub sup: Supervisor,
    pub config: AppConfig,
    config_path: PathBuf,
    pub page: Page,
    pub agent_tab: AgentTab,
    dirty_agents: HashSet<String>,
    settings_dirty: bool,
    dirty_since: Option<Instant>,
    toasts: Vec<Toast>,
    pub show_token: bool,
    pub console_input: String,
    pub emoji_new_tool: String,
    /// Bot's-eyes page state.
    pub eyes_agent: Option<String>,
    pub eyes_guild: Option<u64>,
    pub eyes_channel: Option<u64>,
    pub eyes_reply_to: Option<u64>,
    pub eyes_input: String,
    pub memory_query: String,
    pub new_memory: String,
    pub buffers: HashMap<String, String>,
    pub knowledge_guild: Option<u64>,
    pub knowledge_tab: KnowledgeTab,
    pub confirm_delete: Option<String>,
    pub feed_kind: Option<FeedKind>,
    pub editing_memory: Option<(u64, String)>,
    pub editing_task: Option<(u64, u64, String)>,
    pub selected_mcp: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        let config_path = crate::paths::config_path();
        let (config, migrated) = match config::load(&config_path) {
            Some(config) => (config, false),
            None => match config::migrate_legacy() {
                Some(config) => (config, true),
                None => (AppConfig::default(), false),
            },
        };
        cc.egui_ctx
            .set_zoom_factor(config.settings.ui_scale.clamp(0.7, 2.0));
        let sup = Supervisor::new(config.settings.clone());
        sup.live.set_egui(cc.egui_ctx.clone());
        for agent in &config.agents {
            sup.hive
                .upsert_peer(crate::runtime::agent::peer_from(agent));
            if agent.autostart && !agent.token.trim().is_empty() {
                sup.start(agent);
            }
        }
        let page = match config.agents.first() {
            Some(_) => Page::Dashboard,
            None => Page::Dashboard,
        };
        let mut app = Self {
            sup,
            config,
            config_path,
            page,
            agent_tab: AgentTab::Overview,
            dirty_agents: HashSet::new(),
            settings_dirty: false,
            dirty_since: None,
            toasts: Vec::new(),
            show_token: false,
            console_input: String::new(),
            emoji_new_tool: String::new(),
            eyes_agent: None,
            eyes_guild: None,
            eyes_channel: None,
            eyes_reply_to: None,
            eyes_input: String::new(),
            memory_query: String::new(),
            new_memory: String::new(),
            buffers: HashMap::new(),
            knowledge_guild: None,
            knowledge_tab: KnowledgeTab::Culture,
            confirm_delete: None,
            feed_kind: None,
            editing_memory: None,
            editing_task: None,
            selected_mcp: None,
        };
        if migrated {
            app.toast("Imported your agent from the previous version", false);
            app.dirty_since = Some(Instant::now());
        }
        app
    }

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast {
            at: Instant::now(),
            text: text.into(),
            error,
        });
    }

    pub fn mark_agent_dirty(&mut self, id: &str) {
        self.dirty_agents.insert(id.to_owned());
        self.dirty_since = Some(Instant::now());
    }

    pub fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.dirty_since = Some(Instant::now());
    }

    pub fn agent_index(&self, id: &str) -> Option<usize> {
        self.config.agents.iter().position(|a| a.id == id)
    }

    /// Edit an agent through a closure on a copy; writes back and marks
    /// dirty only when something changed.
    pub fn edit_agent(&mut self, id: &str, edit: impl FnOnce(&mut Self, &mut AgentProfile)) {
        let Some(index) = self.agent_index(id) else {
            return;
        };
        let mut profile = self.config.agents[index].clone();
        edit(self, &mut profile);
        if let Some(index) = self.agent_index(id) {
            if self.config.agents[index] != profile {
                self.config.agents[index] = profile;
                self.mark_agent_dirty(id);
            }
        }
    }

    fn flush(&mut self, force: bool) {
        let Some(since) = self.dirty_since else {
            return;
        };
        if !force && since.elapsed() < Duration::from_millis(800) {
            return;
        }
        for id in std::mem::take(&mut self.dirty_agents) {
            if let Some(profile) = self.config.agents.iter().find(|a| a.id == id) {
                self.sup.update(profile);
                self.sup.hive.update_peer(&id, |p| {
                    p.name = profile.name.clone();
                    p.aliases = profile.aliases.clone();
                    p.specialty = profile.specialty.clone();
                });
            }
        }
        if std::mem::take(&mut self.settings_dirty) {
            self.sup.update_settings(self.config.settings.clone());
        }
        if let Err(error) = config::save(&self.config_path, &self.config) {
            self.toast(format!("Could not save settings: {error}"), true);
        }
        self.dirty_since = None;
    }

    pub fn new_agent(&mut self) {
        const NAMES: [&str; 10] = [
            "Nova", "Vex", "Echo", "Sage", "Pixel", "Atlas", "Juno", "Orion", "Wren", "Kai",
        ];
        const ACCENTS: [[u8; 3]; 8] = [
            [124, 108, 255],
            [62, 207, 142],
            [78, 161, 255],
            [245, 183, 79],
            [240, 97, 109],
            [196, 120, 255],
            [56, 210, 220],
            [255, 140, 90],
        ];
        let taken: HashSet<String> = self
            .config
            .agents
            .iter()
            .map(|a| a.name.to_lowercase())
            .collect();
        let name = NAMES
            .iter()
            .find(|n| !taken.contains(&n.to_lowercase()))
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("Agent {}", self.config.agents.len() + 1));
        let mut profile = AgentProfile {
            name,
            accent: ACCENTS[self.config.agents.len() % ACCENTS.len()],
            ..AgentProfile::default()
        };
        // New agents reuse the first agent's model setup — usually the
        // same local server.
        if let Some(first) = self.config.agents.first() {
            profile.model = first.model.clone();
        }
        let id = profile.id.clone();
        self.sup
            .hive
            .upsert_peer(crate::runtime::agent::peer_from(&profile));
        self.config.agents.push(profile);
        self.mark_agent_dirty(&id);
        self.page = Page::Agent(id);
        self.agent_tab = AgentTab::Overview;
    }

    pub fn delete_agent(&mut self, id: &str) {
        self.sup.remove(id);
        self.config.agents.retain(|a| a.id != id);
        self.dirty_since = Some(Instant::now());
        self.page = Page::Dashboard;
        self.toast(
            "Agent removed (its memory files stay in the data folder)",
            false,
        );
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .exact_width(236.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::symmetric(12, 14))
                    .stroke(Stroke::new(1.0_f32, theme::BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("✚").size(22.0).color(ACCENT));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new("Axis Agents").size(17.0).strong().color(TEXT));
                        ui.label(
                            RichText::new("autonomous Discord team")
                                .size(11.0)
                                .color(WEAK),
                        );
                    });
                });
                ui.add_space(14.0);
                if nav_item(ui, "🏠  Dashboard", self.page == Page::Dashboard) {
                    self.page = Page::Dashboard;
                }
                ui.add_space(12.0);
                ui.label(RichText::new("AGENTS").size(11.0).color(theme::FAINT));
                ui.add_space(2.0);
                let agents: Vec<(String, String, [u8; 3])> = self
                    .config
                    .agents
                    .iter()
                    .map(|a| (a.id.clone(), a.name.clone(), a.accent))
                    .collect();
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 150.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (id, name, accent) in agents {
                            let live = self.sup.live.snapshot(&id);
                            let selected = self.page == Page::Agent(id.clone());
                            let response = egui::Frame::new()
                                .fill(if selected {
                                    theme::dim(ACCENT, 45)
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::symmetric(8, 6))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        avatar(ui, &name, theme::rgb(accent), 28.0);
                                        ui.vertical(|ui| {
                                            ui.spacing_mut().item_spacing.y = 0.0;
                                            ui.label(RichText::new(&name).color(TEXT).strong());
                                            let sub = if !live.thoughts.is_empty() {
                                                "💭 thinking…".to_owned()
                                            } else if live.paused {
                                                "paused".to_owned()
                                            } else {
                                                live.status.label()
                                            };
                                            ui.label(
                                                RichText::new(crate::llm::truncate(&sub, 26))
                                                    .size(11.0)
                                                    .color(WEAK),
                                            );
                                        });
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                status_dot(
                                                    ui,
                                                    status_color(&live.status, live.paused),
                                                    4.5,
                                                );
                                            },
                                        );
                                    });
                                })
                                .response
                                .interact(egui::Sense::click());
                            if response.clicked() {
                                self.page = Page::Agent(id.clone());
                            }
                            if response.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                        }
                        ui.add_space(4.0);
                        if ui
                            .add(
                                egui::Button::new(RichText::new("➕  New agent").color(WEAK))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            self.new_agent();
                        }
                    });
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    let online = self
                        .config
                        .agents
                        .iter()
                        .filter(|a| self.sup.is_running(&a.id))
                        .count();
                    let tokens: u64 = self
                        .config
                        .agents
                        .iter()
                        .map(|a| {
                            let s = self.sup.live.snapshot(&a.id).stats;
                            s.tokens_in + s.tokens_out
                        })
                        .sum();
                    ui.label(
                        RichText::new(format!(
                            "{online}/{} online · {} tokens · v{}",
                            self.config.agents.len(),
                            widgets::compact_number(tokens),
                            env!("CARGO_PKG_VERSION")
                        ))
                        .size(11.0)
                        .color(theme::FAINT),
                    );
                    ui.add_space(4.0);
                    if nav_item(ui, "⚙  Settings", self.page == Page::Settings) {
                        self.page = Page::Settings;
                    }
                    if nav_item(ui, "👁  Bot's eyes", self.page == Page::Eyes) {
                        self.page = Page::Eyes;
                    }
                    if nav_item(ui, "📚  Knowledge", self.page == Page::Knowledge) {
                        self.page = Page::Knowledge;
                    }
                });
            });
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        self.toasts
            .retain(|t| t.at.elapsed() < Duration::from_secs(5));
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .show(ctx, |ui| {
                for toast in &self.toasts {
                    let color = if toast.error {
                        theme::RED
                    } else {
                        theme::GREEN
                    };
                    egui::Frame::new()
                        .fill(theme::PANEL)
                        .stroke(Stroke::new(1.0_f32, theme::dim(color, 160)))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&toast.text).color(TEXT));
                        });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn nav_item(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let response = ui.add_sized(
        [ui.available_width(), 32.0],
        egui::Button::new(RichText::new(label).color(if selected { TEXT } else { WEAK }))
            .fill(if selected {
                theme::dim(ACCENT, 45)
            } else {
                egui::Color32::TRANSPARENT
            })
            .corner_radius(CornerRadius::same(8)),
    );
    response.clicked()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let patches: Vec<_> =
            std::mem::take(&mut *self.sup.live.profile_patches.lock().expect("live poisoned"));
        for (id, patch) in patches {
            self.edit_agent(&id, |_, profile| patch(profile));
        }
        self.flush(false);
        self.sidebar(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(26, 20)),
            )
            .show(ctx, |ui| match self.page.clone() {
                Page::Dashboard => dashboard::show(self, ui),
                Page::Agent(id) => {
                    if self.agent_index(&id).is_some() {
                        agent_page::show(self, ui, &id);
                    } else {
                        self.page = Page::Dashboard;
                    }
                }
                Page::Eyes => eyes::show(self, ui),
                Page::Knowledge => knowledge::show(self, ui),
                Page::Settings => settings::show(self, ui),
            });
        self.toasts(ctx);
        // Keep timers and live thinking fresh while agents run.
        let busy = self
            .config
            .agents
            .iter()
            .any(|a| self.sup.is_running(&a.id));
        if busy || self.dirty_since.is_some() {
            ctx.request_repaint_after(Duration::from_millis(400));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush(true);
        self.sup.shutdown();
    }
}
