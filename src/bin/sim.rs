//! AAv2 simulation harness — exercises the REAL agent pipeline (routing,
//! brain loop, tools, confirmations, denials) against local mocks.
//!
//! Nothing here touches Discord or a real LLM: the REST client points at
//! an in-process mock Discord server, the model endpoint at an in-process
//! mock LLM with scripted responses. Run with:
//!
//!     cargo run --bin sim
//!
//! Scenarios: big generated file, dangerous-action confirmation (button
//! flow), permission denial, 20-user spam burst, and misheard-voice
//! transcripts. Each reports wall time; the report ends with a summary.

use std::{
    net::TcpListener,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use serde_json::{Value, json};

use axis_agents::config::{AgentProfile, GlobalSettings};
use axis_agents::memory::MemoryStore;
use axis_agents::runtime::agent::AgentCore;
use axis_agents::runtime::authority::Principal;
use axis_agents::runtime::brain::{self, Job, Outlet, Reason};
use axis_agents::runtime::hive::Hive;
use axis_agents::runtime::live::Live;
use axis_agents::tools::{self, ToolCtx};

// --- Mock servers ------------------------------------------------------------------

/// Every REST call the agent made: (method, path).
static DISCORD_LOG: OnceLock<Mutex<Vec<(String, String, String)>>> = OnceLock::new();
static LLM_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static LLM_MAX_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static LLM_CALLS: AtomicU64 = AtomicU64::new(0);
/// Scripted LLM turns, consumed in order: each is the assistant message
/// (content and/or tool_calls).
static LLM_SCRIPT: OnceLock<Mutex<Vec<Value>>> = OnceLock::new();

fn discord_log() -> &'static Mutex<Vec<(String, String, String)>> {
    DISCORD_LOG.get_or_init(|| Mutex::new(Vec::new()))
}

fn llm_script() -> &'static Mutex<Vec<Value>> {
    LLM_SCRIPT.get_or_init(|| Mutex::new(Vec::new()))
}

fn set_script(turns: Vec<Value>) {
    *llm_script().lock().unwrap() = turns;
}

/// A one-request HTTP server on its own thread. `handle` returns
/// (status, body). Good enough for reqwest clients.
fn serve(
    listener: TcpListener,
    handler: fn(method: &str, path: &str, body: &[u8]) -> (u16, String),
) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read until end of headers.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
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

fn mock_discord(method: &str, path: &str, body: &[u8]) -> (u16, String) {
    let body_snippet: String = String::from_utf8_lossy(body).chars().take(4000).collect();
    discord_log()
        .lock()
        .unwrap()
        .push((method.to_owned(), path.to_owned(), body_snippet));
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["gateway"]) => (
            200,
            json!({ "url": "wss://gateway.discord.gg" }).to_string(),
        ),
        ("GET", ["channels", _]) => (200, channel_json()),
        ("POST", ["channels", _, "messages"]) => (200, message_json()),
        ("PATCH", ["channels", _, "messages", _]) | ("DELETE", ["channels", _, "messages", _]) => {
            (200, message_json())
        }
        ("GET", ["guilds", _]) => (200, guild_json()),
        ("GET", ["guilds", _, "members", _]) => (200, member_json()),
        ("PATCH", ["guilds", _, "members", _]) => (200, member_json()),
        ("PUT", _) | ("DELETE", _) | ("PATCH", _) => (204, String::new()),
        _ => (200, json!({}).to_string()),
    }
}

fn mock_llm(_method: &str, _path: &str, body: &[u8]) -> (u16, String) {
    // Background calls (private summaries, judgments) get canned answers
    // so they never eat a scripted turn.
    if body
        .windows(b"writer's private memory".len())
        .any(|w| w == b"writer's private memory")
    {
        return (
            200,
            json!({ "choices": [{ "message": { "role": "assistant", "content": "Flood rising; Mara investigating the water's meaning." }, "finish_reason": "stop" }] }).to_string(),
        );
    }
    let entered = LLM_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
    LLM_MAX_IN_FLIGHT.fetch_max(entered, Ordering::SeqCst);
    LLM_CALLS.fetch_add(1, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(50)); // simulate thinking
    let turn = llm_script().lock().unwrap().first().cloned();
    if turn.is_some() {
        llm_script().lock().unwrap().remove(0);
    }
    let in_flight = LLM_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst) - 1;
    let _ = in_flight;
    let message = turn.unwrap_or(json!({ "role": "assistant", "content": "NO_REPLY" }));
    (
        200,
        json!({ "choices": [{ "message": message, "finish_reason": "stop" }] }).to_string(),
    )
}

fn channel_json() -> String {
    json!({ "id": "100", "type": 0, "name": "general", "guild_id": "50" }).to_string()
}
fn guild_json() -> String {
    json!({
        "id": "50", "name": "Sim Server", "owner_id": "1", "afk_timeout": 300,
        "verification_level": 0, "default_message_notifications": 0,
        "explicit_content_filter": 0,
        "roles": [
            { "id": "50", "name": "@everyone", "color": 0, "colors": { "primary_color": 0 }, "hoist": false, "position": 0, "permissions": "0", "managed": false, "mentionable": false },
            { "id": "60", "name": "Admin", "color": 0, "colors": { "primary_color": 0 }, "hoist": false, "position": 1, "permissions": "8", "managed": false, "mentionable": true }
        ],
        "emojis": [], "features": [], "mfa_level": 0, "premium_tier": 0,
        "preferred_locale": "en-US", "nsfw_level": 0, "premium_progress_bar_enabled": false
    }).to_string()
}
fn member_json() -> String {
    json!({
        "user": { "id": "1", "username": "simadmin", "discriminator": "0" },
        "roles": ["60"], "deaf": false, "mute": false
    })
    .to_string()
}
fn message_json() -> String {
    json!({
        "id": "999", "channel_id": "100",
        "author": { "id": "7", "username": "simbot", "discriminator": "0" },
        "content": "", "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0
    })
    .to_string()
}

// --- World + core ------------------------------------------------------------------

const GUILD: u64 = 50;
const CHANNEL: u64 = 100;
const ADMIN: u64 = 1;
const BOT: u64 = 7;
const PLEB: u64 = 102;

fn member_info(id: u64, name: &str, roles: Vec<u64>) -> axis_agents::runtime::world::MemberInfo {
    axis_agents::runtime::world::MemberInfo {
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

fn build_core(discord_base: String, llm_base: String) -> Arc<AgentCore> {
    let live = Arc::new(Live::default());
    let settings = GlobalSettings::default();
    let hive = Hive::new(live, settings);
    let mut model = axis_agents::config::ModelConfig::default();
    model.base_url = llm_base;
    model.model = "qwen-sim".to_owned();
    model.streaming = false;
    let profile = AgentProfile {
        id: "sim".to_owned(),
        name: "Magi".to_owned(),
        model,
        ..AgentProfile::default()
    };
    let http = Arc::new(
        axis::prelude::Http::builder(axis::prelude::Token::new("sim-token".to_owned()))
            .api_base(discord_base)
            .build()
            .expect("http builds"),
    );
    let memory = Arc::new(MemoryStore::load(
        axis_agents::paths::data_dir().join("sim-memory.json"),
    ));
    let core = AgentCore::new(profile, hive, memory, http, true);
    seed_world(&core);
    core
}

fn seed_world(core: &Arc<AgentCore>) {
    let guild: axis::model::Guild = serde_json::from_str(&guild_json()).unwrap();
    core.world.insert_guild(&guild, |state| {
        state.channels.insert(
            CHANNEL,
            serde_json::from_str::<axis::model::Channel>(&channel_json()).unwrap(),
        );
        state
            .members
            .insert(ADMIN, member_info(ADMIN, "simadmin", vec![60]));
        // A plain member with no roles.
        state
            .members
            .insert(PLEB, member_info(PLEB, "simmember", vec![]));
        // The bot needs a role above other members for moderation tools'
        // role-hierarchy checks (check_target looks up my_id's roles here).
        let mut bot_info = member_info(BOT, "zz_bot", vec![60]);
        bot_info.bot = true;
        state.members.insert(BOT, bot_info);
    });
    core.world.set_me(
        serde_json::from_value(
            json!({ "id": BOT.to_string(), "username": "Magi", "discriminator": "0" }),
        )
        .unwrap(),
    );
}

fn admin_principal(core: &Arc<AgentCore>) -> Principal {
    core.principal_for(Some(GUILD), &user(ADMIN))
}
fn pleb_principal(core: &Arc<AgentCore>) -> Principal {
    core.principal_for(Some(GUILD), &user(PLEB))
}
fn user(id: u64) -> axis::model::User {
    serde_json::from_value(
        json!({ "id": id.to_string(), "username": format!("u{id}"), "discriminator": "0" }),
    )
    .unwrap()
}

fn job(requester: Principal, question: &str) -> Job {
    Job {
        guild_id: Some(GUILD),
        channel_id: CHANNEL,
        triggers: vec![],
        reason: Reason::Direct,
        requester,
        task: None,
        outlet: Outlet::Channel,
        question: Some(question.to_owned()),
    }
}

fn tool_call(name: &str, args: Value) -> Value {
    json!({ "id": format!("call_{name}"), "type": "function",
            "function": { "name": name, "arguments": args.to_string() } })
}

fn discord_calls(method: &str) -> Vec<(String, String)> {
    discord_log()
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _, _)| m == method)
        .map(|(_, p, b)| (p.clone(), b.clone()))
        .collect()
}

fn reset_between() {
    discord_log().lock().unwrap().clear();
    LLM_CALLS.store(0, Ordering::SeqCst);
    LLM_MAX_IN_FLIGHT.store(0, Ordering::SeqCst);
}

// --- Scenarios ---------------------------------------------------------------------

async fn scenario_big_file(core: &Arc<AgentCore>) -> Result<String, String> {
    set_script(vec![
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("run_script", json!({
                    "code": "open('legion.txt','w').write('legion\\n' * 160_000)"
                }))] }),
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("attach_file", json!({ "name": "legion.txt" }))] }),
        json!({ "role": "assistant", "content": "The 1 MB legion file is below." }),
    ]);
    let started = Instant::now();
    brain::run(
        core,
        job(
            admin_principal(core),
            "make me a 1MB file of the word legion repeated",
        ),
    )
    .await;
    let elapsed = started.elapsed();
    let path = axis_agents::paths::data_dir()
        .join("sandbox")
        .join(CHANNEL.to_string())
        .join("legion.txt");
    let size = std::fs::metadata(&path)
        .map(|m| m.len())
        .map_err(|e| format!("legion.txt missing: {e}"))?;
    if size < 900_000 {
        return Err(format!("legion.txt is only {size} bytes"));
    }
    Ok(format!(
        "legion.txt = {size} bytes, brain loop took {:.2}s, {} LLM calls",
        elapsed.as_secs_f32(),
        LLM_CALLS.load(Ordering::SeqCst)
    ))
}

async fn scenario_confirmation(core: &Arc<AgentCore>) -> Result<String, String> {
    set_script(vec![
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("delete_channel", json!({ "channel": "general", "reason": "sim" }))] }),
        json!({ "role": "assistant", "content": "Queued the channel deletion — approve it below." }),
    ]);
    brain::run(
        core,
        job(admin_principal(core), "delete the general channel"),
    )
    .await;
    // The gate must have posted a button message and NOT deleted yet.
    let posted = discord_calls("POST");
    let confirm_posted = posted
        .iter()
        .any(|(path, body)| path.ends_with("/messages") && body.contains("axis_confirm"));
    if !confirm_posted {
        let feed: Vec<String> = core
            .live()
            .feed_snapshot(Some(&core.id), 8)
            .iter()
            .map(|item| format!("[{:?}] {}", item.kind, item.text))
            .collect();
        return Err(format!(
            "no confirmation message was posted (posts: {posted:?})
LLM calls: {}
agent feed:
  {}",
            LLM_CALLS.load(Ordering::SeqCst),
            feed.join(
                "
  "
            )
        ));
    }
    let deleted = discord_calls("DELETE");
    if deleted
        .iter()
        .any(|(path, _)| path.trim_end_matches('/').ends_with("/channels/100"))
    {
        return Err("the channel was deleted WITHOUT approval".to_owned());
    }
    Ok("gate held: buttons posted, nothing deleted until approved".to_owned())
}

async fn scenario_denial(core: &Arc<AgentCore>) -> Result<String, String> {
    let media = Arc::new(Mutex::new(axis_agents::tools::images::Media::default()));
    let ctx = ToolCtx {
        core: Arc::clone(core),
        guild_id: Some(GUILD),
        channel_id: CHANNEL,
        message_id: None,
        requester: pleb_principal(core),
        autonomous: false,
        replies_here: true,
        media,
        wants_file_attached: false,
        inputs: vec![],
    };
    let call = axis_agents::llm::ToolCall {
        id: "sim".to_owned(),
        name: "ban_member".to_owned(),
        arguments: json!({ "user": "1", "reason": "sim" }),
    };
    let outcome = tools::execute(&ctx, &call).await;
    if outcome.notice.is_none() {
        return Err(format!(
            "no app-posted notice on denial (text: {})",
            outcome.text
        ));
    }
    if !discord_calls("PUT").iter().any(|(p, _)| p.contains("bans")) {
        Ok("denied before Discord: fixed notice produced, no ban call".to_owned())
    } else {
        Err("a ban call reached Discord despite the denial".to_owned())
    }
}

async fn scenario_spam(core: &Arc<AgentCore>) -> Result<String, String> {
    set_script(
        (0..20)
            .map(|_| json!({ "role": "assistant", "content": "Noted." }))
            .collect(),
    );
    let started = Instant::now();
    let mut handles = Vec::new();
    for i in 0..20u64 {
        let core = Arc::clone(core);
        handles.push(tokio::spawn(async move {
            let mut requester = pleb_principal(&core);
            requester.name = format!("user{i}");
            brain::run(&core, job(requester, "hello?")).await
        }));
    }
    let mut failures = 0;
    for handle in handles {
        if !handle.await.unwrap_or(false) {
            failures += 1;
        }
    }
    let elapsed = started.elapsed();
    let max_conc = LLM_MAX_IN_FLIGHT.load(Ordering::SeqCst);
    Ok(format!(
        "20 concurrent jobs in {:.2}s, {} failures, peak concurrent LLM calls: {} (limit 2)",
        elapsed.as_secs_f32(),
        failures,
        max_conc
    ))
}

async fn scenario_misheard(core: &Arc<AgentCore>) -> Result<String, String> {
    // Garbled, echo-like, and nonsense transcripts, as a fast transcription
    // would produce them.
    let cases = ["magi magi magi", "the the the", "mumble xyzzy qux", ""];
    set_script(vec![
        json!({ "role": "assistant", "content": "NO_REPLY" });
        4
    ]);
    let mut ok = 0;
    for text in cases {
        if brain::run(core, job(admin_principal(core), text)).await {
            ok += 1;
        }
    }
    Ok(format!(
        "{ok}/4 garbled transcripts processed without a crash"
    ))
}

// --- Scenarios: AAv2 features ------------------------------------------------------

async fn scenario_catch_up_toggle(core: &Arc<AgentCore>) -> Result<String, String> {
    use axis_agents::memory::{StandingTask, TaskScope, TaskTrigger};
    core.hive.knowledge.guild_mut(GUILD, |k| {
        k.tasks.push(StandingTask {
            id: 1,
            agent_id: core.id.clone(),
            instruction: "report the news".to_owned(),
            trigger: TaskTrigger::Schedule {
                channel_id: CHANNEL,
                every_minutes: 60,
                at_minute: None,
                days: vec![],
            },
            author_id: ADMIN,
            author_name: "simadmin".to_owned(),
            author_rank: 0,
            author_level: axis_agents::config::AccessLevel::Owner,
            created: now_ms() - 10 * 3_600_000,
            active: true,
            runs: 0,
            last_run: now_ms() - 10 * 3_600_000,
            last_result: String::new(),
            origin_channel: CHANNEL,
            scope: TaskScope::Origin,
        });
    });
    let now = now_ms();
    // The catch-up skip: what the scheduler does on start with the toggle
    // off — every missed schedule counts *now* as its last run.
    core.hive.knowledge.guild_mut(GUILD, |k| {
        for task in k
            .tasks
            .iter_mut()
            .filter(|t| matches!(t.trigger, TaskTrigger::Schedule { .. }))
        {
            task.last_run = now;
        }
    });
    let still_due = core.hive.knowledge.guild(GUILD, |k| {
        k.tasks.iter().any(|t| matches!(&t.trigger, TaskTrigger::Schedule { every_minutes, .. } if now - t.last_run >= *every_minutes as i64 * 60_000))
    });
    core.hive.knowledge.guild_mut(GUILD, |k| k.tasks.clear());
    if still_due {
        Err("the overdue schedule would still fire after the skip".to_owned())
    } else {
        Ok("catch-up skip: the 10-hour-old schedule tick is treated as run".to_owned())
    }
}

async fn scenario_chill_cleanup(core: &Arc<AgentCore>) -> Result<String, String> {
    core.hive.knowledge.guild_mut(GUILD, |k| {
        k.chill.push(axis_agents::memory::ChillRule {
            channel_id: CHANNEL,
            agent_id: Some(core.id.clone()),
            setter_id: ADMIN,
            setter_name: "simadmin".to_owned(),
        });
    });
    let old = now_ms() - 10 * 60_000;
    let mut ids = Vec::new();
    core.hive.knowledge.channel_mut(CHANNEL, |archive| {
        for i in 0..5u64 {
            let id = 500 + i;
            ids.push(id);
            archive.push(axis_agents::memory::StoredMessage {
                id,
                author_id: BOT,
                author: "Magi".to_owned(),
                author_bot: true,
                content: format!("chatter number {i}"),
                ts: old,
                reply_to: None,
                reply_author: None,
                reply_snippet: None,
                attachments: vec![],
                deleted: false,
                edited: false,
                reactions: vec![],
            });
        }
        archive.push(axis_agents::memory::StoredMessage {
            id: 600,
            author_id: PLEB,
            author: "simmember".to_owned(),
            author_bot: false,
            content: "human says hi".to_owned(),
            ts: old,
            reply_to: None,
            reply_author: None,
            reply_snippet: None,
            attachments: vec![],
            deleted: false,
            edited: false,
            reactions: vec![],
        });
    });
    set_script(vec![
        json!({ "role": "assistant", "content": "{\"keep\": [504], \"summary\": \"posted the answer, human said hi\"}" }),
    ]);
    axis_agents::runtime::chill::tick(core).await;
    let deleted_ids: Vec<u64> = discord_calls("DELETE")
        .iter()
        .filter_map(|(p, _)| p.rsplit('/').next().and_then(|id| id.parse().ok()))
        .collect();
    if deleted_ids.contains(&600) {
        return Err("a HUMAN message (600) was deleted".to_owned());
    }
    if !ids
        .iter()
        .filter(|id| **id != 504)
        .all(|id| deleted_ids.contains(id))
    {
        return Err(format!(
            "low-value own messages not all deleted: {deleted_ids:?}"
        ));
    }
    let patched = discord_calls("PATCH");
    if !patched.iter().any(|(p, _)| p.contains("messages/504")) {
        return Err(format!(
            "the kept message was not summarized (patches: {patched:?})"
        ));
    }
    core.hive.knowledge.guild_mut(GUILD, |k| k.chill.clear());
    Ok(
        "chill cleanup: 4 own messages deleted, human kept, summary edited onto the one kept"
            .to_owned(),
    )
}

async fn scenario_chill_channel_silent(core: &Arc<AgentCore>) -> Result<String, String> {
    core.hive.knowledge.guild_mut(GUILD, |k| {
        k.chill.push(axis_agents::memory::ChillRule {
            channel_id: CHANNEL,
            agent_id: None,
            setter_id: ADMIN,
            setter_name: "simadmin".to_owned(),
        });
    });
    let ambient: axis::model::Message = serde_json::from_value(json!({
        "id": "700", "channel_id": CHANNEL.to_string(),
        "author": { "id": PLEB.to_string(), "username": "simmember", "discriminator": "0" },
        "content": "haha look at this video guys", "guild_id": GUILD.to_string(),
        "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "type": 0
    }))
    .unwrap();
    let reason = axis_agents::runtime::agent::classify(core, &ambient);
    core.hive.knowledge.guild_mut(GUILD, |k| k.chill.clear());
    match reason {
        None => Ok("chill channel: ambient chatter gets no reply reason (silent)".to_owned()),
        Some(reason) => Err(format!("ambient message would still trigger: {reason:?}")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// --- Scenarios: exact size + markup leak ---------------------------------------------

async fn scenario_exact_size_file(core: &Arc<AgentCore>) -> Result<String, String> {
    // The model asks write_file for exactly 1MB of "legion " (spaced).
    set_script(vec![
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("write_file", json!({
                    "name": "legion_spaced.txt",
                    "repeat": "legion",
                    "until_bytes": 1_048_576,
                    "join": " "
                }))] }),
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("attach_file", json!({ "name": "legion_spaced.txt" }))] }),
        json!({ "role": "assistant", "content": "The exact 1MB file is below." }),
    ]);
    brain::run(
        core,
        job(
            admin_principal(core),
            "make me exactly 1MB of the word legion with spaces",
        ),
    )
    .await;
    let path = axis_agents::paths::data_dir()
        .join("sandbox")
        .join(CHANNEL.to_string())
        .join("legion_spaced.txt");
    let bytes = std::fs::read(&path).map_err(|e| format!("file missing: {e}"))?;
    if bytes.len() != 1_048_576 {
        return Err(format!("{} bytes, not exactly 1048576", bytes.len()));
    }
    if !bytes.starts_with(b"legion legion") {
        return Err("no spaces between the words".to_owned());
    }
    Ok(format!("exactly 1048576 bytes with spaces between words"))
}

async fn scenario_markup_leak(core: &Arc<AgentCore>) -> Result<String, String> {
    // The model "replies" in its native XML call format — the exact leak.
    // The app must parse it as a tool call (send_message executes) and
    // the raw markup must never reach Discord.
    set_script(vec![
        json!({ "role": "assistant", "content": "<function=send_message>
<parameter=channel>general</parameter>
<parameter=content>Redone with spaces — exact 1MB this time.</parameter>
</function>" }),
        json!({ "role": "assistant", "content": "Redone: spaced, exact 1MB." }),
    ]);
    brain::run(
        core,
        job(admin_principal(core), "you forgot the spaces, redo it"),
    )
    .await;
    let posted = discord_calls("POST");
    let leaked: Vec<&String> = posted
        .iter()
        .map(|(_, b)| b)
        .filter(|b| b.contains("<function") || b.contains("<parameter"))
        .collect();
    if !leaked.is_empty() {
        return Err(format!("raw markup reached Discord: {leaked:?}"));
    }
    // The parsed call should have sent the salvaged content.
    // With edit-in-place the salvaged text may land as an edit to the
    // status message; either way the markup never got through.
    let patches = discord_calls("PATCH");
    let sent = posted
        .iter()
        .chain(patches.iter())
        .any(|(_, b)| b.contains("Redone with spaces"));
    if !sent {
        return Err("the salvaged content never reached Discord".to_owned());
    }
    Ok("XML markup salvaged to plain text; never posted raw".to_owned())
}

// --- Scenario: live edited answer ---------------------------------------------------

async fn scenario_live_edited_answer(core: &Arc<AgentCore>) -> Result<String, String> {
    // "search for x": the status message is posted fast, edited with
    // progress, then EDITED INTO the final answer with a file attached —
    // one message the whole way, attachments landing at the end.
    set_script(vec![
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("web_search", json!({ "query": "chess openings" }))] }),
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("write_file", json!({
                    "name": "openings.txt", "content": "1. e4 e5
2. Nf3 Nc6
3. Bb5"
                }))] }),
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("attach_file", json!({ "name": "openings.txt" }))] }),
        json!({ "role": "assistant", "content": "Found it — the Ruy Lopez. Openings file below." }),
    ]);
    brain::run(
        core,
        job(
            admin_principal(core),
            "magi search for the best chess opening and save it",
        ),
    )
    .await;

    let posts = discord_calls("POST");
    let patches = discord_calls("PATCH");
    // A status message was posted (the icon), and the FINAL answer landed
    // as a PATCH on that same message — not as a separate POST.
    let status_post = posts
        .iter()
        .find(|(p, b)| p.ends_with("/messages") && !b.contains("Found it"));
    let Some((status_path, _)) = status_post else {
        return Err(format!(
            "no status message was posted first (posts: {posts:?})"
        ));
    };
    let _ = status_path;
    let answer_patch = patches.iter().find(|(_, b)| b.contains("Found it"));
    let Some((_, _)) = answer_patch else {
        return Err(format!(
            "the answer never landed as an edit (patches: {patches:?})"
        ));
    };
    // No second message carrying the answer text.
    if posts.iter().any(|(_, b)| b.contains("Found it")) {
        return Err(
            "the answer was posted as a NEW message instead of editing the status".to_owned(),
        );
    }
    Ok("status posted fast, progress edited, answer edited in place".to_owned())
}

// --- Scenarios: long-form sections + numbered user picks -----------------------------

async fn scenario_long_form_sections(core: &Arc<AgentCore>) -> Result<String, String> {
    // A chapter request: the model writes real prose section by section
    // via write_section; the private summary keeps its context flat; the
    // finished document is saved and attached.
    set_script(vec![
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("write_section", json!({
                    "text": "Chapter One.

The rain had not stopped for nine days, and the streets of Velle ran with it..."
                }))] }),
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("write_section", json!({
                    "text": "By morning the flood had reached the lower market, and Mara knew what the water meant..."
                }))] }),
        json!({ "role": "assistant", "content": "",
                "tool_calls": [tool_call("write_section", json!({
                    "text": "She reached the archive at dusk. The doors were already open.",
                    "finished": true,
                    "name": "chapter_one.txt"
                }))] }),
        json!({ "role": "assistant", "content": "Chapter one is written in full — the file is below." }),
    ]);
    brain::run(
        core,
        job(
            admin_principal(core),
            "magi write me a full chapter of a story about a flood",
        ),
    )
    .await;
    let path = axis_agents::paths::data_dir()
        .join("sandbox")
        .join(CHANNEL.to_string())
        .join("chapter_one.txt");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        let feed: Vec<String> = core
            .live()
            .feed_snapshot(Some(&core.id), 10)
            .iter()
            .map(|item| format!("[{:?}] {}", item.kind, item.text))
            .collect();
        format!(
            "chapter missing: {e}
feed:
  {}",
            feed.join(
                "
  "
            )
        )
    })?;
    assert!(text.contains("nine days"), "first section missing");
    assert!(text.contains("lower market"), "second section missing");
    assert!(text.contains("archive at dusk"), "third section missing");
    // The private summary must never be posted to Discord.
    let posts = discord_calls("POST");
    let leaked_summary = posts.iter().any(|(_, b)| b.contains("PRIVATE NOTES"));
    if leaked_summary {
        return Err("the private summary leaked into chat".to_owned());
    }
    Ok("full chapter written across sections, saved, private notes never posted".to_owned())
}

async fn scenario_numbered_user_pick(core: &Arc<AgentCore>) -> Result<String, String> {
    // find_users lists numbered candidates; the moderation tool then
    // resolves `#2` to that exact ID — the model never writes an ID.
    let ctx = ToolCtx {
        core: Arc::clone(core),
        guild_id: Some(GUILD),
        channel_id: CHANNEL,
        message_id: None,
        requester: admin_principal(core),
        autonomous: false,
        replies_here: true,
        media: Arc::new(std::sync::Mutex::new(
            axis_agents::tools::images::Media::default(),
        )),
        wants_file_attached: false,
        inputs: vec![],
    };
    let listing = axis_agents::tools::users::find_users(&ctx, &serde_json::json!({ "like": "" }))
        .await
        .map_err(|e| format!("find_users failed: {e}"))?;
    assert!(listing.contains("[1]"), "numbered list missing: {listing}");
    assert!(listing.contains("[2]"), "candidate 2 missing: {listing}");

    // Verify pick_users inspects and understands the selection
    let inspection =
        axis_agents::tools::users::pick_users(&ctx, &serde_json::json!({ "pick": "2" }))
            .await
            .map_err(|e| format!("pick_users failed: {e}"))?;
    assert!(
        inspection.contains("simmember"),
        "pick_users missing simmember: {inspection}"
    );
    assert!(
        inspection.contains("102"),
        "pick_users missing ID 102: {inspection}"
    );

    // kick_member with `user: "#2"` resolves through the list to user 102
    // (the second member) — the model never types an ID.
    let call = axis_agents::llm::ToolCall {
        id: "sim".to_owned(),
        name: "kick_member".to_owned(),
        arguments: serde_json::json!({ "user": "#2", "reason": "sim pick" }),
    };
    let outcome = axis_agents::tools::execute(&ctx, &call).await;
    if outcome.text.starts_with("confirmation pending") {
        let feed_note = outcome.text.clone();
        return Err(format!(
            "kick hit the confirmation gate (unexpected): {feed_note}"
        ));
    }
    if !outcome.ok {
        return Err(format!("kick failed: {}", outcome.text));
    }
    let deletes = discord_calls("DELETE");
    let kicked = deletes.iter().any(|(p, _)| p.contains("members/102"));
    if !kicked {
        return Err(format!(
            "#2 did not resolve to member 102 (deletes: {deletes:?})"
        ));
    }
    Ok(
        "#2 resolved to exact member 102; inspected with pick_users, no ID was ever typed"
            .to_owned(),
    )
}

async fn scenario_interactive_voting(core: &Arc<AgentCore>) -> Result<String, String> {
    let ctx = ToolCtx {
        core: Arc::clone(core),
        guild_id: Some(GUILD),
        channel_id: CHANNEL,
        message_id: None,
        requester: admin_principal(core),
        autonomous: false,
        replies_here: true,
        media: Arc::new(std::sync::Mutex::new(
            axis_agents::tools::images::Media::default(),
        )),
        wants_file_attached: false,
        inputs: vec![],
    };

    // 1. Create an anonymous vote with 3 options and a 2-hour timer in the channel
    let call = axis_agents::llm::ToolCall {
        id: "sim".to_owned(),
        name: "create_vote".to_owned(),
        arguments: serde_json::json!({
            "question": "What should we play tonight?",
            "options": ["Minecraft", "Valorant", "Lethal Company"],
            "anonymous": true,
            "ends": "in 2 hours",
            "channel": "general"
        }),
    };
    let outcome = axis_agents::tools::execute(&ctx, &call).await;
    if !outcome.ok {
        return Err(format!("create_vote failed: {}", outcome.text));
    }
    assert!(
        outcome.text.contains("anonymous vote"),
        "confirmation text missing anonymous note"
    );
    assert!(
        outcome.text.contains("3 options"),
        "confirmation text missing option count"
    );

    // Verify Discord received the message with interactive buttons
    let posts = discord_calls("POST");
    let vote_post = posts.iter().find(|(p, b)| {
        p.contains("/channels/100/messages") && b.contains("What should we play tonight?")
    });
    let Some((_, body)) = vote_post else {
        return Err("vote message was not posted to Discord".to_owned());
    };
    assert!(body.contains("axis_vote:0"), "button 0 missing");
    assert!(body.contains("axis_vote:1"), "button 1 missing");
    assert!(body.contains("axis_vote:2"), "button 2 missing");
    assert!(
        body.contains("Anonymous Vote"),
        "anonymous notice missing from embed"
    );

    // 2. Cast a vote via button component
    let interaction_json = serde_json::json!({
        "id": "888",
        "application_id": "7",
        "type": 3,
        "guild_id": GUILD.to_string(),
        "channel_id": CHANNEL.to_string(),
        "token": "sim-token",
        "version": 1,
        "message": serde_json::from_str::<serde_json::Value>(&message_json()).unwrap(),
        "member": {
            "user": { "id": "102", "username": "simmember", "discriminator": "0" },
            "roles": [], "deaf": false, "mute": false
        },
        "data": {
            "component_type": 2,
            "custom_id": "axis_vote:0"
        }
    });
    let interaction: axis::model::Interaction = serde_json::from_value(interaction_json)
        .map_err(|e| format!("interaction parse failed: {e}"))?;

    let handled = axis_agents::runtime::community::on_component(core, &interaction).await;
    if !handled {
        return Err("button interaction was not handled by on_component".to_owned());
    }

    // 3. End the vote early via end_vote tool
    let end_call = axis_agents::llm::ToolCall {
        id: "sim".to_owned(),
        name: "end_vote".to_owned(),
        arguments: serde_json::json!({
            "vote": "play"
        }),
    };
    let end_outcome = axis_agents::tools::execute(&ctx, &end_call).await;
    if !end_outcome.ok {
        return Err(format!("end_vote failed: {}", end_outcome.text));
    }
    assert!(
        end_outcome.text.contains("Ended vote"),
        "end_vote result unexpected: {}",
        end_outcome.text
    );
    assert!(
        end_outcome.text.contains("Minecraft"),
        "winner missing from result: {}",
        end_outcome.text
    );

    Ok("interactive vote created with buttons, anonymous ballot unsealed and ended".to_owned())
}

async fn scenario_gif_and_sticker_search_and_pick(core: &Arc<AgentCore>) -> Result<String, String> {
    let ctx = ToolCtx {
        core: Arc::clone(core),
        guild_id: Some(GUILD),
        channel_id: CHANNEL,
        message_id: None,
        requester: admin_principal(core),
        autonomous: false,
        replies_here: true,
        media: Arc::new(std::sync::Mutex::new(
            axis_agents::tools::images::Media::default(),
        )),
        wants_file_attached: false,
        inputs: vec![],
    };

    // 1. Verify safety filter prevents NSFW searches
    let nsfw_call = axis_agents::llm::ToolCall {
        id: "sim-nsfw".to_owned(),
        name: "find_gifs".to_owned(),
        arguments: serde_json::json!({ "query": "free porn nsfw" }),
    };
    let nsfw_outcome = axis_agents::tools::execute(&ctx, &nsfw_call).await;
    assert!(!nsfw_outcome.ok, "nsfw search should have been rejected");
    assert!(
        nsfw_outcome.text.contains("disabled")
            || nsfw_outcome.text.contains("NSFW")
            || nsfw_outcome.text.contains("denied"),
        "error message should cite safety: {}",
        nsfw_outcome.text
    );

    // 2. Set mock candidates to test numbering, slug names, and pick tools
    let candidates = vec![
        axis_agents::tools::gifs::CandidateGif {
            id: 1,
            name: "happy-cat-jumping".to_owned(),
            title: "Happy Cat Jumping GIF".to_owned(),
            url: "https://gifdb.com/images/thumbnail/cat-jumping.gif".to_owned(),
            page_url: "https://gifdb.com/gif/cat-jumping".to_owned(),
            provider: "GIFDB".to_owned(),
            is_sticker: false,
            width: 400,
            height: 400,
        },
        axis_agents::tools::gifs::CandidateGif {
            id: 2,
            name: "party-parrot-sticker".to_owned(),
            title: "Party Parrot Dance".to_owned(),
            url: "https://gifdb.com/images/thumbnail/party-parrot.gif".to_owned(),
            page_url: "https://gifdb.com/gif/party-parrot".to_owned(),
            provider: "GIFDB".to_owned(),
            is_sticker: true,
            width: 300,
            height: 300,
        },
    ];
    let listing = axis_agents::tools::gifs::set_candidates(CHANNEL, candidates);
    assert!(listing.contains("[1]"), "candidate 1 missing from listing");
    assert!(
        listing.contains("happy-cat-jumping"),
        "slug 1 missing from listing"
    );
    assert!(listing.contains("[2]"), "candidate 2 missing from listing");
    assert!(
        listing.contains("[Sticker]"),
        "sticker tag missing from listing"
    );

    // 3. Resolve by number #1 and by slug name
    let c1 = axis_agents::tools::gifs::resolve(CHANNEL, "#1")
        .map_err(|e| format!("resolve #1 failed: {e}"))?;
    assert_eq!(c1.name, "happy-cat-jumping");

    let c2 = axis_agents::tools::gifs::resolve(CHANNEL, "party-parrot-sticker")
        .map_err(|e| format!("resolve slug failed: {e}"))?;
    assert_eq!(c2.id, 2);

    // 4. send_gif as embed link (as_attachment: false)
    let send_call = axis_agents::llm::ToolCall {
        id: "sim-send".to_owned(),
        name: "send_gif".to_owned(),
        arguments: serde_json::json!({
            "gif": "#1",
            "as_attachment": false
        }),
    };
    let send_outcome = axis_agents::tools::execute(&ctx, &send_call).await;
    assert!(send_outcome.ok, "send_gif failed: {}", send_outcome.text);
    assert!(
        send_outcome.text.contains("cat-jumping.gif"),
        "expected GIF link in outcome: {}",
        send_outcome.text
    );

    // 5. Verify recently-used tracking and "different" sticker resolution
    assert!(
        axis_agents::tools::gifs::is_recently_used(CHANNEL, &c1),
        "candidate 1 should now be recorded as used"
    );
    assert!(
        !axis_agents::tools::gifs::is_recently_used(CHANNEL, &c2),
        "candidate 2 should still be unused"
    );

    // Asking for a "different" sticker picks candidate 2 automatically
    let diff = axis_agents::tools::gifs::resolve(CHANNEL, "different")
        .map_err(|e| format!("resolve 'different' failed: {e}"))?;
    assert_eq!(diff.name, "party-parrot-sticker");

    // send_sticker with sticker: "different"
    let diff_call = axis_agents::llm::ToolCall {
        id: "sim-diff".to_owned(),
        name: "send_sticker".to_owned(),
        arguments: serde_json::json!({
            "sticker": "different",
            "as_attachment": false
        }),
    };
    let diff_outcome = axis_agents::tools::execute(&ctx, &diff_call).await;
    assert!(
        diff_outcome.ok,
        "send_sticker failed: {}",
        diff_outcome.text
    );
    assert!(
        diff_outcome.text.contains("party-parrot.gif"),
        "expected party-parrot sticker link: {}",
        diff_outcome.text
    );

    // 6. list_recent_gifs shows the history of media used in this channel
    let recent_call = axis_agents::llm::ToolCall {
        id: "sim-recent".to_owned(),
        name: "list_recent_gifs".to_owned(),
        arguments: serde_json::json!({}),
    };
    let recent_outcome = axis_agents::tools::execute(&ctx, &recent_call).await;
    assert!(
        recent_outcome.ok,
        "list_recent_gifs failed: {}",
        recent_outcome.text
    );
    assert!(
        recent_outcome.text.contains("happy-cat-jumping"),
        "recent history missing cat: {}",
        recent_outcome.text
    );
    assert!(
        recent_outcome.text.contains("party-parrot-sticker"),
        "recent history missing parrot: {}",
        recent_outcome.text
    );

    Ok("GIF & sticker search filtered NSFW; history tracked and 'different' resolved unused sticker".to_owned())
}

// --- Main --------------------------------------------------------------------------

fn main() {
    // Point the app's data dir at a scratch folder so the sim never reads
    // or writes the real config (set_var before any path is read).
    let scratch = std::env::temp_dir().join(format!("axis-sim-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    unsafe { std::env::set_var("AXIS_AGENTS_HOME", &scratch) };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("sim runtime")
        .block_on(sim_main(&scratch));
}

async fn sim_main(scratch: &std::path::Path) {
    let discord = TcpListener::bind("127.0.0.1:0").unwrap();
    let llm = TcpListener::bind("127.0.0.1:0").unwrap();
    let discord_base = format!("http://127.0.0.1:{}", discord.local_addr().unwrap().port());
    let llm_base = format!("http://127.0.0.1:{}/v1", llm.local_addr().unwrap().port());
    serve(discord, mock_discord);
    serve(llm, mock_llm);

    let core = build_core(discord_base, llm_base);

    let scenarios: Vec<(
        &str,
        fn(&Arc<AgentCore>) -> futures_local::BoxFuture<'_, Result<String, String>>,
    )> = vec![
        ("big generated file (1MB legion)", |c| {
            Box::pin(scenario_big_file(c))
        }),
        ("dangerous action confirmation", |c| {
            Box::pin(scenario_confirmation(c))
        }),
        ("permission denial (no narration)", |c| {
            Box::pin(scenario_denial(c))
        }),
        ("20-user spam burst", |c| Box::pin(scenario_spam(c))),
        ("misheard voice transcripts", |c| {
            Box::pin(scenario_misheard(c))
        }),
        ("startup catch-up toggle", |c| {
            Box::pin(scenario_catch_up_toggle(c))
        }),
        ("chill channel stays silent", |c| {
            Box::pin(scenario_chill_channel_silent(c))
        }),
        ("chill cleanup prunes own chatter", |c| {
            Box::pin(scenario_chill_cleanup(c))
        }),
        ("exact-size spaced file", |c| {
            Box::pin(scenario_exact_size_file(c))
        }),
        ("XML tool-call leak is parsed, not posted", |c| {
            Box::pin(scenario_markup_leak(c))
        }),
        ("live edited answer (one message, start to end)", |c| {
            Box::pin(scenario_live_edited_answer(c))
        }),
        ("long-form written in sections", |c| {
            Box::pin(scenario_long_form_sections(c))
        }),
        ("numbered user pick resolves exactly", |c| {
            Box::pin(scenario_numbered_user_pick(c))
        }),
        (
            "interactive voting with buttons and anonymous ballot",
            |c| Box::pin(scenario_interactive_voting(c)),
        ),
        (
            "GIF & sticker search, safety filter, and numbered pick",
            |c| Box::pin(scenario_gif_and_sticker_search_and_pick(c)),
        ),
    ];

    println!("Axis Agents simulation — real pipeline, mocked Discord + LLM\n");
    let mut passed = 0;
    for (name, run) in &scenarios {
        reset_between();
        match run(&core).await {
            Ok(report) => {
                passed += 1;
                println!("  PASS  {name}: {report}");
            }
            Err(error) => println!("  FAIL  {name}: {error}"),
        }
    }
    println!("\n{passed}/{} scenarios passed.", scenarios.len());
    let _ = std::fs::remove_dir_all(scratch);
    if passed < scenarios.len() {
        std::process::exit(1);
    }
}

/// A tiny no-op future helper module so the scenario table can hold async
/// fns without pulling in a futures crate.
mod futures_local {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
    #[allow(dead_code)]
    pub struct Never(pub ());
    impl Future for Never {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }
}
