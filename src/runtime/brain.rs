//! Thinking and replying.
//!
//! One [`Job`] = one reply decision: build the prompt, run the model with
//! tools until it produces an answer (or decides to stay quiet), and
//! deliver it. While it works, a *thinking message* is posted and kept
//! updated — tool steps, optionally the model's reasoning, and the answer
//! as it streams — then edited into the final reply. (Discord's own
//! typing indicator times out after ~10 seconds; this does not.)

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

use axis::model::{AllowedMentions, Message};
use axis::prelude::*;
use tokio::sync::watch;

use super::agent::AgentCore;
use super::authority::Principal;
use super::context;
use super::live::{FeedKind, PlaygroundRole};
use crate::config::{ThinkingStyle, ToolMode};
use crate::llm::{ChatRequest, LlmError, Msg, StreamEvent, text::parse_text_tool_calls};
use crate::memory::{Memory, StandingTask, now_ms};
use crate::tools::{self, ToolCtx};

/// Why the agent is answering. Ordered by how strongly it was addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    Ambient,
    Task,
    Conversation,
    Hq,
    Direct,
    Ask,
}

#[derive(Debug, Clone)]
pub enum Outlet {
    /// Reply in the channel (with a thinking message per the style).
    Channel,
    /// Edit a deferred interaction response (/ask).
    Interaction { application_id: u64, token: String },
    /// The local test console.
    Playground,
    /// Spoken in a voice channel.
    Voice { guild_id: u64, speaker: u64 },
}

#[derive(Debug, Clone)]
pub struct Job {
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub triggers: Vec<Message>,
    pub reason: Reason,
    pub requester: Principal,
    pub task: Option<StandingTask>,
    pub outlet: Outlet,
    /// A question that isn't a channel message (/ask, playground).
    pub question: Option<String>,
}

impl Job {
    fn reply_to(&self) -> Option<MessageId> {
        self.triggers.last().map(|m| m.id)
    }
    fn addressed(&self) -> bool {
        matches!(
            self.reason,
            Reason::Direct | Reason::Hq | Reason::Ask | Reason::Conversation
        )
    }
    /// Called on directly: show the thinking message right away.
    fn direct(&self) -> bool {
        matches!(self.reason, Reason::Direct | Reason::Hq | Reason::Ask)
    }
}

#[derive(Debug, Clone)]
struct Step {
    label: String,
    state: Option<bool>,
    /// The same step repeated back to back shows once, as "×N".
    count: u32,
    /// Reading/recalling (past chats, memory, the web) vs other work.
    reading: bool,
}

/// Tools that read: shown with the "reading" emoji.
fn is_reading_tool(name: &str) -> bool {
    matches!(
        name,
        "read_messages"
            | "search_messages"
            | "list_forum_posts"
            | "get_pinned_messages"
            | "recall"
            | "fetch_url"
            | "web_search"
            | "wikipedia"
            | "define_word"
            | "get_audit_log"
            | "search_audit_logs"
            | "list_threads"
            | "list_directives"
            | "list_tasks"
            | "list_reminders"
            | "look_at_image"
            | "image_search"
    )
}

/// What the thinking message shows.
#[derive(Debug)]
struct View {
    started: Instant,
    steps: Vec<Step>,
    reasoning: String,
    answer: String,
    version: u64,
    /// Ambient/task jobs only show a thinking message once it's clear
    /// the agent is going to say something.
    revealed: bool,
}

impl View {
    fn touch(&mut self) {
        self.version += 1;
        if !self.revealed {
            let answer = self.answer.trim_start();
            // "NO_REPLY" is 8 characters: anything longer that doesn't
            // start with it means the agent is really answering.
            let committed =
                answer.chars().count() >= 10 && !answer.to_uppercase().starts_with("NO_REPLY");
            let acting = self.steps.iter().any(|s| !s.label.starts_with("reacting"));
            if committed || acting {
                self.revealed = true;
            }
        }
    }
}

fn render_view(
    view: &View,
    style: ThinkingStyle,
    emojis: &crate::config::ThinkingEmojis,
    small: bool,
) -> String {
    // A steady, compact status — the header plus what it's doing now —
    // so the message doesn't grow and then collapse when the reply lands.
    let size = if small { "-# " } else { "" };
    // Icon only; progress lines follow below it as work happens.
    let mut out = format!("{size}{}", emojis.thinking.trim());
    if style == ThinkingStyle::Placeholder {
        return out;
    }
    if let Some(step) = view.steps.last() {
        let mark = match step.state {
            None if step.reading => emojis.reading.trim(),
            None => emojis.working.trim(),
            Some(true) => "✅",
            Some(false) => "⚠️",
        };
        let count = if step.count > 1 {
            format!(" ×{}", step.count)
        } else {
            String::new()
        };
        let progress = if view.steps.len() > 1 {
            format!(" · step {}", view.steps.len())
        } else {
            String::new()
        };
        out.push_str(&format!(
            "
{size}{mark} {}{count}{progress}",
            step.label
        ));
    }
    if style == ThinkingStyle::LiveWithReasoning && !view.reasoning.trim().is_empty() {
        // The latest line of reasoning, in place.
        if let Some(line) = view
            .reasoning
            .trim()
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
        {
            out.push_str(&format!(
                "
> {}",
                crate::llm::truncate(line.trim(), 160)
            ));
        }
    }
    crate::llm::truncate(&out, 1990)
}

pub struct Outcome {
    pub text: Option<String>,
    pub error: Option<String>,
    pub tools_used: u32,
    /// The last tool step: (label, succeeded, result) — used to explain
    /// what happened if the model goes silent after acting.
    pub last_tool: Option<(String, bool, String)>,
    /// Images to upload with the reply.
    pub files: Vec<(String, Vec<u8>)>,
}

/// Run one job end to end.
/// Run a job to the end. Returns false when the model failed (nothing
/// could be said).
pub async fn run(core: &Arc<AgentCore>, job: Job) -> bool {
    *core
        .running
        .lock()
        .expect("running poisoned")
        .entry(job.channel_id)
        .or_default() += 1;
    struct Done<'a>(&'a AgentCore, u64);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            if let Some(n) = self
                .0
                .running
                .lock()
                .expect("running poisoned")
                .get_mut(&self.1)
            {
                *n = n.saturating_sub(1);
            }
        }
    }
    let _done = Done(core, job.channel_id);
    // Follow-ups from the person this job works for join it.
    let steered = matches!(job.outlet, Outlet::Channel)
        && matches!(
            job.reason,
            Reason::Direct | Reason::Conversation | Reason::Hq
        );
    if steered {
        core.running_for
            .lock()
            .expect("running poisoned")
            .insert(job.channel_id, job.requester.user_id);
    }
    let profile = core.profile();
    let style = profile.behavior.thinking_style;
    // Agents in one channel take turns, so later ones see earlier replies.
    let floor = core.hive.floor(job.channel_id);
    let _turn = match job.outlet {
        Outlet::Channel => Some(floor.lock().await),
        _ => None,
    };
    let label = match job.outlet {
        Outlet::Playground => "console".to_owned(),
        _ => core.channel_label(job.guild_id, job.channel_id),
    };
    let about = job
        .triggers
        .last()
        .map(|m| {
            format!(
                "{}: {}",
                super::agent::display_name(m),
                crate::llm::truncate(&m.content, 80)
            )
        })
        .or_else(|| job.question.clone().map(|q| crate::llm::truncate(&q, 80)))
        .or_else(|| job.task.as_ref().map(|t| format!("task #{}", t.id)))
        .unwrap_or_default();
    let thought = core.live().begin_thought(&core.id, label.clone(), about);

    let view = Arc::new(Mutex::new(View {
        started: Instant::now(),
        steps: Vec::new(),
        reasoning: String::new(),
        answer: String::new(),
        version: 0,
        revealed: job.direct(),
    }));
    let (done_tx, done_rx) = watch::channel(false);
    let presenter = {
        let core = Arc::clone(core);
        let view = Arc::clone(&view);
        let job = job.clone();
        tokio::spawn(async move { present(core, job, view, style, thought, done_rx).await })
    };

    // Optional "working on it" reaction.
    let reaction = profile.behavior.working_reaction.trim().to_owned();
    let reacted =
        if !reaction.is_empty() && matches!(job.outlet, Outlet::Channel) && job.addressed() {
            match job.reply_to() {
                Some(id) => core
                    .http
                    .add_reaction(
                        ChannelId::new(job.channel_id),
                        id,
                        tools::discord::parse_emoji(&reaction),
                    )
                    .await
                    .is_ok(),
                None => false,
            }
        } else {
            false
        };

    let outcome = think(core, &job, &view, thought).await;
    let _ = done_tx.send(true);
    let placeholder = presenter.await.ok().flatten();
    deliver(core, &job, &outcome, placeholder, &view, style).await;

    if reacted {
        if let Some(id) = job.reply_to() {
            let _ = core
                .http
                .delete_own_reaction(
                    ChannelId::new(job.channel_id),
                    id,
                    tools::discord::parse_emoji(&reaction),
                )
                .await;
        }
    }
    core.live().end_thought(&core.id, thought);
    if outcome.text.is_some() {
        for trigger in &job.triggers {
            core.note_conversation(job.channel_id, trigger.author.id.get());
        }
    }
    if matches!(job.outlet, Outlet::Channel) {
        let core = Arc::clone(core);
        let channel = job.channel_id;
        tokio::spawn(async move { maybe_compact(&core, channel).await });
    }
    // Messages that came in too late for this job get their own turn.
    if steered {
        let mut running_for = core.running_for.lock().expect("running poisoned");
        if running_for.get(&job.channel_id) == Some(&job.requester.user_id) {
            running_for.remove(&job.channel_id);
        }
        drop(running_for);
        let leftover = core
            .steering
            .lock()
            .expect("steering poisoned")
            .remove(&job.channel_id)
            .unwrap_or_default();
        for (message, reason) in leftover {
            super::agent::enqueue(core, message, reason);
        }
    }
    outcome.error.is_none()
}

/// Keep the user informed while the agent thinks: typing indicator until
/// a thinking message exists, then periodic edits of that message.
async fn present(
    core: Arc<AgentCore>,
    job: Job,
    view: Arc<Mutex<View>>,
    style: ThinkingStyle,
    thought: u64,
    mut done: watch::Receiver<bool>,
) -> Option<MessageId> {
    let channel = ChannelId::new(job.channel_id);
    // Already showing "thinking…" (posted when the message arrived)?
    let early = if matches!(job.outlet, Outlet::Channel) {
        core.early
            .lock()
            .expect("early poisoned")
            .remove(&job.channel_id)
    } else {
        None
    };
    let mut placeholder: Option<MessageId> = match early {
        Some(slot) => *slot.lock().await,
        None => None,
    };
    let mut last_typing: Option<Instant> = None;
    let mut last_edit = Instant::now();
    let mut shown_version = 0;
    // Never post (even "thinking…") where it was told never to chat.
    let channel_outlet = matches!(job.outlet, Outlet::Channel)
        && !job
            .guild_id
            .is_some_and(|g| core.barred(g, job.channel_id).is_some());
    loop {
        // Mirror progress into the dashboard.
        let (rendered, version, revealed) = {
            let view = view.lock().expect("view poisoned");
            let reasoning = view.reasoning.clone();
            let answer = view.answer.clone();
            let steps: Vec<String> = view.steps.iter().map(|s| s.label.clone()).collect();
            core.live().update_thought(&core.id, thought, |t| {
                t.reasoning = reasoning;
                t.answer = answer;
                t.steps = steps;
                t.phase = if t.answer.is_empty() {
                    "thinking".to_owned()
                } else {
                    "writing".to_owned()
                };
            });
            (
                {
                    let behavior = core.profile().behavior;
                    render_view(&view, style, &behavior.emojis, behavior.small_thinking)
                },
                view.version,
                view.revealed,
            )
        };
        if matches!(job.outlet, Outlet::Playground) {
            let mut playground = core.live().playground.lock().expect("live poisoned");
            if let Some(entry) = playground.get_mut(&core.id) {
                entry.thought = core
                    .live()
                    .snapshot(&core.id)
                    .thoughts
                    .into_iter()
                    .find(|t| t.id == thought);
            }
        }
        if channel_outlet {
            if style.uses_placeholder() && revealed && placeholder.is_none() {
                // A plain status message, not a reply: the real reply (with
                // its ping) is sent only once the answer is ready.
                let request = core
                    .http
                    .create_message(channel)
                    .content(rendered.clone())
                    .allowed_mentions(AllowedMentions::none());
                match request.await {
                    Ok(message) => {
                        placeholder = Some(message.id);
                        shown_version = version;
                        last_edit = Instant::now();
                        // Posting a message clears the typing indicator.
                        last_typing = None;
                    }
                    Err(error) => tracing::debug!(%error, "could not post thinking message"),
                }
            } else if placeholder.is_some()
                && matches!(
                    style,
                    ThinkingStyle::Live
                        | ThinkingStyle::LiveWithReasoning
                        | ThinkingStyle::Placeholder
                )
                && last_edit.elapsed() >= Duration::from_millis(1400)
                && (version != shown_version
                    || style == ThinkingStyle::Placeholder
                        && last_edit.elapsed() >= Duration::from_secs(5))
            {
                if let Some(id) = placeholder {
                    let _ = core
                        .http
                        .edit_message(channel, id)
                        .content(rendered.clone())
                        .allowed_mentions(AllowedMentions::none())
                        .await;
                }
                shown_version = version;
                last_edit = Instant::now();
            }
            // Keep "… is typing" showing for the whole time the agent works.
            if (revealed || placeholder.is_some())
                && last_typing.is_none_or(|t| t.elapsed() >= Duration::from_secs(8))
            {
                let _ = core.http.trigger_typing(channel).await;
                last_typing = Some(Instant::now());
            }
        } else if let Outlet::Interaction {
            application_id,
            token,
        } = &job.outlet
        {
            if matches!(
                style,
                ThinkingStyle::Live | ThinkingStyle::LiveWithReasoning
            ) && last_edit.elapsed() >= Duration::from_millis(1500)
                && version != shown_version
            {
                let _ = core
                    .http
                    .edit_followup(
                        ApplicationId::new(*application_id),
                        token.clone(),
                        axis::rest::builders::ORIGINAL_RESPONSE,
                        axis::rest::builders::FollowupEditFields::default()
                            .content(rendered.clone()),
                    )
                    .await;
                shown_version = version;
                last_edit = Instant::now();
            }
        }
        tokio::select! {
            _ = done.changed() => break,
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
    }
    placeholder
}

async fn retrieve_memories(core: &Arc<AgentCore>, job: &Job) -> Vec<Memory> {
    let query: String = job
        .triggers
        .iter()
        .map(|m| m.content.as_str())
        .chain(job.question.as_deref())
        .chain(job.task.as_ref().map(|t| t.instruction.as_str()))
        .collect::<Vec<_>>()
        .join(" ");
    let participants: Vec<u64> = job
        .triggers
        .iter()
        .map(|m| m.author.id.get())
        .chain(std::iter::once(job.requester.user_id))
        .collect();
    let embedding = core.embed_query(&query).await;
    let limit = if context::is_compact(core) { 4 } else { 8 };
    core.memory.search(
        &query,
        &participants,
        job.guild_id,
        embedding.as_deref(),
        limit,
    )
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

async fn think(core: &Arc<AgentCore>, job: &Job, view: &Arc<Mutex<View>>, thought: u64) -> Outcome {
    let profile = core.profile();
    let endpoint = core.endpoint();
    let memories = retrieve_memories(core, job).await;
    let sight = super::vision::look(core, &job.triggers).await;
    let images = sight.images;
    let image_notes = sight.notes;
    let mut shrink = 1.0f32;
    let mut prompt = context::build(core, job, shrink, &memories, &images, &image_notes);
    if matches!(job.outlet, Outlet::Voice { .. }) {
        let offered: Vec<&str> = prompt.tools.iter().map(|t| t.name.as_str()).collect();
        tracing::info!(agent = %core.id, request = job.question.as_deref().unwrap_or(""), ?offered, text_tools = prompt.text_tools, "voice turn");
    }
    let media: Arc<Mutex<tools::images::Media>> = Arc::default();
    let mut convo: Vec<Msg> = Vec::new();
    let ctx = ToolCtx {
        core: Arc::clone(core),
        guild_id: job.guild_id,
        channel_id: job.channel_id,
        message_id: job.reply_to().map(|id| id.get()),
        requester: job.requester.clone(),
        autonomous: job.reason == Reason::Ambient,
        replies_here: matches!(job.outlet, Outlet::Channel),
        media: Arc::clone(&media),
        inputs: super::vision::sources(core, &job.triggers),
    };
    let mut max_rounds = profile.behavior.max_tool_rounds.clamp(1, 24);
    // A plan for big jobs: steps done one at a time; finished steps shrink
    // to their result so long jobs fit the context.
    let mut plan: Vec<PlanStep> = Vec::new();
    let mut step_start = 0usize;
    let mut step_nudges = 0u32;
    let job_started = crate::memory::now_ms();
    let mut round = 0u32;
    let mut tools_used = 0u32;
    let mut repeats: HashMap<String, u32> = HashMap::new();
    // In a call: what the agent already said it's doing (no silent pauses,
    // no repeating itself).
    let mut said_progress: std::collections::HashSet<&'static str> = Default::default();
    let mut pushed_follow_through = false;
    // Asked once for a reply after tools ran but the model said nothing.
    let mut pushed_summary = false;
    // An answer cut off by the output limit, continued in later rounds.
    let mut written = String::new();
    let mut continuations = 0u32;
    // Set when a thinking model spent its whole budget reasoning and
    // never answered: the retry turns thinking off.
    let mut think_off = false;
    let mut room_retries = 0u32;
    let mut out_budget = prompt.max_output;
    let mut tool_uses: HashMap<String, u32> = HashMap::new();
    let mut count_nudges = 0u32;
    // Calls that worked, per tool (what counted jobs count).
    let mut ok_uses: HashMap<String, u32> = HashMap::new();
    // Tools the agent loaded mid-job (load_tools).
    let mut loaded: Vec<crate::llm::ToolSpec> = Vec::new();
    // "Duplicate this 10 times": that many calls of one tool are expected.
    let requested = request_text(job).map(|t| requested_count(&t)).unwrap_or(0);
    if requested > 3 {
        // Room for every call (plus pauses) in a counted job.
        max_rounds = max_rounds.max((requested * 3 + 6).min(80));
    }
    let mut last_tool: Option<(String, bool, String)> = None;
    let mut retried = 0;

    loop {
        // "Stop" from the person: wrap up with what's done.
        let stopped = core
            .interrupts
            .lock()
            .expect("interrupts poisoned")
            .get(&job.channel_id)
            .is_some_and(|t| *t >= job_started);
        if stopped {
            core.interrupts
                .lock()
                .expect("interrupts poisoned")
                .remove(&job.channel_id);
            let done: Vec<String> = plan
                .iter()
                .filter(|s| s.done)
                .map(|s| format!("✓ {}", s.text))
                .collect();
            return Outcome {
                text: Some(if done.is_empty() {
                    "Okay, stopped.".to_owned()
                } else {
                    format!("Okay, stopped. Done so far:\n{}", done.join("\n"))
                }),
                error: None,
                tools_used,
                last_tool,
                files: media.lock().expect("media poisoned").files(),
            };
        }
        // New messages from the person, mid-job: show them and adjust.
        let arrived: Vec<(Message, Reason)> = core
            .steering
            .lock()
            .expect("steering poisoned")
            .remove(&job.channel_id)
            .unwrap_or_default();
        if !arrived.is_empty() {
            for (message, _) in &arrived {
                convo.push(Msg::user(format!(
                    "[New message from {} while you work: \"{}\" — take it into account now: it may correct what you're doing or change the order. Continue the job with that in mind.]",
                    super::agent::display_name(message),
                    message.full_text()
                )));
            }
            max_rounds = max_rounds.max(round + 4);
        }
        let last_round = round >= max_rounds;
        let mut messages = prompt.messages.clone();
        messages.extend(convo.iter().cloned());
        if last_round {
            messages.push(Msg::user(if plan.iter().any(|s| !s.done) {
                "You've used all your tool steps. Write your final reply now, without calling tools: say what you finished and which steps are left."
            } else {
                "You've used all your tool steps. Write your final reply now, without calling tools."
            }));
        }
        let tools = if last_round {
            Vec::new()
        } else {
            let mut tools = prompt.tools.clone();
            tools.extend(
                loaded
                    .iter()
                    .filter(|l| !tools.iter().any(|t| t.name == l.name))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            tools
        };
        let request = ChatRequest {
            thinking_budget: if think_off {
                Some(0)
            } else {
                prompt.thinking_budget
            },
            model: &profile.model.model,
            messages: &messages,
            tools: &tools,
            temperature: profile.model.temperature,
            max_tokens: out_budget.max(prompt.max_output),
            stream: profile.model.streaming,
            json: false,
            reasoning: if think_off {
                Some("none")
            } else {
                match job.reason {
                    // Small talk in a call stays fast; a spoken request
                    // ("send…", "search…") gets the same thinking as a
                    // typed one — small models skip tools without it.
                    _ if matches!(job.outlet, Outlet::Voice { .. }) => {
                        if job.question.as_deref().is_some_and(looks_like_request) {
                            profile.model.thinking.replies.effort()
                        } else {
                            profile.model.thinking.voice.effort()
                        }
                    }
                    Reason::Ambient => profile.model.thinking.chiming_in.effort(),
                    Reason::Task => profile.model.thinking.tasks.effort(),
                    _ => profile.model.thinking.replies.effort(),
                }
            },
        };
        {
            let mut v = view.lock().expect("view poisoned");
            v.answer.clear();
            v.touch();
        }
        let permit = core.hive.endpoint_permit(&endpoint.limit_key()).await;
        let mut on_event = |event: StreamEvent<'_>| {
            let mut v = view.lock().expect("view poisoned");
            match event {
                StreamEvent::Content(text) => v.answer.push_str(text),
                StreamEvent::Reasoning(text) => v.reasoning.push_str(text),
                // A tool call is coming: the agent is acting, so it's
                // worth showing the thinking message.
                StreamEvent::ToolCall(name) => {
                    if !name.starts_with("react") {
                        v.revealed = true;
                    }
                }
            }
            v.touch();
        };
        let result = endpoint.chat(&request, &mut on_event).await;
        drop(permit);
        core.live().stat(&core.id, |s| s.llm_calls += 1);
        let output = match result {
            Ok(output) => output,
            Err(LlmError::ContextOverflow(message)) if shrink > 0.25 => {
                // The server says how much it really has ("available context
                // size (8192 tokens)" — e.g. the model was just loaded with
                // a smaller context): use that; otherwise shrink.
                let window = core.context_window();
                match available_context(&message) {
                    Some(real) if real < window => {
                        core.model_info
                            .write()
                            .expect("model info poisoned")
                            .context_window = Some(real);
                    }
                    _ => {
                        shrink *= 0.6;
                        core.model_info
                            .write()
                            .expect("model info poisoned")
                            .context_window = Some(((window as f32) * 0.75) as u32);
                    }
                }
                prompt = context::build(core, job, shrink, &memories, &images, &image_notes);
                for message in &mut convo {
                    if message.text().len() > 600 {
                        message.parts = vec![crate::llm::Part::Text(crate::llm::truncate(
                            &message.text(),
                            600,
                        ))];
                    }
                }
                core.feed(
                    FeedKind::System,
                    "context window full — compacting and retrying",
                    None,
                );
                continue;
            }
            Err(LlmError::ToolsUnsupported(_))
                if !prompt.text_tools
                    && profile.model.tool_mode != ToolMode::Native
                    && !prompt.tools.is_empty() =>
            {
                core.text_tools.store(true, Ordering::Relaxed);
                core.feed(
                    FeedKind::System,
                    "model has no native tool support — switching to the text tool protocol",
                    None,
                );
                prompt = context::build(core, job, shrink, &memories, &images, &image_notes);
                continue;
            }
            Err(LlmError::VisionUnsupported(_)) if !core.no_vision.load(Ordering::Relaxed) => {
                core.no_vision.store(true, Ordering::Relaxed);
                prompt = context::build(core, job, shrink, &memories, &[], &image_notes);
                continue;
            }
            Err(error) if error.is_retryable() && retried < 2 => {
                retried += 1;
                let wait = match &error {
                    LlmError::RateLimited(_, Some(wait)) => (*wait).min(Duration::from_secs(30)),
                    _ => Duration::from_secs(2 * retried),
                };
                core.live().update_thought(&core.id, thought, |t| {
                    t.phase = format!("retrying ({error})")
                });
                tokio::time::sleep(wait).await;
                continue;
            }
            Err(error) => {
                core.live().stat(&core.id, |s| s.errors += 1);
                core.live()
                    .agent(&core.id, |l| l.last_error = Some(error.to_string()));
                return Outcome {
                    text: None,
                    error: Some(error.to_string()),
                    tools_used,
                    last_tool,
                    files: media.lock().expect("media poisoned").files(),
                };
            }
        };
        if let Some(usage) = &output.usage {
            core.live().stat(&core.id, |s| {
                s.tokens_in += usage.prompt_tokens as u64;
                s.tokens_out += usage.completion_tokens as u64;
            });
            if usage.prompt_tokens > 200 {
                let sent: usize = messages.iter().map(Msg::cost_chars).sum::<usize>()
                    + tools
                        .iter()
                        .map(|t| t.description.len() + t.parameters.to_string().len() + 40)
                        .sum::<usize>();
                let observed = (sent as f32 / usage.prompt_tokens as f32).clamp(2.0, 6.0);
                let mut cpt = core.chars_per_token.lock().expect("calibration poisoned");
                *cpt = *cpt * 0.7 + observed * 0.3;
            }
        }

        // A thinking model that hit the token limit while still reasoning
        // produced nothing. Give it more room (remembered for next time —
        // some models always think), and ask again with thinking off.
        if output.content.trim().is_empty()
            && output.tool_calls.is_empty()
            && output.finish_reason.as_deref() == Some("length")
            && room_retries < 3
        {
            room_retries += 1;
            let cap = (core.context_window() / 2).max(512);
            let room = core
                .thinking_room
                .load(std::sync::atomic::Ordering::Relaxed);
            let grown = (room.max(prompt.max_output / 2) * 2)
                .min(cap.saturating_sub(profile.model.max_output_tokens.min(cap)));
            core.thinking_room
                .store(grown, std::sync::atomic::Ordering::Relaxed);
            out_budget = (out_budget * 2).min(cap).max(out_budget);
            think_off = true;
            core.feed(
                FeedKind::System,
                format!("the model ran out of room while thinking — answering again with room for {out_budget} tokens"),
                None,
            );
            continue;
        }
        let (calls, content) = if prompt.text_tools {
            parse_text_tool_calls(&output.content)
        } else {
            (output.tool_calls.clone(), output.content.clone())
        };
        // "Never chat in #x" / "you can chat in #x again": a rule tool,
        // when it's on offer.
        let rule_tool = request_text(job)
            .filter(|_| job.task.is_none())
            .and_then(|t| {
                channel_rule_request(&t)
                    .or_else(|| github_request(&t))
                    .or_else(|| schedule_request(&t))
            })
            .filter(|tool| prompt.text_tools || prompt.tools.iter().any(|t| t.name == *tool));
        // Small models love "Want me to check X?" instead of checking X.
        // When it offers something it could just do, make it do it.
        if calls.is_empty()
            && !last_round
            && !pushed_follow_through
            && (!prompt.tools.is_empty() || prompt.text_tools)
            && matches!(
                job.reason,
                Reason::Direct | Reason::Hq | Reason::Task | Reason::Ask | Reason::Conversation
            )
            // Only before anything was done: after real work, a closing
            // "want me to dig deeper?" is fine.
            && tools_used == 0
            && (offers_instead_of_doing(&content)
                || claims_action(&content)
                || rule_tool.is_some()
                || (asks_for_details(&content) && request_text(job).is_some_and(|t| looks_like_request(&t))))
        {
            pushed_follow_through = true;
            convo.push(Msg::assistant(content.clone()));
            let hint = match rule_tool {
                Some("create_task") => "This is a scheduled job: call create_task now — watch \"schedule\" with at (time of day) and days if they gave them, or every_minutes for \"every N minutes/hours\"; watch \"once\" for a one-time thing. ".to_owned(),
                Some("set_reminder") => "This is a reminder: call set_reminder now. ".to_owned(),
                Some("watch_github") => "This is about watching a GitHub repo: call watch_github now with the repo they gave. ".to_owned(),
                Some("stop_watching_github") => "Call stop_watching_github now with that repo. ".to_owned(),
                Some("list_tasks") => "Call list_tasks now and tell them what you're watching and doing (repos, schedules, tasks). ".to_owned(),
                Some(tool) => {
                    let text = request_text(job).unwrap_or_default();
                    let names = job.guild_id.map(|g| named_channels(core, g, &text)).unwrap_or_default();
                    let which = if !names.is_empty() {
                        format!("channels \"{}\"", names.join(", "))
                    } else {
                        "the channels they named (\"this\" only if they meant this channel)".to_owned()
                    };
                    format!("This is a rule about where you may chat: call {tool} now with {which}. ")
                }
                None => String::new(),
            };
            convo.push(Msg::user(format!(
                "{hint}Don't ask or just say it — actually do it now by calling the right tool, then write your final answer with the actual result. \
                 Take any details from what was already said in the conversation, or pick a sensible default and mention it. \
                 (Your previous draft will not be shown to anyone.)",
            )));
            round += 1;
            continue;
        }
        // "Every 2 hours check the news": schedule it, don't just do it
        // once now.
        if rule_tool == Some("create_task")
            && !calls.is_empty()
            && !pushed_follow_through
            && tools_used == 0
            && !last_round
            && !calls.iter().any(|c| {
                matches!(
                    c.name.as_str(),
                    "create_task" | "set_reminder" | "make_plan"
                )
            })
        {
            pushed_follow_through = true;
            convo.push(Msg::assistant(content.clone()));
            convo.push(Msg::user(
                "They asked for this on a schedule. Set it up first with create_task (watch \"schedule\" with every_minutes, or at/days; \"once\" for a single later time) — don't just do it once now. Then reply.",
            ));
            round += 1;
            continue;
        }
        // Tools ran, then nothing was said: ask for the reply itself rather
        // than falling back to reading out a raw tool result.
        if calls.is_empty()
            && !last_round
            && !pushed_summary
            && tools_used > 0
            && content.trim().trim_start_matches("CHAT:").trim().is_empty()
        {
            pushed_summary = true;
            convo.push(Msg::assistant(content.clone()));
            convo.push(Msg::user(
                "Now write your reply to them: briefly say what you did or found, in your own words.",
            ));
            round += 1;
            continue;
        }
        if calls.is_empty() && !last_round && step_nudges < 3 {
            if let Some((index, step)) = plan.iter().enumerate().find(|(_, s)| !s.done) {
                step_nudges += 1;
                convo.push(Msg::assistant(content.clone()));
                convo.push(Msg::user(format!(
                    "Not done yet — step {} of {} is next: {}. Do it now with your tools, then call step_done. \
                     Keep going until every step is done (if one truly can't be done, call step_done saying why and move on).",
                    index + 1,
                    plan.len(),
                    step.text
                )));
                round += 1;
                continue;
            }
        }
        // Cut off by the output limit mid-answer: keep writing from where it
        // stopped instead of sending half an answer.
        if calls.is_empty()
            && continuations < 4
            && output.finish_reason.as_deref() == Some("length")
            && !content.trim().is_empty()
        {
            continuations += 1;
            written = join_continued(&written, &content);
            out_budget = (out_budget * 2)
                .min((core.context_window() / 2).max(512))
                .max(out_budget);
            core.feed(
                FeedKind::System,
                "the answer hit the length limit — continuing it",
                None,
            );
            convo.push(Msg::assistant(content.clone()));
            convo.push(Msg::user(
                "You were cut off by the length limit. Continue exactly where you stopped — no repeating, no preamble, just the rest.",
            ));
            continue;
        }
        let content = if written.is_empty() {
            content
        } else {
            join_continued(&written, &content)
        };
        // A counted job ending short ("delete 10", 9 done): finish it first.
        if calls.is_empty() && !last_round && count_nudges < 3 && requested >= 3 {
            let short = tool_uses
                .iter()
                .map(|(name, tried)| (name, *tried, ok_uses.get(name).copied().unwrap_or(0)))
                .filter(|(name, tried, worked)| {
                    *tried >= 2
                        && *worked < requested
                        && !is_work_tool(name)
                        && !matches!(name.as_str(), "make_plan" | "step_done" | "load_tools")
                })
                .map(|(name, _, worked)| {
                    format!("{name} has worked {worked} of the {requested} times asked")
                })
                .collect::<Vec<_>>();
            if !short.is_empty() {
                count_nudges += 1;
                convo.push(Msg::assistant(content.clone()));
                convo.push(Msg::user(format!(
                    "Not finished: {}. Do the rest now (same pacing as before), then reply — unless it truly can't be done, then say exactly what's left.",
                    short.join("; ")
                )));
                round += 1;
                continue;
            }
        }
        if calls.is_empty() || last_round {
            // Pushed to act and still only claimed it: don't pass that on.
            let content = if calls.is_empty()
                && pushed_follow_through
                && tools_used == 0
                && claims_action(&content)
            {
                tracing::warn!(agent = %core.id, draft = %content, "claimed an action without doing it");
                "Sorry — I didn't actually manage to do that. Can you ask me again?".to_owned()
            } else {
                content
            };
            return Outcome {
                text: Some(content),
                error: None,
                tools_used,
                last_tool,
                files: media.lock().expect("media poisoned").files(),
            };
        }
        if prompt.text_tools {
            convo.push(Msg::assistant(output.content.clone()));
        } else {
            // Only the calls that will actually receive results may enter
            // the history — strict servers reject orphaned tool_calls.
            let executed: Vec<_> = calls.iter().take(8).cloned().collect();
            convo.push(Msg::assistant_with_tools(content.clone(), executed));
        }
        let mut text_results = Vec::new();
        let mut posted_notices: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for call in calls.iter().take(8) {
            let key = format!("{}|{}", call.name, call.arguments);
            let count = {
                let entry = repeats.entry(key).or_insert(0);
                *entry += 1;
                *entry
            };
            let label = match call.name.as_str() {
                "make_plan" | "step_done" => {
                    let next = plan
                        .iter()
                        .position(|s| !s.done)
                        .map(|i| i + 1 + usize::from(call.name == "step_done"));
                    match (call.name.as_str(), next) {
                        ("make_plan", _) => "📋 planning the steps".to_owned(),
                        (_, Some(n)) if n <= plan.len() => format!("📋 step {n} of {}", plan.len()),
                        _ => "📋 wrapping up".to_owned(),
                    }
                }
                _ => humanize_ids(
                    core,
                    job.guild_id,
                    &tools::step_label(&call.name, &call.arguments),
                ),
            };
            {
                let mut v = view.lock().expect("view poisoned");
                match v.steps.last_mut() {
                    Some(last) if last.label == label => {
                        last.count += 1;
                        last.state = None;
                    }
                    _ => v.steps.push(Step {
                        label: label.clone(),
                        state: None,
                        count: 1,
                        reading: is_reading_tool(&call.name),
                    }),
                }
                v.touch();
            }
            if let Outlet::Voice { guild_id, .. } = job.outlet {
                if let Some(line) = super::voice::progress_line(&call.name) {
                    if said_progress.insert(line) {
                        let core = Arc::clone(core);
                        tokio::spawn(async move {
                            let _ = super::voice::say_quietly(&core, guild_id, line).await;
                        });
                    }
                }
            }
            let uses = {
                let entry = tool_uses.entry(call.name.clone()).or_insert(0);
                *entry += 1;
                *entry
            };
            let plan_result = match call.name.as_str() {
                "load_tools" => {
                    let what = call
                        .arguments
                        .get("what")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let have: Vec<String> = prompt
                        .tools
                        .iter()
                        .chain(loaded.iter())
                        .map(|t| t.name.clone())
                        .collect();
                    let (found, blocked) = tools::find_tools(
                        core,
                        &job.requester,
                        job.guild_id.is_some(),
                        what,
                        &have,
                    );
                    let mut text = if found.is_empty() {
                        format!("no other tool matches \"{what}\"")
                    } else {
                        format!(
                            "added — call them now: {}",
                            found
                                .iter()
                                .map(|t| format!(
                                    "{} ({})",
                                    t.name,
                                    crate::llm::truncate(&t.description, 90)
                                ))
                                .collect::<Vec<_>>()
                                .join("; ")
                        )
                    };
                    if !blocked.is_empty() {
                        text.push_str(&format!(
                            "\n{} isn't allowed to use: {} — tell them they don't have permission for that (not that you lack the tool).",
                            job.requester.name,
                            blocked.join(", ")
                        ));
                    }
                    loaded.extend(found);
                    Some(tools::ToolOutcome {
                        text,
                        ok: true,
                        notice: None,
                    })
                }
                "make_plan" => {
                    let text = call
                        .arguments
                        .get("steps")
                        .map(|v| match v {
                            serde_json::Value::Array(items) => items
                                .iter()
                                .filter_map(|i| i.as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                            other => other.as_str().unwrap_or_default().to_owned(),
                        })
                        .unwrap_or_default();
                    plan = parse_steps(&text);
                    if plan.is_empty() {
                        Some(tools::ToolOutcome {
                            text: "error: list the steps, one per line".to_owned(),
                            ok: false,
                            notice: None,
                        })
                    } else {
                        max_rounds = max_rounds.max((plan.len() as u32 * 5 + 4).min(80));
                        step_nudges = 0;
                        Some(tools::ToolOutcome {
                            text: format!(
                                "Plan saved ({} steps). Do step 1 now: {}. Call step_done after each step.",
                                plan.len(),
                                plan[0].text
                            ),
                            ok: true,
                            notice: None,
                        })
                    }
                }
                "step_done" => {
                    let result = call
                        .arguments
                        .get("result")
                        .and_then(|v| v.as_str())
                        .unwrap_or("done")
                        .to_owned();
                    match plan.iter_mut().find(|s| !s.done) {
                        Some(step) => {
                            step.done = true;
                            step.result = crate::llm::truncate(&result, 600);
                            step_nudges = 0;
                            let next = plan.iter().enumerate().find(|(_, s)| !s.done);
                            Some(tools::ToolOutcome {
                                text: match next {
                                    Some((i, s)) => format!("Noted. Next, step {} of {}: {}", i + 1, plan.len(), s.text),
                                    None => "All steps are done — write your final reply: what you did and the results.".to_owned(),
                                },
                                ok: true,
                                notice: None,
                            })
                        }
                        None => Some(tools::ToolOutcome {
                            text: "There's no unfinished step (make a plan first, or just answer)."
                                .to_owned(),
                            ok: true,
                            notice: None,
                        }),
                    }
                }
                _ => None,
            };
            let outcome = if let Some(outcome) = plan_result {
                outcome
            } else if count > 2 && count > requested && call.name != "wait" {
                tools::ToolOutcome {
                    text: "error: you already made this exact call; use the earlier result or try something else".to_owned(),
                    ok: false, notice: None }
            } else if uses > 5 && uses > requested + 2 && !is_work_tool(&call.name) {
                tools::ToolOutcome {
                    text: format!(
                        "error: you've used {} {uses} times already — stop and answer with what you have",
                        call.name
                    ),
                    ok: false,
                    notice: None,
                }
            } else {
                let mut outcome = tools::execute(&ctx, call).await;
                // Counted jobs: keep score of what worked, so "10 times"
                // means 10.
                if outcome.ok {
                    *ok_uses.entry(call.name.clone()).or_insert(0) += 1;
                }
                if requested >= 3 && !is_work_tool(&call.name) {
                    let done = ok_uses.get(&call.name).copied().unwrap_or(0);
                    outcome.text.push_str(&format!(
                        "
({} has worked {done} time(s) so far — they asked for {requested}{})",
                        call.name,
                        if done >= requested {
                            "; that's all of them"
                        } else {
                            ""
                        }
                    ));
                }
                if uses >= 3 && uses >= requested && !is_work_tool(&call.name) {
                    outcome.text.push_str(&format!(
                        "\n\n(Note: that's call #{uses} of {} — you probably have enough to answer now.)",
                        call.name
                    ));
                }
                outcome
            };
            last_tool = Some((label.clone(), outcome.ok, outcome.text.clone()));
            tools_used += 1;
            {
                let mut v = view.lock().expect("view poisoned");
                if let Some(step) = v.steps.last_mut() {
                    step.state = Some(outcome.ok);
                }
                v.touch();
            }
            core.live().stat(&core.id, |s| s.tool_calls += 1);
            if !outcome.ok {
                core.feed(
                    FeedKind::Tool,
                    format!("{label} → {}", crate::llm::truncate(&outcome.text, 140)),
                    None,
                );
            }
            // AAv2: permission denials are posted by the app, not narrated
            // by the model — one fixed message, deduped per job.
            if let Some(notice) = &outcome.notice {
                if posted_notices.insert(notice.clone()) {
                    let channel = job.channel_id;
                    let _ = core
                        .http
                        .raw(
                            "POST",
                            &format!("/channels/{channel}/messages"),
                            Some(serde_json::json!({
                                "content": notice,
                                "allowed_mentions": { "parse": [] },
                                "flags": 0
                            })),
                            None,
                        )
                        .await;
                }
            }
            if prompt.text_tools {
                text_results.push(format!("TOOL RESULT ({}): {}", call.name, outcome.text));
            } else {
                convo.push(Msg::tool_result(call.id.clone(), outcome.text));
            }
        }
        if prompt.text_tools {
            convo.push(Msg::user(text_results.join("\n\n")));
        }
        let finished_step = calls.iter().any(|c| c.name == "step_done");
        if finished_step && !plan.is_empty() && step_start <= convo.len() {
            convo.truncate(step_start);
            let progress: Vec<String> = plan
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    if s.done {
                        format!("✓ {}. {} — {}", i + 1, s.text, s.result)
                    } else {
                        format!("☐ {}. {}", i + 1, s.text)
                    }
                })
                .collect();
            let next = plan.iter().enumerate().find(|(_, s)| !s.done);
            let drafts = core.docs.drafts_since(job_started);
            convo.push(Msg::user(format!(
                "Plan progress:\n{}{}\n\n{}",
                progress.join("\n"),
                if drafts.is_empty() {
                    String::new()
                } else {
                    format!("\n\nYour drafts (continue these — don't start new ones for the same file):\n{}", drafts.join("\n"))
                },
                match next {
                    Some((i, s)) => format!("Do step {} now: {}", i + 1, s.text),
                    None => "Every step is done — write your final reply: what you did and the results.".to_owned(),
                }
            )));
            step_start = convo.len();
        } else if calls.iter().any(|c| c.name == "make_plan") {
            // Steps start after this round (its call and result stay paired).
            step_start = convo.len();
        }
        round += 1;
    }
}

/// Glue a continued answer onto the part written before the cut.
pub fn join_continued(before: &str, after: &str) -> String {
    if before.is_empty() {
        return after.to_owned();
    }
    let needs_space = !before.ends_with(char::is_whitespace)
        && after.starts_with(|c: char| c.is_alphanumeric() || c == '*' || c == '(')
        && before.ends_with(|c: char| {
            c.is_alphanumeric() || matches!(c, '.' | ',' | ':' | ';' | '!' | '?' | ')' | '*')
        });
    // The model sometimes repeats the last few words; drop the overlap.
    let tail: String = before
        .chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let trimmed = after.trim_start();
    let mut skip = 0;
    for len in (8..=tail.len().min(trimmed.len())).rev() {
        if tail.is_char_boundary(tail.len() - len)
            && trimmed.is_char_boundary(len)
            && tail[tail.len() - len..].trim_start() == trimmed[..len].trim_start()
            && !trimmed[..len].trim().is_empty()
        {
            skip = len;
            break;
        }
    }
    let rest = if skip > 0 { &trimmed[skip..] } else { after };
    format!(
        "{before}{}{rest}",
        if needs_space && skip == 0 { " " } else { "" }
    )
}

/// Did they ask for the whole thing at once (no page buttons)?
pub fn wants_everything(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "all at once",
        "at once",
        "no pages",
        "don't page",
        "dont page",
        "without pages",
        "whole thing",
        "in full",
        "full list",
        "send it all",
        "everything in one",
        "no paging",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// Split a long answer into pages of about a screenful, at paragraph or
/// line breaks (never mid-code-block when avoidable).
pub fn paginate(text: &str) -> Vec<String> {
    const PAGE: usize = 1_400;
    let mut pages = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    for line in text.lines() {
        let toggles = line.trim_start().starts_with("```");
        let would = current.len() + line.len() + 1;
        // Break at a blank line or between items once a page is full; force
        // a break well before Discord's limit.
        let soft = would > PAGE
            && !in_code
            && (line.trim().is_empty()
                || line.trim_start().starts_with(['-', '*', '•', '#'])
                || line
                    .trim_start()
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit()));
        let hard = would > 1_850;
        if (soft || hard) && !current.trim().is_empty() {
            if in_code {
                current.push_str("```");
            }
            pages.push(current.trim_end().to_owned());
            current = if in_code {
                "```\n".to_owned()
            } else {
                String::new()
            };
        }
        if toggles {
            in_code = !in_code;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        pages.push(current.trim_end().to_owned());
    }
    // A footer line (-# thought for…) stays on every page.
    if pages.len() > 1 {
        if let Some(footer) = pages
            .last()
            .and_then(|p| p.lines().last())
            .filter(|l| l.starts_with("-# "))
            .map(str::to_owned)
        {
            let n = pages.len();
            for (i, page) in pages.iter_mut().enumerate() {
                if i + 1 < n {
                    page.push_str(&format!("\n{footer}"));
                }
            }
        }
    }
    pages
}

/// ◀ 1/3 ▶ buttons.
pub fn page_buttons(index: usize, total: usize) -> Vec<serde_json::Value> {
    vec![serde_json::json!({ "type": 1, "components": [
        { "type": 2, "style": 2, "label": "◀", "custom_id": "axis_page:prev", "disabled": index == 0 },
        { "type": 2, "style": 2, "label": format!("{}/{}", index + 1, total), "custom_id": "axis_page:noop", "disabled": true },
        { "type": 2, "style": 2, "label": "▶", "custom_id": "axis_page:next", "disabled": index + 1 >= total },
        { "type": 2, "style": 2, "label": "Show all", "custom_id": "axis_page:all" },
    ]})]
}

/// Tools meant to be called many times in one job (writing a file a
/// section at a time, reading a document, working through a plan).
fn is_work_tool(name: &str) -> bool {
    matches!(
        name,
        "draft_write"
            | "draft_edit"
            | "draft_view"
            | "read_section"
            | "find_in_document"
            | "step_done"
            | "design_rank_card"
            | "wait"
    )
}

/// The biggest count in a request ("10 times", "make 15 channels"), 0
/// when none — how many calls of one tool to expect.
pub fn requested_count(text: &str) -> u32 {
    const WORDS: [(&str, u32); 11] = [
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("eight", 8),
        ("nine", 9),
        ("ten", 10),
        ("twelve", 12),
        ("twenty", 20),
    ];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|w| {
            w.parse::<u32>()
                .ok()
                .or_else(|| WORDS.iter().find(|(n, _)| *n == w).map(|(_, v)| *v))
        })
        .filter(|n| (2..=50).contains(n))
        .max()
        .unwrap_or(0)
}

/// One step of a plan.
#[derive(Debug, Clone, Default)]
pub struct PlanStep {
    pub text: String,
    pub done: bool,
    pub result: String,
}

/// "1. find an image\n2) set the colors\n- check it" → steps.
pub fn parse_steps(text: &str) -> Vec<PlanStep> {
    text.split(['\n', ';'])
        .map(|line| {
            line.trim()
                .trim_start_matches(|c: char| {
                    c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | '•' | ' ' | '\t')
                })
                .trim()
                .to_owned()
        })
        .filter(|line| line.len() > 2)
        .take(25)
        .map(|text| PlanStep {
            text,
            ..Default::default()
        })
        .collect()
}

/// "stop", "cancel", "nevermind"… said to the agent while it works.
pub fn is_interrupt(text: &str) -> bool {
    let t = text.to_lowercase();
    let t = t.trim().trim_matches(|c: char| !c.is_alphanumeric());
    if t.len() > 40 {
        return false;
    }
    let words: Vec<&str> = t
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    ["stop", "cancel", "abort", "nevermind", "halt", "quit"]
        .iter()
        .any(|w| words.contains(w))
        || t.contains("never mind")
        || t.contains("hold on")
        || t.contains("forget it")
}

/// Run a job's thinking (tools and all) without posting anything; returns
/// the outcome and the steps it took. For experiments.
#[cfg(test)]
pub async fn think_only(core: &Arc<AgentCore>, job: &Job) -> (Outcome, Vec<String>) {
    let view = Arc::new(Mutex::new(View {
        started: Instant::now(),
        steps: Vec::new(),
        reasoning: String::new(),
        answer: String::new(),
        version: 0,
        revealed: true,
    }));
    let thought = core
        .live()
        .begin_thought(&core.id, "lab".to_owned(), String::new());
    let outcome = think(core, job, &view, thought).await;
    let steps = view
        .lock()
        .expect("view poisoned")
        .steps
        .iter()
        .map(|s| format!("{} ×{}", s.label, s.count))
        .collect();
    (outcome, steps)
}

pub fn is_no_reply(text: &str) -> bool {
    let trimmed = text
        .trim()
        .trim_matches(['*', '_', '`', '"', '.', '(', ')', '[', ']']);
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("NO_REPLY")
        || trimmed.eq_ignore_ascii_case("NO REPLY")
        || trimmed.to_uppercase().starts_with("NO_REPLY")
}

/// Remove artifacts models add: "Nova:" prefixes, mimicked timestamps,
/// a trailing NO_REPLY, runs of blank lines.
/// When someone asked directly and the model came back empty (common for
/// small models right after a tool fails), say what actually happened
/// instead of deleting the thinking message and going silent.
fn fallback_reply(job: &Job, outcome: &Outcome) -> Option<String> {
    if !matches!(job.reason, Reason::Direct | Reason::Hq | Reason::Ask) {
        return None;
    }
    let empty = outcome
        .text
        .as_deref()
        .is_none_or(|t| t.trim().is_empty() || is_no_reply(t));
    if !empty || outcome.error.is_some() {
        return None;
    }
    let (label, ok, result) = outcome.last_tool.as_ref()?;
    let result = result
        .trim_start_matches("denied: ")
        .trim_start_matches("error: ")
        .split("\n\n(Note:")
        .next()
        .unwrap_or_default()
        .trim();
    Some(if *ok {
        format!(
            "Done — {}.",
            crate::llm::truncate(result, 400).trim_end_matches('.')
        )
    } else {
        format!(
            "I couldn't finish that ({label}): {}",
            crate::llm::truncate(result, 400)
        )
    })
}

/// Replace raw channel/user IDs in a step label with names.
fn humanize_ids(core: &AgentCore, guild_id: Option<u64>, label: &str) -> String {
    let Some(guild_id) = guild_id else {
        return label.to_owned();
    };
    label
        .split(' ')
        .map(|word| {
            let Some(id) = super::world::parse_snowflake(word) else {
                return word.to_owned();
            };
            core.world
                .with_guild(guild_id, |g| {
                    g.channels
                        .get(&id)
                        .and_then(|c| c.name.clone())
                        .map(|n| format!("#{n}"))
                        .or_else(|| g.members.get(&id).map(|m| m.display_name().to_owned()))
                })
                .flatten()
                .unwrap_or_else(|| word.to_owned())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the person asked, for request detection.
fn request_text(job: &Job) -> Option<String> {
    job.question
        .clone()
        .or_else(|| job.triggers.last().map(|m| m.content.clone()))
}

/// Does a message ask the agent to do something (vs. small talk)?
pub fn looks_like_request(text: &str) -> bool {
    let text = text.to_lowercase();
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .collect();
    const VERBS: [&str; 57] = [
        "send",
        "post",
        "message",
        "dm",
        "tell",
        "search",
        "google",
        "find",
        "check",
        "look",
        "move",
        "mute",
        "unmute",
        "deafen",
        "kick",
        "ban",
        "timeout",
        "delete",
        "remove",
        "clear",
        "purge",
        "join",
        "leave",
        "remind",
        "set",
        "change",
        "make",
        "create",
        "pin",
        "react",
        "give",
        "add",
        "show",
        "read",
        "summarize",
        "list",
        "draw",
        "edit",
        "rename",
        "lock",
        "open",
        "close",
        "start",
        "stop",
        "do",
        "fix",
        "redo",
        "retry",
        "try",
        "again",
        "bring",
        "go",
        "switch",
        "put",
        "turn",
        "play",
        "get",
    ];
    words.iter().any(|w| VERBS.contains(w))
        || text.contains("look up")
        || text.contains("can you")
        || text.contains("could you")
}

/// Does a reply say it's doing (or did) something — which, with no tool
/// called, it didn't?
pub fn claims_action(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "sending",
        "i'll send",
        "i will send",
        "i've sent",
        "i sent",
        "sent it",
        "posting",
        "i'll post",
        "i've posted",
        "posted it",
        "moving you",
        "i'll move",
        "moved you",
        "deleting",
        "i'll delete",
        "i've deleted",
        "searching",
        "i'll search",
        "looking it up",
        "let me check",
        "checking now",
        "i'm leaving",
        "leaving the",
        "leaving now",
        "i'm out",
        "heading out",
        "i'll leave",
        "i left",
        "joining",
        "i'll join",
        "done —",
        "done!",
        "done.",
        "on it",
        "i'll remind",
        "reminder set",
        "i've set",
        "changing",
        "i've changed",
        "i'll do that",
        "doing it now",
        "consider it done",
        "i'll make sure",
        "from now on",
        "i've made",
        "i made",
        "i've updated",
        "updated it",
        "won't chat",
        "won't talk",
        "won't post",
        "won't reply",
        "won't respond",
        "won't speak",
        "will not chat",
        "will not talk",
        "will not post",
        "i'll stay out",
        "staying out",
        "keep my replies",
        "i'll stay quiet",
        "i can chat",
        "i can talk",
        "unblocked",
        "going quiet",
        "i'll be quiet",
        "quiet time",
        "staying quiet",
        "i'll go quiet",
    ]
    .iter()
    .any(|p| text.contains(p))
}

/// The channels (or categories) a message names, by their real names —
/// "the ticket support category", "scratchpad3 and c-md".
pub fn named_channels(core: &AgentCore, guild_id: u64, text: &str) -> Vec<String> {
    let norm = |s: &str| {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let said = format!(" {} ", norm(text));
    let wants_category = said.contains(" category ");
    let wants_voice = [" vc ", " voice "].iter().any(|w| said.contains(w));
    core.world
        .with_guild(guild_id, |g| {
            let mut hits: Vec<(String, bool)> = Vec::new();
            for c in g.channels.values() {
                let Some(name) = c.name.clone() else { continue };
                let n = norm(&name);
                if n.is_empty()
                    || !said.contains(&format!(" {n} "))
                    || (c.is_voice() && !wants_voice)
                {
                    continue;
                }
                let category = c.r#type == axis::model::ChannelType::GuildCategory;
                if !hits.iter().any(|(h, _)| norm(h) == n && !category) {
                    hits.push((name, category));
                }
            }
            // "the X category" means the category; otherwise channels win.
            if wants_category && hits.iter().any(|(_, c)| *c) {
                hits.retain(|(_, c)| *c);
            } else if hits.iter().any(|(_, c)| !*c) {
                hits.retain(|(_, c)| !*c);
            }
            hits.into_iter().map(|(n, _)| n).collect()
        })
        .unwrap_or_default()
}

/// "watch this repo for releases" → watch_github; "stop watching it" →
/// stop_watching_github.
pub fn github_request(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    // "what are you watching?" / "list the repos you follow"
    let listing = ["what", "which", "list", "show"]
        .iter()
        .any(|w| t.contains(w));
    if listing
        && [
            "watching",
            "following",
            "keeping an eye",
            "watch list",
            "repos you",
        ]
        .iter()
        .any(|w| t.contains(w))
    {
        return Some("list_tasks");
    }
    let about_repo = t.contains("github") || t.contains("repo");
    if !about_repo {
        return None;
    }
    if [
        "stop watching",
        "unwatch",
        "stop following",
        "stop posting",
        "no more",
        "stop checking",
    ]
    .iter()
    .any(|p| t.contains(p))
    {
        return Some("stop_watching_github");
    }
    let watch = [
        "watch",
        "every release",
        "each release",
        "new release",
        "every commit",
        "new commit",
        "whenever",
        "keep an eye",
        "notify",
        "let me know",
        "follow",
    ]
    .iter()
    .any(|p| t.contains(p));
    (watch && !t.trim_end().ends_with('?')).then_some("watch_github")
}

/// The context size an overflow error reports ("available context size
/// (8192 tokens)", "n_ctx: 4096", "context length of 8192").
pub fn available_context(message: &str) -> Option<u32> {
    let lower = message.to_lowercase();
    for marker in [
        "available context size",
        "n_ctx",
        "context length of",
        "context_length",
        "maximum context length is",
    ] {
        if let Some(at) = lower.find(marker) {
            let digits: String = lower[at + marker.len()..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                if n >= 512 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// "every 5 seconds", "wait 10 seconds", "pause between them" — the
/// job needs the wait tool.
pub fn timing_request(text: &str) -> bool {
    let t = text.to_lowercase();
    let words: Vec<&str> = t
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words.windows(2).any(|w| {
        w[0].chars().all(|c| c.is_ascii_digit()) && (w[1].starts_with("sec") || w[1] == "s")
    }) || [
        "wait ",
        "pause",
        "delay",
        "one by one",
        "slowly",
        "a second between",
        "seconds apart",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// "every day at 9 post …", "every 2 hours check …", "tomorrow at 3 …"
/// → create_task ("remind me …" → set_reminder).
pub fn schedule_request(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    // A question about the past isn't a job to schedule.
    let asking = t.trim_end().ends_with('?')
        && !["can you", "could you", "will you", "would you", "please"]
            .iter()
            .any(|p| t.contains(p));
    if asking {
        return None;
    }
    if t.contains("remind me") {
        return Some("set_reminder");
    }
    let repeating = [
        "every day",
        "everyday",
        "every morning",
        "every night",
        "every evening",
        "every week",
        "every hour",
        "every half hour",
        "hourly",
        "daily",
        "weekly",
        "each day",
        "each morning",
        "every monday",
        "every tuesday",
        "every wednesday",
        "every thursday",
        "every friday",
        "every saturday",
        "every sunday",
        "every weekday",
        "every weekend",
        "on weekdays",
        "on weekends",
    ]
    .iter()
    .any(|p| t.contains(p));
    let every_n = t
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(3)
        .any(|w| {
            w[0] == "every"
                && w[1].chars().all(|c| c.is_ascii_digit())
                && ["min", "hour", "hr", "day", "week"]
                    .iter()
                    .any(|u| w[2].starts_with(u))
        });
    let later = [
        "tomorrow at",
        "tonight at",
        "later today",
        "this evening at",
        "at noon",
    ]
    .iter()
    .any(|p| t.contains(p))
        || t.split_whitespace()
            .collect::<Vec<_>>()
            .windows(3)
            .any(|w| {
                w[0] == "in"
                    && w[1].chars().all(|c| c.is_ascii_digit())
                    && ["min", "hour", "hr"].iter().any(|u| w[2].starts_with(u))
            });
    (repeating || every_n || later).then_some("create_task")
}

/// "never chat in #memes" → stop_chatting_in; "you can talk in #memes
/// again" → allow_chatting_in.
pub fn channel_rule_request(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase().replace('’', "'");
    let verb = [
        "chat", "talk", "post", "speak", "reply", "respond", "message", "type",
    ]
    .iter()
    .any(|v| t.contains(v));
    let place = [" in ", " on ", "here", "channel", "category", "#"]
        .iter()
        .any(|p| t.contains(p));
    if !verb || !place {
        return None;
    }
    let never = [
        "never",
        "don't",
        "dont",
        "do not",
        "can't",
        "cant ",
        "cannot",
        "no more",
        "not allowed",
        "stay out",
        "not to ",
    ]
    .iter()
    .any(|p| t.contains(p));
    if never {
        return Some("stop_chatting_in");
    }
    let lift = [
        "can chat",
        "can talk",
        "can post",
        "can speak",
        "can reply",
        "allowed to",
        "again",
        "unblock",
        "lift",
        "start chatting",
        "start talking",
        "you may",
    ]
    .iter()
    .any(|p| t.contains(p));
    lift.then_some("allow_chatting_in")
}

/// Does the reply end by asking for details the agent could have taken
/// from the conversation or a sensible default?
pub fn asks_for_details(text: &str) -> bool {
    let last = text.trim().to_lowercase();
    if !last.ends_with('?') {
        return false;
    }
    [
        "could you clarify",
        "can you clarify",
        "could you specify",
        "can you specify",
        "did you mean",
        "do you mean",
        "which one",
        "what kind of",
        "which kind",
        "what size",
        "which size",
        "what format",
        "which channel",
        "what should",
        "what would you like",
        "any specific",
        "anything specific",
        "more details",
        "more specific",
    ]
    .iter()
    .any(|p| last.contains(p))
}

/// Does the reply end by offering to do something instead of doing it?
pub fn offers_instead_of_doing(text: &str) -> bool {
    let last = text
        .trim()
        .rsplit(['\n', '.', '!'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !last.ends_with('?') {
        return false;
    }
    [
        "want me to",
        "should i",
        "would you like me to",
        "do you want me to",
        "shall i",
        "i can check",
        "i could check",
        "want me to check",
        "should i check",
        "can i check",
        "i can look",
        "want me to look",
        "should i look",
        "want me to dig",
        "i can dig",
    ]
    .iter()
    .any(|p| last.contains(p))
}

/// Remove a generic closer line ("Anything else…?", "Let me know if…").
pub fn strip_generic_closer(text: &str) -> String {
    let trimmed = text.trim_end();
    let Some(split) = trimmed.rfind(['\n', '.', '!', '?']).and_then(|end| {
        // Find the start of the final sentence.
        let body = &trimmed[..end];
        let start = body
            .rfind(['\n', '.', '!', '?'])
            .map(|i| i + 1)
            .unwrap_or(0);
        Some(start)
    }) else {
        return trimmed.to_owned();
    };
    let last = trimmed[split..].trim().to_lowercase();
    let generic = [
        "anything else",
        "let me know if",
        "hope this helps",
        "feel free to",
        "just let me know",
        "is there anything",
        "happy to help",
    ]
    .iter()
    .any(|p| last.starts_with(p));
    if generic && !trimmed[..split].trim().is_empty() {
        trimmed[..split].trim_end().to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Did the request ask the agent to reach other people?
pub fn wants_to_reach_others(job: &Job) -> bool {
    const WORDS: &[&str] = &[
        "ping",
        "tell",
        "remind",
        "notify",
        "let them know",
        "let him know",
        "let her know",
        "mention",
        "summon",
        "alert",
        "tag",
        "call them",
        "get their attention",
        "dm",
    ];
    job.triggers
        .iter()
        .map(|m| m.content.as_str())
        .chain(job.question.as_deref())
        .chain(job.task.as_ref().map(|t| t.instruction.as_str()))
        .any(|text| {
            let lower = text.to_lowercase();
            WORDS.iter().any(|w| super::agent::mentions_word(&lower, w))
        })
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE0F | 0x200D)
}

/// Remove an emoji sign-off at the end ("…something specific? 👀").
pub fn strip_emoji_signoff(text: &str) -> String {
    let trimmed = text.trim_end();
    let kept = trimmed.trim_end_matches(|c: char| is_emoji(c) || c.is_whitespace());
    // Keep messages that are nothing but emoji (a deliberate reaction-style reply).
    if kept.trim().is_empty() {
        return trimmed.to_owned();
    }
    kept.to_owned()
}

/// Drop an opening "uvx —" / "uvx," address: the reply already targets them.
fn strip_address(text: &str, job: &Job) -> String {
    let Some(trigger) = job.triggers.last() else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    for name in [
        super::agent::display_name(trigger),
        trigger.author.username.clone(),
    ] {
        let lower = out.to_lowercase();
        let name_lower = name.to_lowercase();
        for prefix in [format!("@{name_lower}"), name_lower.clone()] {
            if let Some(rest) = lower.strip_prefix(&prefix) {
                let rest_trim = rest.trim_start();
                if let Some(first) = rest_trim.chars().next() {
                    if matches!(first, '—' | '–' | '-' | ',' | ':' | '!') {
                        let cut = out.len() - rest_trim.len() + first.len_utf8();
                        let remainder = out[cut..].trim_start();
                        if !remainder.is_empty() {
                            let mut chars = remainder.chars();
                            out = match chars.next() {
                                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                                None => String::new(),
                            };
                            return out;
                        }
                    }
                }
            }
        }
    }
    out
}

/// Does this text still carry tool-call markup?
pub fn contains_tool_markup(text: &str) -> bool {
    text.contains("<function=")
        || text.contains("<fn=")
        || text.contains("<tool_call>")
        || text.contains("</tool_call>")
}

/// Pull the human-readable text out of leaked tool markup: the value of a
/// `content` parameter when there is one, else the text with all tags
/// stripped. Never returns the raw markup.
fn salvage_markup_text(text: &str) -> String {
    for open in ["<parameter=content>", "<param=content>"] {
        if let Some(start) = text.find(open) {
            let value_start = start + open.len();
            if let Some(rel_end) = text[value_start..]
                .find("</parameter>")
                .or_else(|| text[value_start..].find("</param>"))
            {
                let salvaged = text[value_start..value_start + rel_end].trim();
                if !salvaged.is_empty() {
                    return salvaged.to_owned();
                }
            }
        }
    }
    // No content parameter: strip every tag and keep what's left.
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            other if depth == 0 => out.push(other),
            _ => {}
        }
    }
    let stripped = out.trim();
    if stripped.is_empty() {
        "...".to_owned()
    } else {
        stripped.to_owned()
    }
}

pub fn clean_reply(text: &str, name: &str) -> String {
    let mut text = text.trim().to_owned();
    // Last-ditch guard: tool-call markup must never reach the chat. If a
    // reply still carries it (a model leaking its native format), salvage
    // the `content` parameter's text; otherwise strip the tags.
    if contains_tool_markup(&text) {
        text = salvage_markup_text(&text);
    }
    // "[12:01] Nova (you):" style prefixes copied from the transcript.
    if text.starts_with('[') {
        if let Some(end) = text.find(']') {
            let inside = &text[1..end];
            let looks_like_time = end < 24
                && inside.starts_with(|c: char| c.is_ascii_digit())
                && inside.contains(':')
                && !text[end + 1..].starts_with('(');
            if looks_like_time {
                text = text[end + 1..].trim_start().to_owned();
            }
        }
    }
    let lower = text.to_lowercase();
    let name_lower = name.to_lowercase();
    for prefix in [
        format!("{name_lower} (you):"),
        format!("{name_lower}:"),
        format!("**{name_lower}**:"),
        format!("**{name_lower}:**"),
        "assistant:".to_owned(),
    ] {
        if lower.starts_with(&prefix) {
            text = text[prefix.len()..].trim_start().to_owned();
            break;
        }
    }
    text = context::strip_artifacts(&text);
    if let Some(stripped) = text.strip_suffix("NO_REPLY") {
        text = stripped.trim_end().to_owned();
    }
    text = strip_emoji_signoff(&text);
    text = strip_generic_closer(&text);
    text = strip_emoji_signoff(&text);
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text
}

/// Split text into Discord-sized messages, preferring paragraph, line,
/// then sentence boundaries, and keeping code fences balanced.
pub fn split_message(text: &str) -> Vec<String> {
    const MAX: usize = 1900;
    let mut chunks = Vec::new();
    let mut rest = text.trim().to_owned();
    let mut reopen: Option<String> = None;
    while !rest.is_empty() {
        if let Some(fence) = reopen.take() {
            rest = format!("{fence}\n{rest}");
        }
        if rest.chars().count() <= MAX {
            chunks.push(rest);
            break;
        }
        let mut cut = rest
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i <= MAX)
            .last()
            .unwrap_or(MAX.min(rest.len()));
        let window = &rest[..cut];
        for separator in ["\n\n", "\n", ". ", " "] {
            if let Some(at) = window.rfind(separator) {
                if at > MAX / 3 {
                    cut = at + separator.len();
                    break;
                }
            }
        }
        let mut chunk = rest[..cut].trim_end().to_owned();
        // Balance ``` fences across the split.
        let fences = chunk.matches("```").count();
        if fences % 2 == 1 {
            let opener = chunk
                .rfind("```")
                .map(|at| chunk[at..].lines().next().unwrap_or("```").to_owned())
                .unwrap_or_else(|| "```".to_owned());
            chunk.push_str("\n```");
            reopen = Some(opener);
        }
        chunks.push(chunk);
        rest = rest[cut..].trim_start().to_owned();
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

async fn deliver(
    core: &Arc<AgentCore>,
    job: &Job,
    outcome: &Outcome,
    placeholder: Option<MessageId>,
    view: &Arc<Mutex<View>>,
    style: ThinkingStyle,
) {
    let profile = core.profile();
    let channel = ChannelId::new(job.channel_id);
    let elapsed = view.lock().expect("view poisoned").started.elapsed();
    let label = match job.outlet {
        Outlet::Playground => "console".to_owned(),
        _ => core.channel_label(job.guild_id, job.channel_id),
    };

    let fallback = fallback_reply(job, outcome);
    let text = outcome
        .text
        .as_deref()
        .map(|t| clean_reply(t, &profile.name))
        .filter(|t| !t.trim().is_empty() && !(is_no_reply(t) && fallback.is_some()))
        .or(fallback)
        .or_else(|| (!outcome.files.is_empty()).then(String::new))
        .map(|t| strip_address(&t, job))
        .filter(|t| !is_no_reply(t) || job.reason == Reason::Ask);

    if let Some(task) = &job.task {
        if let Some(guild_id) = job.guild_id {
            let summary = match (&text, &outcome.error) {
                (Some(text), _) => crate::llm::truncate(text, 200),
                (None, Some(error)) => format!("failed: {}", crate::llm::truncate(error, 120)),
                (None, None) => "nothing needed".to_owned(),
            };
            core.hive.knowledge.guild_mut(guild_id, |k| {
                if let Some(stored) = k.tasks.iter_mut().find(|t| t.id == task.id) {
                    stored.runs += 1;
                    stored.last_run = now_ms();
                    stored.last_result = summary.clone();
                }
            });
        }
    }

    // In the text chat of a call the agent is in, SAY: lines are spoken.
    let text = match (&job.outlet, job.guild_id) {
        (Outlet::Channel, Some(guild_id))
            if core.voice.channel(guild_id) == Some(job.channel_id) =>
        {
            text.and_then(|t| {
                let (rest, said) = super::voice::split_say(&t);
                if !said.is_empty() {
                    let core = Arc::clone(core);
                    let line = said.join(" ");
                    tokio::spawn(async move {
                        if let Err(error) = super::voice::say(&core, guild_id, &line).await {
                            core.feed(FeedKind::Error, format!("couldn't speak: {error}"), None);
                        }
                    });
                }
                (!rest.trim().is_empty() || !outcome.files.is_empty()).then_some(rest)
            })
        }
        _ => text,
    };

    match &job.outlet {
        Outlet::Playground => {
            let mut playground = core.live().playground.lock().expect("live poisoned");
            let entry = playground.entry(core.id.clone()).or_default();
            entry.busy = false;
            entry.thought = None;
            match (&text, &outcome.error) {
                (Some(text), _) => entry.messages.push((PlaygroundRole::Agent, text.clone())),
                (None, Some(error)) => entry
                    .messages
                    .push((PlaygroundRole::Note, format!("⚠ {error}"))),
                (None, None) => entry
                    .messages
                    .push((PlaygroundRole::Note, "(chose not to reply)".to_owned())),
            }
            if outcome.tools_used > 0 {
                entry.messages.push((
                    PlaygroundRole::Note,
                    format!(
                        "used {} tool(s) · {:.1}s",
                        outcome.tools_used,
                        elapsed.as_secs_f32()
                    ),
                ));
            }
            core.live().repaint();
            return;
        }
        Outlet::Interaction {
            application_id,
            token,
        } => {
            let content = match (&text, &outcome.error) {
                (Some(text), _) => text.clone(),
                (None, Some(error)) => format!(
                    "⚠️ I couldn't answer that right now ({}).",
                    crate::llm::truncate(error, 200)
                ),
                (None, None) => "I don't have anything to add.".to_owned(),
            };
            let chunks = split_message(&content);
            let _ = core
                .http
                .edit_followup(
                    ApplicationId::new(*application_id),
                    token.clone(),
                    axis::rest::builders::ORIGINAL_RESPONSE,
                    axis::rest::builders::FollowupEditFields::default().content(chunks[0].clone()),
                )
                .await;
            for chunk in chunks.iter().skip(1) {
                let data = axis::model::InteractionMessageCallbackData {
                    content: Some(chunk.clone()),
                    flags: Some(MessageFlags::EPHEMERAL),
                    ..Default::default()
                };
                let _ = core
                    .http
                    .create_followup(ApplicationId::new(*application_id), token.clone(), data)
                    .await;
            }
            if text.is_some() {
                core.live().stat(&core.id, |s| s.replies += 1);
                core.feed(
                    FeedKind::Reply,
                    format!("answered /ask from {}", job.requester.name),
                    Some(label),
                );
            }
            return;
        }
        Outlet::Voice { guild_id, speaker } => {
            match (&text, &outcome.error) {
                (Some(text), _) => {
                    super::voice::reply(core, *guild_id, *speaker, text, &outcome.files).await
                }
                (None, None) if !outcome.files.is_empty() => {
                    super::voice::reply(core, *guild_id, *speaker, "Here you go.", &outcome.files)
                        .await
                }
                (None, Some(error)) => {
                    core.feed(
                        FeedKind::Error,
                        format!("couldn't answer in voice: {error}"),
                        Some(label),
                    );
                    let _ = super::voice::say(
                        core,
                        *guild_id,
                        "Sorry, I couldn't come up with an answer to that.",
                    )
                    .await;
                }
                (None, None) => {}
            }
            return;
        }
        Outlet::Channel => {}
    }

    // Told never to chat here: stay silent (but confirm the rule itself
    // when it was set by this very request).
    if let Some(rule) = job.guild_id.and_then(|g| core.barred(g, job.channel_id)) {
        let just_set = rule.created >= crate::memory::now_ms() - elapsed.as_millis() as i64 - 5_000;
        if !just_set {
            if let Some(id) = placeholder {
                let _ = core.http.delete_message(channel, id).await;
            }
            core.feed(
                FeedKind::System,
                format!(
                    "stayed silent: never chat in {label} (set by {})",
                    rule.setter_name
                ),
                None,
            );
            return;
        }
    }
    let Some(text) = text else {
        if let Some(id) = placeholder {
            let _ = core.http.delete_message(channel, id).await;
        }
        match &outcome.error {
            Some(error) => {
                core.feed(
                    FeedKind::Error,
                    format!("couldn't reply: {error}"),
                    Some(label),
                );
                if job.direct() {
                    let note = format!(
                        "-# ⚠️ I couldn't think that through — my model isn't responding ({}). Try again in a bit.",
                        crate::llm::truncate(error, 120)
                    );
                    let mut request = core
                        .http
                        .create_message(channel)
                        .content(note)
                        .allowed_mentions(AllowedMentions::none());
                    if let Some(reply) = job.reply_to() {
                        request = request.reply(reply);
                    }
                    let _ = request.await;
                }
            }
            None => core.live().stat(&core.id, |s| s.skipped += 1),
        }
        return;
    };

    // AAv2: the "thought for X seconds" footer is gone for good — the
    // config toggle stays (ignored) so old configs keep loading.
    let _ = (profile.behavior.show_footer, style, elapsed);
    let mut final_text = text.clone();
    if let Some(guild_id) = job.guild_id {
        final_text = link_mentions(core, guild_id, &final_text);
    }
    // GitHub updates: heading, the release link with its preview, nothing
    // else embedded.
    if let Some(task) = job
        .task
        .as_ref()
        .filter(|t| matches!(t.trigger, crate::memory::TaskTrigger::GitHub { .. }))
    {
        final_text =
            super::github::finish_post(&final_text, job.question.as_deref(), &task.instruction);
    }
    let chunks = split_message(&final_text);
    // Replies ping the person being answered (that's what makes it a
    // proper reply); ambient remarks don't.
    // The person being answered gets a normal reply ping. Anyone else the
    // reply names shows as a clickable name but is only notified when the
    // request was about reaching them ("ping/tell/remind X"). Roles and
    // @everyone never ping.
    let ping = matches!(job.reason, Reason::Direct | Reason::Hq | Reason::Task);
    let mentions = if wants_to_reach_others(job) {
        AllowedMentions::users()
    } else {
        AllowedMentions::none()
    }
    .with_replied_user(ping);
    // Live-answer flow: when the whole answer fits one message and a
    // status placeholder exists, the placeholder BECOMES the answer —
    // edited in place, files attached to it — instead of a second
    // message. Mentions in the content still notify on the edit, and
    // later content-only edits never strip the attachments.
    let edit_in_place = placeholder.is_some()
        && matches!(job.outlet, Outlet::Channel)
        && chunks.len() == 1
        && outcome.files.len() <= 10;
    let mut first_id: Option<axis::prelude::MessageId> = None;
    let mut edited_in_place = false;
    if let (true, Some(id)) = (edit_in_place, placeholder) {
        let mut request = core
            .http
            .edit_message(channel, id)
            .content(chunks[0].clone())
            .allowed_mentions(mentions.clone());
        for (name, bytes) in &outcome.files {
            request = request.file(name.clone(), bytes.clone());
        }
        match request.await {
            Ok(edited) => {
                edited_in_place = true;
                first_id = Some(edited.id);
            }
            Err(error) => {
                // Fall through to the fresh-message flow; the placeholder
                // is deleted there.
                core.feed(
                    FeedKind::Error,
                    format!(
                        "could not edit the answer into place ({}); sending a new message",
                        tools::discord_error(error)
                    ),
                    Some(label.clone()),
                );
            }
        }
    }
    if !edited_in_place {
        if let Some(id) = placeholder {
            let _ = core.http.delete_message(channel, id).await;
        }
    }
    // Long answers: pages with buttons — unless they asked for it all at
    // once, then several messages (each under Discord's limit). Files and
    // images go at the very end either way.
    let all_at_once =
        request_text(job).is_some_and(|t| wants_everything(&t)) || !profile.behavior.paginate;
    let pages = if chunks.len() > 1 && !all_at_once {
        paginate(&final_text)
    } else {
        Vec::new()
    };
    if !edited_in_place && pages.len() > 1 {
        let mut request = core
            .http
            .create_message(channel)
            .content(pages[0].clone())
            .allowed_mentions(mentions.clone())
            .components(page_buttons(0, pages.len()));
        if let Some(reply) = job.reply_to() {
            if job.addressed() || job.task.is_some() {
                request = request.reply(reply);
            }
        }
        for (name, bytes) in &outcome.files {
            request = request.file(name.clone(), bytes.clone());
        }
        match request.await {
            Ok(message) => {
                first_id = Some(message.id);
                core.pages
                    .lock()
                    .expect("pages poisoned")
                    .insert(message.id.get(), pages.clone());
            }
            Err(error) => {
                let permission = tools::is_permission_error(&error);
                let why = tools::discord_error(error);
                core.feed(
                    FeedKind::Error,
                    format!("could not send reply: {why}"),
                    Some(label.clone()),
                );
                if permission {
                    super::tasks::report_cant_post(
                        core,
                        job,
                        "I'm missing permission to post there",
                    )
                    .await;
                }
                return;
            }
        }
    }
    let chunks = if pages.len() > 1 || edited_in_place {
        Vec::new()
    } else {
        chunks
    };
    let last = chunks.len().saturating_sub(1);
    for (index, chunk) in chunks.iter().enumerate() {
        let mut request = core
            .http
            .create_message(channel)
            .content(chunk.clone())
            .allowed_mentions(mentions.clone());
        if index == 0 {
            if let Some(reply) = job.reply_to() {
                if job.addressed() || job.task.is_some() {
                    request = request.reply(reply);
                }
            }
        }
        if index == last {
            for (name, bytes) in &outcome.files {
                request = request.file(name.clone(), bytes.clone());
            }
        }
        match request.await {
            Ok(message) => {
                if first_id.is_none() {
                    first_id = Some(message.id);
                }
            }
            Err(error) => {
                let permission = tools::is_permission_error(&error);
                if permission {
                    super::tasks::report_cant_post(
                        core,
                        job,
                        "I'm missing permission to post there",
                    )
                    .await;
                }
                core.feed(
                    FeedKind::Error,
                    format!("could not send reply: {}", tools::discord_error(error)),
                    Some(label.clone()),
                );
                return;
            }
        }
    }
    // Make sure the archive holds the final text, not "thinking…".
    if let Some(id) = first_id {
        core.hive.knowledge.channel_mut(job.channel_id, |archive| {
            if let Some(stored) = archive.get_mut(id.get()) {
                stored.content = pages.first().cloned().unwrap_or_else(|| final_text.clone());
            }
        });
    }
    core.live().stat(&core.id, |s| s.replies += 1);
    core.feed(
        FeedKind::Reply,
        format!(
            "{}{}",
            match job.reason {
                Reason::Ambient => "chimed in: ",
                Reason::Task => "task: ",
                Reason::Hq => "HQ: ",
                _ => "",
            },
            crate::llm::truncate(&text, 160)
        ),
        Some(label),
    );
}

/// Turn "@name" written by the model into a real <@id> mention when it
/// names a member (by username or display name) or a fellow agent —
/// and "@1234…" (a bare user ID, which small models like to copy from the
/// transcript) into a mention too.
pub fn link_mentions(core: &AgentCore, guild_id: u64, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let preceded_ok = out
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '<');
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '-')
            .collect();
        let name = name.trim_end_matches(['.', '-']).to_owned();
        let lower = name.to_lowercase();
        let bare_id = (17..=20).contains(&name.len()) && name.chars().all(|c| c.is_ascii_digit());
        let found = if preceded_ok && bare_id {
            name.parse::<u64>().ok()
        } else if preceded_ok && !name.is_empty() && lower != "everyone" && lower != "here" {
            core.world
                .with_guild(guild_id, |g| {
                    g.members
                        .values()
                        .find(|m| {
                            m.username.to_lowercase() == lower
                                || m.display_name().to_lowercase() == lower
                                || m.global_name
                                    .as_deref()
                                    .is_some_and(|n| n.to_lowercase() == lower)
                        })
                        .map(|m| m.user_id)
                })
                .flatten()
                .or_else(|| {
                    core.hive
                        .peer_by_name(&name)
                        .map(|p| p.user_id)
                        .filter(|id| *id != 0)
                })
        } else {
            None
        };
        match found {
            Some(id) => {
                out.push_str(&format!("<@{id}>"));
                rest = &after[name.len()..];
            }
            None => {
                out.push('@');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Background model calls
// ---------------------------------------------------------------------------

/// A single non-streaming completion for background work (summaries,
/// culture notes, moderation reviews), on the background model if set.
pub async fn quick_completion(
    core: &AgentCore,
    system: &str,
    user: &str,
    max_tokens: u32,
    json: bool,
) -> Result<String, String> {
    let profile = core.profile();
    let model = if profile.model.background_model.trim().is_empty() {
        profile.model.model.clone()
    } else {
        profile.model.background_model.trim().to_owned()
    };
    let endpoint = core.endpoint();
    let messages = [Msg::system(system), Msg::user(user)];
    let request = ChatRequest {
        thinking_budget: None,
        model: &model,
        messages: &messages,
        tools: &[],
        temperature: 0.3,
        max_tokens,
        stream: false,
        json,
        reasoning: profile.model.thinking.background.effort(),
    };
    let permit = core.hive.endpoint_permit(&endpoint.limit_key()).await;
    let result = endpoint.chat(&request, &mut |_| {}).await;
    drop(permit);
    core.live().stat(&core.id, |s| s.llm_calls += 1);
    let output = result.map_err(|e| e.to_string())?;
    if let Some(usage) = &output.usage {
        core.live().stat(&core.id, |s| {
            s.tokens_in += usage.prompt_tokens as u64;
            s.tokens_out += usage.completion_tokens as u64;
        });
    }
    Ok(output.content.trim().to_owned())
}

/// Extract a JSON object from a model reply (tolerating code fences and
/// chatter around it).
pub fn extract_json(text: &str) -> Option<serde_json::Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(&text[start..=end]).ok()
}

/// Context compaction: once a channel's unsummarized history grows past
/// what prompts can hold, fold the oldest part into its rolling summary.
pub async fn maybe_compact(core: &Arc<AgentCore>, channel_id: u64) {
    let (pending, chunk, summary, guild_id) = core.hive.knowledge.channel(channel_id, |archive| {
        let unsummarized: Vec<_> = archive.unsummarized().cloned().collect();
        let keep_recent = 70;
        let take = unsummarized.len().saturating_sub(keep_recent).min(90);
        (
            unsummarized.len(),
            unsummarized.into_iter().take(take).collect::<Vec<_>>(),
            archive.summary.clone(),
            archive.guild_id,
        )
    });
    if pending < 140 || chunk.is_empty() {
        return;
    }
    let Some(_guard) = core.hive.begin_job(channel_id, "summary") else {
        return;
    };
    let guild = if guild_id == 0 { None } else { Some(guild_id) };
    let mut cache = HashMap::new();
    let lines: Vec<String> = chunk
        .iter()
        .map(|m| context::render_line(core, guild, m, false, &mut cache))
        .collect();
    let system = "You maintain the running memory of a Discord channel for a team of AI agents. Write compact, factual summaries.";
    let user = format!(
        "Current summary:\n{}\n\nNewer messages to fold in:\n{}\n\nWrite the updated summary (max 250 words): ongoing topics, decisions, open questions, notable events, who was involved, running jokes. Plain text, no preamble.",
        if summary.is_empty() {
            "(none yet)"
        } else {
            &summary
        },
        crate::llm::truncate(&lines.join("\n"), 24_000)
    );
    match quick_completion(core, system, &user, 600, false).await {
        Ok(updated) if !updated.is_empty() => {
            let (updated, _) = crate::llm::text::split_think(&updated);
            let upto = chunk.last().map(|m| m.id).unwrap_or(0);
            core.hive.knowledge.channel_mut(channel_id, |archive| {
                archive.summary = updated;
                archive.summary_upto = archive.summary_upto.max(upto);
            });
            core.feed(
                FeedKind::Learning,
                format!("compacted {} older messages into the summary", chunk.len()),
                Some(core.channel_label(guild, channel_id)),
            );
        }
        Ok(_) => {}
        Err(error) => tracing::debug!(%error, "summary failed"),
    }
}

/// Playground: chat with the agent locally.
pub async fn playground_turn(core: Arc<AgentCore>, text: String) {
    {
        let mut playground = core.live().playground.lock().expect("live poisoned");
        let entry = playground.entry(core.id.clone()).or_default();
        entry.messages.push((PlaygroundRole::User, text.clone()));
        entry.busy = true;
    }
    core.live().repaint();
    let job = Job {
        guild_id: None,
        channel_id: 0,
        triggers: Vec::new(),
        reason: Reason::Direct,
        requester: Principal::operator(),
        task: None,
        outlet: Outlet::Playground,
        question: Some(text),
    };
    run(&core, job).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaked_markup_never_reaches_chat() {
        let leaked = "<function=send_message>
<parameter=channel>robots</parameter>
<parameter=content>
Ohhh right! My bad - pure legion density, not readability.
</parameter>
</function>
</tool_call>";
        assert!(contains_tool_markup(leaked));
        let cleaned = clean_reply(leaked, "Magi");
        assert!(!contains_tool_markup(&cleaned), "{cleaned}");
        assert!(cleaned.contains("legion density"), "{cleaned}");
        assert!(!cleaned.contains('<'), "{cleaned}");
    }

    #[test]
    fn markup_without_content_strips_to_plain_text() {
        let cleaned = clean_reply("before <tool_call>{skill}</tool_call> after", "Magi");
        assert!(!contains_tool_markup(&cleaned), "{cleaned}");
        assert!(
            cleaned.contains("before") && cleaned.contains("after"),
            "{cleaned}"
        );
    }

    #[test]
    fn no_reply_variants() {
        assert!(is_no_reply("NO_REPLY"));
        assert!(is_no_reply("  no_reply. "));
        assert!(is_no_reply("**NO_REPLY**"));
        assert!(is_no_reply(""));
        assert!(!is_no_reply("No, reply to her first"));
    }

    #[test]
    fn replies_are_cleaned() {
        assert_eq!(clean_reply("Nova: hey there", "Nova"), "hey there");
        assert_eq!(clean_reply("[12:01] Nova (you): hi", "Nova"), "hi");
        assert_eq!(
            clean_reply("sure thing\n\n\n\nok NO_REPLY", "Nova"),
            "sure thing\n\nok"
        );
        assert_eq!(
            clean_reply("[link](x) is good", "Nova"),
            "[link](x) is good"
        );
    }

    #[test]
    fn long_messages_split_and_keep_fences() {
        let text = format!(
            "intro\n```rust\n{}\n```\nouttro",
            "let x = 1;\n".repeat(400)
        );
        let chunks = split_message(&text);
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 2000, "{}", chunk.len());
            assert_eq!(chunk.matches("```").count() % 2, 0, "unbalanced fence");
        }
        assert_eq!(split_message("short"), vec!["short"]);
    }

    #[test]
    fn view_reveals_only_when_committed() {
        let mut view = View {
            started: Instant::now(),
            steps: Vec::new(),
            reasoning: String::new(),
            answer: "NO_RE".to_owned(),
            version: 0,
            revealed: false,
        };
        view.touch();
        assert!(!view.revealed);
        view.answer = "Actually, the answer is 42".to_owned();
        view.touch();
        assert!(view.revealed);
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn footers_never_reach_the_model() {
        assert_eq!(
            context::strip_artifacts("hello there\n-# 💭 thought for 2.1s · 1 tool"),
            "hello there"
        );
        assert_eq!(context::strip_artifacts("-# 💭 thinking… · 3s\n"), "");
        assert_eq!(clean_reply("sure!\n-# 💭 thought for 9s", "Nova"), "sure!");
    }

    #[test]
    fn emoji_signoffs_are_removed() {
        assert_eq!(strip_emoji_signoff("Anything else? 👀"), "Anything else?");
        assert_eq!(strip_emoji_signoff("nice 👍👍 "), "nice");
        assert_eq!(strip_emoji_signoff("🎉"), "🎉");
        assert_eq!(strip_emoji_signoff("I like 🍕 a lot"), "I like 🍕 a lot");
    }

    #[test]
    fn offers_and_closers_are_detected() {
        assert!(offers_instead_of_doing(
            "My bad. Want me to check the sue-updates channel for the latest?"
        ));
        assert!(offers_instead_of_doing("Should I look into it?"));
        assert!(claims_action(
            "Got it — sending a random message to #scratchpad3."
        ));
        let steps = parse_steps(
            "1. find an ocean image\n2) pick colors; - apply the design\n\n3. look at it",
        );
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].text, "find an ocean image");
        assert!(is_interrupt("magi stop"));
        let long = (0..60)
            .map(|i| format!("- item {i}: some text that makes the line longer than usual"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n-# 💭 thought for 2.0s";
        let pages = paginate(&long);
        assert!(pages.len() >= 3, "{}", pages.len());
        assert!(
            pages
                .iter()
                .all(|p| p.len() <= 2000 && p.ends_with("thought for 2.0s"))
        );
        assert!(wants_everything("send it all at once please"));
        assert_eq!(
            join_continued("self-hosting is a serious", "investment."),
            "self-hosting is a serious investment."
        );
        assert_eq!(
            join_continued("line one\n", "line two"),
            "line one\nline two"
        );
        assert_eq!(
            join_continued(
                "it's a chunky boy. Full model wants",
                "Full model wants ~614GB"
            ),
            "it's a chunky boy. Full model wants ~614GB"
        );
        assert!(is_interrupt("nevermind"));
        assert!(!is_interrupt(
            "how do i stop the music bot from leaving every time it ends a song"
        ));
        assert!(asks_for_details(
            "I didn't find it. Could you clarify if you meant an actual zebra? Also, which size did you have in mind?"
        ));
        assert!(!asks_for_details("The latest is v0.0.6."));
        assert!(!claims_action("Not much! Just hanging out here."));
        assert!(claims_action(
            "Got it, uvx. Won't chat in scratchpad3 or c-md."
        ));
        assert_eq!(
            channel_rule_request("never chat in scratchpad3 or c-md"),
            Some("stop_chatting_in")
        );
        assert_eq!(
            schedule_request("every 2 hours check the news and post anything big"),
            Some("create_task")
        );
        assert_eq!(
            requested_count("magi duplicate this channel 10 times and every 5 seconds delete 1"),
            10
        );
        assert_eq!(requested_count("make five roles"), 5);
        assert_eq!(requested_count("hi magi"), 0);
        assert!(timing_request("every 5 seconds delete one"));
        assert!(!timing_request("what's 5 + 5"));
        assert_eq!(
            available_context(
                r#"request (15098 tokens) exceeds the available context size (8192 tokens), try increasing it"#
            ),
            Some(8192)
        );
        assert_eq!(available_context("too long"), None);
        assert_eq!(
            github_request(
                "magi watch for this repo https://github.com/a/b and every release send the link here"
            ),
            Some("watch_github")
        );
        assert_eq!(
            github_request("stop watching the llama.cpp repo"),
            Some("stop_watching_github")
        );
        assert_eq!(
            github_request("what repos are you watching?"),
            Some("list_tasks")
        );
        assert_eq!(
            github_request("what are you watching right now?"),
            Some("list_tasks")
        );
        assert_eq!(
            github_request(
                "watch https://github.com/MagiGun/sue/releases and post every release here"
            ),
            Some("watch_github")
        );
        assert_eq!(
            schedule_request("every monday and friday at 6pm remind everyone"),
            Some("create_task")
        );
        assert_eq!(
            schedule_request("tomorrow at 3pm post the link"),
            Some("create_task")
        );
        assert_eq!(
            schedule_request("remind me in 10 minutes to stretch"),
            Some("set_reminder")
        );
        assert_eq!(
            schedule_request("what did you do every day last week?"),
            None
        );
        assert_eq!(
            schedule_request("can you post a quote every day at 9?"),
            Some("create_task")
        );
        assert_eq!(schedule_request("what's the news today"), None);
        assert_eq!(
            channel_rule_request("from now on dont post anything in general"),
            Some("stop_chatting_in")
        );
        assert_eq!(
            channel_rule_request("don't ever talk in the ticket support category"),
            Some("stop_chatting_in")
        );
        assert_eq!(
            channel_rule_request("ok you can chat in scratchpad3 again"),
            Some("allow_chatting_in")
        );
        assert_eq!(channel_rule_request("be quiet for 10 minutes"), None);
        assert_eq!(channel_rule_request("what's new in general?"), None);
        assert!(claims_action("Got it — I'll keep my replies in #general."));
        assert!(looks_like_request(
            "Send a message in CMD's channel, just whatever."
        ));
        assert!(looks_like_request("can you look up the weather"));
        assert!(!looks_like_request("Yo, what's up?"));
        assert!(!offers_instead_of_doing("The latest is v0.0.6. It adds X."));
        assert!(!offers_instead_of_doing("Which server do you mean?"));
        assert_eq!(
            strip_generic_closer("The latest is v0.0.6.\n\nAnything else you want me to track?"),
            "The latest is v0.0.6."
        );
        assert_eq!(strip_generic_closer("Anything else?"), "Anything else?");
    }

    #[test]
    fn reasons_order_by_strength() {
        assert!(Reason::Direct > Reason::Conversation);
        assert!(Reason::Conversation > Reason::Ambient);
    }
}
