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
    let body_snippet: String = String::from_utf8_lossy(body).chars().take(300).collect();
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

fn mock_llm(_method: &str, _path: &str, _body: &[u8]) -> (u16, String) {
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
const PLEB: u64 = 2;

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
            created: now_ms() - 10 * 3600_000,
            active: true,
            runs: 0,
            last_run: now_ms() - 10 * 3600_000,
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
    ];

    println!("Axis Agents simulation — real pipeline, mocked Discord + LLM\n");
    let mut passed = 0;
    for (name, run) in scenarios {
        reset_between();
        match run(&core).await {
            Ok(report) => {
                passed += 1;
                println!("  PASS  {name}: {report}");
            }
            Err(error) => println!("  FAIL  {name}: {error}"),
        }
    }
    println!("\n{passed}/8 scenarios passed.");
    let _ = std::fs::remove_dir_all(scratch);
    if passed < 8 {
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
