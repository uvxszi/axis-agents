//! A Model Context Protocol client: connects to MCP servers (local
//! processes over stdio, or remote Streamable HTTP endpoints), lists their
//! tools, and calls them on the agents' behalf.
//!
//! MCP tools can do anything the server can — read files, run commands —
//! so each server has an [`AccessLevel`]: only people at or above it can
//! cause its tools to run.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::oneshot,
};

use crate::config::{AccessLevel, McpServerConfig, McpTransport};
use crate::runtime::live::{Live, McpStatus};

const PROTOCOL_VERSION: &str = "2025-06-18";
const CALL_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone)]
pub struct McpTool {
    pub server_id: String,
    pub server_name: String,
    pub tool_name: String,
    pub exposed_name: String,
    pub description: String,
    pub schema: Value,
    pub access: AccessLevel,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

enum Transport {
    Stdio {
        _child: tokio::process::Child,
        stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
        pending: Pending,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
        session: Mutex<Option<String>>,
    },
}

struct Connection {
    transport: Transport,
    next_id: AtomicU64,
}

impl Connection {
    async fn start(config: &McpServerConfig) -> Result<Self, String> {
        let transport = match config.transport {
            McpTransport::Stdio => {
                if config.command.trim().is_empty() {
                    return Err("no command configured".to_owned());
                }
                let mut command =
                    if cfg!(windows) && !config.command.to_lowercase().ends_with(".exe") {
                        // `npx`, `uvx`, and friends are .cmd shims on Windows.
                        let mut command = tokio::process::Command::new("cmd");
                        command.arg("/C").arg(&config.command);
                        command
                    } else {
                        tokio::process::Command::new(&config.command)
                    };
                command
                    .args(&config.args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                for (key, value) in &config.env {
                    if !key.trim().is_empty() {
                        command.env(key.trim(), value);
                    }
                }
                #[cfg(windows)]
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                let mut child = command
                    .spawn()
                    .map_err(|error| format!("could not start `{}`: {error}", config.command))?;
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = child.stdout.take().ok_or("no stdout")?;
                let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
                let reader_pending = Arc::clone(&pending);
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(message) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let Some(id) = message.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        if message.get("method").is_some() {
                            continue; // a server→client request; not supported
                        }
                        let sender = reader_pending.lock().expect("mcp poisoned").remove(&id);
                        if let Some(sender) = sender {
                            let _ = sender.send(response_result(&message));
                        }
                    }
                    // Process ended: fail everything still waiting.
                    for (_, sender) in reader_pending.lock().expect("mcp poisoned").drain() {
                        let _ = sender.send(Err("the MCP server exited".to_owned()));
                    }
                });
                Transport::Stdio {
                    _child: child,
                    stdin: tokio::sync::Mutex::new(stdin),
                    pending,
                }
            }
            McpTransport::Http => {
                if config.url.trim().is_empty() {
                    return Err("no URL configured".to_owned());
                }
                Transport::Http {
                    url: config.url.trim().to_owned(),
                    headers: config.headers.clone(),
                    session: Mutex::new(None),
                }
            }
        };
        let connection = Self {
            transport,
            next_id: AtomicU64::new(1),
        };
        connection
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "axis-agents", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        connection
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(connection)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut stdin = stdin.lock().await;
                let line = format!("{message}\n");
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                stdin.flush().await.map_err(|e| e.to_string())
            }
            Transport::Http { .. } => self.http_post(&message, None).await.map(|_| ()),
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        match &self.transport {
            Transport::Stdio { stdin, pending, .. } => {
                let (sender, receiver) = oneshot::channel();
                pending.lock().expect("mcp poisoned").insert(id, sender);
                {
                    let mut stdin = stdin.lock().await;
                    let line = format!("{message}\n");
                    stdin
                        .write_all(line.as_bytes())
                        .await
                        .map_err(|e| format!("write failed: {e}"))?;
                    stdin.flush().await.map_err(|e| e.to_string())?;
                }
                match tokio::time::timeout(CALL_TIMEOUT, receiver).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err("the MCP server closed the request".to_owned()),
                    Err(_) => {
                        pending.lock().expect("mcp poisoned").remove(&id);
                        Err(format!("{method} timed out"))
                    }
                }
            }
            Transport::Http { .. } => {
                let reply = self.http_post(&message, Some(id)).await?;
                response_result(&reply.ok_or("empty response")?)
            }
        }
    }

    async fn http_post(&self, message: &Value, id: Option<u64>) -> Result<Option<Value>, String> {
        let Transport::Http {
            url,
            headers,
            session,
        } = &self.transport
        else {
            unreachable!("http_post on stdio transport");
        };
        let mut request = crate::llm::http_client()
            .post(url)
            .timeout(CALL_TIMEOUT)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(message);
        for (key, value) in headers {
            if !key.trim().is_empty() {
                request = request.header(key.trim(), value);
            }
        }
        if let Some(id) = session.lock().expect("mcp poisoned").clone() {
            request = request.header("Mcp-Session-Id", id);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if let Some(value) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session.lock().expect("mcp poisoned") = Some(value.to_owned());
        }
        let status = response.status();
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
        let body = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                crate::llm::truncate(&body, 300)
            ));
        }
        if id.is_none() || body.trim().is_empty() {
            return Ok(None);
        }
        if is_sse {
            let mut parser = crate::llm::sse::SseParser::default();
            let mut events = parser.push(body.as_bytes());
            events.extend(parser.finish());
            for data in events {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    if value.get("id").and_then(Value::as_u64) == id {
                        return Ok(Some(value));
                    }
                }
            }
            Err("no response in the event stream".to_owned())
        } else {
            serde_json::from_str(&body)
                .map(Some)
                .map_err(|e| format!("invalid JSON: {e}"))
        }
    }

    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _page in 0..20 {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            if let Some(list) = result.get("tools").and_then(Value::as_array) {
                tools.extend(list.iter().cloned());
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }
}

fn response_result(message: &Value) -> Result<Value, String> {
    if let Some(error) = message.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP error")
            .to_owned());
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}

pub fn exposed_name(server: &str, tool: &str) -> String {
    let mut name = format!("mcp_{}_{}", sanitize(server), sanitize(tool));
    name.truncate(64);
    name
}

/// Render a `tools/call` result as text for the model.
pub fn render_result(result: &Value) -> (String, bool) {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    item.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ),
                Some("image") => parts.push("[image content]".to_owned()),
                Some("audio") => parts.push("[audio content]".to_owned()),
                Some("resource") => {
                    let text = item
                        .pointer("/resource/text")
                        .and_then(Value::as_str)
                        .or_else(|| item.pointer("/resource/uri").and_then(Value::as_str))
                        .unwrap_or("[resource]");
                    parts.push(text.to_owned());
                }
                Some("resource_link") => parts.push(format!(
                    "[link: {}]",
                    item.get("uri").and_then(Value::as_str).unwrap_or("?")
                )),
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            parts.push(structured.to_string());
        }
    }
    (parts.join("\n"), is_error)
}

#[derive(Default)]
pub struct McpManager {
    connections: tokio::sync::Mutex<HashMap<String, Arc<Connection>>>,
    tools: RwLock<Vec<McpTool>>,
    configs: RwLock<Vec<McpServerConfig>>,
}

impl McpManager {
    pub fn tools(&self) -> Vec<McpTool> {
        self.tools.read().expect("mcp poisoned").clone()
    }

    /// Connect enabled servers, drop removed/disabled ones, refresh tools.
    pub async fn sync(&self, configs: Vec<McpServerConfig>, live: &Live) {
        let previous = self.configs.read().expect("mcp poisoned").clone();
        *self.configs.write().expect("mcp poisoned") = configs.clone();
        let mut connections = self.connections.lock().await;
        // Drop servers that went away or whose settings changed.
        connections.retain(|id, _| {
            let now = configs.iter().find(|c| &c.id == id && c.enabled);
            let before = previous.iter().find(|c| &c.id == id);
            matches!((now, before), (Some(now), Some(before)) if now == before)
        });
        let mut all_tools = Vec::new();
        for config in configs.iter().filter(|c| c.enabled) {
            let connection = match connections.get(&config.id) {
                Some(connection) => Arc::clone(connection),
                None => match Connection::start(config).await {
                    Ok(connection) => {
                        let connection = Arc::new(connection);
                        connections.insert(config.id.clone(), Arc::clone(&connection));
                        connection
                    }
                    Err(error) => {
                        live.mcp.lock().expect("live poisoned").insert(
                            config.id.clone(),
                            McpStatus {
                                connected: false,
                                tools: Vec::new(),
                                error: Some(error),
                            },
                        );
                        continue;
                    }
                },
            };
            match connection.list_tools().await {
                Ok(tools) => {
                    let names: Vec<String> = tools
                        .iter()
                        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
                        .collect();
                    for tool in &tools {
                        let Some(name) = tool.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let mut schema = tool
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                        if schema.get("type").is_none() {
                            schema["type"] = json!("object");
                        }
                        all_tools.push(McpTool {
                            server_id: config.id.clone(),
                            server_name: config.name.clone(),
                            tool_name: name.to_owned(),
                            exposed_name: exposed_name(&config.name, name),
                            description: format!(
                                "[{} via MCP] {}",
                                config.name,
                                tool.get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                            ),
                            schema,
                            access: config.access,
                        });
                    }
                    live.mcp.lock().expect("live poisoned").insert(
                        config.id.clone(),
                        McpStatus {
                            connected: true,
                            tools: names,
                            error: None,
                        },
                    );
                }
                Err(error) => {
                    connections.remove(&config.id);
                    live.mcp.lock().expect("live poisoned").insert(
                        config.id.clone(),
                        McpStatus {
                            connected: false,
                            tools: Vec::new(),
                            error: Some(error),
                        },
                    );
                }
            }
        }
        *self.tools.write().expect("mcp poisoned") = all_tools;
        live.repaint();
    }

    pub async fn call(&self, exposed: &str, arguments: Value) -> Result<String, String> {
        let tool = self
            .tools()
            .into_iter()
            .find(|t| t.exposed_name == exposed)
            .ok_or_else(|| format!("unknown MCP tool {exposed}"))?;
        let connection = {
            let connections = self.connections.lock().await;
            connections
                .get(&tool.server_id)
                .cloned()
                .ok_or("that MCP server is not connected")?
        };
        let result = connection
            .request(
                "tools/call",
                json!({ "name": tool.tool_name, "arguments": arguments }),
            )
            .await?;
        let (text, is_error) = render_result(&result);
        if is_error { Err(text) } else { Ok(text) }
    }

    pub async fn shutdown(&self) {
        self.connections.lock().await.clear();
        self.tools.write().expect("mcp poisoned").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_names_are_safe() {
        assert_eq!(
            exposed_name("File System", "read-file"),
            "mcp_file_system_read_file"
        );
        assert!(exposed_name(&"x".repeat(80), "y").len() <= 64);
    }

    #[test]
    fn results_render() {
        let (text, error) = render_result(&json!({
            "content": [{ "type": "text", "text": "hello" }, { "type": "image", "data": "..." }],
            "isError": false
        }));
        assert_eq!(text, "hello\n[image content]");
        assert!(!error);
        let (_, error) = render_result(&json!({ "content": [], "isError": true }));
        assert!(error);
    }
}
