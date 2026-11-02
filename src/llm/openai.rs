//! The OpenAI chat-completions protocol (and its many compatible servers).

use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{
    ChatOutput, ChatRequest, Endpoint, LlmError, ModelInfo, Msg, Part, ProviderKind, Quirks, Role,
    StreamEvent, ToolCall, Usage, http_client, network_error, parse_arguments, quirks, retry_after,
    set_quirks, sse::SseParser, text::ThinkSplitter,
};

fn message_json(message: &Msg) -> Value {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let content = if message.has_images() {
        Value::Array(
            message
                .parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => json!({ "type": "text", "text": text }),
                    Part::Image(url) => json!({ "type": "image_url", "image_url": { "url": url } }),
                })
                .collect(),
        )
    } else {
        Value::String(message.text())
    };
    let mut value = json!({ "role": role, "content": content });
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": { "name": call.name, "arguments": call.arguments.to_string() },
                    })
                })
                .collect(),
        );
    }
    if let Some(id) = &message.tool_call_id {
        value["tool_call_id"] = json!(id);
    }
    value
}

pub fn build_body(endpoint: &Endpoint, request: &ChatRequest<'_>, quirks: Quirks) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
        "stream": request.stream,
    });
    if !quirks.no_temperature {
        body["temperature"] = json!(request.temperature);
    }
    let use_completion_tokens =
        quirks.max_completion_tokens || endpoint.kind == ProviderKind::OpenAI;
    if use_completion_tokens {
        body["max_completion_tokens"] = json!(request.max_tokens);
    } else {
        body["max_tokens"] = json!(request.max_tokens);
    }
    if request.stream && !quirks.no_stream_options {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        },
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    if request.json && !quirks.no_json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    }
    if let Some(effort) = request.reasoning {
        if !quirks.no_reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
    }
    // LM Studio caps thinking exactly — the only reliable "off" for models
    // that always think.
    if endpoint.kind == ProviderKind::LmStudio && !quirks.no_thinking_budget {
        let budget = if request.reasoning == Some("none") { Some(0) } else { request.thinking_budget };
        if let Some(budget) = budget {
            body["thinking_budget_tokens"] = json!(budget);
        }
    }
    body
}

fn auth(endpoint: &Endpoint, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let builder = if endpoint.api_key.is_empty() {
        builder
    } else {
        builder.bearer_auth(&endpoint.api_key)
    };
    if endpoint.kind == ProviderKind::OpenRouter {
        builder
            .header("HTTP-Referer", "https://github.com/axis-agents")
            .header("X-Title", "Axis Agents")
    } else {
        builder
    }
}

/// Learn a quirk from a 400 response; returns true when a retry with the
/// adjusted request might succeed.
fn learn_quirk(body: &str, quirks: &mut Quirks) -> bool {
    let lower = body.to_lowercase();
    if lower.contains("stream_options") && !quirks.no_stream_options {
        quirks.no_stream_options = true;
        return true;
    }
    if lower.contains("max_tokens") && lower.contains("max_completion_tokens") {
        quirks.max_completion_tokens = !quirks.max_completion_tokens;
        return true;
    }
    if lower.contains("temperature")
        && (lower.contains("unsupported") || lower.contains("not support"))
        && !quirks.no_temperature
    {
        quirks.no_temperature = true;
        return true;
    }
    if lower.contains("response_format") && !quirks.no_json_mode {
        quirks.no_json_mode = true;
        return true;
    }
    if lower.contains("thinking_budget") && !quirks.no_thinking_budget {
        quirks.no_thinking_budget = true;
        return true;
    }
    if lower.contains("reasoning_effort") && !quirks.no_reasoning_effort {
        quirks.no_reasoning_effort = true;
        return true;
    }
    false
}

pub async fn chat(
    endpoint: &Endpoint,
    request: &ChatRequest<'_>,
    on_event: &mut (dyn FnMut(StreamEvent<'_>) + Send),
) -> Result<ChatOutput, LlmError> {
    let mut quirks = quirks(endpoint, request.model);
    let url = format!("{}/chat/completions", endpoint.base_url);
    for _attempt in 0..4 {
        let body = build_body(endpoint, request, quirks);
        let response = auth(
            endpoint,
            http_client()
                .post(&url)
                .timeout(endpoint.timeout)
                .json(&body),
        )
        .send()
        .await
        .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let wait = retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            if status.as_u16() == 400 && learn_quirk(&text, &mut quirks) {
                set_quirks(endpoint, request.model, quirks);
                continue;
            }
            return Err(LlmError::from_status(status.as_u16(), &text, wait));
        }
        return if request.stream {
            read_stream(response, on_event).await
        } else {
            let text = response.text().await.map_err(network_error)?;
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| LlmError::Malformed(format!("invalid JSON: {error}")))?;
            parse_response(&value, on_event)
        };
    }
    Err(LlmError::Malformed(
        "the provider kept rejecting the request parameters".to_owned(),
    ))
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

async fn read_stream(
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(StreamEvent<'_>) + Send),
) -> Result<ChatOutput, LlmError> {
    let mut stream = response.bytes_stream();
    let mut parser = SseParser::default();
    let mut splitter = ThinkSplitter::default();
    let mut output = ChatOutput::default();
    let mut calls: Vec<PartialCall> = Vec::new();

    let mut handle = |data: &str,
                      output: &mut ChatOutput,
                      calls: &mut Vec<PartialCall>,
                      splitter: &mut ThinkSplitter|
     -> Result<(), LlmError> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(_) => return Ok(()),
        };
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("stream error")
                .to_owned();
            return Err(LlmError::from_status(400, &message, None));
        }
        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            output.usage = Some(parse_usage(usage));
        }
        let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            output.finish_reason = Some(reason.to_owned());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta.get(key).and_then(Value::as_str) {
                if !text.is_empty() {
                    output.reasoning.push_str(text);
                    on_event(StreamEvent::Reasoning(text));
                }
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            let (content, reasoning) = splitter.push(text);
            if !reasoning.is_empty() {
                output.reasoning.push_str(&reasoning);
                on_event(StreamEvent::Reasoning(&reasoning));
            }
            if !content.is_empty() {
                output.content.push_str(&content);
                on_event(StreamEvent::Content(&content));
            }
        }
        if let Some(deltas) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in deltas.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize)
                    .unwrap_or(position);
                while calls.len() <= index {
                    calls.push(PartialCall::default());
                }
                let slot = &mut calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot.id = id.to_owned();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        if slot.name.is_empty() {
                            slot.name = name.to_owned();
                            on_event(StreamEvent::ToolCall(name));
                        }
                    }
                    match function.get("arguments") {
                        Some(Value::String(args)) => slot.arguments.push_str(args),
                        Some(other @ Value::Object(_)) => slot.arguments = other.to_string(),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    };

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(network_error)?;
        for data in parser.push(&chunk) {
            handle(&data, &mut output, &mut calls, &mut splitter)?;
        }
    }
    if let Some(data) = parser.finish() {
        handle(&data, &mut output, &mut calls, &mut splitter)?;
    }
    let (content, reasoning) = splitter.finish();
    if !reasoning.is_empty() {
        output.reasoning.push_str(&reasoning);
    }
    if !content.is_empty() {
        output.content.push_str(&content);
        on_event(StreamEvent::Content(&content));
    }
    if let Some((answer, stray)) = super::text::strip_stray_close(&output.content) {
        output.reasoning.push_str(&stray);
        output.content = answer;
    }
    output.tool_calls = calls
        .into_iter()
        .filter(|call| !call.name.is_empty())
        .enumerate()
        .map(|(index, call)| ToolCall {
            id: if call.id.is_empty() {
                format!("call_{index}_{}", fastrand::u32(..))
            } else {
                call.id
            },
            name: call.name,
            arguments: parse_arguments(&call.arguments),
        })
        .collect();
    Ok(output)
}

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    }
}

pub fn parse_response(
    body: &Value,
    on_event: &mut (dyn FnMut(StreamEvent<'_>) + Send),
) -> Result<ChatOutput, LlmError> {
    if let Some(error) = body.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error");
        return Err(LlmError::from_status(400, message, None));
    }
    let message = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .ok_or_else(|| LlmError::Malformed("no choices in the response".to_owned()))?;
    let raw = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (content, mut reasoning) = super::text::split_think(raw);
    for key in ["reasoning_content", "reasoning"] {
        if let Some(text) = message.get(key).and_then(Value::as_str) {
            if !text.is_empty() {
                reasoning = format!("{text}{reasoning}");
            }
        }
    }
    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let Some(function) = call.get("function") else {
                continue;
            };
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            on_event(StreamEvent::ToolCall(name));
            let arguments = match function.get("arguments") {
                Some(Value::String(raw)) => parse_arguments(raw),
                Some(other @ Value::Object(_)) => other.clone(),
                _ => json!({}),
            };
            tool_calls.push(ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{index}")),
                name: name.to_owned(),
                arguments,
            });
        }
    }
    if !reasoning.is_empty() {
        on_event(StreamEvent::Reasoning(&reasoning));
    }
    if !content.is_empty() {
        on_event(StreamEvent::Content(&content));
    }
    Ok(ChatOutput {
        content,
        reasoning,
        tool_calls,
        usage: body.get("usage").map(parse_usage),
        finish_reason: body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

async fn get_json(endpoint: &Endpoint, url: &str) -> Result<Value, String> {
    let response = auth(
        endpoint,
        http_client()
            .get(url)
            .timeout(std::time::Duration::from_secs(20)),
    )
    .send()
    .await
    .map_err(|error| network_error(error).to_string())?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(LlmError::from_status(status.as_u16(), &text, None).to_string());
    }
    serde_json::from_str(&text).map_err(|error| format!("invalid JSON: {error}"))
}

pub async fn list_models(endpoint: &Endpoint) -> Result<Vec<String>, String> {
    let body = get_json(endpoint, &format!("{}/models", endpoint.base_url)).await?;
    let mut models: Vec<String> = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|model| {
                    model
                        .get("id")
                        .or_else(|| model.get("name"))
                        .and_then(Value::as_str)
                        .map(|id| id.trim_start_matches("models/").to_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    models.dedup();
    if models.is_empty() {
        Err("the provider returned no models (is one loaded?)".to_owned())
    } else {
        Ok(models)
    }
}

/// The server root without the `/v1`-style suffix (LM Studio, Ollama).
fn server_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_owned()
}

pub async fn model_info(endpoint: &Endpoint, model: &str) -> ModelInfo {
    if model.is_empty() {
        return ModelInfo::default();
    }
    match endpoint.kind {
        ProviderKind::LmStudio => {
            // The v1 API reports how the model is loaded: its context and
            // how many parallel requests share it (each gets a slice).
            let url = format!("{}/api/v1/models", server_root(&endpoint.base_url));
            if let Ok(list) = get_json(endpoint, &url).await {
                let entry = list
                    .get("models")
                    .and_then(Value::as_array)
                    .and_then(|models| {
                        models.iter().find(|m| m.get("key").and_then(Value::as_str) == Some(model))
                    });
                if let Some(entry) = entry {
                    let instance = entry.pointer("/loaded_instances/0/config");
                    let loaded = instance
                        .and_then(|c| c.get("context_length"))
                        .and_then(Value::as_u64)
                        .map(|n| n as u32);
                    let parallel = instance
                        .and_then(|c| c.get("parallel"))
                        .and_then(Value::as_u64)
                        .map(|n| n.max(1) as u32);
                    return ModelInfo {
                        context_window: loaded
                            .map(|n| n / parallel.unwrap_or(1))
                            .or_else(|| entry.get("max_context_length").and_then(Value::as_u64).map(|n| n as u32)),
                        vision: entry.pointer("/capabilities/vision").and_then(Value::as_bool),
                        tools: entry
                            .pointer("/capabilities/trained_for_tool_use")
                            .and_then(Value::as_bool),
                        loaded_context: loaded,
                        parallel,
                    };
                }
            }
            let url = format!("{}/api/v0/models/{model}", server_root(&endpoint.base_url));
            match get_json(endpoint, &url).await {
                Ok(info) => ModelInfo {
                    context_window: info
                        .get("loaded_context_length")
                        .or_else(|| info.get("max_context_length"))
                        .and_then(Value::as_u64)
                        .map(|n| n as u32),
                    vision: info
                        .get("type")
                        .and_then(Value::as_str)
                        .map(|kind| kind == "vlm"),
                    tools: info
                        .get("capabilities")
                        .and_then(Value::as_array)
                        .map(|caps| caps.iter().any(|c| c.as_str() == Some("tool_use"))),
                    loaded_context: None,
                    parallel: None,
                },
                Err(_) => ModelInfo::default(),
            }
        }
        ProviderKind::Ollama => {
            let url = format!("{}/api/show", server_root(&endpoint.base_url));
            let Ok(response) = http_client()
                .post(&url)
                .timeout(std::time::Duration::from_secs(20))
                .json(&json!({ "model": model }))
                .send()
                .await
            else {
                return ModelInfo::default();
            };
            let Ok(info) = response.json::<Value>().await else {
                return ModelInfo::default();
            };
            let context = info
                .get("model_info")
                .and_then(Value::as_object)
                .and_then(|map| {
                    map.iter()
                        .find(|(key, _)| key.ends_with("context_length"))
                        .and_then(|(_, value)| value.as_u64())
                })
                .map(|n| n as u32);
            let caps: Vec<String> = info
                .get("capabilities")
                .and_then(Value::as_array)
                .map(|caps| {
                    caps.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            ModelInfo {
                // Ollama serves with a 2048–8192 default unless configured;
                // the model's trained length is an upper bound.
                context_window: context.map(|n| n.min(32_768)),
                vision: Some(caps.iter().any(|c| c == "vision")),
                tools: Some(caps.iter().any(|c| c == "tools")),
                ..ModelInfo::default()
            }
        }
        ProviderKind::OpenRouter => {
            match get_json(endpoint, &format!("{}/models", endpoint.base_url)).await {
                Ok(body) => body
                    .get("data")
                    .and_then(Value::as_array)
                    .and_then(|list| {
                        list.iter()
                            .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
                    })
                    .map(|m| ModelInfo {
                        context_window: m
                            .get("context_length")
                            .and_then(Value::as_u64)
                            .map(|n| n as u32),
                        vision: m
                            .pointer("/architecture/input_modalities")
                            .and_then(Value::as_array)
                            .map(|mods| mods.iter().any(|v| v.as_str() == Some("image"))),
                        tools: m
                            .get("supported_parameters")
                            .and_then(Value::as_array)
                            .map(|params| params.iter().any(|v| v.as_str() == Some("tools"))),
                        ..ModelInfo::default()
                    })
                    .unwrap_or_default(),
                Err(_) => ModelInfo::default(),
            }
        }
        _ => ModelInfo::default(),
    }
}

pub async fn embed(
    endpoint: &Endpoint,
    model: &str,
    inputs: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let response = auth(
        endpoint,
        http_client()
            .post(format!("{}/embeddings", endpoint.base_url))
            .timeout(std::time::Duration::from_secs(60))
            .json(&json!({ "model": model, "input": inputs })),
    )
    .send()
    .await
    .map_err(|error| network_error(error).to_string())?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(LlmError::from_status(status.as_u16(), &text, None).to_string());
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or("no embeddings in the response")?;
    let mut out: Vec<(usize, Vec<f32>)> = data
        .iter()
        .enumerate()
        .filter_map(|(position, item)| {
            let index = item
                .get("index")
                .and_then(Value::as_u64)
                .map(|i| i as usize)
                .unwrap_or(position);
            let vector = item
                .get("embedding")?
                .as_array()?
                .iter()
                .filter_map(Value::as_f64)
                .map(|v| v as f32)
                .collect();
            Some((index, vector))
        })
        .collect();
    out.sort_by_key(|(index, _)| *index);
    Ok(out.into_iter().map(|(_, vector)| vector).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;

    fn endpoint(kind: ProviderKind) -> Endpoint {
        Endpoint::new(kind, "http://localhost:1234/v1", "", 60)
    }

    #[test]
    fn body_shapes_messages_tools_and_images() {
        let messages = vec![
            Msg::system("sys"),
            Msg::user_parts(vec![
                Part::Text("look".to_owned()),
                Part::Image("https://x/y.png".to_owned()),
            ]),
            Msg::assistant_with_tools(
                "",
                vec![ToolCall {
                    id: "c1".to_owned(),
                    name: "react".to_owned(),
                    arguments: json!({ "emoji": "👍" }),
                }],
            ),
            Msg::tool_result("c1", "done"),
        ];
        let tools = vec![ToolSpec {
            name: "react".to_owned(),
            description: "d".to_owned(),
            parameters: json!({ "type": "object", "properties": {} }),
        }];
        let request = ChatRequest {
            thinking_budget: None,
            model: "m",
            messages: &messages,
            tools: &tools,
            temperature: 0.5,
            max_tokens: 100,
            stream: true,
            json: false,
            reasoning: None,
        };
        let body = build_body(
            &endpoint(ProviderKind::LmStudio),
            &request,
            Quirks::default(),
        );
        assert_eq!(body["messages"][1]["content"][1]["type"], "image_url");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "react"
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["stream_options"]["include_usage"], true);

        let body = build_body(
            &endpoint(ProviderKind::OpenAI),
            &request,
            Quirks {
                no_stream_options: true,
                ..Quirks::default()
            },
        );
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn response_parsing_splits_reasoning() {
        let body = json!({ "choices": [{ "message": {
            "content": "<think>hmm</think>Hi!", "reasoning_content": "first" } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 3 } });
        let output = parse_response(&body, &mut |_| {}).unwrap();
        assert_eq!(output.content, "Hi!");
        assert_eq!(super::super::text::split_think("plan the post</think>

Yo, new release!").0, "Yo, new release!");
        assert_eq!(super::super::text::split_think("just an answer").0, "just an answer");
        assert_eq!(output.reasoning, "firsthmm");
        assert_eq!(output.usage.unwrap().prompt_tokens, 10);
    }

    #[test]
    fn quirks_are_learned() {
        let mut quirks = Quirks::default();
        assert!(learn_quirk(
            "Unrecognized parameter stream_options",
            &mut quirks
        ));
        assert!(quirks.no_stream_options);
        assert!(!learn_quirk("something else", &mut quirks));
    }

    #[test]
    fn server_root_strips_version() {
        assert_eq!(
            server_root("http://localhost:1234/v1"),
            "http://localhost:1234"
        );
        assert_eq!(server_root("http://host:11434/"), "http://host:11434");
    }
}

/// Live checks against a local LM Studio; run with
/// `AXIS_LIVE_MODEL=qwen/qwen3.5-9b cargo test live_ -- --ignored`.
#[cfg(test)]
mod live {
    use super::*;

    fn red_square_png() -> String {
        let image = image::RgbaImage::from_pixel(48, 48, image::Rgba([220, 30, 30, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        crate::runtime::vision::prepare(&png).unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn live_vision_without_thinking() {
        let model = std::env::var("AXIS_LIVE_MODEL").unwrap_or_else(|_| "qwen/qwen3.5-9b".to_owned());
        let endpoint = Endpoint::new(ProviderKind::LmStudio, "http://localhost:1234/v1", "", 120);
        let messages = [Msg::user_parts(vec![
            Part::Text("What color is this square? One word.".to_owned()),
            Part::Image(red_square_png()),
        ])];
        for stream in [false, true] {
            let request = ChatRequest {
                thinking_budget: None,
                model: &model,
                messages: &messages,
                tools: &[],
                temperature: 0.2,
                max_tokens: 300,
                stream,
                json: false,
                reasoning: Some("none"),
            };
            let started = std::time::Instant::now();
            let output = endpoint.chat(&request, &mut |_| {}).await.unwrap();
            println!("stream={stream} {:.1}s content={:?}", started.elapsed().as_secs_f32(), output.content);
            assert!(output.content.to_lowercase().contains("red"), "{output:?}");
        }
    }
}
