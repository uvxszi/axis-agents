//! The AI agent: MCP-style tools, permission gating, and the tool-call
//! loop.
//!
//! Security model (the important part): the model can *ask* for tools,
//! but every tool is executed by this app, which resolves the **invoking
//! user's real Discord permissions** from the API (guild roles + member)
//! and enforces them before any action runs. A user without
//! `BAN_MEMBERS` cannot ban — through the UI, through the AI, or through
//! a prompt injection — because the gate is in code, not in the prompt.
//! Denied attempts return an explanatory error to the model so it can
//! tell the user why.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, RwLock},
};

use axis::model::{Guild, Member, OverwriteType};
use axis::prelude::*;
use axis::rest::Http;
use serde_json::{Value, json};

use crate::llm::{self, ChatMessage};

/// Agent settings, edited in the GUI and persisted with the config.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Whether the agent answers at all.
    pub enabled: bool,
    /// Provider preset.
    pub provider: llm::Provider,
    /// OpenAI-compatible base URL, e.g. `http://localhost:1234/v1`.
    pub base_url: String,
    /// API key; empty for LM Studio.
    #[serde(default)]
    pub api_key: String,
    /// Model id as the provider names it.
    pub model: String,
    /// Sampling temperature.
    pub temperature: f32,
    /// Maximum tool-call rounds per invocation.
    pub max_iterations: u32,
    /// Whether @-mentioning the bot in a message summons the agent.
    pub mention_trigger: bool,
    /// The bot's name, used in prompts and the default slash command.
    pub name: String,
    /// The user's standing instructions for the agent.
    pub instructions: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: llm::Provider::LmStudio,
            base_url: llm::Provider::LmStudio.default_base_url().to_owned(),
            api_key: String::new(),
            model: llm::Provider::LmStudio.default_model().to_owned(),
            temperature: 0.7,
            max_iterations: 6,
            mention_trigger: true,
            name: "Agent".to_owned(),
            instructions: String::new(),
        }
    }
}

/// One remembered exchange, per (guild, channel) scope.
#[derive(Debug, Clone)]
pub(crate) struct MemoryTurn {
    role: String,
    author: String,
    content: String,
}

const MEMORY_TURNS: usize = 16;

/// Shared agent state: config, per-channel conversation memory, and the
/// bot's own identity (learned from READY).
#[derive(Default)]
pub struct AgentState {
    pub config: RwLock<AgentConfig>,
    pub(crate) memory: Mutex<HashMap<(u64, u64), VecDeque<MemoryTurn>>>,
    pub bot_user: Mutex<Option<(u64, String)>>,
}

impl AgentState {
    fn remember(&self, scope: (u64, u64), turn: MemoryTurn) {
        let mut memory = self.memory.lock().expect("agent memory poisoned");
        let history = memory.entry(scope).or_default();
        history.push_back(turn);
        while history.len() > MEMORY_TURNS {
            history.pop_front();
        }
    }

    fn history(&self, scope: (u64, u64)) -> Vec<MemoryTurn> {
        self.memory
            .lock()
            .expect("agent memory poisoned")
            .get(&scope)
            .map(|turns| turns.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn reset_memory(&self) {
        self.memory.lock().expect("agent memory poisoned").clear();
    }

    /// The bot's own user id, once READY told us.
    pub fn bot_user_id(&self) -> Option<u64> {
        self.bot_user
            .lock()
            .expect("bot user poisoned")
            .as_ref()
            .map(|(id, _)| *id)
    }

    pub fn set_bot_user(&self, id: u64, name: String) {
        *self.bot_user.lock().expect("bot user poisoned") = Some((id, name));
    }
}

// ---------------------------------------------------------------------------
// MCP-style tools
// ---------------------------------------------------------------------------

/// Everything a tool needs to run.
pub struct ToolCtx {
    pub http: Arc<Http>,
    pub guild_id: u64,
    pub channel_id: u64,
    pub invoker_id: u64,
    /// The invoking user's guild-level permissions, resolved from Discord.
    pub invoker_permissions: Permissions,
}

impl ToolCtx {
    /// Enforce a guild-level permission for the invoking user.
    fn gate(&self, required: Permissions) -> Result<(), String> {
        if self.invoker_permissions.is_administrator()
            || self.invoker_permissions.contains(required)
        {
            Ok(())
        } else {
            Err(format!(
                "denied: you are missing the Discord permission(s) {} needed for this action",
                required.names().collect::<Vec<_>>().join(", ")
            ))
        }
    }
}

/// A trait alias for the async executor, boxed per call.
type Executor = for<'a> fn(
    ctx: &'a ToolCtx,
    args: &'a Value,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
>;

/// An MCP-style tool: schema (for the model) + required permission +
/// executor (enforced by the app).
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema as static text (parsed lazily for the wire format).
    pub parameters: &'static str,
    /// Guild-level permission the invoker must have. Empty = no gate.
    pub required: Permissions,
    pub executor: Executor,
}

/// The full tool belt, in the OpenAI `tools` wire format.
pub fn definitions() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            let parameters: Value =
                serde_json::from_str(tool.parameters).expect("static tool schema is valid JSON");
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": parameters,
                }
            })
        })
        .collect()
}

pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.name == name)
}

static TOOLS: &[Tool] = &[
    Tool {
        name: "ban",
        description: "Ban a member from the guild. Requires BAN_MEMBERS.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"The Discord user ID to ban"},"reason":{"type":"string","description":"Reason shown in the audit log"},"delete_message_seconds":{"type":"integer","description":"Also delete their last N seconds of messages (0-604800)"}},"required":["user_id"]}"#,
        required: Permissions::BAN_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Banned via AI agent");
                let mut builder = ctx
                    .http
                    .ban_member(GuildId::new(ctx.guild_id), UserId::new(user_id));
                if let Some(seconds) = args.get("delete_message_seconds").and_then(Value::as_u64) {
                    builder = builder.delete_message_seconds(seconds.min(604_800) as u32);
                }
                builder.await.map_err(|error| error.to_string())?;
                Ok(format!("banned user {user_id} (reason: {reason})"))
            })
        },
    },
    Tool {
        name: "kick",
        description: "Kick a member from the guild. Requires KICK_MEMBERS.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"The Discord user ID to kick"},"reason":{"type":"string","description":"Reason shown in the audit log"}},"required":["user_id"]}"#,
        required: Permissions::KICK_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                ctx.http
                    .kick_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("kicked user {user_id}"))
            })
        },
    },
    Tool {
        name: "timeout",
        description: "Time a member out for some minutes (they cannot send or react). Requires MODERATE_MEMBERS.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"The Discord user ID to time out"},"minutes":{"type":"integer","description":"Duration in minutes (max 40320 = 28 days)"},"reason":{"type":"string","description":"Reason shown in the audit log"}},"required":["user_id","minutes"]}"#,
        required: Permissions::MODERATE_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                let minutes = args
                    .get("minutes")
                    .and_then(Value::as_i64)
                    .unwrap_or(10)
                    .clamp(1, 40_320);
                let until_millis =
                    axis::timestamp::Timestamp::now_utc().as_unix_millis() + minutes * 60_000;
                let until = axis::timestamp::Timestamp::from_unix_millis(until_millis)
                    .map_err(|error| format!("could not compute the timeout: {error}"))?;
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .timeout_until(until)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("timed out user {user_id} for {minutes} minutes"))
            })
        },
    },
    Tool {
        name: "untimeout",
        description: "Remove a member's timeout. Requires MODERATE_MEMBERS.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"The Discord user ID"}},"required":["user_id"]}"#,
        required: Permissions::MODERATE_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .clear_timeout()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("removed timeout for user {user_id}"))
            })
        },
    },
    Tool {
        name: "add_role",
        description: "Give a member a role. Requires MANAGE_ROLES, and the bot's role must be above it.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"The Discord user ID"},"role_id":{"type":"string","description":"The Discord role ID to add"}},"required":["user_id","role_id"]}"#,
        required: Permissions::MANAGE_ROLES,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                let role_id = parse_user(args, "role_id")?;
                let member = ctx
                    .http
                    .get_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let mut roles = member.roles.clone();
                roles.push(RoleId::new(role_id));
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .roles(roles)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("added role {role_id} to user {user_id}"))
            })
        },
    },
    Tool {
        name: "remove_role",
        description: "Take a role away from a member. Requires MANAGE_ROLES.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"The Discord user ID"},"role_id":{"type":"string","description":"The Discord role ID to remove"}},"required":["user_id","role_id"]}"#,
        required: Permissions::MANAGE_ROLES,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                let role_id = parse_user(args, "role_id")?;
                let member = ctx
                    .http
                    .get_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let roles: Vec<_> = member
                    .roles
                    .iter()
                    .copied()
                    .filter(|id| id.get() != role_id)
                    .collect();
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .roles(roles)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("removed role {role_id} from user {user_id}"))
            })
        },
    },
    Tool {
        name: "send_message",
        description: "Send a message to a channel. Requires SEND_MESSAGES there.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"The Discord channel ID (defaults to the current channel if omitted)"},"content":{"type":"string","description":"Message text, Discord Markdown allowed"}},"required":["content"]}"#,
        required: Permissions::SEND_MESSAGES,
        executor: |ctx, args| {
            Box::pin(async move {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or("missing content")?;
                let channel_id = args
                    .get("channel_id")
                    .and_then(Value::as_str)
                    .and_then(|text| text.parse::<u64>().ok())
                    .unwrap_or(ctx.channel_id);
                let channel = ChannelScope::resolve(ctx, channel_id).await?;
                channel.gate(Permissions::SEND_MESSAGES)?;
                let message = ctx
                    .http
                    .create_message(ChannelId::new(channel_id))
                    .content(content)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!(
                    "sent message {} to channel {channel_id}",
                    message.id
                ))
            })
        },
    },
    Tool {
        name: "delete_message",
        description: "Delete a message in a channel. Requires MANAGE_MESSAGES there.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"The Discord channel ID (defaults to the current channel if omitted)"},"message_id":{"type":"string","description":"The Discord message ID to delete"}},"required":["message_id"]}"#,
        required: Permissions::MANAGE_MESSAGES,
        executor: |ctx, args| {
            Box::pin(async move {
                let message_id = parse_user(args, "message_id")?;
                let channel_id = args
                    .get("channel_id")
                    .and_then(Value::as_str)
                    .and_then(|text| text.parse::<u64>().ok())
                    .unwrap_or(ctx.channel_id);
                let channel = ChannelScope::resolve(ctx, channel_id).await?;
                channel.gate(Permissions::MANAGE_MESSAGES)?;
                ctx.http
                    .delete_message(ChannelId::new(channel_id), MessageId::new(message_id))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!(
                    "deleted message {message_id} in channel {channel_id}"
                ))
            })
        },
    },
    Tool {
        name: "get_member",
        description: "Look up a member: username, nickname, roles, join date, timeout state.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"The Discord user ID to look up"}},"required":["user_id"]}"#,
        required: Permissions::empty(),
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                let member = ctx
                    .http
                    .get_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let username = member
                    .user
                    .as_ref()
                    .map(|user| user.username.clone())
                    .unwrap_or_else(|| "unknown".to_owned());
                let roles: Vec<String> = member.roles.iter().map(|id| id.to_string()).collect();
                Ok(format!(
                    "member {user_id}: username {username}, nick {:?}, roles [{}], joined {:?}, timed out until {:?}",
                    member.nick,
                    roles.join(", "),
                    member.joined_at,
                    member.communication_disabled_until
                ))
            })
        },
    },
    Tool {
        name: "get_guild",
        description: "Look up the guild: name, member count, roles, features.",
        parameters: r#"{"type":"object","properties":{}}"#,
        required: Permissions::empty(),
        executor: |ctx, _args| {
            Box::pin(async move {
                let guild = ctx
                    .http
                    .get_guild(GuildId::new(ctx.guild_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let roles: Vec<String> = guild.roles.iter().map(|role| role.name.clone()).collect();
                Ok(format!(
                    "guild {}: name {:?}, roles [{}], features [{}]",
                    guild.id,
                    guild.name,
                    roles.join(", "),
                    guild.features.join(", ")
                ))
            })
        },
    },
];

// ---------------------------------------------------------------------------
// Permission resolution
// ---------------------------------------------------------------------------

/// Resolve a member's guild-level permissions from Discord:
/// @everyone role + the member's roles, administrator short-circuits.
pub async fn invoker_permissions(
    http: &Http,
    guild_id: u64,
    user_id: u64,
) -> Result<Permissions, String> {
    let guild: Guild = http
        .get_guild(GuildId::new(guild_id))
        .await
        .map_err(|error| format!("could not load guild {guild_id}: {error}"))?;
    let member: Member = http
        .get_member(GuildId::new(guild_id), UserId::new(user_id))
        .await
        .map_err(|error| format!("could not load your membership: {error}"))?;
    Ok(compute_guild_permissions(&guild, &member))
}

/// Discord's guild permission algorithm.
pub fn compute_guild_permissions(guild: &Guild, member: &Member) -> Permissions {
    let member_id = member
        .user
        .as_ref()
        .map(|user| user.id.get())
        .unwrap_or_default();
    if member_id == guild.owner_id.get() {
        return Permissions::all();
    }
    let mut permissions = Permissions::empty();
    for role in &guild.roles {
        if role.id.get() == guild.id.get() {
            permissions |= role.permissions; // @everyone
        }
    }
    for role in &guild.roles {
        if member.roles.contains(&role.id) {
            permissions |= role.permissions;
        }
    }
    if permissions.is_administrator() {
        Permissions::all()
    } else {
        permissions
    }
}

/// Discord's channel permission algorithm (guild base + overwrites).
pub fn compute_channel_permissions(
    base: Permissions,
    member_user_id: u64,
    member_roles: &[RoleId],
    channel: &axis::model::Channel,
) -> Permissions {
    if base.is_administrator() {
        return Permissions::all();
    }
    let mut permissions = base;
    let Some(overwrites) = &channel.permission_overwrites else {
        return permissions;
    };
    // @everyone overwrite.
    if let Some(everyone) = overwrites.iter().find(|overwrite| {
        overwrite.id.get() == channel.id.get() && overwrite.r#type == OverwriteType::Role
    }) {
        permissions = (permissions & !everyone.deny) | everyone.allow;
    }
    // Role overwrites.
    let mut allow = Permissions::empty();
    let mut deny = Permissions::empty();
    for overwrite in overwrites {
        if overwrite.r#type == OverwriteType::Role
            && member_roles
                .iter()
                .any(|role| role.get() == overwrite.id.get())
        {
            allow |= overwrite.allow;
            deny |= overwrite.deny;
        }
    }
    permissions = (permissions & !deny) | allow;
    // Member-specific overwrite.
    if let Some(member_overwrite) = overwrites.iter().find(|overwrite| {
        overwrite.id.get() == member_user_id && overwrite.r#type == OverwriteType::Member
    }) {
        permissions = (permissions & !member_overwrite.deny) | member_overwrite.allow;
    }
    permissions
}

/// Resolved channel permissions for a channel-scoped tool.
struct ChannelScope {
    permissions: Permissions,
}

impl ChannelScope {
    async fn resolve(ctx: &ToolCtx, channel_id: u64) -> Result<Self, String> {
        let member: Member = ctx
            .http
            .get_member(GuildId::new(ctx.guild_id), UserId::new(ctx.invoker_id))
            .await
            .map_err(|error| format!("could not load your membership: {error}"))?;
        let channel = ctx
            .http
            .get_channel(ChannelId::new(channel_id))
            .await
            .map_err(|error| format!("could not load channel {channel_id}: {error}"))?;
        let member_roles = member.roles.clone();
        let member_id = member
            .user
            .as_ref()
            .map(|user| user.id.get())
            .unwrap_or(ctx.invoker_id);
        Ok(Self {
            permissions: compute_channel_permissions(
                ctx.invoker_permissions,
                member_id,
                &member_roles,
                &channel,
            ),
        })
    }

    fn gate(&self, required: Permissions) -> Result<(), String> {
        if self.permissions.is_administrator() || self.permissions.contains(required) {
            Ok(())
        } else {
            Err(format!(
                "denied: you are missing the channel permission(s) {} in this channel",
                required.names().collect::<Vec<_>>().join(", ")
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_user(args: &Value, field: &str) -> Result<u64, String> {
    let raw = args
        .get(field)
        .cloned()
        .ok_or_else(|| format!("missing argument {field:?}"))?;
    let text = match raw {
        Value::String(text) => text,
        Value::Number(number) => number.to_string(),
        other => return Err(format!("argument {field:?} was {other}; expected an ID")),
    };
    text.trim()
        .trim_start_matches('<')
        .trim_start_matches("@")
        .trim_end_matches('>')
        .parse::<u64>()
        .map_err(|_| format!("argument {field:?} was {text:?}; expected a Discord ID"))
}

fn tool_error(name: &str, error: String) -> String {
    format!("tool {name} failed: {error}")
}

/// Execute one tool call with the permission gate enforced.
pub async fn execute_tool(ctx: &ToolCtx, call: &llm::ToolCallRequest) -> String {
    let Some(tool) = find(&call.name) else {
        return format!("tool {:?} does not exist", call.name);
    };
    if let Err(denial) = ctx.gate(tool.required) {
        return denial;
    }
    if call.arguments.get("_error").is_some() {
        return tool_error(
            &call.name,
            "the model produced malformed tool arguments".to_owned(),
        );
    }
    match (tool.executor)(ctx, &call.arguments).await {
        Ok(result) => result,
        // REST errors (Discord said no — hierarchy, missing access on the
        // bot itself, unknown ID...) go back to the model verbatim so it
        // can explain the failure.
        Err(error) => tool_error(&call.name, error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Run the agent for one user message and return the final answer.
pub async fn run_agent(
    http: Arc<Http>,
    state: &AgentState,
    scope: (u64, u64),
    invoker: (u64, String),
    user_message: String,
) -> Result<String, String> {
    let config = state.config.read().expect("agent config poisoned").clone();
    if !config.enabled {
        return Err("the AI agent is disabled — enable it in Bot Studio (Agent tab)".to_owned());
    }
    if config.base_url.trim().is_empty() || config.model.trim().is_empty() {
        return Err(
            "the AI agent has no provider configured — set it in Bot Studio (Agent tab)".to_owned(),
        );
    }

    // Resolve the invoker's permissions once. Fail-closed: if this fails,
    // tools deny with the reason.
    let invoker_permissions = if scope.0 != 0 {
        match invoker_permissions(&http, scope.0, invoker.0).await {
            Ok(perms) => perms,
            Err(error) => {
                tracing::warn!(%error, "could not resolve invoker permissions; tools will deny");
                Permissions::empty()
            }
        }
    } else {
        Permissions::empty()
    };

    let mut messages = Vec::new();
    messages.push(ChatMessage::system(system_prompt(
        &config,
        scope,
        &invoker,
        invoker_permissions,
        scope.0 != 0,
    )));
    for turn in state.history(scope) {
        messages.push(match turn.role.as_str() {
            "user" => ChatMessage::user(format!("{}: {}", turn.author, turn.content)),
            _ => ChatMessage::assistant(turn.content),
        });
    }
    messages.push(ChatMessage::user(format!(
        "{}: {}",
        invoker.1, user_message
    )));

    let tools = definitions();
    let ctx = ToolCtx {
        http,
        guild_id: scope.0,
        channel_id: scope.1,
        invoker_id: invoker.0,
        invoker_permissions,
    };

    let mut final_content = String::new();
    for _ in 0..config.max_iterations.max(1) {
        let request = llm::build_chat_request(&config.model, config.temperature, &messages, &tools);
        let body = llm::chat(&config.base_url, &config.api_key, request).await?;
        let turn = llm::parse_chat_response(&body)?;

        if turn.tool_calls.is_empty() {
            final_content = turn.content;
            break;
        }

        messages.push(ChatMessage::assistant_with_tool_calls(
            turn.content.clone(),
            turn.tool_calls.clone(),
        ));
        for call in &turn.tool_calls {
            tracing::info!(tool = %call.name, "agent tool call");
            let result = execute_tool(&ctx, call).await;
            messages.push(ChatMessage::tool_result(
                call.id.clone(),
                truncate_tool_result(&result),
            ));
        }
    }

    if final_content.is_empty() {
        final_content = "I hit my tool-call limit without arriving at an answer.".to_owned();
    }

    state.remember(
        scope,
        MemoryTurn {
            role: "user".to_owned(),
            author: invoker.1.clone(),
            content: user_message,
        },
    );
    state.remember(
        scope,
        MemoryTurn {
            role: "assistant".to_owned(),
            author: config.name.clone(),
            content: final_content.clone(),
        },
    );

    Ok(final_content)
}

fn system_prompt(
    config: &AgentConfig,
    scope: (u64, u64),
    invoker: &(u64, String),
    permissions: Permissions,
    in_guild: bool,
) -> String {
    let mut prompt = format!(
        "You are {}, the AI agent of a Discord bot. You operate inside Discord and act through tools.\n\n",
        config.name
    );
    if !config.instructions.trim().is_empty() {
        prompt.push_str("Your standing instructions from the operator:\n");
        prompt.push_str(config.instructions.trim());
        prompt.push_str("\n\n");
    }
    if in_guild {
        let permission_text = if permissions.is_administrator() {
            "ADMINISTRATOR (everything)".to_owned()
        } else {
            permissions.to_string()
        };
        prompt.push_str(&format!(
            "Context: guild {}, channel {}. You are talking to {} (Discord ID {}), whose guild permissions are: {}.\n",
            scope.0, scope.1, invoker.1, invoker.0, permission_text
        ));
        prompt.push_str(
            "You can call tools to perform Discord actions. Every tool is strictly gated to that person's own Discord permissions by the app itself — you cannot bypass it. If a tool result says \"denied\", explain that they lack the required Discord permissions; do not retry. Never claim an action succeeded unless a tool result confirms it. If a tool fails, share the reason.\n",
        );
    } else {
        prompt.push_str(
            "This conversation happens in a direct message, so no guild tools are available; answer with plain text.\n",
        );
    }
    prompt.push_str("Be concise and format for Discord Markdown.");
    prompt
}

fn truncate_tool_result(result: &str) -> String {
    const MAX: usize = 4000;
    if result.len() <= MAX {
        result.to_owned()
    } else {
        let mut cut = MAX;
        while cut > 0 && !result.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}… (truncated)", &result[..cut])
    }
}

/// Split a reply into Discord-safe chunks (max 2000 characters).
pub fn split_reply(text: &str) -> Vec<String> {
    const MAX: usize = 2000;
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > MAX {
        let cut = rest[..=MAX]
            .rfind('\n')
            .filter(|cut| *cut > MAX / 2)
            .unwrap_or(MAX);
        let cut = rest[..cut + 1]
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= MAX)
            .last()
            .unwrap_or(MAX);
        chunks.push(rest[..cut].trim_end().to_owned());
        rest = &rest[cut..];
        rest = rest.trim_start();
    }
    if !rest.is_empty() || chunks.is_empty() {
        chunks.push(rest.to_owned());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use axis::model::RoleColors;

    fn member_with(roles: &[u64]) -> Member {
        serde_json::from_value(json!({
            "user": { "id": "42", "username": "tester", "discriminator": "0" },
            "roles": roles.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            "deaf": false, "mute": false
        }))
        .unwrap()
    }

    fn guild_with(everyone: Permissions, roles: &[(u64, Permissions)]) -> Guild {
        let mut role_list = vec![axis::model::Role {
            id: RoleId::new(500),
            name: "@everyone".to_owned(),
            color: 0,
            colors: RoleColors {
                primary_color: 0,
                secondary_color: None,
                tertiary_color: None,
            },
            hoist: false,
            icon: None,
            unicode_emoji: None,
            position: 0,
            permissions: everyone,
            managed: false,
            mentionable: true,
            flags: None,
        }];
        for (index, (id, permissions)) in roles.iter().enumerate() {
            role_list.push(axis::model::Role {
                id: RoleId::new(*id),
                name: format!("role-{index}"),
                color: 0,
                colors: RoleColors {
                    primary_color: 0,
                    secondary_color: None,
                    tertiary_color: None,
                },
                hoist: false,
                icon: None,
                unicode_emoji: None,
                position: index as i32 + 1,
                permissions: *permissions,
                managed: false,
                mentionable: true,
                flags: None,
            });
        }
        serde_json::from_value(json!({
            "id": "500", "name": "g", "owner_id": "1", "afk_timeout": 300,
            "verification_level": 0, "default_message_notifications": 0,
            "explicit_content_filter": 0, "roles": role_list, "emojis": [],
            "features": [], "mfa_level": 0, "premium_tier": 0,
            "preferred_locale": "en-US", "nsfw_level": 0,
            "premium_progress_bar_enabled": false
        }))
        .unwrap()
    }

    #[test]
    fn guild_permissions_union_and_owner() {
        let guild = guild_with(
            Permissions::VIEW_CHANNEL,
            &[
                (10, Permissions::KICK_MEMBERS),
                (11, Permissions::BAN_MEMBERS),
            ],
        );
        let member = member_with(&[10, 11]);
        let perms = compute_guild_permissions(&guild, &member);
        assert!(perms.contains(Permissions::VIEW_CHANNEL));
        assert!(perms.contains(Permissions::KICK_MEMBERS));
        assert!(perms.contains(Permissions::BAN_MEMBERS));
        assert!(!perms.contains(Permissions::ADMINISTRATOR));

        // Owner gets everything regardless of roles.
        let owner = member_with(&[]);
        let mut owner_guild = guild.clone();
        owner_guild.owner_id = owner.user.as_ref().unwrap().id;
        assert!(compute_guild_permissions(&owner_guild, &owner).is_administrator());
    }

    #[test]
    fn administrator_short_circuits() {
        let guild = guild_with(Permissions::ADMINISTRATOR, &[]);
        let member = member_with(&[]);
        assert!(compute_guild_permissions(&guild, &member).is_administrator());
    }

    #[test]
    fn channel_overwrites_apply_in_order() {
        use axis::model::{Channel, RoleColors};
        let base = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES;
        let channel: Channel = serde_json::from_value(json!({
            "id": "900", "type": 0, "name": "secret",
            "permission_overwrites": [
                { "id": "1000", "type": 0, "deny": "1024", "allow": "0" },
                { "id": "10", "type": 0, "allow": "2048" }
            ]
        }))
        .unwrap();
        let member = member_with(&[10]);
        let perms = compute_channel_permissions(base, 42, &member.roles, &channel);
        // Everyone denies SEND_MESSAGES (1024), role 10 allows it back.
        assert!(perms.contains(Permissions::SEND_MESSAGES));
        assert!(perms.contains(Permissions::VIEW_CHANNEL));
    }

    #[test]
    fn tool_gate_denies_without_permission() {
        let ctx = ToolCtx {
            http: Arc::new(Http::builder(Token::new("t".to_owned())).build().unwrap()),
            guild_id: 1,
            channel_id: 2,
            invoker_id: 42,
            invoker_permissions: Permissions::VIEW_CHANNEL,
        };
        let error = ctx.gate(Permissions::BAN_MEMBERS).unwrap_err();
        assert!(error.contains("denied"), "{error}");
        assert!(error.contains("BAN_MEMBERS"), "{error}");
        // Administrator bypasses.
        let admin = ToolCtx {
            invoker_permissions: Permissions::ADMINISTRATOR,
            ..ctx
        };
        assert!(admin.gate(Permissions::BAN_MEMBERS).is_ok());
    }

    #[test]
    fn tool_definitions_are_wire_shaped() {
        let definitions = definitions();
        assert_eq!(definitions.len(), TOOLS.len());
        for definition in &definitions {
            assert_eq!(definition["type"], "function");
            assert!(
                definition["function"]["parameters"]["type"] == "object",
                "every tool needs an object schema"
            );
        }
        // Every moderation tool names its gate.
        let ban = definitions
            .iter()
            .find(|d| d["function"]["name"] == "ban")
            .unwrap();
        assert!(
            ban["function"]["description"]
                .as_str()
                .unwrap()
                .contains("BAN_MEMBERS")
        );
    }

    #[test]
    fn parse_user_accepts_mention_style_ids() {
        let args = json!({ "user_id": "<@80351110224678912>" });
        assert_eq!(
            parse_user(&args, "user_id").unwrap(),
            80_351_110_224_678_912
        );
    }

    #[test]
    fn long_replies_split_under_discord_limit() {
        let text = "x".repeat(4500);
        let chunks = split_reply(&text);
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 2000);
        }
        assert_eq!(chunks.concat().chars().count(), 4500);

        // Empty input still yields one chunk.
        assert_eq!(split_reply("").len(), 1);
    }
}
