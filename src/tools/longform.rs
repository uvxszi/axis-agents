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
    let done = args
        .get("finished")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "document.txt".to_owned());

    let (total_before, _tail_before, summary_before, sections_before) = {
        let mut map = docs().lock().expect("longform poisoned");
        let doc = map.entry(ctx.channel_id).or_default();
        if !doc.text.is_empty() && !doc.text.ends_with('\n') {
            doc.text.push('\n');
        }
        doc.text.push_str(section);
        doc.sections += 1;
        (
            doc.text.len(),
            tail_of(&doc.text),
            doc.summary.clone(),
            doc.sections,
        )
    };
    let _ = total_before;

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
        return Ok(format!(
            "document finished: {chars} characters across {sections_before} sections, saved as {name}. Attach it with attach_file (name: \"{name}\"). Do not rewrite it in chat."
        ));
    }

    Ok(format!(
        "Section {sections_before} kept ({tail_after_len} chars so far). Continue with the next section.\nYOUR PRIVATE NOTES (the user never sees these; do not repeat them as text):\n{summary}\nThe document currently ends with:\n---\n{tail_before_and_after}\n---\nWrite the next section now — continue seamlessly from that ending. Same voice, no recap, no heading like 'Section {next}' unless the structure calls for it.",
        tail_after_len = tail_after.len(),
        tail_before_and_after = tail_after,
        next = sections_before + 1,
    ))
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
