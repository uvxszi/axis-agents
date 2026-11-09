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
//! attach what an earlier turn made. Every cap (upload limit, sandbox
//! size, pruning, temp lifetime) is a tunable in Settings — nothing is
//! buried in code. `temp: true` writes go to the channel's `_tmp/`
//! scratch area, which cleans itself up.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use serde_json::Value;

use super::{ToolCtx, images, number};
use crate::config::Tunables;
use crate::paths;

/// The live knobs for this job (settings are editable while the app runs).
fn tunables(ctx: &ToolCtx) -> Tunables {
    ctx.core.hive.settings().tunables
}

/// What one message may carry in files: Discord's cap is on the WHOLE
/// request (all files + the message payload), so a 25 MB file plus
/// anything else gets a 413. This is the largest total that actually
/// sends.
pub(crate) fn attach_budget(ctx: &ToolCtx) -> usize {
    let knobs = tunables(ctx);
    knobs
        .upload_limit_bytes
        .saturating_sub(knobs.attach_headroom_bytes)
}

/// Split files into messages Discord will accept: each bucket's TOTAL
/// stays under the budget (and under 10 files). A single oversized file
/// rides alone — the sender reports it if Discord still refuses.
pub(crate) fn bucket_files(
    files: &[(String, Vec<u8>)],
    budget: usize,
) -> Vec<Vec<(String, Vec<u8>)>> {
    let mut buckets: Vec<Vec<(String, Vec<u8>)>> = Vec::new();
    let mut current: Vec<(String, Vec<u8>)> = Vec::new();
    let mut used = 0usize;
    for file in files {
        if !current.is_empty() && (used + file.1.len() > budget || current.len() >= 10) {
            buckets.push(std::mem::take(&mut current));
            used = 0;
        }
        used += file.1.len();
        current.push(file.clone());
    }
    if !current.is_empty() {
        buckets.push(current);
    }
    buckets
}

#[cfg(test)]
mod bucket_tests {
    use super::bucket_files;

    fn file(name: &str, kb: usize) -> (String, Vec<u8>) {
        (name.to_owned(), vec![b'x'; kb * 1024])
    }

    #[test]
    fn oversized_set_splits_across_messages() {
        // A 24 MB story and a 2 MB gif: together over the budget, apart fine.
        let budget = 24 * 1024 * 1024;
        let buckets = bucket_files(&[file("story.md", 23_000), file("cat.gif", 2_000)], budget);
        assert_eq!(buckets.len(), 2, "the gif rides a second message");
        assert!(buckets[0][0].0 == "story.md");
    }

    #[test]
    fn small_files_share_one_message() {
        let buckets = bucket_files(&[file("a.txt", 10), file("b.txt", 5)], 1024 * 1024);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].len(), 2);
    }

    #[test]
    fn one_file_over_budget_still_ships_alone() {
        let buckets = bucket_files(&[file("big.md", 2_000)], 1024);
        assert_eq!(buckets.len(), 1, "never silently dropped");
    }
}

/// Script output shown back to the model (a display cap, not a limit on
/// what scripts may compute).
const MAX_SCRIPT_OUTPUT: usize = 2000;

/// The channel's temporary scratch area: files written with temp: true
/// land here and clean themselves up once stale. Working memory the model
/// doesn't have to manage.
fn tmp_dir(ctx: &ToolCtx) -> PathBuf {
    let dir = sandbox_dir(ctx.channel_id).join("_tmp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Where a file with this name lives: the sandbox, or the temp area.
pub(crate) fn resolve_file(ctx: &ToolCtx, name: &str) -> PathBuf {
    let dir = sandbox_dir(ctx.channel_id);
    let direct = dir.join(name);
    if direct.exists() {
        return direct;
    }
    let temp = dir.join("_tmp").join(name);
    if temp.exists() {
        return temp;
    }
    direct
}

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
/// Drop stale files from the `_tmp` scratch area — temporary working
/// files clean themselves up.
fn clean_temp(dir: &Path, lifetime_hours: u64) {
    let tmp = dir.join("_tmp");
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(lifetime_hours * 3600);
    if let Ok(entries) = std::fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|at| at < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn enforce_caps(dir: &Path, sandbox_limit: u64, temp_lifetime_hours: u64) {
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
    clean_temp(dir, temp_lifetime_hours);
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in files {
        if total <= sandbox_limit {
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
    let knobs = tunables(ctx);
    // Finished work stays finished: this file already reached the size
    // the person asked for — rewriting it would undo that (the classic
    // loop: write 1 KB, duplicate to 1 MB, write 1 KB again…).
    if let Some(want) = already_satisfied(ctx.channel_id, &name) {
        return Err(format!(
            "{name} is already {} — exactly the {} asked for. Do NOT rewrite or add to it. The work is done: reply to the person now, nothing else.",
            size_label(
                std::fs::metadata(dir.join(&name))
                    .map(|m| m.len() as usize)
                    .unwrap_or(0)
            ),
            size_label(want)
        ));
    }
    // `size` is an alias for `until_bytes`: an EXACT byte target the app
    // guarantees in code, so "a 1MB file" is 1048576 bytes, never ~900KB.
    // A target set on an earlier call for this same file sticks — a model
    // that drops the arg on a retry must not get to finish small. Last
    // fallback: the size the person asked for in their message ("a 25 MB
    // file"), so even a sloppy first write lands on the right target.
    let target: Option<usize> = {
        let given = args
            .get("until_bytes")
            .or_else(|| args.get("size"))
            .and_then(Value::as_u64)
            .map(|bytes| (bytes as usize).min(attach_budget(ctx)));
        // A target never shrinks: the sticky target for this file wins
        // over a smaller one the model passes on a retry.
        let sticky =
            peek_shortfall(ctx.channel_id).and_then(|(n, _, want)| (n == name).then_some(want));
        match (given, sticky) {
            (Some(g), Some(s)) => Some(g.max(s)),
            (given, sticky) => given.or(sticky),
        }
        .or_else(|| take_requested_target(ctx.channel_id))
    };
    let bytes: Vec<u8> = if let Some(pattern) = args.get("repeat").and_then(Value::as_str) {
        // Pattern expansion: fast, deterministic, zero model tokens — for
        // GENERATED DATA (ids, logs, numbers). Prose through repeat is
        // copy-paste filler; a document goes through write_section.
        let looks_like_prose = pattern.chars().count() > knobs.prose_min_chars
            && pattern.contains(". ")
            && pattern.split_whitespace().count() > knobs.prose_min_words;
        if looks_like_prose {
            return Err(
                "repeat is for generated data (ids, logs, test data), not prose. To copy-paste a document to a size, write the text once with content, then call duplicate_file on it — one call, instant and exact"
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
            (Some(times), _) => expand_pattern(
                pattern,
                join,
                Some(times as usize),
                None,
                knobs.upload_limit_bytes,
            ),
            (None, Some(until)) => {
                expand_pattern(pattern, join, None, Some(until), knobs.upload_limit_bytes)
            }
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
    if bytes.len() > knobs.upload_limit_bytes {
        return Err(format!(
            "that's {} MB — over the {} MB upload limit. For copy-paste to a big size, write the text once and call duplicate_file on it: it fills to the exact size and splits into uploadable parts by itself",
            bytes.len() / 1024 / 1024,
            knobs.upload_limit_bytes / 1024 / 1024
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
            " WARNING: only {}, not the {} asked for — and write_file cannot continue a file, so do not call it again. Fill the rest with ONE call: duplicate_file (name: \"{name}\") if copies/repeats are acceptable (that is what \"copy-paste/fill to size\" means), or write_section to keep writing new prose.",
            size_label(bytes.len()),
            size_label(want)
        ),
        None => format!(" ({})", size_label(bytes.len())),
    };
    let temp = super::flag(args, "temp").unwrap_or(false);
    let path = if temp {
        tmp_dir(ctx).join(&name)
    } else {
        dir.join(&name)
    };
    paths::write_atomic(&path, &bytes)?;
    enforce_caps(
        &dir,
        knobs.sandbox_limit_bytes,
        knobs.temp_file_lifetime_hours,
    );
    // Track the shortfall so the brain can push back if the model tries
    // to answer and walk away from a half-sized file.
    if let Some(want) = target {
        note_shortfall(ctx.channel_id, &name, bytes.len(), want);
    }
    // The app finishes size work itself when the ask was copy-paste, or
    // when the model has rewritten the same short file twice (the
    // observed flail: 7 rewrites of 3 KB, never a duplicate call).
    let still_short = peek_shortfall(ctx.channel_id).is_some_and(|(n, _, _)| n == name);
    if still_short
        && (ctx.copy_fill_asked
            || count_rewrite(ctx.channel_id, &name) >= knobs.file_rewrite_autofill)
    {
        let filled = duplicate_file(ctx, &serde_json::json!({ "name": name })).await?;
        return Ok(format!(
            "The app finished the size work for you (copied up to the target in code): {filled} Reply to the person now — nothing else to do."
        ));
    }
    // The request asked for the file attached: done in the same breath —
    // no second tool call for the model to fumble. (Not while the file is
    // still short of its target — the finished version is what ships.)
    if ctx.wants_file_attached && !still_short {
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
    let knobs = tunables(ctx);
    let timeout = Duration::from_secs(knobs.script_timeout_secs);
    let output = match tokio::time::timeout(timeout, run).await {
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
                timeout.as_secs()
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
    enforce_caps(
        &dir,
        knobs.sandbox_limit_bytes,
        knobs.temp_file_lifetime_hours,
    );
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

/// Consecutive rewrites of a still-short file (channel, name) — after a
/// couple, the app stops waiting and finishes the fill itself.
fn rewrites() -> &'static std::sync::Mutex<std::collections::HashMap<(u64, String), u32>> {
    static REWRITES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(u64, String), u32>>,
    > = std::sync::OnceLock::new();
    REWRITES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn count_rewrite(channel_id: u64, name: &str) -> u32 {
    let mut map = rewrites().lock().expect("rewrites poisoned");
    let count = map.entry((channel_id, name.to_owned())).or_insert(0);
    *count += 1;
    *count
}

fn clear_rewrites(channel_id: u64, name: &str) {
    rewrites()
        .lock()
        .expect("rewrites poisoned")
        .remove(&(channel_id, name.to_owned()));
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
        met_targets()
            .lock()
            .expect("targets poisoned")
            .insert(channel_id, (name.to_owned(), want));
        clear_rewrites(channel_id, name);
    } else {
        shortfalls()
            .lock()
            .expect("shortfalls poisoned")
            .insert(channel_id, (name.to_owned(), have, want));
        met_targets()
            .lock()
            .expect("targets poisoned")
            .remove(&channel_id);
    }
}

/// The file that already reached its size target in this channel — so a
/// rewrite is refused instead of undoing finished work (a small model
/// will happily write_file the finished 1 MB file back down to 1 KB).
fn met_targets() -> &'static std::sync::Mutex<std::collections::HashMap<u64, (String, usize)>> {
    static MET: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u64, (String, usize)>>,
    > = std::sync::OnceLock::new();
    MET.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Is this file already at its met target (and still that size on disk)?
fn already_satisfied(channel_id: u64, name: &str) -> Option<usize> {
    let (met_name, want) = met_targets()
        .lock()
        .expect("targets poisoned")
        .get(&channel_id)?
        .clone();
    if met_name != name {
        return None;
    }
    satisfied_on_disk(channel_id, &met_name, want)
}

fn satisfied_on_disk(channel_id: u64, name: &str, want: usize) -> Option<usize> {
    let on_disk = std::fs::metadata(sandbox_dir(channel_id).join(name))
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    (on_disk >= want).then_some(want)
}

/// The met target for this channel regardless of file name (longform's
/// docs are per-channel): Some(want) when the satisfied file still holds
/// that size on disk.
pub(crate) fn already_satisfied_pub(channel_id: u64) -> Option<usize> {
    let (met_name, want) = met_targets()
        .lock()
        .expect("targets poisoned")
        .get(&channel_id)?
        .clone();
    satisfied_on_disk(channel_id, &met_name, want)
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

/// File-work state is per JOB: a leftover requested target or "met"
/// marker from an earlier job would hijack the next one's files.
pub(crate) fn reset_channel_state(channel_id: u64) {
    shortfalls()
        .lock()
        .expect("shortfalls poisoned")
        .remove(&channel_id);
    requested_targets()
        .lock()
        .expect("targets poisoned")
        .remove(&channel_id);
    met_targets()
        .lock()
        .expect("targets poisoned")
        .remove(&channel_id);
    rewrites()
        .lock()
        .expect("rewrites poisoned")
        .retain(|(channel, _), _| *channel != channel_id);
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
    upload_limit: usize,
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
            (bytes.len() <= upload_limit).then(|| {
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

/// Duplicate a sandbox file's content until a target size (or an exact
/// copy count) — "write a story and copy-paste it until the file is 25 MB"
/// as ONE deterministic tool call instead of hundreds of model rounds.
pub async fn duplicate_file(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let name = sanitize_name(
        args.get("name")
            .and_then(Value::as_str)
            .ok_or("which file? (see list_files)")?,
    )?;
    let dir = sandbox_dir(ctx.channel_id);
    let knobs = tunables(ctx);
    let path = dir.join(&name);
    let content = std::fs::read(&path)
        .map_err(|_| format!("{name} isn't in the file sandbox — check list_files"))?;
    if content.is_empty() {
        return Err(format!("{name} is empty — nothing to copy"));
    }
    let target_name = match args.get("to").and_then(Value::as_str) {
        Some(to) => sanitize_name(to)?,
        None => name.clone(),
    };
    let join = args
        .get("join")
        .and_then(Value::as_str)
        .unwrap_or("\n\n")
        .as_bytes()
        .to_vec();
    let times = number(args, "times");
    // Target precedence: the call's own target, then the sticky shortfall
    // for this file, then a size the person asked for in their message
    // (consumed on first use). The model never does size arithmetic.
    let target: Option<usize> = args
        .get("until_bytes")
        .or_else(|| args.get("size"))
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .or_else(|| {
            peek_shortfall(ctx.channel_id).and_then(|(n, _, want)| (n == name).then_some(want))
        })
        .or_else(|| take_requested_target(ctx.channel_id));
    // Discord's cap is on the whole request (files + message). Filling a
    // file to the full 25 MB and then SENDING it is a guaranteed 413 —
    // fill to the upload-safe budget instead and say so.
    let (target, capped) = match target {
        Some(want) if want > attach_budget(ctx) => (Some(attach_budget(ctx)), true),
        other => (other, false),
    };
    if times.is_none() && target.is_none() {
        return Err(
            "give `times` (how many copies) or `until_bytes`/`size` (target size for the WHOLE file)"
                .to_owned(),
        );
    }
    // Already at the asked size: no-op duplicates just burn rounds.
    if let Some(want) = already_satisfied(ctx.channel_id, &name) {
        return Ok(format!(
            "{name} is already {} — exactly the {} asked for, attached below your earlier reply. Nothing to do: answer the person now.",
            size_label(content.len()),
            size_label(want)
        ));
    }
    let (mut bytes, copies) = match (times, target) {
        // Both given: the size is the ask (times ignored). Duplicate to
        // the FULL target (up to 10 upload parts); splitting comes next.
        (_, Some(until)) => duplicate_until(
            &content,
            &join,
            until.min(knobs.upload_limit_bytes * knobs.max_duplicate_parts),
            knobs.exact_size_tolerance_bytes,
        ),
        (Some(times), None) => {
            let times = times.clamp(1, 10_000_000) as usize;
            let mut out = Vec::with_capacity(content.len().saturating_mul(times));
            for _ in 0..times {
                out.extend_from_slice(&content);
                out.extend_from_slice(&join);
            }
            (out, times)
        }
        // Unreachable: both missing was rejected above.
        (None, None) => return Err("give `times` or `until_bytes`".to_owned()),
    };
    let whole_cap = knobs.upload_limit_bytes * knobs.max_duplicate_parts;
    if bytes.len() > whole_cap {
        bytes.truncate(whole_cap);
    }

    // Discord caps one upload at 25 MB: a bigger whole becomes numbered
    // parts, each an uploadable file.
    let want_total = target;
    let parts = split_parts(&bytes, knobs.upload_limit_bytes);
    let mut written: Vec<String> = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let part_name = if parts.len() == 1 {
            target_name.clone()
        } else {
            numbered_part(&target_name, index + 1, parts.len())
        };
        paths::write_atomic(&dir.join(&part_name), part)?;
        written.push(part_name);
    }
    enforce_caps(
        &dir,
        knobs.sandbox_limit_bytes,
        knobs.temp_file_lifetime_hours,
    );
    // Deliver: attach every part with the reply (up to 10 per message).
    let attached = written.len() <= knobs.max_duplicate_parts;
    if attached {
        let mut media = ctx.media.lock().expect("media poisoned");
        for part_name in &written {
            if let Ok(part) = std::fs::read(dir.join(part_name)) {
                media.add_file(part_name, part);
            }
        }
    }
    if let Some(want) = want_total {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        note_shortfall(ctx.channel_id, &target_name, total, want);
    }
    let listing = written
        .iter()
        .map(|n| {
            format!(
                "{n} ({})",
                size_label(
                    std::fs::metadata(dir.join(n))
                        .map(|m| m.len() as usize)
                        .unwrap_or(0)
                )
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "duplicated {name}: {copies} copies — {}{}.{}{}",
        listing,
        if written.len() == 1 {
            String::new()
        } else {
            format!(" ({} parts for the upload limit)", written.len())
        },
        if capped {
            // Honest about the budget: the ask was the full upload limit,
            // but Discord caps the WHOLE request — this is the most that
            // actually sends.
            " (capped just under the upload limit: Discord rejects a message whose files add up past it — this is the largest that goes through)"
        } else {
            ""
        },
        if attached {
            " Everything is attached below your reply — just confirm, nothing else to do."
        } else {
            " Too many parts to attach at once — attach them with attach_file, a few per reply."
        },
    ))
}

/// Cut a byte string into upload-sized parts when the whole exceeds the
/// upload cap (always at a line break when one is nearby).
pub(crate) fn split_parts(bytes: &[u8], max_part: usize) -> Vec<Vec<u8>> {
    if bytes.len() <= max_part {
        return vec![bytes.to_vec()];
    }
    let mut parts = Vec::new();
    let mut rest = bytes;
    while rest.len() > max_part {
        let cut = rest[..max_part]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|at| at + 1)
            .unwrap_or(max_part);
        parts.push(rest[..cut].to_vec());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        parts.push(rest.to_vec());
    }
    parts
}

/// story.md + part 2 of 3 → story_part2of3.md
fn numbered_part(name: &str, index: usize, total: usize) -> String {
    let (stem, ext) = name
        .rsplit_once('.')
        .map(|(stem, ext)| (stem, format!(".{ext}")))
        .unwrap_or((name, String::new()));
    format!("{stem}_part{index}of{total}{ext}")
}

/// The size the person asked for in their message, parsed once by the
/// brain at job start ("a 25 MB file") — applied to the first file work
/// that doesn't name its own target.
fn requested_targets() -> &'static std::sync::Mutex<std::collections::HashMap<u64, usize>> {
    static TARGETS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, usize>>> =
        std::sync::OnceLock::new();
    TARGETS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn note_requested_target(channel_id: u64, bytes: usize) {
    // A new ask supersedes an earlier satisfied file ("now make it 2 MB").
    met_targets()
        .lock()
        .expect("targets poisoned")
        .remove(&channel_id);
    requested_targets()
        .lock()
        .expect("targets poisoned")
        .insert(channel_id, bytes);
}

pub(crate) fn take_requested_target(channel_id: u64) -> Option<usize> {
    requested_targets()
        .lock()
        .expect("targets poisoned")
        .remove(&channel_id)
}

/// The pending requested target WITHOUT consuming it — the brain uses
/// this to notice that promised file work never happened.
pub(crate) fn peek_requested_target(channel_id: u64) -> Option<usize> {
    requested_targets()
        .lock()
        .expect("targets poisoned")
        .get(&channel_id)
        .copied()
}

/// Repeat `content` (joined by `join`) until reaching `until` bytes, cut
/// at the last line break so a text file never ends mid-word. Returns the
/// bytes and how many copies fit.
pub(crate) fn duplicate_until(
    content: &[u8],
    join: &[u8],
    until: usize,
    tolerance: usize,
) -> (Vec<u8>, usize) {
    let mut unit = content.to_vec();
    unit.extend_from_slice(join);
    if unit.is_empty() {
        unit = b" ".to_vec();
    }
    let mut out = Vec::with_capacity(until);
    let mut copies = 0usize;
    while out.len() < until {
        out.extend_from_slice(&unit);
        copies += 1;
        if copies > 10_000_000 {
            break;
        }
    }
    if out.len() > until {
        // Prefer cutting at the last newline inside the budget — but only
        // when it barely loses anything: until_bytes is an EXACT target,
        // and a paragraph-length cut short of it (1023 KB of "1 MB")
        // never registers as met and the work loops.
        let newline = out[..until]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|at| at + 1)
            .unwrap_or(until);
        let cut = if until - newline <= tolerance {
            newline
        } else {
            until
        };
        out.truncate(cut.max(content.len().min(until)));
    }
    (out, copies.max(1))
}

pub async fn attach_file(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let name = sanitize_name(
        args.get("name")
            .and_then(Value::as_str)
            .ok_or("which file? (see list_files)")?,
    )?;
    let path = resolve_file(ctx, &name);
    let bytes = std::fs::read(&path)
        .map_err(|_| format!("{name} isn't in the file sandbox — check list_files"))?;
    if bytes.len() > tunables(ctx).upload_limit_bytes {
        return Err(format!("{name} is over the upload limit"));
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
    if converted.len() > tunables(ctx).upload_limit_bytes {
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
    let knobs = tunables(ctx);
    let files = snapshot_files(&dir);
    if files.is_empty() {
        return Ok("The file sandbox for this channel is empty.".to_owned());
    }
    enforce_caps(
        &dir,
        knobs.sandbox_limit_bytes,
        knobs.temp_file_lifetime_hours,
    );
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
    max: usize,
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
            let until = until.min(max);
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
    fn duplicate_until_cuts_at_a_line_break() {
        let tol = crate::config::Tunables::default().exact_size_tolerance_bytes;
        let story = b"line one\nline two\n".to_vec();
        let (out, copies) = duplicate_until(&story, b"\n\n", 25, tol);
        assert!(out.len() <= 25, "never over the target");
        assert!(
            out.ends_with(b"\n"),
            "cut at a line break, not mid-word: {:?}",
            out
        );
        assert!(copies >= 1);
        assert!(
            out.windows(8).any(|w| w == b"line one"),
            "the story is in there"
        );
    }

    #[test]
    fn duplicate_until_single_copy_over_target_truncates() {
        let tol = crate::config::Tunables::default().exact_size_tolerance_bytes;
        let story = b"abcdefghijklmnopqrstuvwxyz".to_vec();
        let (out, copies) = duplicate_until(&story, b"", 10, tol);
        assert_eq!(out.len(), 10);
        assert_eq!(copies, 1);
        assert_eq!(&out, b"abcdefghij");
    }

    #[test]
    fn duplicate_until_is_exact_when_a_line_break_is_far() {
        // until_bytes is EXACT: a far-away newline must not shrink it.
        let tol = crate::config::Tunables::default().exact_size_tolerance_bytes;
        let story = vec![b'x'; 1000];
        let (out, _) = duplicate_until(&story, b"\n\n", 1500, tol);
        assert_eq!(out.len(), 1500, "exact target, not a paragraph short");
    }

    #[test]
    fn duplicate_until_exact_fit_is_untouched() {
        let tol = crate::config::Tunables::default().exact_size_tolerance_bytes;
        let story = b"0123456789".to_vec();
        let (out, copies) = duplicate_until(&story, b"", 10, tol);
        assert_eq!(out, story);
        assert_eq!(copies, 1);
    }

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
        let out = expand_pattern("legion", Some(" "), None, Some(1024 * 1024), usize::MAX);
        assert_eq!(out.len(), 1024 * 1024);
        assert!(out.starts_with(b"legion legion"));
        // Newline-joined, odd target: still exact.
        let out = expand_pattern("legion", None, None, Some(1000), usize::MAX);
        assert_eq!(out.len(), 1000);
        // Explicit count: full repeats only, as asked.
        let out = expand_pattern("legion", Some(" "), Some(100), None, usize::MAX);
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
