//! Colors, fonts, and widget styling.

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals};

pub const BG: Color32 = Color32::from_rgb(13, 15, 21);
pub const SIDEBAR: Color32 = Color32::from_rgb(17, 19, 27);
pub const PANEL: Color32 = Color32::from_rgb(20, 23, 32);
pub const CARD: Color32 = Color32::from_rgb(26, 29, 41);
pub const CARD_HOVER: Color32 = Color32::from_rgb(33, 37, 52);
pub const INPUT: Color32 = Color32::from_rgb(15, 17, 24);
pub const BORDER: Color32 = Color32::from_rgb(40, 45, 63);
pub const TEXT: Color32 = Color32::from_rgb(228, 231, 240);
pub const WEAK: Color32 = Color32::from_rgb(139, 145, 166);
pub const FAINT: Color32 = Color32::from_rgb(92, 98, 118);
pub const ACCENT: Color32 = Color32::from_rgb(124, 108, 255);
pub const GREEN: Color32 = Color32::from_rgb(62, 207, 142);
pub const YELLOW: Color32 = Color32::from_rgb(245, 183, 79);
pub const RED: Color32 = Color32::from_rgb(240, 97, 109);
pub const BLUE: Color32 = Color32::from_rgb(78, 161, 255);

pub fn rgb(color: [u8; 3]) -> Color32 {
    Color32::from_rgb(color[0], color[1], color[2])
}

pub fn dim(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto_emoji".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/NotoEmoji.ttf"
        ))),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("noto_emoji".to_owned());
    }
    ctx.set_fonts(fonts);

    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.extreme_bg_color = INPUT;
    visuals.text_edit_bg_color = Some(INPUT);
    visuals.faint_bg_color = CARD;
    visuals.code_bg_color = INPUT;
    visuals.hyperlink_color = BLUE;
    visuals.warn_fg_color = YELLOW;
    visuals.error_fg_color = RED;
    visuals.selection.bg_fill = dim(ACCENT, 110);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    let radius = CornerRadius::same(8);
    visuals.widgets.noninteractive.bg_fill = CARD;
    visuals.widgets.noninteractive.weak_bg_fill = CARD;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.noninteractive.corner_radius = radius;
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(36, 40, 56);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(34, 38, 53);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(46, 51, 72);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(44, 49, 69);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, dim(ACCENT, 160));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, TEXT);
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.active.bg_fill = dim(ACCENT, 140);
    visuals.widgets.active.weak_bg_fill = dim(ACCENT, 120);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.open = visuals.widgets.hovered;
    ctx.set_visuals(visuals);

    ctx.style_mut(|style| {
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Proportional),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        );
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.window_margin = Margin::same(16);
        style.spacing.combo_width = 240.0;
        style.spacing.text_edit_width = 320.0;
        style.spacing.slider_width = 220.0;
        style.spacing.scroll.bar_width = 8.0;
    });
}
