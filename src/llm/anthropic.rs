//! Anthropic's Messages API.

use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{
    ChatOutput, ChatRequest, Endpoint, LlmError, Msg, Part, Role, StreamEvent, ToolCall, Usage,
    http_client, network_error, parse_arguments, retry_after, sse::SseParser,
};

const VERSION: &str = "2023-06-01";

fn image_block(url: &str) -> Value {
    // data:<media type>;base64,<data>
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            let media_type = meta.trim_end_matches(";base64");
            return json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            });
        }
    }
    json!({ "type": "image", "source": { "type": "url", "url": url } })
}

/// Convert the neutral transcript: system messages join the `system`
/// field, tool results become `tool_result` blocks in user turns, and
/// consecutive same-role turns merge (the API expects alternation).
pub fn build_body(request: &ChatRequest<'_>) -> Value {
    let mut system = Vec::new();
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in request.messages {
        let (role, blocks): (&'static str, Vec<Value>) = match message.role {
            Role::System => {
                system.push(message.text());
                continue;
            }
            Role::User => ("user", content_blocks(message)),
            Role::Assistant => {
                let mut blocks = Vec::new();
                let text = message.text();
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for call in &message.tool_calls {
                    let input = if call.arguments.is_object() {
                        call.arguments.clone()
                    } else {
                        json!({})
                    };
                    blocks.push(json!({
                        "type": "tool_use", "id": call.id, "name": call.name, "input": input,
                    }));
                }
                ("assistant", blocks)
            }
            Role::Tool => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": message.text(),
                })],
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }
    // The first turn must be from the user.
    if turns.first().is_some_and(|(role, _)| *role == "assistant") {
        turns.insert(
            0,
            (
                "user",
                vec![json!({ "type": "text", "text": "(conversation start)" })],
            ),
        );
    }
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.max(1),
        "temperature": request.temperature.clamp(0.0, 1.0),
        "messages": turns
            .into_iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect::<Vec<_>>(),
        "stream": request.stream,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

fn content_blocks(message: &Msg) -> Vec<Value> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(text) if text.trim().is_empty() => None,
            Part::Text(text) => Some(json!({ "type": "text", "text": text })),
            Part::Image(url) => Some(image_block(url)),
        })
        .collect()
}

fn request(endpoint: &Endpoint, url: String) -> reqwest::RequestBuilder {
    http_client()
        .post(url)
        .header("x-api-key", &endpoint.api_key)
        .header("anthropic-version", VERSION)
}

pub async fn chat(
    endpoint: &Endpoint,
    chat_request: &ChatRequest<'_>,
    on_event: &mut (dyn FnMut(StreamEvent<'_>) + Send),
) -> Result<ChatOutput, LlmError> {
    let body = build_body(chat_request);
    let response = request(endpoint, format!("{}/messages", endpoint.base_url))
        .timeout(endpoint.timeout)
        .json(&body)
        .send()
        .await
        .map_err(network_error)?;
    let status = response.status();
    if !status.is_success() {
        let wait = retry_after(response.headers());
        let text = response.text().await.unwrap_or_default();
        let status = if status.as_u16() == 529 {
            503
        } else {
            status.as_u16()
        };
        return Err(LlmError::from_status(status, &text, wait));
    }
    if chat_request.stream {
        read_stream(response, on_event).await
    } else {
        let value: Value = response.json().await.map_err(network_error)?;
        parse_response(&value, on_event)
    }
}

pub fn parse_response(
    body: &Value,
    on_event: &mut (dyn FnMut(StreamEvent<'_>) + Send),
) -> Result<ChatOutput, LlmError> {
    let blocks = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Malformed("no content in the response".to_owned()))?;
    let mut output = ChatOutput::default();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                output.content.push_str(text);
            }
            Some("thinking") => {
                let text = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                output.reasoning.push_str(text);
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                on_event(StreamEvent::ToolCall(name));
                output.tool_calls.push(ToolCall {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: name.to_owned(),
                    arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                });
            }
            _ => {}
        }
    }
    if !output.reasoning.is_empty() {
        on_event(StreamEvent::Reasoning(&output.reasoning));
    }
    if !output.content.is_empty() {
        on_event(StreamEvent::Content(&output.content));
    }
    output.usage = body.get("usage").map(|usage| Usage {
        prompt_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        completion_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    });
    output.finish_reason = body
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(output)
}

#[derive(Default)]
struct Block {
    kind: String,
    id: String,
    name: String,
    json: String,
}

async fn read_stream(
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(StreamEvent<'_>) + Send),
) -> Result<ChatOutput, LlmError> {
    let mut stream = response.bytes_stream();
    let mut parser = SseParser::default();
    let mut output = ChatOutput::default();
    let mut blocks: Vec<Block> = Vec::new();
    let mut usage = Usage::default();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(network_error)?;
        for data in parser.push(&chunk) {
            let Ok(event) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    usage.prompt_tokens = event
                        .pointer("/message/usage/input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                }
                Some("content_block_start") => {
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while blocks.len() <= index {
                        blocks.push(Block::default());
                    }
                    let block = event.get("content_block").cloned().unwrap_or_default();
                    let slot = &mut blocks[index];
                    slot.kind = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    slot.id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    slot.name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if slot.kind == "tool_use" {
                        on_event(StreamEvent::ToolCall(&slot.name));
                    }
                }
                Some("content_block_delta") => {
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let delta = event.get("delta").cloned().unwrap_or_default();
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                            output.content.push_str(text);
                            on_event(StreamEvent::Content(text));
                        }
                        Some("thinking_delta") => {
                            let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                            output.reasoning.push_str(text);
                            on_event(StreamEvent::Reasoning(text));
                        }
                        Some("input_json_delta") => {
                            if let Some(slot) = blocks.get_mut(index) {
                                slot.json.push_str(
                                    delta
                                        .get("partial_json")
                                        .and_then(Value::as_str)
                                        .unwrap_or(""),
                                );
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) =
                        event.pointer("/delta/stop_reason").and_then(Value::as_str)
                    {
                        output.finish_reason = Some(reason.to_owned());
                    }
                    if let Some(tokens) = event
                        .pointer("/usage/output_tokens")
                        .and_then(Value::as_u64)
                    {
                        usage.completion_tokens = tokens as u32;
                    }
                }
                Some("error") => {
                    let message = event
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("stream error");
                    return Err(LlmError::from_status(400, message, None));
                }
                _ => {}
            }
        }
    }
    output.tool_calls = blocks
        .into_iter()
        .filter(|block| block.kind == "tool_use")
        .map(|block| ToolCall {
            id: block.id,
            name: block.name,
            arguments: parse_arguments(&block.json),
        })
        .collect();
    output.usage = Some(usage);
    Ok(output)
}

pub async fn list_models(endpoint: &Endpoint) -> Result<Vec<String>, String> {
    let response = http_client()
        .get(format!("{}/models?limit=100", endpoint.base_url))
        .header("x-api-key", &endpoint.api_key)
        .header("anthropic-version", VERSION)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| network_error(error).to_string())?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(LlmError::from_status(status.as_u16(), &text, None).to_string());
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;

    #[test]
    fn transcript_converts_and_merges() {
        let messages = vec![
            Msg::system("be kind"),
            Msg::user("hi"),
            Msg::user("again"),
            Msg::assistant_with_tools(
                "checking",
                vec![ToolCall {
                    id: "t1".to_owned(),
                    name: "get_time".to_owned(),
                    arguments: json!({}),
                }],
            ),
            Msg::tool_result("t1", "noon"),
        ];
        let tools = vec![ToolSpec {
            name: "get_time".to_owned(),
            description: "d".to_owned(),
            parameters: json!({ "type": "object", "properties": {} }),
        }];
        let request = ChatRequest {
            thinking_budget: None,
            model: "claude-sonnet-5-5",
            messages: &messages,
            tools: &tools,
            temperature: 1.4,
            max_tokens: 500,
            stream: false,
            json: false,
            reasoning: None,
        };
        let body = build_body(&request);
        assert_eq!(body["system"], "be kind");
        let turns = body["messages"].as_array().unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(turns[1]["content"][1]["type"], "tool_use");
        assert_eq!(turns[2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["temperature"], 1.0);
    }

    #[test]
    fn data_uri_images_become_base64_blocks() {
        let block = image_block("data:image/png;base64,AAAA");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(image_block("https://x/y.png")["source"]["type"], "url");
    }

    #[test]
    fn response_parses_tool_use() {
        let body = json!({ "content": [
            { "type": "text", "text": "ok" },
            { "type": "tool_use", "id": "a", "name": "react", "input": { "emoji": "x" } }
        ], "usage": { "input_tokens": 5, "output_tokens": 2 }, "stop_reason": "tool_use" });
        let output = parse_response(&body, &mut |_| {}).unwrap();
        assert_eq!(output.content, "ok");
        assert_eq!(output.tool_calls[0].arguments["emoji"], "x");
        assert_eq!(output.usage.unwrap().completion_tokens, 2);
    }
}
