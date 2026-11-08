//! The agent's picture of Discord, kept current from gateway events:
//! guilds, channels, threads, roles, members, and who is in voice.
//!
//! Reading from this cache instead of calling the API on every message is
//! what lets an agent watch busy servers without hitting rate limits.

use std::{collections::HashMap, sync::RwLock};

use axis::model::{
    Channel, ChannelType, Emoji, Guild, GuildCreate, Member, Role, User, VoiceState,
};
use axis::prelude::Timestamp;

#[derive(Debug, Clone)]
pub struct MemberInfo {
    pub user_id: u64,
    pub username: String,
    pub global_name: Option<String>,
    pub nick: Option<String>,
    pub roles: Vec<u64>,
    pub bot: bool,
    pub joined_at: Option<Timestamp>,
    pub timeout_until: Option<Timestamp>,
    pub avatar: Option<String>,
    pub guild_avatar: Option<String>,
}

impl MemberInfo {
    pub fn display_name(&self) -> &str {
        self.nick
            .as_deref()
            .or(self.global_name.as_deref())
            .unwrap_or(&self.username)
    }

    fn from_member(member: &Member, user: &User) -> Self {
        Self {
            user_id: user.id.get(),
            username: user.username.clone(),
            global_name: user.global_name.clone(),
            nick: member.nick.clone(),
            roles: member.roles.iter().map(|id| id.get()).collect(),
            bot: user.bot.unwrap_or(false),
            joined_at: member.joined_at,
            timeout_until: member.communication_disabled_until,
            avatar: user.avatar.clone(),
            guild_avatar: member.avatar.clone(),
        }
    }

    pub fn is_timed_out(&self) -> bool {
        self.timeout_until
            .is_some_and(|until| until > Timestamp::now_utc())
    }
}

#[derive(Debug, Clone)]
pub struct GuildState {
    pub id: u64,
    pub name: String,
    pub owner_id: u64,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub features: Vec<String>,
    pub system_channel: Option<u64>,
    pub rules_channel: Option<u64>,
    pub member_count: u64,
    pub roles: HashMap<u64, Role>,
    pub channels: HashMap<u64, Channel>,
    pub members: HashMap<u64, MemberInfo>,
    /// user ID → voice channel ID
    pub voice: HashMap<u64, u64>,
    pub emojis: Vec<Emoji>,
}

impl GuildState {
    fn from_guild(guild: &Guild) -> Self {
        Self {
            id: guild.id.get(),
            name: guild.name.clone(),
            owner_id: guild.owner_id.get(),
            icon: guild.icon.clone(),
            description: guild.description.clone(),
            features: guild.features.clone(),
            system_channel: guild.system_channel_id.map(|id| id.get()),
            rules_channel: guild.rules_channel_id.map(|id| id.get()),
            member_count: 0,
            roles: guild
                .roles
                .iter()
                .map(|r| (r.id.get(), r.clone()))
                .collect(),
            channels: HashMap::new(),
            members: HashMap::new(),
            voice: HashMap::new(),
            emojis: guild.emojis.clone(),
        }
    }

    fn update_guild(&mut self, guild: &Guild) {
        self.name = guild.name.clone();
        self.owner_id = guild.owner_id.get();
        self.icon = guild.icon.clone();
        self.description = guild.description.clone();
        self.features = guild.features.clone();
        self.system_channel = guild.system_channel_id.map(|id| id.get());
        self.rules_channel = guild.rules_channel_id.map(|id| id.get());
        self.roles = guild
            .roles
            .iter()
            .map(|r| (r.id.get(), r.clone()))
            .collect();
        self.emojis = guild.emojis.clone();
    }

    pub fn channel_name(&self, channel_id: u64) -> String {
        self.channels
            .get(&channel_id)
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| channel_id.to_string())
    }

    /// Roles sorted from highest to lowest position (excluding @everyone).
    pub fn sorted_roles(&self) -> Vec<&Role> {
        let mut roles: Vec<&Role> = self
            .roles
            .values()
            .filter(|role| role.id.get() != self.id)
            .collect();
        roles.sort_by(|a, b| b.position.cmp(&a.position));
        roles
    }

    /// The member's highest role, if any.
    pub fn top_role(&self, member: &MemberInfo) -> Option<&Role> {
        member
            .roles
            .iter()
            .filter_map(|id| self.roles.get(id))
            .max_by_key(|role| role.position)
    }

    /// Text-like channels sorted by position, for listings.
    pub fn sorted_channels(&self) -> Vec<&Channel> {
        let mut channels: Vec<&Channel> = self.channels.values().collect();
        channels.sort_by_key(|c| {
            (
                c.parent_id.map(|p| p.get()).unwrap_or(0),
                c.position.unwrap_or(0),
            )
        });
        channels
    }

    /// Find a channel by `#name`, mention, or ID.
    pub fn find_channel(&self, query: &str) -> Option<&Channel> {
        self.find_channel_where(query, |_| true)
    }

    /// Like `find_channel`, among channels that pass `keep` (so "ticket
    /// support" finds the text channel, not the category of that name).
    pub fn find_channel_where(
        &self,
        query: &str,
        keep: impl Fn(&Channel) -> bool,
    ) -> Option<&Channel> {
        let query = query.trim();
        let id = parse_snowflake(query);
        if let Some(id) = id {
            if let Some(channel) = self.channels.get(&id).filter(|c| keep(c)) {
                return Some(channel);
            }
        }
        let name = query.trim_start_matches('#').to_lowercase();
        // Compare letters and digits only, so "sue updates" finds
        // "🪶sue-updates".
        let squash = |text: &str| -> String {
            text.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect()
        };
        let exact = self
            .channels
            .values()
            .filter(|c| keep(c))
            .find(|c| c.name.as_deref().is_some_and(|n| n.to_lowercase() == name));
        if exact.is_some() {
            return exact;
        }
        // Spoken or loosely written names: "the scratch pad three chat"
        // finds "scratchpad3".
        let spoken: String = name
            .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
            .filter(|w| {
                !matches!(
                    *w,
                    "the" | "channel" | "chat" | "text" | "in" | "my" | "our"
                )
            })
            .map(|w| match w {
                "zero" => "0",
                "one" => "1",
                "two" => "2",
                "three" => "3",
                "four" => "4",
                "five" => "5",
                "six" => "6",
                "seven" => "7",
                "eight" => "8",
                "nine" => "9",
                "ten" => "10",
                other => other,
            })
            .collect::<Vec<_>>()
            .join(" ");
        for wanted in [squash(&name), squash(&spoken)] {
            if wanted.is_empty() {
                continue;
            }
            let found = self
                .channels
                .values()
                .filter(|c| keep(c))
                .find(|c| c.name.as_deref().is_some_and(|n| squash(n) == wanted))
                .or_else(|| {
                    self.channels.values().filter(|c| keep(c)).find(|c| {
                        c.name
                            .as_deref()
                            .is_some_and(|n| squash(n).contains(&wanted))
                    })
                });
            if found.is_some() {
                return found;
            }
        }
        // Close misspellings (speech recognition): the nearest name.
        let wanted = squash(&spoken);
        if wanted.chars().count() < 4 {
            return None;
        }
        let allowed = (wanted.chars().count() / 4).max(1);
        self.channels
            .values()
            .filter(|c| keep(c))
            .filter_map(|c| {
                let distance = super::voice::edit_distance(&squash(c.name.as_deref()?), &wanted);
                (distance <= allowed).then_some((distance, c))
            })
            .min_by_key(|(distance, _)| *distance)
            .map(|(_, c)| c)
    }

    /// Find a role by mention, ID, or name.
    pub fn find_role(&self, query: &str) -> Option<&Role> {
        let query = query.trim();
        if let Some(id) = parse_snowflake(query) {
            if let Some(role) = self.roles.get(&id) {
                return Some(role);
            }
        }
        let name = query.trim_start_matches('@').to_lowercase();
        // Letters and digits only ("Gamer 🎮" finds "Gamer"), and plurals
        // ("Moderators" finds "Moderator").
        let squash = |text: &str| -> String {
            text.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect()
        };
        let wanted = squash(&name);
        let singular = wanted.trim_end_matches('s').to_owned();
        self.roles
            .values()
            .find(|r| r.name.to_lowercase() == name)
            .or_else(|| {
                self.roles
                    .values()
                    .find(|r| !wanted.is_empty() && squash(&r.name) == wanted)
            })
            .or_else(|| {
                self.roles.values().find(|r| {
                    !singular.is_empty() && squash(&r.name).trim_end_matches('s') == singular
                })
            })
            .or_else(|| {
                self.roles
                    .values()
                    .find(|r| !wanted.is_empty() && squash(&r.name).contains(&wanted))
            })
    }

    /// Find a cached member by mention, ID, username, or display name.
    pub fn find_member(&self, query: &str) -> Option<&MemberInfo> {
        let query = query.trim();
        if let Some(id) = parse_snowflake(query) {
            return self.members.get(&id);
        }
        let name = query.trim_start_matches('@').to_lowercase();
        let exact = self.members.values().find(|m| {
            m.username.to_lowercase() == name
                || m.display_name().to_lowercase() == name
                || m.global_name
                    .as_deref()
                    .is_some_and(|g| g.to_lowercase() == name)
        });
        exact.or_else(|| {
            self.members.values().find(|m| {
                m.username.to_lowercase().starts_with(&name)
                    || m.display_name().to_lowercase().starts_with(&name)
            })
        })
    }

    /// For threads, the parent channel; otherwise the channel itself.
    pub fn permission_source(&self, channel_id: u64) -> Option<&Channel> {
        let channel = self.channels.get(&channel_id)?;
        if channel.is_thread() {
            channel
                .parent_id
                .and_then(|parent| self.channels.get(&parent.get()))
        } else {
            Some(channel)
        }
    }
}

/// Parse `<@123>`, `<@!123>`, `<#123>`, `<@&123>`, or a bare ID.
pub fn parse_snowflake(text: &str) -> Option<u64> {
    let trimmed = text
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_start_matches(['@', '#', '!', '&']);
    let trimmed = trimmed.trim_start_matches(['!', '&']);
    if trimmed.len() >= 15 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        trimmed.parse().ok()
    } else {
        None
    }
}

#[derive(Debug, Default)]
pub struct World {
    me: RwLock<Option<User>>,
    guilds: RwLock<HashMap<u64, GuildState>>,
    /// DM channel ID → the other user's ID.
    dms: RwLock<HashMap<u64, u64>>,
}

impl World {
    /// Put a guild in the cache directly (simulation harness, experiments
    /// and tests).
    pub fn insert_guild(&self, guild: &Guild, edit: impl FnOnce(&mut GuildState)) {
        let mut state = GuildState::from_guild(guild);
        edit(&mut state);
        self.guilds
            .write()
            .expect("world poisoned")
            .insert(state.id, state);
    }

    pub fn set_me(&self, user: User) {
        *self.me.write().expect("world poisoned") = Some(user);
    }

    pub fn me(&self) -> Option<User> {
        self.me.read().expect("world poisoned").clone()
    }

    pub fn my_id(&self) -> u64 {
        self.me
            .read()
            .expect("world poisoned")
            .as_ref()
            .map(|u| u.id.get())
            .unwrap_or(0)
    }

    pub fn with_guild<R>(&self, guild_id: u64, f: impl FnOnce(&GuildState) -> R) -> Option<R> {
        self.guilds
            .read()
            .expect("world poisoned")
            .get(&guild_id)
            .map(f)
    }

    fn with_guild_mut(&self, guild_id: u64, f: impl FnOnce(&mut GuildState)) {
        if let Some(guild) = self
            .guilds
            .write()
            .expect("world poisoned")
            .get_mut(&guild_id)
        {
            f(guild);
        }
    }

    pub fn guild_ids(&self) -> Vec<u64> {
        self.guilds
            .read()
            .expect("world poisoned")
            .keys()
            .copied()
            .collect()
    }

    pub fn guild_names(&self) -> Vec<(u64, String)> {
        let mut names: Vec<(u64, String)> = self
            .guilds
            .read()
            .expect("world poisoned")
            .values()
            .map(|g| (g.id, g.name.clone()))
            .collect();
        names.sort_by(|a, b| a.1.cmp(&b.1));
        names
    }

    /// The guild a channel belongs to, if cached.
    pub fn guild_of_channel(&self, channel_id: u64) -> Option<u64> {
        self.guilds
            .read()
            .expect("world poisoned")
            .values()
            .find(|g| g.channels.contains_key(&channel_id))
            .map(|g| g.id)
    }

    pub fn channel(&self, guild_id: u64, channel_id: u64) -> Option<Channel> {
        self.with_guild(guild_id, |g| g.channels.get(&channel_id).cloned())
            .flatten()
    }

    pub fn member(&self, guild_id: u64, user_id: u64) -> Option<MemberInfo> {
        self.with_guild(guild_id, |g| g.members.get(&user_id).cloned())
            .flatten()
    }

    pub fn note_dm(&self, channel_id: u64, user_id: u64) {
        self.dms
            .write()
            .expect("world poisoned")
            .insert(channel_id, user_id);
    }

    // --- event application -------------------------------------------------

    pub fn apply_guild_create(&self, create: &GuildCreate) {
        let mut state = GuildState::from_guild(&create.guild);
        state.member_count = create.member_count.unwrap_or(0);
        for channel in create.channels.iter().flatten() {
            let mut channel = channel.clone();
            channel.guild_id = Some(create.guild.id);
            state.channels.insert(channel.id.get(), channel);
        }
        for thread in create.threads.iter().flatten() {
            let mut thread = thread.clone();
            thread.guild_id = Some(create.guild.id);
            state.channels.insert(thread.id.get(), thread);
        }
        for member in create.members.iter().flatten() {
            if let Some(user) = &member.user {
                state
                    .members
                    .insert(user.id.get(), MemberInfo::from_member(member, user));
            }
        }
        for voice in create.voice_states.iter().flatten() {
            if let Some(channel) = voice.channel_id {
                state.voice.insert(voice.user_id.get(), channel.get());
            }
        }
        self.guilds
            .write()
            .expect("world poisoned")
            .insert(state.id, state);
    }

    pub fn apply_guild_update(&self, guild: &Guild) {
        self.with_guild_mut(guild.id.get(), |state| state.update_guild(guild));
    }

    pub fn remove_guild(&self, guild_id: u64) {
        self.guilds
            .write()
            .expect("world poisoned")
            .remove(&guild_id);
    }

    pub fn upsert_channel(&self, channel: &Channel) {
        let Some(guild_id) = channel.guild_id.map(|g| g.get()) else {
            return;
        };
        self.with_guild_mut(guild_id, |state| {
            state.channels.insert(channel.id.get(), channel.clone());
        });
    }

    pub fn remove_channel(&self, channel: &Channel) {
        let guild_id = channel
            .guild_id
            .map(|g| g.get())
            .or_else(|| self.guild_of_channel(channel.id.get()));
        if let Some(guild_id) = guild_id {
            self.with_guild_mut(guild_id, |state| {
                state.channels.remove(&channel.id.get());
            });
        }
    }

    pub fn upsert_role(&self, guild_id: u64, role: &Role) {
        self.with_guild_mut(guild_id, |state| {
            state.roles.insert(role.id.get(), role.clone());
        });
    }

    pub fn remove_role(&self, guild_id: u64, role_id: u64) {
        self.with_guild_mut(guild_id, |state| {
            state.roles.remove(&role_id);
        });
    }

    pub fn upsert_member(&self, guild_id: u64, member: &Member) {
        let Some(user) = &member.user else {
            return;
        };
        let info = MemberInfo::from_member(member, user);
        self.with_guild_mut(guild_id, |state| {
            state.members.insert(info.user_id, info);
        });
    }

    /// Message events carry the author's member data without the user.
    pub fn upsert_author(&self, guild_id: u64, author: &User, member: Option<&Member>) {
        self.with_guild_mut(guild_id, |state| match member {
            Some(member) => {
                state
                    .members
                    .insert(author.id.get(), MemberInfo::from_member(member, author));
            }
            None => {
                state
                    .members
                    .entry(author.id.get())
                    .or_insert_with(|| MemberInfo {
                        user_id: author.id.get(),
                        username: author.username.clone(),
                        global_name: author.global_name.clone(),
                        nick: None,
                        roles: Vec::new(),
                        bot: author.bot.unwrap_or(false),
                        joined_at: None,
                        timeout_until: None,
                        avatar: author.avatar.clone(),
                        guild_avatar: None,
                    });
            }
        });
    }

    pub fn remove_member(&self, guild_id: u64, user_id: u64) {
        self.with_guild_mut(guild_id, |state| {
            state.members.remove(&user_id);
            state.voice.remove(&user_id);
        });
    }

    pub fn apply_voice(&self, voice: &VoiceState) {
        let Some(guild_id) = voice.guild_id.map(|g| g.get()) else {
            return;
        };
        if let Some(member) = &voice.member {
            self.upsert_member(guild_id, member);
        }
        self.with_guild_mut(guild_id, |state| match voice.channel_id {
            Some(channel) => {
                state.voice.insert(voice.user_id.get(), channel.get());
            }
            None => {
                state.voice.remove(&voice.user_id.get());
            }
        });
    }

    pub fn channel_kind_label(kind: ChannelType) -> &'static str {
        match kind {
            ChannelType::GuildText => "text",
            ChannelType::Dm => "DM",
            ChannelType::GuildVoice => "voice",
            ChannelType::GroupDm => "group DM",
            ChannelType::GuildCategory => "category",
            ChannelType::GuildAnnouncement => "announcement",
            ChannelType::AnnouncementThread => "announcement thread",
            ChannelType::PublicThread => "thread",
            ChannelType::PrivateThread => "private thread",
            ChannelType::GuildStageVoice => "stage",
            ChannelType::GuildDirectory => "directory",
            ChannelType::GuildForum => "forum",
            ChannelType::GuildMedia => "media",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_channels_by_loose_and_spoken_names() {
        let guild: Guild = serde_json::from_value(serde_json::json!({
            "id": "1", "name": "g", "owner_id": "2", "roles": [], "emojis": [], "premium_progress_bar_enabled": false, "nsfw_level": 0, "preferred_locale": "en-US", "premium_tier": 0, "mfa_level": 0, "features": [], "explicit_content_filter": 0, "default_message_notifications": 0, "verification_level": 0, "afk_timeout": 60
        }))
        .unwrap();
        let mut state = GuildState::from_guild(&guild);
        for (id, name) in [
            (10, "general"),
            (11, "scratchpad3"),
            (12, "🪶sue-updates"),
            (13, "c-md"),
        ] {
            let channel: Channel = serde_json::from_value(serde_json::json!({
                "id": id.to_string(), "type": 0, "name": name
            }))
            .unwrap();
            state.channels.insert(id, channel);
        }
        let find = |q: &str| state.find_channel(q).map(|c| c.id.get());
        assert_eq!(find("general"), Some(10));
        assert_eq!(find("#scratchpad3"), Some(11));
        assert_eq!(find("scratch pad three"), Some(11));
        assert_eq!(find("the scratchpad 3 channel"), Some(11));
        assert_eq!(find("sue updates"), Some(12));
        assert_eq!(find("C-MD chat"), Some(13));
        assert_eq!(find("genral"), Some(10));
        assert_eq!(find("nothing like it"), None);
    }

    #[test]
    fn snowflakes_parse_from_mentions() {
        assert_eq!(
            parse_snowflake("<@80351110224678912>"),
            Some(80_351_110_224_678_912)
        );
        assert_eq!(
            parse_snowflake("<@!80351110224678912>"),
            Some(80_351_110_224_678_912)
        );
        assert_eq!(
            parse_snowflake("<#80351110224678912>"),
            Some(80_351_110_224_678_912)
        );
        assert_eq!(
            parse_snowflake("<@&80351110224678912>"),
            Some(80_351_110_224_678_912)
        );
        assert_eq!(
            parse_snowflake("80351110224678912"),
            Some(80_351_110_224_678_912)
        );
        assert_eq!(parse_snowflake("general"), None);
        assert_eq!(parse_snowflake("12"), None);
    }
}
