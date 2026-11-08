//! Rank cards: every part can be styled — layout, fonts, colors per
//! element, which parts show and where, avatar shape, the bar, and the
//! background (color, gradient, image with opacity/blur/brightness).

use std::sync::OnceLock;

use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use image::{DynamicImage, Rgba, RgbaImage, imageops};

use super::images::{blend, decode, download, ellipse, encode, fill_rect, parse_color};
use crate::memory::RankCard;

/// What goes on a rank card.
pub struct RankCardData {
    pub name: String,
    pub avatar_url: Option<String>,
    pub level: u32,
    pub rank: usize,
    pub xp_into: u64,
    pub xp_needed: u64,
    pub total_xp: u64,
}

/// Fonts by name (Google Fonts, downloaded once), or any .ttf/.otf URL.
pub const FONTS: [(&str, &str); 16] = [
    ("default", ""),
    ("mono", ""),
    ("inter", "inter/Inter%5Bopsz,wght%5D.ttf"),
    ("poppins", "poppins/Poppins-Bold.ttf"),
    ("poppins light", "poppins/Poppins-Regular.ttf"),
    ("montserrat", "montserrat/Montserrat%5Bwght%5D.ttf"),
    ("bebas neue", "bebasneue/BebasNeue-Regular.ttf"),
    ("oswald", "oswald/Oswald%5Bwght%5D.ttf"),
    ("orbitron", "orbitron/Orbitron%5Bwght%5D.ttf"),
    ("press start 2p", "pressstart2p/PressStart2P-Regular.ttf"),
    ("pacifico", "pacifico/Pacifico-Regular.ttf"),
    ("lobster", "lobster/Lobster-Regular.ttf"),
    ("nunito", "nunito/Nunito%5Bwght%5D.ttf"),
    (
        "playfair display",
        "playfairdisplay/PlayfairDisplay%5Bwght%5D.ttf",
    ),
    ("rubik", "rubik/Rubik%5Bwght%5D.ttf"),
    ("roboto mono", "robotomono/RobotoMono%5Bwght%5D.ttf"),
];

pub const LAYOUTS: [&str; 4] = ["classic", "mirrored", "centered", "compact"];
pub const STYLES: [&str; 4] = ["classic", "minimal", "bold", "glass"];

async fn load_font(choice: &str) -> FontArc {
    static DEFAULT: OnceLock<FontArc> = OnceLock::new();
    let fallback = || {
        DEFAULT
            .get_or_init(|| {
                FontArc::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font")
            })
            .clone()
    };
    let choice = choice.trim().to_lowercase();
    if choice.is_empty() || choice == "default" {
        return fallback();
    }
    if choice == "mono" {
        return FontArc::try_from_slice(epaint_default_fonts::HACK_REGULAR)
            .unwrap_or_else(|_| fallback());
    }
    let url = if choice.starts_with("http") {
        choice.clone()
    } else {
        match FONTS
            .iter()
            .find(|(name, _)| *name == choice || name.replace(' ', "") == choice.replace(' ', ""))
        {
            Some((_, path)) if !path.is_empty() => {
                format!("https://raw.githubusercontent.com/google/fonts/main/ofl/{path}")
            }
            _ => return fallback(),
        }
    };
    // Cached on disk by URL.
    let dir = crate::paths::data_dir().join("fonts");
    let file = dir.join(format!("{:016x}.ttf", fnv(&url)));
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(_) => match download(&url, false).await {
            Ok(bytes) => {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(&file, &bytes);
                bytes
            }
            Err(_) => return fallback(),
        },
    };
    FontArc::try_from_vec(bytes).unwrap_or_else(|_| fallback())
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

fn width(font: &FontArc, text: &str, size: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut previous = None;
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(p) = previous {
            w += scaled.kern(p, id);
        }
        w += scaled.h_advance(id);
        previous = Some(id);
    }
    w
}

fn glyphs(
    canvas: &mut RgbaImage,
    font: &FontArc,
    text: &str,
    size: f32,
    x: f32,
    y: f32,
    color: Rgba<u8>,
) {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let baseline = y + scaled.ascent();
    let mut caret = x;
    let mut previous = None;
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(p) = previous {
            caret += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(scale, point(caret, baseline));
        caret += scaled.h_advance(id);
        previous = Some(id);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                blend(
                    canvas,
                    bounds.min.x as i64 + gx as i64,
                    bounds.min.y as i64 + gy as i64,
                    color,
                    coverage,
                );
            });
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Right,
    Center,
}

struct Pen<'a> {
    font: &'a FontArc,
    passes: &'a [(f32, f32)],
    scale: f32,
}

impl Pen<'_> {
    fn width(&self, text: &str, size: f32) -> f32 {
        width(self.font, text, size * self.scale)
    }

    /// Draw text anchored at x (left/right/center); returns its width.
    fn text(
        &self,
        canvas: &mut RgbaImage,
        text: &str,
        size: f32,
        x: f32,
        y: f32,
        align: Align,
        color: Rgba<u8>,
    ) -> f32 {
        let size = size * self.scale;
        let w = width(self.font, text, size);
        let x = match align {
            Align::Left => x,
            Align::Right => x - w,
            Align::Center => x - w / 2.0,
        };
        for (dx, dy) in self.passes {
            glyphs(canvas, self.font, text, size, x + dx, y + dy, color);
        }
        w
    }
}

fn bar(canvas: &mut RgbaImage, x: f32, y: f32, w: f32, h: f32, color: Rgba<u8>, round: bool) {
    if !round {
        fill_rect(canvas, x, y, w, h, color);
        return;
    }
    let r = h / 2.0;
    let (ax, bx, cy) = (x + r, (x + w - r).max(x + r), y + r);
    for py in y.floor() as i64..=(y + h).ceil() as i64 {
        for px in x.floor() as i64..=(x + w).ceil() as i64 {
            let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
            let nearest = fx.clamp(ax, bx);
            let d = ((fx - nearest).powi(2) + (fy - cy).powi(2)).sqrt();
            blend(canvas, px, py, color, r - d + 0.5);
        }
    }
}

fn short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn color_or(value: &str, fallback: Rgba<u8>) -> Rgba<u8> {
    if value.trim().is_empty() {
        fallback
    } else {
        parse_color(value)
    }
}

fn with_alpha(mut c: Rgba<u8>, alpha: u8) -> Rgba<u8> {
    c.0[3] = alpha;
    c
}

/// Where things go, per layout.
struct Frame {
    w: u32,
    h: u32,
    avatar: (f32, f32, f32),
    left: f32,
    right: f32,
    centered: bool,
    top_y: f32,
    name_y: f32,
    xp_y: f32,
    bar_y: f32,
    name_size: f32,
    /// Size of the rank/level row relative to the classic layout.
    row_scale: f32,
}

fn frame(layout: &str, bar_h: f32) -> Frame {
    match layout {
        "mirrored" => Frame {
            w: 934,
            h: 282,
            avatar: (934.0 - 141.0, 141.0, 92.0),
            left: 50.0,
            right: 934.0 - 270.0,
            centered: false,
            top_y: 38.0,
            name_y: 112.0,
            xp_y: 124.0,
            bar_y: 180.0,
            name_size: 46.0,
            row_scale: 1.0,
        },
        // Stacked: avatar, name, rank/level, XP, bar.
        "centered" => Frame {
            w: 934,
            h: 440,
            avatar: (467.0, 98.0, 68.0),
            left: 110.0,
            right: 824.0,
            centered: true,
            top_y: 238.0,
            name_y: 180.0,
            xp_y: 304.0,
            bar_y: 346.0,
            name_size: 46.0,
            row_scale: 0.75,
        },
        "compact" => Frame {
            w: 934,
            h: 170,
            avatar: (90.0, 85.0, 58.0),
            left: 175.0,
            right: 900.0,
            centered: false,
            top_y: 18.0,
            name_y: 46.0,
            xp_y: 62.0,
            bar_y: 170.0 - 30.0 - bar_h.min(28.0),
            name_size: 36.0,
            row_scale: 0.6,
        },
        _ => Frame {
            w: 934,
            h: 282,
            avatar: (141.0, 141.0, 92.0),
            left: 270.0,
            right: 884.0,
            centered: false,
            top_y: 38.0,
            name_y: 112.0,
            xp_y: 124.0,
            bar_y: 180.0,
            name_size: 46.0,
            row_scale: 1.0,
        },
    }
}

/// Draw a rank card (PNG).
pub async fn render(card: &RankCard, data: &RankCardData) -> Result<Vec<u8>, String> {
    let layout = if LAYOUTS.contains(&card.layout.as_str()) {
        card.layout.as_str()
    } else {
        "classic"
    };
    let style = card.style.to_lowercase();
    let bar_h = if card.bar_height > 0 {
        card.bar_height as f32
    } else if style == "minimal" {
        14.0
    } else {
        36.0
    };
    let f = frame(layout, bar_h);
    let (w, h) = (f.w, f.h);

    let background = parse_color(&card.background);
    let accent = parse_color(&card.accent);
    let text = parse_color(&card.text);
    let muted = parse_color(&card.muted);
    let mut canvas = RgbaImage::from_pixel(w, h, background);

    // Gradient.
    if !card.background2.trim().is_empty() {
        let end = parse_color(&card.background2);
        for (x, y, pixel) in canvas.enumerate_pixels_mut() {
            let t = match card.gradient.as_str() {
                "vertical" => y as f32 / h as f32,
                "diagonal" => (x as f32 / w as f32 + y as f32 / h as f32) / 2.0,
                _ => x as f32 / w as f32,
            };
            for c in 0..3 {
                pixel.0[c] = (background.0[c] as f32 * (1.0 - t) + end.0[c] as f32 * t) as u8;
            }
        }
    }
    // Background image: cover-fit, then blur, brightness, opacity, darkening.
    if !card.background_url.trim().is_empty() {
        if let Ok(image) = download(card.background_url.trim(), false)
            .await
            .and_then(|b| decode(&b))
        {
            let mut image = image.resize_to_fill(w, h, imageops::FilterType::Triangle);
            if card.blur > 0 {
                image = image.blur(card.blur.min(40) as f32);
            }
            if card.image_brightness != 0 {
                image = image.brighten(card.image_brightness.clamp(-100, 100) * 2);
            }
            let opacity = if card.image_opacity == 0 {
                100
            } else {
                card.image_opacity.min(100)
            } as f32
                / 100.0;
            let image = image.to_rgba8();
            for (x, y, pixel) in image.enumerate_pixels() {
                blend(
                    &mut canvas,
                    x as i64,
                    y as i64,
                    *pixel,
                    opacity * pixel.0[3] as f32 / 255.0,
                );
            }
            if card.overlay > 0 {
                fill_rect(
                    &mut canvas,
                    0.0,
                    0.0,
                    w as f32,
                    h as f32,
                    Rgba([0, 0, 0, (card.overlay.min(100) as f32 * 2.55) as u8]),
                );
            }
        }
    }
    // Decoration.
    let panel_alpha = if card.panel_opacity == 0 {
        70
    } else {
        (card.panel_opacity.min(100) as f32 * 2.55) as u8
    };
    match style.as_str() {
        "bold" => fill_rect(&mut canvas, 0.0, 0.0, 14.0, h as f32, accent),
        "glass" => {
            fill_rect(
                &mut canvas,
                20.0,
                20.0,
                w as f32 - 40.0,
                h as f32 - 40.0,
                Rgba([255, 255, 255, panel_alpha / 3]),
            );
            let edge = with_alpha(text, 60);
            fill_rect(&mut canvas, 20.0, 20.0, w as f32 - 40.0, 1.5, edge);
            fill_rect(
                &mut canvas,
                20.0,
                h as f32 - 21.5,
                w as f32 - 40.0,
                1.5,
                edge,
            );
        }
        "minimal" => {}
        _ => fill_rect(
            &mut canvas,
            20.0,
            20.0,
            w as f32 - 40.0,
            h as f32 - 40.0,
            Rgba([0, 0, 0, panel_alpha]),
        ),
    }

    // Avatar.
    let (cx, cy, r) = f.avatar;
    if card.show_avatar {
        let ring = color_or(&card.ring_color, accent);
        let shape = card.avatar_shape.to_lowercase();
        let inside = |dx: f32, dy: f32, rr: f32| -> f32 {
            match shape.as_str() {
                "square" => (rr - dx.abs().max(dy.abs()) + 0.5).clamp(0.0, 1.0),
                "rounded" => {
                    let corner = rr * 0.3;
                    let qx = (dx.abs() - (rr - corner)).max(0.0);
                    let qy = (dy.abs() - (rr - corner)).max(0.0);
                    let outside = (qx * qx + qy * qy).sqrt() - corner;
                    let edge = dx.abs().max(dy.abs()) - rr;
                    (0.5 - outside.max(edge)).clamp(0.0, 1.0)
                }
                _ => (rr - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0),
            }
        };
        if card.avatar_ring {
            let rr = r + 7.0;
            for py in (cy - rr - 1.0) as i64..=(cy + rr + 1.0) as i64 {
                for px in (cx - rr - 1.0) as i64..=(cx + rr + 1.0) as i64 {
                    let c = inside(px as f32 + 0.5 - cx, py as f32 + 0.5 - cy, rr);
                    if c > 0.0 {
                        blend(&mut canvas, px, py, ring, c);
                    }
                }
            }
        }
        let avatar = match &data.avatar_url {
            Some(url) => download(url, false).await.and_then(|b| decode(&b)).ok(),
            None => None,
        };
        let size = (r * 2.0) as u32;
        let avatar = avatar
            .map(|a| {
                a.resize_to_fill(size, size, imageops::FilterType::Triangle)
                    .to_rgba8()
            })
            .unwrap_or_else(|| RgbaImage::from_pixel(size, size, muted));
        for (x, y, pixel) in avatar.enumerate_pixels() {
            let c = inside(x as f32 + 0.5 - r, y as f32 + 0.5 - r, r);
            if c > 0.0 {
                blend(
                    &mut canvas,
                    (cx - r) as i64 + x as i64,
                    (cy - r) as i64 + y as i64,
                    *pixel,
                    c * pixel.0[3] as f32 / 255.0,
                );
            }
        }
    }

    // Text.
    let font = load_font(&card.font).await;
    let passes: &[(f32, f32)] = match card.font_weight.as_str() {
        "light" => &[(0.0, 0.0)],
        "bold" => &[(0.0, 0.0), (0.8, 0.0), (0.0, 0.8), (0.8, 0.8), (0.4, 0.4)],
        _ => &[(0.0, 0.0), (0.6, 0.0), (0.0, 0.6)],
    };
    let pen = Pen {
        font: &font,
        passes,
        scale: card.text_scale.clamp(0.5, 2.0).max(0.01),
    };
    let name_color = color_or(&card.name_color, text);
    let rank_color = color_or(&card.rank_color, text);
    let level_color = color_or(&card.level_color, accent);
    let xp_color = color_or(&card.xp_color, muted);
    let rank_label = if card.rank_label.is_empty() {
        "RANK"
    } else {
        &card.rank_label
    };
    let level_label = if card.level_label.is_empty() {
        "LEVEL"
    } else {
        &card.level_label
    };
    let total_text = format!("{} XP total", short(data.total_xp));
    // Stacked layouts have no room between the XP line and the bar.
    let total_position = match card.total_position.as_str() {
        "above_bar" if f.centered => "with_xp",
        other => other,
    };
    let mut xp_text = format!("{} / {} XP", short(data.xp_into), short(data.xp_needed));
    if card.show_total && total_position == "with_xp" {
        xp_text.push_str(&format!(" · {}", short(data.total_xp)));
    }

    // Rank and level: right-aligned (or centered) row.
    let mut parts: Vec<(String, f32, Rgba<u8>)> = Vec::new();
    if card.show_rank {
        parts.push((format!("{rank_label} "), 24.0, muted));
        parts.push((format!("#{}", data.rank), 60.0, rank_color));
    }
    if card.show_level {
        parts.push((format!("   {level_label} "), 24.0, muted));
        parts.push((data.level.to_string(), 60.0, level_color));
    }
    let compact = layout == "compact";
    let shrink = f.row_scale;
    let row_width: f32 = parts.iter().map(|(t, s, _)| pen.width(t, s * shrink)).sum();
    let mut x = if f.centered {
        w as f32 / 2.0 - row_width / 2.0
    } else {
        f.right - row_width
    };
    let row_y = f.top_y;
    for (t, s, c) in &parts {
        let size = s * shrink;
        // Labels sit on the numbers' baseline.
        let y = if *s < 30.0 {
            row_y + (60.0 - 24.0) * shrink * 0.9 * pen.scale
        } else {
            row_y
        };
        x += pen.text(&mut canvas, t, size, x, y, Align::Left, *c);
    }

    // Name, shortened to fit.
    let mut name = data.name.clone();
    let room = if f.centered {
        (f.right - f.left).max(200.0)
    } else if compact {
        f.right - f.left - row_width - 30.0
    } else {
        (f.right - f.left - 260.0).max(200.0)
    };
    while pen.width(&name, f.name_size) > room && name.chars().count() > 3 {
        name.pop();
        name = format!("{}…", name.trim_end_matches('…'));
    }
    let name_x = if f.centered { w as f32 / 2.0 } else { f.left };
    pen.text(
        &mut canvas,
        &name,
        f.name_size,
        name_x,
        if compact { f.top_y } else { f.name_y },
        if f.centered {
            Align::Center
        } else {
            Align::Left
        },
        name_color,
    );

    // XP text and total.
    if card.show_xp {
        let (xx, align) = if f.centered {
            (w as f32 / 2.0, Align::Center)
        } else {
            (f.right, Align::Right)
        };
        let y = if compact {
            f.bar_y - 30.0 * pen.scale
        } else if f.centered {
            f.xp_y
        } else {
            f.xp_y
        };
        pen.text(
            &mut canvas,
            &xp_text,
            26.0 * shrink.max(0.8),
            xx,
            y,
            align,
            xp_color,
        );
    }
    if card.show_total && total_position != "with_xp" {
        let (tx, ty, align) = match total_position {
            "above_bar" => (
                if f.centered { w as f32 / 2.0 } else { f.left },
                f.bar_y - 34.0 * pen.scale,
                if f.centered {
                    Align::Center
                } else {
                    Align::Left
                },
            ),
            "top" => (
                if f.centered { f.left } else { f.left },
                if f.centered { 24.0 } else { 26.0 },
                Align::Left,
            ),
            _ => (
                if f.centered { w as f32 / 2.0 } else { f.left },
                f.bar_y + bar_h + 10.0,
                if f.centered {
                    Align::Center
                } else {
                    Align::Left
                },
            ),
        };
        if ty + 20.0 < h as f32 {
            pen.text(&mut canvas, &total_text, 20.0, tx, ty, align, xp_color);
        }
    }

    // Progress bar.
    if card.show_bar {
        let round = card.bar_shape != "square";
        let track = color_or(&card.bar_track, with_alpha(muted, 70));
        let fill = color_or(&card.bar_color, accent);
        let (bx, bw) = (f.left, f.right - f.left);
        bar(&mut canvas, bx, f.bar_y, bw, bar_h, track, round);
        let fraction = if data.xp_needed == 0 {
            0.0
        } else {
            (data.xp_into as f32 / data.xp_needed as f32).clamp(0.0, 1.0)
        };
        if fraction > 0.0 {
            bar(
                &mut canvas,
                bx,
                f.bar_y,
                (bw * fraction).max(if round { bar_h } else { 2.0 }),
                bar_h,
                fill,
                round,
            );
        }
    }

    // Rounded card corners (transparent).
    if card.corner_radius > 0 {
        let rr = card.corner_radius.min(h / 2) as f32;
        for (x, y, pixel) in canvas.enumerate_pixels_mut() {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let qx = (rr - fx).max(fx - (w as f32 - rr)).max(0.0);
            let qy = (rr - fy).max(fy - (h as f32 - rr)).max(0.0);
            if qx > 0.0 && qy > 0.0 {
                let coverage = (rr - (qx * qx + qy * qy).sqrt() + 0.5).clamp(0.0, 1.0);
                pixel.0[3] = (pixel.0[3] as f32 * coverage) as u8;
            }
        }
    }
    let _ = ellipse;
    encode(DynamicImage::ImageRgba8(canvas))
}

/// Look at a rendered card with the vision model, so the agent can check
/// its own design.
pub async fn look(
    core: &crate::runtime::agent::AgentCore,
    png: &[u8],
    question: &str,
) -> Result<String, String> {
    let seen = crate::runtime::vision::SeenImage {
        label: "the rank card you just designed".to_owned(),
        data_uri: crate::runtime::vision::prepare(png).ok_or("couldn't prepare the image")?,
    };
    let profile = core.profile();
    let model = if profile.model.vision_model.trim().is_empty() {
        profile.model.model.clone()
    } else {
        profile.model.vision_model.trim().to_owned()
    };
    crate::runtime::vision::describe(core, &model, &seen, question).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_layout_and_style_renders() {
        for layout in LAYOUTS {
            for style in STYLES {
                let card = RankCard {
                    layout: layout.into(),
                    style: style.into(),
                    background2: "#000000".into(),
                    avatar_shape: "rounded".into(),
                    total_position: "above_bar".into(),
                    corner_radius: 24,
                    ..Default::default()
                };
                let data = RankCardData {
                    name: "uvx".into(),
                    avatar_url: None,
                    level: 3,
                    rank: 1,
                    xp_into: 40,
                    xp_needed: 255,
                    total_xp: 500,
                };
                let png = render(&card, &data).await.unwrap();
                assert!(image::load_from_memory(&png).is_ok(), "{layout}/{style}");
            }
        }
    }
}
