//! Minimal OpenAI-compatible chat-completions client.
//!
//! Works with LM Studio (`http://localhost:1234/v1`), OpenAI, OpenRouter,
//! Groq, and any other provider that speaks the `/chat/completions` shape.
//! Request building and response parsing are pure functions so they are
//! unit-testable without a server.

use std::sync::OnceLock;

use serde_json::{Value, json};

/// Which provider preset is selected in the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Provider {
    LmStudio,
    OpenAI,
    OpenRouter,
    Groq,
    Custom,
}

impl Provider {
    pub const ALL: [Provider; 5] = [
        Provider::LmStudio,
        Provider::OpenAI,
        Provider::OpenRouter,
        Provider::Groq,
        Provider::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::LmStudio => "LM Studio (local)",
            Provider::OpenAI => "OpenAI",
            Provider::OpenRouter => "OpenRouter",
            Provider::Groq => "Groq",
            Provider::Custom => "Custom (OpenAI-compatible)",
        }
    }

    /// The default base URL for the preset.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::LmStudio => "http://localhost:1234/v1",
            Provider::OpenAI => "https://api.openai.com/v1",
            Provider::OpenRouter => "https://openrouter.ai/api/v1",
            Provider::Groq => "https://api.groq.com/openai/v1",
            Provider::Custom => "",
        }
    }

    /// The default model for the preset.
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::LmStudio => "local-model",
            Provider::OpenAI => "gpt-4o-mini",
            Provider::OpenRouter => "openai/gpt-4o-mini",
            Provider::Groq => "llama-3.3-70b-versatile",
            Provider::Custom => "",
        }
    }
}

/// One message in an OpenAI-shaped conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    /// `system`, `user`, `assistant`, or `tool`.
    pub role: String,
    pub content: String,
    /// Present on assistant messages that request tool calls.
    pub tool_calls: Vec<ToolCallRequest>,
    /// Present on `tool` messages: the id of the call being answered.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_owned(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCallRequest>,
    ) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_owned(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn to_json(&self) -> Value {
        let mut message = json!({ "role": self.role, "content": self.content });
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = json!(
                self.tool_calls
                    .iter()
                    .map(|call| json!({
                        "id": call.id,
                        "type": "function",
                        "function": { "name": call.name, "arguments": call.arguments.to_string() }
                    }))
                    .collect::<Vec<_>>()
            );
        }
        if let Some(id) = &self.tool_call_id {
            message["tool_call_id"] = json!(id);
        }
        message
    }
}

/// A tool call the model asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    /// Parsed arguments object. Malformed JSON from the model becomes
    /// `{"_raw": "..."}` so the executor can report it.
    pub arguments: Value,
}

/// One round of the conversation: content plus any requested tool calls.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmTurn {
    pub content: String,
    pub tool_calls: Vec<ToolCallRequest>,
}

/// Build the `/chat/completions` request body.
pub fn build_chat_request(
    model: &str,
    temperature: f32,
    messages: &[ChatMessage],
    tools: &[Value],
) -> Value {
    let mut request = json!({
        "model": model,
        "temperature": temperature,
        "messages": messages.iter().map(ChatMessage::to_json).collect::<Vec<_>>(),
    });
    if !tools.is_empty() {
        request["tools"] = json!(tools);
        request["tool_choice"] = json!("auto");
    }
    request
}

/// Parse a `/chat/completions` response body into a turn.
pub fn parse_chat_response(body: &Value) -> Result<LlmTurn, String> {
    let error = |context: &str| format!("LLM response was malformed: {context}");
    let choices = body
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| error("missing choices"))?;
    let first = choices.first().ok_or_else(|| error("empty choices"))?;
    let message = first
        .get("message")
        .ok_or_else(|| error("missing message"))?;

    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| error("tool call missing id"))?
                .to_owned();
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| error("tool call missing function name"))?
                .to_owned();
            let raw_arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            let arguments = match raw_arguments {
                Value::String(text) => serde_json::from_str(&text).unwrap_or_else(
                    |_| json!({ "_raw": text, "_error": "arguments were not valid JSON" }),
                ),
                other @ Value::Object(_) => other,
                other => {
                    json!({ "_raw": other.to_string(), "_error": "arguments were not an object" })
                }
            };
            tool_calls.push(ToolCallRequest {
                id,
                name,
                arguments,
            });
        }
    }

    Ok(LlmTurn {
        content,
        tool_calls,
    })
}

/// Extract model ids from a `/models` response.
pub fn parse_models_response(body: &Value) -> Vec<String> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A long-timeout client for LLM calls (local models can be slow).
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("llm http client")
    })
}

fn auth_header(api_key: &str) -> Option<(&'static str, String)> {
    if api_key.trim().is_empty() {
        None
    } else {
        Some(("Authorization", format!("Bearer {}", api_key.trim())))
    }
}

async fn send_json(
    base_url: &str,
    api_key: &str,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("no API base URL configured".to_owned());
    }
    let url = format!("{base}{path}");

    let mut request = http_client().post(&url).json(&body);
    if let Some((name, value)) = auth_header(api_key) {
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .map_err(|error| format!("request to {url} failed: {error}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "the provider answered HTTP {status}: {}",
            truncate(&text, 400)
        ));
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("provider response was not valid JSON: {error}"))
}

async fn get_json(base_url: &str, api_key: &str, path: &str) -> Result<Value, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("no API base URL configured".to_owned());
    }
    let url = format!("{base}{path}");

    let mut request = http_client().get(&url);
    if let Some((name, value)) = auth_header(api_key) {
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .map_err(|error| format!("request to {url} failed: {error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "the provider answered HTTP {status}: {}",
            truncate(&text, 400)
        ));
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("provider response was not valid JSON: {error}"))
}

/// Run one chat-completions round.
pub async fn chat(base_url: &str, api_key: &str, request: Value) -> Result<Value, String> {
    send_json(base_url, api_key, "/chat/completions", request).await
}

/// List the models the provider exposes.
pub async fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>, String> {
    let body = get_json(base_url, api_key, "/models").await?;
    let models = parse_models_response(&body);
    if models.is_empty() {
        return Err("the provider returned no models".to_owned());
    }
    Ok(models)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_owned()
    } else {
        let mut cut = max;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &text[..cut])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_includes_tools_and_messages() {
        let messages = vec![
            ChatMessage::system("be nice"),
            ChatMessage::user("hello"),
            ChatMessage::assistant_with_tool_calls(
                String::new(),
                vec![ToolCallRequest {
                    id: "call_1".to_owned(),
                    name: "ban".to_owned(),
                    arguments: json!({ "user_id": "42" }),
                }],
            ),
            ChatMessage::tool_result("call_1", "denied"),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": { "name": "ban", "description": "b", "parameters": {} }
        })];
        let request = build_chat_request("m", 0.5, &messages, &tools);

        assert_eq!(request["model"], "m");
        assert_eq!(request["temperature"], 0.5);
        assert_eq!(request["messages"].as_array().unwrap().len(), 4);
        assert_eq!(
            request["messages"][2]["tool_calls"][0]["function"]["name"],
            "ban"
        );
        // Arguments travel as a JSON string, per the wire format.
        assert_eq!(
            request["messages"][2]["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"user_id":"42"}"#)
        );
        assert_eq!(request["messages"][3]["tool_call_id"], "call_1");
        assert_eq!(request["tools"].as_array().unwrap().len(), 1);
        assert_eq!(request["tool_choice"], "auto");
    }

    #[test]
    fn chat_response_parses_content() {
        let body = json!({
            "choices": [ { "message": { "role": "assistant", "content": "hi there" } } ]
        });
        let turn = parse_chat_response(&body).unwrap();
        assert_eq!(turn.content, "hi there");
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn chat_response_parses_tool_calls_with_string_arguments() {
        let body = json!({
            "choices": [ {
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [ {
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "kick", "arguments": "{\"user_id\": \"7\"}" }
                    } ]
                },
                "finish_reason": "tool_calls"
            } ]
        });
        let turn = parse_chat_response(&body).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_9");
        assert_eq!(turn.tool_calls[0].name, "kick");
        assert_eq!(turn.tool_calls[0].arguments["user_id"], "7");
    }

    #[test]
    fn malformed_tool_arguments_become_raw() {
        let body = json!({
            "choices": [ { "message": { "role": "assistant", "tool_calls": [ {
                "id": "c", "type": "function",
                "function": { "name": "x", "arguments": "not json" }
            } ] } } ]
        });
        let turn = parse_chat_response(&body).unwrap();
        assert_eq!(turn.tool_calls[0].arguments["_raw"], "not json");
    }

    #[test]
    fn models_response_parses() {
        let body = json!({ "data": [ { "id": "a" }, { "id": "b" }, { "nope": 1 } ] });
        assert_eq!(parse_models_response(&body), vec!["a", "b"]);
    }

    #[test]
    fn provider_presets() {
        assert_eq!(
            Provider::LmStudio.default_base_url(),
            "http://localhost:1234/v1"
        );
        assert_eq!(Provider::OpenAI.default_model(), "gpt-4o-mini");
    }
}
