//! Image tools: profile pictures, web image search, drawing (boxes,
//! shapes, arrows, text), charts, local editing (filters, crops, meme
//! captions…), and looking at images.
//!
//! Images live in a per-reply workspace ([`Media`]) under short names
//! ("image1", "image2", …) so the model can chain steps — load an avatar,
//! caption it, rotate it — and only the final results are uploaded with
//! its reply.

use std::io::Cursor;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use image::{DynamicImage, GenericImageView, ImageFormat, Rgba, RgbaImage, imageops};
use serde_json::Value;

use super::{ToolCtx, arg, number, required};
use crate::runtime::live::FeedKind;
use crate::runtime::vision::ImageSource;

const MAX_SIDE: u32 = 4096;
const MAX_UPLOAD: usize = 7_500_000;

/// The images produced or loaded while answering one message.
#[derive(Debug, Default)]
pub struct Media {
    images: Vec<(String, Vec<u8>)>,
    attach: Vec<String>,
    /// Latest image per purpose ("rank card preview"): earlier drafts
    /// aren't attached.
    latest: std::collections::HashMap<String, String>,
    /// Other files to upload, by their real file name.
    named: Vec<(String, Vec<u8>)>,
}

impl Media {
    pub fn add(&mut self, png: Vec<u8>, attach: bool) -> String {
        let name = format!("image{}", self.images.len() + 1);
        self.images.push((name.clone(), png));
        if attach {
            self.attach.push(name.clone());
        }
        name
    }

    /// Add an attached image that replaces the previous one for the same
    /// purpose (only the final draft is posted).
    pub fn add_replacing(&mut self, purpose: &str, png: Vec<u8>) -> String {
        let name = self.add(png, true);
        if let Some(old) = self.latest.insert(purpose.to_owned(), name.clone()) {
            self.attach.retain(|n| *n != old);
        }
        name
    }

    fn names(&self) -> Vec<String> {
        self.images.iter().map(|(n, _)| n.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Vec<u8>> {
        self.images.iter().find(|(n, _)| n == name).map(|(_, b)| b)
    }

    /// Replace an attached source by its edited result.
    fn supersede(&mut self, source: &str, result: &str) {
        let was_attached = self.attach.iter().any(|n| n == source);
        self.attach.retain(|n| n != source);
        if was_attached && !self.attach.iter().any(|n| n == result) {
            self.attach.push(result.to_owned());
        }
    }

    pub fn attach(&mut self, name: &str) {
        if !self.attach.iter().any(|n| n == name) {
            self.attach.push(name.to_owned());
        }
    }

    /// Attach a file under its own name (replacing one of the same name).
    pub fn add_file(&mut self, filename: &str, bytes: Vec<u8>) {
        self.named.retain(|(n, _)| n != filename);
        self.named.push((filename.to_owned(), bytes));
    }

    /// The files to upload with the reply.
    pub fn files(&self) -> Vec<(String, Vec<u8>)> {
        self.named
            .iter()
            .cloned()
            .chain(self.files_images())
            .take(10)
            .collect()
    }

    fn files_images(&self) -> Vec<(String, Vec<u8>)> {
        self.attach
            .iter()
            .filter_map(|name| {
                self.get(name).map(|bytes| {
                    let ext = if bytes.starts_with(&[0xFF, 0xD8]) {
                        "jpg"
                    } else {
                        "png"
                    };
                    (format!("{name}.{ext}"), bytes.clone())
                })
            })
            .take(10)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

pub(crate) fn decode(bytes: &[u8]) -> Result<DynamicImage, String> {
    image::load_from_memory(bytes).map_err(|e| format!("that isn't an image I can read ({e})"))
}

/// Encode as PNG (or JPEG when PNG would be too large), keeping the
/// result small enough to upload.
pub(crate) fn encode(mut image: DynamicImage) -> Result<Vec<u8>, String> {
    let (w, h) = image.dimensions();
    if w.max(h) > MAX_SIDE {
        image = image.resize(MAX_SIDE, MAX_SIDE, imageops::FilterType::Triangle);
    }
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    if png.len() <= MAX_UPLOAD {
        return Ok(png);
    }
    let mut jpeg = Vec::new();
    DynamicImage::ImageRgb8(image.to_rgb8())
        .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    Ok(jpeg)
}

pub(crate) async fn download(url: &str, allow_private: bool) -> Result<Vec<u8>, String> {
    super::web::fetch_bytes(url, allow_private, 20_000_000).await
}

// ---------------------------------------------------------------------------
// Avatars
// ---------------------------------------------------------------------------

pub fn avatar_url(
    guild_id: Option<u64>,
    user_id: u64,
    avatar: Option<&str>,
    guild_avatar: Option<&str>,
) -> String {
    match (guild_id, guild_avatar, avatar) {
        (Some(guild), Some(hash), _) => format!(
            "https://cdn.discordapp.com/guilds/{guild}/users/{user_id}/avatars/{hash}.png?size=1024"
        ),
        (_, _, Some(hash)) => {
            format!("https://cdn.discordapp.com/avatars/{user_id}/{hash}.png?size=1024")
        }
        _ => format!(
            "https://cdn.discordapp.com/embed/avatars/{}.png",
            (user_id >> 22) % 6
        ),
    }
}

/// Find an image the model refers to among the images around the
/// conversation: by exact name, by part of the name, or by "this".
fn find_input<'a>(inputs: &'a [ImageSource], wanted: &str) -> Option<&'a ImageSource> {
    let wanted = wanted.trim().trim_matches(['"', '\'', '`']).to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let generic = matches!(
        wanted.as_str(),
        "attachment"
            | "attached"
            | "image"
            | "the image"
            | "this"
            | "it"
            | "that"
            | "picture"
            | "photo"
            | "screenshot"
            | "the attachment"
            | "this image"
            | "that image"
            | "the picture"
    );
    if generic {
        return inputs.iter().find(|s| s.primary).or_else(|| inputs.first());
    }
    if let Some(n) = wanted
        .strip_prefix("attachment")
        .and_then(|n| n.trim().parse::<usize>().ok())
    {
        return inputs.get(n.saturating_sub(1));
    }
    let stem = |name: &str| {
        name.rsplit_once('.')
            .map(|(stem, _)| stem.to_owned())
            .unwrap_or_else(|| name.to_owned())
    };
    inputs
        .iter()
        .find(|s| s.name.to_lowercase() == wanted)
        .or_else(|| {
            inputs
                .iter()
                .find(|s| stem(&s.name.to_lowercase()) == stem(&wanted))
        })
        .or_else(|| {
            inputs.iter().find(|s| {
                let name = s.name.to_lowercase();
                wanted.len() >= 4 && (name.contains(&wanted) || wanted.contains(&stem(&name)))
            })
        })
        .or_else(|| {
            inputs
                .iter()
                .find(|s| s.url.to_lowercase().contains(&wanted))
        })
}

/// Load an image into the workspace (or find it there).
async fn load(ctx: &ToolCtx, source: &str) -> Result<(String, DynamicImage), String> {
    let source = source.trim();
    let lower = source.to_lowercase();
    // Already in the workspace (image1, image2, …).
    let existing = ctx
        .media
        .lock()
        .expect("media poisoned")
        .get(&lower)
        .cloned();
    if let Some(bytes) = existing {
        return Ok((lower, decode(&bytes)?));
    }
    let bytes = if lower.starts_with("http://") || lower.starts_with("https://") {
        let settings = ctx.core.hive.settings();
        download(
            source,
            settings.allow_private_fetch
                && ctx.requester.level >= crate::config::AccessLevel::Admin,
        )
        .await?
    } else if let Some(input) = find_input(&ctx.inputs, source) {
        download(&input.url, false).await?
    } else if matches!(
        lower.as_str(),
        "server icon" | "server_icon" | "guild icon" | "icon"
    ) {
        let guild_id = ctx.require_guild()?;
        let icon = ctx
            .core
            .world
            .with_guild(guild_id, |g| g.icon.clone())
            .flatten()
            .ok_or("this server has no icon")?;
        download(
            &format!("https://cdn.discordapp.com/icons/{guild_id}/{icon}.png?size=1024"),
            true,
        )
        .await?
    } else {
        // "avatar:@name", "pfp of name", or just a person's name.
        let who = lower
            .trim_start_matches("avatar:")
            .trim_start_matches("avatar of ")
            .trim_start_matches("pfp:")
            .trim_start_matches("pfp of ")
            .trim_end_matches("'s avatar")
            .trim_end_matches("'s pfp")
            .trim();
        match avatar_for(ctx, who).await {
            Ok(url) => download(&url, true).await?,
            Err(_) => {
                let mut names: Vec<String> = ctx
                    .inputs
                    .iter()
                    .map(|s| format!("\"{}\"", s.name))
                    .collect();
                names.extend(
                    ctx.media
                        .lock()
                        .expect("media poisoned")
                        .names()
                        .into_iter()
                        .map(|n| format!("\"{n}\"")),
                );
                return Err(if names.is_empty() {
                    format!(
                        "\"{source}\" isn't an image I can find — there are no images in this conversation; use a URL, avatar:NAME, or server icon"
                    )
                } else {
                    format!(
                        "\"{source}\" isn't an image I can find. Available: {}; or use a URL, avatar:NAME, server icon",
                        names.join(", ")
                    )
                });
            }
        }
    };
    let image = decode(&bytes)?;
    let name = ctx
        .media
        .lock()
        .expect("media poisoned")
        .add(encode(image.clone())?, false);
    Ok((name, image))
}

async fn avatar_for(ctx: &ToolCtx, who: &str) -> Result<String, String> {
    let member = super::discord::resolve_member(ctx, who).await?;
    Ok(avatar_url(
        ctx.guild_id,
        member.user_id,
        member.avatar.as_deref(),
        member.guild_avatar.as_deref(),
    ))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

pub async fn get_avatar(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let who = required(args, "user")?;
    let member = super::discord::resolve_member(ctx, &who).await?;
    let server = super::flag(args, "server_avatar").unwrap_or(true);
    let url = avatar_url(
        ctx.guild_id,
        member.user_id,
        member.avatar.as_deref(),
        if server {
            member.guild_avatar.as_deref()
        } else {
            None
        },
    );
    let bytes = download(&url, true).await?;
    let image = decode(&bytes)?;
    let (w, h) = image.dimensions();
    let name = ctx
        .media
        .lock()
        .expect("media poisoned")
        .add(encode(image)?, true);
    Ok(format!(
        "loaded {}'s profile picture as {name} ({w}×{h}); it will be attached to your reply. Direct link: {url}",
        member.display_name()
    ))
}

/// Many profile pictures in one image: a numbered grid with each
/// person's display name and @username under their picture.
pub async fn avatar_grid(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let names: Vec<String> = match args.get("users") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(text)) => text
            .split([',', '\n', ';'])
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let mut people: Vec<crate::runtime::world::MemberInfo> = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        match super::discord::resolve_member(ctx, &name).await {
            Ok(m) if !people.iter().any(|p| p.user_id == m.user_id) => people.push(m),
            Ok(_) => {}
            Err(_) => missing.push(name),
        }
    }
    if let Some(role) = arg(args, "role") {
        let members = ctx
            .core
            .world
            .with_guild(guild_id, |g| {
                let id = g.find_role(&role)?.id.get();
                Some(
                    g.members
                        .values()
                        .filter(|m| m.roles.contains(&id))
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .ok_or_else(|| format!("no role matches \"{role}\""))?;
        for m in members {
            if !people.iter().any(|p| p.user_id == m.user_id) {
                people.push(m);
            }
        }
    }
    if super::flag(args, "voice").unwrap_or(false) {
        let me = ctx.core.world.my_id();
        let members = ctx
            .core
            .world
            .with_guild(guild_id, |g| {
                let channel = g
                    .voice
                    .get(&ctx.requester.user_id)
                    .or_else(|| g.voice.get(&me))
                    .copied()?;
                Some(
                    g.voice
                        .iter()
                        .filter(|(_, c)| **c == channel)
                        .filter_map(|(u, _)| g.members.get(u).cloned())
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .ok_or("you're not in a voice channel")?;
        for m in members {
            if !people.iter().any(|p| p.user_id == m.user_id) {
                people.push(m);
            }
        }
    }
    if people.is_empty() {
        return Err(if missing.is_empty() {
            "say whose pictures: names (users), a role, or voice: true".to_owned()
        } else {
            format!("couldn't find {}", missing.join(", "))
        });
    }
    people.truncate(48);
    // Download them all at once.
    let downloads: Vec<_> = people
        .iter()
        .map(|m| {
            let url = avatar_url(
                Some(guild_id),
                m.user_id,
                m.avatar.as_deref(),
                m.guild_avatar.as_deref(),
            )
            .replace("size=1024", "size=256");
            tokio::spawn(async move {
                download(&url, true)
                    .await
                    .ok()
                    .and_then(|b| decode(&b).ok())
            })
        })
        .collect();
    let mut pictures = Vec::new();
    for handle in downloads {
        pictures.push(handle.await.ok().flatten());
    }
    let entries: Vec<(String, String)> = people
        .iter()
        .map(|m| (m.display_name().to_owned(), format!("@{}", m.username)))
        .collect();
    let png = encode(DynamicImage::ImageRgba8(avatar_sheet(&pictures, &entries)))?;
    let name = ctx
        .media
        .lock()
        .expect("media poisoned")
        .add_replacing("avatar grid", png);
    let mut out = format!(
        "{name}: a grid of {} profile picture(s), attached below your message:\n",
        people.len()
    );
    for (i, m) in people.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n",
            i + 1,
            super::person_label(
                m.nick.as_deref(),
                m.global_name.as_deref(),
                &m.username,
                m.user_id
            )
        ));
    }
    if !missing.is_empty() {
        out.push_str(&format!("Couldn't find: {}\n", missing.join(", ")));
    }
    out.push_str("They can ask for any one in full resolution (get_avatar).");
    Ok(out)
}

/// The picture for avatar_grid: numbered circles, names underneath.
pub fn avatar_sheet(pictures: &[Option<DynamicImage>], entries: &[(String, String)]) -> RgbaImage {
    const TILE: u32 = 160;
    const GAP: u32 = 24;
    const TEXT: u32 = 52;
    let n = entries.len().max(1) as u32;
    let cols = ((n as f32).sqrt().ceil() as u32)
        .clamp(1, 6)
        .max(if n >= 4 { 2 } else { n });
    let rows = n.div_ceil(cols);
    let cell_w = TILE + GAP;
    let cell_h = TILE + TEXT + GAP;
    let mut canvas = RgbaImage::from_pixel(
        cols * cell_w + GAP,
        rows * cell_h + GAP,
        Rgba([30, 31, 34, 255]),
    );
    let font = FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font");
    let fit = |text: &str, size: f32| -> String {
        let max = (cell_w - 6) as f32;
        if line_width(&font, text, size) <= max {
            return text.to_owned();
        }
        let mut cut: String = text.to_owned();
        while !cut.is_empty() && line_width(&font, &format!("{cut}…"), size) > max {
            cut.pop();
        }
        format!("{cut}…")
    };
    let r = TILE as f32 / 2.0;
    for (i, (display, username)) in entries.iter().enumerate() {
        let col = i as u32 % cols;
        let row = i as u32 / cols;
        let x0 = GAP + col * cell_w;
        let y0 = GAP + row * cell_h;
        match pictures.get(i).cloned().flatten() {
            Some(picture) => {
                let picture = picture
                    .resize_to_fill(TILE, TILE, imageops::FilterType::Triangle)
                    .to_rgba8();
                for (px, py, pixel) in picture.enumerate_pixels() {
                    let dx = px as f32 + 0.5 - r;
                    let dy = py as f32 + 0.5 - r;
                    let edge = (r - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                    if edge > 0.0 {
                        blend(
                            &mut canvas,
                            (x0 + px) as i64,
                            (y0 + py) as i64,
                            Rgba([pixel[0], pixel[1], pixel[2], 255]),
                            edge * pixel[3] as f32 / 255.0,
                        );
                    }
                }
            }
            None => {
                ellipse(
                    &mut canvas,
                    x0 as f32 + r,
                    y0 as f32 + r,
                    r,
                    r,
                    Rgba([88, 101, 242, 255]),
                    true,
                    0.0,
                );
                let initial: String = display
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().collect())
                    .unwrap_or_default();
                let w = line_width(&font, &initial, 64.0);
                draw_text(
                    &mut canvas,
                    &font,
                    &initial,
                    64.0,
                    x0 as f32 + r - w / 2.0,
                    y0 as f32 + r - 38.0,
                    Rgba([255, 255, 255, 255]),
                );
            }
        }
        // Number badge.
        let label = (i + 1).to_string();
        ellipse(
            &mut canvas,
            x0 as f32 + 18.0,
            y0 as f32 + 18.0,
            17.0,
            17.0,
            Rgba([235, 69, 158, 255]),
            true,
            0.0,
        );
        let w = line_width(&font, &label, 20.0);
        draw_text(
            &mut canvas,
            &font,
            &label,
            20.0,
            x0 as f32 + 18.0 - w / 2.0,
            y0 as f32 + 6.0,
            Rgba([255, 255, 255, 255]),
        );
        // Names, centered under the picture.
        let center = x0 as f32 + r;
        let display = fit(display, 19.0);
        let w = line_width(&font, &display, 19.0);
        draw_text(
            &mut canvas,
            &font,
            &display,
            19.0,
            center - w / 2.0,
            (y0 + TILE + 6) as f32,
            Rgba([242, 243, 245, 255]),
        );
        let username = fit(username, 15.0);
        let w = line_width(&font, &username, 15.0);
        draw_text(
            &mut canvas,
            &font,
            &username,
            15.0,
            center - w / 2.0,
            (y0 + TILE + 30) as f32,
            Rgba([148, 155, 164, 255]),
        );
    }
    canvas
}

pub async fn look_at_image(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let source = required(args, "image")?;
    let question =
        arg(args, "question").unwrap_or_else(|| "Describe this image in detail.".to_owned());
    let (name, image) = load(ctx, &source).await?;
    let png = encode(image.resize(1024, 1024, imageops::FilterType::Triangle))?;
    let seen = crate::runtime::vision::SeenImage {
        label: name.clone(),
        data_uri: crate::runtime::vision::prepare(&png).ok_or("couldn't prepare the image")?,
    };
    let profile = ctx.core.profile();
    let model = if profile.model.vision_model.trim().is_empty() {
        profile.model.model.clone()
    } else {
        profile.model.vision_model.trim().to_owned()
    };
    match crate::runtime::vision::describe(&ctx.core, &model, &seen, &question).await {
        Ok(text) => Ok(format!("{name}: {text}")),
        Err(error) if error.contains("image") => Err(format!(
            "the model {model} can't see images — set a vision model on the Model tab (e.g. a small -VL model)"
        )),
        Err(error) => Err(format!("couldn't look at it: {error}")),
    }
}

pub async fn attach_image(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let source = required(args, "image")?;
    let (name, image) = load(ctx, &source).await?;
    ctx.media.lock().expect("media poisoned").attach(&name);
    let (w, h) = image.dimensions();
    Ok(format!("{name} ({w}×{h}) will be attached to your reply"))
}

pub async fn edit_image(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let source = required(args, "image")?;
    let operations: Vec<String> = match args.get("operations") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(text)) => text
            .split([';', '\n'])
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => {
            return Err(
                "give operations, e.g. [\"grayscale\", \"caption top text | bottom text\"]"
                    .to_owned(),
            );
        }
    };
    if operations.is_empty() {
        return Err("no operations given".to_owned());
    }
    let (source_name, mut image) = load(ctx, &source).await?;
    let mut done = Vec::new();
    for operation in operations.iter().take(20) {
        image = apply(image, operation).map_err(|e| format!("{operation}: {e}"))?;
        done.push(operation.clone());
    }
    let (w, h) = image.dimensions();
    let mut media = ctx.media.lock().expect("media poisoned");
    let name = media.add(encode(image)?, true);
    media.supersede(&source_name, &name);
    Ok(format!(
        "edited {source_name} → {name} ({w}×{h}) with: {}; it will be attached to your reply",
        done.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Web image search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FoundImage {
    pub title: String,
    pub url: String,
    pub page: String,
}

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

fn html_unescape(text: &str) -> String {
    super::web::decode_entities(text)
}

/// Parse Bing's image results page (each result's metadata is a JSON blob
/// in the `m` attribute of an `iusc` link).
pub fn parse_bing_images(html: &str, limit: usize) -> Vec<FoundImage> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < limit {
        let Some(at) = rest.find("class=\"iusc\"") else {
            break;
        };
        rest = &rest[at + 12..];
        let Some(m_at) = rest.find(" m=\"") else {
            break;
        };
        let after = &rest[m_at + 4..];
        let Some(end) = after.find('"') else { break };
        let meta = html_unescape(&after[..end]);
        let Ok(value) = serde_json::from_str::<Value>(&meta) else {
            continue;
        };
        let Some(url) = value.get("murl").and_then(Value::as_str) else {
            continue;
        };
        out.push(FoundImage {
            title: value
                .get("t")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            url: url.to_owned(),
            page: value
                .get("purl")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        });
    }
    out
}

async fn search_bing(query: &str, limit: usize) -> Result<Vec<FoundImage>, String> {
    let html = crate::llm::http_client()
        .get("https://www.bing.com/images/search")
        .query(&[("q", query), ("form", "HDRSC2"), ("first", "1")])
        .header("User-Agent", BROWSER_UA)
        .header("Accept-Language", "en-US,en;q=0.9")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    Ok(parse_bing_images(&html, limit))
}

/// Openverse: openly licensed images (Flickr, museums…), searched by
/// their real titles and tags — reliably on topic.
async fn search_openverse(query: &str, limit: usize) -> Result<Vec<FoundImage>, String> {
    let value: Value = crate::llm::http_client()
        .get("https://api.openverse.org/v1/images/")
        .query(&[("q", query), ("page_size", &limit.to_string())])
        .header("User-Agent", "AxisAgents/1.0")
        // A bonus source: often slow for new searches, so don't wait on it.
        .timeout(std::time::Duration::from_secs(6))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(FoundImage {
                title: r
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("image")
                    .to_owned(),
                url: r.get("url")?.as_str()?.to_owned(),
                page: r
                    .get("foreign_landing_url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect())
}

/// Is a result about the query? (Scraped results drift off topic.)
fn relevant(image: &FoundImage, query: &str) -> bool {
    let haystack = format!("{} {}", image.title, image.url).to_lowercase();
    let words: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| {
            w.len() >= 3
                && !matches!(
                    *w,
                    "the" | "and" | "for" | "with" | "image" | "picture" | "photo" | "png" | "jpg"
                )
        })
        .map(str::to_owned)
        .collect();
    words.is_empty()
        || words.iter().any(|w| {
            // "zebras" / "zebra"
            let stem = w.trim_end_matches('s');
            haystack.contains(stem)
        })
}

async fn search_commons(query: &str, limit: usize) -> Result<Vec<FoundImage>, String> {
    let value: Value = crate::llm::http_client()
        .get("https://commons.wikimedia.org/w/api.php")
        .query(&[
            ("action", "query"),
            ("format", "json"),
            ("generator", "search"),
            ("gsrnamespace", "6"),
            ("gsrlimit", &limit.to_string()),
            ("gsrsearch", query),
            ("prop", "imageinfo"),
            ("iiprop", "url|mime"),
            ("iiurlwidth", "1280"),
        ])
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let pages = value.pointer("/query/pages").and_then(Value::as_object);
    Ok(pages
        .into_iter()
        .flat_map(|pages| pages.values())
        .filter_map(|page| {
            let info = page.pointer("/imageinfo/0")?;
            if !info.get("mime")?.as_str()?.starts_with("image/") {
                return None;
            }
            Some(FoundImage {
                title: page
                    .get("title")?
                    .as_str()?
                    .trim_start_matches("File:")
                    .to_owned(),
                url: info
                    .get("thumburl")
                    .or_else(|| info.get("url"))?
                    .as_str()?
                    .to_owned(),
                page: info
                    .get("descriptionurl")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect())
}

pub async fn image_search(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let query = required(args, "query")?;
    let count = number(args, "count").unwrap_or(5).clamp(1, 10) as usize;
    // Search wide, then keep only links that actually load as images —
    // a dead or hotlink-blocked link can't be embedded.
    let (openverse, commons, bing) = tokio::join!(
        search_openverse(&query, count * 2),
        search_commons(&query, count),
        search_bing(&query, count * 3),
    );
    let mut candidates: Vec<FoundImage> = Vec::new();
    for image in openverse
        .unwrap_or_default()
        .into_iter()
        .chain(commons.unwrap_or_default())
        .chain(bing.unwrap_or_default())
    {
        if relevant(&image, &query) && !candidates.iter().any(|c| c.url == image.url) {
            candidates.push(image);
        }
    }
    let checks = candidates
        .iter()
        .take(count * 4)
        .map(|image| super::web::is_live_image(&image.url));
    let alive = futures_util::future::join_all(checks).await;
    let found: Vec<FoundImage> = candidates
        .into_iter()
        .zip(alive)
        .filter(|(_, ok)| *ok)
        .map(|(image, _)| image)
        .take(count)
        .collect();
    if found.is_empty() {
        return Err(format!(
            "I couldn't find a working image for \"{query}\" — the results were dead or blocked links. Tell them that instead of guessing a link."
        ));
    }
    ctx.core
        .feed(FeedKind::Tool, format!("searched images: {query}"), None);
    let mut out = String::new();
    // Optionally attach the first one that actually downloads.
    if super::flag(args, "attach").unwrap_or(false) {
        for image in &found {
            if let Ok(bytes) = download(&image.url, false).await {
                if let Ok(decoded) = decode(&bytes) {
                    let name = ctx
                        .media
                        .lock()
                        .expect("media poisoned")
                        .add(encode(decoded)?, true);
                    out.push_str(&format!(
                        "Attached {name} ({}) to your reply.\n\n",
                        image.title
                    ));
                    break;
                }
            }
        }
    }
    out.push_str("Verified image links (each one loads; post one on its own line and Discord embeds it — never post a link that isn't in this list):\n");
    for (index, image) in found.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n   image: {}\n   page: {}\n",
            index + 1,
            crate::llm::truncate(&image.title, 90),
            image.url,
            image.page
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Blend a color into a pixel with the given coverage (0–1).
pub(crate) fn blend(canvas: &mut RgbaImage, x: i64, y: i64, color: Rgba<u8>, coverage: f32) {
    let (w, h) = canvas.dimensions();
    if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 || coverage <= 0.0 {
        return;
    }
    let alpha = coverage.min(1.0) * color.0[3] as f32 / 255.0;
    let pixel = canvas.get_pixel_mut(x as u32, y as u32);
    for c in 0..3 {
        pixel.0[c] = (pixel.0[c] as f32 * (1.0 - alpha) + color.0[c] as f32 * alpha).round() as u8;
    }
    pixel.0[3] = (pixel.0[3] as f32 + (255.0 - pixel.0[3] as f32) * alpha).round() as u8;
}

pub(crate) fn fill_rect(canvas: &mut RgbaImage, x: f32, y: f32, w: f32, h: f32, color: Rgba<u8>) {
    let (x0, y0) = (x.min(x + w).floor() as i64, y.min(y + h).floor() as i64);
    let (x1, y1) = (x.max(x + w).ceil() as i64, y.max(y + h).ceil() as i64);
    for py in y0..y1 {
        for px in x0..x1 {
            blend(canvas, px, py, color, 1.0);
        }
    }
}

/// An anti-aliased thick line segment.
fn line(canvas: &mut RgbaImage, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Rgba<u8>) {
    let half = (width / 2.0).max(0.5);
    let (dx, dy) = (x2 - x1, y2 - y1);
    let length_sq = (dx * dx + dy * dy).max(1e-6);
    let (minx, maxx) = (x1.min(x2) - half - 1.0, x1.max(x2) + half + 1.0);
    let (miny, maxy) = (y1.min(y2) - half - 1.0, y1.max(y2) + half + 1.0);
    for py in miny.floor() as i64..=maxy.ceil() as i64 {
        for px in minx.floor() as i64..=maxx.ceil() as i64 {
            let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
            let t = (((cx - x1) * dx + (cy - y1) * dy) / length_sq).clamp(0.0, 1.0);
            let (nx, ny) = (x1 + t * dx, y1 + t * dy);
            let distance = ((cx - nx).powi(2) + (cy - ny).powi(2)).sqrt();
            blend(canvas, px, py, color, half + 0.5 - distance);
        }
    }
}

pub(crate) fn ellipse(
    canvas: &mut RgbaImage,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    color: Rgba<u8>,
    fill: bool,
    width: f32,
) {
    let (rx, ry) = (rx.max(0.5), ry.max(0.5));
    for py in (cy - ry - width).floor() as i64..=(cy + ry + width).ceil() as i64 {
        for px in (cx - rx - width).floor() as i64..=(cx + rx + width).ceil() as i64 {
            let (dx, dy) = ((px as f32 + 0.5 - cx) / rx, (py as f32 + 0.5 - cy) / ry);
            let d = (dx * dx + dy * dy).sqrt();
            // Signed distance in pixels from the edge (approximate).
            let edge = (1.0 - d) * rx.min(ry);
            let coverage = if fill {
                edge + 0.5
            } else {
                width / 2.0 + 0.5 - edge.abs()
            };
            blend(canvas, px, py, color, coverage);
        }
    }
}

fn polygon(canvas: &mut RgbaImage, points: &[(f32, f32)], color: Rgba<u8>) {
    if points.len() < 3 {
        return;
    }
    let miny = points.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i64;
    let maxy = points.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i64;
    let minx = points.iter().map(|p| p.0).fold(f32::MAX, f32::min).floor() as i64;
    let maxx = points.iter().map(|p| p.0).fold(f32::MIN, f32::max).ceil() as i64;
    for py in miny..=maxy {
        for px in minx..=maxx {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let mut inside = false;
            let mut j = points.len() - 1;
            for i in 0..points.len() {
                let (xi, yi) = points[i];
                let (xj, yj) = points[j];
                if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                    inside = !inside;
                }
                j = i;
            }
            if inside {
                blend(canvas, px, py, color, 1.0);
            }
        }
    }
}

fn text_at(canvas: &mut RgbaImage, x: &str, y: f32, size: f32, color: Rgba<u8>, text: &str) {
    let font = FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font");
    let width = line_width(&font, text, size);
    let x = match x {
        "center" | "c" => (canvas.width() as f32 - width) / 2.0,
        "right" | "r" => canvas.width() as f32 - width - 8.0,
        other => other.parse().unwrap_or(0.0),
    };
    // A faint heavier pass makes the light font read as regular weight.
    for (dx, dy) in [(0.0, 0.0), (0.6, 0.0), (0.0, 0.6)] {
        draw_text(canvas, &font, text, size, x + dx, y + dy, color);
    }
}

/// Draw one shape command onto the canvas.
pub fn draw_shape(canvas: &mut RgbaImage, command: &str) -> Result<(), String> {
    let command = command.trim();
    let mut words = command.split_whitespace();
    let verb = words.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = words.collect();
    let num = |i: usize| -> Result<f32, String> {
        rest.get(i)
            .and_then(|v| v.parse::<f32>().ok())
            .ok_or_else(|| format!("expected a number at position {}", i + 1))
    };
    let color_at = |i: usize| parse_color(rest.get(i).copied().unwrap_or("white"));
    let outline = rest.iter().any(|w| w.eq_ignore_ascii_case("outline"));
    let thickness = |default: f32| {
        rest.iter()
            .rev()
            .find_map(|w| {
                w.strip_prefix("width=")
                    .or_else(|| w.strip_prefix("w="))
                    .and_then(|v| v.parse::<f32>().ok())
            })
            .unwrap_or(default)
    };
    match verb.as_str() {
        "rect" | "box" | "rectangle" | "square" => {
            let (x, y, w, h) = (num(0)?, num(1)?, num(2)?, num(3)?);
            let color = color_at(4);
            if outline {
                let t = thickness(3.0);
                line(canvas, x, y, x + w, y, t, color);
                line(canvas, x + w, y, x + w, y + h, t, color);
                line(canvas, x + w, y + h, x, y + h, t, color);
                line(canvas, x, y + h, x, y, t, color);
            } else {
                fill_rect(canvas, x, y, w, h, color);
            }
        }
        "circle" => ellipse(
            canvas,
            num(0)?,
            num(1)?,
            num(2)?,
            num(2)?,
            color_at(3),
            !outline,
            thickness(3.0),
        ),
        "ellipse" | "oval" => ellipse(
            canvas,
            num(0)?,
            num(1)?,
            num(2)?,
            num(3)?,
            color_at(4),
            !outline,
            thickness(3.0),
        ),
        "line" => line(
            canvas,
            num(0)?,
            num(1)?,
            num(2)?,
            num(3)?,
            thickness(4.0),
            color_at(4),
        ),
        "arrow" => {
            let (x1, y1, x2, y2) = (num(0)?, num(1)?, num(2)?, num(3)?);
            let color = color_at(4);
            let t = thickness(4.0);
            let angle = (y2 - y1).atan2(x2 - x1);
            let head = (t * 4.0).max(12.0);
            let back = (x2 - head * 0.8 * angle.cos(), y2 - head * 0.8 * angle.sin());
            line(canvas, x1, y1, back.0, back.1, t, color);
            let left = (
                x2 - head * (angle - 0.45).cos(),
                y2 - head * (angle - 0.45).sin(),
            );
            let right = (
                x2 - head * (angle + 0.45).cos(),
                y2 - head * (angle + 0.45).sin(),
            );
            polygon(canvas, &[(x2, y2), left, right], color);
        }
        "polygon" | "triangle" => {
            let color = color_at(0);
            let values: Vec<f32> = rest[1..].iter().filter_map(|v| v.parse().ok()).collect();
            let points: Vec<(f32, f32)> = values
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| (c[0], c[1]))
                .collect();
            if points.len() < 3 {
                return Err(
                    "polygon needs a color and at least 3 points: polygon red 10 10 100 10 50 80"
                        .to_owned(),
                );
            }
            polygon(canvas, &points, color);
        }
        "grid" => {
            let step = num(0)?.max(4.0);
            let color = color_at(1);
            let (w, h) = (canvas.width() as f32, canvas.height() as f32);
            let mut x = 0.0;
            while x <= w {
                line(canvas, x, 0.0, x, h, 1.0, color);
                x += step;
            }
            let mut y = 0.0;
            while y <= h {
                line(canvas, 0.0, y, w, y, 1.0, color);
                y += step;
            }
        }
        "text" => {
            // text X Y SIZE COLOR the words…   (X may be center/right)
            let x = rest.first().copied().unwrap_or("0");
            let (y, size) = (num(1)?, num(2)?.clamp(6.0, 400.0));
            let color = color_at(3);
            let words = rest.get(4..).map(|w| w.join(" ")).unwrap_or_default();
            if words.is_empty() {
                return Err("text needs: text X Y SIZE COLOR your words".to_owned());
            }
            text_at(canvas, &x.to_lowercase(), y, size, color, &words);
        }
        "fill" | "background" => {
            let color = color_at(0);
            let (w, h) = (canvas.width() as f32, canvas.height() as f32);
            fill_rect(canvas, 0.0, 0.0, w, h, color);
        }
        other => {
            return Err(format!(
                "unknown shape \"{other}\" — use rect, circle, ellipse, line, arrow, polygon, grid, text, fill"
            ));
        }
    }
    Ok(())
}

pub async fn draw_image(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let shapes: Vec<String> = match args.get("shapes") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(text)) => text
            .split([';', '\n'])
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => return Err(
            "give shapes, e.g. [\"rect 20 20 200 100 blue\", \"text center 140 32 white Hello\"]"
                .to_owned(),
        ),
    };
    let (mut canvas, base) = match arg(args, "base_image") {
        Some(source) => {
            let (name, image) = load(ctx, &source).await?;
            (image.to_rgba8(), Some(name))
        }
        None => {
            let width = number(args, "width").unwrap_or(800).clamp(16, 2048) as u32;
            let height = number(args, "height").unwrap_or(500).clamp(16, 2048) as u32;
            let background = arg(args, "background").unwrap_or_else(|| "white".to_owned());
            let color = if background.eq_ignore_ascii_case("transparent") {
                Rgba([0, 0, 0, 0])
            } else {
                parse_color(&background)
            };
            (RgbaImage::from_pixel(width, height, color), None)
        }
    };
    let mut errors = Vec::new();
    for shape in shapes.iter().take(200) {
        if let Err(error) = draw_shape(&mut canvas, shape) {
            errors.push(format!("\"{shape}\": {error}"));
        }
    }
    let (w, h) = canvas.dimensions();
    let mut media = ctx.media.lock().expect("media poisoned");
    let name = media.add(encode(DynamicImage::ImageRgba8(canvas))?, true);
    if let Some(base) = base {
        media.supersede(&base, &name);
    }
    let mut out = format!(
        "drew {name} ({w}×{h}, {} shapes); it will be attached to your reply",
        shapes.len() - errors.len()
    );
    if !errors.is_empty() {
        out.push_str(&format!("\nskipped: {}", errors.join("; ")));
    }
    Ok(out)
}

const CHART_COLORS: [[u8; 3]; 8] = [
    [99, 102, 241],
    [34, 197, 94],
    [249, 115, 22],
    [236, 72, 153],
    [14, 165, 233],
    [234, 179, 8],
    [168, 85, 247],
    [20, 184, 166],
];

fn compact(value: f64) -> String {
    if value.abs() >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value.abs() >= 10_000.0 {
        format!("{:.1}k", value / 1000.0)
    } else if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value:.2}")
    }
}

/// Render a bar, line, or pie chart.
pub fn chart(
    kind: &str,
    title: &str,
    labels: &[String],
    values: &[f64],
    width: u32,
    height: u32,
) -> RgbaImage {
    let background = Rgba([24, 26, 36, 255]);
    let text = Rgba([230, 232, 240, 255]);
    let faint = Rgba([90, 96, 116, 255]);
    let mut canvas = RgbaImage::from_pixel(width, height, background);
    let (w, h) = (width as f32, height as f32);
    let top = if title.is_empty() { 20.0 } else { 64.0 };
    if !title.is_empty() {
        text_at(&mut canvas, "center", 16.0, 28.0, text, title);
    }
    let n = values.len().max(1);
    let color = |i: usize| {
        let c = CHART_COLORS[i % CHART_COLORS.len()];
        Rgba([c[0], c[1], c[2], 255])
    };
    if kind == "pie" {
        let total: f64 = values.iter().filter(|v| **v > 0.0).sum::<f64>().max(1e-9);
        let radius = ((h - top - 30.0) / 2.0).min(w * 0.3);
        let (cx, cy) = (w * 0.36, top + (h - top) / 2.0);
        let mut angle = -std::f32::consts::FRAC_PI_2;
        for (i, value) in values.iter().enumerate() {
            let sweep = (*value).max(0.0) as f32 / total as f32 * std::f32::consts::TAU;
            let steps = ((sweep / 0.05).ceil() as usize).max(2);
            let mut points = vec![(cx, cy)];
            for s in 0..=steps {
                let a = angle + sweep * s as f32 / steps as f32;
                points.push((cx + radius * a.cos(), cy + radius * a.sin()));
            }
            polygon(&mut canvas, &points, color(i));
            angle += sweep;
            // Legend
            let ly = top + 10.0 + i as f32 * 30.0;
            fill_rect(&mut canvas, w * 0.68, ly, 18.0, 18.0, color(i));
            let label = labels.get(i).cloned().unwrap_or_default();
            text_at(
                &mut canvas,
                &format!("{}", w * 0.68 + 28.0),
                ly - 2.0,
                18.0,
                text,
                &format!(
                    "{label} — {} ({:.0}%)",
                    compact(*value),
                    *value / total * 100.0
                ),
            );
        }
        return canvas;
    }
    let left = 70.0;
    let bottom = h - 56.0;
    let right = w - 24.0;
    let max = values.iter().copied().fold(0.0f64, f64::max).max(1e-9);
    let plot_h = bottom - top;
    // Axes and gridlines with value labels.
    for g in 0..=4 {
        let y = bottom - plot_h * g as f32 / 4.0;
        line(&mut canvas, left, y, right, y, 1.0, faint);
        text_at(
            &mut canvas,
            &format!("{}", 8.0),
            y - 9.0,
            14.0,
            faint,
            &compact(max * g as f64 / 4.0),
        );
    }
    let slot = (right - left) / n as f32;
    let y_of = |v: f64| bottom - (v.max(0.0) / max) as f32 * plot_h;
    if kind == "line" {
        let points: Vec<(f32, f32)> = values
            .iter()
            .enumerate()
            .map(|(i, v)| (left + slot * (i as f32 + 0.5), y_of(*v)))
            .collect();
        for pair in points.windows(2) {
            line(
                &mut canvas,
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1,
                4.0,
                color(0),
            );
        }
        for (x, y) in &points {
            ellipse(&mut canvas, *x, *y, 6.0, 6.0, color(0), true, 1.0);
        }
    } else {
        for (i, value) in values.iter().enumerate() {
            let bar_w = slot * 0.66;
            let x = left + slot * i as f32 + (slot - bar_w) / 2.0;
            let y = y_of(*value);
            fill_rect(&mut canvas, x, y, bar_w, bottom - y, color(i));
            text_at(
                &mut canvas,
                &format!("{}", x),
                y - 22.0,
                15.0,
                text,
                &compact(*value),
            );
        }
    }
    for (i, label) in labels.iter().enumerate().take(n) {
        let x = left + slot * i as f32 + 4.0;
        let label = crate::llm::truncate(label, ((slot / 8.0) as usize).max(3));
        text_at(
            &mut canvas,
            &format!("{x}"),
            bottom + 10.0,
            15.0,
            text,
            &label,
        );
    }
    canvas
}

pub async fn make_chart(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let kind = arg(args, "type")
        .unwrap_or_else(|| "bar".to_owned())
        .to_lowercase();
    if !matches!(kind.as_str(), "bar" | "line" | "pie") {
        return Err("type must be bar, line, or pie".to_owned());
    }
    let labels = super::list(args, "labels");
    let values: Vec<f64> = match args.get("values") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            })
            .collect(),
        Some(Value::String(text)) => text
            .split(',')
            .filter_map(|v| v.trim().parse().ok())
            .collect(),
        _ => Vec::new(),
    };
    if values.is_empty() || values.len() > 40 {
        return Err("give 1-40 numeric values".to_owned());
    }
    let title = arg(args, "title").unwrap_or_default();
    let width = number(args, "width").unwrap_or(900).clamp(300, 2048) as u32;
    let height = number(args, "height").unwrap_or(520).clamp(200, 2048) as u32;
    let image = chart(&kind, &title, &labels, &values, width, height);
    let name = ctx
        .media
        .lock()
        .expect("media poisoned")
        .add(encode(DynamicImage::ImageRgba8(image))?, true);
    Ok(format!(
        "made a {kind} chart {name} with {} values; it will be attached to your reply",
        values.len()
    ))
}

// ---------------------------------------------------------------------------
// Local operations
// ---------------------------------------------------------------------------

fn numbers(text: &str) -> Vec<f32> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter_map(|n| n.parse().ok())
        .collect()
}

pub(crate) fn parse_color(text: &str) -> Rgba<u8> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() == 6 {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return Rgba([(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]);
        }
    }
    match text.trim().to_lowercase().as_str() {
        "black" => Rgba([0, 0, 0, 255]),
        "red" => Rgba([230, 60, 60, 255]),
        "blue" => Rgba([60, 120, 230, 255]),
        "green" => Rgba([60, 190, 100, 255]),
        "yellow" => Rgba([245, 200, 60, 255]),
        "purple" => Rgba([140, 100, 240, 255]),
        "orange" => Rgba([249, 140, 40, 255]),
        "pink" => Rgba([240, 110, 180, 255]),
        "gray" | "grey" => Rgba([128, 128, 128, 255]),
        "cyan" => Rgba([40, 200, 220, 255]),
        "brown" => Rgba([140, 90, 50, 255]),
        "navy" => Rgba([30, 40, 110, 255]),
        "white" => Rgba([255, 255, 255, 255]),
        _ => Rgba([255, 255, 255, 255]),
    }
}

pub fn apply(image: DynamicImage, operation: &str) -> Result<DynamicImage, String> {
    let op = operation.trim();
    let (verb, rest) = op.split_once(' ').unwrap_or((op, ""));
    let verb = verb.to_lowercase();
    let nums = numbers(rest);
    let (w, h) = image.dimensions();
    let image = match verb.as_str() {
        "resize" | "scale" | "size" => {
            if rest.contains('%') || (verb == "scale" && nums.len() == 1) {
                let factor = if rest.contains('%') {
                    nums.first().copied().unwrap_or(100.0) / 100.0
                } else {
                    nums[0]
                };
                let factor = factor.clamp(0.05, 8.0);
                image.resize_exact(
                    ((w as f32 * factor) as u32).max(1),
                    ((h as f32 * factor) as u32).max(1),
                    imageops::FilterType::Lanczos3,
                )
            } else if nums.len() >= 2 {
                image.resize_exact(
                    nums[0].clamp(1.0, 4096.0) as u32,
                    nums[1].clamp(1.0, 4096.0) as u32,
                    imageops::FilterType::Lanczos3,
                )
            } else if let Some(side) = nums.first() {
                image.resize(*side as u32, *side as u32, imageops::FilterType::Lanczos3)
            } else {
                return Err("use resize 512x512, resize 50%, or scale 2".to_owned());
            }
        }
        "thumbnail" => image.thumbnail(
            nums.first().copied().unwrap_or(256.0) as u32,
            nums.first().copied().unwrap_or(256.0) as u32,
        ),
        "crop" => {
            if rest.contains("square") {
                let side = w.min(h);
                image.crop_imm((w - side) / 2, (h - side) / 2, side, side)
            } else if nums.len() >= 4 {
                let (x, y) = (nums[0].max(0.0) as u32, nums[1].max(0.0) as u32);
                if x >= w || y >= h {
                    return Err(format!("crop starts outside the {w}×{h} image"));
                }
                image.crop_imm(
                    x,
                    y,
                    (nums[2] as u32).min(w - x).max(1),
                    (nums[3] as u32).min(h - y).max(1),
                )
            } else if nums.len() >= 2 {
                let (cw, ch) = (
                    (nums[0] as u32).min(w).max(1),
                    (nums[1] as u32).min(h).max(1),
                );
                image.crop_imm((w - cw) / 2, (h - ch) / 2, cw, ch)
            } else {
                return Err("use crop square, crop 300x300 (centered), or crop X Y W H".to_owned());
            }
        }
        "rotate" => match nums.first().map(|d| ((*d as i32 % 360) + 360) % 360) {
            Some(90) => image.rotate90(),
            Some(180) => image.rotate180(),
            Some(270) => image.rotate270(),
            Some(0) => image,
            _ => return Err("I can rotate by 90, 180, or 270 degrees".to_owned()),
        },
        "flip" | "mirror" => {
            if rest.to_lowercase().starts_with('v') {
                image.flipv()
            } else {
                image.fliph()
            }
        }
        "grayscale" | "greyscale" | "bw" | "black-and-white" => {
            DynamicImage::ImageRgba8(image.grayscale().to_rgba8())
        }
        "invert" | "negative" => {
            let mut image = image;
            image.invert();
            image
        }
        "blur" => image.blur(nums.first().copied().unwrap_or(4.0).clamp(0.5, 50.0)),
        "sharpen" => image.unsharpen(nums.first().copied().unwrap_or(2.0).clamp(0.5, 10.0), 2),
        "brightness" | "brighten" => {
            image.brighten(nums.first().copied().unwrap_or(20.0).clamp(-255.0, 255.0) as i32)
        }
        "darken" => {
            image.brighten(-(nums.first().copied().unwrap_or(20.0).abs().min(255.0) as i32))
        }
        "contrast" => {
            image.adjust_contrast(nums.first().copied().unwrap_or(20.0).clamp(-100.0, 100.0))
        }
        "hue" => image.huerotate(nums.first().copied().unwrap_or(90.0) as i32),
        "sepia" => {
            let mut rgba = image.to_rgba8();
            for pixel in rgba.pixels_mut() {
                let [r, g, b, a] = pixel.0.map(|v| v as f32);
                pixel.0 = [
                    (0.393 * r + 0.769 * g + 0.189 * b).min(255.0) as u8,
                    (0.349 * r + 0.686 * g + 0.168 * b).min(255.0) as u8,
                    (0.272 * r + 0.534 * g + 0.131 * b).min(255.0) as u8,
                    a as u8,
                ];
            }
            DynamicImage::ImageRgba8(rgba)
        }
        "pixelate" => {
            let block = nums.first().copied().unwrap_or(12.0).clamp(2.0, 200.0) as u32;
            let small = image.resize_exact(
                (w / block).max(1),
                (h / block).max(1),
                imageops::FilterType::Triangle,
            );
            small.resize_exact(w, h, imageops::FilterType::Nearest)
        }
        "deepfry" | "deep-fry" => {
            let fried = image.adjust_contrast(60.0).brighten(15).unsharpen(3.0, 1);
            let mut jpeg = Vec::new();
            DynamicImage::ImageRgb8(fried.to_rgb8())
                .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                    &mut Cursor::new(&mut jpeg),
                    8,
                ))
                .map_err(|e| e.to_string())?;
            decode(&jpeg)?
        }
        "circle" | "round" => {
            let side = w.min(h);
            let mut square = image
                .crop_imm((w - side) / 2, (h - side) / 2, side, side)
                .to_rgba8();
            let radius = side as f32 / 2.0;
            for (x, y, pixel) in square.enumerate_pixels_mut() {
                let dx = x as f32 + 0.5 - radius;
                let dy = y as f32 + 0.5 - radius;
                let edge = radius - (dx * dx + dy * dy).sqrt();
                let coverage = edge.clamp(0.0, 1.0);
                pixel.0[3] = (pixel.0[3] as f32 * coverage) as u8;
            }
            DynamicImage::ImageRgba8(square)
        }
        "border" => {
            let thickness = nums.first().copied().unwrap_or(16.0).clamp(1.0, 400.0) as u32;
            let color = parse_color(
                rest.split_whitespace()
                    .find(|p| !p.chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or("white"),
            );
            let mut canvas = RgbaImage::from_pixel(w + thickness * 2, h + thickness * 2, color);
            imageops::overlay(
                &mut canvas,
                &image.to_rgba8(),
                thickness as i64,
                thickness as i64,
            );
            DynamicImage::ImageRgba8(canvas)
        }
        "caption" | "meme" => {
            let (top, bottom) = rest.split_once('|').unwrap_or((rest, ""));
            caption(image, top.trim(), bottom.trim())
        }
        "top" | "toptext" => caption(image, rest.trim(), ""),
        "bottom" | "bottomtext" => caption(image, "", rest.trim()),
        "text" => caption(image, "", rest.trim()),
        other => {
            return Err(format!(
                "unknown operation \"{other}\" — use resize, crop, rotate, flip, grayscale, invert, blur, sharpen, brightness, contrast, hue, sepia, pixelate, deepfry, circle, border, caption"
            ));
        }
    };
    Ok(image)
}

/// Impact-style meme text: white, outlined, wrapped to fit.
pub fn caption(image: DynamicImage, top: &str, bottom: &str) -> DynamicImage {
    let mut canvas = image.to_rgba8();
    let font = FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font");
    let (w, h) = canvas.dimensions();
    for (text, at_top) in [(top, true), (bottom, false)] {
        let text = text.trim().to_uppercase();
        if text.is_empty() {
            continue;
        }
        let mut size = (h as f32 / 8.0).clamp(14.0, 160.0);
        let lines = loop {
            let lines = wrap(&font, &text, size, w as f32 * 0.92);
            if lines.len() <= 3 || size <= 14.0 {
                break lines;
            }
            size *= 0.85;
        };
        let scaled = font.as_scaled(PxScale::from(size));
        let line_height = scaled.ascent() - scaled.descent();
        let block = line_height * lines.len() as f32;
        let mut y = if at_top {
            h as f32 * 0.03
        } else {
            h as f32 * 0.97 - block
        };
        let outline = (size / 9.0).max(2.0);
        for line in &lines {
            let width = line_width(&font, line, size);
            let x = (w as f32 - width) / 2.0;
            for (dx, dy) in ring(outline) {
                draw_text(
                    &mut canvas,
                    &font,
                    line,
                    size,
                    x + dx,
                    y + dy,
                    Rgba([0, 0, 0, 255]),
                );
            }
            draw_text(
                &mut canvas,
                &font,
                line,
                size,
                x,
                y,
                Rgba([255, 255, 255, 255]),
            );
            y += line_height;
        }
    }
    DynamicImage::ImageRgba8(canvas)
}

fn ring(radius: f32) -> Vec<(f32, f32)> {
    (0..16)
        .map(|i| {
            let angle = i as f32 / 16.0 * std::f32::consts::TAU;
            (angle.cos() * radius, angle.sin() * radius)
        })
        .collect()
}

fn line_width(font: &FontRef<'_>, text: &str, size: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(prev) = previous {
            width += scaled.kern(prev, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

fn wrap(font: &FontRef<'_>, text: &str, size: f32, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if line_width(font, &candidate, size) > max && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current = word.to_owned();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn draw_text(
    canvas: &mut RgbaImage,
    font: &FontRef<'_>,
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
    let (w, h) = canvas.dimensions();
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(prev) = previous {
            caret += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, point(caret, baseline));
        caret += scaled.h_advance(id);
        previous = Some(id);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i64 + gx as i64;
                let py = bounds.min.y as i64 + gy as i64;
                if px < 0 || py < 0 || px >= w as i64 || py >= h as i64 {
                    return;
                }
                let pixel = canvas.get_pixel_mut(px as u32, py as u32);
                let alpha = coverage.clamp(0.0, 1.0);
                for channel in 0..3 {
                    pixel.0[channel] = (pixel.0[channel] as f32 * (1.0 - alpha)
                        + color.0[channel] as f32 * alpha)
                        as u8;
                }
                pixel.0[3] = pixel.0[3].max((alpha * 255.0) as u8);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(200, 100, |x, y| {
            Rgba([x as u8, y as u8, 128, 255])
        }))
    }

    #[test]
    fn operations_apply() {
        assert_eq!(
            apply(sample(), "resize 50x50").unwrap().dimensions(),
            (50, 50)
        );
        assert_eq!(
            apply(sample(), "resize 50%").unwrap().dimensions(),
            (100, 50)
        );
        assert_eq!(
            apply(sample(), "crop square").unwrap().dimensions(),
            (100, 100)
        );
        assert_eq!(
            apply(sample(), "rotate 90").unwrap().dimensions(),
            (100, 200)
        );
        assert_eq!(
            apply(sample(), "border 10 red").unwrap().dimensions(),
            (220, 120)
        );
        let round = apply(sample(), "circle").unwrap().to_rgba8();
        assert_eq!(round.get_pixel(0, 0).0[3], 0, "corners become transparent");
        for op in [
            "grayscale",
            "invert",
            "blur 2",
            "sepia",
            "pixelate 8",
            "deepfry",
            "hue 90",
            "caption hello | world",
        ] {
            apply(sample(), op).unwrap_or_else(|e| panic!("{op}: {e}"));
        }
        assert!(apply(sample(), "explode").is_err());
    }

    #[test]
    fn captions_draw_pixels() {
        let blank =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(300, 200, Rgba([100, 100, 100, 255])));
        let captioned = caption(blank, "top", "").to_rgba8();
        assert!(captioned.pixels().any(|p| p.0 == [255, 255, 255, 255]));
        assert!(captioned.pixels().any(|p| p.0[0] < 20));
    }

    #[test]
    fn shapes_draw() {
        let mut canvas = RgbaImage::from_pixel(200, 120, Rgba([255, 255, 255, 255]));
        for shape in [
            "rect 10 10 50 30 blue",
            "rect 70 10 50 30 red outline width=4",
            "circle 150 30 20 green",
            "line 0 100 200 100 black width=3",
            "arrow 20 80 120 60 orange",
            "polygon purple 10 110 40 90 70 110",
            "text center 50 20 black Hi",
        ] {
            draw_shape(&mut canvas, shape).unwrap_or_else(|e| panic!("{shape}: {e}"));
        }
        assert_eq!(canvas.get_pixel(20, 20).0, [60, 120, 230, 255]);
        assert_eq!(canvas.get_pixel(150, 30).0, [60, 190, 100, 255]);
        assert!(draw_shape(&mut canvas, "star 1 2 3").is_err());
        assert!(draw_shape(&mut canvas, "rect 1 2").is_err());
    }

    #[test]
    fn charts_render() {
        let labels: Vec<String> = ["chat", "art", "games"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for kind in ["bar", "line", "pie"] {
            let image = chart(kind, "Messages", &labels, &[69.0, 44.0, 28.0], 600, 400);
            assert_eq!(image.dimensions(), (600, 400));
        }
    }

    #[test]
    fn bing_results_parse() {
        let html = r##"<a class="iusc" style="" m="{&quot;purl&quot;:&quot;https://x.org/page&quot;,&quot;murl&quot;:&quot;https://x.org/cow.jpg&quot;,&quot;t&quot;:&quot;A cow&quot;}" href="#">x</a>"##;
        let found = parse_bing_images(html, 5);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url, "https://x.org/cow.jpg");
        assert_eq!(found[0].title, "A cow");
    }

    #[test]
    fn inputs_found_by_their_names() {
        let input = |name: &str, primary| ImageSource {
            name: name.to_owned(),
            url: format!("https://cdn/{name}"),
            label: String::new(),
            primary,
        };
        let inputs = vec![
            input("message_stats.png", false),
            input("Screenshot_2026-10-30_191932.png", true),
        ];
        assert_eq!(
            find_input(&inputs, "message_stats.png").unwrap().name,
            "message_stats.png"
        );
        assert_eq!(
            find_input(&inputs, "message_stats").unwrap().name,
            "message_stats.png"
        );
        assert_eq!(
            find_input(&inputs, "this image").unwrap().name,
            "Screenshot_2026-10-30_191932.png"
        );
        assert_eq!(
            find_input(&inputs, "screenshot_2026-10-30_191932.PNG")
                .unwrap()
                .name,
            "Screenshot_2026-10-30_191932.png"
        );
        assert!(find_input(&inputs, "cow.jpg").is_none());
    }

    #[test]
    fn avatar_urls() {
        assert!(avatar_url(Some(1), 2, Some("abc"), None).contains("/avatars/2/abc.png"));
        assert!(
            avatar_url(Some(1), 2, Some("abc"), Some("def"))
                .contains("/guilds/1/users/2/avatars/def")
        );
        assert!(avatar_url(None, 2, None, None).contains("embed/avatars"));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn live_image_search_is_relevant() {
        for query in ["zebra", "red panda"] {
            let mut found = search_commons(query, 6).await.unwrap();
            found.extend(search_openverse(query, 6).await.unwrap_or_default());
            found.extend(
                search_bing(query, 8)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|i| relevant(i, query)),
            );
            assert!(found.len() >= 3, "{query}: {found:?}");
            for image in &found {
                println!("{query}: {} | {}", image.title, image.url);
            }
        }
    }

    #[test]
    fn off_topic_results_are_dropped() {
        let image = |title: &str, url: &str| FoundImage {
            title: title.into(),
            url: url.into(),
            page: String::new(),
        };
        assert!(relevant(
            &image("Zebra portrait", "https://x/1.jpg"),
            "zebra"
        ));
        assert!(relevant(
            &image("Plains zebras", "https://x/2.jpg"),
            "zebras"
        ));
        assert!(!relevant(
            &image("Long Sleeve Wool Dress", "https://etsy/il_1080.jpg"),
            "zebra"
        ));
        assert!(!relevant(
            &image("Class of '74 (1972)", "https://tmdb/x.jpg"),
            "a zebra"
        ));
    }

    /// `cargo test live_ -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_image_search_returns_working_links() {
        let candidates = search_bing("cow", 12).await.unwrap();
        println!("bing candidates: {}", candidates.len());
        let mut alive = 0;
        for image in candidates.iter().take(8) {
            let ok = super::super::web::is_live_image(&image.url).await;
            println!("{ok:5} {}", image.url);
            alive += ok as usize;
        }
        assert!(alive >= 2, "too few working links");
        assert!(
            !super::super::web::is_live_image(
                "https://images.unsplash.com/photo-1529187034699-4d8b914e0c2a?q=80"
            )
            .await
        );
        let commons = search_commons("cow", 3).await.unwrap();
        assert!(!commons.is_empty());
    }
}
