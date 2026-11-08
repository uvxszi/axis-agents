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
    Some((
        content[at + tag.len()..].trim().to_owned(),
        before.trim().to_owned(),
    ))
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

    // Fallback 1: an XML-style call — <function=name> or <fn=name> with
    // <parameter=key>value</parameter> children, with or without a
    // <tool_call> wrapper. Models trained on XML tool formats (Qwen's
    // native style) emit these when a server advertises no native tools;
    // treating them as calls keeps the markup out of the chat.
    if calls.is_empty() {
        let mut xml_rest = rest.clone();
        while let Some(call) = parse_call_xml(&xml_rest, calls.len()) {
            // Remove the matched block from xml_rest (the parse reports
            // where it stopped via the returned span length).
            let _ = &call;
            let span = xml_call_span(&xml_rest).unwrap_or((0, 0));
            xml_rest = format!(
                "{}{}",
                &xml_rest[..span.0],
                &xml_rest[span.1.min(xml_rest.len())..]
            );
            calls.push(call);
        }
        if !calls.is_empty() {
            return (calls, xml_rest.trim().to_owned());
        }
    }

    // Fallback 2: a bare JSON object (or ```json fence) that is a tool call.
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

/// Where the first XML-style call block starts and ends in `text`.
fn xml_call_span(text: &str) -> Option<(usize, usize)> {
    let start = text.find("<function=").or_else(|| text.find("<fn="))?;
    let open_end = text[start..].find('>')? + start + 1;
    // The block ends at the matching close tag, or at the next opening.
    let end = text[open_end..]
        .find("</function>")
        .or_else(|| text[open_end..].find("</fn>"))
        .or_else(|| text[open_end..].find("<function="))
        .or_else(|| text[open_end..].find("<fn="))
        .map(|at| open_end + at)?;
    // Include the closing tag itself when found.
    let close_len = if text[end..].starts_with("</function>") {
        "</function>".len()
    } else if text[end..].starts_with("</fn>") {
        "</fn>".len()
    } else {
        0
    };
    Some((start, (end + close_len).min(text.len())))
}

/// Parse the first XML-style call in `text`, if any.
fn parse_call_xml(text: &str, index: usize) -> Option<ToolCall> {
    let start = text.find("<function=").or_else(|| text.find("<fn="))?;
    let name_start = start + text[start..].find('=')? + 1;
    let name_end = name_start + text[name_start..].find('>')?;
    let name = text[name_start..name_end].trim().to_owned();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let body_end = xml_call_span(text)?.1;
    let body = &text[name_end + 1..body_end];
    let mut arguments = serde_json::Map::new();
    let mut remaining = body;
    while let Some(pstart) = remaining
        .find("<parameter=")
        .or_else(|| remaining.find("<param="))
    {
        let tag = if remaining[pstart..].starts_with("<parameter=") {
            "<parameter="
        } else {
            "<param="
        };
        let key_start = pstart + tag.len();
        let key_end = key_start + remaining[key_start..].find('>')?;
        let key = remaining[key_start..key_end].trim().to_owned();
        let value_start = key_end + 1;
        let value_end = remaining[value_start..]
            .find("</parameter>")
            .or_else(|| remaining[value_start..].find("</param>"))
            .map(|at| value_start + at)?;
        let value = remaining[value_start..value_end].trim();
        // Numbers, booleans and null stay typed; everything else is a
        // string.
        let parsed: Value = match value {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" => Value::Null,
            other => other
                .parse::<i64>()
                .map(Value::from)
                .or_else(|_| other.parse::<f64>().map(Value::from))
                .unwrap_or_else(|_| Value::String(other.to_owned())),
        };
        arguments.insert(key, parsed);
        remaining = &remaining[value_end
            + remaining[value_end..]
                .find('>')
                .map(|at| at + 1)
                .unwrap_or(0)..];
    }
    Some(ToolCall {
        id: format!("text_call_{index}_{}", fastrand::u32(..)),
        name,
        arguments: Value::Object(arguments),
    })
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
    fn xml_style_call_parses() {
        // The exact shape a Qwen model emits when it leaks its native
        // format into the text protocol.
        let text = "<function=send_message>
<parameter=channel>general</parameter>
<parameter=content>hi there</parameter>
</function>";
        let (calls, rest) = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "send_message");
        assert_eq!(calls[0].arguments["channel"], "general");
        assert_eq!(calls[0].arguments["content"], "hi there");
        assert!(
            !rest.contains("<function"),
            "markup must be consumed: {rest}"
        );
    }

    #[test]
    fn xml_style_call_without_wrapper_parses() {
        let text = "sure, posting it now
<fn=send_message><param=content>done</param></fn>";
        let (calls, rest) = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "send_message");
        assert_eq!(calls[0].arguments["content"], "done");
        assert!(rest.contains("posting it now"));
    }

    #[test]
    fn xml_params_stay_typed() {
        let text = "<function=timeout><parameter=minutes>10</parameter><parameter=user_id>42</parameter><parameter>note</parameter></function>";
        let (calls, _) = parse_text_tool_calls(text);
        assert_eq!(calls[0].arguments["minutes"], 10);
        assert_eq!(calls[0].arguments["user_id"], 42);
        assert_eq!(calls[0].arguments["note"], Value::Null);
    }

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
