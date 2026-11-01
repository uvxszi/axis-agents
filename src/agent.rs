//! The AI agent: conversation mode, concurrency limits, permission-gated
//! MCP-style tools, and the tool-call loop.
//!
//! Security model: tools are gated in code by the invoking user's real
//! Discord permissions (resolved from the API), not by the prompt.
//! Conversation mode: after a trigger, subsequent messages go to the
//! agent without re-triggering. The agent can decline with `NO_REPLY`.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use axis::model::{Guild, Member, OverwriteType};
use axis::prelude::*;
use axis::rest::Http;
use serde_json::{Value, json};

use crate::llm::{self, ChatMessage};

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
    pub name: String,
    pub instructions: String,
    pub context_budget: u32,
    pub conversation_timeout_secs: u64,
    pub max_concurrent: u32,
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
            context_budget: 3000,
            conversation_timeout_secs: 300,
            max_concurrent: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryTurn {
    pub role: String,
    pub author: String,
    pub content: String,
}

const MEMORY_TURNS: usize = 16;
const TOOL_RESULT_MAX: usize = 600;
const NO_REPLY: &str = "NO_REPLY";

pub struct AgentState {
    pub config: RwLock<AgentConfig>,
    pub(crate) memory: Mutex<HashMap<(u64, u64), VecDeque<MemoryTurn>>>,
    pub bot_user: Mutex<Option<(u64, String)>>,
    conversations: Mutex<HashMap<(u64, u64, u64), Instant>>,
    llm_semaphore: Arc<tokio::sync::Semaphore>,
}

impl AgentState {
    pub fn new(max_concurrent: u32) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(AgentConfig::default()),
            memory: Mutex::new(HashMap::new()),
            bot_user: Mutex::new(None),
            conversations: Mutex::new(HashMap::new()),
            llm_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1) as usize)),
        })
    }

    /// Whether a message should go to the agent.
    pub fn should_process(
        &self,
        scope: (u64, u64),
        user_id: u64,
        content: &str,
        is_bot: bool,
    ) -> bool {
        if is_bot {
            return false;
        }
        // Expire conversations that timed out.
        {
            let timeout = {
                let config = self.config.read().expect("agent config poisoned");
                Duration::from_secs(config.conversation_timeout_secs)
            };
            self.conversations
                .lock()
                .expect("conversations poisoned")
                .retain(|_, last| last.elapsed() < timeout);
        }
        let config = self.config.read().expect("agent config poisoned").clone();
        if !config.enabled {
            return false;
        }

        // Active conversation.
        let key = (scope.0, scope.1, user_id);
        {
            let conversations = self.conversations.lock().expect("conversations poisoned");
            if let Some(&last) = conversations.get(&key) {
                if last.elapsed() < Duration::from_secs(config.conversation_timeout_secs) {
                    return true;
                }
            }
        }

        // Name trigger.
        let agent_name = config.name.to_lowercase();
        if !agent_name.is_empty() && content.to_lowercase().contains(&agent_name) {
            return true;
        }
        false
    }

    /// Check @mention against the bot's user ID.
    pub fn is_mentioned(&self, mentions: &[axis::model::User]) -> bool {
        let Some(bot_id) = self.bot_user_id() else {
            return false;
        };
        self.config
            .read()
            .expect("agent config poisoned")
            .mention_trigger
            && mentions.iter().any(|user| user.id.get() == bot_id)
    }

    pub fn activate_conversation(&self, scope: (u64, u64), user_id: u64) {
        self.conversations
            .lock()
            .expect("conversations poisoned")
            .insert((scope.0, scope.1, user_id), Instant::now());
    }

    fn touch_conversation(&self, scope: (u64, u64), user_id: u64) {
        self.conversations
            .lock()
            .expect("conversations poisoned")
            .insert((scope.0, scope.1, user_id), Instant::now());
    }

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
// Tools
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
            return Ok(());
        }
        let missing: Vec<&str> = required
            .names()
            .filter(|name| !self.invoker_permissions.names().any(|n| n == *name))
            .collect();
        Err(format!(
            "denied: you need the Discord permission(s) {} to do this",
            if missing.is_empty() {
                required.names().collect::<Vec<_>>().join(", ")
            } else {
                missing.join(", ")
            }
        ))
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
                serde_json::from_str(tool.parameters).expect("valid tool schema");
            json!({ "type": "function", "function": {
            "name": tool.name, "description": tool.description, "parameters": parameters } })
        })
        .collect()
}

pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.name == name)
}

fn format_role(role: &axis::model::Role, guild_id: u64) -> String {
    if role.id.get() == guild_id {
        return String::new();
    }
    let perms = if role.permissions.is_administrator() {
        "administrator".to_owned()
    } else {
        let names: Vec<&str> = role.permissions.names().take(5).collect();
        let count = role.permissions.names().count();
        if count > 5 {
            format!("{}, +{} more", names.join(", "), count - 5)
        } else {
            names.join(", ")
        }
    };
    format!(
        "  {} (ID: {}, color: #{:06X}, {})",
        role.name, role.id, role.color, perms
    )
}

fn parse_id(args: &Value, field: &str) -> Result<u64, String> {
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
        .trim_start_matches("#")
        .trim_end_matches('>')
        .parse::<u64>()
        .map_err(|_| format!("{field:?} was {text:?}; expected a Discord ID"))
}

fn parse_optional_channel(args: &Value, field: &str, default: u64) -> u64 {
    match args.get(field) {
        Some(Value::String(text)) => text.trim().parse::<u64>().unwrap_or(default),
        Some(Value::Number(number)) => number.as_u64().unwrap_or(default),
        _ => default,
    }
}

static TOOLS: &[Tool] = &[
    Tool {
        name: "ban",
        description: "Ban a member from the guild.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID to ban"},"reason":{"type":"string","description":"Reason for the audit log"}},"required":["user_id"]}"#,
        required: Permissions::BAN_MEMBERS,
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_id(args, "user_id")?;
                ctx.http
                    .ban_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|e| format!("Discord rejected the ban: {e}"))?;
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
                let user_id = parse_id(args, "user_id")?;
                ctx.http
                    .kick_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|e| format!("Discord rejected the kick: {e}"))?;
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
                let user_id = parse_id(args, "user_id")?;
                let minutes = args
                    .get("minutes")
                    .and_then(Value::as_i64)
                    .unwrap_or(10)
                    .clamp(1, 40_320);
                let millis =
                    axis::timestamp::Timestamp::now_utc().as_unix_millis() + minutes * 60_000;
                let until = axis::timestamp::Timestamp::from_unix_millis(millis)
                    .map_err(|e| e.to_string())?;
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .timeout_until(until)
                    .await
                    .map_err(|e| format!("Discord rejected the timeout: {e}"))?;
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
                let user_id = parse_id(args, "user_id")?;
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .clear_timeout()
                    .await
                    .map_err(|e| format!("Discord rejected the untimeout: {e}"))?;
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
                let user_id = parse_id(args, "user_id")?;
                let role_id = parse_id(args, "role_id")?;
                let member = ctx
                    .http
                    .get_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|e| format!("could not find that member: {e}"))?;
                let mut roles = member.roles.clone();
                roles.push(RoleId::new(role_id));
                ctx.http
                    .edit_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .roles(roles)
                    .await
                    .map_err(|e| format!("Discord rejected the role change: {e}"))?;
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
                let user_id = parse_id(args, "user_id")?;
                let role_id = parse_id(args, "role_id")?;
                let member = ctx
                    .http
                    .get_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|e| format!("could not find that member: {e}"))?;
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
                    .map_err(|e| format!("Discord rejected the role change: {e}"))?;
                Ok(format!("removed role {role_id} from {user_id}"))
            })
        },
    },
    Tool {
        name: "send_message",
        description: "Send a message to a channel. Omit channel_id for the current channel.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"Target channel ID (omit for current channel)"},"content":{"type":"string","description":"Message text (Discord Markdown)"}},"required":["content"]}"#,
        required: Permissions::SEND_MESSAGES,
        executor: |ctx, args| {
            Box::pin(async move {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or("missing content")?;
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
                let channel_id = parse_optional_channel(args, "channel_id", ctx.channel_id);
                let scope = ChannelScope::resolve(ctx, channel_id).await?;
                scope.gate(Permissions::SEND_MESSAGES)?;
                let message = ctx
                    .http
                    .create_message(ChannelId::new(channel_id))
                    .content(&content)
                    .await
                    .map_err(|e| format!("could not send: {e}"))?;
                Ok(format!(
                    "sent message {} in channel {}",
                    message.id, channel_id
                ))
            })
        },
    },
    Tool {
        name: "delete_message",
        description: "Delete a message.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"Channel ID (omit for current)"},"message_id":{"type":"string","description":"Message ID to delete"}},"required":["message_id"]}"#,
        required: Permissions::MANAGE_MESSAGES,
        executor: |ctx, args| {
            Box::pin(async move {
                let message_id = parse_id(args, "message_id")?;
                let channel_id = parse_optional_channel(args, "channel_id", ctx.channel_id);
                let scope = ChannelScope::resolve(ctx, channel_id).await?;
                scope.gate(Permissions::MANAGE_MESSAGES)?;
                ctx.http
                    .delete_message(ChannelId::new(channel_id), MessageId::new(message_id))
                    .await
                    .map_err(|e| format!("could not delete: {e}"))?;
                Ok(format!("deleted message {message_id}"))
            })
        },
    },
    Tool {
        name: "list_roles",
        description: "List all roles in the guild.",
        parameters: r#"{"type":"object","properties":{}}"#,
        required: Permissions::empty(),
        executor: |ctx, _args| {
            Box::pin(async move {
                let guild = ctx
                    .http
                    .get_guild(GuildId::new(ctx.guild_id))
                    .await
                    .map_err(|e| format!("could not load guild: {e}"))?;
                let roles: Vec<String> = guild
                    .roles
                    .iter()
                    .filter(|role| role.id.get() != ctx.guild_id)
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
        description: "List all channels with names, IDs, and types.",
        parameters: r#"{"type":"object","properties":{}}"#,
        required: Permissions::empty(),
        executor: |ctx, _args| {
            Box::pin(async move {
                let channels: Vec<axis::model::Channel> = ctx
                    .http
                    .get_guild_channels(GuildId::new(ctx.guild_id))
                    .await
                    .map_err(|e| format!("could not load channels: {e}"))?;
                let lines: Vec<String> = channels
                    .iter()
                    .map(|c| {
                        format!(
                            "  #{} ({}, ID: {})",
                            c.name.as_deref().unwrap_or("unnamed"),
                            format_channel_type(c.r#type),
                            c.id
                        )
                    })
                    .collect();
                Ok(format!("Channels:\n{}", lines.join("\n")))
            })
        },
    },
    Tool {
        name: "get_channel_info",
        description: "Get detailed info about a channel.",
        parameters: r#"{"type":"object","properties":{"channel_id":{"type":"string","description":"Channel ID"}},"required":["channel_id"]}"#,
        required: Permissions::empty(),
        executor: |ctx, args| {
            Box::pin(async move {
                let channel_id = parse_id(args, "channel_id")?;
                let channel = ctx
                    .http
                    .get_channel(ChannelId::new(channel_id))
                    .await
                    .map_err(|e| format!("could not load channel: {e}"))?;
                Ok(format!(
                    "channel {}: name {:?}, type {}, topic {:?}",
                    channel.id,
                    channel.name,
                    format_channel_type(channel.r#type),
                    channel.topic
                ))
            })
        },
    },
    Tool {
        name: "get_role_info",
        description: "Get detailed info about a role by ID or name.",
        parameters: r#"{"type":"object","properties":{"role":{"type":"string","description":"Role ID or name"}},"required":["role"]}"#,
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
                    .map_err(|e| format!("could not load guild: {e}"))?;
                let role = guild
                    .roles
                    .iter()
                    .find(|r| r.id.to_string() == query || r.name.eq_ignore_ascii_case(&query))
                    .ok_or_else(|| format!("role {query:?} not found"))?;
                if role.id.get() == ctx.guild_id {
                    return Ok("@everyone is the default role all members have.".to_owned());
                }
                Ok(format!(
                    "role {}: name {:?}, color #{:06X}, permissions: {}",
                    role.id,
                    role.name,
                    role.color,
                    role.permissions.names().collect::<Vec<_>>().join(", ")
                ))
            })
        },
    },
    Tool {
        name: "get_member",
        description: "Look up a member's username, nickname, roles, and join date.",
        parameters: r#"{"type":"object","properties":{"user_id":{"type":"string","description":"Discord user ID"}},"required":["user_id"]}"#,
        required: Permissions::empty(),
        executor: |ctx, args| {
            Box::pin(async move {
                let user_id = parse_id(args, "user_id")?;
                let member = ctx
                    .http
                    .get_member(GuildId::new(ctx.guild_id), UserId::new(user_id))
                    .await
                    .map_err(|e| format!("could not find that member: {e}"))?;
                let username = member
                    .user
                    .as_ref()
                    .map(|u| u.username.as_str())
                    .unwrap_or("unknown");
                let nickname = member.nick.as_deref().unwrap_or(username);
                let role_ids: Vec<String> = member.roles.iter().map(|id| id.to_string()).collect();
                Ok(format!(
                    "member {user_id}: username {username:?}, nickname {nickname:?}, roles [{}], joined {:?}, timeout {:?}",
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
                    .map_err(|e| format!("could not load guild: {e}"))?;
                let role_names: Vec<String> = guild
                    .roles
                    .iter()
                    .filter(|r| r.id.get() != ctx.guild_id)
                    .map(|r| r.name.clone())
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
        .map_err(|e| format!("could not load guild: {e}"))?;
    let member: Member = http
        .get_member(GuildId::new(guild_id), UserId::new(user_id))
        .await
        .map_err(|e| format!("could not load your membership: {e}"))?;
    Ok(compute_guild_permissions(&guild, &member))
}

pub fn compute_guild_permissions(guild: &Guild, member: &Member) -> Permissions {
    let member_id = member.user.as_ref().map(|u| u.id.get()).unwrap_or_default();
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
            && member_roles.iter().any(|r| r.get() == overwrite.id.get())
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
            .map_err(|e| format!("could not load your membership: {e}"))?;
        let channel = ctx
            .http
            .get_channel(ChannelId::new(channel_id))
            .await
            .map_err(|e| format!("could not load channel: {e}"))?;
        let member_roles = member.roles.clone();
        let member_id = member
            .user
            .as_ref()
            .map(|u| u.id.get())
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
            return Ok(());
        }
        Err(format!(
            "denied: you are missing {} in this channel",
            required.names().collect::<Vec<_>>().join(", ")
        ))
    }
}

// ---------------------------------------------------------------------------
// Context management
// ---------------------------------------------------------------------------

fn estimate_tokens(message: &ChatMessage) -> usize {
    message.content.len() / 3 + 10
}

fn compact_history(
    system: ChatMessage,
    history: Vec<ChatMessage>,
    budget: usize,
) -> Vec<ChatMessage> {
    let system_cost = estimate_tokens(&system);
    if system_cost >= budget {
        return vec![system];
    }
    let remaining = budget - system_cost;
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

pub async fn run_agent(
    http: Arc<Http>,
    state: &AgentState,
    scope: (u64, u64),
    invoker: (u64, String),
    invoker_nick: Option<String>,
    user_message: String,
) -> Result<Option<String>, String> {
    let config = state.config.read().expect("agent config poisoned").clone();
    if !config.enabled {
        return Err("the AI agent is disabled".to_owned());
    }
    if config.base_url.trim().is_empty() || config.model.trim().is_empty() {
        return Err("no LLM provider configured".to_owned());
    }

    let _permit = state
        .llm_semaphore
        .acquire()
        .await
        .map_err(|_| "the agent is shutting down".to_owned())?;

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

    let display_name = invoker_nick.clone().unwrap_or_else(|| invoker.1.clone());
    let system = build_system_prompt(
        &config,
        scope,
        &invoker,
        &display_name,
        invoker_permissions,
        scope.0 != 0,
    );
    let tools = definitions();
    let ctx = ToolCtx {
        http: Arc::clone(&http),
        guild_id: scope.0,
        channel_id: scope.1,
        invoker_id: invoker.0,
        invoker_permissions,
    };

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
        display_name, user_message
    )));
    let mut messages = compact_history(system, history, config.context_budget as usize);

    let mut final_content = String::new();
    let mut compacted = false;

    for _ in 0..config.max_iterations.max(1) {
        let request = llm::build_chat_request(&config.model, config.temperature, &messages, &tools);
        let body = match llm::chat(&config.base_url, &config.api_key, request).await {
            Ok(body) => body,
            Err(error) => {
                if is_context_overflow(&error) && !compacted {
                    compacted = true;
                    let keep_from = messages.len().saturating_sub(2).max(1);
                    let system = messages[0].clone();
                    messages = messages[keep_from..].to_vec();
                    messages.insert(0, system);
                    tracing::info!("context overflow; compacted and retrying");
                    continue;
                }
                return Err(error);
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
        final_content = turn.content;
    }

    if final_content.trim() == NO_REPLY {
        state.touch_conversation(scope, invoker.0);
        return Ok(None);
    }
    if final_content.is_empty() {
        return Err("the agent could not produce a response".to_owned());
    }

    state.remember(
        scope,
        MemoryTurn {
            role: "user".to_owned(),
            author: display_name.clone(),
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
    state.touch_conversation(scope, invoker.0);

    Ok(Some(final_content))
}

fn build_system_prompt(
    config: &AgentConfig,
    scope: (u64, u64),
    invoker: &(u64, String),
    display_name: &str,
    permissions: Permissions,
    in_guild: bool,
) -> ChatMessage {
    let mut prompt = format!(
        "You are {}, a Discord bot. You respond naturally and conversationally. Never dump raw data — summarize findings in your own words. Keep responses short (1-3 paragraphs).",
        config.name
    );
    if !config.instructions.trim().is_empty() {
        prompt.push_str("\n\nYour operator's instructions:\n");
        prompt.push_str(config.instructions.trim());
    }
    if in_guild {
        let perm_text = if permissions.is_administrator() {
            "administrator (full access)".to_owned()
        } else {
            permissions.names().take(8).collect::<Vec<_>>().join(", ")
        };
        prompt.push_str(&format!(
            "\n\nContext: guild {gid}, channel {cid}. Talking to {name} (ID: {uid}). Their Discord permissions: {perms}.",
            gid = scope.0, cid = scope.1, name = display_name, uid = invoker.0, perms = perm_text,
        ));
        prompt.push_str("\n\nTools are permission-gated to them — if one returns \"denied\", tell them they lack the required Discord permission. Never fabricate results.");
        prompt.push_str("\n\nIf the message is not directed at you or you have nothing useful to add, respond with exactly NO_REPLY and nothing else.");
    } else {
        prompt.push_str("\n\nThis is a DM — no guild tools available.");
    }
    ChatMessage::system(prompt)
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
            "deaf": false, "mute": false }))
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
            "premium_progress_bar_enabled": false }))
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
            r#"HTTP 400: Context size has been exceeded."#
        ));
        assert!(!is_context_overflow("connection refused"));
    }

    #[test]
    fn compact_history_keeps_system_and_recent() {
        let system = ChatMessage::system("be nice");
        let history: Vec<ChatMessage> = (0..50)
            .map(|i| ChatMessage::user(format!("message {i} with padding text here")))
            .collect();
        let compacted = compact_history(system, history, 500);
        assert_eq!(compacted[0].role, "system");
        assert!(compacted.len() < 50);
        assert!(compacted.len() > 1);
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
    fn parse_id_handles_mentions() {
        let args = json!({ "user_id": "<@80351110224678912>" });
        assert_eq!(parse_id(&args, "user_id").unwrap(), 80_351_110_224_678_912);
    }

    #[test]
    fn parse_channel_id_handles_both_types() {
        assert_eq!(
            parse_optional_channel(&json!({ "channel_id": 123 }), "channel_id", 0),
            123
        );
        assert_eq!(
            parse_optional_channel(&json!({ "channel_id": "456" }), "channel_id", 0),
            456
        );
        assert_eq!(parse_optional_channel(&json!({}), "channel_id", 789), 789);
    }

    #[test]
    fn conversation_expiration() {
        let state = AgentState::new(2);
        {
            let mut config = state.config.write().unwrap();
            config.enabled = true;
            config.name = "test".to_owned();
        }
        state.activate_conversation((1, 2), 42);
        assert!(state.should_process((1, 2), 42, "hello", false));
        assert!(!state.should_process((1, 2), 99, "hello", false));
    }

    #[test]
    fn agent_name_is_trigger() {
        let state = AgentState::new(2);
        {
            let mut config = state.config.write().unwrap();
            config.enabled = true;
            config.name = "magi".to_owned();
        }
        assert!(state.should_process((1, 2), 42, "hey magi what's up", false));
        assert!(state.should_process((1, 2), 42, "MAGI help me", false));
        assert!(!state.should_process((1, 2), 42, "just chatting", false));
    }
}
