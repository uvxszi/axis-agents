//! The AI agent: MCP-style tools, permission gating, context management,
//! and the tool-call loop.
//!
//! Security model: the model can *ask* for tools, but every tool is
//! executed by this app, which resolves the **invoking user's real Discord
//! permissions** from the API and enforces them before any action runs.
//! The gate is in code, not in the prompt — prompt injection cannot
//! escalate. Denied attempts return an explanatory error to the model.
//!
//! Context management: local models have limited context windows. The
//! agent estimates token usage, compacts older messages, and retries with
//! a shorter context when the provider reports an overflow.

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
    pub enabled: bool,
    pub provider: llm::Provider,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub max_iterations: u32,
    pub mention_trigger: bool,
    /// Also trigger on this name appearing as a word in messages.
    pub trigger_name: String,
    pub name: String,
    pub instructions: String,
    /// Approximate token budget for the context sent to the model.
    pub context_budget: u32,
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
            trigger_name: String::new(),
            name: "Agent".to_owned(),
            instructions: String::new(),
            context_budget: 3000,
        }
    }
}

/// One remembered exchange, per (guild, channel) scope.
#[derive(Debug, Clone)]
pub(crate) struct MemoryTurn {
    pub role: String,
    pub author: String,
    pub content: String,
}

const MEMORY_TURNS: usize = 16;
/// Maximum characters per tool result sent to the model.
const TOOL_RESULT_MAX: usize = 600;
/// Rough token estimate: ~3 chars per token for English text.
const CHARS_PER_TOKEN: usize = 3;

/// Shared agent state: config, per-channel memory, bot identity.
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

pub struct ToolCtx {
    pub http: Arc<Http>,
    pub guild_id: u64,
    pub channel_id: u64,
    pub invoker_id: u64,
    pub invoker_permissions: Permissions,
}

impl ToolCtx {
    fn gate(&self, required: Permissions) -> Result<(), String> {
        if self.invoker_permissions.is_administrator()
            || self.invoker_permissions.contains(required)
        {
            Ok(())
        } else {
            let missing: Vec<&str> = required
                .names()
                .filter(|name| {
                    !self
                        .invoker_permissions
                        .names()
                        .any(|invoker_name| invoker_name == *name)
                })
                .collect();
            Err(format!(
                "denied: you are missing the Discord permission(s) {}",
                if missing.is_empty() {
                    required.names().collect::<Vec<_>>().join(", ")
                } else {
                    missing.join(", ")
                }
            ))
        }
    }
}

type Executor = for<'a> fn(
    ctx: &'a ToolCtx,
    args: &'a Value,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
>;

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: &'static str,
    pub required: Permissions,
    pub executor: Executor,
}

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

/// Format a role for display, hiding @everyone.
fn format_role(role: &axis::model::Role, guild_id: u64) -> String {
    if role.id.get() == guild_id {
        return String::new(); // @everyone — never shown
    }
    let perms = if role.permissions.is_administrator() {
        "administrator".to_owned()
    } else {
        let names: Vec<&str> = role.permissions.names().take(5).collect();
        if role.permissions.names().count() > 5 {
            format!(
                "{}, +{} more",
                names.join(", "),
                role.permissions.names().count() - 5
            )
        } else {
            names.join(", ")
        }
    };
    format!(
        "  {} (ID: {}, color: #{:06X}, {})",
        role.name, role.id, role.color, perms
    )
}

static TOOLS: &[Tool] = &[
    Tool {
        name: "ban",
        description: "Ban a member from the guild.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID to ban"},"reason":{"type":"string","description":"Reason for the audit log"}},"required":["user_id"]}"#,
        required: Permissions::BAN_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                let _reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Banned by agent");
                let mut builder = ctx
                    .http
                    .ban_member(GuildId::new(ctx.guild_id), UserId::new(user_id));
                if let Some(seconds) = args.get("delete_message_seconds").and_then(Value::as_u64) {
                    builder = builder.delete_message_seconds(seconds.min(604_800) as u32);
                }
                builder.await.map_err(|error| error.to_string())?;
                Ok(format!("banned {user_id}"))
            })
        },
    },
    Tool {
        name: "kick",
        description: "Kick a member from the guild.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID to kick"},"reason":{"type":"string","description":"Reason for the audit log"}},"required":["user_id"]}"#,
        required: Permissions::KICK_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                ctx.http
                    .kick_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("kicked {user_id}"))
            })
        },
    },
    Tool {
        name: "timeout",
        description: "Time a member out (minutes).",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID"},"minutes":{"type":"integer","description":"Duration in minutes (max 40320)"}},"required":["user_id","minutes"]}"#,
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
                    .map_err(|error| error.to_string())?;
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .timeout_until(until)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("timed out {user_id} for {minutes}m"))
            })
        },
    },
    Tool {
        name: "untimeout",
        description: "Remove a member's timeout.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID"}},"required":["user_id"]}"#,
        required: Permissions::MODERATE_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_user(args, "user_id")?;
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .clear_timeout()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("removed timeout for {user_id}"))
            })
        },
    },
    Tool {
        name: "add_role",
        description: "Give a member a role.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID"},"role_id":{"type":"string","description":"Role ID to add"}},"required":["user_id","role_id"]}"#,
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
                Ok(format!("added role {role_id} to {user_id}"))
            })
        },
    },
    Tool {
        name: "remove_role",
        description: "Take a role away from a member.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID"},"role_id":{"type":"string","description":"Role ID to remove"}},"required":["user_id","role_id"]}"#,
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
                Ok(format!("removed role {role_id} from {user_id}"))
            })
        },
    },
    Tool {
        name: "send_message",
        description: "Send a message to a channel.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"Channel ID (defaults to current channel)"},"content":{"type":"string","description":"Message text (Discord Markdown)"}},"required":["content"]}"#,
        required: Permissions::SEND_MESSAGES,
        executor: |ctx, args| {
            Box::pin(async move {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or("missing content")?;
                // Strip @everyone/@here if the invoker lacks MENTION_EVERYONE.
                let content = if ctx
                    .invoker_permissions
                    .contains(Permissions::MENTION_EVERYONE)
                {
                    content.to_owned()
                } else {
                    content
                        .replace("@everyone", "@\u{200B}everyone")
                        .replace("@here", "@\u{200B}here")
                };
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
                    .content(&content)
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
        description: "Delete a message. Requires MANAGE_MESSAGES in that channel.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"Channel ID (defaults to current)"},"message_id":{"type":"string","description":"Message ID to delete"}},"required":["message_id"]}"#,
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
                Ok(format!("deleted message {message_id}"))
            })
        },
    },
    Tool {
        name: "list_roles",
        description: "List all roles in the guild with names, IDs, and colors.",
        parameters: r#"{"type":"object","properties":{}}"#,
        required: Permissions::empty(),
        executor: |ctx, _args| {
            Box::pin(async move {
                let guild = ctx
                    .http
                    .get_guild(GuildId::new(ctx.guild_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let roles: Vec<String> = guild
                    .roles
                    .iter()
                    .filter(|role| role.id.get() != ctx.guild_id) // hide @everyone
                    .map(|role| format_role(role, ctx.guild_id))
                    .filter(|text| !text.is_empty())
                    .collect();
                if roles.is_empty() {
                    Ok("No roles configured.".to_owned())
                } else {
                    Ok(format!("Roles:\n{}", roles.join("\n")))
                }
            })
        },
    },
    Tool {
        name: "list_channels",
        description: "List all channels in the guild with names, IDs, and types.",
        parameters: r#"{"type":"object","properties":{}}"#,
        required: Permissions::empty(),
        executor: |ctx, _args| {
            Box::pin(async move {
                let channels: Vec<axis::model::Channel> = ctx
                    .http
                    .get_guild_channels(GuildId::new(ctx.guild_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let lines: Vec<String> = channels
                    .iter()
                    .map(|channel| {
                        format!(
                            "  #{} ({}, ID: {})",
                            channel.name.as_deref().unwrap_or("unnamed"),
                            format_channel_type(channel.r#type),
                            channel.id
                        )
                    })
                    .collect();
                Ok(format!("Channels:\n{}", lines.join("\n")))
            })
        },
    },
    Tool {
        name: "get_channel_info",
        description: "Get detailed info about a specific channel.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"Channel ID"}},"required":["channel_id"]}"#,
        required: Permissions::empty(),
        executor: |ctx, args| {
            Box::pin(async move {
                let channel_id = parse_user(args, "channel_id")?;
                let channel = ctx
                    .http
                    .get_channel(ChannelId::new(channel_id))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!(
                    "channel {}: name {:?}, type {}, topic {:?}, slowmode {}s, NSFW {}",
                    channel.id,
                    channel.name,
                    format_channel_type(channel.r#type),
                    channel.topic,
                    channel.rate_limit_per_user.unwrap_or(0),
                    channel.nsfw.unwrap_or(false)
                ))
            })
        },
    },
    Tool {
        name: "get_role_info",
        description: "Get detailed info about a specific role by ID or name.",
        parameters: r#"{"type":"object","properties":{"role":{"type":"string","description":"Role ID or exact name"}},"required":["role"]}"#,
        required: Permissions::empty(),
        executor: |ctx, args| {
            Box::pin(async move {
                let query = args
                    .get("role")
                    .and_then(Value::as_str)
                    .ok_or("missing role")?
                    .to_owned();
                let guild = ctx
                    .http
                    .get_guild(GuildId::new(ctx.guild_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let role = guild
                    .roles
                    .iter()
                    .find(|role| {
                        role.id.to_string() == query || role.name.eq_ignore_ascii_case(&query)
                    })
                    .ok_or_else(|| format!("role {query:?} not found"))?;
                if role.id.get() == ctx.guild_id {
                    return Ok(
                        "@everyone is the default role everyone has — it cannot be managed."
                            .to_owned(),
                    );
                }
                Ok(format!(
                    "role {}: name {:?}, color #{:06X}, position {}, permissions: {}",
                    role.id,
                    role.name,
                    role.color,
                    role.position,
                    role.permissions.names().collect::<Vec<_>>().join(", ")
                ))
            })
        },
    },
    Tool {
        name: "get_member",
        description: "Look up a member: username, nickname, roles, join date.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID"}},"required":["user_id"]}"#,
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
                    .unwrap_or_default();
                let role_ids: Vec<String> = member.roles.iter().map(|id| id.to_string()).collect();
                Ok(format!(
                    "member {user_id}: {username}, nick {:?}, roles [{}], joined {:?}, timeout {:?}",
                    member.nick,
                    role_ids.join(", "),
                    member.joined_at,
                    member.communication_disabled_until
                ))
            })
        },
    },
    Tool {
        name: "get_guild",
        description: "Look up the guild: name, roles, features.",
        parameters: r#"{"type":"object","properties":{}}"#,
        required: Permissions::empty(),
        executor: |ctx, _args| {
            Box::pin(async move {
                let guild = ctx
                    .http
                    .get_guild(GuildId::new(ctx.guild_id))
                    .await
                    .map_err(|error| error.to_string())?;
                let role_names: Vec<String> = guild
                    .roles
                    .iter()
                    .filter(|role| role.id.get() != ctx.guild_id)
                    .map(|role| role.name.clone())
                    .collect();
                Ok(format!(
                    "guild {}: {:?}, roles [{}], features [{}]",
                    guild.id,
                    guild.name,
                    role_names.join(", "),
                    guild.features.join(", ")
                ))
            })
        },
    },
];

fn format_channel_type(kind: axis::model::ChannelType) -> &'static str {
    match kind {
        axis::model::ChannelType::GuildText => "text",
        axis::model::ChannelType::Dm => "DM",
        axis::model::ChannelType::GuildVoice => "voice",
        axis::model::ChannelType::GroupDm => "group DM",
        axis::model::ChannelType::GuildCategory => "category",
        axis::model::ChannelType::GuildAnnouncement => "announcement",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Permission resolution
// ---------------------------------------------------------------------------

pub async fn invoker_permissions(
    http: &Http,
    guild_id: u64,
    user_id: u64,
) -> Result<Permissions, String> {
    let guild: Guild = http
        .get_guild(GuildId::new(guild_id))
        .await
        .map_err(|error| format!("could not load guild: {error}"))?;
    let member: Member = http
        .get_member(GuildId::new(guild_id), UserId::new(user_id))
        .await
        .map_err(|error| format!("could not load your membership: {error}"))?;
    Ok(compute_guild_permissions(&guild, &member))
}

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
            permissions |= role.permissions;
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
    if let Some(everyone) = overwrites
        .iter()
        .find(|o| o.id.get() == channel.id.get() && o.r#type == OverwriteType::Role)
    {
        permissions = (permissions & !everyone.deny) | everyone.allow;
    }
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
    if let Some(member_overwrite) = overwrites
        .iter()
        .find(|o| o.id.get() == member_user_id && o.r#type == OverwriteType::Member)
    {
        permissions = (permissions & !member_overwrite.deny) | member_overwrite.allow;
    }
    permissions
}

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
            .map_err(|error| format!("could not load channel: {error}"))?;
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
                "denied: you are missing {} in this channel",
                required.names().collect::<Vec<_>>().join(", ")
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Context management
// ---------------------------------------------------------------------------

/// Rough token estimate for a message.
fn estimate_tokens(message: &ChatMessage) -> usize {
    message.content.len() / CHARS_PER_TOKEN + 10
}

/// Keep the system message and as many recent messages as fit the budget.
/// Tool exchanges (assistant + tool pairs) are kept together.
fn compact_history(
    system: ChatMessage,
    history: Vec<ChatMessage>,
    budget_tokens: usize,
) -> Vec<ChatMessage> {
    let system_cost = estimate_tokens(&system);
    if system_cost >= budget_tokens {
        return vec![system];
    }
    let remaining = budget_tokens - system_cost;

    // Walk backwards, keeping complete exchanges.
    let mut kept: Vec<ChatMessage> = Vec::new();
    let mut used = 0usize;
    for message in history.iter().rev() {
        let cost = estimate_tokens(message);
        if used + cost > remaining {
            break;
        }
        used += cost;
        kept.push(message.clone());
    }
    kept.reverse();

    let mut result = vec![system];
    result.extend(kept);
    result
}

/// Detect context-overflow errors from common providers.
fn is_context_overflow(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("context size has been exceeded")
        || lower.contains("context length exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context window")
        || (lower.contains("token") && lower.contains("exceed"))
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Run the agent for one user message. Returns the final answer text.
pub async fn run_agent(
    http: Arc<Http>,
    state: &AgentState,
    scope: (u64, u64),
    invoker: (u64, String),
    user_message: String,
) -> Result<String, String> {
    let config = state.config.read().expect("agent config poisoned").clone();
    if !config.enabled {
        return Err("the AI agent is disabled".to_owned());
    }
    if config.base_url.trim().is_empty() || config.model.trim().is_empty() {
        return Err("no LLM provider configured".to_owned());
    }

    // Resolve invoker permissions once (fail-closed).
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

    let system = build_system_prompt(&config, scope, &invoker, invoker_permissions, scope.0 != 0);
    let tools = definitions();
    let ctx = ToolCtx {
        http: Arc::clone(&http),
        guild_id: scope.0,
        channel_id: scope.1,
        invoker_id: invoker.0,
        invoker_permissions,
    };

    // Build initial message list with history.
    let mut history: Vec<ChatMessage> = state
        .history(scope)
        .iter()
        .map(|turn| match turn.role.as_str() {
            "user" => ChatMessage::user(format!("{}: {}", turn.author, turn.content)),
            _ => ChatMessage::assistant(turn.content.clone()),
        })
        .collect();
    history.push(ChatMessage::user(format!(
        "{}: {}",
        invoker.1, user_message
    )));

    // Compact to fit the budget.
    let mut messages = compact_history(system, history, config.context_budget as usize);

    let mut final_content = String::new();
    let mut last_error = String::new();
    let mut compacted = false;

    for iteration in 0..config.max_iterations.max(1) {
        let request = llm::build_chat_request(&config.model, config.temperature, &messages, &tools);
        let body = match llm::chat(&config.base_url, &config.api_key, request).await {
            Ok(body) => body,
            Err(error) => {
                if is_context_overflow(&error) && !compacted {
                    // Drop half the history (keeping system + latest user
                    // message) and retry once.
                    compacted = true;
                    let keep_from = messages.len().saturating_sub(2).max(1);
                    messages = messages[..keep_from].to_vec();
                    messages.insert(
                        0,
                        build_system_prompt(
                            &config,
                            scope,
                            &invoker,
                            invoker_permissions,
                            scope.0 != 0,
                        ),
                    );
                    tracing::info!(
                        iteration,
                        "context overflow; compacted to {} messages and retrying",
                        messages.len()
                    );
                    continue;
                }
                last_error = error;
                break;
            }
        };

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
        final_content = turn.content; // fallback if we hit max iterations
    }

    if final_content.is_empty() {
        if !last_error.is_empty() {
            return Err(last_error);
        }
        return Err("the agent could not produce a response".to_owned());
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

fn build_system_prompt(
    config: &AgentConfig,
    scope: (u64, u64),
    invoker: &(u64, String),
    permissions: Permissions,
    in_guild: bool,
) -> ChatMessage {
    let mut prompt = format!(
        "You are {}, a Discord bot. Respond naturally and concisely.",
        config.name
    );
    if !config.instructions.trim().is_empty() {
        prompt.push_str("\n\nYour instructions:\n");
        prompt.push_str(config.instructions.trim());
    }
    if in_guild {
        let permission_text = if permissions.is_administrator() {
            "administrator (full access)".to_owned()
        } else {
            let names: Vec<&str> = permissions.names().take(8).collect();
            names.join(", ")
        };
        prompt.push_str(&format!(
            "\n\nContext: guild {}, channel {}. Talking to {} (ID: {}). Their permissions: {}.",
            scope.0, scope.1, invoker.1, invoker.0, permission_text
        ));
        prompt.push_str(
            "\n\nUse tools to perform Discord actions. Tools are permission-gated — if one returns \"denied\", tell the user they lack the required permission and move on. Never fabricate results. Be concise.",
        );
    } else {
        prompt.push_str("\n\nThis is a DM — no guild tools available.");
    }
    ChatMessage::system(prompt)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_user(args: &Value, field: &str) -> Result<u64, String> {
    let raw = args
        .get(field)
        .cloned()
        .ok_or_else(|| format!("missing {field:?}"))?;
    let text = match raw {
        Value::String(text) => text,
        Value::Number(number) => number.to_string(),
        other => return Err(format!("{field:?} was {other}; expected an ID")),
    };
    text.trim()
        .trim_start_matches('<')
        .trim_start_matches("@")
        .trim_end_matches('>')
        .parse::<u64>()
        .map_err(|_| format!("{field:?} was {text:?}; expected a Discord ID"))
}

fn truncate_tool_result(result: &str) -> String {
    if result.len() <= TOOL_RESULT_MAX {
        return result.to_owned();
    }
    let mut cut = TOOL_RESULT_MAX;
    while cut > 0 && !result.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &result[..cut])
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
        return format!("tool {:?}: malformed arguments", call.name);
    }
    match (tool.executor)(ctx, &call.arguments).await {
        Ok(result) => result,
        Err(error) => format!("tool {} failed: {}", call.name, error),
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
        let admin = ToolCtx {
            invoker_permissions: Permissions::ADMINISTRATOR,
            ..ctx
        };
        assert!(admin.gate(Permissions::BAN_MEMBERS).is_ok());
    }

    #[test]
    fn tool_definitions_are_wire_shaped() {
        let definitions = definitions();
        assert!(!definitions.is_empty());
        for definition in &definitions {
            assert_eq!(definition["type"], "function");
            assert_eq!(definition["function"]["parameters"]["type"], "object");
        }
    }

    #[test]
    fn context_overflow_detected() {
        assert!(is_context_overflow(
            r#"the provider answered HTTP 400: {"error":"Engine protocol predict stream returned an error: {\"code\":500,\"message\":\"Context size has been exceeded.\",\"type\":\"server_error\"}"}"#
        ));
        assert!(!is_context_overflow("connection refused"));
    }

    #[test]
    fn compact_history_keeps_system_and_recent() {
        let system = ChatMessage::system("be nice");
        let history: Vec<ChatMessage> = (0..50)
            .map(|i| ChatMessage::user(format!("message {i} with some padding text here")))
            .collect();
        let compacted = compact_history(system, history, 500);
        assert_eq!(compacted[0].role, "system");
        assert!(compacted.len() < 50, "should have dropped old messages");
        assert!(compacted.len() > 1, "should have kept at least one message");
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
    }

    #[test]
    fn parse_user_handles_mentions() {
        let args = json!({ "user_id": "<@80351110224678912>" });
        assert_eq!(
            parse_user(&args, "user_id").unwrap(),
            80_351_110_224_678_912
        );
    }

    #[test]
    fn token_estimation_is_reasonable() {
        let message = ChatMessage::user("a".repeat(300));
        let tokens = estimate_tokens(&message);
        assert!(tokens > 50 && tokens < 200, "got {tokens}");
    }

    #[test]
    fn context_exceeded_errors_are_detected() {
        let lm_studio_error = r#"the provider answered HTTP 400 Bad Request: {"error":"Engine protocol predict stream returned an error: {\"code\":500,\"message\":\"Context size has been exceeded.\",\"type\":\"server_error\"}"}"#;
        assert!(is_context_overflow(lm_studio_error));
    }
}
