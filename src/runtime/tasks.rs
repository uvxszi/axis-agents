//! Running standing tasks: duties people assigned to an agent ("check
//! every new forum post against the guidelines", "post a daily
//! question", "welcome new members").
//!
//! A task runs with the authority of whoever assigned it, recomputed from
//! their *current* roles — if they lose their moderator role, so does
//! the task.

use std::{sync::Arc, time::Duration};

use axis::model::{Member, Message};
use axis::prelude::ChannelId;

use super::agent::{AgentCore, task_channel_matches};
use super::authority::{Principal, PrincipalKind};
use super::brain::{self, Job, Outlet, Reason};
use super::hive::ClaimKind;
use crate::memory::{StandingTask, TaskTrigger, now_ms};

fn my_tasks(core: &AgentCore, guild_id: u64) -> Vec<StandingTask> {
    core.hive.knowledge.guild(guild_id, |k| {
        k.tasks
            .iter()
            .filter(|t| t.agent_id == core.id && t.active)
            .cloned()
            .collect()
    })
}

/// The task assigner as a principal, from their current membership.
fn assigner(core: &AgentCore, guild_id: u64, task: &StandingTask) -> Principal {
    core.world
        .with_guild(guild_id, |g| match g.members.get(&task.author_id) {
            Some(member) => Principal::for_member(g, member, PrincipalKind::Human),
            None => {
                let mut principal =
                    Principal::unknown_member(g, task.author_id, task.author_name.clone());
                // Not cached: fall back to what they had when assigning,
                // but never above moderator.
                principal.level = task.author_level.min(crate::config::AccessLevel::Moderator);
                principal
            }
        })
        .unwrap_or_else(|| Principal::dm_user(task.author_id, task.author_name.clone()))
}

/// Posting failed for lack of permission: tell the task's assigner (or the
/// person who asked) somewhere they'll see it — where the task was set up,
/// else by DM — at most every few hours per channel.
pub async fn report_cant_post(core: &Arc<AgentCore>, job: &Job, why: &str) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<(u64, u64), i64>>> = OnceLock::new();
    let Some(guild_id) = job.guild_id else {
        return;
    };
    let channel_id = job.channel_id;
    let (who, origin, about) = match &job.task {
        Some(task) => {
            let channel = core.world.with_guild(guild_id, |g| g.channel_name(task.trigger.channel_id())).unwrap_or_default();
            (task.author_id, task.origin_channel, format!("task #{} ({})", task.id, task.trigger.describe(&channel)))
        }
        None => (job.requester.user_id, 0, "your message".to_owned()),
    };
    if who == 0 {
        return;
    }
    {
        let key = (channel_id, job.task.as_ref().map(|t| t.id).unwrap_or(0));
        let mut last = LAST.get_or_init(Default::default).lock().expect("notice poisoned");
        let now = now_ms();
        if last.get(&key).is_some_and(|t| now - t < 6 * 3_600_000) {
            return;
        }
        last.insert(key, now);
    }
    if let Some(task) = &job.task {
        core.hive.knowledge.guild_mut(guild_id, |k| {
            if let Some(t) = k.tasks.iter_mut().find(|t| t.id == task.id) {
                t.last_result = format!("couldn't post in <#{channel_id}>: {why}");
            }
        });
    }
    let text = format!(
        "<@{who}> I couldn't post in <#{channel_id}> for {about} — {why}. Give my role **View Channel** and **Send Messages** there (channel settings → Permissions), or tell me another channel, and I'll carry on."
    );
    core.feed(crate::runtime::live::FeedKind::Error, format!("couldn't post in a channel for {about}: {why} — telling the assigner"), None);
    // Where it was set up, if the agent can talk there…
    if origin != 0 && origin != channel_id && core.cant_post(guild_id, origin).is_none() {
        let sent = core
            .http
            .create_message(ChannelId::new(origin))
            .content(text.clone())
            .allowed_mentions(axis::model::AllowedMentions::users())
            .await;
        if sent.is_ok() {
            return;
        }
    }
    // …otherwise by DM…
    if let Ok(dm) = core.http.create_dm(axis::prelude::UserId::new(who)).await {
        let server = core.world.with_guild(guild_id, |g| g.name.clone()).unwrap_or_default();
        let sent = core
            .http
            .create_message(dm.id)
            .content(format!("{} (in **{server}**)", text.replacen(&format!("<@{who}> "), "", 1)))
            .await;
        if sent.is_ok() {
            return;
        }
    }
    // …and with DMs closed: Agent HQ, or any channel they can read and the
    // agent can post in (the server's system channel first).
    for channel in fallback_channels(core, guild_id, who, channel_id) {
        let sent = core
            .http
            .create_message(ChannelId::new(channel))
            .content(text.clone())
            .allowed_mentions(axis::model::AllowedMentions::users())
            .await;
        if sent.is_ok() {
            return;
        }
    }
    tracing::warn!(guild_id, who, "couldn't tell anyone that a post failed");
}

/// Channels to reach someone in when their DMs are closed: HQ, the
/// system channel, then the rest by position — only where they can read
/// it and the agent can post.
pub(crate) fn fallback_channels(core: &AgentCore, guild_id: u64, who: u64, failed: u64) -> Vec<u64> {
    let hq = core.hive.knowledge.guild(guild_id, |k| k.hq.channel_id);
    core.world
        .with_guild(guild_id, |g| {
            let roles = g.members.get(&who).map(|m| m.roles.clone()).unwrap_or_default();
            let mut list: Vec<u64> = hq.into_iter().collect();
            list.extend(g.system_channel);
            list.extend(
                g.sorted_channels()
                    .into_iter()
                    .filter(|c| c.r#type == axis::model::ChannelType::GuildText)
                    .map(|c| c.id.get()),
            );
            let mut seen = Vec::new();
            list.retain(|c| {
                let fresh = !seen.contains(c);
                seen.push(*c);
                fresh && *c != failed
            });
            list.into_iter()
                .filter(|c| {
                    super::authority::channel_permissions(g, who, &roles, *c).contains(axis::prelude::Permissions::VIEW_CHANNEL)
                })
                .take(3)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|c| core.cant_post(guild_id, *c).is_none())
        .collect()
}

pub(crate) fn spawn_task(
    core: &Arc<AgentCore>,
    guild_id: u64,
    channel_id: u64,
    task: StandingTask,
    trigger: Option<Message>,
    question: Option<String>,
) {
    let core = Arc::clone(core);
    tokio::spawn(async move {
        run_task_now(&core, guild_id, channel_id, task, trigger, question).await;
    });
}

/// Run a task and wait; true when it went through (false: it couldn't
/// post there, or the model failed).
pub(crate) async fn run_task_now(
    core: &Arc<AgentCore>,
    guild_id: u64,
    channel_id: u64,
    task: StandingTask,
    trigger: Option<Message>,
    question: Option<String>,
) -> bool {
    let job = Job {
        guild_id: Some(guild_id),
        channel_id,
        triggers: trigger.into_iter().collect(),
        reason: Reason::Task,
        requester: assigner(core, guild_id, &task),
        task: Some(task),
        outlet: Outlet::Channel,
        question,
    };
    // Can't post there (permissions changed since it was set up): say so
    // instead of working on a post nobody will see.
    if let Some(missing) = core.cant_post(guild_id, channel_id) {
        report_cant_post(core, &job, &format!("my role is missing {missing} there")).await;
        return false;
    }
    brain::run(core, job).await
}

/// A ticket was opened: run the agent's new-ticket tasks inside it.
pub fn on_new_ticket(core: &Arc<AgentCore>, guild_id: u64, channel_id: u64, opener: &str) {
    for task in my_tasks(core, guild_id) {
        if matches!(task.trigger, TaskTrigger::NewTicket { .. }) {
            spawn_task(core, guild_id, channel_id, task, None, Some(opener.to_owned()));
        }
    }
}

/// New messages: forum posts (a thread's starter message has the thread's
/// ID) and watched channels. Returns whether a task will handle it.
pub fn on_message(core: &Arc<AgentCore>, guild_id: u64, message: &Message) -> bool {
    let tasks = my_tasks(core, guild_id);
    if tasks.is_empty() {
        return false;
    }
    let channel_id = message.channel_id.get();
    let starter = message.id.get() == channel_id;
    let cached_parent = core
        .world
        .channel(guild_id, channel_id)
        .and_then(|c| c.parent_id.map(|p| p.get()));
    // Decide synchronously when the cache allows it.
    let likely = tasks.iter().any(|task| match task.trigger {
        TaskTrigger::NewPost { .. } => starter,
        TaskTrigger::NewMessage { .. } => {
            task_channel_matches(&task.trigger, channel_id, cached_parent)
        }
        _ => false,
    });
    if !likely {
        return false;
    }
    let core = Arc::clone(core);
    let message = message.clone();
    tokio::spawn(async move {
        let mut parent = cached_parent;
        if parent.is_none() && starter {
            // A brand-new thread may not be cached yet.
            if let Ok(channel) = core.http.get_channel(ChannelId::new(channel_id)).await {
                parent = channel.parent_id.map(|p| p.get());
                core.world.upsert_channel(&channel);
            }
        }
        for task in tasks {
            let matches = match task.trigger {
                TaskTrigger::NewPost { .. } => {
                    starter && task_channel_matches(&task.trigger, channel_id, parent)
                }
                TaskTrigger::NewMessage { .. } => {
                    task_channel_matches(&task.trigger, channel_id, parent)
                }
                _ => false,
            };
            // The same post can arrive by gateway and by fetch: run once.
            let key = message
                .id
                .get()
                .wrapping_add(task.id.wrapping_mul(0x9E37_79B9));
            if !matches || !core.hive.claim_once(key, ClaimKind::TaskRun) {
                continue;
            }
            if matches!(task.trigger, TaskTrigger::NewPost { .. }) {
                // Give the author a moment to finish (attachments, edits).
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
            spawn_task(
                &core,
                guild_id,
                channel_id,
                task,
                Some(message.clone()),
                None,
            );
        }
    });
    true
}

pub fn on_member_join(core: &Arc<AgentCore>, guild_id: u64, member: &Member) {
    if core.paused.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(user) = &member.user else {
        return;
    };
    if user.bot.unwrap_or(false) {
        return;
    }
    let name = format!(
        "{} (<@{}>)",
        user.global_name
            .clone()
            .unwrap_or_else(|| user.username.clone()),
        user.id
    );
    for task in my_tasks(core, guild_id) {
        if let TaskTrigger::MemberJoin { channel_id } = task.trigger {
            spawn_task(core, guild_id, channel_id, task, None, Some(name.clone()));
        }
    }
}

/// Give older memories embeddings once an embedding model is configured.
async fn backfill_embeddings(core: &AgentCore) {
    let model = core.profile().model.embedding_model;
    if model.trim().is_empty() {
        return;
    }
    let pending = core.memory.missing_embeddings(16);
    if pending.is_empty() {
        return;
    }
    let texts: Vec<String> = pending.iter().map(|(_, text)| text.clone()).collect();
    if let Ok(vectors) = core.endpoint().embed(&model, &texts).await {
        for ((id, _), vector) in pending.into_iter().zip(vectors) {
            core.memory.set_embedding(id, vector);
        }
    }
}

/// Channels agents read but rarely speak in still get their history
/// folded into summaries (one agent per server does it).
async fn compact_quiet_channels(core: &Arc<AgentCore>) {
    for guild_id in core.world.guild_ids() {
        let leader = core
            .hive
            .peers_in_guild(guild_id)
            .first()
            .map(|p| p.id.clone());
        if leader.as_deref() != Some(core.id.as_str()) {
            continue;
        }
        let channels: Vec<u64> = core
            .world
            .with_guild(guild_id, |g| g.channels.keys().copied().collect())
            .unwrap_or_default();
        for channel in channels {
            brain::maybe_compact(core, channel).await;
        }
    }
}

/// Fire scheduled tasks when due.
pub async fn scheduler(core: Arc<AgentCore>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut ticks: u64 = 0;
    loop {
        tick.tick().await;
        ticks += 1;
        backfill_embeddings(&core).await;
        if ticks % 10 == 0 {
            compact_quiet_channels(&core).await;
        }
        if core.paused.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        super::community::tick(&core).await;
        super::levels::tick(&core).await;
        super::translate::tick(&core).await;
        super::github::tick(&core).await;
        for guild_id in core.world.guild_ids() {
            let due: Vec<StandingTask> = core.hive.knowledge.guild_mut(guild_id, |k| {
                let now = now_ms();
                let mut due = Vec::new();
                for task in k
                    .tasks
                    .iter_mut()
                    .filter(|t| t.agent_id == core.id && t.active)
                {
                    let is_due = match &task.trigger {
                        TaskTrigger::Schedule { at_minute: Some(minute), days, every_minutes, .. } => {
                            crate::memory::schedule_due(now, task.last_run, *minute, days, *every_minutes)
                        }
                        TaskTrigger::Schedule { every_minutes, .. } => now - task.last_run >= *every_minutes as i64 * 60_000,
                        TaskTrigger::Once { at, .. } => *at <= now,
                        _ => false,
                    };
                    if is_due {
                        task.last_run = now;
                        task.runs += 1;
                        due.push(task.clone());
                    }
                }
                // One-time tasks are done once they run.
                let once: Vec<u64> = due.iter().filter(|t| matches!(t.trigger, TaskTrigger::Once { .. })).map(|t| t.id).collect();
                k.tasks.retain(|t| !once.contains(&t.id));
                due
            });
            for task in due {
                let channel_id = task.trigger.channel_id();
                spawn_task(&core, guild_id, channel_id, task, None, None);
            }
            // Reminders are delivered word for word, then removed.
            let reminders: Vec<StandingTask> = core.hive.knowledge.guild_mut(guild_id, |k| {
                let now = now_ms();
                let (due, keep): (Vec<StandingTask>, Vec<StandingTask>) =
                    k.tasks.drain(..).partition(|t| {
                        t.agent_id == core.id
                            && matches!(t.trigger, TaskTrigger::Reminder { at, .. } if at <= now)
                    });
                k.tasks = keep;
                due
            });
            for reminder in reminders {
                if let TaskTrigger::Reminder {
                    channel_id,
                    user_id,
                    ..
                } = reminder.trigger
                {
                    // A channel it never chats in: remind them by DM instead.
                    let channel_id = if core.barred_anywhere(channel_id).is_some() {
                        match core.http.create_dm(axis::prelude::UserId::new(user_id)).await {
                            Ok(dm) => dm.id.get(),
                            Err(_) => continue,
                        }
                    } else {
                        channel_id
                    };
                    let set_by = if reminder.author_id == user_id {
                        String::new()
                    } else {
                        format!("\n-# set by {}", reminder.author_name)
                    };
                    let _ = core
                        .http
                        .create_message(ChannelId::new(channel_id))
                        .content(crate::llm::truncate(
                            &format!("⏰ <@{user_id}> reminder: {}{set_by}", reminder.instruction),
                            1990,
                        ))
                        .allowed_mentions(axis::model::AllowedMentions {
                            users: vec![axis::prelude::UserId::new(user_id)],
                            ..axis::model::AllowedMentions::none()
                        })
                        .await;
                    core.feed(
                        crate::runtime::live::FeedKind::Reply,
                        format!(
                            "reminded <@{user_id}>: {}",
                            crate::llm::truncate(&reminder.instruction, 80)
                        ),
                        None,
                    );
                }
            }
        }
    }
}
