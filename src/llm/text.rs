//! Text-level helpers: separating `<think>` reasoning from answers, and a
//! text tool-calling protocol for models without native function calling.

use serde_json::Value;

use super::{ToolCall, ToolSpec};

const OPEN_TAGS: [&str; 2] = ["<think>", "<thinking>"];
const CLOSE_TAGS: [&str; 2] = ["</think>", "</thinking>"];

/// Splits streamed text into answer and reasoning, routing anything inside
/// `<think>…</think>` (DeepSeek-R1, Qwen3, …) to reasoning. Tags split
/// across chunks are handled by holding back a possible tag prefix.
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    in_think: bool,
    held: String,
    /// Strip whitespace right after a closing tag.
    trim_next: bool,
}

impl ThinkSplitter {
    /// Returns `(content, reasoning)` deltas for this chunk.
    pub fn push(&mut self, chunk: &str) -> (String, String) {
        let mut text = std::mem::take(&mut self.held);
        text.push_str(chunk);
        let mut content = String::new();
        let mut reasoning = String::new();
        loop {
            let tags: &[&str] = if self.in_think {
                &CLOSE_TAGS
            } else {
                &OPEN_TAGS
            };
            let found = tags
                .iter()
                .filter_map(|tag| text.find(tag).map(|at| (at, *tag)))
                .min_by_key(|(at, _)| *at);
            match found {
                Some((at, tag)) => {
                    let before = &text[..at];
                    if self.in_think {
                        reasoning.push_str(before);
                    } else {
                        self.emit_content(&mut content, before);
                    }
                    self.in_think = !self.in_think;
                    if !self.in_think {
                        self.trim_next = true;
                    }
                    text = text[at + tag.len()..].to_owned();
                }
                None => {
                    // Hold back a suffix that could be the start of a tag.
                    let hold = tags
                        .iter()
                        .flat_map(|tag| (1..tag.len()).map(move |n| &tag[..n]))
                        .filter(|prefix| text.ends_with(prefix))
                        .map(str::len)
                        .max()
                        .unwrap_or(0);
                    let cut = text.len() - hold;
                    let (now, later) = text.split_at(cut);
                    if self.in_think {
                        reasoning.push_str(now);
                    } else {
                        self.emit_content(&mut content, now);
                    }
                    self.held = later.to_owned();
                    break;
                }
            }
        }
        (content, reasoning)
    }

    fn emit_content(&mut self, content: &mut String, text: &str) {
        if self.trim_next {
            let trimmed = text.trim_start();
            if !trimmed.is_empty() {
                self.trim_next = false;
            }
            content.push_str(trimmed);
        } else {
            content.push_str(text);
        }
    }

    /// Flush held-back text at the end of the stream.
    pub fn finish(&mut self) -> (String, String) {
        let held = std::mem::take(&mut self.held);
        if self.in_think {
            (String::new(), held)
        } else {
            (held, String::new())
        }
    }
}

/// A closing `</think>` with no opening tag (the chat template opened it
/// inside the prompt): everything before it was reasoning.
pub fn strip_stray_close(content: &str) -> Option<(String, String)> {
    let (at, tag) = CLOSE_TAGS
        .iter()
        .filter_map(|tag| content.rfind(tag).map(|at| (at, *tag)))
        .max_by_key(|(at, _)| *at)?;
    let before = &content[..at];
    if OPEN_TAGS.iter().any(|open| before.contains(open)) {
        return None;
    }
    Some((content[at + tag.len()..].trim().to_owned(), before.trim().to_owned()))
}

/// Split a complete text into `(answer, reasoning)`.
pub fn split_think(text: &str) -> (String, String) {
    let mut splitter = ThinkSplitter::default();
    let (mut content, mut reasoning) = splitter.push(text);
    let (c, r) = splitter.finish();
    content.push_str(&c);
    reasoning.push_str(&r);
    if let Some((answer, stray)) = strip_stray_close(&content) {
        return (answer, format!("{reasoning}{stray}").trim().to_owned());
    }
    (content.trim().to_owned(), reasoning.trim().to_owned())
}

/// System-prompt section describing tools for the text protocol.
pub fn tool_protocol_prompt(tools: &[ToolSpec]) -> String {
    let mut out = String::from(
        "## Tools\nYou can call tools. To call one, reply with ONLY one or more blocks like:\n\
         <tool_call>{\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}</tool_call>\n\
         Results come back in a message starting with TOOL RESULT. Then continue: call more \
         tools or write your final reply (plain text, no tool_call blocks).\n\nAvailable tools:\n",
    );
    for tool in tools {
        out.push_str(&format!("- {}: {}", tool.name, tool.description));
        if let Some(props) = tool.parameters.get("properties").and_then(Value::as_object) {
            let required: Vec<&str> = tool
                .parameters
                .get("required")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let params: Vec<String> = props
                .iter()
                .map(|(name, schema)| {
                    let kind = schema
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("string");
                    let mark = if required.contains(&name.as_str()) {
                        ""
                    } else {
                        "?"
                    };
                    format!("{name}{mark}: {kind}")
                })
                .collect();
            if !params.is_empty() {
                out.push_str(&format!(" ({})", params.join(", ")));
            }
        }
        out.push('\n');
    }
    out
}

/// Extract text-protocol tool calls; returns the calls and the text with
/// the blocks removed.
pub fn parse_text_tool_calls(text: &str) -> (Vec<ToolCall>, String) {
    let mut calls = Vec::new();
    let mut rest = String::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("<tool_call>") {
        rest.push_str(&remaining[..start]);
        let after = &remaining[start + "<tool_call>".len()..];
        let (inner, next) = match after.find("</tool_call>") {
            Some(end) => (&after[..end], &after[end + "</tool_call>".len()..]),
            None => (after, ""),
        };
        if let Some(call) = parse_call_json(inner, calls.len()) {
            calls.push(call);
        }
        remaining = next;
    }
    rest.push_str(remaining);

    // Fallback: a bare JSON object (or ```json fence) that is a tool call.
    if calls.is_empty() {
        let trimmed = rest.trim();
        let unfenced = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .and_then(|inner| inner.strip_suffix("```"))
            .unwrap_or(trimmed)
            .trim();
        if unfenced.starts_with('{') && unfenced.contains("\"name\"") {
            if let Some(call) = parse_call_json(unfenced, 0) {
                return (vec![call], String::new());
            }
        }
    }
    (calls, rest.trim().to_owned())
}

fn parse_call_json(inner: &str, index: usize) -> Option<ToolCall> {
    let value: Value = serde_json::from_str(inner.trim()).ok().or_else(|| {
        let start = inner.find('{')?;
        let end = inner.rfind('}')?;
        serde_json::from_str(&inner[start..=end]).ok()
    })?;
    let name = value.get("name")?.as_str()?.to_owned();
    let arguments = match value.get("arguments").or_else(|| value.get("parameters")) {
        Some(Value::String(raw)) => super::parse_arguments(raw),
        Some(other) => other.clone(),
        None => Value::Object(Default::default()),
    };
    Some(ToolCall {
        id: format!("text_call_{index}_{}", fastrand::u32(..)),
        name,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_tags_split_across_chunks() {
        let mut splitter = ThinkSplitter::default();
        let mut content = String::new();
        let mut reasoning = String::new();
        for chunk in ["<thi", "nk>let me ", "think</th", "ink>\n\nHello ", "world"] {
            let (c, r) = splitter.push(chunk);
            content.push_str(&c);
            reasoning.push_str(&r);
        }
        let (c, r) = splitter.finish();
        content.push_str(&c);
        reasoning.push_str(&r);
        assert_eq!(content, "Hello world");
        assert_eq!(reasoning, "let me think");
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(
            split_think("a < b and c"),
            ("a < b and c".to_owned(), String::new())
        );
    }

    #[test]
    fn text_tool_calls_parse() {
        let (calls, rest) = parse_text_tool_calls(
            "Sure.\n<tool_call>{\"name\": \"web_search\", \"arguments\": {\"query\": \"rust\"}}</tool_call>",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].arguments["query"], "rust");
        assert_eq!(rest, "Sure.");

        let (calls, _) =
            parse_text_tool_calls("```json\n{\"name\": \"get_time\", \"arguments\": {}}\n```");
        assert_eq!(calls[0].name, "get_time");

        let (calls, rest) = parse_text_tool_calls("just a normal reply");
        assert!(calls.is_empty());
        assert_eq!(rest, "just a normal reply");
    }
}
