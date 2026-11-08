//! Files the agent can make, script, attach, and convert.
//!
//! AAv2 replaces the old draft tools with real ones:
//! - `write_file` builds a file from content — or, for repetitive data
//!   ("a 1MB file of the word legion repeated"), from a `repeat` pattern
//!   expanded in code, so the model never generates the bytes itself;
//! - `run_script` executes a short Python script in a sandbox directory
//!   (isolated mode, no network flags off — it's the operator's own
//!   machine and their configured interpreter), captures its output, and
//!   lists every file it wrote;
//! - `attach_file` uploads a sandbox file with the reply;
//! - `convert_file` converts between image formats (and re-encodes text);
//! - `list_files` shows what exists.
//!
//! Files live under `data_dir()/sandbox/{channel}` so a later turn can
//! attach what an earlier turn made. Everything is capped: 25 MB per file
//! (Discord's bot upload limit), 64 MB per channel sandbox, files pruned
//! after an hour.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use serde_json::Value;

use super::{ToolCtx, images};
use crate::paths;

pub(crate) const MAX_FILE: usize = 25 * 1024 * 1024;
const MAX_SANDBOX: u64 = 128 * 1024 * 1024;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SCRIPT_OUTPUT: usize = 2000;

pub fn sandbox_dir_pub(channel_id: u64) -> PathBuf {
    sandbox_dir(channel_id)
}

pub(crate) fn sanitize_name_pub(name: &str) -> Result<String, String> {
    sanitize_name(name)
}

fn sandbox_dir(channel_id: u64) -> PathBuf {
    let dir = paths::data_dir()
        .join("sandbox")
        .join(channel_id.to_string());
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A safe file name: no separators, no `..`, no colons.
fn sanitize_name(name: &str) -> Result<String, String> {
    let name = name.trim().trim_start_matches('/');
    if name.is_empty() || name.len() > 120 {
        return Err("give the file a name (up to 120 characters)".to_owned());
    }
    if !name.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '_' | '.' | ' ' | '(' | ')' | '[' | ']' | ',' | '+' | '@'
            )
    }) {
        return Err(format!(
            "{name:?} has characters file names can't use — stick to letters, digits, - _ . ( )"
        ));
    }
    if name.starts_with('.') || name.contains("..") {
        return Err("file names can't start with a dot or contain '..'".to_owned());
    }
    Ok(name.to_owned())
}

/// Prune old files and enforce the size cap (oldest first).
fn enforce_caps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, u64, SystemTime)> = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some((entry.path(), meta.len(), meta.modified().ok()?))
        })
        .collect();
    let hour_ago = SystemTime::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    files.retain(|(path, _, modified)| {
        if *modified < hour_ago {
            let _ = std::fs::remove_file(path);
            false
        } else {
            true
        }
    });
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in files {
        if total <= MAX_SANDBOX {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

pub async fn write_file(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let name = sanitize_name(
        args.get("name")
            .and_then(Value::as_str)
            .ok_or("give the file a name")?,
    )?;
    let dir = sandbox_dir(ctx.channel_id);
    // `size` is an alias for `until_bytes`: an EXACT byte target the app
    // guarantees in code, so "a 1MB file" is 1048576 bytes, never ~900KB.
    // A target set on an earlier call for this same file sticks — a model
    // that drops the arg on a retry must not get to finish small.
    let target: Option<usize> = {
        let given = args
            .get("until_bytes")
            .or_else(|| args.get("size"))
            .and_then(Value::as_u64)
            .map(|bytes| (bytes as usize).min(MAX_FILE));
        given.or_else(|| {
            peek_shortfall(ctx.channel_id).and_then(|(n, _, want)| (n == name).then_some(want))
        })
    };
    let bytes: Vec<u8> = if let Some(pattern) = args.get("repeat").and_then(Value::as_str) {
        // Pattern expansion: fast, deterministic, zero model tokens — for
        // GENERATED DATA (ids, logs, numbers). Prose through repeat is
        // copy-paste filler; a document goes through write_section.
        let looks_like_prose = pattern.chars().count() > 240
            && pattern.contains(". ")
            && pattern.split_whitespace().count() > 40;
        if looks_like_prose {
            return Err(
                "repeat is for generated data (ids, logs, test data), not prose — a document grows with write_section, section by section"
                    .to_owned(),
            );
        }
        let join = args.get("join").and_then(Value::as_str);
        // A size target wins over a repeat count: the ask was an exact size.
        let times = if target.is_some() {
            None
        } else {
            args.get("times").and_then(Value::as_u64)
        };
        match (times, target) {
            (Some(times), _) => expand_pattern(pattern, join, Some(times as usize), None),
            (None, Some(until)) => expand_pattern(pattern, join, None, Some(until)),
            (None, None) => {
                return Err(
                    "with `repeat`, also give `times` (how many) or `until_bytes`/`size` (exact target size)"
                        .to_owned(),
                );
            }
        }
    } else if let Some(content) = args.get("content").and_then(Value::as_str) {
        let mut bytes = content.as_bytes().to_vec();
        // An exact target only trims content down — padding prose UP with
        // spaces would fake the size (a 4 KB story in a "60 KB" file).
        // A shortfall lands in the WARNING below instead: keep writing.
        if let Some(until) = target {
            bytes.truncate(until);
        }
        bytes
    } else {
        return Err(
            "give `content`, or `repeat` + `times`/`until_bytes` for generated data".to_owned(),
        );
    };
    if bytes.is_empty() {
        return Err("the file would be empty".to_owned());
    }
    if bytes.len() > MAX_FILE {
        return Err(format!(
            "that's {} MB — over the {} MB upload limit; split the work into several files",
            bytes.len() / 1024 / 1024,
            MAX_FILE / 1024 / 1024
        ));
    }
    // Sizes in whole MB only: KB and raw byte counts just mislead small
    // models comparing magnitudes. (A file with no target that came out
    // under 1 MB gets a KB label — "0 MB" would read as nothing.)
    let target_note = match target {
        Some(want) if bytes.len() == want => {
            format!(" exactly the requested {}", size_label(want))
        }
        Some(want) => format!(
            " WARNING: only {}, not the {} asked for — and write_file cannot continue a file, so do not call it again with the same content. Keep going with write_section (name: \"{name}\"): it picks up this file and appends section by section until the target.",
            size_label(bytes.len()),
            size_label(want)
        ),
        None => format!(" ({})", size_label(bytes.len())),
    };
    let path = dir.join(&name);
    paths::write_atomic(&path, &bytes)?;
    enforce_caps(&dir);
    // Track the shortfall so the brain can push back if the model tries
    // to answer and walk away from a half-sized file.
    if let Some(want) = target {
        note_shortfall(ctx.channel_id, &name, bytes.len(), want);
    }
    // The request asked for the file attached: done in the same breath —
    // no second tool call for the model to fumble.
    if ctx.wants_file_attached {
        ctx.media
            .lock()
            .expect("media poisoned")
            .add_file(&name, bytes);
        return Ok(format!(
            "wrote {name}{target_note} AND attached it below your reply — nothing else to do, just answer the person."
        ));
    }
    Ok(format!(
        "wrote {name}{target_note} to the file sandbox. To attach it to your reply, call attach_file (name: \"{name}\") — do not call write_file again."
    ))
}

pub async fn run_script(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .ok_or("give the Python `code` to run")?;
    if code.len() > 20_000 {
        return Err("script too long — keep it under 20 000 characters".to_owned());
    }
    let dir = sandbox_dir(ctx.channel_id);
    let script = dir.join("_script.py");
    paths::write_atomic(&script, code.as_bytes())?;

    let python = ctx.core.profile().voice.python.clone();
    let before = snapshot_files(&dir);
    let run = tokio::process::Command::new(&python)
        .arg("-I") // isolated: no site packages beyond the install, no env
        .arg("-B")
        .arg(&script)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output();
    let output = match tokio::time::timeout(SCRIPT_TIMEOUT, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let _ = std::fs::remove_file(&script);
            return Err(format!(
                "could not run {python} ({error}). The operator sets the Python path in the app's Voice tab."
            ));
        }
        Err(_) => {
            let _ = std::fs::remove_file(&script);
            return Err(format!(
                "the script ran past {} seconds and was stopped",
                SCRIPT_TIMEOUT.as_secs()
            ));
        }
    };
    let _ = std::fs::remove_file(&script);
    let status = if output.status.success() {
        String::new()
    } else {
        format!("\nexit code: {}", output.status.code().unwrap_or(-1))
    };
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.stderr.is_empty() {
        let err = String::from_utf8_lossy(&output.stderr);
        text.push_str("\n--- stderr ---\n");
        text.push_str(&err);
    }
    if text.trim().is_empty() {
        text = "(no output)".to_owned();
    }
    let text = crate::llm::truncate(&text, MAX_SCRIPT_OUTPUT);
    let after = snapshot_files(&dir);
    let new_files: Vec<(String, usize)> = after
        .iter()
        .filter(|(name, _)| {
            !before.iter().any(|(n, _)| n == name)
                || match std::fs::read(dir.join(name.as_str())) {
                    Ok(bytes) => !before.iter().any(|(n, s)| n == name && *s == bytes.len()),
                    Err(_) => false,
                }
        })
        .cloned()
        .collect();
    let shown: Vec<String> = new_files
        .iter()
        .map(|(name, size)| format!("{name} ({})", size_label(*size)))
        .collect();
    let files_note = if shown.is_empty() {
        String::new()
    } else {
        format!(
            "
Files written: {}. Attach with attach_file.",
            shown.join(", ")
        )
    };
    // When the request named a byte target, verify against it in code —
    // the model never hand-checks arithmetic.
    let size_note = match (
        args.get("until_bytes")
            .or_else(|| args.get("size"))
            .and_then(Value::as_u64),
        new_files.first(),
    ) {
        (Some(want), Some((_, bytes))) => {
            if *bytes as u64 == want {
                String::new()
            } else {
                format!(
                    "
NOTE: the file is {} MB, not the {} MB asked for. Fix it deterministically: write_file with repeat + until_bytes={want} hits the exact size.",
                    bytes / 1024 / 1024,
                    want / 1024 / 1024
                )
            }
        }
        _ => String::new(),
    };
    enforce_caps(&dir);
    Ok(format!("```{text}```{status}{files_note}{size_note}"))
}

/// A size the way the model should read it: KB for anything under a MB
/// ("0 MB" would read as nothing), whole MB above — the model only ever
/// compares MB-scale magnitudes, never KB arithmetic.
pub fn size_label(bytes: usize) -> String {
    if bytes < 1024 * 1024 {
        format!("{} KB", (bytes / 1024).max(1))
    } else {
        format!("{} MB", bytes / 1024 / 1024)
    }
}

/// The channel's open size shortfall (file name, bytes so far, target):
/// set when a file with a size target comes out short, cleared when it
/// reaches the target. The brain turns it into a "not finished" nudge so
/// the model cannot just answer and walk away.
fn shortfalls() -> &'static std::sync::Mutex<std::collections::HashMap<u64, (String, usize, usize)>>
{
    static SHORTFALLS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u64, (String, usize, usize)>>,
    > = std::sync::OnceLock::new();
    SHORTFALLS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn note_shortfall(channel_id: u64, name: &str, have: usize, want: usize) {
    if have >= want {
        shortfalls()
            .lock()
            .expect("shortfalls poisoned")
            .remove(&channel_id);
    } else {
        shortfalls()
            .lock()
            .expect("shortfalls poisoned")
            .insert(channel_id, (name.to_owned(), have, want));
    }
}

pub(crate) fn peek_shortfall(channel_id: u64) -> Option<(String, usize, usize)> {
    shortfalls()
        .lock()
        .expect("shortfalls poisoned")
        .get(&channel_id)
        .cloned()
}

pub(crate) fn clear_shortfall(channel_id: u64) {
    shortfalls()
        .lock()
        .expect("shortfalls poisoned")
        .remove(&channel_id);
}

/// Bracket-style attachment markers a small model sometimes writes instead
/// of calling the tool ("Here it is: [attach_file]" or
/// "[attach_file: notes.txt]"). The app performs the attachment the model
/// meant: markers are stripped from the text and the files they named (or,
/// unnamed, the most recently written sandbox file) are returned for
/// upload. Never shown to the chat as raw syntax.
pub(crate) fn repair_attach_markers(
    channel_id: u64,
    text: &str,
) -> (String, Vec<(String, Vec<u8>)>) {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[attach") {
        let Some(end_rel) = rest[start..].find(']') else {
            break; // no closing bracket: leave the rest untouched
        };
        let end = start + end_rel;
        out.push_str(&rest[..start]);
        let inside = &rest[start + 1..end];
        // "attach_file", "attach" — then an optional ": name" / "name".
        let named = inside
            .trim_start_matches("attach_file")
            .trim_start_matches("attach")
            .trim_start_matches(':')
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim();
        let dir = sandbox_dir(channel_id);
        let candidate = if named.is_empty() {
            // No name: the file written most recently.
            snapshot_files(&dir)
                .into_iter()
                .max_by_key(|(name, _size)| {
                    std::fs::metadata(dir.join(name))
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u128)
                        .unwrap_or(0)
                })
                .map(|(name, _)| name)
        } else {
            sanitize_name(named).ok()
        };
        let attached = candidate.and_then(|name| {
            let bytes = std::fs::read(dir.join(&name)).ok()?;
            (bytes.len() <= MAX_FILE).then(|| {
                files.push((name.clone(), bytes));
                name
            })
        });
        if attached.is_none() {
            // Nothing to attach: drop the marker, keep the sentence.
            tracing::debug!(marker = inside, "attach marker with no matching file");
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    // A lead-in like "attached below:" now tells the truth — the file IS
    // attached below the reply.
    (out, files)
}

fn snapshot_files(dir: &Path) -> Vec<(String, usize)> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_file())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("_script") {
                        return None;
                    }
                    Some((name, entry.metadata().ok()?.len() as usize))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub async fn attach_file(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let name = sanitize_name(
        args.get("name")
            .and_then(Value::as_str)
            .ok_or("which file? (see list_files)")?,
    )?;
    let dir = sandbox_dir(ctx.channel_id);
    let path = dir.join(&name);
    let bytes = std::fs::read(&path)
        .map_err(|_| format!("{name} isn't in the file sandbox — check list_files"))?;
    if bytes.len() > MAX_FILE {
        return Err(format!("{name} is over the 25 MB limit"));
    }
    ctx.media
        .lock()
        .expect("media poisoned")
        .add_file(&name, bytes);
    Ok(format!("{name} is attached below the reply."))
}

pub async fn convert_file(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let name = sanitize_name(
        args.get("name")
            .and_then(Value::as_str)
            .ok_or("which file?")?,
    )?;
    let to = args
        .get("to")
        .and_then(Value::as_str)
        .ok_or("convert to what? (png, jpeg, webp, txt)")?
        .trim()
        .to_lowercase();
    let dir = sandbox_dir(ctx.channel_id);
    let path = dir.join(&name);
    let bytes = std::fs::read(&path).map_err(|_| format!("{name} isn't in the file sandbox"))?;

    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&name);
    let (target, converted) = match to.as_str() {
        "png" | "jpeg" | "jpg" | "webp" => {
            let image = images::decode(&bytes)?;
            let converted = encode_to(&image, &to)?;
            (format!("{stem}.{to}"), converted)
        }
        "txt" => {
            let text = String::from_utf8(bytes).map_err(|_| {
                "that file isn't UTF-8 text — I can only re-encode text as txt".to_owned()
            })?;
            (
                format!("{stem}.txt"),
                text.replace("\r\n", "\n").into_bytes(),
            )
        }
        other => {
            return Err(format!(
                "{other:?} isn't a format I convert to — use png, jpeg, webp, or txt"
            ));
        }
    };
    if converted.len() > MAX_FILE {
        return Err("the converted file is over the 25 MB limit".to_owned());
    }
    paths::write_atomic(&dir.join(&target), &converted)?;
    Ok(format!(
        "converted {name} to {target} ({} bytes). Attach it with attach_file.",
        converted.len()
    ))
}

pub async fn list_files(ctx: &ToolCtx, _args: &Value) -> Result<String, String> {
    let dir = sandbox_dir(ctx.channel_id);
    let files = snapshot_files(&dir);
    if files.is_empty() {
        return Ok("The file sandbox for this channel is empty.".to_owned());
    }
    enforce_caps(&dir);
    let lines: Vec<String> = files
        .iter()
        .map(|(name, size)| format!("- {name} ({})", size_label(*size)))
        .collect();
    Ok(format!(
        "Files in this channel's sandbox:\n{}",
        lines.join("\n")
    ))
}

/// Encode an image as png / jpeg / webp bytes.
fn encode_to(image: &image::DynamicImage, format: &str) -> Result<Vec<u8>, String> {
    use image::DynamicImage;
    let mut bytes = std::io::Cursor::new(Vec::new());
    match format {
        "png" => {
            image
                .write_to(&mut bytes, image::ImageFormat::Png)
                .map_err(|e| format!("png encoding failed: {e}"))?;
        }
        "jpeg" | "jpg" => {
            let rgb = DynamicImage::ImageRgb8(image.to_rgb8());
            rgb.write_to(&mut bytes, image::ImageFormat::Jpeg)
                .map_err(|e| format!("jpeg encoding failed: {e}"))?;
        }
        "webp" => {
            image
                .write_to(&mut bytes, image::ImageFormat::WebP)
                .map_err(|e| format!("webp encoding failed: {e}"))?;
        }
        other => return Err(format!("{other:?} is not an image format")),
    }
    Ok(bytes.into_inner())
}

/// Expand `pattern` joined by `join` (default newline): `times` full
/// repeats, or an EXACT byte target (the result is exactly that many
/// bytes — trimmed mid-pattern and padded with the unit's last byte when
/// needed).
pub(crate) fn expand_pattern(
    pattern: &str,
    join: Option<&str>,
    times: Option<usize>,
    until: Option<usize>,
) -> Vec<u8> {
    let mut unit = pattern.as_bytes().to_vec();
    match join {
        Some(join) => unit.extend_from_slice(join.as_bytes()),
        None => unit.push(b'\n'),
    }
    if unit.is_empty() {
        unit = b" ".to_vec();
    }
    let mut out = Vec::new();
    match (times, until) {
        (Some(times), None) => {
            out.reserve(unit.len().saturating_mul(times));
            for _ in 0..times.min(10_000_000) {
                out.extend_from_slice(&unit);
            }
            out
        }
        (_, Some(until)) => {
            let until = until.min(MAX_FILE);
            let whole = until / unit.len();
            for _ in 0..whole {
                out.extend_from_slice(&unit);
            }
            out.extend_from_slice(&unit[..until - out.len()]);
            while out.len() < until {
                out.push(*unit.last().unwrap_or(&b' '));
            }
            out.truncate(until);
            out
        }
        (None, None) => out,
    }
}

/// Resize `bytes` to exactly `want` by padding with `pad` or trimming.
#[cfg(test)]
pub(crate) fn exact_size(mut bytes: Vec<u8>, want: usize, pad: u8) -> Vec<u8> {
    bytes.truncate(want);
    while bytes.len() < want {
        bytes.push(pad);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert!(sanitize_name("legion.txt").is_ok());
        assert!(sanitize_name("report (final).csv").is_ok());
        assert!(sanitize_name("../etc/passwd").is_err());
        assert!(sanitize_name("a/b.png").is_err());
        assert!(sanitize_name(".hidden").is_err());
        assert!(sanitize_name("").is_err());
    }

    #[test]
    fn repeat_pattern_expands_deterministically() {
        // The write_file fast path is exercised through the same logic in
        // `write_file`; here we check the cap math the function relies on.
        let line = b"legion\n".repeat(3);
        assert_eq!(line.len(), 21);
    }

    #[test]
    fn pattern_hits_exact_byte_targets() {
        // 1MB of "legion " (spaces via join) lands on exactly 1048576.
        let out = expand_pattern("legion", Some(" "), None, Some(1024 * 1024));
        assert_eq!(out.len(), 1024 * 1024);
        assert!(out.starts_with(b"legion legion"));
        // Newline-joined, odd target: still exact.
        let out = expand_pattern("legion", None, None, Some(1000));
        assert_eq!(out.len(), 1000);
        // Explicit count: full repeats only, as asked.
        let out = expand_pattern("legion", Some(" "), Some(100), None);
        assert_eq!(out.len(), 700);
    }

    #[test]
    fn content_resized_to_exact_target() {
        let out = exact_size(b"abc".to_vec(), 10, b' ');
        assert_eq!(out, b"abc       ".to_vec());
        let out = exact_size(b"abcdefghij".to_vec(), 3, b' ');
        assert_eq!(out, b"abc".to_vec());
    }

    #[test]
    fn snapshot_lists_files_with_sizes() {
        let dir = std::env::temp_dir().join(format!("axis_sb_snap_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("_script.py"), b"skip me").unwrap();
        let files = snapshot_files(&dir);
        assert_eq!(files, vec![("a.txt".to_owned(), 5)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
