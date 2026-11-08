//! Seeing images: attachments, image links, and GIF/link embeds in the
//! messages an agent answers.
//!
//! Images are downloaded, downscaled, and inlined as base64 (local servers
//! like LM Studio reject URLs). GIFs become a 2×2 sheet of frames so the
//! model gets a sense of the motion.
//!
//! With a dedicated vision model configured, that model describes each
//! image and the main model reads the descriptions — so a text-only main
//! model still "sees", and a small vision model does the looking.

use std::io::Cursor;

use axis::model::Message;
use axis::prelude::MessageId;
use image::{AnimationDecoder, DynamicImage, ImageFormat, imageops};

use super::agent::AgentCore;
use crate::llm::{ChatRequest, Msg, Part};

const MAX_IMAGES: usize = 4;
const MAX_BYTES: usize = 15_000_000;

pub struct SeenImage {
    pub label: String,
    pub data_uri: String,
}

/// What the prompt gets: inline images and/or text descriptions.
#[derive(Default)]
pub struct Sight {
    pub images: Vec<String>,
    pub notes: Vec<String>,
}

fn looks_like_image_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn links(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| matches!(c, '<' | '>' | '(' | ')' | '"' | '\'')))
        .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
        .map(str::to_owned)
        .collect()
}

/// An image the agent can look at or work with.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageSource {
    /// What it's called: the file name when there is one.
    pub name: String,
    pub url: String,
    /// "image cat.png attached by Alice" — for the prompt.
    pub label: String,
    /// On the message being answered (vs. a reply target or recent chat).
    pub primary: bool,
}

fn file_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    (looks_like_image_url(last) && last.len() <= 80).then(|| last.to_owned())
}

/// Image URLs in one message, named and labeled.
fn candidates(message: &Message) -> Vec<(String, String, String)> {
    let who = super::agent::display_name(message);
    let mut out: Vec<(String, String, String)> = Vec::new();
    for attachment in &message.attachments {
        let is_image = attachment
            .content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("image/"))
            || looks_like_image_url(&attachment.filename);
        if is_image && attachment.size as usize <= MAX_BYTES {
            out.push((
                attachment.filename.clone(),
                attachment.url.clone(),
                format!("image {} posted by {who}", attachment.filename),
            ));
        }
    }
    for url in message.media_urls() {
        let url = animated_variant(&url).unwrap_or(url);
        if !out.iter().any(|(_, u, _)| *u == url) {
            let name = file_name(&url).unwrap_or_else(|| "embedded image".to_owned());
            out.push((
                name.clone(),
                url,
                format!("image {name} in the card/embed posted by {who}"),
            ));
        }
    }
    for link in links(&message.content) {
        if looks_like_image_url(&link) && !out.iter().any(|(_, u, _)| *u == link) {
            let name = file_name(&link).unwrap_or_else(|| "linked image".to_owned());
            out.push((name.clone(), link, format!("image {name} linked by {who}")));
        }
    }
    for sticker in message.sticker_items.iter().flatten() {
        if let Some(url) = sticker.image_url() {
            out.push((
                format!("sticker {}", sticker.name),
                url,
                format!("the sticker \"{}\" sent by {who}", sticker.name),
            ));
        }
    }
    // Emojis are everywhere: look at them only when they're the point —
    // the message is (nearly) just emojis, or asks about one.
    let emojis = custom_emojis(&message.content);
    if !emojis.is_empty() {
        let mut words = message.content.clone();
        while let (Some(a), Some(b)) = (words.find("<"), words.find(">")) {
            if b < a {
                break;
            }
            words.replace_range(a..=b, "");
        }
        let lower = message.content.to_lowercase();
        let about = ["emoji", "emote", "this", "what", "mean"]
            .iter()
            .any(|w| lower.contains(w));
        if words.trim().chars().count() <= 20 || about {
            for (name, url) in emojis.into_iter().take(3) {
                out.push((
                    format!(":{name}:"),
                    url,
                    format!("the custom emoji :{name}: used by {who}"),
                ));
            }
        }
    }
    out
}

/// Every image the agent could mean right now: on the messages being
/// answered first, then on the message they reply to, then in the last
/// few messages of the channel (people and bots alike).
pub fn sources(core: &AgentCore, triggers: &[Message]) -> Vec<ImageSource> {
    let mut out: Vec<ImageSource> = Vec::new();
    let push =
        |name: String, url: String, label: String, primary: bool, out: &mut Vec<ImageSource>| {
            if !out.iter().any(|s| s.url == url) {
                out.push(ImageSource {
                    name,
                    url,
                    label,
                    primary,
                });
            }
        };
    for message in triggers {
        for (name, url, label) in candidates(message) {
            push(name, url, label, true, &mut out);
        }
    }
    for message in triggers {
        if let Some(reply) = message.referenced_message.as_deref() {
            for (name, url, label) in candidates(reply) {
                push(
                    name,
                    url,
                    format!("{label} (the message being replied to)"),
                    false,
                    &mut out,
                );
            }
        }
    }
    if let Some(first) = triggers.first() {
        let me = core.world.my_id();
        let trigger_ids: Vec<u64> = triggers.iter().map(|m| m.id.get()).collect();
        let recent: Vec<(String, String, String)> =
            core.hive
                .knowledge
                .channel(first.channel_id.get(), |archive| {
                    archive
                        .messages
                        .iter()
                        .rev()
                        .filter(|m| !m.deleted && m.author_id != me && !trigger_ids.contains(&m.id))
                        .take(8)
                        .flat_map(|m| {
                            m.attachments
                                .iter()
                                .filter(|a| {
                                    a.is_image()
                                        || a.name == "embedded image"
                                        || looks_like_image_url(&a.url)
                                })
                                .map(|a| {
                                    let name = if a.name == "embedded image" {
                                        file_name(&a.url).unwrap_or_else(|| a.name.clone())
                                    } else {
                                        a.name.clone()
                                    };
                                    (
                                        name.clone(),
                                        a.url.clone(),
                                        format!("image {name} recently posted by {}", m.author),
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect()
                });
        for (name, url, label) in recent {
            push(name, url, label, false, &mut out);
        }
    }
    // Make names unique so each can be referred to.
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    for source in &mut out {
        let count = seen.entry(source.name.to_lowercase()).or_insert(0);
        *count += 1;
        if *count > 1 {
            source.name = format!("{} ({})", source.name, count);
        }
    }
    out
}

/// Downscale and inline an image; GIFs become a frame sheet.
pub fn prepare(bytes: &[u8]) -> Option<String> {
    prepare_with_note(bytes).map(|(uri, _)| uri)
}

/// Like `prepare`, plus a note for animated GIFs on how their frames are
/// laid out in the picture.
pub fn prepare_with_note(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let format = image::guess_format(bytes).ok()?;
    let (image, note) = match format {
        ImageFormat::Gif => match gif_sheet(bytes) {
            Some((sheet, note)) => (sheet, Some(note)),
            None => (image::load_from_memory(bytes).ok()?, None),
        },
        _ => (image::load_from_memory(bytes).ok()?, None),
    };
    let image = if image.width().max(image.height()) > 1024 {
        image.resize(1024, 1024, imageops::FilterType::Triangle)
    } else {
        image
    };
    let mut jpeg = Vec::new();
    DynamicImage::ImageRgb8(image.to_rgb8())
        .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
        .ok()?;
    Some((
        format!("data:image/jpeg;base64,{}", super::brain::base64(&jpeg)),
        note,
    ))
}

/// Evenly spaced frames of an animated GIF — the first, the last, and
/// some in between — tiled in reading order, with a note saying so.
fn gif_sheet(bytes: &[u8]) -> Option<(DynamicImage, String)> {
    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).ok()?;
    let frames: Vec<_> = decoder
        .into_frames()
        .take(600)
        .filter_map(Result::ok)
        .collect();
    if frames.len() < 2 {
        return None;
    }
    let total_ms: u64 = frames
        .iter()
        .map(|f| {
            let (n, d) = f.delay().numer_denom_ms();
            (n as u64 / d.max(1) as u64).max(20)
        })
        .sum();
    // Long GIFs get six frames (3×2), short ones four (2×2).
    let (count, columns) = if frames.len() >= 24 {
        (6usize, 3u32)
    } else {
        (4, 2)
    };
    let count = count.min(frames.len());
    let picks: Vec<_> = (0..count)
        .map(|i| &frames[(i * (frames.len() - 1)) / (count - 1)])
        .collect();
    let (w, h) = picks[0].buffer().dimensions();
    let rows = (count as u32).div_ceil(columns);
    let gap = 6;
    let mut sheet = image::RgbaImage::from_pixel(
        w * columns + gap * (columns - 1),
        h * rows + gap * (rows - 1),
        image::Rgba([255, 255, 255, 255]),
    );
    for (index, frame) in picks.iter().enumerate() {
        let x = (index as u32 % columns) * (w + gap);
        let y = (index as u32 / columns) * (h + gap);
        imageops::overlay(&mut sheet, frame.buffer(), x as i64, y as i64);
    }
    let note = format!(
        "an animated GIF ({} frames, about {:.1}s long), shown as {count} frames in a {columns}×{rows} grid read left to right, top to bottom: the first is the start, the last is the end",
        frames.len(),
        total_ms as f32 / 1000.0
    );
    Some((DynamicImage::ImageRgba8(sheet), note))
}

/// Tenor and Giphy embeds come as a still preview or an MP4; their GIF
/// version is what shows the motion.
pub fn animated_variant(url: &str) -> Option<String> {
    let base = url.split(['?', '#']).next()?;
    if base.contains("media.tenor.com/") || base.contains("tenor.com/") && base.contains("/media/")
    {
        // .../{id}{5-letter format code}/{slug}.{ext}; AAAAC is the GIF.
        let mut parts: Vec<String> = base.split('/').map(str::to_owned).collect();
        let n = parts.len();
        if n >= 2 && parts[n - 2].len() > 5 {
            let key = &parts[n - 2];
            parts[n - 2] = format!("{}AAAAC", &key[..key.len() - 5]);
            let file = &parts[n - 1];
            let stem = file
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(file)
                .to_owned();
            parts[n - 1] = format!("{stem}.gif");
            return Some(parts.join("/"));
        }
    }
    if base.contains("giphy.com/media/") {
        let id = base.split("/media/").nth(1)?.split('/').next()?;
        return Some(format!("https://media.giphy.com/media/{id}/giphy.gif"));
    }
    None
}

/// Custom emojis written in a message: (name, image URL).
pub fn custom_emojis(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('>') else { break };
        let inner = &tail[..end];
        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() == 3
            && (parts[0].is_empty() || parts[0] == "a")
            && parts[2].chars().all(|c| c.is_ascii_digit())
            && !parts[2].is_empty()
        {
            let ext = if parts[0] == "a" { "gif" } else { "png" };
            let url = format!(
                "https://cdn.discordapp.com/emojis/{}.{ext}?size=128",
                parts[2]
            );
            if !out.iter().any(|(_, u)| *u == url) {
                out.push((parts[1].to_owned(), url));
            }
        }
        rest = &tail[end + 1..];
    }
    out
}

/// Collect and prepare the images in the messages being answered — or,
/// when they have none, the ones they most likely refer to.
pub async fn gather(core: &AgentCore, triggers: &[Message]) -> Vec<SeenImage> {
    let mut messages = triggers.to_vec();
    for message in &mut messages {
        // Discord resolves link embeds (Tenor, Giphy, Imgur…) a moment
        // after the message arrives; refetch to pick them up.
        if !links(&message.content).is_empty() && message.embeds.is_empty() {
            if let Ok(fresh) = core
                .http
                .get_message(message.channel_id, MessageId::new(message.id.get()))
                .await
            {
                *message = fresh;
            }
        }
    }
    let all = sources(core, &messages);
    let primary: Vec<&ImageSource> = all.iter().filter(|s| s.primary).collect();
    // Without images of its own, the message is probably about the one it
    // replies to, or the latest one posted.
    let chosen: Vec<&ImageSource> = if primary.is_empty() {
        all.iter().take(1).collect()
    } else {
        primary
    };
    let mut seen = Vec::new();
    for source in chosen.into_iter().take(MAX_IMAGES) {
        let bytes = match crate::tools::web::fetch_bytes(&source.url, false, MAX_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) if source.url.contains("AAAAC/") => {
                let still = source
                    .url
                    .replace("AAAAC/", "AAAAe/")
                    .replace(".gif", ".png");
                match crate::tools::web::fetch_bytes(&still, false, MAX_BYTES).await {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };
        if let Some((data_uri, note)) = prepare_with_note(&bytes) {
            seen.push(SeenImage {
                label: format!(
                    "{}{} — refer to it as \"{}\" in image tools",
                    source.label,
                    note.map(|n| format!(" ({n})")).unwrap_or_default(),
                    source.name
                ),
                data_uri,
            });
        }
    }
    seen
}

/// Ask a vision model to describe one image.
pub async fn describe(
    core: &AgentCore,
    model: &str,
    image: &SeenImage,
    question: &str,
) -> Result<String, String> {
    let messages = [
        Msg::system(
            "You are the eyes of an assistant that cannot see. Describe the image precisely: what it shows, any text in it (transcribe it), people/characters, UI elements, colors, mood, and anything notable. For a 2x2 grid of frames, it's an animated GIF — describe the motion. Be factual and concise (under 120 words).",
        ),
        Msg::user_parts(vec![
            Part::Text(question.to_owned()),
            Part::Image(image.data_uri.clone()),
        ]),
    ];
    let effort = core.profile().model.thinking.vision.effort();
    let endpoint = core.endpoint();
    let mut output = None;
    // With thinking on, a model can spend the whole budget reasoning and
    // say nothing; then look again with thinking off.
    for reasoning in [effort, Some("none")] {
        let request = ChatRequest {
            thinking_budget: None,
            model,
            messages: &messages,
            tools: &[],
            temperature: 0.2,
            max_tokens: if reasoning == Some("none") { 600 } else { 2500 },
            stream: false,
            json: false,
            reasoning,
        };
        let permit = core.hive.endpoint_permit(&endpoint.limit_key()).await;
        let result = endpoint.chat(&request, &mut |_| {}).await;
        drop(permit);
        core.live().stat(&core.id, |s| s.llm_calls += 1);
        let done = result.as_ref().is_ok_and(|o| !o.content.trim().is_empty());
        output = Some(result);
        if done || reasoning == Some("none") {
            break;
        }
    }
    let result = output.expect("at least one attempt");
    let output = result.map_err(|e| e.to_string())?;
    let (text, reasoning) = crate::llm::text::split_think(&output.content);
    let text = text.trim();
    if !text.is_empty() {
        return Ok(text.to_owned());
    }
    // Some servers ignore the thinking switch: use the end of the
    // reasoning rather than nothing.
    let reasoning = if output.reasoning.trim().is_empty() {
        reasoning
    } else {
        output.reasoning
    };
    if reasoning.trim().is_empty() {
        return Err("the vision model returned an empty description".to_owned());
    }
    let tail: String = {
        let chars: Vec<char> = reasoning.trim().chars().collect();
        chars[chars.len().saturating_sub(700)..].iter().collect()
    };
    Ok(format!("(from its notes) {tail}"))
}

/// What the main model gets for the images in this job.
pub async fn look(core: &AgentCore, triggers: &[Message]) -> Sight {
    let profile = core.profile();
    let vision_model = profile.model.vision_model.trim().to_owned();
    let main_sees = core.vision_enabled();
    if vision_model.is_empty() && !main_sees {
        // Can't see: at least say an image is there.
        let notes = sources(core, triggers)
            .into_iter()
            .filter(|s| s.primary)
            .map(|s| format!("[{} — you can't see images; say so if it matters]", s.label))
            .collect();
        return Sight {
            images: Vec::new(),
            notes,
        };
    }
    let seen = gather(core, triggers).await;
    if seen.is_empty() {
        return Sight::default();
    }
    if vision_model.is_empty() {
        return Sight {
            images: seen.iter().map(|s| s.data_uri.clone()).collect(),
            notes: seen
                .iter()
                .map(|s| format!("[{} is shown below]", s.label))
                .collect(),
        };
    }
    let mut notes = Vec::new();
    for image in &seen {
        match describe(core, &vision_model, image, "Describe this image.").await {
            Ok(text) => notes.push(format!("[{} — what it shows: {text}]", image.label)),
            Err(error) => notes.push(format!("[{} — couldn't be viewed: {error}]", image.label)),
        }
    }
    Sight {
        images: Vec::new(),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_custom_emojis_and_gif_versions() {
        let emojis = custom_emojis("lol <:pepe:123> and <a:spin:456> but not <@789> or <#1>");
        assert_eq!(emojis.len(), 2);
        assert_eq!(emojis[0].0, "pepe");
        assert!(emojis[1].1.ends_with("456.gif?size=128"));
        assert_eq!(
            animated_variant("https://media.tenor.com/abcdEFGHijkAAAAe/cat-dance.png").as_deref(),
            Some("https://media.tenor.com/abcdEFGHijkAAAAC/cat-dance.gif")
        );
        assert_eq!(
            animated_variant("https://media2.giphy.com/media/XyZ/giphy.mp4?cid=1").as_deref(),
            Some("https://media.giphy.com/media/XyZ/giphy.gif")
        );
        assert!(animated_variant("https://cdn.discordapp.com/x.png").is_none());
    }

    #[test]
    fn gifs_become_a_labeled_frame_grid() {
        use image::codecs::gif::GifEncoder;
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for i in 0..8u8 {
                let frame = image::RgbaImage::from_pixel(20, 10, image::Rgba([i * 30, 0, 0, 255]));
                encoder.encode_frame(image::Frame::new(frame)).unwrap();
            }
        }
        let (uri, note) = prepare_with_note(&bytes).unwrap();
        assert!(uri.starts_with("data:image/jpeg"));
        let note = note.unwrap();
        assert!(note.contains("8 frames") && note.contains("2×2"), "{note}");
    }

    #[test]
    fn image_links_are_recognized() {
        assert!(looks_like_image_url(
            "https://cdn.discordapp.com/a/b/cat.PNG?ex=1"
        ));
        assert!(looks_like_image_url("https://media.tenor.com/x.gif"));
        assert!(!looks_like_image_url("https://tenor.com/view/dance-123"));
        assert_eq!(
            links("look <https://x.com/a.png> and (https://y.org)"),
            vec!["https://x.com/a.png", "https://y.org"]
        );
    }

    #[test]
    fn gifs_become_frame_sheets() {
        use image::GenericImageView;
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, Rgba, RgbaImage};
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for shade in [0u8, 80, 160, 240] {
                let frame = Frame::from_parts(
                    RgbaImage::from_pixel(20, 10, Rgba([shade, 0, 0, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                );
                encoder.encode_frame(frame).unwrap();
            }
        }
        let (sheet, _) = gif_sheet(&bytes).unwrap();
        // Two columns and rows of 20×10 frames, 6px apart.
        assert_eq!(sheet.dimensions(), (46, 26));
        assert!(
            prepare(&bytes)
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
    }
}
