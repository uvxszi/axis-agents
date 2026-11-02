//! Provider-neutral LLM client.
//!
//! Two wire protocols cover nearly every provider: the OpenAI
//! chat-completions shape (LM Studio, Ollama, OpenAI, OpenRouter, Groq,
//! Gemini's compatibility endpoint, Mistral, DeepSeek, xAI, Together, vLLM,
//! llama.cpp, …) and Anthropic's Messages API. Both stream, report usage,
//! surface reasoning text, accept images, and call tools.
//!
//! Errors are classified ([`LlmError`]) so the agent can react sensibly:
//! shrink the context on overflow, fall back to the text tool protocol
//! when a model rejects tools, drop images when it rejects vision.

pub mod anthropic;
pub mod openai;
pub mod sse;
pub mod text;

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    LmStudio,
    Ollama,
    OpenAI,
    Anthropic,
    Gemini,
    OpenRouter,
    Groq,
    Mistral,
    DeepSeek,
    XAi,
    Together,
    Custom,
}

/// The two wire protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStyle {
    OpenAi,
    Anthropic,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 12] = [
        ProviderKind::LmStudio,
        ProviderKind::Ollama,
        ProviderKind::OpenAI,
        ProviderKind::Anthropic,
        ProviderKind::Gemini,
        ProviderKind::OpenRouter,
        ProviderKind::Groq,
        ProviderKind::Mistral,
        ProviderKind::DeepSeek,
        ProviderKind::XAi,
        ProviderKind::Together,
        ProviderKind::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::LmStudio => "LM Studio (local)",
            ProviderKind::Ollama => "Ollama (local)",
            ProviderKind::OpenAI => "OpenAI",
            ProviderKind::Anthropic => "Anthropic (Claude)",
            ProviderKind::Gemini => "Google Gemini",
            ProviderKind::OpenRouter => "OpenRouter",
            ProviderKind::Groq => "Groq",
            ProviderKind::Mistral => "Mistral",
            ProviderKind::DeepSeek => "DeepSeek",
            ProviderKind::XAi => "xAI (Grok)",
            ProviderKind::Together => "Together AI",
            ProviderKind::Custom => "Custom (OpenAI-compatible)",
        }
    }

    pub fn api_style(self) -> ApiStyle {
        match self {
            ProviderKind::Anthropic => ApiStyle::Anthropic,
            _ => ApiStyle::OpenAi,
        }
    }

    pub fn needs_key(self) -> bool {
        !matches!(
            self,
            ProviderKind::LmStudio | ProviderKind::Ollama | ProviderKind::Custom
        )
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::LmStudio => "http://localhost:1234/v1",
            ProviderKind::Ollama => "http://localhost:11434/v1",
            ProviderKind::OpenAI => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com/v1",
            ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai",
            ProviderKind::OpenRouter => "https://openrouter.ai/api/v1",
            ProviderKind::Groq => "https://api.groq.com/openai/v1",
            ProviderKind::Mistral => "https://api.mistral.ai/v1",
            ProviderKind::DeepSeek => "https://api.deepseek.com/v1",
            ProviderKind::XAi => "https://api.x.ai/v1",
            ProviderKind::Together => "https://api.together.xyz/v1",
            ProviderKind::Custom => "",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            ProviderKind::LmStudio | ProviderKind::Custom => "",
            ProviderKind::Ollama => "llama3.1",
            ProviderKind::OpenAI => "gpt-4o-mini",
            ProviderKind::Anthropic => "claude-sonnet-5-5",
            ProviderKind::Gemini => "gemini-2.5-flash",
            ProviderKind::OpenRouter => "openai/gpt-4o-mini",
            ProviderKind::Groq => "llama-3.3-70b-versatile",
            ProviderKind::Mistral => "mistral-small-latest",
            ProviderKind::DeepSeek => "deepseek-chat",
            ProviderKind::XAi => "grok-3-mini",
            ProviderKind::Together => "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    /// An image by URL (`https://…` or a `data:` URI).
    Image(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub role: Role,
    pub parts: Vec<Part>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl Msg {
    fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![Part::Text(text.into())],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self::new(Role::System, text)
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, text)
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, text)
    }
    pub fn user_parts(parts: Vec<Part>) -> Self {
        Self {
            role: Role::User,
            parts,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn assistant_with_tools(text: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            parts: vec![Part::Text(text.into())],
            tool_calls: calls,
            tool_call_id: None,
        }
    }
    pub fn tool_result(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            parts: vec![Part::Text(text.into())],
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        }
    }

    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.as_str()),
                Part::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn has_images(&self) -> bool {
        self.parts.iter().any(|part| matches!(part, Part::Image(_)))
    }

    /// Characters the message costs, for token estimation. Images count as
    /// a flat ~800 tokens' worth.
    pub fn cost_chars(&self) -> usize {
        let parts: usize = self
            .parts
            .iter()
            .map(|part| match part {
                Part::Text(text) => text.len(),
                Part::Image(_) => 2800,
            })
            .sum();
        let calls: usize = self
            .tool_calls
            .iter()
            .map(|call| call.name.len() + call.arguments.to_string().len())
            .sum();
        parts + calls + 16
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Msg],
    pub tools: &'a [ToolSpec],
    pub temperature: f32,
    pub max_tokens: u32,
    pub stream: bool,
    /// Ask for a JSON object response (where supported).
    pub json: bool,
    /// `reasoning_effort` for thinking models ("none" turns thinking off
    /// in LM Studio); `None` leaves the model's default.
    pub reasoning: Option<&'a str>,
    /// LM Studio: cap on thinking tokens (`thinking_budget_tokens`), so a
    /// thinking model always leaves room for the answer. `reasoning:
    /// "none"` sends 0 even without this.
    pub thinking_budget: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatOutput {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

/// Incremental output while a response streams.
#[derive(Debug, Clone, Copy)]
pub enum StreamEvent<'a> {
    Content(&'a str),
    Reasoning(&'a str),
    ToolCall(&'a str),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LlmError {
    ContextOverflow(String),
    ToolsUnsupported(String),
    VisionUnsupported(String),
    RateLimited(String, Option<Duration>),
    Auth(String),
    Http(u16, String),
    Network(String),
    Malformed(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::ContextOverflow(m) => write!(f, "context window exceeded: {m}"),
            LlmError::ToolsUnsupported(m) => write!(f, "model does not support tools: {m}"),
            LlmError::VisionUnsupported(m) => write!(f, "model does not accept images: {m}"),
            LlmError::RateLimited(m, _) => write!(f, "rate limited: {m}"),
            LlmError::Auth(m) => write!(f, "authentication failed (check the API key): {m}"),
            LlmError::Http(status, m) => write!(f, "HTTP {status}: {m}"),
            LlmError::Network(m) => write!(f, "network error: {m}"),
            LlmError::Malformed(m) => write!(f, "unexpected response: {m}"),
        }
    }
}

impl LlmError {
    /// Classify a non-success HTTP response body.
    pub fn from_status(status: u16, body: &str, retry_after: Option<Duration>) -> Self {
        let lower = body.to_lowercase();
        let snippet = truncate(body, 400);
        if status == 401 || status == 403 {
            return LlmError::Auth(snippet);
        }
        if status == 429 && !lower.contains("context") {
            return LlmError::RateLimited(snippet, retry_after);
        }
        if is_context_overflow(&lower) {
            return LlmError::ContextOverflow(snippet);
        }
        if is_tools_unsupported(&lower) {
            return LlmError::ToolsUnsupported(snippet);
        }
        // Any 400 complaining about an image means "can't take images
        // this way" — the agent retries without them.
        if lower.contains("image")
            && (lower.contains("not support")
                || lower.contains("vision")
                || lower.contains("base64")
                || lower.contains("image_url")
                || lower.contains("multimodal")
                || status == 400)
        {
            return LlmError::VisionUnsupported(snippet);
        }
        LlmError::Http(status, snippet)
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, LlmError::RateLimited(..) | LlmError::Network(_))
            || matches!(self, LlmError::Http(status, _) if *status >= 500)
    }
}

fn is_context_overflow(lower: &str) -> bool {
    [
        "context length",
        "context_length",
        "maximum context",
        "context size",
        "context window",
        "too many tokens",
        "prompt is too long",
        "n_ctx",
        "reduce the length",
        "exceeds the maximum",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_tools_unsupported(lower: &str) -> bool {
    (lower.contains("tool") || lower.contains("function"))
        && (lower.contains("not support")
            || lower.contains("unsupported")
            || lower.contains("does not support")
            || lower.contains("unrecognized request argument")
            || lower.contains("not available"))
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

/// What the app knows about a model, detected from the provider.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    /// Usable tokens per request (for LM Studio: loaded context divided
    /// by its parallel slots).
    pub context_window: Option<u32>,
    pub vision: Option<bool>,
    pub tools: Option<bool>,
    /// LM Studio: the context the model was loaded with, and how many
    /// requests share it.
    pub loaded_context: Option<u32>,
    pub parallel: Option<u32>,
}

/// A configured endpoint.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub timeout: Duration,
}

impl Endpoint {
    pub fn new(kind: ProviderKind, base_url: &str, api_key: &str, timeout_secs: u64) -> Self {
        Self {
            kind,
            base_url: base_url.trim().trim_end_matches('/').to_owned(),
            api_key: api_key.trim().to_owned(),
            timeout: Duration::from_secs(timeout_secs.clamp(10, 900)),
        }
    }

    pub fn from_config(config: &crate::config::ModelConfig) -> Self {
        Self::new(
            config.provider,
            &config.base_url,
            &config.api_key,
            config.request_timeout_secs,
        )
    }

    /// Key for sharing concurrency limits between agents on one endpoint.
    pub fn limit_key(&self) -> String {
        self.base_url.to_lowercase()
    }

    pub async fn chat(
        &self,
        request: &ChatRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent<'_>) + Send),
    ) -> Result<ChatOutput, LlmError> {
        if self.base_url.is_empty() {
            return Err(LlmError::Malformed("no API base URL configured".to_owned()));
        }
        if request.model.trim().is_empty() {
            return Err(LlmError::Malformed(
                "no model selected — pick one on the agent's Model tab".to_owned(),
            ));
        }
        let output = match self.kind.api_style() {
            ApiStyle::OpenAi => openai::chat(self, request, on_event).await?,
            ApiStyle::Anthropic => anthropic::chat(self, request, on_event).await?,
        };
        Ok(output)
    }

    pub async fn list_models(&self) -> Result<Vec<String>, String> {
        match self.kind.api_style() {
            ApiStyle::OpenAi => openai::list_models(self).await,
            ApiStyle::Anthropic => anthropic::list_models(self).await,
        }
    }

    pub async fn model_info(&self, model: &str) -> ModelInfo {
        let detected = match self.kind.api_style() {
            ApiStyle::OpenAi => openai::model_info(self, model).await,
            ApiStyle::Anthropic => ModelInfo {
                context_window: Some(200_000),
                vision: Some(true),
                tools: Some(true),
                ..ModelInfo::default()
            },
        };
        let guessed = guess_model_info(model);
        ModelInfo {
            context_window: detected.context_window.or(guessed.context_window),
            vision: detected.vision.or(guessed.vision),
            tools: detected.tools.or(guessed.tools),
            loaded_context: detected.loaded_context,
            parallel: detected.parallel,
        }
    }

    pub async fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, String> {
        match self.kind.api_style() {
            ApiStyle::OpenAi => openai::embed(self, model, inputs).await,
            ApiStyle::Anthropic => Err("Anthropic has no embeddings endpoint".to_owned()),
        }
    }
}

/// Context windows and capabilities for well-known model families, used
/// when the provider does not report them.
pub fn guess_model_info(model: &str) -> ModelInfo {
    let m = model.to_lowercase();
    let context = if m.contains("claude") {
        Some(200_000)
    } else if m.contains("gemini") {
        Some(1_000_000)
    } else if m.contains("gpt-4.1") {
        Some(1_000_000)
    } else if m.contains("gpt-4o") || m.contains("gpt-5") || m.contains("o3") || m.contains("o4") {
        Some(128_000)
    } else if m.contains("llama-3") || m.contains("llama3") || m.contains("qwen") {
        Some(32_768)
    } else if m.contains("mistral") || m.contains("mixtral") {
        Some(32_768)
    } else if m.contains("deepseek") {
        Some(64_000)
    } else if m.contains("grok") {
        Some(131_072)
    } else {
        None
    };
    let vision = if m.contains("vision")
        || m.contains("-vl")
        || m.contains("llava")
        || m.contains("gpt-4o")
        || m.contains("claude")
        || m.contains("gemini")
        || m.contains("gemma-3")
        || m.contains("pixtral")
    {
        Some(true)
    } else {
        None
    };
    ModelInfo {
        context_window: context,
        vision,
        tools: None,
        ..ModelInfo::default()
    }
}

/// Provider quirks learned at runtime (e.g. "rejects `stream_options`"),
/// keyed by endpoint + model, so a quirk is retried around only once.
#[derive(Debug, Clone, Copy, Default)]
pub struct Quirks {
    pub no_stream_options: bool,
    pub no_temperature: bool,
    pub max_completion_tokens: bool,
    pub no_json_mode: bool,
    pub no_reasoning_effort: bool,
    pub no_thinking_budget: bool,
}

fn quirk_table() -> &'static Mutex<HashMap<String, Quirks>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Quirks>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn quirks(endpoint: &Endpoint, model: &str) -> Quirks {
    quirk_table()
        .lock()
        .expect("quirks poisoned")
        .get(&format!("{}|{model}", endpoint.base_url))
        .copied()
        .unwrap_or_default()
}

pub fn set_quirks(endpoint: &Endpoint, model: &str, quirks: Quirks) {
    quirk_table()
        .lock()
        .expect("quirks poisoned")
        .insert(format!("{}|{model}", endpoint.base_url), quirks);
}

pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .user_agent(concat!("AxisAgents/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("llm http client")
    })
}

pub(crate) fn network_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Network("the request timed out".to_owned())
    } else if error.is_connect() {
        LlmError::Network(format!(
            "could not connect ({error}) — is the server running?"
        ))
    } else {
        LlmError::Network(error.to_string())
    }
}

pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(|secs| Duration::from_secs_f64(secs.min(120.0)))
}

/// Parse tool arguments that arrive as a JSON string (OpenAI) or object.
pub(crate) fn parse_arguments(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| {
        // Small models sometimes emit trailing junk after the object.
        if let Some(end) = trimmed.rfind('}') {
            if let Ok(value) = serde_json::from_str(&trimmed[..=end]) {
                return value;
            }
        }
        serde_json::json!({ "_malformed": trimmed })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_classified() {
        assert!(matches!(
            LlmError::from_status(
                400,
                "This model's maximum context length is 8192 tokens",
                None
            ),
            LlmError::ContextOverflow(_)
        ));
        assert!(matches!(
            LlmError::from_status(400, "model does not support tools", None),
            LlmError::ToolsUnsupported(_)
        ));
        assert!(matches!(
            LlmError::from_status(401, "bad key", None),
            LlmError::Auth(_)
        ));
        assert!(matches!(
            LlmError::from_status(429, "slow down", None),
            LlmError::RateLimited(..)
        ));
        assert!(LlmError::from_status(503, "overloaded", None).is_retryable());
    }

    #[test]
    fn arguments_tolerate_junk() {
        assert_eq!(parse_arguments(r#"{"a":1}"#)["a"], 1);
        assert_eq!(parse_arguments(r#"{"a":1} trailing"#)["a"], 1);
        assert!(parse_arguments("").is_object());
        assert!(parse_arguments("nope").get("_malformed").is_some());
    }

    #[test]
    fn model_guesses() {
        assert_eq!(
            guess_model_info("claude-sonnet-5-5").context_window,
            Some(200_000)
        );
        assert_eq!(guess_model_info("qwen2.5-vl-7b").vision, Some(true));
        assert_eq!(guess_model_info("mystery").context_window, None);
    }
}
