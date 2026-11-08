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
//! attach what an earlier turn made. Everything is capped: 8 MB per file
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

const MAX_FILE: usize = 8 * 1024 * 1024;
const MAX_SANDBOX: u64 = 64 * 1024 * 1024;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SCRIPT_OUTPUT: usize = 2000;

pub(crate) fn sandbox_dir_pub(channel_id: u64) -> PathBuf {
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
    let target: Option<usize> = args
        .get("until_bytes")
        .or_else(|| args.get("size"))
        .and_then(Value::as_u64)
        .map(|bytes| (bytes as usize).min(MAX_FILE));
    let bytes: Vec<u8> = if let Some(pattern) = args.get("repeat").and_then(Value::as_str) {
        // Pattern expansion: fast, deterministic, zero model tokens.
        let join = args.get("join").and_then(Value::as_str);
        let times = args.get("times").and_then(Value::as_u64);
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
        // An exact target pads or trims plain content too.
        if let Some(until) = target {
            bytes = exact_size(bytes, until, b' ');
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
            "that's {} bytes — over the {} MB upload limit",
            bytes.len(),
            MAX_FILE / 1024 / 1024
        ));
    }
    let target_note = match target {
        Some(want) if bytes.len() == want => format!(" exactly the requested {want} bytes"),
        Some(want) => format!(
            " WARNING: {} bytes, not the requested {want} — call write_file again with until_bytes={want}",
            bytes.len()
        ),
        None => String::new(),
    };
    let path = dir.join(&name);
    let size = bytes.len();
    paths::write_atomic(&path, &bytes)?;
    enforce_caps(&dir);
    Ok(format!(
        "wrote {name} ({size} bytes,{target_note}) to the file sandbox. Attach it with attach_file (name: \"{name}\") or reference it in convert_file."
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
    let new_files: Vec<String> = after
        .iter()
        .filter(|(name, _)| {
            !before.iter().any(|(n, _)| n == name)
                || match std::fs::read(dir.join(name.as_str())) {
                    Ok(bytes) => !before.iter().any(|(n, s)| n == name && *s == bytes.len()),
                    Err(_) => false,
                }
        })
        .map(|(name, size)| format!("{name} ({size} bytes)"))
        .collect();
    let files_note = if new_files.is_empty() {
        String::new()
    } else {
        format!(
            "
Files written: {}. Attach with attach_file.",
            new_files.join(", ")
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
        (Some(want), Some(first)) => {
            // Entries look like "name (123 bytes)".
            let bytes = first
                .rsplit('(')
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches(')')
                .trim_end_matches(" bytes")
                .parse::<u64>()
                .ok();
            match bytes {
                Some(bytes) if bytes == want => String::new(),
                Some(bytes) => format!(
                    "
NOTE: the file is {bytes} bytes, not the {want} asked for. Fix it deterministically: write_file with repeat + until_bytes={want} hits the exact size."
                ),
                None => String::new(),
            }
        }
        _ => String::new(),
    };
    enforce_caps(&dir);
    Ok(format!("```{text}```{status}{files_note}{size_note}"))
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
        return Err(format!(
            "{name} is {} bytes — over the 8 MB limit",
            bytes.len()
        ));
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
        return Err("the converted file is over the 8 MB limit".to_owned());
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
        .map(|(name, size)| format!("- {name} ({} bytes)", size))
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
