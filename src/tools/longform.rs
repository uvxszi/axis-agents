//! Long-form writing with a private rolling summary.
//!
//! A model asked for "a full chapter" with a small context window writes
//! summaries instead of prose: it knows it cannot hold the whole text and
//! keep writing, so it compresses. `write_section` fixes the mechanism:
//! the model writes one section at a time; after each section the APP
//! folds what exists into a short private summary (a background-model
//! call, capped tight) and hands back only that summary plus the last few
//! lines of the text. The model's context stays flat no matter how long
//! the document grows, and it never needs to summarize *to the user* —
//! the summary is its own working memory, invisible in the chat.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use serde_json::Value;

use super::ToolCtx;
use crate::llm::truncate;

/// One in-progress document per channel.
#[derive(Default)]
struct Doc {
    text: String,
    /// The model's private running summary (never shown to the user).
    summary: String,
    sections: u32,
    /// The file name this document will be published under.
    name: String,
    /// The size target for the WHOLE document, remembered from the first
    /// call that set it — finishing early is not the model's call.
    target: Option<usize>,
}

fn docs() -> &'static Mutex<HashMap<u64, Doc>> {
    static DOCS: OnceLock<Mutex<HashMap<u64, Doc>>> = OnceLock::new();
    DOCS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How much of the document tail the model sees to continue seamlessly.
const TAIL_CHARS: usize = 400;
/// Hard cap on the private summary length.
const SUMMARY_CHARS: usize = 900;

/// Append a section to the channel's document. Returns what the model
/// needs to keep writing — the private summary and the tail — and nothing
/// else: the full text never re-enters its context.
pub async fn write_section(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let section = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or("give the section's `text`")?;
    if section.trim().is_empty() {
        return Err("the section is empty".to_owned());
    }
    // This channel's file already reached the size the person asked for
    // (usually via duplicate_file): no more sections — reply instead.
    if let Some(want) = super::files::already_satisfied_pub(ctx.channel_id) {
        return Ok(format!(
            "the file for this channel is already at the {want} asked for. Nothing to do: answer the person now.",
            want = super::files::size_label(want)
        ));
    }
    let done = args
        .get("finished")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // An optional size target (until_bytes/size) for the WHOLE document:
    // when set, progress is reported in MB each section and finishing
    // early is not the model's call.
    let target: Option<usize> = args
        .get("until_bytes")
        .or_else(|| args.get("size"))
        .and_then(Value::as_u64)
        .map(|v| v as usize);
    let mut name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "document.txt".to_owned());
    // A file with an open size target was already started (write_file) —
    // continue THAT one, even when the model forgot to pass its name.
    {
        let dir = super::files::sandbox_dir_pub(ctx.channel_id);
        let named = super::files::sanitize_name_pub(&name)
            .ok()
            .map(|n| dir.join(n));
        let doc_empty = {
            let map = docs().lock().expect("longform poisoned");
            map.get(&ctx.channel_id)
                .map(|d| d.text.is_empty())
                .unwrap_or(true)
        };
        if doc_empty && !named.as_ref().is_some_and(|p| p.exists()) {
            if let Some((short_name, _, _)) = super::files::peek_shortfall(ctx.channel_id) {
                let path = super::files::sanitize_name_pub(&short_name)
                    .ok()
                    .map(|n| dir.join(n));
                if path.as_ref().is_some_and(|p| p.exists()) {
                    name = short_name;
                }
            }
        }
    }

    let (summary_before, sections_before, doc_target) = {
        let mut map = docs().lock().expect("longform poisoned");
        let doc = map.entry(ctx.channel_id).or_default();
        doc.name = name.clone();
        // The target sticks once set: a model that drops until_bytes on a
        // later call must not get to finish early.
        if target.is_some() {
            doc.target = target;
        }
        let doc_target = doc.target;
        // A file the model already started with write_file under this
        // name: pick it up so the document continues instead of restarting.
        if doc.text.is_empty() {
            let existing = std::fs::read(
                super::files::sandbox_dir_pub(ctx.channel_id)
                    .join(super::files::sanitize_name_pub(&name).unwrap_or_else(|_| name.clone())),
            )
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default();
            if !existing.trim().is_empty() {
                doc.text = existing;
            }
        }
        if !doc.text.is_empty() && !doc.text.ends_with('\n') {
            doc.text.push('\n');
        }
        doc.text.push_str(section);
        doc.sections += 1;
        (doc.summary.clone(), doc.sections, doc_target)
    };

    // Fold the new section into the private summary. The summary is the
    // model's memory of its own text — characters, plot, arguments made,
    // where it stopped — and only the model ever sees it.
    let summary = super::super::runtime::brain::quick_completion(
        &ctx.core,
        "You maintain a writer's private memory of a long document they are producing section by section. Produce a dense running summary (max 130 words): key characters/subjects, established facts and plot, style/tone commitments, open threads to pick up, and exactly where the text stopped. This summary replaces the previous one — carry forward everything still relevant, drop what is finished.",
        &format!(
            "Previous summary (may be empty):\n{}\n\nNew section just written:\n{}\n\nWrite the updated summary.",
            if summary_before.is_empty() { "(none yet)" } else { summary_before.as_str() },
            truncate(section, 4000),
        ),
        350,
        false,
    )
    .await
    .unwrap_or_else(|_| {
        // The judge call failing must never stop the writing; a crude
        // summary beats none.
        format!("{summary_before}\n+ {section}")
    });
    let summary = truncate(summary.trim(), SUMMARY_CHARS).to_owned();

    {
        let mut map = docs().lock().expect("longform poisoned");
        let doc = map.entry(ctx.channel_id).or_default();
        doc.summary = summary.clone();
    }

    let tail_after = {
        let map = docs().lock().expect("longform poisoned");
        tail_of(
            &map.get(&ctx.channel_id)
                .map(|d| d.text.as_str())
                .unwrap_or(""),
        )
    };

    let (current_label, target_label, short, pace_hint) = {
        let map = docs().lock().expect("longform poisoned");
        let len = map.get(&ctx.channel_id).map(|d| d.text.len()).unwrap_or(0);
        let current = super::files::size_label(len);
        let want = doc_target.map(super::files::size_label).unwrap_or_default();
        let short = doc_target.is_some_and(|t| len < t);
        // Keep the shortfall current so the brain's "not finished" nudge
        // reflects real progress (and clears when the target is met).
        if let Some(t) = doc_target {
            super::files::note_shortfall(ctx.channel_id, &name, len, t);
        }
        // At this pace the target is unreachable by writing alone — a
        // 500 KB ask is not 170 sections of prose. Say it HERE, in the
        // result the model reads, before its rounds run out.
        let pace_hint = match doc_target {
            Some(t) if t > len && t - len > section.len().max(2000) * 4 => format!(
                " AT THIS PACE YOU WILL NOT REACH {want} before your tool steps run out — call duplicate_file (name: \"{name}\") NOW: one call fills the rest exactly (copy-paste is what \"fill to size\" means).",
                want = want,
                name = name,
            ),
            _ => String::new(),
        };
        (current, want, short, pace_hint)
    };
    if done && short {
        return Err(format!(
            "not finished: the document is {current_label} of the {target_label} asked for. Keep writing sections — finished: true is not allowed until the target is reached."
        ));
    }
    if done {
        // Publish: write the whole document to the sandbox as a file.
        let text = {
            let mut map = docs().lock().expect("longform poisoned");
            map.remove(&ctx.channel_id)
                .map(|d| d.text)
                .unwrap_or_default()
        };
        let dir = super::files::sandbox_dir_pub(ctx.channel_id);
        let path = dir.join(super::files::sanitize_name_pub(&name)?);
        crate::paths::write_atomic(&path, text.as_bytes())?;
        let chars = text.chars().count();
        let label = super::files::size_label(text.len());
        // The ask was "a file" — deliver it: attach with the reply (when
        // it fits an upload) instead of leaving the model to chain
        // another tool call.
        let upload_limit = ctx.core.hive.settings().tunables.upload_limit_bytes;
        if text.len() <= upload_limit {
            ctx.media
                .lock()
                .expect("media poisoned")
                .add_file(&name, text.into_bytes());
            return Ok(format!(
                "document finished: {chars} characters ({label}) across {sections_before} sections, saved as {name} AND attached below your reply — just confirm to the person, do not rewrite it in chat."
            ));
        }
        return Ok(format!(
            "document finished: {chars} characters ({label}) across {sections_before} sections, saved as {name} in the sandbox (too large to attach). Do not rewrite it in chat."
        ));
    }

    let progress = if doc_target.is_some() {
        format!(
            " Document so far: {current_label} of the {target_label} target — keep writing until you reach it."
        )
    } else {
        String::new()
    };
    Ok(format!(
        "Section {sections_before} kept.{progress}{pace_hint}
YOUR PRIVATE NOTES (the user never sees these; do not repeat them as text):
{summary}
The document currently ends with:
---
{tail}
---
Write the next section now — continue seamlessly from that ending. Same voice, no recap, never a summary in place of prose.",
        tail = tail_after,
    ))
}

/// Publish the in-progress document to the sandbox NOW (without ending
/// it): duplicate_file uses this when the sandbox file is missing or
/// smaller than what the sections built — the doc is the truth. Returns
/// (name, bytes) when there is anything worth publishing.
pub fn publish_doc(channel_id: u64) -> Option<(String, usize)> {
    let map = docs().lock().expect("longform poisoned");
    let doc = map.get(&channel_id)?;
    if doc.text.trim().is_empty() {
        return None;
    }
    let name = doc.name.clone();
    let len = doc.text.len();
    let text = doc.text.clone();
    drop(map);
    let dir = super::files::sandbox_dir_pub(channel_id);
    if let Ok(sanitized) = super::files::sanitize_name_pub(&name) {
        let path = dir.join(&sanitized);
        let on_disk = std::fs::metadata(&path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if len > on_disk {
            let _ = crate::paths::write_atomic(&path, text.as_bytes());
        }
    }
    Some((name, len))
}

/// Peek at the current document (used by tests and the dashboard).
pub fn current(channel_id: u64) -> (String, String, u32) {
    let map = docs().lock().expect("longform poisoned");
    map.get(&channel_id)
        .map(|d| (d.text.clone(), d.summary.clone(), d.sections))
        .unwrap_or_default()
}

/// Drop the channel's document.
pub fn discard(channel_id: u64) {
    docs()
        .lock()
        .expect("longform poisoned")
        .remove(&channel_id);
}

fn tail_of(text: &str) -> String {
    let start = text.len().saturating_sub(TAIL_CHARS);
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= start)
        .unwrap_or(start);
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_takes_the_last_lines() {
        let text = "a".repeat(1000) + "\nthe end\n";
        let tail = tail_of(&text);
        assert!(tail.len() <= TAIL_CHARS + 8);
        assert!(tail.ends_with("the end\n"));
    }

    #[test]
    fn tail_of_short_text_is_whole_text() {
        assert_eq!(tail_of("short"), "short");
    }
}
