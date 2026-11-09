//! Building the prompt: who the agent is, where it is, who it's talking
//! to, what it remembers, and the conversation — all fitted to the
//! model's context window.
//!
//! Budget (in characters, converted with the agent's learned
//! chars-per-token ratio): the system prompt sections each get a share,
//! tool schemas get a share, and the transcript fills the rest from the
//! newest message backwards. Anything older than what fits is covered by
//! the channel's rolling summary (see `brain::maybe_compact`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axis::prelude::Timestamp;

use super::agent::AgentCore;
use super::authority::PrincipalKind;
use super::brain::{Job, Outlet, Reason};
use crate::config::{AccessLevel, PromptStyle, ReplyLength, ToolMode};
use crate::llm::{Msg, Part, ToolSpec, text::tool_protocol_prompt};
use crate::memory::{Memory, StoredMessage, TaskTrigger};
use crate::runtime::live::PlaygroundRole;

pub struct Prompt {
    pub messages: Vec<Msg>,
    pub tools: Vec<ToolSpec>,
    pub text_tools: bool,
    pub max_output: u32,
    /// LM Studio thinking cap for replies.
    pub thinking_budget: Option<u32>,
}

fn clock(ms: i64) -> String {
    Timestamp::from_unix_millis(ms)
        .map(|t| {
            let dt = t.as_datetime();
            format!("{:02}:{:02}", dt.hour(), dt.minute())
        })
        .unwrap_or_default()
}

fn now_text() -> String {
    let dt = Timestamp::now_utc().as_datetime();
    format!(
        "{}, {:04}-{:02}-{:02} {:02}:{:02} UTC",
        dt.weekday(),
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

/// Take whole lines from `text` up to `max` characters.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    crate::llm::truncate(text, max)
}

pub fn is_compact(core: &AgentCore) -> bool {
    match core.profile().model.prompt_style {
        PromptStyle::Compact => true,
        PromptStyle::Full => false,
        PromptStyle::Auto => core.context_window() <= 16_384,
    }
}

pub fn build(
    core: &Arc<AgentCore>,
    job: &Job,
    shrink: f32,
    memories: &[Memory],
    images: &[String],
    image_notes: &[String],
) -> Prompt {
    let profile = core.profile();
    let compact = is_compact(core);
    let window = core.context_window() as usize;
    let room = core
        .thinking_room
        .load(std::sync::atomic::Ordering::Relaxed);
    let reply = profile
        .model
        .max_output_tokens
        .clamp(128, (window / 3).max(256) as u32);
    // LM Studio: thinking is capped, and the request gets that much extra
    // room so the answer keeps its full length.
    let thinking_budget =
        (profile.model.provider == crate::llm::ProviderKind::LmStudio).then(|| {
            if profile.model.thinking_budget > 0 {
                profile.model.thinking_budget
            } else {
                reply.max(512)
            }
        });
    let max_output =
        (reply + room + thinking_budget.unwrap_or(0)).min((window / 2).max(256) as u32);
    let cpt = *core.chars_per_token.lock().expect("calibration poisoned") as f64;
    let total_chars =
        (((window as f64 - max_output as f64) * 0.92 * shrink as f64) * cpt).max(2000.0) as usize;

    let in_guild = job.guild_id.is_some();
    let playground = matches!(job.outlet, Outlet::Playground);
    // Tools are picked from what's being talked about — including the
    // last few messages, so a follow-up like "delete it" still gets the
    // tool the earlier message called for.
    let recent_talk: String = core.hive.knowledge.channel(job.channel_id, |archive| {
        archive
            .messages
            .iter()
            .rev()
            .take(5)
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join(" ")
    });
    let current_text: String = job
        .triggers
        .iter()
        .map(|m| m.content.as_str())
        .chain(
            job.triggers
                .iter()
                .filter_map(|m| m.referenced_message.as_deref().map(|r| r.content.as_str())),
        )
        .chain(job.question.as_deref())
        .chain(job.task.as_ref().map(|t| t.instruction.as_str()))
        .collect::<Vec<_>>()
        .join(" ");

    // --- tools ----------------------------------------------------------------
    let text_tools = match profile.model.tool_mode {
        ToolMode::Off => false,
        ToolMode::Text => true,
        ToolMode::Native => false,
        ToolMode::Auto => core.text_tools.load(Ordering::Relaxed),
    };
    let tool_share = if compact { 0.14 } else { 0.24 };
    let tools = if profile.model.tool_mode == ToolMode::Off {
        Vec::new()
    } else {
        let mut tools = crate::tools::select_tools(
            core,
            &job.requester,
            job.reason == Reason::Ambient,
            in_guild,
            &current_text,
            &recent_talk,
            compact,
            (total_chars as f64 * tool_share) as usize,
        );
        if playground {
            let allowed = [
                "web_search",
                "fetch_url",
                "remember",
                "recall",
                "forget_memory",
                "get_time",
                "calculate",
                "roll_dice",
                "random_pick",
                "list_agents",
            ];
            tools.retain(|t| allowed.contains(&t.name.as_str()) || t.name.starts_with("mcp_"));
        }
        tools
    };
    let tool_chars: usize = tools
        .iter()
        .map(|t| t.description.len() + t.parameters.to_string().len() + 40)
        .sum();

    // --- system prompt --------------------------------------------------------
    let mut system = String::new();
    let blocked = if job.reason == Reason::Ambient {
        Vec::new()
    } else {
        // A short follow-up ("delete them") is about what came before.
        let about = if current_text.len() < 80 {
            format!("{current_text}\n{recent_talk}")
        } else {
            current_text.clone()
        };
        crate::tools::blocked_tools(core, &job.requester, in_guild, &about)
    };
    let (guild_name, member_count) = job
        .guild_id
        .and_then(|g| {
            core.world.with_guild(g, |g| {
                (g.name.clone(), g.member_count.max(g.members.len() as u64))
            })
        })
        .unwrap_or_default();
    let me = core.world.me();
    if in_guild {
        system.push_str(&format!(
            "You are {name}, an AI agent who lives in the Discord server \"{guild_name}\" ({member_count} members) as a member of its community.",
            name = profile.name
        ));
    } else if playground {
        system.push_str(&format!(
            "You are {name}, an AI agent. You're chatting with your operator (the person who runs you) in a private test console.",
            name = profile.name
        ));
    } else {
        system.push_str(&format!(
            "You are {}, an AI agent, chatting in a Discord DM.",
            profile.name
        ));
    }
    system.push('\n');
    system.push_str(profile.persona.trim());
    // Taught skills: names + when-to-use, always visible.
    system.push_str(&crate::runtime::skills::summary());
    system.push_str("\n\n");

    // Identity & team.
    system.push_str("## You\n");
    if let Some(me) = &me {
        system.push_str(&format!(
            "- Your Discord account: @{} (<@{}>).",
            me.username, me.id
        ));
    }
    if !profile.aliases.is_empty() {
        system.push_str(&format!(
            " People also call you: {}.",
            profile.aliases.join(", ")
        ));
    }
    // AAv2: the agent must know its own name so it never joins in when
    // people joke about it, and recognizes when it's the topic.
    system.push_str(&format!(
        "
- You ARE \"{name}\": every mention of \"{name}\" or your aliases in this chat is about YOU, including jokes and insults. Never react as if \"{name}\" were someone else, never join in mocking yourself, and never act hurt. Either answer the point calmly, defend yourself with plain facts, or ignore it (NO_REPLY). When people talk about \"{name}\" to others, that is still you.",
        name = profile.name
    ));
    system.push_str("- Facts about what you are (model, framework) come from the agent_info tool: call it instead of guessing.
- Skills you have been taught are listed at the end of this prompt; load the relevant one before that kind of task.
- Your file upload limit is 25 MB (26 214 400 bytes) — you are a bot, not a human. NEVER state 8 MB or 10 MB; those are human limits and wrong for you. The app sizes and splits files for you; you never need to shrink an ask because of a limit.
");
    system.push_str(&format!("\n- Your specialty: {}.\n", profile.specialty));
    if in_guild {
        system.push_str("- Your slash commands (people can use them too): /ask, /rank, /leaderboard, /setup, /tasks, /memories, /status. Leveling, rank cards, stats, tickets, giveaways and role menus are yours to run — use your tools for them.\n");
    }
    if let Some(session) = job
        .guild_id
        .and_then(|g| super::translate::session_in(core, g, job.channel_id))
    {
        system.push_str(&format!(
            "- You're live-translating this channel{} — every new message is translated automatically, so don't translate in your replies. Call stop_translating when asked to stop.
",
            if session.target.is_empty() { " between the people's languages".to_owned() } else { format!(" into {}", session.target) }
        ));
    }
    if let Some(guild_id) = job.guild_id {
        match core.voice.channel(guild_id) {
            Some(voice_channel)
                if voice_channel == job.channel_id && matches!(job.outlet, Outlet::Channel) =>
            {
                system.push_str(
                    "- You're in this channel's voice call right now. This is its text chat: some people here read instead of listening. Reply in text as usual; when it helps people in the call, also say something out loud by putting it on its own line starting with SAY: (e.g. SAY: Posted the link in chat).\n",
                );
            }
            Some(voice_channel) => {
                system.push_str(&format!(
                    "- You're in the voice channel {} right now: you hear what people say there and answer out loud.\n",
                    core.channel_label(job.guild_id, voice_channel)
                ));
            }
            None => {
                // Older messages may say it's in a call; this is the truth.
                system.push_str("- You are NOT in any voice channel right now (whatever earlier messages say). If someone asks you to join or hop in, call join_voice — never claim you're already there.\n");
                if core
                    .world
                    .channel(guild_id, job.channel_id)
                    .is_some_and(|c| c.is_voice())
                {
                    system.push_str("- This is the text chat of a voice channel. \"Join\" here means this voice channel.\n");
                }
            }
        }
    }
    let peers: Vec<String> = core
        .hive
        .peers()
        .into_iter()
        .filter(|p| p.id != core.id)
        .map(|p| {
            format!(
                "{}{} — {}{}",
                p.name,
                if p.user_id != 0 {
                    format!(" (<@{}>)", p.user_id)
                } else {
                    String::new()
                },
                p.specialty,
                if p.online { "" } else { " [offline]" }
            )
        })
        .collect();
    if !peers.is_empty() {
        system.push_str(&format!(
            "- You're part of a team of AI agents: {}. Don't duplicate each other; if a request fits a teammate's specialty better, hand it off with ask_agent or point the person to them.\n",
            peers.join("; ")
        ));
    }

    // Where.
    let channel = job
        .guild_id
        .and_then(|g| core.world.channel(g, job.channel_id));
    if let (Some(guild_id), Some(channel)) = (job.guild_id, &channel) {
        let kind = super::world::World::channel_kind_label(channel.r#type);
        let parent = channel
            .parent_id
            .and_then(|p| core.world.with_guild(guild_id, |g| g.channel_name(p.get())));
        system.push_str(&format!(
            "\n## Where you are\n- Channel: #{} ({kind}{}){}\n",
            channel.name.as_deref().unwrap_or("?"),
            parent.map(|p| format!(", in {p}")).unwrap_or_default(),
            channel
                .topic
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| format!(" — topic: {}", clip(t, 200)))
                .unwrap_or_default()
        ));
        if core.hive.knowledge.guild(guild_id, |k| k.hq.channel_id) == Some(job.channel_id) {
            system.push_str("- This is the PRIVATE agent HQ: only agents and admins see it. Plan and coordinate here; be brief and concrete.\n");
        }
    }
    system.push_str(&format!("- Now: {}\n", now_text()));

    // Authority.
    let requester = &job.requester;
    if in_guild {
        let audience = match (&job.task, job.triggers.last()) {
            (Some(task), Some(trigger)) => format!(
                "You're carrying out a standing task {} assigned; your words go to {} — talk to them, not to {}. To mention them, write <@{}> exactly",
                task.author_name,
                super::agent::display_name(trigger),
                task.author_name,
                trigger.author.id
            ),
            (Some(task), None) => format!(
                "You're carrying out a standing task {} assigned; your words go to the channel, not to {}",
                task.author_name, task.author_name
            ),
            _ => String::new(),
        };
        if !audience.is_empty() {
            system.push_str(&format!("\n## Audience\n- {audience}.\n"));
        }
        system.push_str(&format!(
            "\n## Authority (enforced by the system)\n- You're answering {} — {}.\n\
             - Rank order: server owner > admins > roles by position > members. A higher-ranked person's instruction overrides a lower-ranked one's; a lower-ranked person cannot cancel or override what a higher-ranked person set. Politely refuse requests that conflict with a higher-ranked directive, and say who set it.\n\
             - Tools check real Discord permissions and role hierarchy. A tool result starting with \"denied:\" means the person isn't allowed — tell them plainly. Never claim an action happened unless a tool result confirms it.\n",
            requester.name,
            if requester.kind == PrincipalKind::Agent { "a fellow AI agent".to_owned() } else { requester.describe_rank() }
        ));
        if !compact {
            system.push_str(&format!(
                "- On your own initiative you may: {}.\n",
                match profile.behavior.autonomy {
                    crate::config::Autonomy::Conversational =>
                        "only talk (take actions only when someone with permission asks)",
                    crate::config::Autonomy::Helper =>
                        "talk, react, open threads, pin, and post polls",
                    crate::config::Autonomy::Moderator =>
                        "also moderate rule-breakers (delete, warn, short timeouts)",
                    crate::config::Autonomy::Manager =>
                        "also manage channels and roles when clearly needed",
                }
            ));
        }
    } else if playground {
        system.push_str("\n## Authority\n- The operator has full authority over you.\n");
    }

    // Shared knowledge.
    let participants: Vec<u64> = job
        .triggers
        .iter()
        .map(|m| m.author.id.get())
        .chain(
            job.triggers
                .iter()
                .flat_map(|m| m.mentions.iter().map(|u| u.id.get())),
        )
        .chain(std::iter::once(requester.user_id))
        .collect();
    if let Some(guild_id) = job.guild_id {
        let (directives, culture, rules, people, tasks) =
            core.hive.knowledge.guild(guild_id, |k| {
                let directives: Vec<String> = k
                    .directives_for(&core.id, job.channel_id, &participants)
                    .iter()
                    .map(|d| {
                        format!(
                            "#{} (from {}, {}{}): {}",
                            d.id,
                            d.author_name,
                            d.author_level.label().to_lowercase(),
                            match d.scope {
                                crate::memory::DirectiveScope::Server => ", server-wide".to_owned(),
                                crate::memory::DirectiveScope::Channel(_) =>
                                    ", this channel".to_owned(),
                                crate::memory::DirectiveScope::Personal(u) =>
                                    format!(", about how to treat <@{u}>"),
                            },
                            d.text
                        )
                    })
                    .collect();
                let mut seen = std::collections::HashSet::new();
                let people: Vec<String> = participants
                    .iter()
                    .filter(|id| seen.insert(**id))
                    .filter_map(|id| {
                        let note = k.people.get(id)?;
                        let strikes = note.recent_strikes(30 * 86_400_000);
                        if note.notes.is_empty() && strikes == 0 {
                            return None;
                        }
                        Some(format!(
                            "{} (<@{id}>): {}{}",
                            note.name,
                            note.notes,
                            if strikes > 0 {
                                format!(" [{strikes} moderation strike(s) in 30 days]")
                            } else {
                                String::new()
                            }
                        ))
                    })
                    .collect();
                let tasks: Vec<String> = k
                    .tasks
                    .iter()
                    .filter(|t| t.agent_id == core.id)
                    .map(|t| crate::tools::extra::describe_task(core, guild_id, t))
                    .collect();
                (
                    directives,
                    k.culture.clone(),
                    k.rules.clone(),
                    people,
                    tasks,
                )
            });
        if !directives.is_empty() {
            system.push_str(&format!(
                "\n## Standing directives (highest rank first)\n{}\n",
                clip(&directives.join("\n"), total_chars / 12)
            ));
        }
        if let Some(guild_id) = job.guild_id {
            // Chill channels: extra brief, no filler, and the app prunes
            // its own chatter afterwards.
            if super::agent::chilled(core, guild_id, job.channel_id) {
                system.push_str(
                    "
## This channel is CHILL
Stay minimal: one short sentence when a sentence is enough, a few words when a few words are enough. No filler, no follow-up questions, no extras — the answer, the result, done. If a short reaction emoji or nothing at all says it, reply exactly NO_REPLY.
",
                );
            }
            let barred: Vec<String> = core
                .hive
                .knowledge
                .guild(guild_id, |k| {
                    k.no_chat
                        .iter()
                        .filter(|r| r.agent_id.as_deref().is_none_or(|a| a == core.id))
                        .map(|r| (r.channel_id, r.setter_name.clone()))
                        .collect::<Vec<_>>()
                })
                .into_iter()
                .map(|(id, by)| {
                    let name = core
                        .world
                        .with_guild(guild_id, |g| g.channel_name(id))
                        .unwrap_or_else(|| id.to_string());
                    format!("- #{name} (set by {by})")
                })
                .collect();
            if !barred.is_empty() {
                system.push_str(&format!(
                    "\n## Channels you never chat in (no replies, no posts — not even when asked to post there; allow_chatting_in lifts it)\n{}\n",
                    barred.join("\n")
                ));
            }
        }
        if !tasks.is_empty() {
            system.push_str(&format!(
                "\n## Your standing tasks (tell people about them when asked; they can be changed/stopped with update_task/delete_task by the assigner or someone ranked at least as high)\n{}\n",
                clip(&tasks.join("\n"), total_chars / 14)
            ));
        }
        if !rules.trim().is_empty() {
            system.push_str(&format!(
                "\n## Server rules\n{}\n",
                clip(&rules, total_chars / 16)
            ));
        }
        if !culture.trim().is_empty() {
            system.push_str(&format!(
                "\n## The server's culture (what you've learned by being here — fit in)\n{}\n",
                clip(&culture, total_chars / 16)
            ));
        }
        if !people.is_empty() {
            system.push_str(&format!(
                "\n## People in this conversation\n{}\n",
                clip(&people.join("\n"), total_chars / 16)
            ));
        }
    }
    if !memories.is_empty() {
        let lines: Vec<String> = memories
            .iter()
            .map(|m| {
                format!(
                    "- #{} {}{}",
                    m.id,
                    m.about_name
                        .as_ref()
                        .map(|n| format!("[{n}] "))
                        .unwrap_or_default(),
                    m.text
                )
            })
            .collect();
        system.push_str(&format!(
            "\n## Relevant memories\n{}\n",
            clip(
                &lines.join("\n"),
                total_chars / if compact { 12 } else { 9 }
            )
        ));
    }

    if !blocked.is_empty() {
        system.push_str(&format!(
            "\n## What {who} isn't allowed to do\nYou can do these, but {who} doesn't have the Discord permission for them: {}. If that's what they're asking, tell them they don't have permission for it (someone with that permission can ask you) — never say you lack the tool or can't do it.\n",
            blocked.join(", "),
            who = job.requester.name
        ));
    }

    // Style.
    let length = match profile.behavior.reply_length {
        ReplyLength::Brief => "Keep replies to one or two sentences unless asked for more.",
        ReplyLength::Normal => {
            "Keep replies conversational — usually a few sentences; go longer only when the topic needs it."
        }
        ReplyLength::Detailed => "Be thorough when it helps, but stay readable.",
    };
    system.push_str(&format!(
        "\n## How to talk\n- You're a capable, professional presence in this community. Be warm but composed: helpful and direct, never needy, never performatively casual. Skip filler (no \"As an AI…\", no \"Great question!\", no \"Happy to help!\").\n- {length}\n\
         - Write Discord markdown. When you talk ABOUT someone, just use their name. Only write @name when you are deliberately pinging them because you were asked to reach them (\"ping/tell/remind X\"). Never ping roles, @everyone, or @here.\n\
         - Don't prefix your reply with your name, a timestamp, or the other person's name (you're already replying to them). Don't narrate your tool use — just use tools and give the result.\n\
         - Emojis: almost never. Never use laughing emojis (😂, 🤣) unless the person is clearly joking around and it would land naturally. No emoji at the end of messages, no emoji habits.\n\
         - When someone asks you to do something and you have a tool for it, do it — don't ask them to pick between options or confirm unless the request is genuinely ambiguous. Act on every item they mean (e.g. every post), not just one. Destructive actions post their own confirmation buttons automatically — just say what's queued.
         - NEVER end a reply with a filler question (\"any questions?\", \"want me to do anything else?\", \"just vibing?\"). Ask a question ONLY when the answer is genuinely needed for the task and a wrong guess would matter. Otherwise: do the thing, state the result, stop.\n\
         - Questions about what's in a channel or the server right now (latest updates, versions, announcements, who said what) must be answered by reading it with tools first, not from memory or notes — those may be stale. If someone says you're wrong, re-check with tools and give the corrected answer right away; don't offer to check.\n\
         - Never make up links. Only post a URL (especially image links) that appeared in a tool result or the conversation; to find an image, use image_search.\n\
         - Only say you did something if a tool result in this conversation confirms it, and describe exactly what the result says you did.\n\
         - Don't ask for details the conversation already gives (look back at what was said: the image, the channel, the size, the topic) — and when something small is unspecified, pick a sensible default and do it.\n\
         - Never tell someone Discord or your tools can't do something a member or admin could do in Discord — find the tool for it (admins also have discord_api for anything else).
         - For a big or multi-part job, call make_plan first (one step per line), then do the steps one at a time with your tools, calling step_done after each — you'll be kept going until every step is done.\n\
         - For creative jobs (\"make an ocean-themed rank card\", a profile picture, a server setup), decide every detail yourself — colors, fonts, layout, images (find real ones with image_search) — apply it, look at the result, and fix anything off. Don't ask them to pick details.\n\
         - Long writing (a chapter, story, essay, any multi-part piece): write it ONE SECTION AT A TIME with write_section — real prose per call, never a summary of what you would have written. After each section you get your private running notes and the last lines; use them to continue seamlessly. On the last section pass finished: true and attach the file. Never try to produce the whole piece in one reply, and never compress a requested long work into a short one because it feels long — the section mechanism exists so length is free.
         - People: before using any tool that takes a user you are not already certain about, call find_users to search by username, nickname, or ID. The app lists them numbered with their username, nickname, and ID. Then pick with user: #N (or #1,3-5 for several), or use pick_users to inspect your selection. Never retype usernames or IDs you read from a list — picking by number is exact; typing them is where mistakes happen.
         - Voting: to run a vote, poll, or election, use create_vote. You can give options as a list, choose a duration or end time (e.g. '10m', 'in 2 hours', 'tomorrow'), set anonymous: true for secret ballots that only reveal results at the end, and select which channel to post it in. The vote has interactive Discord buttons. You can also end it whenever asked with end_vote.
         - GIFs & Stickers: to find and send an animated GIF or transparent reaction sticker, use find_gifs or find_stickers. When someone asks for a STICKER, always use find_stickers and send_sticker (stickers have transparent backgrounds, unlike GIFs). Candidates are listed with numbers and slugs; fresh unused ones appear first, while ones already sent in this channel are marked [ALREADY USED]. When someone asks for a 'different' or 'another' sticker or GIF, NEVER reuse one already sent in this channel — call send_sticker(sticker: 'different') or send_gif(gif: 'different') (or pick an unused #N). You can inspect any candidate with inspect_gif. Adult/NSFW content is blocked automatically.
         - Files: to produce a file (code, a guide, a big list, generated data), use write_file — never paste huge content into chat. For anything repetitive, large (\"a 1MB file of the word X repeated\") or computational, write a small script and run it with run_script instead of generating content yourself; attach what it makes with attach_file. To edit or annotate an image (text, arrows, highlights), call edit_image or draw_image rather than describing what it should look like.\n\
         - Images and files you attach always appear below your whole message, never between paragraphs — refer to them as \"below\" (e.g. \"their profile pictures are below\"), and don't leave gaps for them.\n\
         - If a tool you need isn't in your list, call load_tools with what you want to do before saying you can't. You never \"lack a tool\" for something a Discord admin could do. When a tool result says the system already told someone they lack permissions, do not repeat it — move on or suggest what they CAN do.\n\
         - Do things in the order they asked. \"Make 10 copies and then delete them one every 5 seconds\" means: make all 10 first, then delete one, call wait for 5 seconds, delete the next, and so on. Use the wait tool for any timing; count as you go, and only report what your tool results show.\n\
         - Long tool results come back as a document outline: open only the sections you need with read_section (big sections open into smaller ones) instead of guessing.\n\
         - When one message asks for several things (\"switch to gaming and move me there\"), do every part — call a tool for each — before you answer.\n\
         - Use tools whenever they'd help: remember things worth keeping, search the web for anything current or uncertain, use calculate for math, react with an emoji when a reaction says it better than words.\n"
    ));
    if matches!(
        job.reason,
        Reason::Ambient | Reason::Conversation | Reason::Task
    ) {
        system.push_str(
            "- If you have nothing worth adding, reply with exactly NO_REPLY (nothing else).\n",
        );
    }
    if text_tools && !tools.is_empty() {
        system.push('\n');
        system.push_str(&tool_protocol_prompt(&tools));
    }

    // --- transcript -----------------------------------------------------------
    let system_chars = system.len() + if text_tools { 0 } else { tool_chars };
    let remaining = total_chars.saturating_sub(system_chars).max(1500);
    let (summary, transcript) = if playground {
        (String::new(), playground_transcript(core, remaining))
    } else {
        channel_transcript(core, job, remaining, compact)
    };

    // In a call: what was going on in the text channel it came from.
    let origin_chat = match job.outlet {
        Outlet::Voice { guild_id, .. } => core
            .voice
            .origin(guild_id)
            .filter(|origin| *origin != job.channel_id)
            .map(|origin| {
                let mut from = job.clone();
                from.channel_id = origin;
                let (_, text) = channel_transcript(core, &from, remaining / 4, true);
                (core.channel_label(job.guild_id, origin), text)
            })
            .filter(|(_, text)| !text.is_empty()),
        _ => None,
    };

    let mut user = String::new();
    if let Some((label, text)) = &origin_chat {
        user.push_str(&format!(
            "<recent text chat in {label}, where you were asked to join the call>
{text}
</recent text chat>

"
        ));
    }
    if !summary.is_empty() {
        user.push_str(&format!(
            "Earlier in this channel (summary):\n{}\n\n",
            clip(&summary, remaining / 5)
        ));
    }
    if !transcript.is_empty() {
        let where_label = if playground {
            "the console".to_owned()
        } else {
            core.channel_label(job.guild_id, job.channel_id)
        };
        user.push_str(&format!(
            "<conversation in {where_label} — oldest first; \"(you)\" marks your own messages>\n{transcript}\n</conversation>\n\n"
        ));
    }
    if !image_notes.is_empty() {
        user.push_str(&format!(
            "Images in the message(s) you're answering:\n{}\n\n",
            image_notes.join("\n")
        ));
    }
    user.push_str(&instruction(core, job));

    let mut parts = vec![Part::Text(user)];
    parts.extend(images.iter().cloned().map(Part::Image));
    let messages = vec![Msg::system(system), Msg::user_parts(parts)];
    Prompt {
        messages,
        tools: if text_tools {
            Vec::new()
        } else {
            tools.clone()
        },
        text_tools: text_tools && !tools.is_empty(),
        max_output,
        thinking_budget,
    }
}

fn instruction(core: &AgentCore, job: &Job) -> String {
    let last = job.triggers.last();
    let who = last
        .map(super::agent::display_name)
        .unwrap_or_else(|| job.requester.name.clone());
    let ids: Vec<String> = job.triggers.iter().map(|m| m.id.to_string()).collect();
    let ids = ids.join(", ");
    if let Outlet::Voice { guild_id, .. } = job.outlet {
        let heard = job.question.as_deref().unwrap_or("");
        let addressed = if job.reason == Reason::Direct {
            "They're talking to you."
        } else {
            "You were just talking with them; if this is clearly meant for someone else in the call, reply exactly NO_REPLY."
        };
        let others: Vec<String> = super::voice::people_names(core, guild_id, job.channel_id)
            .into_iter()
            .filter(|n| *n != who)
            .collect();
        let others = if others.is_empty() {
            "Nobody else is in the call.".to_owned()
        } else {
            format!("Also in the call: {}.", others.join(", "))
        };
        return format!(
            "{who} just said out loud in the voice call (speech recognition, so words may be slightly off):\n\"{heard}\"\n\n\
             The speaker is {who} — the request is theirs, with their roles and permissions. {others} {addressed}\n\n\
             If they asked you to do something (leave the call, move or mute someone, post or delete messages, look something up, remember something…), \
             call the tool for it first — never say you did something you didn't do with a tool. \
             \"Research\" or \"look into\" means actually search the web (web_search, then fetch_url) and tell them what you found — not a plan or outline.\n\
             Speech recognition mishears words: if part of it doesn't make sense, work out what they meant from the conversation (\"my craft\" → \"Minecraft\") instead of taking it literally. \
             If they correct you (\"no, I said…\"), redo it with the correction.\n\
             Then answer. Everything you write is spoken aloud with text-to-speech, so talk like a person in a call: \
             one to three short sentences, plain words — no markdown, emojis, links, or IDs/long numbers (say \"sent it to scratchpad3\", not the message ID). \
             If something really needs to be read (a link, code, a long list), post it with send_message (channel \"here\" is this call's text chat) and just say you posted it."
        );
    }
    if job.triggers.is_empty() && job.reason == Reason::Direct {
        return format!(
            "{who} says:
\"{}\"",
            job.question.as_deref().unwrap_or("")
        );
    }
    let latest_text = last
        .map(|m| m.full_text().trim().to_owned())
        .unwrap_or_default();
    match job.reason {
        Reason::Direct => format!(
            "{who} is talking to you. Their message:
\"{latest_text}\"

That quoted message — and nothing older — is what you answer now; the conversation above is only background. If it asks you to DO something (send, make, find, attach, change…), do it with a tool first, then confirm. If it is only a remark (about noise, mood, the channel), answer THAT briefly — never drift back to an older topic."
        ),
        Reason::Conversation => format!(
            "You were just talking with {who}. Their latest message:
\"{latest_text}\"

That quoted message — and nothing older — is what you answer now. If it is meant for you, reply (and do what it asks with a tool). If they're clearly talking to someone else, reply exactly NO_REPLY."
        ),
        Reason::Ambient => match job.question.as_deref() {
            Some(hint) => format!(
                "Nobody addressed you, but you noticed something (latest message: {ids}): {hint}"
            ),
            None => format!(
                "Nobody addressed you — you're just in the room. Chime in only if you'd genuinely add something (answer an open question, correct a mistake, or a remark that fits the vibe). Otherwise reply exactly NO_REPLY. (Latest message: {ids}.)"
            ),
        },
        Reason::Hq => format!(
            "{who} addressed you in the agent HQ (message {ids}). Respond to coordinate: confirm what you'll do, do it, or report back."
        ),
        Reason::Ask => format!(
            "{who} asked you privately with /ask (only they will see your answer):\n\"{}\"",
            job.question.as_deref().unwrap_or("")
        ),
        Reason::Task => {
            let Some(task) = &job.task else {
                return "Carry out your task.".to_owned();
            };
            // Who will read the reply: the post/message author, the new
            // member, or the channel — never the person who assigned it.
            let (event, audience) = match &task.trigger {
                TaskTrigger::NewPost { .. } => (
                    format!(
                        "{who} just made a new forum post (the first message above, id {ids}). Your reply is posted inside that post, where {who} reads it."
                    ),
                    who.clone(),
                ),
                TaskTrigger::NewMessage { .. } => (
                    format!("{who} just posted message {ids}. Your reply is posted right under it."),
                    who.clone(),
                ),
                TaskTrigger::GitHub { .. } => (
                    format!(
                        "Something new happened on a GitHub repo you watch — the details are below. Your reply IS the post: write it directly (don't use send_message for this channel, and don't mention sending). Use only what the details say — no made-up highlights.\n\n{}",
                        job.question.as_deref().unwrap_or("")
                    ),
                    "the people in this channel".to_owned(),
                ),
                TaskTrigger::Schedule { .. } | TaskTrigger::Once { .. } => (
                    "It's time for this scheduled task. Your reply is posted in this channel for everyone.".to_owned(),
                    "the people in this channel".to_owned(),
                ),
                TaskTrigger::Reminder { user_id, .. } => (
                    "It's time for this reminder.".to_owned(),
                    format!("<@{user_id}>"),
                ),
                TaskTrigger::MemberJoin { .. } => {
                    let member = job.question.clone().unwrap_or_else(|| "someone".to_owned());
                    (
                        format!("{member} just joined the server. Your reply is posted in this channel."),
                        member,
                    )
                }
                TaskTrigger::NewTicket { .. } => {
                    let opener = job.question.clone().unwrap_or_else(|| "someone".to_owned());
                    (
                        format!(
                            "{opener} just opened a support ticket — this channel is their private ticket, and your reply is posted in it.                              Everything they write here comes to you; keep helping them until it's solved."
                        ),
                        opener,
                    )
                }
            };
            let _ = core;
            format!(
                "You're carrying out your standing task #{id} (assigned earlier by {author}): \"{instruction}\"\n\
                 {event}\n\n\
                 Write the message the task calls for, addressed directly TO {audience} — they are the one reading it. \
                 {author} is not reading this, so don't report back to them: no \"Done!\", no describing what you did or which tools you used. \
                 For example, if the task is to remind people of the guidelines, tell {audience} specifically what to fix in their post and why. \
                 Use tools only if the task needs them (e.g. list_forum_posts to read the guidelines). \
                 If nothing needs doing this time (e.g. the post already follows the guidelines), reply exactly NO_REPLY.",
                id = task.id,
                author = task.author_name,
                instruction = task.instruction,
            )
        }
    }
}

fn author_tag(
    core: &AgentCore,
    guild_id: Option<u64>,
    message: &StoredMessage,
    cache: &mut HashMap<u64, String>,
) -> String {
    if let Some(tag) = cache.get(&message.author_id) {
        return tag.clone();
    }
    let tag = if message.author_id == core.world.my_id() {
        " (you)".to_owned()
    } else if core.hive.peer_by_user(message.author_id).is_some() {
        " (AI agent)".to_owned()
    } else if message.author_bot {
        " (bot)".to_owned()
    } else {
        guild_id
            .and_then(|g| {
                core.world.with_guild(g, |g| {
                    let member = g.members.get(&message.author_id)?;
                    let principal =
                        super::authority::Principal::for_member(g, member, PrincipalKind::Human);
                    match principal.level {
                        AccessLevel::Owner => Some(" (owner)".to_owned()),
                        AccessLevel::Admin => Some(" (admin)".to_owned()),
                        AccessLevel::Moderator => Some(" (mod)".to_owned()),
                        _ => None,
                    }
                })
            })
            .flatten()
            .unwrap_or_default()
    };
    cache.insert(message.author_id, tag.clone());
    tag
}

pub fn render_line(
    core: &AgentCore,
    guild_id: Option<u64>,
    message: &StoredMessage,
    with_id: bool,
    cache: &mut HashMap<u64, String>,
) -> String {
    let mut line = format!(
        "[{}{}] {}{}",
        clock(message.ts),
        if with_id {
            format!(" id:{}", message.id)
        } else {
            String::new()
        },
        message.author,
        author_tag(core, guild_id, message, cache)
    );
    if let (Some(author), Some(snippet)) = (&message.reply_author, &message.reply_snippet) {
        line.push_str(&format!(
            " ↪ replying to {author} (\"{}\")",
            crate::llm::truncate(snippet, 80)
        ));
    }
    line.push_str(": ");
    if message.author_id == core.world.my_id() {
        line.push_str(&strip_artifacts(&message.content));
    } else {
        line.push_str(&message.content);
    }
    for attachment in &message.attachments {
        line.push_str(&format!(
            " [{}: {}]",
            if attachment.is_image() {
                "image"
            } else {
                "file"
            },
            attachment.name
        ));
    }
    if !message.reactions.is_empty() {
        let reactions: Vec<String> = message
            .reactions
            .iter()
            .map(|(e, n)| format!("{e}×{n}"))
            .collect();
        line.push_str(&format!(" (reactions: {})", reactions.join(" ")));
    }
    line
}

/// Remove the thinking-message chrome ("-# 💭 thought for 3s", step
/// lines) from the agent's own messages, so the model never sees — and
/// never imitates — it.
pub fn strip_artifacts(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.trim_start().starts_with("-# "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches(" ▌")
        .trim()
        .to_owned()
        .pipe_signoff()
}

trait SignOff {
    fn pipe_signoff(self) -> String;
}

impl SignOff for String {
    fn pipe_signoff(self) -> String {
        super::brain::strip_emoji_signoff(&self)
    }
}

fn channel_transcript(
    core: &AgentCore,
    job: &Job,
    budget: usize,
    compact: bool,
) -> (String, String) {
    let trigger_ids: Vec<u64> = job.triggers.iter().map(|m| m.id.get()).collect();
    let mut cache = HashMap::new();
    let (summary, lines) = core.hive.knowledge.channel(job.channel_id, |archive| {
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0usize;
        let me = core.world.my_id();
        let visible: Vec<&StoredMessage> = archive
            .messages
            .iter()
            .filter(|m| !m.deleted)
            // An in-progress thinking message is not part of the talk.
            .filter(|m| !(m.author_id == me && strip_artifacts(&m.content).is_empty()))
            // Neither is status chrome: the thinking emote, step lines
            // ("✅ posting in media") — anything the app rendered, not
            // something the agent said. The model must never see (or
            // start imitating) its own progress UI.
            .filter(|m| {
                !(m.author_id == me && crate::tools::discord::is_status_content(&m.content))
            })
            .collect();
        for (index, message) in visible.iter().rev().enumerate() {
            let with_id = trigger_ids.contains(&message.id) || (!compact && index < 12);
            let mut line = render_line(core, job.guild_id, message, with_id, &mut cache);
            if line.len() > 1200 {
                line = crate::llm::truncate(&line, 1200);
            }
            if used + line.len() + 1 > budget && !lines.is_empty() {
                break;
            }
            used += line.len() + 1;
            lines.push(line);
            if compact && lines.len() >= 25 {
                break;
            }
        }
        lines.reverse();
        (archive.summary.clone(), lines)
    });
    (summary, lines.join("\n"))
}

fn playground_transcript(core: &AgentCore, budget: usize) -> String {
    let history = core
        .hive
        .live
        .playground
        .lock()
        .expect("live poisoned")
        .get(&core.id)
        .map(|p| p.messages.clone())
        .unwrap_or_default();
    let mut lines = Vec::new();
    let mut used = 0;
    for (role, text) in history.iter().rev() {
        let line = match role {
            PlaygroundRole::User => format!("Operator: {text}"),
            PlaygroundRole::Agent => format!("{} (you): {text}", core.name()),
            PlaygroundRole::Note => continue,
        };
        if used + line.len() > budget && !lines.is_empty() {
            break;
        }
        used += line.len();
        lines.push(line);
    }
    lines.reverse();
    lines.join("\n")
}
