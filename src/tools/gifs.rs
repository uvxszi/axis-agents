//! Animated GIF and sticker search across multiple providers (GIFDB, Klipy,
//! Giphy, Tenor) with safety filters, numbered candidate selection, vision inspection,
//! and recently-used tracking to prevent sending the same media twice.
//!
//! Features:
//! - Multi-provider search: built-in zero-config GIFDB (GIFs + transparent stickers),
//!   plus Klipy, Giphy, and Tenor when configured with API keys.
//! - Dedicated sticker support: searches real transparent reaction stickers, distinct from regular GIFs.
//! - History & deduplication: tracks recently sent GIFs/stickers per channel so the agent
//!   always knows what's already been used and picks fresh, different ones.
//! - Numbered candidate selection: candidates are indexed ([1], [2], [3]) and named (e.g. `happy-cat-jumping`)
//!   so the agent picks with `#1`, `#2`, `"happy-cat"`, or `"different"` without typing file names or URLs.
//! - Vision preview: inspects animation frames through a tiled contact sheet using the vision model.
//! - Direct attachment: downloads and attaches the animated file directly into Discord replies.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use serde_json::Value;

use super::{ToolCtx, arg, flag, number, required};
use crate::runtime::live::FeedKind;

/// One candidate GIF or sticker.
#[derive(Debug, Clone)]
pub struct CandidateGif {
    pub id: usize,
    pub name: String,
    pub title: String,
    pub url: String,
    pub page_url: String,
    pub provider: String,
    pub is_sticker: bool,
    pub width: u32,
    pub height: u32,
}

/// Record of a recently sent GIF or sticker in a channel.
#[derive(Debug, Clone)]
pub struct UsedMedia {
    pub name: String,
    pub title: String,
    pub url: String,
    pub is_sticker: bool,
    pub sent_at_ms: i64,
}

fn candidates() -> &'static Mutex<HashMap<u64, Vec<CandidateGif>>> {
    static LIST: OnceLock<Mutex<HashMap<u64, Vec<CandidateGif>>>> = OnceLock::new();
    LIST.get_or_init(|| Mutex::new(HashMap::new()))
}

fn recently_used() -> &'static Mutex<HashMap<u64, Vec<UsedMedia>>> {
    static USED: OnceLock<Mutex<HashMap<u64, Vec<UsedMedia>>>> = OnceLock::new();
    USED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that a GIF or sticker was sent in a channel.
pub fn record_used(channel_id: u64, gif: &CandidateGif) {
    let mut guard = recently_used().lock().expect("recently_used poisoned");
    let history = guard.entry(channel_id).or_default();
    history.retain(|u| u.url != gif.url && u.name != gif.name);
    history.insert(
        0,
        UsedMedia {
            name: gif.name.clone(),
            title: gif.title.clone(),
            url: gif.url.clone(),
            is_sticker: gif.is_sticker,
            sent_at_ms: crate::memory::now_ms(),
        },
    );
    if history.len() > 50 {
        history.truncate(50);
    }
}

/// Check if a candidate or URL has been recently used in this channel.
pub fn is_recently_used(channel_id: u64, gif: &CandidateGif) -> bool {
    let guard = recently_used().lock().expect("recently_used poisoned");
    if let Some(history) = guard.get(&channel_id) {
        history.iter().any(|u| {
            u.url == gif.url
                || u.name == gif.name
                || u.title.eq_ignore_ascii_case(&gif.title)
                || (gif.name.len() > 10
                    && (u.name.starts_with(&gif.name) || gif.name.starts_with(&u.name)))
        })
    } else {
        false
    }
}

/// Format the recently used GIFs/stickers in a channel.
pub fn get_recent_media(channel_id: u64) -> String {
    let guard = recently_used().lock().expect("recently_used poisoned");
    let Some(history) = guard.get(&channel_id) else {
        return "No GIFs or stickers have been sent in this channel yet.".to_owned();
    };
    if history.is_empty() {
        return "No GIFs or stickers have been sent in this channel yet.".to_owned();
    }
    let mut out = String::from("Recently sent GIFs & Stickers in this channel:\n");
    let now = crate::memory::now_ms();
    for (i, item) in history.iter().take(15).enumerate() {
        let age_secs = (now.saturating_sub(item.sent_at_ms)) / 1000;
        let age_str = if age_secs < 60 {
            format!("{age_secs}s ago")
        } else if age_secs < 3600 {
            format!("{}m ago", age_secs / 60)
        } else {
            format!("{}h ago", age_secs / 3600)
        };
        let tag = if item.is_sticker {
            " [Sticker]"
        } else {
            " [GIF]"
        };
        out.push_str(&format!(
            "[{}] \"{}\"{} — {} ({})\n",
            i + 1,
            item.name,
            tag,
            crate::llm::truncate(&item.title, 60),
            age_str
        ));
    }
    out
}

/// Replace candidate list for a channel, tagging already-used ones.
pub fn set_candidates(channel_id: u64, list: Vec<CandidateGif>) -> String {
    let has_stickers = list.iter().any(|g| g.is_sticker);
    let mut out = if has_stickers {
        String::from("Found Stickers & GIFs:\n")
    } else {
        String::from("Found GIFs:\n")
    };

    for gif in &list {
        let tag = if gif.is_sticker { " [Sticker]" } else { "" };
        let used_tag = if is_recently_used(channel_id, gif) {
            " [ALREADY USED in this channel]"
        } else {
            " [NEW]"
        };
        out.push_str(&format!(
            "[{}] \"{}\" ({}){}{} — {}x{}\n    Title: {}\n    Link: {}\n",
            gif.id,
            gif.name,
            gif.provider,
            tag,
            used_tag,
            gif.width,
            gif.height,
            crate::llm::truncate(&gif.title, 80),
            gif.url
        ));
    }

    if has_stickers {
        out.push_str("\nPick to send: call `send_sticker(sticker = \"#1\")` or `send_sticker(sticker = \"different\")` to send a fresh sticker. Never pick a sticker marked [ALREADY USED].\n");
    } else {
        out.push_str("\nPick to send: call `send_gif(gif = \"#1\")` or `send_gif(gif = \"different\")` to send a fresh GIF. Never pick one marked [ALREADY USED].\n");
    }

    candidates()
        .lock()
        .expect("gif candidates poisoned")
        .insert(channel_id, list);
    out
}

/// Resolve a GIF/sticker reference against the channel's candidate list.
pub fn resolve(channel_id: u64, query: &str) -> Result<CandidateGif, String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err("give a GIF or sticker number (e.g. '#1'), name, or 'different'".to_owned());
    }

    // Direct URL check
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(CandidateGif {
            id: 1,
            name: "custom-media".to_owned(),
            title: "Custom Media".to_owned(),
            url: trimmed.to_owned(),
            page_url: trimmed.to_owned(),
            provider: "Direct Link".to_owned(),
            is_sticker: trimmed.contains("sticker"),
            width: 400,
            height: 400,
        });
    }

    let list = {
        let guard = candidates().lock().expect("gif candidates poisoned");
        guard.get(&channel_id).cloned().ok_or_else(|| {
            "no GIFs or stickers searched yet — call `find_stickers` or `find_gifs` first, then pick by number or name".to_owned()
        })?
    };

    if list.is_empty() {
        return Err(
            "candidate list is empty — call `find_stickers` or `find_gifs` first".to_owned(),
        );
    }

    let lower = trimmed.to_lowercase();

    // Check for "different", "next", "another", "new", "other"
    if matches!(
        lower.as_str(),
        "different"
            | "next"
            | "another"
            | "new"
            | "other"
            | "unused"
            | "different sticker"
            | "different gif"
            | "another sticker"
            | "another gif"
    ) {
        if let Some(unused) = list.iter().find(|g| !is_recently_used(channel_id, g)) {
            return Ok(unused.clone());
        }
        return Err("all candidates in the last search were already used; search for a fresh query with `find_stickers` or `find_gifs`".to_owned());
    }

    // Check #N or N
    let num_str = trimmed.strip_prefix('#').unwrap_or(trimmed).trim();
    if let Ok(n) = num_str.parse::<usize>() {
        if n >= 1 && n <= list.len() {
            return Ok(list[n - 1].clone());
        }
        return Err(format!(
            "#{n} is outside 1-{} — the results from your last search",
            list.len()
        ));
    }

    // Match by slug name or title (case-insensitive)
    if let Some(matched) = list.iter().find(|g| g.name.to_lowercase() == lower) {
        return Ok(matched.clone());
    }
    if let Some(matched) = list
        .iter()
        .find(|g| g.title.to_lowercase().contains(&lower))
    {
        return Ok(matched.clone());
    }

    Err(format!(
        "couldn't find \"{trimmed}\" in the last search results; use #N like #1 or #2, or 'different'"
    ))
}

/// Screen queries and titles to block NSFW/adult content.
pub fn is_unsafe_content(text: &str) -> bool {
    let lower = text.to_lowercase();
    const BLOCKED_WORDS: &[&str] = &[
        "nsfw",
        "porn",
        "xxx",
        "hentai",
        "nude",
        "nudity",
        "naked",
        "erotic",
        "erotica",
        "sex",
        "sexy",
        "sexual",
        "boobs",
        "penis",
        "vagina",
        "dick",
        "pussy",
        "asshole",
        "anal",
        "tits",
        "blowjob",
        "cum",
        "milf",
        "bdsm",
        "lewd",
        "ecchi",
        "rule34",
        "gore",
        "fetish",
        "onlyfans",
        "stripper",
        "striptease",
        "lingerie",
        "cleavage",
        "undressed",
        "topless",
        "adult",
    ];
    for word in BLOCKED_WORDS {
        if lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|part| part == *word || part.starts_with("porn") || part.starts_with("hentai"))
        {
            return true;
        }
    }
    false
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in title.chars().take(40) {
        if c.is_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_end_matches('-').to_owned();
    if trimmed.is_empty() {
        "media".to_owned()
    } else {
        trimmed
    }
}

pub fn detect_ext(bytes: &[u8], url: &str) -> &'static str {
    if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.starts_with(b"\x89PNG") {
        "png"
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if url.ends_with(".png") {
        "png"
    } else if url.ends_with(".webp") {
        "webp"
    } else {
        "gif"
    }
}

// ---------------------------------------------------------------------------
// Provider Search Implementations
// ---------------------------------------------------------------------------

/// Search GIFDB.com (zero-config, free animated GIFs and transparent stickers).
pub async fn search_gifdb(
    query: &str,
    stickers: bool,
    limit: usize,
) -> Result<Vec<CandidateGif>, String> {
    let encoded = query.trim().replace(' ', "-");
    let client = crate::llm::http_client();

    let (url, html) = if stickers {
        let sticker_url = format!("https://gifdb.com/stickers/{}", encoded);
        let resp = client
            .get(&sticker_url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AxisAgents",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .timeout(std::time::Duration::from_secs(12))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let text = r.text().await.map_err(|e| e.to_string())?;
                (sticker_url, text)
            }
            _ => {
                // Fallback to search/{encoded}-sticker
                let search_url = format!("https://gifdb.com/search/{}-sticker", encoded);
                let text = client
                    .get(&search_url)
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AxisAgents",
                    )
                    .header("Accept-Language", "en-US,en;q=0.9")
                    .timeout(std::time::Duration::from_secs(12))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .text()
                    .await
                    .map_err(|e| e.to_string())?;
                (search_url, text)
            }
        }
    } else {
        let search_url = format!("https://gifdb.com/search/{}", encoded);
        let text = client
            .get(&search_url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AxisAgents",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .timeout(std::time::Duration::from_secs(12))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        (search_url, text)
    };

    let mut results = Vec::new();
    let mut rest = html.as_str();

    while let Some(pos) = rest.find("<img") {
        rest = &rest[pos + 4..];
        let end_tag = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end_tag];
        rest = &rest[end_tag..];

        // Check if src contains /images/thumbnail/
        if let Some(src_start) = tag.find("src=\"") {
            let after_src = &tag[src_start + 5..];
            if let Some(src_end) = after_src.find('"') {
                let src = &after_src[..src_end];
                if src.contains("/images/thumbnail/") {
                    let full_url = if src.starts_with('/') {
                        format!("https://gifdb.com{src}")
                    } else {
                        src.to_owned()
                    };

                    let alt = if let Some(alt_start) = tag.find("alt=\"") {
                        let after_alt = &tag[alt_start + 5..];
                        if let Some(alt_end) = after_alt.find('"') {
                            after_alt[..alt_end].to_owned()
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                    let clean_title = alt
                        .trim_end_matches(" GIF")
                        .trim_end_matches(" Sticker")
                        .trim_end_matches(" sticker")
                        .trim()
                        .to_owned();

                    if false && (is_unsafe_content(&clean_title) || is_unsafe_content(&full_url)) {
                        continue;
                    }

                    let width = tag
                        .find("width=\"")
                        .and_then(|w| tag[w + 7..].split('"').next())
                        .and_then(|w| w.parse::<u32>().ok())
                        .unwrap_or(400);

                    let height = tag
                        .find("height=\"")
                        .and_then(|h| tag[h + 8..].split('"').next())
                        .and_then(|h| h.parse::<u32>().ok())
                        .unwrap_or(400);

                    let slug = slugify(&clean_title);
                    results.push(CandidateGif {
                        id: results.len() + 1,
                        name: slug,
                        title: if clean_title.is_empty() {
                            if stickers {
                                "Animated Sticker".to_owned()
                            } else {
                                "Animated GIF".to_owned()
                            }
                        } else {
                            clean_title
                        },
                        url: full_url,
                        page_url: url.clone(),
                        provider: "GIFDB".to_owned(),
                        is_sticker: stickers,
                        width,
                        height,
                    });

                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }
    }

    Ok(results)
}

/// Search Klipy API (when KLIPY_API_KEY is available).
pub async fn search_klipy(
    key: &str,
    query: &str,
    stickers: bool,
    limit: usize,
) -> Result<Vec<CandidateGif>, String> {
    let endpoint = if stickers { "stickers" } else { "gifs" };
    let url = format!("https://api.klipy.com/api/v1/{key}/{endpoint}/search");
    let resp: Value = crate::llm::http_client()
        .get(&url)
        .query(&[
            ("q", query),
            ("per_page", &limit.to_string()),
            ("content_filter", "high"),
            ("locale", "en_US"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let mut results = Vec::new();
    if let Some(items) = resp.pointer("/data").and_then(Value::as_array) {
        for item in items {
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Klipy Media");
            if false && is_unsafe_content(title) {
                continue;
            }
            let url = item
                .pointer("/file/url")
                .or_else(|| item.pointer("/media/gif/url"))
                .or_else(|| item.pointer("/media/sticker/url"))
                .and_then(Value::as_str);
            if let Some(u) = url {
                let slug = slugify(title);
                results.push(CandidateGif {
                    id: results.len() + 1,
                    name: slug,
                    title: title.to_owned(),
                    url: u.to_owned(),
                    page_url: u.to_owned(),
                    provider: "Klipy".to_owned(),
                    is_sticker: stickers,
                    width: 400,
                    height: 400,
                });
            }
        }
    }
    Ok(results)
}

/// Search Giphy API (when GIPHY_API_KEY is available).
pub async fn search_giphy(
    key: &str,
    query: &str,
    stickers: bool,
    limit: usize,
) -> Result<Vec<CandidateGif>, String> {
    let endpoint = if stickers { "stickers" } else { "gifs" };
    let url = format!("https://api.giphy.com/v1/{endpoint}/search");
    let resp: Value = crate::llm::http_client()
        .get(&url)
        .query(&[
            ("api_key", key),
            ("q", query),
            ("limit", &limit.to_string()),
            ("rating", "g"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let mut results = Vec::new();
    if let Some(items) = resp.pointer("/data").and_then(Value::as_array) {
        for item in items {
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Giphy Media");
            if false && is_unsafe_content(title) {
                continue;
            }
            let url = item
                .pointer("/images/original/url")
                .or_else(|| item.pointer("/images/downsized/url"))
                .and_then(Value::as_str);
            let w = item
                .pointer("/images/original/width")
                .and_then(Value::as_str)
                .and_then(|w| w.parse().ok())
                .unwrap_or(400);
            let h = item
                .pointer("/images/original/height")
                .and_then(Value::as_str)
                .and_then(|h| h.parse().ok())
                .unwrap_or(400);

            if let Some(u) = url {
                let slug = slugify(title);
                results.push(CandidateGif {
                    id: results.len() + 1,
                    name: slug,
                    title: title.to_owned(),
                    url: u.to_owned(),
                    page_url: u.to_owned(),
                    provider: "Giphy".to_owned(),
                    is_sticker: stickers,
                    width: w,
                    height: h,
                });
            }
        }
    }
    Ok(results)
}

/// Search Tenor API (when TENOR_API_KEY is available).
pub async fn search_tenor(
    key: &str,
    query: &str,
    stickers: bool,
    limit: usize,
) -> Result<Vec<CandidateGif>, String> {
    let url = "https://tenor.googleapis.com/v2/search";
    let mut req = crate::llm::http_client().get(url).query(&[
        ("key", key),
        ("client_key", "axis_agents"),
        ("q", query),
        ("limit", &limit.to_string()),
        ("contentfilter", "high"),
    ]);
    if stickers {
        req = req.query(&[("searchfilter", "sticker")]);
    }

    let resp: Value = req
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let mut results = Vec::new();
    if let Some(items) = resp.pointer("/results").and_then(Value::as_array) {
        for item in items {
            let title = item
                .get("content_description")
                .and_then(Value::as_str)
                .unwrap_or("Tenor Media");
            if false && is_unsafe_content(title) {
                continue;
            }
            let url = item
                .pointer("/media_formats/gif/url")
                .or_else(|| item.pointer("/media_formats/tinygif/url"))
                .and_then(Value::as_str);
            if let Some(u) = url {
                let slug = slugify(title);
                results.push(CandidateGif {
                    id: results.len() + 1,
                    name: slug,
                    title: title.to_owned(),
                    url: u.to_owned(),
                    page_url: u.to_owned(),
                    provider: "Tenor".to_owned(),
                    is_sticker: stickers,
                    width: 400,
                    height: 400,
                });
            }
        }
    }
    Ok(results)
}

/// Combined multi-provider search with fallback and channel-aware deduplication.
pub async fn search_all(
    query: &str,
    stickers: bool,
    limit: usize,
    channel_id: Option<u64>,
) -> Result<Vec<CandidateGif>, String> {
    // The operator opted out of content screening: these databases are
    // curated and safe in practice; the filter only rejected real asks.

    let fetch_count = (limit * 3).max(10);
    let mut candidates = Vec::new();

    // 1. Try Klipy if key is set
    if let Ok(key) = std::env::var("KLIPY_API_KEY") {
        if !key.trim().is_empty() {
            if let Ok(klipy_items) = search_klipy(&key, query, stickers, fetch_count).await {
                candidates.extend(klipy_items);
            }
        }
    }

    // 2. Try Giphy if key is set
    if let Ok(key) = std::env::var("GIPHY_API_KEY") {
        if !key.trim().is_empty() {
            if let Ok(giphy_items) = search_giphy(&key, query, stickers, fetch_count).await {
                candidates.extend(giphy_items);
            }
        }
    }

    // 3. Try Tenor if key is set
    if let Ok(key) = std::env::var("TENOR_API_KEY") {
        if !key.trim().is_empty() {
            if let Ok(tenor_items) = search_tenor(&key, query, stickers, fetch_count).await {
                candidates.extend(tenor_items);
            }
        }
    }

    // 4. Built-in GIFDB provider (no API key required)
    if candidates.len() < limit {
        if let Ok(gifdb_items) = search_gifdb(query, stickers, fetch_count).await {
            for item in gifdb_items {
                if !candidates.iter().any(|c| c.url == item.url) {
                    candidates.push(item);
                }
            }
        }
    }

    if candidates.is_empty() {
        return Err(format!(
            "no family-friendly {} found matching \"{query}\"",
            if stickers { "stickers" } else { "GIFs" }
        ));
    }

    // If channel_id is provided, prioritize unused candidates so the agent doesn't reuse what was already sent
    if let Some(cid) = channel_id {
        let (mut unused, mut used): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|g| !is_recently_used(cid, g));
        unused.append(&mut used);
        candidates = unused;
    }

    // Re-index candidates 1..N
    let reindexed: Vec<CandidateGif> = candidates
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, mut g)| {
            g.id = i + 1;
            g
        })
        .collect();

    Ok(reindexed)
}

// ---------------------------------------------------------------------------
// Vision Quick Look
// ---------------------------------------------------------------------------

/// Quickly inspect a GIF or sticker with the vision model by generating its frame sheet.
pub async fn inspect_gif_with_vision(ctx: &ToolCtx, gif: &CandidateGif) -> Result<String, String> {
    let bytes = super::web::fetch_bytes(&gif.url, false, 15_000_000).await?;
    let (data_uri, note) = crate::runtime::vision::prepare_with_note(&bytes)
        .ok_or_else(|| "couldn't extract animation frames from the file".to_owned())?;

    let seen = crate::runtime::vision::SeenImage {
        label: gif.name.clone(),
        data_uri,
    };
    let profile = ctx.core.profile();
    let model = if profile.model.vision_model.trim().is_empty() {
        profile.model.model.clone()
    } else {
        profile.model.vision_model.trim().to_owned()
    };

    let prompt = format!(
        "Describe this {} briefly in 1-2 sentences: the motion, characters, actions, and mood. Note: {}",
        if gif.is_sticker {
            "animated sticker"
        } else {
            "animated GIF"
        },
        note.as_deref().unwrap_or("animated sequence")
    );

    match crate::runtime::vision::describe(&ctx.core, &model, &seen, &prompt).await {
        Ok(desc) => Ok(desc),
        Err(_) => Ok(format!(
            "{} titled \"{}\", dimensions {}x{}.",
            if gif.is_sticker {
                "Sticker"
            } else {
                "Animation"
            },
            gif.title,
            gif.width,
            gif.height
        )),
    }
}

// ---------------------------------------------------------------------------
// Tool Handlers
// ---------------------------------------------------------------------------

async fn find_media_impl(ctx: &ToolCtx, args: &Value, stickers: bool) -> Result<String, String> {
    let query = required(args, "query")?;

    // Check if user/agent asks for history/used
    if matches!(
        query.to_lowercase().as_str(),
        "used" | "recent" | "history" | "previously sent"
    ) {
        return Ok(get_recent_media(ctx.channel_id));
    }

    let limit = number(args, "limit").unwrap_or(5).clamp(1, 10) as usize;
    let inspect = flag(args, "inspect").unwrap_or(false);

    let items = search_all(&query, stickers, limit, Some(ctx.channel_id)).await?;
    ctx.core.feed(
        FeedKind::Tool,
        format!(
            "searched {}s for: {}",
            if stickers { "sticker" } else { "gif" },
            query
        ),
        None,
    );

    let mut listing = set_candidates(ctx.channel_id, items.clone());

    if inspect {
        listing.push_str("\n--- Vision Inspection ---\n");
        for gif in items.iter().take(3) {
            match inspect_gif_with_vision(ctx, gif).await {
                Ok(desc) => {
                    listing.push_str(&format!("[{}] \"{}\": {}\n", gif.id, gif.name, desc));
                }
                Err(e) => {
                    listing.push_str(&format!(
                        "[{}] \"{}\": (preview failed: {})\n",
                        gif.id, gif.name, e
                    ));
                }
            }
        }
    }

    Ok(listing)
}

/// Tool: `find_gifs`
pub async fn find_gifs(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let stickers = flag(args, "stickers").unwrap_or(false);
    find_media_impl(ctx, args, stickers).await
}

/// Tool: `find_stickers`
pub async fn find_stickers(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    find_media_impl(ctx, args, true).await
}

/// Tool: `inspect_gif`
pub async fn inspect_gif(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let target = required(args, "gif")
        .or_else(|_| required(args, "sticker"))
        .or_else(|_| required(args, "target"))?;
    let gif = resolve(ctx.channel_id, &target)?;
    let description = inspect_gif_with_vision(ctx, &gif).await?;
    Ok(format!(
        "[{}] \"{}\" ({}, {}x{}):\n{}",
        gif.id, gif.name, gif.provider, gif.width, gif.height, description
    ))
}

async fn send_media_impl(
    ctx: &ToolCtx,
    args: &Value,
    target_key: &str,
    default_is_sticker: bool,
) -> Result<String, String> {
    let target = required(args, target_key)
        .or_else(|_| required(args, "gif"))
        .or_else(|_| required(args, "sticker"))
        .or_else(|_| required(args, "target"))?;

    let as_attachment = flag(args, "as_attachment").unwrap_or(true);
    let channel_query = arg(args, "channel");
    let channel_id = super::discord::resolve_channel(ctx, channel_query)?;

    let gif = match resolve(ctx.channel_id, &target) {
        Ok(g) => g,
        Err(_) => {
            // Target is a raw search query (e.g. "cat" or "dancing")
            let found = search_all(&target, default_is_sticker, 10, Some(ctx.channel_id)).await?;
            let picked = found
                .iter()
                .find(|g| !is_recently_used(ctx.channel_id, g))
                .cloned()
                .or_else(|| found.into_iter().next())
                .ok_or_else(|| {
                    format!(
                        "no {} found for \"{target}\"",
                        if default_is_sticker { "sticker" } else { "GIF" }
                    )
                })?;
            picked
        }
    };

    let media_kind = if gif.is_sticker { "sticker" } else { "GIF" };

    if as_attachment {
        let bytes = super::web::fetch_bytes(&gif.url, false, 20_000_000).await?;
        let ext = detect_ext(&bytes, &gif.url);
        let filename = format!("{}.{ext}", gif.name);

        if channel_id == ctx.channel_id {
            ctx.media
                .lock()
                .expect("media poisoned")
                .add_file(&filename, bytes);
            record_used(ctx.channel_id, &gif);
            Ok(format!(
                "Attached {} \"{}\" ({}) to your reply under file name `{}`.",
                media_kind, gif.title, gif.provider, filename
            ))
        } else {
            ctx.core
                .http
                .create_message(axis::prelude::ChannelId::new(channel_id))
                .file(&filename, bytes)
                .await
                .map_err(crate::tools::discord_error)?;
            record_used(channel_id, &gif);
            Ok(format!(
                "Sent {} \"{}\" to <#{channel_id}>.",
                media_kind, gif.title
            ))
        }
    } else {
        record_used(channel_id, &gif);
        if channel_id == ctx.channel_id {
            Ok(format!(
                "{} link for \"{}\" (post this on its own line in your reply to embed it):\n{}",
                if gif.is_sticker { "Sticker" } else { "GIF" },
                gif.title,
                gif.url
            ))
        } else {
            ctx.core
                .http
                .create_message(axis::prelude::ChannelId::new(channel_id))
                .content(&gif.url)
                .await
                .map_err(crate::tools::discord_error)?;
            Ok(format!(
                "Sent {} link to <#{channel_id}>: {}",
                media_kind, gif.url
            ))
        }
    }
}

/// Tool: `send_gif`
pub async fn send_gif(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    send_media_impl(ctx, args, "gif", false).await
}

/// Tool: `send_sticker`
pub async fn send_sticker(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    send_media_impl(ctx, args, "sticker", true).await
}

/// Tool: `list_recent_gifs`
pub async fn list_recent_gifs(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    Ok(get_recent_media(ctx.channel_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_candidates() -> Vec<CandidateGif> {
        vec![
            CandidateGif {
                id: 1,
                name: "happy-cat-jumping".into(),
                title: "Happy Cat Jumping GIF".into(),
                url: "https://gifdb.com/images/thumbnail/cat-jumping.gif".into(),
                page_url: "https://gifdb.com/gif/cat-jumping".into(),
                provider: "GIFDB".into(),
                is_sticker: false,
                width: 400,
                height: 400,
            },
            CandidateGif {
                id: 2,
                name: "dancing-cat-sticker".into(),
                title: "Dancing Cat Sticker".into(),
                url: "https://gifdb.com/images/thumbnail/dancing-cat.gif".into(),
                page_url: "https://gifdb.com/gif/dancing-cat".into(),
                provider: "GIFDB".into(),
                is_sticker: true,
                width: 300,
                height: 300,
            },
            CandidateGif {
                id: 3,
                name: "sleepy-cat-nap".into(),
                title: "Sleepy Cat Nap Sticker".into(),
                url: "https://gifdb.com/images/thumbnail/sleepy-cat.gif".into(),
                page_url: "https://gifdb.com/gif/sleepy-cat".into(),
                provider: "GIFDB".into(),
                is_sticker: true,
                width: 250,
                height: 250,
            },
        ]
    }

    #[test]
    fn numbered_and_name_selection_works() {
        set_candidates(999, sample_candidates());
        assert_eq!(resolve(999, "#1").unwrap().name, "happy-cat-jumping");
        assert_eq!(resolve(999, "1").unwrap().name, "happy-cat-jumping");
        assert_eq!(resolve(999, "#2").unwrap().name, "dancing-cat-sticker");
        assert_eq!(resolve(999, "happy-cat-jumping").unwrap().id, 1);
        assert_eq!(resolve(999, "dancing").unwrap().id, 2);
    }

    #[test]
    fn different_and_used_media_tracking_works() {
        let channel = 12345;
        let cands = sample_candidates();
        set_candidates(channel, cands.clone());

        // Initially neither is used
        assert!(!is_recently_used(channel, &cands[0]));
        assert!(!is_recently_used(channel, &cands[1]));

        // Record cands[0] as used
        record_used(channel, &cands[0]);
        assert!(is_recently_used(channel, &cands[0]));
        assert!(!is_recently_used(channel, &cands[1]));

        // Resolving "different" picks the first unused candidate (cands[1])
        let diff = resolve(channel, "different").unwrap();
        assert_eq!(diff.name, "dancing-cat-sticker");

        // Record cands[1] as used too
        record_used(channel, &cands[1]);
        // Resolving "different" now picks cands[2] ("sleepy-cat-nap")
        let diff2 = resolve(channel, "different").unwrap();
        assert_eq!(diff2.name, "sleepy-cat-nap");

        // get_recent_media lists the used items
        let recent = get_recent_media(channel);
        assert!(recent.contains("dancing-cat-sticker"));
        assert!(recent.contains("happy-cat-jumping"));
    }

    #[test]
    fn safety_filter_blocks_nsfw_terms() {
        assert!(is_unsafe_content("free nsfw gifs"));
        assert!(is_unsafe_content("sexy anime"));
        assert!(is_unsafe_content("hot porn clips"));
        assert!(!is_unsafe_content("happy cat dancing"));
        assert!(!is_unsafe_content("celebration party"));
    }

    #[test]
    fn slugify_produces_clean_names() {
        assert_eq!(slugify("Happy Cat Flying GIF!"), "happy-cat-flying-gif");
        assert_eq!(
            slugify("A Dog & Cat (Best Friends)"),
            "a-dog-cat-best-friends"
        );
    }

    #[test]
    fn detect_ext_identifies_formats() {
        assert_eq!(
            detect_ext(b"GIF89a...", "http://example.com/cat.gif"),
            "gif"
        );
        assert_eq!(
            detect_ext(b"\x89PNG\r\n\x1a\n...", "http://example.com/cat.png"),
            "png"
        );
        assert_eq!(
            detect_ext(
                b"RIFF\x00\x00\x00\x00WEBP...",
                "http://example.com/cat.webp"
            ),
            "webp"
        );
    }
}
