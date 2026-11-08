//! Real-model probe: drives the REAL pipeline against the REAL model in
//! LM Studio (only Discord is mocked), then prints everything the model
//! was shown and everything it said — so behavior is observed, not
//! guessed.
//!
//!     cargo run --release --bin probe [-- model-id]

use std::{
    net::TcpListener,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use serde_json::{Value, json};

use axis_agents::config::{AgentProfile, GlobalSettings, ModelConfig};
use axis_agents::memory::{MemoryStore, StoredMessage};
use axis_agents::runtime::agent::AgentCore;
use axis_agents::runtime::brain::{self, Job, Outlet, Reason};
use axis_agents::runtime::hive::Hive;
use axis_agents::runtime::live::{FeedKind, Live};
use axis_agents::runtime::world::MemberInfo;

// --- Mock Discord (records everything) ----------------------------------------------

static REST_LOG: OnceLock<Mutex<Vec<(String, String, String)>>> = OnceLock::new();
static LLM_LOG: OnceLock<Mutex<Vec<(Value, Value)>>> = OnceLock::new();

fn rest_log() -> &'static Mutex<Vec<(String, String, String)>> {
    REST_LOG.get_or_init(|| Mutex::new(Vec::new()))
}

fn llm_log() -> &'static Mutex<Vec<(Value, Value)>> {
    LLM_LOG.get_or_init(|| Mutex::new(Vec::new()))
}

fn serve(listener: TcpListener, handler: fn(&str, &str, &[u8]) -> (u16, String)) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut stream, &mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(split) = find_header_end(&buf) {
                            let body_len = content_length(&buf[..split]);
                            if buf.len() >= split + 4 + body_len {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let (method, path) = request_line(&text);
            let split = find_header_end(&buf).unwrap_or(buf.len());
            let body: Vec<u8> = buf.get(split + 4..).map(<[u8]>::to_vec).unwrap_or_default();
            let (status, response_body) = handler(&method, &path, &body);
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> usize {
    let text = String::from_utf8_lossy(headers).to_lowercase();
    text.lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

fn request_line(text: &str) -> (String, String) {
    let mut parts = text.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    (method, path)
}

fn message_json(id: u64, channel: u64, content: &str) -> String {
    json!({
        "id": id.to_string(), "channel_id": channel.to_string(),
        "author": { "id": "7", "username": "Magi", "discriminator": "0" },
        "content": content, "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0
    })
    .to_string()
}

/// A logging pass-through to the real LM Studio: records every
/// request/response pair, then forwards.
fn llm_proxy(method: &str, path: &str, body: &[u8]) -> Option<(u16, String)> {
    if !path.contains("chat/completions") {
        return None; // /models etc: let the caller handle or forward plainly
    }
    let request: Value = serde_json::from_slice(body).ok()?;
    let response_text = forward_to_lm_studio(method, path, body);
    let response: Value = serde_json::from_str(&response_text).unwrap_or(Value::Null);
    llm_log().lock().unwrap().push((request, response.clone()));
    Some((200, response.to_string()))
}

fn forward_to_lm_studio(method: &str, path: &str, body: &[u8]) -> String {
    // reqwest handles chunked encoding and headers correctly; a hand
    // rolled TCP forward does not.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("proxy client");
    let url = format!("http://127.0.0.1:1234{path}");
    let request = match method {
        "POST" => client.post(&url),
        "GET" => client.get(&url),
        _ => client.request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            &url,
        ),
    };
    let response = request
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .expect("LM Studio reachable");
    response.text().unwrap_or_default()
}

fn mock_discord(method: &str, path: &str, body: &[u8]) -> (u16, String) {
    let snippet: String = String::from_utf8_lossy(body).chars().take(300).collect();
    rest_log()
        .lock()
        .unwrap()
        .push((method.to_owned(), path.to_owned(), snippet));
    let query = path
        .split_once('?')
        .map(|(_, q)| q.to_owned())
        .unwrap_or_default();
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["gateway"]) => (
            200,
            json!({ "url": "wss://gateway.discord.gg" }).to_string(),
        ),
        ("GET", ["channels", _, "messages"]) => {
            // What read_messages sees: the seeded history, newest first
            // like Discord, honoring `before` and `limit`.
            let limit: usize = query
                .split('&')
                .find_map(|p| p.strip_prefix("limit="))
                .and_then(|v| v.parse().ok())
                .unwrap_or(50);
            let before: Option<u64> = query
                .split('&')
                .find_map(|p| p.strip_prefix("before="))
                .and_then(|v| v.parse().ok());
            let mut list: Vec<Value> = mock_history()
                .lock()
                .unwrap()
                .iter()
                .filter(|m| before.is_none_or(|b| m.id < b))
                .rev()
                .map(|m| {
                    json!({
                        "id": m.id.to_string(), "channel_id": GENERAL.to_string(),
                        "author": { "id": m.author_id.to_string(), "username": m.author, "discriminator": "0" },
                        "content": m.content, "timestamp": "2026-01-01T00:00:00.000000+00:00",
                        "tts": false, "mention_everyone": false, "mentions": [],
                        "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0
                    })
                })
                .take(limit)
                .collect();
            // Discord order: newest first — read_messages reverses itself.
            (200, json!(list).to_string())
        }
        ("POST", ["channels", _, "messages"]) => (200, message_json(999, 100, "ok")),
        ("PATCH", ["channels", _, "messages", _]) | ("DELETE", ["channels", _, "messages", _]) => {
            (200, message_json(999, 100, "ok"))
        }
        ("PUT", _) | ("DELETE", _) | ("PATCH", _) => (204, String::new()),
        _ => (200, json!({}).to_string()),
    }
}

/// The message history the mock serves — the same messages the probe
/// archives into the hive, so read_messages and the transcript agree.
fn mock_history() -> &'static Mutex<Vec<StoredMessage>> {
    static HISTORY: OnceLock<Mutex<Vec<StoredMessage>>> = OnceLock::new();
    HISTORY.get_or_init(|| Mutex::new(Vec::new()))
}

// --- World + core --------------------------------------------------------------------

const GUILD: u64 = 50;
const GENERAL: u64 = 100;
const MEDIA: u64 = 101;
const VOICE: u64 = 102;
const ADMIN: u64 = 1;
const BOT: u64 = 7;
const PLEB: u64 = 2;

fn member(id: u64, name: &str, roles: Vec<u64>) -> MemberInfo {
    MemberInfo {
        user_id: id,
        username: name.to_owned(),
        global_name: None,
        nick: None,
        roles,
        bot: false,
        joined_at: None,
        timeout_until: None,
        avatar: None,
        guild_avatar: None,
    }
}

fn build(discord_base: String, llm_base: String, model: String) -> Arc<AgentCore> {
    let live = Arc::new(Live::default());
    let hive = Hive::new(live, GlobalSettings::default());
    let model_config = ModelConfig {
        base_url: llm_base,
        model,
        streaming: false,
        ..ModelConfig::default()
    };
    let profile = AgentProfile {
        id: "probe".to_owned(),
        name: "Magi".to_owned(),
        persona: "You are Magi, a helpful, professional Discord agent.".to_owned(),
        model: model_config,
        ..AgentProfile::default()
    };
    let http = Arc::new(
        axis::prelude::Http::builder(axis::prelude::Token::new("probe-token".to_owned()))
            .api_base(discord_base)
            .build()
            .expect("http builds"),
    );
    let memory = Arc::new(MemoryStore::load(
        axis_agents::paths::data_dir().join("probe-memory.json"),
    ));
    let core = AgentCore::new(profile, hive, memory, http, true);
    seed(&core);
    core
}

fn seed(core: &Arc<AgentCore>) {
    let guild_json = json!({
        "id": GUILD.to_string(), "name": "Probe Server", "owner_id": ADMIN.to_string(), "afk_timeout": 300,
        "verification_level": 0, "default_message_notifications": 0, "explicit_content_filter": 0,
        "roles": [
            { "id": GUILD.to_string(), "name": "@everyone", "color": 0, "colors": { "primary_color": 0 }, "hoist": false, "position": 0, "permissions": "0", "managed": false, "mentionable": false },
            { "id": "60", "name": "Admin", "color": 0, "colors": { "primary_color": 0 }, "hoist": false, "position": 1, "permissions": "8", "managed": false, "mentionable": true },
            { "id": "61", "name": "BotRole", "color": 0, "colors": { "primary_color": 0 }, "hoist": true, "position": 2, "permissions": "134217780", "managed": false, "mentionable": true }
        ],
        "emojis": [], "features": [], "mfa_level": 0, "premium_tier": 0,
        "preferred_locale": "en-US", "nsfw_level": 0, "premium_progress_bar_enabled": false
    })
    .to_string();
    let guild: axis::model::Guild = serde_json::from_str(&guild_json).unwrap();
    core.world.insert_guild(&guild, |state| {
        for (id, name, kind) in [
            (GENERAL, "general", 0u8),
            (MEDIA, "media", 0),
            (VOICE, "General Voice", 2),
        ] {
            state.channels.insert(
                id,
                serde_json::from_value::<axis::model::Channel>(json!({
                    "id": id.to_string(), "type": kind, "name": name,
                    "guild_id": GUILD.to_string()
                }))
                .unwrap(),
            );
        }
        // The bot outranks everyone so moderation tools pass hierarchy.
        state.members.insert(BOT, member(BOT, "Magi", vec![61]));
        state
            .members
            .insert(ADMIN, member(ADMIN, "simadmin", vec![60]));
        state
            .members
            .insert(PLEB, member(PLEB, "simmember", vec![]));
    });
    core.world.set_me(
        serde_json::from_value(json!({
            "id": BOT.to_string(), "username": "Magi", "discriminator": "0"
        }))
        .unwrap(),
    );
}

fn trigger_message(question: &str) -> axis::model::Message {
    serde_json::from_value(json!({
        "id": "800", "channel_id": GENERAL.to_string(),
        "author": { "id": ADMIN.to_string(), "username": "simadmin", "discriminator": "0" },
        "content": question, "guild_id": GUILD.to_string(),
        "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "type": 0
    }))
    .unwrap()
}

fn user(id: u64) -> axis::model::User {
    serde_json::from_value(json!({
        "id": id.to_string(), "username": format!("u{id}"), "discriminator": "0"
    }))
    .unwrap()
}

struct RunResult {
    answer: String,
    rest: Vec<(String, String, String)>,
}

async fn run_question(core: &Arc<AgentCore>, question: &str) -> RunResult {
    rest_log().lock().unwrap().clear();
    llm_log().lock().unwrap().clear();
    let job = Job {
        guild_id: Some(GUILD),
        channel_id: GENERAL,
        triggers: vec![trigger_message(question)],
        reason: Reason::Direct,
        requester: core.principal_for(Some(GUILD), &user(ADMIN)),
        task: None,
        outlet: Outlet::Channel,
        question: None,
    };
    let started = Instant::now();
    // The gateway archives every message before the brain runs; the probe
    // must too or the question never reaches the transcript.
    // Distinct id per scenario (the mock serves them; `before=800` pages
    // under the first trigger).
    let id = 800
        + mock_history()
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.author_id == ADMIN)
            .count() as u64;
    let trigger = StoredMessage {
        id,
        author_id: ADMIN,
        author: "simadmin".to_owned(),
        author_bot: false,
        content: question.to_owned(),
        ts: axis_agents::memory::now_ms(),
        reply_to: None,
        reply_author: None,
        reply_snippet: None,
        attachments: vec![],
        deleted: false,
        edited: false,
        reactions: vec![],
    };
    core.hive
        .knowledge
        .channel_mut(GENERAL, |archive| archive.push(trigger.clone()));
    mock_history().lock().unwrap().push(trigger);
    brain::run(core, job).await;
    eprintln!("  ({:.1}s)", started.elapsed().as_secs_f32());
    let feed = core.live().feed_snapshot(Some(&core.id), 30);
    let answer = feed
        .iter()
        .find(|item| matches!(item.kind, FeedKind::Reply))
        .map(|item| item.text.clone())
        .unwrap_or_else(|| "(no reply)".to_owned());
    RunResult {
        answer,
        rest: rest_log().lock().unwrap().clone(),
    }
}

fn print_result(result: &RunResult) {
    println!(
        "  ANSWER: {}",
        axis_agents::llm::truncate(&result.answer, 400)
    );
    println!("  REST calls:");
    for (method, path, body) in result.rest.iter().take(25) {
        let shown: String = body.chars().take(120).collect();
        let extra = if shown.is_empty() {
            String::new()
        } else {
            format!(" | {shown}")
        };
        println!("    {method} {path}{extra}");
    }
}

/// Status chrome the model must never see in a prompt: the thinking
/// emote, small-text prefix, step marks.
const STATUS_NEEDLES: [&str; 5] = ["💭", "-# ", "✅ ", "⚠️ ", "⏳ "];

/// Any of the bot's own status lines that leaked into prompt text.
fn status_lines_in_prompts() -> Vec<String> {
    let log = llm_log().lock().unwrap().clone();
    let mut hits: Vec<String> = Vec::new();
    for (request, _) in &log {
        let Some(messages) = request.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for message in messages {
            let Some(content) = message.get("content").and_then(Value::as_str) else {
                continue;
            };
            for line in content.lines() {
                if let Some(hit) = STATUS_NEEDLES.iter().find(|n| line.contains(**n)) {
                    let found = format!(
                        "{hit:?} in: {}",
                        axis_agents::llm::truncate(line.trim(), 90)
                    );
                    if !hits.contains(&found) {
                        hits.push(found);
                    }
                }
            }
        }
    }
    hits
}

fn print_llm_log(label: &str) {
    let log = llm_log().lock().unwrap().clone();
    println!("  -- LLM rounds for {label}: {}", log.len());
    for (round, (request, response)) in log.iter().take(6).enumerate() {
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        println!("    round {}: {} messages in", round + 1, messages.len());
        if round == 0 {
            if let Some(system) = messages.first() {
                let text = system.get("content").and_then(Value::as_str).unwrap_or("");
                println!(
                    "      SYSTEM ({} chars): {}",
                    text.chars().count(),
                    axis_agents::llm::truncate(text, 700)
                );
            }
        }
        for message in messages.iter().skip(1) {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
            match role {
                "assistant" => {
                    let content = message.get("content").and_then(Value::as_str).unwrap_or("");
                    let calls = message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .map(|c| c.len())
                        .unwrap_or(0);
                    if !content.is_empty() {
                        println!(
                            "      > assistant: {}",
                            axis_agents::llm::truncate(content, 200)
                        );
                    }
                    if calls > 0 {
                        println!("      > assistant called {calls} tool(s)");
                    }
                }
                "tool" => {
                    let content = message.get("content").and_then(Value::as_str).unwrap_or("");
                    println!("      < tool: {}", axis_agents::llm::truncate(content, 200));
                }
                "user" => {
                    let content = message.get("content").and_then(Value::as_str).unwrap_or("");
                    println!("      ? user: {}", axis_agents::llm::truncate(content, 300));
                }
                _ => {}
            }
        }
        let choice = response
            .get("choices")
            .and_then(|c| c.as_array().and_then(|a| a.first()))
            .cloned()
            .unwrap_or_default();
        let final_text = choice
            .pointer("/message/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        println!(
            "      <= model: {}",
            axis_agents::llm::truncate(final_text, 300)
        );
    }
}

fn main() {
    let scratch = std::env::temp_dir().join(format!("axis-probe-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    unsafe { std::env::set_var("AXIS_AGENTS_HOME", &scratch) };

    let model = std::env::args().nth(1).unwrap_or_else(|| {
        "qwen3.5-9b-the-defiant-fable-uncensored-heretic-neo-imatrix-max-mtp@q4_k_m".to_owned()
    });
    // Optional: run only some scenarios by number ("probe <model> 7,8").
    let only: Vec<u8> = std::env::args()
        .nth(2)
        .map(|list| {
            list.split([',', ' '])
                .filter_map(|n| n.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    println!(
        "Axis probe -- REAL model {model} (only Discord is mocked){}\n",
        if only.is_empty() {
            String::new()
        } else {
            format!(" -- scenarios {only:?}")
        }
    );

    let discord = TcpListener::bind("127.0.0.1:0").unwrap();
    let discord_base = format!("http://127.0.0.1:{}", discord.local_addr().unwrap().port());
    serve(discord, mock_discord);
    let llm_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let llm_base = format!(
        "http://127.0.0.1:{}/v1",
        llm_listener.local_addr().unwrap().port()
    );
    serve(llm_listener, |method, path, body| {
        llm_proxy(method, path, body)
            .unwrap_or_else(|| (200, forward_to_lm_studio(method, path, body)))
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("probe runtime")
        .block_on(probe_main(&scratch, discord_base, llm_base, &model, &only));
}

async fn probe_main(
    scratch: &std::path::Path,
    discord_base: String,
    llm_base: String,
    model: &str,
    only: &[u8],
) {
    let core = build(discord_base, llm_base, model.to_owned());

    // Conversation history in #general so "read above" has something to
    // find, spread over the last few minutes.
    core.hive.knowledge.channel_mut(GENERAL, |archive| {
        let lines = [
            "just watched the new dune movie, the sandworm scenes were incredible",
            "anyone else think the pacing dragged in act two?",
            "I liked it more than part one honestly",
            "what does everyone think about the movie?",
        ];
        for (i, text) in lines.iter().enumerate() {
            let seeded = StoredMessage {
                id: 300 + i as u64,
                author_id: PLEB,
                author: "simmember".to_owned(),
                author_bot: false,
                content: (*text).to_owned(),
                ts: axis_agents::memory::now_ms() - (4 - i as i64) * 60_000,
                reply_to: None,
                reply_author: None,
                reply_snippet: None,
                attachments: vec![],
                deleted: false,
                edited: false,
                reactions: vec![],
            };
            archive.push(seeded.clone());
            mock_history().lock().unwrap().push(seeded);
        }
    });

    let mut leak_hits: Vec<String> = Vec::new();
    let mut scenarios: Vec<(&str, &str)> = vec![
        (
            "1. read-above",
            "read what I said above and tell me what you think",
        ),
        (
            "2. channel-target",
            "send a message in the media channel saying the new gallery is up",
        ),
        (
            "3. attach",
            "write a file called notes.txt with three lines about chess, then attach it",
        ),
        (
            "4. long-form",
            // 60 KB, not 20 MB: the probe must verify the mechanism (sections
            // until the target, private summary, no early finish) in minutes;
            // a 20 MB target is thousands of model rounds.
            "write a 60 KB file with a long adventure story about you and another bot named Vex",
        ),
        (
            "5. link",
            "find my message about the movie above and link it",
        ),
        (
            "6. quiet-misfire",
            "this channel is getting noisy, settle down a bit",
        ),
        (
            // The copy-paste flow: write once, duplicate in code. "1 MB" in
            // the message is the requested target — even if the model
            // forgets until_bytes, the app knows the ask. Must be FAST.
            "7. copy-paste",
            "write a one-paragraph story into tale.md then copy-paste it until the file is 1 MB",
        ),
        (
            // "I'll make it" and then nothing: the follow-through nudge
            // must bounce the promise and make the tool run.
            "8. promise",
            "make a 500 KB file called notes2.md full of chess tips",
        ),
    ];
    if !only.is_empty() {
        scenarios.retain(|(label, _)| only.iter().any(|n| label.starts_with(&format!("{n}."))));
    }

    for (label, question) in scenarios {
        println!("== {label}: {question}");
        let result = run_question(&core, question).await;
        print_result(&result);
        print_llm_log(label);
        leak_hits.extend(status_lines_in_prompts());
        // For the file scenarios: what did the file actually turn out to
        // be? Real continuous prose, repeat-padded filler, or the exact
        // size asked for?
        if label.starts_with("4.") || label.starts_with("7.") || label.starts_with("8.") {
            let dir = axis_agents::tools::files::sandbox_dir_pub(GENERAL);
            let biggest = std::fs::read_dir(&dir)
                .ok()
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| e.path().is_file())
                        .filter_map(|e| {
                            let len = e.metadata().ok()?.len() as usize;
                            Some((e.path(), len))
                        })
                        .max_by_key(|(_, len)| *len)
                })
                .flatten();
            if let Some((path, len)) = biggest {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let head: String = text.chars().take(150).collect();
                let tail: String = text
                    .chars()
                    .rev()
                    .take(150)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                // A repeat-padded file repeats its opening chunk many times.
                let probe_chunk: String = text.chars().skip(200).take(100).collect();
                let repeats = if probe_chunk.trim().is_empty() {
                    0
                } else {
                    text.matches(&probe_chunk).count()
                };
                println!(
                    "  FILE {}: {} — {} distinct-paragraphs? head: {:?}… tail: {:?} (opening-chunk repeats: {})",
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    axis_agents::tools::files::size_label(len),
                    text.split("\n\n").count(),
                    head,
                    tail,
                    repeats
                );
            }
        }
        println!();
        axis_agents::tools::longform::discard(GENERAL);
    }

    // The emote-leak check: did any PROMPT TEXT the model saw contain its
    // own status chrome? Only message contents are checked — the request
    // body also carries API parameter names like `thinking_budget_tokens`,
    // which are not text the model reads. Hits accumulate across every
    // scenario (run_question clears the log each time).
    println!(
        "== emote-leak check: {}",
        if leak_hits.is_empty() {
            "clean (status text never in a prompt)".to_owned()
        } else {
            format!(
                "LEAKED -- {} distinct status lines reached the model:",
                leak_hits.len()
            )
        }
    );
    for hit in leak_hits.iter().take(8) {
        println!("     {hit}");
    }

    let _ = std::fs::remove_dir_all(scratch);
}
