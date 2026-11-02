//! Who may do what: Discord permission math, role ranks, and hierarchy.
//!
//! Everything here is enforced in code. The model is told about ranks so
//! it can explain itself, but no prompt can widen what a tool will do:
//! tools check the requester's real channel permissions and the role
//! hierarchy before acting.

use axis::model::OverwriteType;
use axis::prelude::Permissions;

use super::world::{GuildState, MemberInfo};
use crate::config::AccessLevel;

/// Rank of the server owner.
pub const OWNER_RANK: u32 = u32::MAX;
/// Admins rank above every role position.
const ADMIN_BASE: u32 = 1_000_000;

/// The permissions that mark someone as staff.
pub fn moderator_permissions() -> Permissions {
    Permissions::MANAGE_MESSAGES
        | Permissions::KICK_MEMBERS
        | Permissions::BAN_MEMBERS
        | Permissions::MODERATE_MEMBERS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalKind {
    /// A person on Discord.
    Human,
    /// Another agent from this app's team (or the agent itself acting on
    /// its own initiative).
    Agent,
    /// The app's operator, chatting from the playground.
    Operator,
}

/// Whoever a piece of work is being done for.
#[derive(Debug, Clone)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub user_id: u64,
    pub name: String,
    pub rank: u32,
    pub level: AccessLevel,
    /// Guild-level permissions (channel overwrites are applied per tool).
    pub permissions: Permissions,
    pub roles: Vec<u64>,
    pub top_role_name: Option<String>,
}

impl Principal {
    pub fn operator() -> Self {
        Self {
            kind: PrincipalKind::Operator,
            user_id: 0,
            name: "Operator".to_owned(),
            rank: OWNER_RANK,
            level: AccessLevel::Owner,
            permissions: Permissions::all(),
            roles: Vec::new(),
            top_role_name: Some("app operator".to_owned()),
        }
    }

    /// A person in a DM (no guild context): no guild permissions.
    pub fn dm_user(user_id: u64, name: String) -> Self {
        Self {
            kind: PrincipalKind::Human,
            user_id,
            name,
            rank: 0,
            level: AccessLevel::Everyone,
            permissions: Permissions::empty(),
            roles: Vec::new(),
            top_role_name: None,
        }
    }

    pub fn for_member(guild: &GuildState, member: &MemberInfo, kind: PrincipalKind) -> Self {
        let permissions = base_permissions(guild, member.user_id, &member.roles);
        Self {
            kind,
            user_id: member.user_id,
            name: member.display_name().to_owned(),
            rank: rank_of(guild, member.user_id, &member.roles, permissions),
            level: access_level(guild, member.user_id, &member.roles, permissions),
            permissions,
            roles: member.roles.clone(),
            top_role_name: guild.top_role(member).map(|role| role.name.clone()),
        }
    }

    /// A user who is not (or not yet) in the member cache.
    pub fn unknown_member(guild: &GuildState, user_id: u64, name: String) -> Self {
        let permissions = base_permissions(guild, user_id, &[]);
        Self {
            kind: PrincipalKind::Human,
            user_id,
            name,
            rank: rank_of(guild, user_id, &[], permissions),
            level: access_level(guild, user_id, &[], permissions),
            permissions,
            roles: Vec::new(),
            top_role_name: None,
        }
    }

    pub fn describe_rank(&self) -> String {
        match self.level {
            AccessLevel::Owner => "server owner".to_owned(),
            AccessLevel::Admin => "admin".to_owned(),
            AccessLevel::Moderator => format!(
                "moderator{}",
                self.top_role_name
                    .as_ref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ),
            AccessLevel::Trusted => self
                .top_role_name
                .clone()
                .unwrap_or_else(|| "member".to_owned()),
            AccessLevel::Everyone => "member".to_owned(),
        }
    }

    /// Channel-level permissions for this principal.
    pub fn channel_permissions(&self, guild: Option<&GuildState>, channel_id: u64) -> Permissions {
        if self.kind == PrincipalKind::Operator {
            return Permissions::all();
        }
        match guild {
            Some(guild) => channel_permissions(guild, self.user_id, &self.roles, channel_id),
            None => Permissions::empty(),
        }
    }
}

/// Guild-wide permissions: owner → everything; otherwise @everyone plus
/// every role, with Administrator implying all.
pub fn base_permissions(guild: &GuildState, user_id: u64, roles: &[u64]) -> Permissions {
    if user_id == guild.owner_id {
        return Permissions::all();
    }
    let mut permissions = guild
        .roles
        .get(&guild.id)
        .map(|everyone| everyone.permissions)
        .unwrap_or_else(Permissions::empty);
    for role_id in roles {
        if let Some(role) = guild.roles.get(role_id) {
            permissions |= role.permissions;
        }
    }
    if permissions.is_administrator() {
        Permissions::all()
    } else {
        permissions
    }
}

/// Permissions in a channel, applying overwrites in Discord's order:
/// @everyone, then all role overwrites together, then the member. Threads
/// inherit their parent's overwrites.
pub fn channel_permissions(
    guild: &GuildState,
    user_id: u64,
    roles: &[u64],
    channel_id: u64,
) -> Permissions {
    let base = base_permissions(guild, user_id, roles);
    if base.is_administrator() {
        return Permissions::all();
    }
    let Some(channel) = guild.permission_source(channel_id) else {
        return base;
    };
    let Some(overwrites) = &channel.permission_overwrites else {
        return base;
    };
    let mut permissions = base;
    if let Some(everyone) = overwrites
        .iter()
        .find(|o| o.r#type == OverwriteType::Role && o.id.get() == guild.id)
    {
        permissions = (permissions & !everyone.deny) | everyone.allow;
    }
    let mut allow = Permissions::empty();
    let mut deny = Permissions::empty();
    for overwrite in overwrites {
        if overwrite.r#type == OverwriteType::Role && roles.contains(&overwrite.id.get()) {
            allow |= overwrite.allow;
            deny |= overwrite.deny;
        }
    }
    permissions = (permissions & !deny) | allow;
    if let Some(member) = overwrites
        .iter()
        .find(|o| o.r#type == OverwriteType::Member && o.id.get() == user_id)
    {
        permissions = (permissions & !member.deny) | member.allow;
    }
    // Without View Channel, no other channel permission applies.
    if !permissions.contains(Permissions::VIEW_CHANNEL) {
        return Permissions::empty();
    }
    permissions
}

/// The highest role position the member holds (0 = no roles).
pub fn top_position(guild: &GuildState, roles: &[u64]) -> u32 {
    roles
        .iter()
        .filter_map(|id| guild.roles.get(id))
        .map(|role| role.position.max(0) as u32)
        .max()
        .unwrap_or(0)
}

/// A comparable rank: owner > admins (by position) > roles by position.
pub fn rank_of(guild: &GuildState, user_id: u64, roles: &[u64], permissions: Permissions) -> u32 {
    if user_id == guild.owner_id {
        return OWNER_RANK;
    }
    let position = top_position(guild, roles);
    if permissions.is_administrator() {
        ADMIN_BASE + position
    } else {
        position
    }
}

pub fn access_level(
    guild: &GuildState,
    user_id: u64,
    roles: &[u64],
    permissions: Permissions,
) -> AccessLevel {
    if user_id == guild.owner_id {
        AccessLevel::Owner
    } else if permissions.is_administrator() {
        AccessLevel::Admin
    } else if permissions.intersects(moderator_permissions()) {
        AccessLevel::Moderator
    } else if roles.iter().any(|id| guild.roles.contains_key(id)) {
        AccessLevel::Trusted
    } else {
        AccessLevel::Everyone
    }
}

/// Whether `actor` may act on `target` (moderate, change roles): the
/// owner can act on anyone but themselves; otherwise the actor's top role
/// must be strictly above the target's, and nobody acts on the owner.
pub fn outranks(
    guild: &GuildState,
    actor_id: u64,
    actor_roles: &[u64],
    target: &MemberInfo,
) -> bool {
    if target.user_id == guild.owner_id {
        return false;
    }
    if actor_id == guild.owner_id {
        return actor_id != target.user_id;
    }
    top_position(guild, actor_roles) > top_position(guild, &target.roles)
}

/// Whether a role sits below the actor's top role (required to grant,
/// remove, or edit it).
pub fn can_manage_role(
    guild: &GuildState,
    actor_id: u64,
    actor_roles: &[u64],
    role_id: u64,
) -> bool {
    if actor_id == guild.owner_id {
        return true;
    }
    let Some(role) = guild.roles.get(&role_id) else {
        return false;
    };
    !role.managed && (role.position.max(0) as u32) < top_position(guild, actor_roles)
}

pub fn missing(required: Permissions, have: Permissions) -> String {
    let missing: Vec<&str> = (required & !have).names().collect();
    if missing.is_empty() {
        required.names().collect::<Vec<_>>().join(", ")
    } else {
        missing.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axis::model::{Channel, PermissionOverwrite, Role, RoleColors};
    use axis::prelude::{GenericId, RoleId};
    use std::collections::HashMap;

    fn role(id: u64, position: i32, permissions: Permissions) -> Role {
        Role {
            id: RoleId::new(id),
            name: format!("r{id}"),
            color: 0,
            colors: RoleColors {
                primary_color: 0,
                secondary_color: None,
                tertiary_color: None,
            },
            hoist: false,
            icon: None,
            unicode_emoji: None,
            position,
            permissions,
            managed: false,
            mentionable: true,
            flags: None,
        }
    }

    fn guild() -> GuildState {
        let mut roles = HashMap::new();
        roles.insert(
            1,
            role(1, 0, Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES),
        );
        roles.insert(10, role(10, 5, Permissions::MANAGE_MESSAGES));
        roles.insert(20, role(20, 9, Permissions::ADMINISTRATOR));
        roles.insert(30, role(30, 2, Permissions::empty()));
        GuildState {
            id: 1,
            name: "g".to_owned(),
            owner_id: 99,
            icon: None,
            description: None,
            features: Vec::new(),
            system_channel: None,
            rules_channel: None,
            member_count: 0,
            roles,
            channels: HashMap::new(),
            members: HashMap::new(),
            voice: HashMap::new(),
            emojis: Vec::new(),
        }
    }

    fn member(id: u64, roles: &[u64]) -> MemberInfo {
        MemberInfo {
            user_id: id,
            username: format!("u{id}"),
            global_name: None,
            nick: None,
            roles: roles.to_vec(),
            bot: false,
            joined_at: None,
            timeout_until: None,
            avatar: None,
            guild_avatar: None,
        }
    }

    #[test]
    fn levels_and_ranks() {
        let g = guild();
        let modr = Principal::for_member(&g, &member(5, &[10]), PrincipalKind::Human);
        let admin = Principal::for_member(&g, &member(6, &[20]), PrincipalKind::Human);
        let plain = Principal::for_member(&g, &member(7, &[]), PrincipalKind::Human);
        let owner = Principal::for_member(&g, &member(99, &[]), PrincipalKind::Human);
        assert_eq!(modr.level, AccessLevel::Moderator);
        assert_eq!(admin.level, AccessLevel::Admin);
        assert_eq!(plain.level, AccessLevel::Everyone);
        assert_eq!(owner.level, AccessLevel::Owner);
        assert!(owner.rank > admin.rank && admin.rank > modr.rank && modr.rank > plain.rank);
    }

    #[test]
    fn hierarchy_blocks_equal_or_higher_targets() {
        let g = guild();
        assert!(outranks(&g, 5, &[10], &member(7, &[30])));
        assert!(!outranks(&g, 5, &[10], &member(8, &[10])));
        assert!(!outranks(&g, 5, &[10], &member(9, &[20])));
        assert!(!outranks(&g, 6, &[20], &member(99, &[])));
        assert!(outranks(&g, 99, &[], &member(6, &[20])));
        assert!(can_manage_role(&g, 5, &[10], 30));
        assert!(!can_manage_role(&g, 5, &[10], 20));
    }

    #[test]
    fn channel_overwrites_apply_in_order() {
        let mut g = guild();
        let channel: Channel = serde_json::from_value(serde_json::json!({
            "id": "500", "type": 0, "guild_id": "1"
        }))
        .unwrap();
        let mut channel = channel;
        channel.permission_overwrites = Some(vec![
            PermissionOverwrite {
                id: GenericId::new(1),
                r#type: OverwriteType::Role,
                allow: Permissions::empty(),
                deny: Permissions::SEND_MESSAGES,
            },
            PermissionOverwrite {
                id: GenericId::new(30),
                r#type: OverwriteType::Role,
                allow: Permissions::SEND_MESSAGES,
                deny: Permissions::empty(),
            },
        ]);
        g.channels.insert(500, channel);
        let plain = channel_permissions(&g, 7, &[], 500);
        assert!(!plain.contains(Permissions::SEND_MESSAGES));
        let trusted = channel_permissions(&g, 8, &[30], 500);
        assert!(trusted.contains(Permissions::SEND_MESSAGES));
        assert!(channel_permissions(&g, 6, &[20], 500).is_administrator());
    }
}
