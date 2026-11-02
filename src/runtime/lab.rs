//! Experiments against the real model: the same request as a voice turn
//! and as a typed message, with the prompts the agent actually builds.
//! `cargo test lab_ -- --ignored --nocapture` (needs LM Studio running).

use std::sync::Arc;

use axis::model::{Guild, Message, User};
use axis::prelude::*;
use serde_json::json;

use super::agent::AgentCore;
use super::authority::{Principal, PrincipalKind};
use super::brain::{Job, Outlet, Reason};
use super::hive::Hive;
use super::live::Live;
use super::world::MemberInfo;
use crate::llm::{ChatRequest, ModelInfo};
use crate::memory::{MemoryStore, StoredMessage};

const GUILD: u64 = 1;
const GENERAL: u64 = 10;
const SCRATCHPAD: u64 = 11;
const CMD: u64 = 12;
const GAMING_VC: u64 = 20;
const GENERAL_VC: u64 = 21;
const UVX: u64 = 500;
const MAGI: u64 = 900;

fn parse<T: serde::de::DeserializeOwned>(mut value: serde_json::Value) -> T {
    // Fill whatever required fields the model types want, trying a few
    // shapes per field.
    let fillers = [json!(0), json!([]), json!(false), json!(""), json!({}), json!(null)];
    let mut last_field = String::new();
    let mut attempt = 0;
    for _ in 0..200 {
        match serde_json::from_value::<T>(value.clone()) {
            Ok(v) => return v,
            Err(e) => {
                let msg = e.to_string();
                let field = if msg.contains("missing field") {
                    msg.split('`').nth(1).unwrap_or_default().to_owned()
                } else {
                    last_field.clone()
                };
                if field.is_empty() {
                    panic!("{msg}");
                }
                if field == last_field {
                    attempt += 1;
                } else {
                    attempt = 0;
                    last_field = field.clone();
                }
                let filler = fillers.get(attempt).cloned().unwrap_or_else(|| panic!("{field}: {msg}"));
                value[&field] = filler;
            }
        }
    }
    panic!("couldn't build value");
}

fn fix<T: serde::de::DeserializeOwned>(value: serde_json::Value, fixes: &[(&str, serde_json::Value)]) -> T {
    let mut value = value;
    for (k, v) in fixes {
        value[*k] = v.clone();
    }
    parse(value)
}

fn core() -> Arc<AgentCore> {
    let config = crate::config::load(&crate::paths::config_path()).expect("config");
    let profile = config
        .agents
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("magi"))
        .cloned()
        .expect("magi");
    let live = Arc::new(Live::default());
    let hive = Hive::new(live, config.settings.clone());
    let memory = Arc::new(MemoryStore::load(std::env::temp_dir().join("axis-lab-memory.json")));
    let http = Arc::new(Http::builder(Token::new("offline".to_owned())).build().expect("http"));
    let core = AgentCore::new(profile, hive, memory, http, true);
    *core.model_info.write().unwrap() = ModelInfo {
        context_window: Some(16_384),
        ..Default::default()
    };
    core.world.set_me(fix(json!({"id": MAGI.to_string(), "username": "Magi", "bot": true}), &[("discriminator", json!("0"))]));
    let guild: Guild = fix(
        json!({"id": "1", "name": "Sue", "owner_id": UVX.to_string(), "roles": [{"id": "1", "name": "@everyone", "permissions": "3263553", "position": 0, "color": 0, "hoist": false, "managed": false, "mentionable": false, "flags": 0, "colors": {"primary_color": 0}}, {"id": "40", "name": "Moderator", "permissions": "0", "position": 3, "color": 0, "hoist": false, "managed": false, "mentionable": false, "flags": 0, "colors": {"primary_color": 0}}, {"id": "41", "name": "Gamer", "permissions": "0", "position": 1, "color": 0, "hoist": false, "managed": false, "mentionable": false, "flags": 0, "colors": {"primary_color": 0}}, {"id": "42", "name": "Artist", "permissions": "0", "position": 1, "color": 0, "hoist": false, "managed": false, "mentionable": false, "flags": 0, "colors": {"primary_color": 0}}], "emojis": []}),
        &[("premium_progress_bar_enabled", json!(false)), ("preferred_locale", json!("en-US")), ("features", json!([]))],
    );
    core.world.insert_guild(&guild, |g| {
        for (id, name, kind) in [
            (GENERAL, "general", 0),
            (SCRATCHPAD, "scratchpad3", 0),
            (CMD, "c-md", 0),
            (GAMING_VC, "Gaming", 2),
            (GENERAL_VC, "General", 2),
            (30, "Agent HQ", 4),
            (31, "Ticket Support", 4),
            (32, "agent-hq", 0),
            (33, "ticket-0001-uvx", 0),
            (34, "🎫ticket-support", 0),
        ] {
            let channel = fix(json!({"id": id.to_string(), "type": kind, "name": name, "guild_id": "1"}), &[]);
            g.channels.insert(id, channel);
        }
        for (id, name, bot) in [(UVX, "uvx", false), (MAGI, "Magi", true)] {
            g.members.insert(
                id,
                MemberInfo {
                    user_id: id,
                    username: name.to_owned(),
                    global_name: None,
                    nick: None,
                    roles: Vec::new(),
                    bot,
                    joined_at: None,
                    timeout_until: None,
                    avatar: None,
                    guild_avatar: None,
                },
            );
        }
        g.voice.insert(UVX, GAMING_VC);
    });
    core
}

fn line(id: u64, author: u64, name: &str, content: &str) -> StoredMessage {
    StoredMessage {
        id,
        author_id: author,
        author: name.to_owned(),
        author_bot: author == MAGI,
        content: content.to_owned(),
        ts: 1_793_500_000_000 + id as i64,
        reply_to: None,
        reply_author: None,
        reply_snippet: None,
        attachments: Vec::new(),
        edited: false,
        deleted: false,
        reactions: Vec::new(),
    }
}

fn history(core: &AgentCore, channel: u64, voice: bool) {
    let p = if voice { "🎙 (said in voice) " } else { "" };
    let lines = [
        (UVX, "uvx", "Yo, what's up?"),
        (MAGI, "Magi", "Not much! Just hanging out here. What's on your mind?"),
        (UVX, "uvx", "Hello."),
        (MAGI, "Magi", "Not much! Just here and ready to help with whatever you need."),
    ];
    core.hive.knowledge.channel_mut(channel, |archive| {
        for (i, (author, name, text)) in lines.iter().enumerate() {
            archive.push(line(1_000 + i as u64, *author, name, &format!("{p}{text}")));
        }
    });
}

fn requester(core: &AgentCore) -> Principal {
    core.world
        .with_guild(GUILD, |g| Principal::for_member(g, g.members.get(&UVX).unwrap(), PrincipalKind::Human))
        .unwrap()
}

fn voice_job(core: &AgentCore, text: &str) -> Job {
    Job {
        guild_id: Some(GUILD),
        channel_id: GAMING_VC,
        triggers: Vec::new(),
        reason: Reason::Direct,
        requester: requester(core),
        task: None,
        outlet: Outlet::Voice { guild_id: GUILD, speaker: UVX },
        question: Some(text.to_owned()),
    }
}

fn text_job(core: &AgentCore, text: &str) -> Job {
    let author: User = fix(json!({"id": UVX.to_string(), "username": "uvx"}), &[("discriminator", json!("0"))]);
    let message: Message = fix(
        json!({"id": "5000", "channel_id": GENERAL.to_string(), "guild_id": "1", "author": author, "content": text, "timestamp": "2026-11-01T08:00:00Z", "type": 0}),
        &[("edited_timestamp", json!(null)), ("tts", json!(false)), ("mention_everyone", json!(false)), ("pinned", json!(false))],
    );
    core.hive.knowledge.channel_mut(GENERAL, |archive| {
        archive.messages.retain(|m| m.id != 5000);
        archive.push(line(5000, UVX, "uvx", text));
    });
    Job {
        guild_id: Some(GUILD),
        channel_id: GENERAL,
        triggers: vec![message],
        reason: Reason::Direct,
        requester: requester(core),
        task: None,
        outlet: Outlet::Channel,
        question: None,
    }
}

async fn trial(core: &Arc<AgentCore>, label: &str, job: &Job, runs: usize) -> usize {
    let prompt = super::context::build(core, job, 1.0, &[], &[], &[]);
    let tools: Vec<&str> = prompt.tools.iter().map(|t| t.name.as_str()).collect();
    let system = prompt.messages[0].text().len();
    println!("\n=== {label}: {} tools {tools:?}, system {system} chars", tools.len());
    let profile = core.profile();
    let reasoning = match job.outlet {
        Outlet::Voice { .. } if job.question.as_deref().is_some_and(super::brain::looks_like_request) => {
            profile.model.thinking.replies.effort()
        }
        Outlet::Voice { .. } => profile.model.thinking.voice.effort(),
        _ => profile.model.thinking.replies.effort(),
    };
    let endpoint = core.endpoint();
    let mut called = 0;
    for _ in 0..runs {
        let started = std::time::Instant::now();
        let request = ChatRequest {
            thinking_budget: None,
            model: &profile.model.model,
            messages: &prompt.messages,
            tools: &prompt.tools,
            temperature: profile.model.temperature,
            max_tokens: prompt.max_output.max(1500),
            stream: false,
            json: false,
            reasoning,
        };
        match endpoint.chat(&request, &mut |_| {}).await {
            Ok(out) => {
                let calls: Vec<String> = out.tool_calls.iter().map(|c| format!("{} {}", c.name, c.arguments)).collect();
                called += usize::from(!calls.is_empty());
                println!(
                    "  {:.1}s {}",
                    started.elapsed().as_secs_f32(),
                    if calls.is_empty() { format!("TEXT: {}", crate::llm::truncate(&out.content.replace('\n', " "), 140)) } else { format!("TOOLS: {calls:?}") }
                );
            }
            Err(e) => println!("  error: {e}"),
        }
    }
    called
}

#[tokio::test]
#[ignore]
async fn lab_voice_vs_text() {
    let core = core();
    history(&core, GAMING_VC, true);
    history(&core, GENERAL, false);
    if std::env::var("LAB_DUMP").is_ok() {
        let prompt = super::context::build(&core, &voice_job(&core, "Send a message in scratchpad 3 saying hi."), 1.0, &[], &[], &[]);
        for m in &prompt.messages {
            println!("----- {:?}\n{}", m.role, m.text());
        }
    }
    let requests = [
        "Send a message in scratchpad 3 saying hi.",
        "Switch to the general voice channel and move me there too.",
        "Can you find an image of a zebra?",
    ];
    let mut score = (0, 0);
    for request in requests {
        score.0 += trial(&core, &format!("VOICE  {request}"), &voice_job(&core, request), 3).await;
        score.1 += trial(&core, &format!("TEXT   {request}"), &text_job(&core, request), 3).await;
    }
    println!("\ntool calls — voice {}/9, text {}/9", score.0, score.1);
}

/// What fake tools answer, so a whole turn can run without touching Discord.
fn fake_tool(name: &str, args: &str) -> String {
    match name {
        "send_message" => {
            let channel = serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|v| v.get("channel").and_then(|c| c.as_str()).map(str::to_owned))
                .unwrap_or_default();
            format!("sent to #{} (message 1566358877392474205)", channel.trim_start_matches('#'))
        }
        "join_voice" => "joined voice channel #General — I'll hear people there and answer out loud".to_owned(),
        "move_member" => "moved uvx to #General".to_owned(),
        "leave_voice" => "left #Gaming".to_owned(),
        "web_search" => "1. Minecraft 1.21.9 \"The Copper Age\" — Mojang (minecraft.net): adds copper golems, copper tools and armor, shelves.\n\
            2. Minecraft Live 2026 recap — new mob vote, the 'Chase the Skies' drop with happy ghasts.\n\
            3. Java 26.1 snapshot notes — performance work, new world generation tweaks."
            .to_owned(),
        "image_search" => "Verified image links (each one loads; post one on its own line and Discord embeds it — never post a link that isn't in this list):\n\
            1. Plains Zebra Equus quagga.jpg\n   image: https://upload.wikimedia.org/wikipedia/commons/e/e3/Plains_Zebra_Equus_quagga.jpg\n   page: https://commons.wikimedia.org/wiki/File:Plains_Zebra_Equus_quagga.jpg"
            .to_owned(),
        "list_channels" => "#general (text), #scratchpad3 (text), #c-md (text), Gaming (voice), General (voice)".to_owned(),
        _ => format!("done ({name} {args})"),
    }
}

fn messy_history(core: &AgentCore) {
    let lines = [
        (UVX, "uvx", "Yo, what's up?"),
        (MAGI, "Magi", "Not much! Just hanging out here. What's on your mind?"),
        (UVX, "uvx", "Yo, what is up?"),
        (MAGI, "Magi", "Not much! Just hanging out here. What's on your mind?"),
        (UVX, "uvx", "Send a message in CMD's channel, just whatever."),
        (MAGI, "Magi", "Sending a random message to cmd's channel now!"),
        (UVX, "uvx", "No, you didn't. Fix it."),
        (MAGI, "Magi", "Not much! Just here and ready to help with whatever you need."),
        (UVX, "uvx", "Okay, dinner."),
        (MAGI, "Magi", "Got it! Dinner time. I'll be here if you need anything else after."),
    ];
    core.hive.knowledge.channel_mut(GAMING_VC, |archive| {
        archive.messages.clear();
        for (i, (author, name, text)) in lines.iter().enumerate() {
            archive.push(line(2_000 + i as u64, *author, name, &format!("🎙 (said in voice) {text}")));
        }
    });
}

/// One whole voice turn: rounds of tool calls with fake results, the same
/// follow-through checks as the brain, then what would be spoken.
async fn full_turn(core: &Arc<AgentCore>, heard: &str) -> (Vec<String>, String, Option<String>) {
    use super::brain::{asks_for_details, claims_action, looks_like_request, offers_instead_of_doing};
    use crate::llm::Msg;
    let job = voice_job(core, heard);
    let prompt = super::context::build(core, &job, 1.0, &[], &[], &[]);
    let profile = core.profile();
    let reasoning = if looks_like_request(heard) {
        profile.model.thinking.replies.effort()
    } else {
        profile.model.thinking.voice.effort()
    };
    let endpoint = core.endpoint();
    let mut convo: Vec<Msg> = Vec::new();
    let mut used = Vec::new();
    let mut nudged = false;
    let mut summarized = false;
    for _round in 0..6 {
        let mut messages = prompt.messages.clone();
        messages.extend(convo.iter().cloned());
        let request = ChatRequest {
            thinking_budget: None,
            model: &profile.model.model,
            messages: &messages,
            tools: &prompt.tools,
            temperature: profile.model.temperature,
            max_tokens: prompt.max_output.max(1500),
            stream: false,
            json: false,
            reasoning,
        };
        let out = match endpoint.chat(&request, &mut |_| {}).await {
            Ok(out) => out,
            Err(e) => return (used, format!("ERROR {e}"), None),
        };
        if out.tool_calls.is_empty() {
            let content = out.content.clone();
            if !summarized && used.iter().any(|u| u != "<nudged>") && content.trim().trim_start_matches("CHAT:").trim().is_empty() {
                summarized = true;
                used.push("<summary>".to_owned());
                convo.push(Msg::assistant(content));
                convo.push(Msg::user("Now write your reply to them: briefly say what you did or found, in your own words."));
                continue;
            }
            if !nudged
                && used.is_empty()
                && (offers_instead_of_doing(&content)
                    || claims_action(&content)
                    || (asks_for_details(&content) && looks_like_request(heard)))
            {
                nudged = true;
                used.push("<nudged>".to_owned());
                convo.push(Msg::assistant(content));
                convo.push(Msg::user(
                    "Don't ask or just say it — actually do it now by calling the right tool, then write your final answer with the actual result. \
                     Take any details from what was already said in the conversation, or pick a sensible default and mention it. \
                     (Your previous draft will not be shown to anyone.)",
                ));
                continue;
            }
            let (spoken, chat) = super::voice::split_chat(&content);
            return (used, crate::runtime::speech::speakable(&spoken), chat);
        }
        convo.push(Msg::assistant_with_tools(out.content.clone(), out.tool_calls.clone()));
        for call in &out.tool_calls {
            used.push(format!("{} {}", call.name, call.arguments));
            convo.push(Msg::tool_result(call.id.clone(), fake_tool(&call.name, &call.arguments.to_string())));
        }
    }
    (used, "(ran out of rounds)".to_owned(), None)
}

#[tokio::test]
#[ignore]
async fn lab_messy_voice() {
    let core = core();
    let turns = [
        "uh send send a message in uh scratch pad three saying hi",
        "no you didn't, do it",
        "Maggie go go to the general vc and uh bring me with you",
        "can you look up the the latest mine craft update",
        "Do the research plan on my craft.",
        "find a picture of a zee bra",
        "what what's two plus two",
    ];
    for heard in turns {
        messy_history(&core);
        for run in 0..2 {
            let started = std::time::Instant::now();
            let (tools, spoken, chat) = full_turn(&core, heard).await;
            println!(
                "\n[{heard}] run {run} — {:.1}s\n  tools: {tools:?}\n  says: {spoken}\n  chat: {}",
                started.elapsed().as_secs_f32(),
                chat.map(|c| crate::llm::truncate(&c.replace('\n', " | "), 160)).unwrap_or_default()
            );
        }
    }
}

#[tokio::test]
#[ignore]
async fn lab_new_powers() {
    let core = core();
    history(&core, GENERAL, false);
    let requests = [
        "duplicate this channel and call the new one superscratchpad",
        "set the gaming vc status to grinding ranked",
        "make a ticket system in this channel for support, staff role Mods",
        "what tickets are open right now?",
        "add an emoji called pog from https://cdn.discordapp.com/emojis/1.png",
        "make a game night event tomorrow in the gaming vc",
        "make #general so only mods can see it",
        "nice you made the system, now can you put the category of the ticket supprt above agent hq",
        "move scratchpad3 to the top",
    ];
    for request in requests {
        trial(&core, &format!("TEXT   {request}"), &text_job(&core, request), 2).await;
    }
}

#[tokio::test]
#[ignore]
async fn lab_tickets() {
    let core = core();
    core.hive.knowledge.guild_mut(GUILD, |k| {
        k.tickets.agent_id = core.id.clone();
        k.tickets.category_id = 31;
        k.tickets.panel_channel_id = 34;
        k.tickets.next = 1;
        k.tickets.tickets.push(crate::memory::Ticket {
            number: 1,
            channel_id: 33,
            user_id: UVX,
            user_name: "uvx".into(),
            opened: 1,
            ..Default::default()
        });
    });
    let in_ticket = |text: &str| {
        let mut job = text_job(&core, text);
        job.channel_id = 33;
        core.hive.knowledge.channel_mut(33, |archive| {
            archive.messages.clear();
            archive.push(line(4_000, MAGI, "Magi", "🎫 Ticket #1 — @uvx"));
            archive.push(line(5_000, UVX, "uvx", text));
        });
        job
    };
    for request in [
        "can you update this ticket to also have moderator role access",
        "make moderators see all tickets from now on",
        "move the ticket panel to the ticket support channel",
        "make the panel a dropdown with bug report, billing, and other",
        "close this ticket, it's solved",
        "what was ticket 1 about?",
        "summarize every ticket uvx has opened",
    ] {
        trial(&core, &format!("TICKET {request}"), &in_ticket(request), 2).await;
    }
    for request in [
        "make a role menu for Gamer and Artist",
    ] {
        trial(&core, &format!("TEXT   {request}"), &text_job(&core, request), 2).await;
    }
}

#[tokio::test]
#[ignore]
async fn lab_rank_card_preview() {
    let out = std::path::PathBuf::from(std::env::var("LAB_OUT").unwrap_or_else(|_| ".".into()));
    for style in ["classic", "minimal", "bold"] {
        let card = crate::memory::RankCard { style: style.into(), ..Default::default() };
        let data = crate::tools::rankcard::RankCardData {
            name: "uvx".into(),
            avatar_url: Some("https://cdn.discordapp.com/embed/avatars/0.png".into()),
            level: 12,
            rank: 3,
            xp_into: 420,
            xp_needed: 1_000,
            total_xp: 12_345,
        };
        let png = crate::tools::rankcard::render(&card, &data).await.unwrap();
        std::fs::write(out.join(format!("rank-{style}.png")), png).unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn lab_levels_and_settings() {
    let core = core();
    history(&core, GENERAL, false);
    for request in [
        "what's my level?",
        "show me the leaderboard",
        "how active has the server been this week?",
        "pause the stats tracking",
        "design a better rank card, make it purple and sleek",
        "only let the Moderator role change your settings",
        "show me your leveling settings",
        "give people the Gamer role at level 10",
    ] {
        trial(&core, &format!("TEXT   {request}"), &text_job(&core, request), 2).await;
    }
}

#[tokio::test]
#[ignore]
async fn lab_translation() {
    let core = core();
    for (text, targets) in [
        ("hola, ¿alguien sabe cómo arreglar el error de la versión 0.0.6?", vec!["English".to_owned()]),
        ("yeah just reinstall it lol, worked for me", vec!["Spanish".to_owned()]),
        ("c'est vraiment nul ce bug 😭", vec!["English".to_owned(), "Spanish".to_owned()]),
        ("thanks!", vec!["English".to_owned()]),
    ] {
        let started = std::time::Instant::now();
        let result = super::translate::translate_text(&core, text, &targets).await;
        println!("[{:.1}s] {text}
  → {result:?}", started.elapsed().as_secs_f32());
    }
    // The agent deciding to translate, and being stopped.
    let pablo: u64 = 501;
    core.world.insert_guild(
        &fix(json!({"id": "2", "name": "x", "owner_id": "1", "roles": [], "emojis": []}), &[("premium_progress_bar_enabled", json!(false)), ("preferred_locale", json!("en-US")), ("features", json!([]))]),
        |_| {},
    );
    core.world.with_guild(GUILD, |_| ());
    core.hive.knowledge.channel_mut(GENERAL, |a| {
        a.messages.clear();
        a.push(line(6_000, UVX, "uvx", "anyone know how to fix the crash on startup?"));
        a.push(line(6_001, pablo, "Pablo", "no entiendo, ¿qué dices? yo también tengo un problema con el juego"));
        a.push(line(6_002, UVX, "uvx", "what? i don't understand"));
    });
    let mut job = text_job(&core, "what? i don't understand");
    job.reason = Reason::Ambient;
    job.question = Some("uvx is writing in English and Pablo in Spanish — they may not understand each other. If translating would help them, call start_translating for both (users: \"uvx, Pablo\", no fixed language) and tell them in one short line in both languages that you'll translate (and that they can ask you to stop). If they clearly understand each other, reply exactly NO_REPLY.".into());
    trial(&core, "AMBIENT mixed languages", &job, 2).await;
    trial(&core, "TEXT   translate what Pablo says into english", &text_job(&core, "magi can you translate what pablo says into english"), 2).await;
    trial(&core, "TEXT   stop translating", &text_job(&core, "ok you can stop translating now"), 2).await;
}

#[tokio::test]
#[ignore]
async fn lab_rank_card_designs() {
    let out = std::path::PathBuf::from(std::env::var("LAB_OUT").unwrap_or_else(|_| ".".into()));
    let data = || crate::tools::rankcard::RankCardData {
        name: "uvx".into(),
        avatar_url: Some("https://cdn.discordapp.com/embed/avatars/0.png".into()),
        level: 1,
        rank: 1,
        xp_into: 32,
        xp_needed: 155,
        total_xp: 132,
    };
    let designs = [
        ("bw-centered", crate::memory::RankCard {
            layout: "centered".into(), style: "minimal".into(), background: "#000000".into(), accent: "#ffffff".into(),
            text: "#ffffff".into(), muted: "#9a9a9a".into(), font: "poppins".into(), font_weight: "bold".into(),
            total_position: "above_bar".into(), corner_radius: 28, ..Default::default()
        }),
        ("glass-gradient", crate::memory::RankCard {
            style: "glass".into(), background: "#3b0764".into(), background2: "#0f172a".into(), gradient: "diagonal".into(),
            accent: "#c084fc".into(), font: "montserrat".into(), avatar_shape: "rounded".into(), total_position: "with_xp".into(),
            corner_radius: 20, ..Default::default()
        }),
        ("compact-mono", crate::memory::RankCard {
            layout: "compact".into(), style: "bold".into(), accent: "#22c55e".into(), font: "press start 2p".into(),
            text_scale: 0.7, bar_shape: "square".into(), avatar_shape: "square".into(), ..Default::default()
        }),
    ];
    for (name, card) in designs {
        let png = crate::tools::rankcard::render(&card, &data()).await.unwrap();
        std::fs::write(out.join(format!("card-{name}.png")), png).unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn lab_card_requests() {
    let core = core();
    history(&core, GENERAL, false);
    for request in [
        "can you put the xp total higher and make it brighter",
    ] {
        trial(&core, &format!("TEXT   {request}"), &text_job(&core, request), 2).await;
    }
}

#[tokio::test]
#[ignore]
async fn lab_agentic() {
    let core = core();
    for request in std::env::var("LAB_REQUESTS").unwrap_or_default().split("||").filter(|r| !r.trim().is_empty()) {
        history(&core, GENERAL, false);
        let started = std::time::Instant::now();
        let (outcome, steps) = super::brain::think_only(&core, &text_job(&core, request.trim())).await;
        println!("\n=== {request} ({:.0}s, {} tools)", started.elapsed().as_secs_f32(), outcome.tools_used);
        for step in steps {
            println!("  · {step}");
        }
        println!("  files: {:?}", outcome.files.iter().map(|(n, b)| format!("{n} ({} bytes)", b.len())).collect::<Vec<_>>());
        if let Ok(dir) = std::env::var("LAB_OUT") {
            for (name, bytes) in &outcome.files {
                let _ = std::fs::write(std::path::Path::new(&dir).join(name), bytes);
            }
        }
        println!("  reply: {}", crate::llm::truncate(&outcome.text.unwrap_or_default().replace('\n', " / "), 500));
    }
}

#[tokio::test]
#[ignore]
async fn lab_dm_reach() {
    let core = core();
    core.world.with_guild(GUILD, |_| ());
    let dm_job = |text: &str| {
        let mut job = text_job(&core, text);
        job.guild_id = None;
        job.channel_id = 7_000;
        job.requester = super::authority::Principal::dm_user(UVX, "uvx".into());
        core.hive.knowledge.channel_mut(7_000, |a| {
            a.messages.clear();
            a.push(line(5_000, UVX, "uvx", text));
        });
        job
    };
    for request in [
        "can you dm susie from sue server and tell them i said whatsupp",
        "who is susie?",
        "who banned someone recently in my servers?",
        "post 'server maintenance tonight' in the general channel on sue",
        "what servers are you in",
    ] {
        trial(&core, &format!("DM     {request}"), &dm_job(request), 2).await;
    }
}

#[tokio::test]
#[ignore]
async fn lab_people_and_logs() {
    let core = core();
    history(&core, GENERAL, false);
    for request in [
        "get the profile pictures of uvx, susie and magi",
        "show me everyone's pfps with the Mods role",
        "what's uvx's display name and username?",
        "show me the audit logs",
        "who timed someone out recently?",
        "who deleted messages in general?",
        "write me 40 fun facts about space, all at once",
    ] {
        trial(&core, &format!("TEXT   {request}"), &text_job(&core, request), 2).await;
    }
}

#[test]
#[ignore]
fn lab_avatar_sheet() {
    use image::{DynamicImage, Rgba, RgbaImage};
    let colors = [[200, 60, 60], [60, 160, 90], [70, 90, 220], [220, 180, 40], [150, 70, 200]];
    let pictures: Vec<Option<DynamicImage>> = (0..7)
        .map(|i| (i != 3).then(|| { let c = colors[i % 5]; DynamicImage::ImageRgba8(RgbaImage::from_pixel(256, 256, Rgba([c[0], c[1], c[2], 255]))) }))
        .collect();
    let entries: Vec<(String, String)> = ["Fn", "Susie the Great", "Magi", "a really long display name here", "Bob", "Zed", "Q"]
        .iter()
        .enumerate()
        .map(|(i, n)| (n.to_string(), format!("@user_{i}")))
        .collect();
    let sheet = crate::tools::images::avatar_sheet(&pictures, &entries);
    let out = std::env::var("LAB_OUT").unwrap_or_else(|_| ".".into());
    sheet.save(std::path::Path::new(&out).join("avatar_sheet.png")).unwrap();
}

#[tokio::test]
#[ignore]
async fn lab_no_chat_rule() {
    let core = core();
    core.hive.knowledge.guild_mut(GUILD, |k| k.no_chat.clear());
    // A ticket channel inside the "Ticket Support" category.
    core.world.upsert_channel(&fix(json!({"id": "33", "type": 0, "name": "ticket-0001-uvx", "guild_id": "1", "parent_id": "31"}), &[]));
    let rule = |channel_id: u64, agent: Option<String>| crate::memory::NoChatRule { channel_id, agent_id: agent, setter_name: "uvx".into(), setter_rank: 100, ..Default::default() };
    core.hive.knowledge.guild_mut(GUILD, |k| {
        k.no_chat.push(rule(SCRATCHPAD, Some(core.id.clone())));
        k.no_chat.push(rule(31, None));
        k.no_chat.push(rule(CMD, Some("someone-else".into())));
    });
    assert!(core.barred(GUILD, SCRATCHPAD).is_some());
    assert!(core.barred(GUILD, 33).is_some(), "a category covers its channels");
    assert!(core.barred(GUILD, GENERAL).is_none());
    assert!(core.barred(GUILD, CMD).is_none(), "another agent's rule");
    assert!(core.barred_anywhere(SCRATCHPAD).is_some());
    assert!(core.barred(GUILD, GENERAL_VC).is_none());
    assert!(super::agent::asks_to_lift("magi you can chat here again"));
    assert_eq!(super::brain::named_channels(&core, GUILD, "don't ever talk in the ticket support category"), vec!["Ticket Support".to_owned()]);
    let mut both = super::brain::named_channels(&core, GUILD, "never chat in scratchpad3 or c-md");
    both.sort();
    assert_eq!(both, vec!["c-md".to_owned(), "scratchpad3".to_owned()]);
    assert_eq!(super::brain::named_channels(&core, GUILD, "dont post in general"), vec!["general".to_owned()]);
    assert!(!super::agent::asks_to_lift("magi what's the weather"));
    core.hive.knowledge.guild_mut(GUILD, |k| k.no_chat.clear());

    // The model picks the right tools.
    history(&core, GENERAL, false);
    for request in [
        "never chat in scratchpad3 or c-md",
        "don't ever talk in the ticket support category",
        "from now on dont post anything in general",
        "ok you can chat in scratchpad3 again",
        "be quiet for 10 minutes",
    ] {
        if request.contains("again") {
            core.hive.knowledge.guild_mut(GUILD, |k| k.no_chat.push(rule(SCRATCHPAD, Some(core.id.clone()))));
        }
        for _ in 0..2 {
            core.hive.knowledge.guild_mut(GUILD, |k| k.no_chat.retain(|r| r.channel_id == SCRATCHPAD && request.contains("again")));
            if request.contains("again") && core.barred(GUILD, SCRATCHPAD).is_none() {
                core.hive.knowledge.guild_mut(GUILD, |k| k.no_chat.push(rule(SCRATCHPAD, Some(core.id.clone()))));
            }
            let (outcome, steps) = super::brain::think_only(&core, &text_job(&core, request)).await;
            let rules: Vec<u64> = core.hive.knowledge.guild(GUILD, |k| k.no_chat.iter().map(|r| r.channel_id).collect());
            println!("
=== {request}
  steps: {steps:?}
  rules now: {rules:?}
  reply: {}", outcome.text.unwrap_or_default());
        }
    }
}

#[tokio::test]
#[ignore]
async fn lab_cut_off_continues() {
    let core = core();
    let mut profile = core.profile();
    core.set_profile(profile);
    history(&core, GENERAL, false);
    let request = "compare the main CPU architectures (x86, ARM, RISC-V): history, how each works, strengths, weaknesses, who uses them, and where each is heading. be thorough, several sections";
    let (outcome, steps) = super::brain::think_only(&core, &text_job(&core, request)).await;
    println!("error: {:?}", outcome.error);
    let text = outcome.text.unwrap_or_default();
    println!("steps: {steps:?}\nlength: {} chars\n---\n{text}\n---", text.len());
}

#[tokio::test]
#[ignore]
async fn lab_commands_and_schedules() {
    let core = core();
    core.hive.knowledge.guild_mut(GUILD, |k| {
        k.commands.clear();
        k.tasks.clear();
    });
    let only = std::env::var("LAB_ONLY").ok();
    for request in [
        "make a /rules command that posts our server rules: be nice, no spam, have fun",
        "make a /news command that makes you look up today's top tech news and post a summary",
        "turn off the leaderboard command in this server",
        "every day at 9am post a motivational quote in general",
        "every monday and friday at 6pm remind everyone in general about game night",
        "tomorrow at 3pm post 'meeting starts now' in general",
        "every 2 hours check the news and post anything big in general",
    ] {
        if only.as_deref().is_some_and(|o| !request.contains(o)) {
            continue;
        }
        history(&core, GENERAL, false);
        let (outcome, steps) = super::brain::think_only(&core, &text_job(&core, request)).await;
        println!("\n=== {request}\n  steps: {steps:?}\n  reply: {}", crate::llm::truncate(&outcome.text.unwrap_or_default().replace('\n', " / "), 220));
    }
    let (commands, off, tasks) = core.hive.knowledge.guild(GUILD, |k| (k.commands.clone(), k.disabled_commands.clone(), k.tasks.clone()));
    println!("\ncommands: {:?}", commands.iter().map(|c| format!("/{} [{:?}] {}", c.name, c.mode, c.text)).collect::<Vec<_>>());
    println!("off here: {off:?}");
    for t in tasks {
        println!("task #{}: {} — {}", t.id, t.trigger.describe("general"), t.instruction);
    }
}

#[tokio::test]
#[ignore]
async fn lab_github_watch() {
    let core = core();
    core.hive.knowledge.guild_mut(GUILD, |k| k.tasks.clear());
    for request in [
        "magi watch https://github.com/MagiGun/sue/releases and every release i want you to send the link of the release to this channel and talk about its highlights update",
        "what are you watching right now?",
    ] {
        history(&core, GENERAL, false);
        let (outcome, steps) = super::brain::think_only(&core, &text_job(&core, request)).await;
        println!("\n=== {request}\n  steps: {steps:?}\n  reply: {}", crate::llm::truncate(&outcome.text.unwrap_or_default().replace('\n', " / "), 400));
    }
    let task = core.hive.knowledge.guild(GUILD, |k| k.tasks.first().cloned()).expect("a watch task");
    println!("\ntask: {} — {}\n  trigger: {:?}", task.trigger.describe("general"), task.instruction, task.trigger);

    // A release comes out: pretend we last saw the one before the newest.
    let crate::memory::TaskTrigger::GitHub { repo, .. } = &task.trigger else { panic!("not a github task") };
    let super::github::Fetched::Body(serde_json::Value::Array(list), _) = super::github::api(&format!("/repos/{repo}/releases?per_page=3"), "", "").await.expect("releases") else { panic!() };
    let second = list[1]["id"].as_u64().unwrap();
    let inputs: Vec<String> = super::github::new_releases(&list, second, repo).into_iter().map(|(_, t)| t).collect();
    let mut job = text_job(&core, "");
    job.reason = super::brain::Reason::Task;
    job.triggers.clear();
    job.task = Some(task.clone());
    job.question = inputs.first().cloned();
    let (outcome, steps) = super::brain::think_only(&core, &job).await;
    let post = super::github::finish_post(&outcome.text.unwrap_or_default(), job.question.as_deref(), &task.instruction);
    println!("\n=== new release arrives\n  steps: {steps:?}\n  post (as it goes out):\n{post}");

    history(&core, GENERAL, false);
    let request = "magi stop watching the sue repo";
    let (outcome, steps) = super::brain::think_only(&core, &text_job(&core, request)).await;
    println!("\n=== {request}\n  steps: {steps:?}\n  reply: {}", outcome.text.unwrap_or_default().replace('\n', " / "));
    println!("tasks left: {}", core.hive.knowledge.guild(GUILD, |k| k.tasks.len()));
}

#[tokio::test]
#[ignore]
async fn lab_cant_post_there() {
    let core = core();
    core.hive.knowledge.guild_mut(GUILD, |k| k.tasks.clear());
    // #announcements: everyone (and so the bot) can read but not send.
    core.world.upsert_channel(&fix(
        json!({"id": "40", "type": 0, "name": "announcements", "guild_id": "1", "permission_overwrites": [{"id": "1", "type": 0, "allow": "0", "deny": "2048"}]}),
        &[],
    ));
    println!("can't post in #announcements: {:?}", core.cant_post(GUILD, 40));
    println!("can't post in #general: {:?}", core.cant_post(GUILD, GENERAL));
    println!("if DMs are closed, tell uvx in: {:?}", super::tasks::fallback_channels(&core, GUILD, UVX, 40));
    assert!(core.cant_post(GUILD, 40).is_some());
    assert!(core.cant_post(GUILD, GENERAL).is_none());
    history(&core, GENERAL, false);
    let request = "magi watch https://github.com/MagiGun/sue/releases and post every new release in #announcements with its highlights";
    let (outcome, steps) = super::brain::think_only(&core, &text_job(&core, request)).await;
    println!("\n=== {request}\n  steps: {steps:?}\n  reply: {}", outcome.text.unwrap_or_default().replace('\n', " / "));
    println!("tasks made: {}", core.hive.knowledge.guild(GUILD, |k| k.tasks.len()));
}


#[tokio::test]
#[ignore]
async fn lab_clone_and_delete_loop() {
    // Fake channel tools (no real Discord): set LAB_FAKE_DISCORD=1.
    let core = core();
    history(&core, GENERAL, false);
    let request = "magi duplicate this channel 10 times and every 5 seconds delete 1 of them until its back to only this one";
    let job = text_job(&core, request);
    // A correction comes in while it works (if it started wrong).
    let steer = std::env::var("LAB_STEER").is_ok();
    if steer {
        let core2 = core.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(25)).await;
            let author: User = fix(json!({"id": UVX.to_string(), "username": "uvx"}), &[("discriminator", json!("0"))]);
            let message: Message = fix(
                json!({"id": "5001", "channel_id": GENERAL.to_string(), "guild_id": "1", "author": author, "content": "no, first do all the clones first", "timestamp": "2026-11-01T08:00:30Z", "type": 0}),
                &[("edited_timestamp", json!(null)), ("tts", json!(false)), ("mention_everyone", json!(false)), ("pinned", json!(false))],
            );
            core2.steering.lock().unwrap().entry(GENERAL).or_default().push((message, super::brain::Reason::Direct));
            println!("  (sent: no, first do all the clones first)");
        });
    }
    let started = std::time::Instant::now();
    let (outcome, steps) = super::brain::think_only(&core, &job).await;
    println!("\n=== {request} ({:.0}s)\n  steps: {steps:?}", started.elapsed().as_secs_f32());
    let log = crate::tools::lab_fake::LOG.lock().unwrap().clone();
    for (at, what) in &log {
        println!("  {:>5.1}s  {what}", at.duration_since(started).as_secs_f32());
    }
    println!("  copies left: {:?}", crate::tools::lab_fake::CHANNELS.lock().unwrap());
    println!("  reply: {}", outcome.text.unwrap_or_default().replace('\n', " / "));
}

#[tokio::test]
#[ignore]
async fn lab_not_allowed_vs_no_tool() {
    let core = core();
    const MISAKA: u64 = 777;
    let misaka: User = fix(json!({"id": MISAKA.to_string(), "username": "misaka"}), &[("discriminator", json!("0"))]);
    core.world.upsert_author(GUILD, &misaka, None);
    core.hive.knowledge.channel_mut(GENERAL, |a| {
        a.messages.clear();
        a.push(line(4990, UVX, "uvx", "magi duplicate this channel 4 times"));
        a.push(line(4991, MAGI, "Magi", "Done — made 4 copies: general-copy-1, general-copy-2, general-copy-3, general-copy-4."));
    });
    for (who, name, text) in [(MISAKA, "misaka", "delete them magi"), (UVX, "uvx", "do what misaka said magi")] {
        let mut job = text_job(&core, text);
        let author: User = fix(json!({"id": who.to_string(), "username": name}), &[("discriminator", json!("0"))]);
        job.triggers[0].author = author.clone();
        job.requester = core.principal_for(Some(GUILD), &author);
        core.hive.knowledge.channel_mut(GENERAL, |a| {
            a.messages.retain(|m| m.id != 5000);
            a.push(line(if who == MISAKA { 4995 } else { 5000 }, who, name, text));
        });
        let (outcome, steps) = super::brain::think_only(&core, &job).await;
        println!("\n=== {name}: {text}\n  steps: {steps:?}\n  reply: {}", outcome.text.unwrap_or_default().replace('\n', " / "));
        core.hive.knowledge.channel_mut(GENERAL, |a| a.push(line(5100 + who % 7, MAGI, "Magi", "(answered)")));
    }
}
