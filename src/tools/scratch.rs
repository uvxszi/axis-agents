//! Temporary storage that everything flows through.
//!
//! A small model forgets: three tool calls later it no longer remembers
//! what it wrote or read, so it redoes work (or "hand-copies" from an
//! earlier result and gets it wrong). The scratchpad fixes the mechanism:
//! every tool result in a channel is recorded here, capped and numbered,
//! and `recall_scratch` pulls it back verbatim. The model never re-derives
//! anything — it references the scratch.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use serde_json::Value;

use super::ToolCtx;

/// One recorded result (or note), as the model may reference it.
#[derive(Debug, Clone)]
struct Entry {
    tool: String,
    text: String,
    at: i64,
}

/// Per-channel scratch, newest last. Capped: recent results are the
/// relevant ones, and this is a memory aid, not an archive.
fn pad() -> &'static Mutex<HashMap<u64, Vec<Entry>>> {
    static PAD: OnceLock<Mutex<HashMap<u64, Vec<Entry>>>> = OnceLock::new();
    PAD.get_or_init(|| Mutex::new(HashMap::new()))
}

const MAX_ENTRIES: usize = 120;
const MAX_ENTRY_CHARS: usize = 1600;

/// Record a tool result (called from the dispatch gateway — everything
/// flows through here).
pub fn record(channel_id: u64, tool: &str, text: &str) {
    let mut pad = pad().lock().expect("scratch poisoned");
    let entries = pad.entry(channel_id).or_default();
    entries.push(Entry {
        tool: tool.to_owned(),
        text: crate::llm::truncate(text, MAX_ENTRY_CHARS).to_owned(),
        at: crate::memory::now_ms(),
    });
    if entries.len() > MAX_ENTRIES {
        let drop = entries.len() - MAX_ENTRIES;
        entries.drain(..drop);
    }
}

/// Look back through the scratch. With a query, only matching entries;
/// without, the most recent ones. Newest last, numbered.
pub async fn recall(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let pad = pad().lock().expect("scratch poisoned");
    let entries = pad.get(&ctx.channel_id).cloned().unwrap_or_default();
    drop(pad);
    if entries.is_empty() {
        return Ok("the scratchpad is empty for this channel — nothing recorded yet".to_owned());
    }
    let matched: Vec<&Entry> = if query.is_empty() {
        entries.iter().rev().take(15).collect::<Vec<_>>()
    } else {
        entries
            .iter()
            .filter(|e| {
                e.text.to_lowercase().contains(&query) || e.tool.to_lowercase().contains(&query)
            })
            .rev()
            .take(15)
            .collect()
    };
    if matched.is_empty() {
        let tools: Vec<String> = entries
            .iter()
            .rev()
            .take(8)
            .map(|e| e.tool.clone())
            .collect();
        return Ok(format!(
            "nothing in the scratch matches {query:?} — recent tools used: {}",
            tools.join(", ")
        ));
    }
    let mut out = String::from("Scratchpad (oldest first):\n");
    for (index, entry) in matched.iter().rev().enumerate() {
        let ago = (crate::memory::now_ms() - entry.at) / 1000;
        out.push_str(&format!(
            "[{}] {} ({}s ago): {}\n",
            index + 1,
            entry.tool,
            ago,
            crate::llm::truncate(&entry.text, 400)
        ));
    }
    out.push_str(
        "These are verbatim records of earlier results — copy from them instead of redoing work.",
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_caps() {
        for n in 0..(MAX_ENTRIES + 10) {
            record(999_001, "write_file", &format!("result {n}"));
        }
        let pad = pad().lock().unwrap();
        let entries = &pad[&999_001];
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(entries.last().unwrap().text.contains("result 129"));
    }
}
