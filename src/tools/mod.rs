//! The agent's tools and the gate every call passes through.
//!
//! A tool runs only if:
//! 1. its group is enabled for the agent,
//! 2. the requester may use it — a person needs the real Discord
//!    permissions in that channel; an agent acting on its own initiative
//!    needs a high enough autonomy level; MCP tools need the server's
//!    access level,
//! 3. tool-specific checks pass (role hierarchy, own-message rules, …).
//!
//! The model never sees these rules as optional: a denied call returns a
//! "denied: …" result it has to relay.

pub mod discord;
pub mod docs;
pub mod documents;
pub mod extra;
pub mod files;
pub mod gifs;
pub mod images;
pub mod longform;
pub mod mcp;
pub mod power;
pub mod rankcard;
pub mod reach;
pub mod scratch;
pub mod users;
pub mod web;

use std::sync::{Arc, OnceLock};

use axis::prelude::{AxisError, Permissions};
use serde_json::{Value, json};

use crate::config::{AccessLevel, Autonomy, ToolConfig};
use crate::llm::{ToolCall, ToolSpec};
use crate::runtime::agent::AgentCore;
use crate::runtime::authority::{Principal, PrincipalKind, missing};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    ServerInfo,
    Messaging,
    Reactions,
    Threads,
    Polls,
    Voice,
    Moderation,
    Roles,
    Channels,
    WebSearch,
    WebFetch,
    Memory,
    Coordination,
    Utilities,
    Images,
}

impl Group {
    pub fn enabled(self, config: &ToolConfig) -> bool {
        match self {
            Group::ServerInfo => config.server_info,
            Group::Messaging => config.messaging,
            Group::Reactions => config.reactions,
            Group::Threads => config.threads,
            Group::Polls => config.polls,
            Group::Voice => config.voice,
            Group::Moderation => config.moderation,
            Group::Roles => config.roles,
            Group::Channels => config.channels,
            Group::WebSearch => config.web_search,
            Group::WebFetch => config.web_fetch,
            Group::Memory => config.memory,
            Group::Coordination => config.coordination,
            Group::Utilities => config.utilities,
            Group::Images => config.images,
        }
    }
}

pub struct ToolDef {
    pub name: &'static str,
    pub group: Group,
    pub description: &'static str,
    pub params: Value,
    /// Channel permissions a person must have to request this.
    pub requires: Permissions,
    /// Autonomy an agent needs to do this on its own initiative.
    pub autonomy: Autonomy,
    pub guild_only: bool,
    /// Always offered (when enabled), even to small models.
    pub core: bool,
    /// Words that make the tool relevant to a message.
    pub keywords: &'static [&'static str],
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

fn prop(kind: &str, description: &str) -> Value {
    json!({ "type": kind, "description": description })
}

macro_rules! tool {
    ($name:literal, $group:ident, $autonomy:ident, $requires:expr, guild: $guild:literal, core: $core:literal,
     keywords: [$($kw:literal),*], $description:literal, { $($pname:literal : ($pkind:literal, $pdesc:literal)),* $(,)? }, required: [$($req:literal),*]) => {
        ToolDef {
            name: $name,
            group: Group::$group,
            description: $description,
            params: schema(json!({ $($pname: prop($pkind, $pdesc)),* }), &[$($req),*]),
            requires: $requires,
            autonomy: Autonomy::$autonomy,
            guild_only: $guild,
            core: $core,
            keywords: &[$($kw),*],
        }
    };
}

pub fn catalog() -> &'static [ToolDef] {
    static CATALOG: OnceLock<Vec<ToolDef>> = OnceLock::new();
    CATALOG.get_or_init(build_catalog)
}

fn build_catalog() -> Vec<ToolDef> {
    let none = Permissions::empty();
    let mut tools = vec![
        // --- server info -----------------------------------------------------
        tool!("get_server_info", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["server", "guild", "members", "how many", "owner", "boost"],
            "Overview of this server: name, owner, member count, channel/role counts, rules channel.", {}, required: []),
        tool!("list_channels", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["channel", "channels", "where", "category", "forum", "voice"],
            "List the server's channels (name, type, category) that the requester can see.", {}, required: []),
        tool!("list_roles", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["role", "roles", "rank", "staff", "mods", "admins"],
            "List the server's roles from highest to lowest with member counts.", {}, required: []),
        tool!("get_member", ServerInfo, Conversational, none, guild: true, core: true,
            keywords: ["who", "member", "user", "joined", "roles", "profile"],
            "Look up a member: name, nickname, roles, join date, timeout, voice channel.",
            { "user": ("string", "Mention, user ID, or name") }, required: ["user"]),
        tool!("list_members", ServerInfo, Conversational, none, guild: false, core: false,
            keywords: ["members", "who has", "list", "people", "staff", "everyone in", "member list"],
            "List members of this server or any other server you're in (server: its name) — all of them, or only those with a role or whose name matches.",
            { "server": ("string", "Another server's name (omit for this one)"), "role": ("string", "Only members with this role"), "query": ("string", "Name to search for"), "limit": ("integer", "Max results here (default 25)") },
            required: []),
        tool!("read_messages", ServerInfo, Conversational, Permissions::READ_MESSAGE_HISTORY, guild: false, core: true,
            keywords: ["earlier", "said", "messages", "history", "read", "summar", "recap", "context", "scroll", "above", "latest", "last", "update", "news", "changelog", "release", "version", "announce", "channel", "what's new", "check"],
            "Read recent messages in a channel (newest last). Use it whenever a question is about what's in a channel — latest updates, announcements, what someone said — instead of answering from memory. \"What I/they said above\" means the last few messages: start with limit 10 and only page further back (before_message_id) if those don't cover it.",
            { "channel": ("string", "Channel name, e.g. general (omit for this one)"), "limit": ("integer", "How many (1-100, default 30)"), "before_message_id": ("string", "Only messages before this ID") },
            required: []),
        tool!("search_messages", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["search", "find", "said", "mentioned", "who said", "when did"],
            "Search recent message history (across channels the requester can see) for words.",
            { "query": ("string", "Words to look for"), "channel": ("string", "Limit to one channel"), "author": ("string", "Limit to one author") },
            required: ["query"]),
        tool!("list_forum_posts", ServerInfo, Conversational, Permissions::READ_MESSAGE_HISTORY, guild: true, core: false,
            keywords: ["forum", "post", "posts", "guideline", "guidelines", "thread", "threads", "bug report", "suggestion"],
            "Read a forum channel: its post guidelines and tags, and each open post's title, tags, author, and opening message. Use this to check posts against the guidelines; then reply inside a post with send_message(channel = post id).",
            { "forum": ("string", "The forum channel"), "limit": ("integer", "How many newest posts (default 10, max 25)") },
            required: ["forum"]),
        tool!("get_pinned_messages", ServerInfo, Conversational, Permissions::READ_MESSAGE_HISTORY, guild: false, core: false,
            keywords: ["pin", "pinned", "pins"],
            "List a channel's pinned messages.", { "channel": ("string", "Channel name, e.g. general (omit for this one)") }, required: []),
        tool!("list_threads", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["thread", "threads", "forum", "post", "posts"],
            "List active threads and forum posts.", {}, required: []),
        tool!("voice_overview", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["voice", "vc", "who's in"],
            "Show who is in which voice channel. Only for questions about VOICE channels — who is in a call, who's in VC. Never for text-channel questions or general chat.", {}, required: []),
        // --- messaging -------------------------------------------------------
        tool!("send_message", Messaging, Helper, Permissions::SEND_MESSAGES, guild: false, core: false,
            keywords: ["send", "post", "announce", "tell", "say", "message", "channel"],
            "Send a message to another channel or thread. (Your normal reply already goes to the current channel — don't use this for it.)",
            { "channel": ("string", "Target channel's name, e.g. general — the one they named, not this one"), "content": ("string", "Message text (Discord markdown)"), "reply_to": ("string", "Message ID to reply to") },
            required: ["channel", "content"]),
        tool!("send_dm", Messaging, Helper, none, guild: false, core: false,
            keywords: ["dm", "direct message", "message them", "tell them", "privately", "send"],
            "DM someone, found by name across your servers (or one server) — works from DMs too. Moderators of a server they share can ask; anyone can ask to be DMed themselves. If the name is ambiguous you'll get the look-alikes to ask about.",
            { "user": ("string", "Their name, @username, or ID"), "content": ("string", "The message"), "server": ("string", "The server they're in (helps tell people apart)") }, required: ["user", "content"]),
        tool!("edit_my_message", Messaging, Conversational, none, guild: false, core: false,
            keywords: ["edit", "fix", "change your message", "typo"],
            "Edit one of your own earlier messages.",
            { "message_id": ("string", "Your message's ID"), "content": ("string", "New text"), "channel": ("string", "Channel name, e.g. general (omit for this one)") },
            required: ["message_id", "content"]),
        tool!("delete_my_message", Messaging, Conversational, none, guild: false, core: false,
            keywords: ["delete your", "remove your", "undo", "delete it", "your message", "you have a message", "you posted", "delete that"],
            "Delete one of your own earlier messages. Without message_id it finds your latest message in that channel (including posts inside a forum/media channel), or the one containing some text.",
            { "channel": ("string", "Channel name, e.g. media (omit to search everywhere)"), "contains": ("string", "Text the message contains"), "message_id": ("string", "The exact message ID, if known") }, required: []),
        tool!("pin_message", Messaging, Helper, Permissions::PIN_MESSAGES, guild: false, core: false,
            keywords: ["pin", "unpin"],
            "Pin or unpin a message.",
            { "message_id": ("string", "Message ID (omit for the message you're answering)"), "channel": ("string", "Channel name, e.g. general (omit for this one)"), "unpin": ("boolean", "Unpin instead") },
            required: []),
        // --- reactions -------------------------------------------------------
        tool!("react", Reactions, Conversational, Permissions::ADD_REACTIONS, guild: false, core: true,
            keywords: ["react", "emoji", "reaction"],
            "React to a message with an emoji (unicode like 👍 or a custom <:name:id>). With no channel/message_id it reacts to the message you're answering. To react to a forum post or thread, pass channel = its id. For any other message, pass channel and message_id.",
            { "emoji": ("string", "The emoji"), "message_id": ("string", "Message ID (omit for the message you're answering)"), "channel": ("string", "Channel name, e.g. general (omit for this one)"), "remove": ("boolean", "Remove your reaction instead") },
            required: ["emoji"]),
        // --- polls -----------------------------------------------------------
        tool!("create_poll", Polls, Helper, Permissions::SEND_POLLS, guild: false, core: false,
            keywords: ["poll", "vote", "survey", "decide"],
            "Post a native Discord poll.",
            { "question": ("string", "The question"), "answers": ("array", "2-10 answers"), "hours": ("integer", "Duration in hours (default 24)"), "multiple": ("boolean", "Allow picking several"), "channel": ("string", "Channel name, e.g. general (omit for this one)") },
            required: ["question", "answers"]),
        tool!("create_vote", Polls, Helper, Permissions::SEND_MESSAGES, guild: true, core: true,
            keywords: ["vote", "voting", "poll", "ballot", "election", "survey", "decide", "anonymous vote", "options", "button vote", "start vote"],
            "Start an interactive button-based vote in a channel. Supports normal lists of options (as an array or text list), custom end times (e.g. '10m', 'in 2 hours', 'tomorrow'), anonymous voting (secret ballot where votes are hidden until the vote ends), and interactive Discord buttons for voting. Can be ended early with end_vote.",
            { "question": ("string", "The question or proposal to vote on"), "options": ("array", "List of 2-25 options (e.g. [\"Option 1\", \"Option 2\"] or separated by semicolons/newlines)"), "ends": ("string", "When it ends, e.g. 'in 2 hours', '10m', 'tomorrow' (default in 24 hours)"), "anonymous": ("boolean", "Anonymous voting: votes are secret and only revealed when the vote ends (default false)"), "channel": ("string", "Channel to post the vote in (omit for this one)") },
            required: ["question", "options"]),
        tool!("end_vote", Polls, Helper, Permissions::SEND_MESSAGES, guild: true, core: true,
            keywords: ["end vote", "close vote", "stop vote", "finish vote", "tally vote", "vote results"],
            "End a running vote early and announce the final results (unsealing votes if anonymous).",
            { "vote": ("string", "Question words or message ID of the vote (omit for the latest running vote)"), "channel": ("string", "Channel where the vote is running (omit for this one)") },
            required: []),
        tool!("list_votes", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["list votes", "votes", "active votes", "running votes", "show votes"],
            "List active or recent votes with options, anonymous status, voter counts, and end times.",
            { "include_ended": ("boolean", "Include concluded votes"), "channel": ("string", "Filter to one channel (omit for all)") },
            required: []),
        // --- threads & forums -----------------------------------------------
        tool!("create_thread", Threads, Helper, Permissions::CREATE_PUBLIC_THREADS, guild: true, core: false,
            keywords: ["thread", "side conversation", "spin off", "discuss"],
            "Start a thread, from a message (default: the one you're answering) or standalone.",
            { "name": ("string", "Thread name"), "from_message_id": ("string", "Start from this message (or 'none' for a standalone thread)"), "private": ("boolean", "Private thread"), "channel": ("string", "Parent channel (omit for this one)"), "first_message": ("string", "Optional first message to post in it") },
            required: ["name"]),
        tool!("create_forum_post", Threads, Helper, Permissions::SEND_MESSAGES, guild: true, core: false,
            keywords: ["forum", "post", "suggestion", "ticket", "report", "faq"],
            "Create a post in a forum channel.",
            { "forum": ("string", "Forum channel"), "title": ("string", "Post title"), "content": ("string", "Post body"), "tags": ("array", "Tag names to apply") },
            required: ["forum", "title", "content"]),
        tool!("archive_thread", Threads, Helper, Permissions::MANAGE_THREADS, guild: true, core: false,
            keywords: ["archive", "close thread", "lock thread", "resolve"],
            "Archive (and optionally lock) a thread.",
            { "thread": ("string", "Thread (omit for this one)"), "lock": ("boolean", "Also lock it") }, required: []),
        tool!("add_to_thread", Threads, Helper, Permissions::SEND_MESSAGES_IN_THREADS, guild: true, core: false,
            keywords: ["add to thread", "invite"],
            "Add a member to a thread.",
            { "user": ("string", "Who to add"), "thread": ("string", "Thread (omit for this one)") }, required: ["user"]),
        // --- voice -----------------------------------------------------------
        tool!("join_voice", Voice, Conversational, none, guild: true, core: false,
            keywords: ["join", "vc", "voice", "hop in", "call", "come to", "talk to me", "switch", "go to", "move to", "yourself"],
            "Join a voice channel yourself to listen and talk there — also how you switch yourself to another voice channel (move_member only moves other people). Without a channel: the voice channel whose chat this is, else the one the requester is in.",
            { "channel": ("string", "Voice channel name (omit for the requester's channel)") }, required: []),
        tool!("leave_voice", Voice, Conversational, none, guild: true, core: false,
            keywords: ["leave", "vc", "voice", "disconnect", "get out", "hang up"],
            "Leave the voice channel you're in.",
            {}, required: []),
        tool!("say_in_voice", Voice, Conversational, none, guild: true, core: false,
            keywords: ["say", "speak", "vc", "voice", "out loud", "announce"],
            "Say something out loud in the voice channel you're in (text-to-speech).",
            { "text": ("string", "What to say") }, required: ["text"]),
        tool!("list_voices", Voice, Conversational, none, guild: false, core: false,
            keywords: ["voice", "voices", "sound like", "accent", "speak like"],
            "List the voices you can speak with (and which one you use now), so people can pick one.",
            {}, required: []),
        tool!("set_voice", Voice, Conversational, none, guild: false, core: false,
            keywords: ["voice", "voices", "sound like", "accent", "change your voice", "speak like", "talk like", "faster", "slower"],
            "Change your own speaking voice: a voice ID from list_voices, or a description like \"british male\" or \"soft female\". Optionally change your speaking speed.",
            { "voice": ("string", "Voice ID or description (omit to keep the voice)"), "speed": ("number", "Speaking speed 0.5-2.0 (1 = normal)") }, required: []),
        tool!("move_member", Voice, Moderator, Permissions::MOVE_MEMBERS, guild: true, core: false,
            keywords: ["move", "vc", "voice", "drag", "channel"],
            "Move someone (who is in voice) to another voice channel. Anyone may ask to move themselves to a channel they can join.",
            { "user": ("string", "Who to move ('me' for the requester)"), "channel": ("string", "Destination voice channel") }, required: ["user", "channel"]),
        tool!("disconnect_member", Voice, Moderator, Permissions::MOVE_MEMBERS, guild: true, core: false,
            keywords: ["disconnect", "kick from vc", "voice", "vc"],
            "Disconnect someone from voice. Anyone may ask to disconnect themselves.",
            { "user": ("string", "Who ('me' for the requester)") }, required: ["user"]),
        tool!("voice_mute", Voice, Moderator, Permissions::MUTE_MEMBERS, guild: true, core: false,
            keywords: ["mute", "unmute", "deafen", "undeafen", "voice"],
            "Server-mute/unmute or deafen/undeafen someone in voice.",
            { "user": ("string", "Who"), "mute": ("boolean", "Mute (true) or unmute (false)"), "deafen": ("boolean", "Deafen (true) or undeafen (false)") }, required: ["user"]),
        // --- moderation ------------------------------------------------------
        tool!("delete_message", Moderation, Moderator, Permissions::MANAGE_MESSAGES, guild: false, core: false,
            keywords: ["delete", "remove message", "spam", "clean"],
            "Delete a message. People may always ask you to delete their own messages.",
            { "message_id": ("string", "Message ID"), "channel": ("string", "Channel name, e.g. general (omit for this one)"), "reason": ("string", "Audit log reason") }, required: ["message_id"]),
        tool!("purge_messages", Moderation, Moderator, Permissions::MANAGE_MESSAGES, guild: true, core: false,
            keywords: ["purge", "clear", "clean up", "bulk delete", "spam", "delete all", "delete the messages", "delete messages", "wipe", "all the messages", "all messages"],
            "Delete messages in bulk: the latest N (up to 1000), or count \"all\" to clear a whole channel. Recent ones go in batches of 100; ones older than 14 days are deleted one by one in the background. Optionally only one user's messages. When someone asks to clear or delete messages, just call this — don't ask for message IDs.",
            { "count": ("string", "How many recent messages (1-1000), or \"all\""), "user": ("string", "Only this user's messages"), "channel": ("string", "Channel name, e.g. general (omit for this one)"), "reason": ("string", "Reason") }, required: ["count"]),
        tool!("warn_member", Moderation, Moderator, Permissions::MANAGE_MESSAGES, guild: true, core: false,
            keywords: ["warn", "warning", "strike"],
            "Formally warn a member: records a strike and tells them why.",
            { "user": ("string", "Who"), "reason": ("string", "Why") }, required: ["user", "reason"]),
        tool!("timeout_member", Moderation, Moderator, Permissions::MODERATE_MEMBERS, guild: true, core: false,
            keywords: ["timeout", "mute", "shut up", "silence", "time out"],
            "Time a member out (they can't talk) for some minutes, or lift a timeout with minutes=0.",
            { "user": ("string", "Who"), "minutes": ("integer", "Duration (0 lifts the timeout, max 40320)"), "reason": ("string", "Why") }, required: ["user", "minutes"]),
        tool!("kick_member", Moderation, Manager, Permissions::KICK_MEMBERS, guild: true, core: false,
            keywords: ["kick", "remove from server"],
            "Kick a member from the server.", { "user": ("string", "Who"), "reason": ("string", "Why") }, required: ["user", "reason"]),
        tool!("ban_member", Moderation, Manager, Permissions::BAN_MEMBERS, guild: true, core: false,
            keywords: ["ban", "unban"],
            "Ban a member (optionally deleting their recent messages), or unban with unban=true.",
            { "user": ("string", "Who (user ID works for people no longer in the server)"), "reason": ("string", "Why"), "delete_message_hours": ("integer", "Delete their messages from the last N hours (max 168)"), "unban": ("boolean", "Lift a ban instead") },
            required: ["user"]),
        tool!("set_slowmode", Moderation, Moderator, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["slowmode", "slow mode", "slow down", "calm"],
            "Set a channel's slowmode (seconds between messages per user; 0 turns it off).",
            { "seconds": ("integer", "0-21600"), "channel": ("string", "Channel name, e.g. general (omit for this one)") }, required: ["seconds"]),
        tool!("lock_channel", Moderation, Moderator, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["lock", "unlock", "lockdown", "raid"],
            "Lock a channel so regular members can't send messages, or unlock it.",
            { "channel": ("string", "Channel name, e.g. general (omit for this one)"), "locked": ("boolean", "true to lock, false to unlock"), "reason": ("string", "Why") }, required: ["locked"]),
        tool!("get_strikes", Moderation, Conversational, Permissions::MANAGE_MESSAGES, guild: true, core: false,
            keywords: ["strikes", "warnings", "history", "record", "infractions"],
            "Show a member's moderation history (strikes and actions).", { "user": ("string", "Who") }, required: ["user"]),
        // --- roles -----------------------------------------------------------
        tool!("add_role", Roles, Manager, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["role", "give", "assign", "add role"],
            "Give a member a role (the role must be below yours and the bot's top role).",
            { "user": ("string", "Who"), "role": ("string", "Role name, mention, or ID") }, required: ["user", "role"]),
        tool!("remove_role", Roles, Manager, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["role", "remove role", "take", "revoke"],
            "Remove a role from a member.", { "user": ("string", "Who"), "role": ("string", "Role") }, required: ["user", "role"]),
        tool!("create_role", Roles, Manager, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["create role", "new role", "make a role"],
            "Create a new role (no permissions).",
            { "name": ("string", "Role name"), "color": ("string", "Hex color like #ff8800"), "mentionable": ("boolean", "Allow anyone to mention it"), "hoist": ("boolean", "Show separately in the member list") },
            required: ["name"]),
        // --- channels --------------------------------------------------------
        tool!("clone_channel", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["duplicate", "clone", "copy", "copy of", "same as", "channel"],
            "Duplicate a channel: a new channel with the same type, category, topic, settings, and permission overwrites (messages aren't copied).",
            { "channel": ("string", "Channel to copy (omit for this one)"), "name": ("string", "Name for the copy") }, required: []),
        tool!("move_channel", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["move", "reorder", "order", "above", "below", "top", "bottom", "position", "put the", "category", "under", "into"],
            "Reorder channels and categories: put one above/below another, at the top or bottom, or move a channel into another category.",
            { "channel": ("string", "Channel or category to move (omit for this one)"), "above": ("string", "Put it right above this channel/category"), "below": ("string", "Put it right below this channel/category"), "category": ("string", "Move a channel into this category (\"none\" for no category)"), "position": ("string", "top or bottom"), "sync_permissions": ("boolean", "Take the new category's permissions") }, required: []),
        tool!("set_voice_status", Voice, Helper, none, guild: true, core: false,
            keywords: ["status", "voice status", "channel status", "vc status", "set the status"],
            "Set (or clear, with an empty status) a voice channel's status text shown under its name.",
            { "channel": ("string", "Voice channel (omit for the one you're in or the requester is in)"), "status": ("string", "Status text; empty clears it") }, required: []),
        tool!("set_channel_permissions", Channels, Manager, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["permission", "permissions", "access", "allow", "deny", "hide", "private", "can't see", "can see", "lock"],
            "Set a role's or member's permission overwrites in a channel (allow/deny lists like \"view channel, send messages\"), or reset them.",
            { "channel": ("string", "Channel (omit for this one)"), "target": ("string", "Role name, member, or everyone"), "allow": ("string", "Permissions to allow, comma-separated"), "deny": ("string", "Permissions to deny, comma-separated"), "reset": ("boolean", "Remove this target's overwrite") }, required: ["target"]),
        tool!("edit_thread", Threads, Helper, Permissions::MANAGE_THREADS, guild: true, core: false,
            keywords: ["thread", "post", "rename", "archive", "lock", "tag", "tags"],
            "Rename, archive/unarchive, lock/unlock a thread or forum post, or set a forum post's tags by name.",
            { "thread": ("string", "Thread or post (omit for this one)"), "name": ("string", "New name"), "archived": ("boolean", "Archive"), "locked": ("boolean", "Lock"), "tags": ("string", "Forum tag names, comma-separated") }, required: []),
        tool!("unpin_message", Messaging, Helper, Permissions::MANAGE_MESSAGES, guild: true, core: false,
            keywords: ["unpin"],
            "Unpin a message.",
            { "message_id": ("string", "Message ID"), "channel": ("string", "Channel (omit for this one)") }, required: ["message_id"]),
        tool!("publish_message", Messaging, Helper, none, guild: true, core: false,
            keywords: ["publish", "crosspost", "announcement"],
            "Publish a message in an announcement channel to every server following it.",
            { "message_id": ("string", "Message ID"), "channel": ("string", "Announcement channel") }, required: ["message_id"]),
        tool!("end_poll", Polls, Helper, none, guild: false, core: false,
            keywords: ["end poll", "close poll", "stop poll", "poll"],
            "End one of your polls now.",
            { "message_id": ("string", "The poll's message ID"), "channel": ("string", "Channel (omit for this one)") }, required: ["message_id"]),
        tool!("edit_role", Roles, Manager, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["role", "rename role", "color", "colour", "hoist", "role permission"],
            "Edit a role: name, color (#hex), shown separately (hoist), mentionable, and add/remove permissions.",
            { "role": ("string", "Role"), "name": ("string", "New name"), "color": ("string", "Hex color like #ff8800"), "hoist": ("boolean", "Show separately"), "mentionable": ("boolean", "Mentionable"), "add_permissions": ("string", "Permissions to add, comma-separated"), "remove_permissions": ("string", "Permissions to remove, comma-separated") }, required: ["role"]),
        tool!("delete_role", Roles, Manager, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["delete role", "remove role", "role"],
            "Delete a role from the server.",
            { "role": ("string", "Role") }, required: ["role"]),
        tool!("list_bans", Moderation, Moderator, Permissions::BAN_MEMBERS, guild: true, core: false,
            keywords: ["bans", "banned", "ban list"],
            "List banned users and why.",
            {}, required: []),
        tool!("unban_member", Moderation, Moderator, Permissions::BAN_MEMBERS, guild: true, core: false,
            keywords: ["unban", "pardon", "banned"],
            "Unban someone (by username or user ID).",
            { "user": ("string", "Username or ID"), "reason": ("string", "Why") }, required: ["user"]),
        tool!("edit_server", Channels, Manager, Permissions::MANAGE_GUILD, guild: true, core: false,
            keywords: ["server name", "server settings", "rename the server", "server description", "afk", "verification", "system channel"],
            "Change server settings: name, description, system/AFK/rules channel, AFK timeout, verification level (0-4).",
            { "name": ("string", "Server name"), "description": ("string", "Description"), "system_channel": ("string", "Welcome/system messages channel"), "afk_channel": ("string", "AFK voice channel"), "rules_channel": ("string", "Rules channel"), "afk_timeout_minutes": ("integer", "1, 5, 15, 30, or 60"), "verification_level": ("integer", "0 none – 4 highest") }, required: []),
        tool!("create_event", Channels, Helper, Permissions::CREATE_EVENTS, guild: true, core: false,
            keywords: ["event", "schedule", "game night", "meetup", "stream"],
            "Create a scheduled server event (in a voice channel or at a location).",
            { "name": ("string", "Event name"), "start": ("string", "When: \"in 2 hours\", \"tomorrow\", or ISO time"), "duration_minutes": ("integer", "Length (default 60)"), "channel": ("string", "Voice channel it's in"), "location": ("string", "Or a place/link"), "description": ("string", "Details") }, required: ["name", "start"]),
        tool!("list_events", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["event", "events", "scheduled"],
            "List the server's scheduled events.",
            {}, required: []),
        tool!("delete_event", Channels, Helper, Permissions::MANAGE_EVENTS, guild: true, core: false,
            keywords: ["cancel event", "delete event", "event"],
            "Cancel/delete a scheduled event (by name or ID).",
            { "event": ("string", "Event name or ID") }, required: ["event"]),
        tool!("create_emoji", Channels, Helper, Permissions::CREATE_GUILD_EXPRESSIONS, guild: true, core: false,
            keywords: ["emoji", "emote", "add emoji", "custom emoji"],
            "Add a custom emoji from an image URL or an image in the chat (under 256 KB).",
            { "name": ("string", "Emoji name"), "image": ("string", "Image URL or the name of an image in the chat") }, required: ["name", "image"]),
        tool!("delete_emoji", Channels, Helper, Permissions::MANAGE_GUILD_EXPRESSIONS, guild: true, core: false,
            keywords: ["emoji", "emote", "remove emoji", "delete emoji"],
            "Delete a custom emoji.",
            { "emoji": ("string", "Emoji name") }, required: ["emoji"]),
        tool!("setup_tickets", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["ticket", "tickets", "support", "ticket system", "help desk"],
            "Create (or reconnect to) the support-ticket system: a panel with an \"Open a ticket\" button; each ticket is a private channel for the opener, staff, and you. instruction = what you do when a ticket opens. To change it later use ticket_settings / edit_ticket_panel instead of running this again.",
            { "panel_channel": ("string", "Text channel for the panel (required the first time)"), "category_name": ("string", "Category for tickets (default Tickets)"), "staff_roles": ("string", "Roles that see all tickets, comma-separated"), "instruction": ("string", "What to do when a ticket opens"), "auto_reply": ("boolean", "Answer people in their tickets (default true)") }, required: []),
        tool!("edit_ticket_panel", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["panel", "ticket", "button", "dropdown", "options", "title", "ticket message", "ticket types", "categories"],
            "Change the ticket panel: title, message, color, button text/emoji/color, the welcome line in new tickets, or dropdown ticket types (\"Label | emoji | description | extra instructions\" separated by ;). Can also move the panel to another channel.",
            { "title": ("string", "Panel title"), "message": ("string", "Panel text"), "color": ("string", "Hex color"), "button_label": ("string", "Button text"), "button_emoji": ("string", "Button emoji"), "button_color": ("string", "primary, secondary, success, or danger"), "options": ("string", "Dropdown types: Label | emoji | description | instructions; ..."), "welcome": ("string", "First line in new tickets ({user}, {number}, {type})"), "panel_channel": ("string", "Move the panel to this channel") }, required: []),
        tool!("ticket_settings", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["ticket", "staff", "transcript", "log", "moderator", "settings"],
            "Change ticket settings: add/remove staff roles (they see every ticket), transcript log channel, DM transcripts to openers, whether you answer in tickets, max open tickets per person.",
            { "add_staff_roles": ("string", "Roles to add, comma-separated"), "remove_staff_roles": ("string", "Roles to remove"), "log_channel": ("string", "Channel for transcripts (none to stop)"), "dm_transcripts": ("boolean", "DM openers their transcript"), "auto_reply": ("boolean", "Answer in tickets"), "max_open_per_user": ("integer", "Open tickets per person") }, required: []),
        tool!("ticket_access", Channels, Helper, none, guild: true, core: false,
            keywords: ["ticket", "access", "add", "remove", "role", "see this", "let"],
            "Give or remove one person's or role's access to a single ticket (default: this one).",
            { "who": ("string", "Member or role"), "ticket": ("string", "Ticket number or channel (omit for this one)"), "remove": ("boolean", "Take access away") }, required: ["who"]),
        tool!("close_ticket", Channels, Conversational, none, guild: true, core: false,
            keywords: ["close", "close ticket", "close this", "resolve", "solved", "done with", "ticket", "delete ticket"],
            "Close a ticket (default: this one): saves a transcript (posted to the log channel), locks it with Reopen/Delete buttons — or deletes the channel.",
            { "ticket": ("string", "Ticket number or channel (omit for this one)"), "reason": ("string", "Why it's closed"), "delete": ("boolean", "Delete the channel") }, required: []),
        tool!("reopen_ticket", Channels, Conversational, none, guild: true, core: false,
            keywords: ["reopen", "open again", "ticket"],
            "Reopen a closed ticket.",
            { "ticket": ("string", "Ticket number or channel (omit for this one)") }, required: []),
        tool!("claim_ticket", Channels, Conversational, none, guild: true, core: false,
            keywords: ["claim", "handle", "take this ticket", "ticket"],
            "Claim a ticket for the requester (staff).",
            { "ticket": ("string", "Ticket number or channel (omit for this one)") }, required: []),
        tool!("rename_ticket", Channels, Helper, none, guild: true, core: false,
            keywords: ["rename", "ticket", "name"],
            "Rename a ticket channel.",
            { "name": ("string", "New name"), "ticket": ("string", "Ticket number or channel (omit for this one)") }, required: ["name"]),
        tool!("open_ticket", Channels, Conversational, none, guild: true, core: false,
            keywords: ["open a ticket", "ticket for", "new ticket", "ticket"],
            "Open a ticket for someone (staff) or the requester, optionally of a dropdown type.",
            { "user": ("string", "Who it's for (omit for the requester)"), "type": ("string", "Ticket type from the dropdown") }, required: []),
        tool!("list_tickets", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["ticket", "tickets", "open tickets", "new tickets"],
            "List open (or all) tickets: number, opener, type, claim, and a summary for closed ones.",
            { "include_closed": ("boolean", "Include closed tickets"), "user": ("string", "Only this person's") }, required: []),
        tool!("ticket_transcript", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["transcript", "ticket", "what happened", "summarize", "summary", "history"],
            "Read a ticket's transcript and summary by number (to answer questions about it or summarize it).",
            { "ticket": ("string", "Ticket number like #12 (omit for this one)") }, required: []),
        tool!("search_tickets", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["search", "find", "transcript", "tickets", "ticket", "summarize", "summaries", "which tickets", "about"],
            "Search all ticket transcripts and summaries (open and closed) for words, and/or one person's tickets. Returns ticket numbers with snippets — read full ones with ticket_transcript, then summarize.",
            { "query": ("string", "Words to look for"), "user": ("string", "Only this person's tickets") }, required: []),
        tool!("create_role_menu", Roles, Manager, Permissions::MANAGE_ROLES, guild: true, core: false,
            keywords: ["role menu", "reaction roles", "self roles", "self-assign", "pick roles", "color roles", "roles"],
            "Post a role menu where members give themselves roles: buttons (toggle) or a dropdown. roles = \"Role | emoji | description; ...\".",
            { "roles": ("string", "Role | emoji | description; ..."), "channel": ("string", "Where (omit for this one)"), "title": ("string", "Title"), "message": ("string", "Text"), "style": ("string", "buttons or dropdown") }, required: ["roles"]),
        tool!("start_giveaway", Channels, Helper, Permissions::MANAGE_MESSAGES, guild: true, core: false,
            keywords: ["giveaway", "raffle", "give away", "prize"],
            "Start a giveaway with an Enter button; winners are drawn automatically when it ends.",
            { "prize": ("string", "What's given away"), "ends": ("string", "When: \"in 2 hours\", \"in 3 days\", or ISO"), "winners": ("integer", "How many winners"), "channel": ("string", "Where (omit for this one)"), "required_role": ("string", "Only members with this role can enter") }, required: ["prize"]),
        tool!("end_giveaway", Channels, Helper, Permissions::MANAGE_MESSAGES, guild: true, core: false,
            keywords: ["giveaway", "end", "reroll", "draw", "winner"],
            "End a running giveaway now, or reroll (reroll=true) an ended one for a new winner.",
            { "giveaway": ("string", "Prize name or message ID (omit for the latest)"), "reroll": ("boolean", "Pick another winner for an ended giveaway") }, required: []),
        tool!("list_giveaways", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["giveaway", "giveaways"],
            "List giveaways: entries, end times, winners.",
            {}, required: []),
        tool!("get_rank", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["level", "rank", "xp", "my level", "rank card", "what level", "my card"],
            "Someone's level, rank, and XP, with their rank card image (default: the requester). Anyone can ask.",
            { "user": ("string", "Whose (omit for the requester)") }, required: []),
        tool!("leaderboard", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["leaderboard", "top", "most xp", "highest level", "ranking", "rankings", "who's winning"],
            "The XP leaderboard.",
            { "count": ("integer", "How many (default 10)") }, required: []),
        tool!("server_stats", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["stats", "statistics", "activity", "how active", "member count", "growth", "joins", "busiest", "most active", "messages per"],
            "Collected server statistics: messages, active people, joins/leaves, member count over time, voice minutes, busiest channels and hours.",
            { "days": ("integer", "How many days back (default 7, up to 120)") }, required: []),
        tool!("leveling_settings", Roles, Manager, none, guild: true, core: false,
            keywords: ["leveling", "levels", "xp", "level up", "level-up", "reward", "rewards", "level role", "at level", "reach level", "reaches level", "hits level"],
            "Change leveling (settings managers only): on/off, pause, XP per message, cooldown, voice XP, where level-ups are announced (same, channel, dm, off), the level-up message ({user} {level} {name}), role rewards (\"10: Regular; 20: Veteran\"), stacking, ignored channels.",
            { "enabled": ("boolean", "On/off"), "paused": ("boolean", "Pause"), "xp_min": ("integer", "Least XP per message"), "xp_max": ("integer", "Most XP per message"), "cooldown_seconds": ("integer", "Seconds between XP messages"), "voice_xp_per_minute": ("integer", "Voice XP per minute"), "announce": ("string", "same, channel, dm, or off"), "announce_channel": ("string", "Channel for level-ups"), "message": ("string", "Level-up message"), "add_rewards": ("string", "level: role; ..."), "remove_rewards": ("string", "Levels to remove, comma-separated"), "stack_rewards": ("boolean", "Keep lower reward roles"), "ignore_channel": ("string", "A channel that gives no XP") }, required: []),
        tool!("set_xp", Roles, Manager, none, guild: true, core: false,
            keywords: ["xp", "level", "set level", "give xp", "reset xp"],
            "Set, add, or remove someone's XP, or set their level (settings managers only).",
            { "user": ("string", "Who"), "xp": ("integer", "Set total XP"), "add_xp": ("integer", "Add (or subtract) XP"), "level": ("integer", "Set to the start of this level") }, required: ["user"]),
        tool!("design_rank_card", Roles, Manager, none, guild: true, core: false,
            keywords: ["rank card", "card", "design", "profile card", "level card", "theme", "colors", "colour", "brighter", "darker", "font", "layout", "background", "blur", "black and white", "bw", "aesthetic", "look", "higher", "lower", "bigger", "smaller", "xp total"],
            "Design the server's rank cards — every part (settings managers only). layout: classic/mirrored/centered/compact. style: classic/minimal/bold/glass. Colors (#hex): background, background2 (gradient) + gradient direction, accent, text, muted, name_color, rank_color, level_color, xp_color, bar_color, bar_track, ring_color. font: inter, poppins, montserrat, bebas neue, oswald, orbitron, press start 2p, pacifico, lobster, nunito, playfair display, rubik, roboto mono, mono, default, or a .ttf URL; font_weight; text_scale. Background image with image_opacity, blur, image_brightness, darken. avatar_shape, avatar_ring, bar_height, bar_shape, corner_radius, total_position (below_bar/above_bar/with_xp/top), hide/show parts (avatar, rank, level, xp, total, bar), rank_label/level_label. Change only what was asked — the rest of the current design stays (use reset only when asked to start over). For a whole theme (\"ocean\", \"cyberpunk\"): call image_search first for a fitting background picture, then call this once with that image URL plus matching colors, font and style, with look=true; fix what's off. Examples: \"put the XP total higher\" → total_position above_bar (or top); \"brighter\" → lighter background/text/accent colors (or image_brightness for an image); \"bigger text\" → text_scale. Attaches a preview.",
            { "layout": ("string", "classic, mirrored, centered, compact"), "style": ("string", "classic, minimal, bold, glass"), "background": ("string", "#hex"), "background2": ("string", "#hex gradient end (empty = none)"), "gradient": ("string", "horizontal, vertical, diagonal"), "accent": ("string", "#hex"), "text": ("string", "#hex"), "muted": ("string", "#hex secondary text"), "name_color": ("string", "#hex"), "rank_color": ("string", "#hex"), "level_color": ("string", "#hex"), "xp_color": ("string", "#hex"), "bar_color": ("string", "#hex"), "bar_track": ("string", "#hex"), "ring_color": ("string", "#hex"), "font": ("string", "Font name or .ttf URL"), "font_weight": ("string", "light, regular, bold"), "text_scale": ("number", "0.5-2"), "background_image": ("string", "Image URL (empty removes)"), "image_opacity": ("integer", "1-100"), "blur": ("integer", "0-40"), "image_brightness": ("integer", "-100 to 100"), "darken": ("integer", "0-100"), "panel_opacity": ("integer", "0-100"), "avatar_shape": ("string", "circle, rounded, square"), "avatar_ring": ("boolean", "Ring around the avatar"), "bar_height": ("integer", "4-80"), "bar_shape": ("string", "round, square"), "corner_radius": ("integer", "Rounded card corners, px"), "total_position": ("string", "below_bar, above_bar, with_xp, top"), "hide": ("string", "Parts to hide, comma-separated"), "show": ("string", "Parts to show again"), "rank_label": ("string", "Text instead of RANK"), "level_label": ("string", "Text instead of LEVEL"), "reset": ("boolean", "Start over from the default design (only when asked)"), "look": ("boolean", "Look at the result yourself") }, required: []),
        tool!("stats_settings", Channels, Manager, none, guild: true, core: false,
            keywords: ["stats", "statistics", "tracking", "collect", "collecting", "data", "pause", "stop tracking"],
            "Turn statistics collection on/off, pause it, choose what's tracked (members, messages, voice, joins), or delete the collected data (settings managers only).",
            { "enabled": ("boolean", "On/off"), "paused": ("boolean", "Pause"), "track_members": ("boolean", "Member counts"), "track_messages": ("boolean", "Messages and activity"), "track_voice": ("boolean", "Voice minutes"), "track_joins": ("boolean", "Joins and leaves"), "clear_data": ("boolean", "Delete everything collected") }, required: []),
        tool!("settings_access", Channels, Manager, none, guild: true, core: false,
            keywords: ["who can change", "settings access", "permissions for settings", "allowed to change", "only allow", "settings"],
            "See who may change the agents' server settings; the owner can restrict it to roles/people (then other admins can't) or reset it to admins.",
            { "roles": ("string", "Roles allowed, comma-separated"), "users": ("string", "People allowed, comma-separated"), "reset": ("boolean", "Back to admins") }, required: []),
        tool!("show_settings", ServerInfo, Conversational, none, guild: true, core: false,
            keywords: ["settings", "setup", "config", "configuration", "show settings", "menu", "panel"],
            "Post the interactive settings panel (like /setup) here: overview, leveling, card, stats, tickets, access, or agent.",
            { "section": ("string", "Which section (default overview)") }, required: []),
        tool!("start_translating", Messaging, Helper, none, guild: true, core: false,
            keywords: ["translate", "translation", "translating", "language", "speak", "english", "spanish", "interpret", "understand"],
            "Translate a conversation live from now on: every new message from these people (or everyone) gets translated — into one language, or between the people's own languages — until asked to stop or it goes quiet. Use it when asked to translate someone, or when people talking can't understand each other.",
            { "users": ("string", "Whose messages, comma-separated (omit for everyone)"), "language": ("string", "Translate into this language (omit to translate between their languages)"), "languages": ("string", "Known languages, e.g. \"uvx: English, Pablo: Spanish\""), "stop_after_quiet_minutes": ("integer", "Stop after this many quiet minutes (default 15)") }, required: []),
        tool!("stop_translating", Messaging, Conversational, none, guild: true, core: false,
            keywords: ["stop translating", "stop translation", "translate", "translating", "stop"],
            "Stop translating in this channel.",
            {}, required: []),
        tool!("load_tools", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["tool", "tools", "can you", "able to"],
            "Your tool list only has tools that looked relevant. If you need one that isn't there (e.g. delete a channel, make a role, ban someone), call this with what you want to do — it adds the matching tools. Never tell someone you don't have a tool, or can't do something, without checking here first.",
            { "what": ("string", "What you want to do, e.g. \"delete channels\"") }, required: ["what"]),
        tool!("make_plan", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["plan", "steps", "and then", "also", "everything", "all of", "set up", "build", "create", "make"],
            "For a big or multi-part job: write the steps you'll take (one per line). You'll then work through them one at a time, calling step_done after each — and you'll be kept going until every step is done.",
            { "steps": ("string", "The steps, one per line, in order") }, required: ["steps"]),
        tool!("step_done", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["step"],
            "Mark the current plan step done, with a short note of what you did or found (it's all that's kept of that step, so include anything later steps need: names, links, IDs). If a step can't be done, say why.",
            { "result": ("string", "What you did / found") }, required: ["result"]),
        tool!("read_section", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["section", "doc", "document", "outline", "read more"],
            "Open one section of a long document (from a long tool result's outline), like \"3\" or \"3.2\". Big sections open as a finer outline.",
            { "document": ("string", "Document ID, e.g. doc3"), "section": ("string", "Section number from the outline") }, required: ["document", "section"]),
        tool!("find_in_document", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["section", "doc", "document", "find", "where", "function"],
            "Find where a word or name appears in a long document (from a long tool result) — returns the lines and the section numbers to open with read_section.",
            { "document": ("string", "Document ID, e.g. doc3"), "text": ("string", "What to find, e.g. a function name") }, required: ["document", "text"]),
        tool!("set_my_avatar", Utilities, Manager, none, guild: true, core: false,
            keywords: ["avatar", "profile picture", "pfp", "profile icon", "your icon", "your picture"],
            "Change your own (the bot's) profile picture to an image — a URL (e.g. from image_search) or an image made/edited in this conversation. Admins only; it shows in every server.",
            { "image": ("string", "Image URL or the name of an image here") }, required: ["image"]),
        tool!("find_users", ServerInfo, Conversational, none, guild: true, core: true,
            keywords: ["who is", "find user", "find member", "which user", "user id", "member list", "search users", "who's", "pick user", "select user", "list users", "members"],
            "Find members by name, nickname, or ID and list them NUMBERED (display name, @username, ID, bot flag). Then pick with #N: every tool that takes a user also accepts `user: #2` (or a list like `user: #1,3-5` where several are allowed). NEVER retype a username or ID — selecting by number is exact; typing them is where mistakes happen. Call this before any tool that needs a person you are not 100% sure about.",
            { "like": ("string", "Part of a name, nickname, or an ID (empty = everyone, first 25)") }, required: []),
        tool!("pick_users", ServerInfo, Conversational, none, guild: true, core: true,
            keywords: ["pick user", "select user", "choose user", "picked", "select users", "which user"],
            "Inspect and select users from the last find_users numbered list (e.g. '2', '#2', '1,3-5', or 'all'). Returns the full details of what was selected (name, nick, ID, bot) so you understand exactly who you selected before acting on them.",
            { "pick": ("string", "Selection from find_users: singular ('#2' or '2'), comma-separated ('1,3'), range ('2-5'), or 'all'") }, required: ["pick"]),
        tool!("find_people", ServerInfo, Conversational, none, guild: false, core: false,
            keywords: ["who is", "find", "username", "member", "user", "someone", "person", "people", "named", "called", "dm", "message", "from", "server"],
            "Find people by name across every server you're in (or one), from anywhere — even DMs. Shows look-alike names so you can ask which one.",
            { "name": ("string", "Name, nickname, @username or ID"), "server": ("string", "Only this server (omit for all)") }, required: ["name"]),
        tool!("send_to_server", Messaging, Helper, none, guild: false, core: false,
            keywords: ["send", "post", "message", "tell", "announce", "server", "in the", "over in"],
            "Post a message in a channel of any server you're in (e.g. from a DM, or from another server). The requester's permissions in that server apply.",
            { "server": ("string", "Server name"), "channel": ("string", "Channel name"), "content": ("string", "The message") }, required: ["server", "channel", "content"]),
        tool!("search_audit_logs", ServerInfo, Conversational, none, guild: false, core: false,
            keywords: ["audit", "audit log", "audit logs", "logs", "mod log", "mod actions", "who banned", "who kicked", "who deleted", "who changed", "who made", "who created", "who gave", "who removed", "who did", "who timed", "who muted", "who renamed", "who pinned", "who added", "log", "history", "recent actions", "what happened"],
            "The audit log: who did what, to whom, when and why — in this server, a named one, or all of them. Filter by who did it (by), who it was done to (target), the kind of action (ban, unban, kick, timeout, role, nickname, message delete, channel, invite, emoji, webhook, thread, pin, bot…), or any text. Leave filters out for the latest entries.",
            { "server": ("string", "Server name (omit for this one)"), "all_servers": ("boolean", "Search every server"), "by": ("string", "Who did it"), "target": ("string", "Who or what it was done to"), "action": ("string", "Kind of action, e.g. ban, kick, timeout, role, message delete, channel create"), "text": ("string", "Other text to look for (e.g. a reason)"), "limit": ("integer", "Max entries (default 25)") }, required: []),
        tool!("list_servers", ServerInfo, Conversational, none, guild: false, core: false,
            keywords: ["servers", "which server", "what servers", "guilds", "every server", "all the servers", "server you are in", "servers you're in"],
            "The servers you're in.",
            {}, required: []),
        tool!("privacy_settings", Channels, Manager, none, guild: true, core: false,
            keywords: ["privacy", "private", "global search", "other servers", "outside", "opt out", "cross-server", "cross server"],
            "What requests from other servers and DMs may do in this server (settings managers only): member search, DMs to members, posting in channels, audit log search. All allowed by default.",
            { "member_search": ("boolean", "Find members by name"), "dms": ("boolean", "DM members"), "outside_messages": ("boolean", "Post in channels"), "audit_search": ("boolean", "Search the audit log") }, required: []),
        tool!("discord_api", Channels, Manager, none, guild: true, core: false,
            keywords: ["api", "webhook", "automod", "soundboard", "sticker", "onboarding", "welcome screen", "stage", "integration", "anything"],
            "Admins only: call any Discord REST API endpoint for this server when no other tool does the job — never tell an admin something can't be done before trying this (webhooks, automod rules, stickers, onboarding, welcome screen, stage instances, soundboard…). Paths like /guilds/{guild_id}/auto-moderation/rules; body is JSON.",
            { "method": ("string", "GET, POST, PATCH, PUT, or DELETE"), "path": ("string", "API path, e.g. /channels/123/webhooks"), "body": ("string", "JSON body"), "reason": ("string", "Audit-log reason") }, required: ["method", "path"]),
        tool!("create_channel", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["create channel", "new channel", "make a channel", "category", "forum", "create", "channel"],
            "Create a channel.",
            { "name": ("string", "Channel name"), "kind": ("string", "text, voice, category, forum, or announcement (default text)"), "category": ("string", "Parent category"), "topic": ("string", "Topic"), "private": ("boolean", "Hide from @everyone") },
            required: ["name"]),
        tool!("edit_channel", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["rename", "topic", "edit channel", "description"],
            "Rename a channel or change its topic.",
            { "channel": ("string", "Channel name, e.g. general (omit for this one)"), "name": ("string", "New name"), "topic": ("string", "New topic") }, required: []),
        tool!("delete_channel", Channels, Manager, Permissions::MANAGE_CHANNELS, guild: true, core: false,
            keywords: ["delete channel", "remove channel", "delete the channel", "delete channels", "delete the copies", "delete the clones", "remove the channels", "get rid of the channel"],
            "Delete a channel. Irreversible — make sure it's the channel they mean (when they asked to delete copies you made, just do it).",
            { "channel": ("string", "Channel"), "reason": ("string", "Why") }, required: ["channel"]),
        tool!("set_nickname", Channels, Manager, Permissions::MANAGE_NICKNAMES, guild: true, core: false,
            keywords: ["nickname", "nick", "rename"],
            "Change a member's nickname (or your own with user='me-bot'). Empty nick clears it.",
            { "user": ("string", "Who ('me-bot' for yourself)"), "nick": ("string", "New nickname") }, required: ["user"]),
        // --- web -------------------------------------------------------------
        tool!("web_search", WebSearch, Conversational, none, guild: false, core: true,
            keywords: ["search", "google", "look up", "latest", "news", "current", "today", "who is", "what is", "price", "weather", "score", "research", "look into", "find out"],
            "Search the web. Use for anything recent, factual, or that you're unsure about — and whenever asked to research something (then report the findings, not a plan).",
            { "query": ("string", "Search query") }, required: ["query"]),
        tool!("get_avatar", Images, Conversational, none, guild: false, core: false,
            keywords: ["pfp", "avatar", "profile picture", "profile pic", "icon", "picture of"],
            "Get someone's profile picture: it's attached to your reply and loaded as an image you can edit.",
            { "user": ("string", "Who (name, mention, ID, or 'me')"), "server_avatar": ("boolean", "Prefer their server-specific avatar (default true)") },
            required: ["user"]),
        tool!("avatar_grid", Images, Conversational, none, guild: true, core: false,
            keywords: ["pfp", "pfps", "avatars", "avatar", "profile pictures", "profile pics", "profile picture", "everyone's", "all of them", "their pictures", "grid"],
            "Several people's profile pictures in ONE image: a numbered grid with each person's display name and @username under their picture. Use whenever more than one person's picture is wanted (then get_avatar for any one in full resolution).",
            { "users": ("string", "Names, mentions or IDs, comma-separated"), "role": ("string", "Everyone with this role"), "voice": ("boolean", "Everyone in your voice channel") },
            required: []),
        tool!("image_search", Images, Conversational, none, guild: false, core: false,
            keywords: ["find an image", "find a picture", "image of", "picture of", "photo of", "show me", "pic of", "find me a", "gif of"],
            "Search the web for real images (photos, art, memes) and get direct image links. Post a link on its own line and Discord shows it as an embed. Set attach=true to upload the first result instead. Use this whenever someone asks you to find or show an image.",
            { "query": ("string", "What to find"), "count": ("integer", "How many results (1-10, default 5)"), "attach": ("boolean", "Upload the first image with your reply") },
            required: ["query"]),
        tool!("draw_image", Images, Conversational, none, guild: false, core: false,
            keywords: ["draw", "diagram", "sketch", "box", "boxes", "shape", "rectangle", "circle", "arrow", "mockup", "layout", "illustrate", "visualize", "flowchart"],
            "Draw an image from shapes on a canvas (or on top of an image via base_image). Coordinates are pixels from the top-left. Shapes, one per item: rect X Y W H COLOR [outline] [width=N] | circle CX CY R COLOR [outline] | ellipse CX CY RX RY COLOR | line X1 Y1 X2 Y2 COLOR [width=N] | arrow X1 Y1 X2 Y2 COLOR [width=N] | polygon COLOR x1 y1 x2 y2 x3 y3… | grid STEP COLOR | text X Y SIZE COLOR words (X can be center/right) | fill COLOR. Colors: names or #rrggbb. The result is attached to your reply.",
            { "shapes": ("array", "Shape commands in drawing order"), "width": ("integer", "Canvas width (default 800)"), "height": ("integer", "Canvas height (default 500)"), "background": ("string", "Canvas color (default white; or transparent)"), "base_image": ("string", "Draw on this image instead (a name, URL, avatar:NAME…)") },
            required: ["shapes"]),
        tool!("make_chart", Images, Conversational, none, guild: false, core: false,
            keywords: ["chart", "graph", "plot", "bar chart", "pie chart", "visualize", "stats"],
            "Make a bar, line, or pie chart image from numbers (e.g. server stats). The result is attached to your reply.",
            { "type": ("string", "bar, line, or pie"), "labels": ("array", "Label per value"), "values": ("array", "The numbers"), "title": ("string", "Chart title") },
            required: ["values"]),
        tool!("edit_image", Images, Conversational, none, guild: false, core: false,
            keywords: ["edit", "meme", "caption", "crop", "resize", "rotate", "flip", "grayscale", "black and white", "blur", "filter", "circle", "deepfry", "pixelate", "invert", "border", "sepia"],
            "Edit an image locally. image = 'attachment' (the image in the message), an imageN from earlier, a URL, 'avatar:NAME', or 'server icon'. operations, applied in order: resize WxH | resize 50% | crop square | crop WxH | crop X Y W H | rotate 90/180/270 | flip horizontal/vertical | grayscale | invert | blur N | sharpen | brightness N | contrast N | hue DEG | sepia | pixelate N | deepfry | circle | border N COLOR | caption TOP TEXT | BOTTOM TEXT | top TEXT | bottom TEXT. The result is attached to your reply.",
            { "image": ("string", "What to edit"), "operations": ("array", "Operations in order, e.g. [\"crop square\", \"caption when the code works | first try\"]") },
            required: ["image", "operations"]),
        tool!("look_at_image", Images, Conversational, none, guild: false, core: false,
            keywords: ["what is this", "what's in", "describe", "look at", "see", "read the", "screenshot", "image", "picture", "photo"],
            "Look at an image and describe it or answer a question about it (needs a vision model). image works like edit_image.",
            { "image": ("string", "Which image"), "question": ("string", "What you want to know") },
            required: ["image"]),
        tool!("find_gifs", Images, Conversational, none, guild: false, core: true,
            keywords: ["gif", "gifs", "reaction gif", "find gif", "search gif", "tenor", "giphy", "klipy", "gifdb", "animation"],
            "Search for animated GIFs across free services (GIFDB, Klipy, Giphy, Tenor). Returns a numbered list of candidates with slugs/names (e.g. [1] \"happy-cat-jumping\"). Prioritizes fresh, unused GIFs and tags ones already sent in this channel. Pass inspect=true to inspect candidates with vision. Pick a GIF to send with send_gif(gif: \"#1\") or send_gif(gif: \"different\").",
            { "query": ("string", "What to search for, or 'recent' to view previously sent GIFs"), "stickers": ("boolean", "Search for stickers instead of regular GIFs"), "limit": ("integer", "Number of candidates (1-10, default 5)"), "inspect": ("boolean", "Run vision inspection on top results") },
            required: ["query"]),
        tool!("find_stickers", Images, Conversational, none, guild: false, core: true,
            keywords: ["sticker", "stickers", "reaction sticker", "find sticker", "search sticker", "send a sticker"],
            "Search for transparent reaction stickers across free services (GIFDB, Klipy, Giphy, Tenor). Returns a numbered list of candidates with slugs/names (e.g. [1] \"party-parrot-sticker\"). Fresh stickers appear first; already-used stickers are marked [ALREADY USED in this channel]. Pick with send_sticker(sticker: \"#1\") or send_sticker(sticker: \"different\").",
            { "query": ("string", "What sticker to search for, or 'recent' to see what was already sent"), "limit": ("integer", "Number of candidates (1-10, default 5)"), "inspect": ("boolean", "Run vision inspection on top results") },
            required: ["query"]),
        tool!("inspect_gif", Images, Conversational, none, guild: false, core: false,
            keywords: ["inspect gif", "preview gif", "look at gif", "check gif", "inspect sticker"],
            "Inspect an animated GIF or sticker using computer vision. Pass a candidate reference like \"#1\", \"#2\", or the slug name from find_gifs / find_stickers.",
            { "gif": ("string", "The GIF or sticker candidate reference, e.g. \"#1\" or \"happy-cat\"") },
            required: ["gif"]),
        tool!("send_gif", Images, Conversational, none, guild: false, core: true,
            keywords: ["send gif", "post gif", "post a gif", "send a gif"],
            "Send or attach an animated GIF to Discord. Pass gif as a candidate reference from find_gifs (e.g. \"#1\", \"happy-cat\"), \"different\" / \"next\" to automatically select an unused GIF without repeating, or a search query. Set as_attachment=true (default) to upload the file directly with your reply.",
            { "gif": ("string", "The candidate reference (e.g. \"#1\"), slug name, \"different\", or search query"), "channel": ("string", "Target channel name/ID (defaults to current channel)"), "as_attachment": ("boolean", "Attach the file directly (true, default) or provide the embed link (false)") },
            required: ["gif"]),
        tool!("send_sticker", Images, Conversational, none, guild: false, core: true,
            keywords: ["send sticker", "post sticker", "send a sticker", "post a sticker", "different sticker", "another sticker"],
            "Send or attach a transparent reaction sticker to Discord. Pass sticker as a reference from find_stickers (e.g. \"#1\", \"party-parrot\"), \"different\" / \"next\" to automatically send an unused sticker without repeating, or a search query. The transparent sticker is uploaded directly with your reply.",
            { "sticker": ("string", "The candidate reference (e.g. \"#1\"), slug name, \"different\", or search query"), "channel": ("string", "Target channel name/ID (defaults to current channel)"), "as_attachment": ("boolean", "Attach the sticker directly (true, default) or provide the embed link (false)") },
            required: ["sticker"]),
        tool!("list_recent_gifs", Images, Conversational, none, guild: false, core: false,
            keywords: ["recent gifs", "used gifs", "recent stickers", "used stickers", "what gifs", "what stickers", "what did you send"],
            "List the GIFs and stickers that have been recently sent in this channel so you know what has already been used.",
            {},
            required: []),
        tool!("write_file", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["file", "write", "make a file", "create a file", "save", "document", "txt", "csv", "json", "generate", "repeated", "1mb", "mb file"],
            "Write a file into this channel's file sandbox. For a SIZE-targeted or repetitive file (\"a 1MB file of the word legion\") this is the right tool: pass `repeat`, `until_bytes` (or `size`) for the exact byte count, and `join` for the separator — join: \" \") puts a space between repeats, the default is a newline. The app hits the exact size every time and you write none of the bytes yourself. For normal content pass `content` (a size target only ever TRIMS it — content is never padded up, so keep writing until the target). YOUR upload limit is 25 MB (26 214 400 bytes) — never say 8 MB or 10 MB, those are human limits. If the person named a size in their message you may omit until_bytes; the app knows the ask. For copy-pasting a document to a size, write it once then duplicate_file. Intermediate working files (drafts, partial results you will build on): pass temp: true and they live in the _tmp scratch area, which cleans itself up.",
            { "name": ("string", "File name, e.g. legion.txt"), "content": ("string", "The file's text content (omit when using repeat)"), "repeat": ("string", "A short pattern to repeat (e.g. a word or line)"), "times": ("number", "Repeat count (with repeat)"), "until_bytes": ("number", "EXACT target size in bytes — the file will be exactly this many bytes"), "size": ("number", "Alias for until_bytes"), "join": ("string", "Separator between repeats: a space \" \") for readable columns, \"\n\" or omit for lines") }, required: ["name"]),
        tool!("write_section", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["chapter", "story", "essay", "write", "section", "novel", "long", "continue writing", "part", "scene", "article", "guide", "document"],
            "Write ONE section of a long document (a chapter, scene, essay part — 500-1500 words of actual prose, never a summary). The app keeps the full text and gives you back your private running notes plus the last lines, so you can write any length without the text crowding your memory. If a file with this name was already started (write_file), it picks up where that file ends and appends. Repeat per section; pass finished: true on the last one and it is saved as a file you attach. Never write the whole piece in one reply and never summarize instead of writing — the section IS the content.",
            { "text": ("string", "The section's full text — real prose, not a summary"), "finished": ("boolean", "true on the LAST section (saves the file; NOT allowed before a size target is reached)"), "until_bytes": ("number", "Target size in bytes for the WHOLE document — pass it on every section"), "size": ("number", "Alias for until_bytes"), "name": ("string", "File name for the finished document (default document.txt)") }, required: ["text"]),
        tool!("run_script", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["script", "python", "code", "compute", "calculate", "generate", "loop", "program", "execute", "sandbox"],
            "Run a short Python script in this channel's sandbox (isolated mode, 30s limit). Use it whenever the job is computational or repetitive: generate files, crunch numbers, transform data — write 5 lines of code instead of producing content by hand. The script sees and writes files in the sandbox; anything it writes can be attached with attach_file. Output (stdout+stderr) comes back to you.",
            { "code": ("string", "Python source to run") }, required: ["code"]),
        tool!("attach_file", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["attach", "upload", "send file", "here's the file", "file below"],
            "Attach a file from this channel's sandbox to your reply (it appears below the message). Your upload limit is 25 MB per file (26 214 400 bytes) — bots get 25 MB, never claim 8 MB or 10 MB.",
            { "name": ("string", "File name from write_file/run_script/list_files") }, required: ["name"]),
        tool!("duplicate_file", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["copy", "paste", "duplicate", "repeat", "copy-paste", "copies", "fill", "expand", "over and over", "again and again", "multiple times", "x times"],
            "Copy-paste a sandbox file's content until it reaches a size (or an exact copy count) — instant and exact, done in code, and the parts are attached to your reply automatically. Use it whenever someone asks to repeat/duplicate/copy-paste a file or its text to a size: never write the copies out by hand, never script it. If the person named a size in their message you can even omit until_bytes — the app knows the ask.",
            { "name": ("string", "The file to duplicate"), "until_bytes": ("number", "Target size in bytes for the WHOLE resulting file"), "size": ("number", "Alias for until_bytes"), "times": ("number", "Exact number of copies instead of a size"), "to": ("string", "Write the result to this file name instead (default: same file)"), "join": ("string", "Separator between copies (default: a blank line)") },
            required: ["name"]),
        tool!("set_logging", Utilities, Conversational, none, guild: true, core: false,
            keywords: ["logging", "logs", "activity log", "stop logging", "turn off logging", "log activity", "audit"],
            "Turn the agent activity log (the servers' activity channels and the audit trail) on or off. Admins only.",
            { "on": ("boolean", "true to enable, false to disable") }, required: ["on"]),
        tool!("recall_scratch", Utilities, Conversational, none, guild: false, core: true,
            keywords: ["what did i", "earlier result", "previous result", "scratch", "last result", "remember", "what was the", "did i write", "what happened", "temporary storage", "reference"],
            "Look back through the scratchpad: every tool result this conversation produced is recorded verbatim. Pull it back instead of redoing work or copying from memory.",
            { "query": ("string", "Words to filter by (omit for the most recent results)") },
            required: []),
        tool!("convert_file", Utilities, Conversational, none, guild: false, core: false,
            keywords: ["convert", "png", "jpeg", "jpg", "webp", "format", "re-encode"],
            "Convert a sandbox file between formats: images (png, jpeg, webp) or re-encode text as UTF-8 (txt).",
            { "name": ("string", "File name in the sandbox"), "to": ("string", "Target format: png, jpeg, webp, or txt") }, required: ["name", "to"]),
        tool!("list_files", Utilities, Conversational, none, guild: false, core: false,
            keywords: ["files", "sandbox", "what files", "my files"],
            "List the files in this channel's file sandbox with sizes.",
            {}, required: []),
        tool!("attach_image", Images, Conversational, none, guild: false, core: false,
            keywords: ["show", "post the image", "send the picture", "attach"],
            "Attach an image (a URL, imageN, avatar:NAME, server icon) to your reply.",
            { "image": ("string", "Which image") },
            required: ["image"]),
        tool!("fetch_url", WebFetch, Conversational, none, guild: false, core: false,
            keywords: ["http", "link", "url", "website", "page", "article", "read this"],
            "Read a web page's text (e.g. a link someone shared).", { "url": ("string", "http(s) URL") }, required: ["url"]),
    ];
    tools.extend(extra::definitions());
    tools
}

/// Everything a tool needs to act.
#[derive(Clone)]
pub struct ToolCtx {
    pub core: Arc<AgentCore>,
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub message_id: Option<u64>,
    pub requester: Principal,
    /// The agent decided this itself (no person asked).
    pub autonomous: bool,
    /// The agent's final answer will be posted in `channel_id` anyway.
    pub replies_here: bool,
    /// Images made or loaded for this reply (uploaded with it).
    pub media: Arc<std::sync::Mutex<images::Media>>,
    /// The person's request asked for the file to be attached/sent with
    /// the reply ("write X and attach it") — write_file then attaches as
    /// soon as it writes; the model never has to chain a second call.
    pub wants_file_attached: bool,
    /// The request was a copy-paste/fill-to-size ask — write_file finishes
    /// the whole job itself (writes, fills by copying, attaches).
    pub copy_fill_asked: bool,
    /// Images around the conversation (on the message being answered,
    /// the one it replies to, recent ones), by name.
    pub inputs: Vec<crate::runtime::vision::ImageSource>,
}

impl ToolCtx {
    pub fn require_guild(&self) -> Result<u64, String> {
        self.guild_id
            .ok_or_else(|| "that only works inside a server, not in DMs".to_owned())
    }

    /// The bot's own permissions in a channel (what bypassed users get).
    fn bot_permissions_in(&self, channel_id: u64) -> Permissions {
        let guild = self.guild_id.or_else(|| {
            self.core.world.guild_ids().into_iter().find(|g| {
                self.core
                    .world
                    .with_guild(*g, |k| k.channels.contains_key(&channel_id))
                    .unwrap_or(false)
            })
        });
        match guild {
            Some(g) => {
                crate::runtime::authority::bot_channel_permissions(&self.core, g, channel_id)
            }
            None => Permissions::empty(),
        }
    }

    /// Is this requester on the operator's bypass list — acting with the
    /// bot's own permissions instead of their own?
    pub fn requester_bypassed(&self) -> bool {
        self.requester.kind != PrincipalKind::Operator
            && self
                .core
                .hive
                .settings()
                .bypass_user_ids
                .contains(&self.requester.user_id)
    }

    pub fn requester_permissions_in(&self, channel_id: u64) -> Permissions {
        if self.requester.kind == PrincipalKind::Operator {
            return Permissions::all();
        }
        // Bypass list: the trusted operator acts with the bot's own
        // permissions in that channel.
        if self.requester_bypassed() {
            return self.bot_permissions_in(channel_id);
        }
        match self.guild_id {
            // A channel outside this server grants nothing: permissions
            // here say nothing about another server.
            Some(guild_id) => self
                .core
                .world
                .with_guild(guild_id, |g| {
                    if g.channels.contains_key(&channel_id) {
                        self.requester.channel_permissions(Some(g), channel_id)
                    } else {
                        Permissions::empty()
                    }
                })
                .unwrap_or_else(Permissions::empty),
            // In a DM, a person only has rights in that DM — never in
            // server channels the bot can see.
            None if channel_id == self.channel_id => {
                Permissions::VIEW_CHANNEL
                    | Permissions::SEND_MESSAGES
                    | Permissions::READ_MESSAGE_HISTORY
                    | Permissions::ADD_REACTIONS
            }
            None => Permissions::empty(),
        }
    }

    /// Check a person's permissions in a specific channel (tools that act
    /// on another channel than the current one call this).
    pub fn require_in(&self, channel_id: u64, required: Permissions) -> Result<(), String> {
        if self.acting_as_agent() || required.is_empty() {
            return Ok(());
        }
        let have = self.requester_permissions_in(channel_id);
        if have.contains(required) {
            Ok(())
        } else {
            Err(format!(
                "denied: {} lacks {} in that channel",
                self.requester.name,
                missing(required, have)
            ))
        }
    }

    pub fn acting_as_agent(&self) -> bool {
        self.autonomous || self.requester.kind == PrincipalKind::Agent
    }

    pub fn reason(&self, given: Option<String>) -> String {
        let base = given.unwrap_or_else(|| "no reason given".to_owned());
        let by = if self.autonomous {
            format!("{} (AI agent, on its own judgement)", self.core.name())
        } else {
            format!("{} via {}", self.requester.name, self.core.name())
        };
        format!("{base} — {by}")
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub text: String,
    pub ok: bool,
    /// A fixed message the app posts to Discord itself (permission
    /// denials). The model is instructed not to repeat it.
    pub notice: Option<String>,
}

const RESULT_MAX: usize = 3500;

/// Tests only: pretend Discord made/deleted channels (LAB_FAKE_DISCORD).
#[cfg(test)]
pub mod lab_fake {
    use std::sync::Mutex;

    pub static LOG: Mutex<Vec<(std::time::Instant, String)>> = Mutex::new(Vec::new());
    pub static CHANNELS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub fn answer(call: &super::ToolCall) -> Option<String> {
        std::env::var("LAB_FAKE_DISCORD").ok()?;
        let channel = super::arg(&call.arguments, "channel")
            .or_else(|| super::arg(&call.arguments, "name"))
            .unwrap_or_else(|| "general".to_owned())
            .trim_start_matches('#')
            .to_owned();
        let text = match call.name.as_str() {
            "clone_channel" => {
                let mut channels = CHANNELS.lock().unwrap();
                let made = match super::arg(&call.arguments, "name") {
                    Some(name) => name
                        .trim_start_matches('#')
                        .to_lowercase()
                        .replace(' ', "-"),
                    None => format!("general-copy-{}", channels.len() + 1),
                };
                let base = "general";
                channels.push(made.clone());
                format!("cloned #{base} as #{made} (same permissions and settings)")
            }
            "delete_channel" => {
                let mut channels = CHANNELS.lock().unwrap();
                match channels.iter().position(|c| *c == channel) {
                    Some(i) => {
                        channels.remove(i);
                        format!("deleted #{channel}")
                    }
                    None => format!(
                        "error: no channel called {channel} (copies left: {})",
                        channels.join(", ")
                    ),
                }
            }
            _ => return None,
        };
        LOG.lock().unwrap().push((
            std::time::Instant::now(),
            format!("{} {channel}", call.name),
        ));
        Some(text)
    }
}

pub async fn execute(ctx: &ToolCtx, call: &ToolCall) -> ToolOutcome {
    #[cfg(test)]
    if let Some(text) = lab_fake::answer(call) {
        return ToolOutcome {
            ok: !text.starts_with("error"),
            text,
            notice: None,
        };
    }
    let profile = ctx.core.profile();
    if call.arguments.get("_malformed").is_some() {
        return ToolOutcome {
            text: format!(
                "error: the arguments for {} were not valid JSON — call it again with a JSON object",
                call.name
            ),
            ok: false,
            notice: None,
        };
    }
    if call.name.starts_with("mcp_") {
        return run_mcp(ctx, call, &profile.tools).await;
    }
    let Some(def) = catalog().iter().find(|d| d.name == call.name) else {
        return ToolOutcome {
            text: format!("error: there is no tool named {}", call.name),
            ok: false,
            notice: None,
        };
    };
    if !def.group.enabled(&profile.tools) {
        return denied(format!(
            "the {} tool is turned off for this agent",
            def.name
        ));
    }
    if def.guild_only && ctx.guild_id.is_none() {
        return denied("that only works inside a server, not in DMs".to_owned());
    }
    if ctx.acting_as_agent() && ctx.requester.kind != PrincipalKind::Operator {
        if profile.behavior.autonomy < def.autonomy {
            return denied(format!(
                "{} needs autonomy level \"{}\"; nobody with the right permissions asked for it",
                def.name,
                def.autonomy.label()
            ));
        }
    } else if !def.requires.is_empty() {
        // Self-service exceptions are handled inside those tools.
        let self_service = matches!(
            def.name,
            "move_member" | "disconnect_member" | "delete_message" | "pin_message" | "set_nickname"
        );
        let have = ctx.requester_permissions_in(ctx.channel_id);
        if !have.contains(def.requires) && !self_service {
            // AAv2: the app tells the user directly; the model is told the
            // user already knows, so it must not narrate the denial.
            let needed = missing(def.requires, have);
            let outcome = denied(format!(
                "{} lacks the Discord permission {} needed for {} — the system already told them; do not repeat this, just continue or suggest what they can do",
                ctx.requester.name, needed, def.name
            ));
            return ToolOutcome {
                notice: Some(format!(
                    "You don't have the required permissions for that: {needed}."
                )),
                ..outcome
            };
        }
    }
    // AAv2: big or irreversible actions wait for a button click first.
    // The model cannot talk its way past this gate.
    {
        let mode = crate::runtime::confirm::mode_of(&ctx.core, ctx.guild_id);
        if let Some(label) = crate::runtime::confirm::gate(
            &ctx.core,
            ctx.guild_id,
            def.name,
            &call.arguments,
            mode == crate::memory::OperationMode::Autonomous,
        ) {
            let text = ctx
                .core
                .hive
                .confirmations
                .request(
                    &ctx.core,
                    crate::runtime::confirm::Pending {
                        tool: def.name,
                        args: call.arguments.clone(),
                        requester: ctx.requester.clone(),
                        guild_id: ctx.guild_id,
                        channel_id: ctx.channel_id,
                    },
                    label,
                )
                .await;
            return ToolOutcome {
                text,
                ok: true,
                notice: None,
            };
        }
    }

    let run = dispatch(ctx, def.name, &call.arguments);
    let result = match tokio::time::timeout(std::time::Duration::from_secs(90), run).await {
        Ok(result) => result,
        Err(_) => Err("the tool timed out".to_owned()),
    };
    // The audit trail: what ran, with what, and what came back. Args and
    // results are truncated — file contents never bloat the log. It can
    // be switched off by the operator or by the bot when told.
    if ctx.core.hive.settings().log_agent_activity {
        let args = crate::llm::truncate(&call.arguments.to_string(), 400);
        match &result {
            Ok(text) => tracing::info!(
                agent = %ctx.core.id,
                tool = def.name,
                args = %args,
                result = %crate::llm::truncate(text, 300),
                "tool ok"
            ),
            Err(error) => tracing::warn!(
                agent = %ctx.core.id,
                tool = def.name,
                args = %args,
                error = %error,
                "tool failed"
            ),
        }
    }
    // Everything flows through the scratchpad: the model can pull any
    // earlier result back verbatim instead of redoing the work.
    if !matches!(def.name, "recall_scratch" | "list_files" | "read_messages") {
        let knobs = ctx.core.hive.settings().tunables;
        match &result {
            Ok(text) => scratch::record(
                ctx.channel_id,
                def.name,
                text,
                knobs.scratch_entries,
                knobs.scratch_entry_chars,
            ),
            Err(error) => scratch::record(
                ctx.channel_id,
                def.name,
                &format!("(failed) {error}"),
                knobs.scratch_entries,
                knobs.scratch_entry_chars,
            ),
        }
    }
    // From a DM: log it in the servers where the requester has the rank
    // that let them ask.
    if let (Ok(text), None) = (&result, ctx.guild_id) {
        if !crate::runtime::activity::is_read_only(def.name)
            && !matches!(def.name, "send_dm" | "send_to_server")
        {
            let theirs: Vec<u64> = ctx
                .core
                .world
                .guild_ids()
                .into_iter()
                .filter(|g| reach::principal_in(ctx, *g).level >= AccessLevel::Moderator)
                .collect();
            for guild_id in theirs {
                crate::runtime::activity::log_outside(
                    &ctx.core,
                    guild_id,
                    &format!(
                        "{} — asked by {} in a DM: {}",
                        step_label(def.name, &call.arguments),
                        ctx.requester.name,
                        crate::llm::truncate(text, 160)
                    ),
                );
            }
        }
    }
    if let (Ok(text), Some(guild_id)) = (&result, ctx.guild_id) {
        if !crate::runtime::activity::is_read_only(def.name)
            && def.name != "send_dm"
            && def.name != "send_to_server"
        {
            let place = ctx.core.channel_label(Some(guild_id), ctx.channel_id);
            // Human summary only: what ran, who asked, where. Tool
            // RESULTS are model-facing (progress notes, nudges) and never
            // belong in the activity feed.
            crate::runtime::activity::log(
                &ctx.core,
                guild_id,
                &format!(
                    "{}{} — asked by {} in {place}",
                    step_label(def.name, &call.arguments),
                    if ctx.requester_bypassed() {
                        " 🔑"
                    } else {
                        ""
                    },
                    if ctx.acting_as_agent() {
                        "itself".to_owned()
                    } else {
                        ctx.requester.name.clone()
                    }
                ),
            );
        }
    }
    match result {
        // Too long to take in at once: an outline to read section by section.
        Ok(text) if text.len() > docs::LONG && def.name != "read_section" => ToolOutcome {
            text: docs::store(
                &ctx.core.docs,
                &step_label(def.name, &call.arguments),
                &text,
            ),
            ok: true,
            notice: None,
        },
        Ok(text) => ToolOutcome {
            text: crate::llm::truncate(&text, RESULT_MAX),
            ok: true,
            notice: None,
        },
        Err(error) if error.starts_with("denied") => {
            // A permission failure inside the tool: the app posts the
            // fixed message so the model does not have to.
            let needed = error
                .split("lacks")
                .nth(1)
                .map(|rest| {
                    rest.split(" in ")
                        .next()
                        .unwrap_or(rest)
                        .trim()
                        .trim_end_matches('.')
                        .to_owned()
                })
                .unwrap_or_else(|| "the required permissions".to_owned());
            ToolOutcome {
                text: format!("{error} — the system already told them; do not repeat this"),
                ok: false,
                notice: Some(format!(
                    "You don't have the required permissions for that: {needed}."
                )),
            }
        }
        Err(error) => ToolOutcome {
            text: format!("error: {error}"),
            ok: false,
            notice: None,
        },
    }
}

fn denied(text: String) -> ToolOutcome {
    ToolOutcome {
        text: format!("denied: {text}"),
        ok: false,
        notice: None,
    }
}

async fn run_mcp(ctx: &ToolCtx, call: &ToolCall, config: &ToolConfig) -> ToolOutcome {
    if !config.mcp {
        return denied("MCP tools are turned off for this agent".to_owned());
    }
    let Some(tool) = ctx
        .core
        .hive
        .mcp
        .tools()
        .into_iter()
        .find(|t| t.exposed_name == call.name)
    else {
        return ToolOutcome {
            text: format!("error: MCP tool {} is not available right now", call.name),
            ok: false,
            notice: None,
        };
    };
    let allowed = ctx.requester.kind == PrincipalKind::Operator
        || (!ctx.acting_as_agent() && ctx.requester.level >= tool.access)
        || (ctx.acting_as_agent() && tool.access == AccessLevel::Everyone);
    if !allowed {
        return denied(format!(
            "{} tools are limited to {} (the requester is {})",
            tool.server_name,
            tool.access.label().to_lowercase(),
            ctx.requester.describe_rank()
        ));
    }
    match ctx
        .core
        .hive
        .mcp
        .call(&call.name, call.arguments.clone())
        .await
    {
        Ok(text) => ToolOutcome {
            text: crate::llm::truncate(&text, RESULT_MAX),
            ok: true,
            notice: None,
        },
        Err(error) => ToolOutcome {
            text: format!("error: {error}"),
            ok: false,
            notice: None,
        },
    }
}

/// Run a tool whose confirmation button was clicked. The action was
/// permission-checked for the original requester when it was queued;
/// `run_confirmed` re-runs it with that same authority (the approver
/// proved they are the requester or a moderator).
pub async fn run_confirmed(
    core: &Arc<AgentCore>,
    name: &str,
    args: Value,
    requester: Principal,
    channel_id: u64,
) -> Result<String, String> {
    let guild_id = core.world.guild_ids().into_iter().find(|g| {
        core.world
            .with_guild(*g, |k| k.channels.contains_key(&channel_id))
            .unwrap_or(false)
    });
    let ctx = ToolCtx {
        core: Arc::clone(core),
        guild_id,
        channel_id,
        message_id: None,
        requester,
        autonomous: false,
        replies_here: true,
        media: Arc::default(),
        wants_file_attached: false,
        copy_fill_asked: false,
        inputs: Vec::new(),
    };
    let run = dispatch(&ctx, name, &args);
    match tokio::time::timeout(std::time::Duration::from_secs(90), run).await {
        Ok(result) => result,
        Err(_) => Err("the tool timed out".to_owned()),
    }
}

pub(crate) async fn dispatch(ctx: &ToolCtx, name: &str, args: &Value) -> Result<String, String> {
    use discord as d;
    match name {
        "get_server_info" => d::get_server_info(ctx).await,
        "list_channels" => d::list_channels(ctx).await,
        "list_roles" => d::list_roles(ctx).await,
        "get_member" => d::get_member(ctx, args).await,
        "list_members" if arg(args, "server").is_some() || ctx.guild_id.is_none() => {
            reach::list_server_members(ctx, args).await
        }
        "list_members" => d::list_members(ctx, args).await,
        "read_messages" => d::read_messages(ctx, args).await,
        "search_messages" => d::search_messages(ctx, args).await,
        "get_pinned_messages" => d::get_pinned(ctx, args).await,
        "list_forum_posts" => d::list_forum_posts(ctx, args).await,
        "list_threads" => d::list_threads(ctx).await,
        "voice_overview" => d::voice_overview(ctx).await,
        // Older name: same search.
        "get_audit_log" => {
            let mut args = args.clone();
            if let (Some(user), Some(map)) = (args.get("user").cloned(), args.as_object_mut()) {
                map.insert("by".to_owned(), user);
            }
            reach::search_audit_logs(ctx, &args).await
        }
        "send_message" => d::send_message(ctx, args).await,
        "send_dm" => reach::send_dm(ctx, args).await,
        "edit_my_message" => d::edit_my_message(ctx, args).await,
        "delete_my_message" => d::delete_my_message(ctx, args).await,
        "pin_message" => d::pin_message(ctx, args).await,
        "react" => d::react(ctx, args).await,
        "create_poll" => d::create_poll(ctx, args).await,
        "create_thread" => d::create_thread(ctx, args).await,
        "create_forum_post" => d::create_forum_post(ctx, args).await,
        "archive_thread" => d::archive_thread(ctx, args).await,
        "add_to_thread" => d::add_to_thread(ctx, args).await,
        "move_member" => d::move_member(ctx, args).await,
        "join_voice" => d::join_voice(ctx, args).await,
        "leave_voice" => d::leave_voice(ctx, args).await,
        "say_in_voice" => d::say_in_voice(ctx, args).await,
        "list_voices" => d::list_voices(ctx, args).await,
        "set_voice" => d::set_voice(ctx, args).await,
        "disconnect_member" => d::disconnect_member(ctx, args).await,
        "voice_mute" => d::voice_mute(ctx, args).await,
        "delete_message" => d::delete_message(ctx, args).await,
        "purge_messages" => d::purge_messages(ctx, args).await,
        "warn_member" => d::warn_member(ctx, args).await,
        "timeout_member" => d::timeout_member(ctx, args).await,
        "kick_member" => d::kick_member(ctx, args).await,
        "ban_member" => d::ban_member(ctx, args).await,
        "set_slowmode" => d::set_slowmode(ctx, args).await,
        "lock_channel" => d::lock_channel(ctx, args).await,
        "get_strikes" => d::get_strikes(ctx, args).await,
        "add_role" => d::change_role(ctx, args, true).await,
        "remove_role" => d::change_role(ctx, args, false).await,
        "create_role" => d::create_role(ctx, args).await,
        "create_channel" => d::create_channel(ctx, args).await,
        "clone_channel" => power::clone_channel(ctx, args).await,
        "set_voice_status" => power::set_voice_status(ctx, args).await,
        "move_channel" => power::move_channel(ctx, args).await,
        "set_channel_permissions" => power::set_channel_permissions(ctx, args).await,
        "edit_thread" => power::edit_thread(ctx, args).await,
        "unpin_message" => power::unpin_message(ctx, args).await,
        "publish_message" => power::publish_message(ctx, args).await,
        "end_poll" => power::end_poll(ctx, args).await,
        "create_vote" => power::create_vote(ctx, args).await,
        "end_vote" => power::end_vote(ctx, args).await,
        "list_votes" => power::list_votes(ctx, args).await,
        "edit_role" => power::edit_role(ctx, args).await,
        "delete_role" => power::delete_role(ctx, args).await,
        "list_bans" => power::list_bans(ctx, args).await,
        "unban_member" => power::unban_member(ctx, args).await,
        "edit_server" => power::edit_server(ctx, args).await,
        "create_event" => power::create_event(ctx, args).await,
        "list_events" => power::list_events(ctx, args).await,
        "delete_event" => power::delete_event(ctx, args).await,
        "create_emoji" => power::create_emoji(ctx, args).await,
        "delete_emoji" => power::delete_emoji(ctx, args).await,
        "setup_tickets" => power::setup_tickets(ctx, args).await,
        "edit_ticket_panel" => power::edit_ticket_panel(ctx, args).await,
        "ticket_settings" => power::ticket_settings(ctx, args).await,
        "ticket_access" | "add_to_ticket" => power::ticket_access(ctx, args).await,
        "list_tickets" => power::list_tickets(ctx, args).await,
        "close_ticket" => power::close_ticket(ctx, args).await,
        "reopen_ticket" => power::reopen_ticket(ctx, args).await,
        "claim_ticket" => power::claim_ticket(ctx, args).await,
        "rename_ticket" => power::rename_ticket(ctx, args).await,
        "open_ticket" => power::open_ticket(ctx, args).await,
        "ticket_transcript" => power::ticket_transcript(ctx, args).await,
        "search_tickets" => power::search_tickets(ctx, args).await,
        "discord_api" => power::discord_api(ctx, args).await,
        "find_users" => users::find_users(ctx, args).await,
        "pick_users" => users::pick_users(ctx, args).await,
        "find_people" => reach::find_people(ctx, args).await,
        "privacy_settings" => power::privacy_settings(ctx, args).await,
        "send_to_server" => reach::send_to_server(ctx, args).await,
        "search_audit_logs" => reach::search_audit_logs(ctx, args).await,
        "list_servers" => reach::list_servers(ctx, args).await,
        "write_file" => files::write_file(ctx, args).await,
        "run_script" => files::run_script(ctx, args).await,
        "write_section" => longform::write_section(ctx, args).await,
        "attach_file" => files::attach_file(ctx, args).await,
        "duplicate_file" => files::duplicate_file(ctx, args).await,
        "recall_scratch" => scratch::recall(ctx, args).await,
        "set_logging" => extra::set_logging(ctx, args).await,
        "convert_file" => files::convert_file(ctx, args).await,
        "list_files" => files::list_files(ctx, args).await,
        "find_in_document" => docs::find(
            &ctx.core.docs,
            &required(args, "document")?,
            &required(args, "text")?,
        ),
        "read_section" => docs::read(
            &ctx.core.docs,
            &required(args, "document")?,
            &arg(args, "section").unwrap_or_default(),
        ),
        "set_my_avatar" => power::set_my_avatar(ctx, args).await,
        "make_plan" | "step_done" => Err("plans are handled by the agent loop".to_owned()),
        "start_translating" => power::start_translating(ctx, args).await,
        "stop_translating" => power::stop_translating(ctx, args).await,
        "get_rank" => power::get_rank(ctx, args).await,
        "leaderboard" => power::leaderboard(ctx, args).await,
        "server_stats" => power::server_stats(ctx, args).await,
        "leveling_settings" => power::leveling_settings(ctx, args).await,
        "set_xp" => power::set_xp(ctx, args).await,
        "design_rank_card" => power::design_rank_card(ctx, args).await,
        "stats_settings" => power::stats_settings(ctx, args).await,
        "settings_access" => power::settings_access(ctx, args).await,
        "show_settings" => power::show_settings(ctx, args).await,
        "create_role_menu" => power::create_role_menu(ctx, args).await,
        "start_giveaway" => power::start_giveaway(ctx, args).await,
        "end_giveaway" => power::end_giveaway(ctx, args).await,
        "list_giveaways" => power::list_giveaways(ctx, args).await,
        "edit_channel" => d::edit_channel(ctx, args).await,
        "delete_channel" => d::delete_channel(ctx, args).await,
        "set_nickname" => d::set_nickname(ctx, args).await,
        "web_search" => extra::web_search(ctx, args).await,
        "get_avatar" => images::get_avatar(ctx, args).await,
        "avatar_grid" => images::avatar_grid(ctx, args).await,
        "edit_image" => images::edit_image(ctx, args).await,
        "look_at_image" => images::look_at_image(ctx, args).await,
        "attach_image" => images::attach_image(ctx, args).await,
        "image_search" => images::image_search(ctx, args).await,
        "draw_image" => images::draw_image(ctx, args).await,
        "make_chart" => images::make_chart(ctx, args).await,
        "find_gifs" => gifs::find_gifs(ctx, args).await,
        "find_stickers" => gifs::find_stickers(ctx, args).await,
        "inspect_gif" => gifs::inspect_gif(ctx, args).await,
        "send_gif" => gifs::send_gif(ctx, args).await,
        "send_sticker" => gifs::send_sticker(ctx, args).await,
        "list_recent_gifs" => gifs::list_recent_gifs(ctx, args).await,
        "fetch_url" => extra::fetch_url(ctx, args).await,
        other => extra::dispatch(ctx, other, args).await,
    }
}

/// Pick the tools to offer for this turn. Large models get every enabled
/// tool; small ones get the core set plus whatever the message is about,
/// within a schema budget — fewer, relevant tools make small models far
/// more reliable.
/// A multi-word keyword whose words all appear ("delete … channel").
fn all_words(text: &str, keyword: &str) -> bool {
    keyword.contains(' ')
        && keyword
            .split(' ')
            .filter(|w| w.len() >= 3 && !matches!(*w, "the" | "and" | "for"))
            .all(|w| {
                text.split(|c: char| !c.is_alphanumeric())
                    .any(|t| t == w || t.strip_suffix('s') == Some(w))
            })
}

/// Tools for "what": the agent asked for one it doesn't have. Returns
/// (tools it may use, "tool (needs X)" for ones this person can't).
pub fn find_tools(
    core: &AgentCore,
    requester: &Principal,
    in_guild: bool,
    what: &str,
    have: &[String],
) -> (Vec<ToolSpec>, Vec<String>) {
    let profile = core.profile();
    let query = what.to_lowercase();
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .collect();
    let mut scored: Vec<(usize, &ToolDef)> = catalog()
        .iter()
        .filter(|d| d.group.enabled(&profile.tools) && (in_guild || !d.guild_only))
        .filter(|d| !have.iter().any(|h| h == d.name) && d.name != "load_tools")
        .map(|d| {
            let name = d.name.replace('_', " ");
            let description = d.description.to_lowercase();
            let score = words
                .iter()
                .map(|w| {
                    let stem = w.trim_end_matches('s');
                    (if name.contains(stem) { 4 } else { 0 })
                        + (if d.keywords.iter().any(|k| k.contains(stem)) {
                            2
                        } else {
                            0
                        })
                        + usize::from(description.contains(stem))
                })
                .sum::<usize>();
            (score, d)
        })
        .filter(|(score, _)| *score >= 3)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut allowed = Vec::new();
    let mut blocked = Vec::new();
    for (_, d) in scored.into_iter().take(6) {
        let may = d.requires.is_empty()
            || requester.kind == PrincipalKind::Operator
            || requester.permissions.contains(d.requires);
        if may {
            if allowed.len() < 4 {
                allowed.push(ToolSpec {
                    name: d.name.to_owned(),
                    description: d.description.to_owned(),
                    parameters: d.params.clone(),
                });
            }
        } else {
            blocked.push(format!(
                "{} (needs {})",
                d.name,
                crate::runtime::authority::missing(d.requires, requester.permissions)
            ));
        }
    }
    (allowed, blocked)
}

/// Tools a message asks for that this person isn't allowed to use — so
/// the agent says "you don't have permission" instead of "I can't".
pub fn blocked_tools(
    core: &AgentCore,
    requester: &Principal,
    in_guild: bool,
    message_text: &str,
) -> Vec<String> {
    if requester.kind == PrincipalKind::Operator {
        return Vec::new();
    }
    let profile = core.profile();
    let text = message_text.to_lowercase();
    let text = text.as_str();
    catalog()
        .iter()
        .filter(|d| d.group.enabled(&profile.tools) && (in_guild || !d.guild_only))
        .filter(|d| !d.requires.is_empty() && !requester.permissions.contains(d.requires))
        .filter(|d| {
            !matches!(
                d.name,
                "move_member" | "disconnect_member" | "delete_message" | "pin_message"
            )
        })
        .filter(|d| {
            d.keywords
                .iter()
                .any(|k| k.len() >= 4 && (text.contains(k) || all_words(text, k)))
        })
        .map(|d| {
            format!(
                "{} (needs {})",
                d.name,
                crate::runtime::authority::missing(d.requires, requester.permissions)
            )
        })
        .take(6)
        .collect()
}

pub fn select_tools(
    core: &AgentCore,
    requester: &Principal,
    autonomous: bool,
    in_guild: bool,
    message_text: &str,
    recent_text: &str,
    compact: bool,
    budget_chars: usize,
) -> Vec<ToolSpec> {
    let profile = core.profile();
    let text = message_text.to_lowercase();
    let recent = recent_text.to_lowercase();
    // Words of the message, for typo-tolerant matching ("leaderbaord").
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 5)
        .collect();
    let close = |keyword: &str| {
        !keyword.contains(' ')
            && keyword.len() >= 5
            && words.iter().any(|w| {
                w.chars().next() == keyword.chars().next()
                    && w.len().abs_diff(keyword.len()) <= 2
                    && crate::runtime::voice::edit_distance(w, keyword)
                        <= if keyword.len() >= 8 { 2 } else { 1 }
            })
    };
    let staff =
        requester.level >= AccessLevel::Moderator || requester.kind == PrincipalKind::Operator;
    // "delete them", "do what she said", "do it again": the earlier
    // conversation says what.
    let follow_up = text.len() < 80
        && text.split(|c: char| !c.is_alphanumeric()).any(|w| {
            matches!(
                w,
                "them" | "it" | "those" | "these" | "that" | "said" | "again" | "same" | "rest"
            )
        });
    let mut scored: Vec<(i32, &ToolDef)> = catalog()
        .iter()
        .filter(|d| d.group.enabled(&profile.tools))
        .filter(|d| in_guild || !d.guild_only)
        .filter(|d| {
            // On its own initiative, the agent's autonomy decides.
            if autonomous {
                return profile.behavior.autonomy >= d.autonomy;
            }
            // Don't offer tools the requester could never use.
            d.requires.is_empty()
                || requester.kind == PrincipalKind::Operator
                || requester.permissions.contains(d.requires)
                || matches!(
                    d.name,
                    "move_member" | "disconnect_member" | "delete_message" | "pin_message"
                )
        })
        .map(|d| {
            // What the message asks for outranks everything: a tool it
            // names beats the always-on core set, which beats tools the
            // earlier conversation touched on.
            let hits = d
                .keywords
                .iter()
                .filter(|k| text.contains(*k) || close(k) || all_words(&text, k))
                .count() as i32;
            let earlier = d.keywords.iter().filter(|k| recent.contains(*k)).count() as i32;
            let mut score = hits * 30
                + if follow_up {
                    earlier.min(3) * 15
                } else {
                    earlier.min(2) * 4
                };
            if d.core {
                score += 25;
            }
            if staff && matches!(d.group, Group::Moderation) {
                score += 5;
            }
            if d.name == "discord_api"
                && (requester.kind == PrincipalKind::Operator
                    || requester.level >= AccessLevel::Admin)
            {
                score += 12;
            }
            (score, d)
        })
        .collect();
    // Tools that need another one to do the job bring it along.
    const COMPANIONS: [(&str, &[&str]); 11] = [
        ("clone_channel", &["delete_channel", "list_channels"]),
        ("create_channel", &["delete_channel", "list_channels"]),
        ("get_avatar", &["avatar_grid"]),
        ("avatar_grid", &["get_avatar", "list_members"]),
        ("design_rank_card", &["image_search", "get_rank"]),
        (
            "set_my_avatar",
            &["image_search", "edit_image", "draw_image"],
        ),
        ("create_emoji", &["image_search", "edit_image"]),
        ("edit_ticket_panel", &["list_channels"]),
        ("create_role_menu", &["list_roles"]),
        ("make_plan", &["step_done"]),
        (
            "draft_create",
            &[
                "draft_write",
                "draft_edit",
                "draft_view",
                "draft_publish",
                "make_plan",
                "step_done",
            ],
        ),
    ];
    // Requests the agent recognizes on its own always get their tool.
    let recognized = [
        crate::runtime::brain::schedule_request(message_text),
        crate::runtime::brain::channel_rule_request(message_text),
        crate::runtime::brain::github_request(message_text),
        crate::runtime::brain::timing_request(message_text).then_some("wait"),
    ];
    for (score, def) in scored.iter_mut() {
        if recognized.contains(&Some(def.name)) {
            *score = (*score).max(60);
        }
    }
    let wanted: Vec<&str> = COMPANIONS
        .iter()
        .filter(|(tool, _)| {
            scored
                .iter()
                .any(|(score, d)| d.name == *tool && *score >= 30)
        })
        .flat_map(|(_, companions)| companions.iter().copied())
        .collect();
    for (score, def) in scored.iter_mut() {
        if wanted.contains(&def.name) {
            *score = (*score).max(35);
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = Vec::new();
    let mut used = 0usize;
    for (score, def) in scored {
        if compact && score == 0 {
            continue;
        }
        let cost = def.description.len() + def.params.to_string().len() + 40;
        if used + cost > budget_chars && !out.is_empty() {
            continue;
        }
        used += cost;
        out.push(ToolSpec {
            name: def.name.to_owned(),
            description: def.description.to_owned(),
            parameters: def.params.clone(),
        });
    }
    if profile.tools.mcp {
        for tool in core.hive.mcp.tools() {
            let allowed =
                requester.kind == PrincipalKind::Operator || requester.level >= tool.access;
            if !allowed {
                continue;
            }
            let cost = tool.description.len() + tool.schema.to_string().len() + 40;
            if used + cost > budget_chars {
                continue;
            }
            used += cost;
            out.push(ToolSpec {
                name: tool.exposed_name.clone(),
                description: crate::llm::truncate(&tool.description, 600),
                parameters: tool.schema.clone(),
            });
        }
    }
    out
}

/// A short present-tense phrase for the thinking message, e.g.
/// "searching the web for “rust 2024”".
pub fn step_label(name: &str, args: &Value) -> String {
    let get = |key: &str| {
        args.get(key)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .map(|s| crate::llm::truncate(&s, 60))
            .unwrap_or_default()
    };
    match name {
        "read_section" => format!("reading {} section {}", get("document"), get("section")),
        "find_in_document" => format!("looking for “{}” in {}", get("text"), get("document")),
        "draft_create" => format!("starting {}", get("filename")),
        "draft_write" => format!("writing {} section {}", get("draft"), get("section")),
        "draft_edit" => format!("editing {}", get("draft")),
        "draft_view" => format!("checking {}", get("draft")),
        "draft_publish" => format!("finishing {}", get("draft")),
        "find_people" => format!("looking for “{}”", get("name")),
        "send_to_server" => format!("posting in {} on {}", get("channel"), get("server")),
        "search_audit_logs" => "checking the audit logs".to_owned(),
        "list_servers" => "checking my servers".to_owned(),
        "make_plan" => "planning the steps".to_owned(),
        "step_done" => "finishing a step".to_owned(),
        "set_my_avatar" => "changing my profile picture".to_owned(),
        "web_search" => format!("searching the web for “{}”", get("query")),
        "fetch_url" => format!("reading {}", get("url")),
        "read_messages" => match get("channel") {
            channel if channel.is_empty() => "reading back through this channel".to_owned(),
            channel => format!("reading {channel}"),
        },
        "list_forum_posts" => format!("reading the posts in {}", get("forum")),
        "search_messages" => format!("searching messages for “{}”", get("query")),
        "get_member" => format!("looking up {}", get("user")),
        "join_voice" => "joining voice".to_owned(),
        "leave_voice" => "leaving voice".to_owned(),
        "say_in_voice" => "speaking".to_owned(),
        "list_voices" => "checking my voices".to_owned(),
        "set_voice" => "changing my voice".to_owned(),
        "list_members" => "checking the member list".to_owned(),
        "get_server_info" => "checking the server".to_owned(),
        "list_channels" => "looking at the channels".to_owned(),
        "list_roles" => "looking at the roles".to_owned(),
        "react" => format!("reacting {}", get("emoji")),
        "send_message" => format!("posting in {}", get("channel")),
        "duplicate_file" => format!("copy-pasting {}", get("name")),
        "send_dm" => format!("DMing {}", get("user")),
        "create_poll" => "setting up a poll".to_owned(),
        "create_thread" => format!("opening a thread “{}”", get("name")),
        "create_forum_post" => format!("posting in {}", get("forum")),
        "move_member" => format!("moving {} to {}", get("user"), get("channel")),
        "disconnect_member" => format!("disconnecting {}", get("user")),
        "timeout_member" => format!("timing out {}", get("user")),
        "warn_member" => format!("warning {}", get("user")),
        "kick_member" => format!("kicking {}", get("user")),
        "ban_member" => format!("banning {}", get("user")),
        "delete_message" | "purge_messages" => "cleaning up messages".to_owned(),
        "remember" => "saving that to memory".to_owned(),
        "recall" => format!("remembering “{}”", get("query")),
        "note_about_person" => format!("taking a note about {}", get("user")),
        "set_directive" => "noting a standing instruction".to_owned(),
        "go_quiet" => "going quiet".to_owned(),
        "load_tools" => format!("finding a tool to {}", get("what")),
        "wait" => format!("waiting {}s", get("seconds")),
        "watch_github" => format!("watching {} on GitHub", get("repo")),
        "stop_watching_github" => format!("no longer watching {}", get("repo")),
        "create_command" => format!("making /{}", get("name").trim_start_matches('/')),
        "delete_command" => format!("deleting /{}", get("name").trim_start_matches('/')),
        "toggle_command" => format!("switching /{}", get("name").trim_start_matches('/')),
        "list_commands" => "listing the commands".to_owned(),
        "stop_chatting_in" => format!("staying out of {}", get("channels")),
        "allow_chatting_in" => format!("allowing myself back in {}", get("channels")),
        "post_to_hq" | "ask_agent" => "coordinating with the other agents".to_owned(),
        "calculate" => format!("calculating {}", get("expression")),
        "weather" => format!("checking the weather in {}", get("location")),
        "wikipedia" => format!("looking up “{}” on Wikipedia", get("query")),
        "define_word" => format!("looking up “{}”", get("word")),
        "convert_currency" => format!(
            "converting {} {} to {}",
            get("amount"),
            get("from"),
            get("to")
        ),
        "set_reminder" => "setting a reminder".to_owned(),
        "create_invite" => "making an invite link".to_owned(),
        "server_activity" => "crunching activity stats".to_owned(),
        "get_avatar" => format!("getting {}'s profile picture", get("user")),
        "avatar_grid" => "putting their profile pictures together".to_owned(),
        "edit_image" => "editing the image".to_owned(),
        "look_at_image" => "looking at the image".to_owned(),
        "attach_image" => "attaching the image".to_owned(),
        "image_search" => format!("searching images for “{}”", get("query")),
        "draw_image" => "drawing".to_owned(),
        "find_gifs" => format!("searching GIFs for “{}”", get("query")),
        "find_stickers" => format!("searching stickers for “{}”", get("query")),
        "inspect_gif" => "previewing media with vision".to_owned(),
        "send_gif" => "sending GIF".to_owned(),
        "send_sticker" => "sending sticker".to_owned(),
        "list_recent_gifs" => "checking recent GIFs and stickers".to_owned(),
        "make_chart" => format!(
            "making a {} chart",
            if get("type").is_empty() {
                "bar".to_owned()
            } else {
                get("type")
            }
        ),
        other if other.starts_with("mcp_") => format!("using {}", other.trim_start_matches("mcp_")),
        other => other.replace('_', " "),
    }
}

// --- argument helpers -------------------------------------------------------

pub fn arg(args: &Value, key: &str) -> Option<String> {
    match args.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn required(args: &Value, key: &str) -> Result<String, String> {
    arg(args, key).ok_or_else(|| format!("missing \"{key}\""))
}

pub fn number(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// "display name “X” · username @y · ID n" — spelled out so the model
/// never mixes up display names and usernames.
pub fn person_label(nick: Option<&str>, global: Option<&str>, username: &str, id: u64) -> String {
    let nick = nick.map(str::trim).filter(|n| !n.is_empty());
    let global = global.map(str::trim).filter(|g| !g.is_empty());
    let display = match (nick, global) {
        (Some(n), Some(g)) if n != g => {
            format!("display name “{n}” (server nickname; their global display name is “{g}”)")
        }
        (Some(n), _) => format!("display name “{n}” (server nickname)"),
        (None, Some(g)) => format!("display name “{g}”"),
        (None, None) => format!("display name “{username}” (none set, so it shows their username)"),
    };
    format!("{display} · username @{username} · ID {id}")
}

pub fn flag(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Some(true),
            "false" | "no" | "0" | "off" => Some(false),
            _ => None,
        },
        Value::Number(n) => Some(n.as_i64() != Some(0)),
        _ => None,
    }
}

pub fn list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.trim().to_owned()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split([',', '\n', '|'])
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Explain a Discord API failure in words the agent can relay.
/// Discord refused for lack of permission or access.
pub fn is_permission_error(error: &AxisError) -> bool {
    match error {
        AxisError::Http(http) => matches!(http.code, Some(50013 | 50001)) || http.status == 403,
        _ => false,
    }
}

pub fn discord_error(error: AxisError) -> String {
    match &error {
        AxisError::Http(http) => match http.code {
            Some(50013) => "Discord says I (the bot) lack permission — my role needs that permission and must sit above the target's highest role".to_owned(),
            Some(50001) => "I (the bot) can't access that channel".to_owned(),
            Some(10008) => "that message doesn't exist (maybe it was deleted)".to_owned(),
            Some(10007) => "that person isn't a member of this server".to_owned(),
            Some(10013) => "that user doesn't exist".to_owned(),
            Some(10003) => "that channel doesn't exist".to_owned(),
            Some(10011) => "that role doesn't exist".to_owned(),
            Some(10026) => "that user isn't banned".to_owned(),
            Some(50007) => "I can't DM that user (their DMs are closed)".to_owned(),
            Some(50034) => "messages older than 14 days can't be bulk-deleted".to_owned(),
            Some(160002) => "that message already has a thread".to_owned(),
            _ => format!(
                "Discord rejected that ({}{})",
                http.status,
                http.message
                    .as_ref()
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            ),
        },
        _ => format!("request failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_well_formed() {
        let mut names = std::collections::HashSet::new();
        for def in catalog() {
            assert!(names.insert(def.name), "duplicate tool {}", def.name);
            assert!(def.name.len() <= 64);
            assert_eq!(def.params["type"], "object");
            assert!(def.description.len() > 10);
            for required in def.params["required"].as_array().unwrap() {
                let key = required.as_str().unwrap();
                assert!(
                    def.params["properties"].get(key).is_some(),
                    "{}: {key}",
                    def.name
                );
            }
        }
        assert!(catalog().len() > 50, "{}", catalog().len());
    }

    #[test]
    fn arg_helpers_are_lenient() {
        let args = json!({ "a": " x ", "n": "12", "b": "yes", "l": "one, two", "arr": ["p", 3] });
        assert_eq!(arg(&args, "a").as_deref(), Some("x"));
        assert_eq!(number(&args, "n"), Some(12));
        assert_eq!(flag(&args, "b"), Some(true));
        assert_eq!(list(&args, "l"), vec!["one", "two"]);
        assert_eq!(list(&args, "arr"), vec!["p", "3"]);
        assert!(required(&args, "missing").is_err());
    }

    #[test]
    fn step_labels_read_naturally() {
        assert_eq!(
            step_label("web_search", &json!({ "query": "rust" })),
            "searching the web for “rust”"
        );
        assert_eq!(step_label("some_tool", &json!({})), "some tool");
    }
}
