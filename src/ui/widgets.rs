//! Reusable pieces: cards, pills, avatars, switches, form rows.

use egui::{
    Align2, Color32, CornerRadius, FontId, Margin, Response, RichText, Sense, Stroke, Ui, vec2,
};

use super::theme::{self, ACCENT, BORDER, CARD, FAINT, TEXT, WEAK};
use crate::runtime::live::AgentStatus;

pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A card that reacts to hover and returns a click response.
pub fn clickable_card(ui: &mut Ui, accent: Color32, add: impl FnOnce(&mut Ui)) -> Response {
    let id = ui.next_auto_id();
    let hovered = ui.ctx().data(|d| d.get_temp::<bool>(id).unwrap_or(false));
    let response = egui::Frame::new()
        .fill(if hovered { theme::CARD_HOVER } else { CARD })
        .stroke(Stroke::new(
            1.0_f32,
            if hovered {
                theme::dim(accent, 180)
            } else {
                BORDER
            },
        ))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .response
        .interact(Sense::click());
    ui.ctx().data_mut(|d| d.insert_temp(id, response.hovered()));
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(24.0).strong().color(TEXT));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(WEAK));
    }
    ui.add_space(10.0);
}

pub fn section(ui: &mut Ui, title: &str, help: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(16.0).strong().color(TEXT));
    if !help.is_empty() {
        ui.label(RichText::new(help).size(12.5).color(WEAK));
    }
    ui.add_space(4.0);
}

pub fn status_color(status: &AgentStatus, paused: bool) -> Color32 {
    if paused && status.is_running() {
        return theme::YELLOW;
    }
    match status {
        AgentStatus::Online => theme::GREEN,
        AgentStatus::Starting | AgentStatus::Reconnecting => theme::YELLOW,
        AgentStatus::Error(_) => theme::RED,
        AgentStatus::Stopped => FAINT,
    }
}

pub fn status_dot(ui: &mut Ui, color: Color32, radius: f32) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(radius * 2.0 + 2.0, radius * 2.0 + 2.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}

pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    egui::Frame::new()
        .fill(theme::dim(color, 38))
        .stroke(Stroke::new(1.0_f32, theme::dim(color, 110)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(color));
        })
        .response
}

pub fn avatar(ui: &mut Ui, name: &str, accent: Color32, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let painter = ui.painter();
    painter.circle_filled(rect.center(), size / 2.0, theme::dim(accent, 60));
    painter.circle_stroke(
        rect.center(),
        size / 2.0 - 0.5,
        Stroke::new(1.5_f32, accent),
    );
    let initial: String = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        initial,
        FontId::proportional(size * 0.45),
        Color32::WHITE,
    );
    response
}

/// An iOS-style switch.
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let size = vec2(36.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let fill = egui::lerp(
        egui::Rgba::from(theme::dim(FAINT, 120))..=egui::Rgba::from(ACCENT),
        t,
    );
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), Color32::from(fill));
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
    painter.circle_filled(egui::pos2(x, rect.center().y), 7.0, Color32::WHITE);
    response
}

/// A switch with a label and a help line; returns true when toggled.
pub fn toggle_row(ui: &mut Ui, on: &mut bool, label: &str, help: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        changed = switch(ui, on).changed();
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new(label).color(TEXT));
            if !help.is_empty() {
                ui.label(RichText::new(help).size(12.0).color(WEAK));
            }
        });
    });
    ui.add_space(2.0);
    changed
}

/// A labeled form row: label (+help) on the left column, control right.
pub fn form_row<R>(ui: &mut Ui, label: &str, help: &str, control: impl FnOnce(&mut Ui) -> R) -> R {
    let mut out = None;
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            vec2(190.0, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(190.0);
                ui.add_space(5.0);
                ui.label(RichText::new(label).color(TEXT));
                if !help.is_empty() {
                    ui.label(RichText::new(help).size(11.5).color(FAINT));
                }
            },
        );
        ui.vertical(|ui| out = Some(control(ui)));
    });
    ui.add_space(4.0);
    out.expect("control ran")
}

pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE).strong())
            .fill(ACCENT)
            .corner_radius(CornerRadius::same(8)),
    )
}

pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(theme::RED))
            .fill(theme::dim(theme::RED, 30))
            .stroke(Stroke::new(1.0_f32, theme::dim(theme::RED, 120)))
            .corner_radius(CornerRadius::same(8)),
    )
}

pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(text).color(TEXT)).corner_radius(CornerRadius::same(8)))
}

pub fn stat(ui: &mut Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(value).size(20.0).strong().color(TEXT));
        ui.label(RichText::new(label).size(11.5).color(WEAK));
    });
}

pub fn tab_bar<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, tabs: &[(T, &str)]) {
    ui.horizontal(|ui| {
        for (tab, label) in tabs {
            let selected = *current == *tab;
            let text = RichText::new(*label).color(if selected { TEXT } else { WEAK });
            let response = ui.add(
                egui::Button::new(text)
                    .fill(if selected {
                        theme::dim(ACCENT, 60)
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(if selected {
                        Stroke::new(1.0_f32, theme::dim(ACCENT, 160))
                    } else {
                        Stroke::NONE
                    })
                    .corner_radius(CornerRadius::same(8)),
            );
            if response.clicked() {
                *current = *tab;
            }
        }
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(6.0);
}

pub fn compact_number(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1_000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

pub fn ago(ms: i64) -> String {
    let secs = ((crate::memory::now_ms() - ms) / 1000).max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Open a URL or folder with the system's default handler.
pub fn open_external(target: &str) {
    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", target])
        .spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(target).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(target).spawn();
    if let Err(error) = result {
        tracing::warn!(%error, "could not open {target}");
    }
}
