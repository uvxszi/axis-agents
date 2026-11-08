//! The agents' private coordination channel ("HQ").
//!
//! On joining a server the agents look for it by name. If it's missing,
//! one agent (elected by lowest user ID) asks the server's admins whether
//! it may create a private category + channel — or, if it lacks Manage
//! Channels, asks them to create it. Only an admin (or someone with
//! Manage Channels) can answer; a "no" is respected for a week.

use std::sync::Arc;

use axis::model::{AllowedMentions, ChannelType, Message, OverwriteType, PermissionOverwrite};
use axis::prelude::{ChannelId, GenericId, GuildId, Permissions};

use super::agent::{AgentCore, mentions_word};
use super::authority;
use super::hive::ClaimKind;
use super::live::FeedKind;
use crate::config::AccessLevel;
use crate::memory::{HqRequest, now_ms};

const WEEK_MS: i64 = 7 * 24 * 3600 * 1000;

fn agent_permissions() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::ADD_REACTIONS
        | Permissions::EMBED_LINKS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::SEND_MESSAGES_IN_THREADS
}

fn my_permissions(core: &AgentCore, guild_id: u64) -> Permissions {
    let me = core.world.my_id();
    core.world
        .with_guild(guild_id, |g| {
            let roles = g
                .members
                .get(&me)
                .map(|m| m.roles.clone())
                .unwrap_or_default();
            authority::base_permissions(g, me, &roles)
        })
        .unwrap_or_else(Permissions::empty)
}

fn is_leader(core: &AgentCore, guild_id: u64) -> bool {
    core.hive
        .peers_in_guild(guild_id)
        .first()
        .is_some_and(|leader| leader.id == core.id)
}

/// Find or negotiate the HQ channel for a guild.
pub async fn ensure(core: &Arc<AgentCore>, guild_id: u64) {
    if !core.profile().behavior.use_hq {
        return;
    }
    let settings = core.hive.settings();
    let wanted = settings
        .hq_channel
        .trim()
        .trim_start_matches('#')
        .to_lowercase();
    if wanted.is_empty() {
        return;
    }
    let found = core
        .world
        .with_guild(guild_id, |g| {
            g.channels
                .values()
                .find(|c| {
                    c.r#type == ChannelType::GuildText
                        && c.name
                            .as_deref()
                            .is_some_and(|n| n.to_lowercase() == wanted)
                })
                .map(|c| (c.id.get(), c.parent_id.map(|p| p.get())))
        })
        .flatten();
    if let Some((channel_id, category)) = found {
        core.hive.knowledge.guild_mut(guild_id, |k| {
            k.hq.channel_id = Some(channel_id);
            k.hq.category_id = category;
            k.hq.request = None;
        });
        if is_leader(core, guild_id) {
            grant_peers(core, guild_id, channel_id, category).await;
        }
        return;
    }
    if !is_leader(core, guild_id) || !core.hive.claim(guild_id, ClaimKind::HqSetup, &core.id) {
        return;
    }
    let (pending, declined_until, approved) = core.hive.knowledge.guild(guild_id, |k| {
        (k.hq.request.clone(), k.hq.declined_until, k.hq.approved)
    });
    let can_create = my_permissions(core, guild_id)
        .contains(Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES);
    if approved && can_create {
        create(core, guild_id).await;
        return;
    }
    if now_ms() < declined_until {
        return;
    }
    if pending.is_some_and(|p| now_ms() - p.asked_at < 3 * 24 * 3600 * 1000) {
        return;
    }
    ask(core, guild_id, can_create, &settings.hq_category, &wanted).await;
}

/// The server's leaders: the owner, and roles that can run the server
/// (Administrator or Manage Server) — not ordinary moderator roles.
pub fn leaders(core: &AgentCore, guild_id: u64) -> (u64, Vec<u64>, Vec<u64>) {
    core.world
        .with_guild(guild_id, |g| {
            let roles: Vec<u64> = g
                .roles
                .values()
                .filter(|r| r.id.get() != guild_id && !r.managed)
                .filter(|r| {
                    r.permissions
                        .intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_GUILD)
                        || ["owner", "founder", "co-owner", "admin", "head"]
                            .iter()
                            .any(|k| r.name.to_lowercase().contains(k))
                            && !r.name.to_lowercase().contains("mod")
                })
                .map(|r| r.id.get())
                .collect();
            let people: Vec<u64> = g
                .members
                .values()
                .filter(|m| !m.bot && m.roles.iter().any(|r| roles.contains(r)))
                .map(|m| m.user_id)
                .take(5)
                .collect();
            (g.owner_id, roles, people)
        })
        .unwrap_or_default()
}

/// Tell the server's leaders something: in a channel the bot can write
/// in (tagging the owner and top staff roles), or by DM if it can't post
/// anywhere. Returns the channel and message when posted in the server.
pub async fn notify_leaders(
    core: &Arc<AgentCore>,
    guild_id: u64,
    text: &str,
) -> Option<(u64, u64)> {
    let (owner, roles, people) = leaders(core, guild_id);
    let mut tags: Vec<String> = Vec::new();
    if owner != 0 {
        tags.push(format!("<@{owner}>"));
    }
    tags.extend(roles.iter().take(3).map(|r| format!("<@&{r}>")));
    if let Some(target) = ask_channel(core, guild_id) {
        let posted = core
            .http
            .create_message(ChannelId::new(target))
            .content(format!("{} {text}", tags.join(" ")))
            .allowed_mentions(AllowedMentions {
                users: if owner != 0 {
                    vec![axis::prelude::UserId::new(owner)]
                } else {
                    Vec::new()
                },
                roles: roles
                    .iter()
                    .take(3)
                    .map(|r| axis::prelude::RoleId::new(*r))
                    .collect(),
                ..AllowedMentions::none()
            })
            .await;
        if let Ok(message) = posted {
            return Some((target, message.id.get()));
        }
    }
    // Nowhere to post: DM the owner and the people holding top roles.
    let server = core
        .world
        .with_guild(guild_id, |g| g.name.clone())
        .unwrap_or_default();
    let mut recipients = vec![owner];
    recipients.extend(people);
    recipients.retain(|u| *u != 0);
    recipients.dedup();
    for user in recipients.into_iter().take(4) {
        if let Ok(dm) = core.http.create_dm(axis::prelude::UserId::new(user)).await {
            let _ = core
                .http
                .create_message(dm.id)
                .content(format!("**{server}:** {text}"))
                .allowed_mentions(AllowedMentions::none())
                .await;
        }
    }
    None
}

/// Pick where to ask: a staff-ish channel the bot can post in, else the
/// system channel, else the first writable text channel.
fn ask_channel(core: &AgentCore, guild_id: u64) -> Option<u64> {
    let me = core.world.my_id();
    core.world
        .with_guild(guild_id, |g| {
            let roles = g
                .members
                .get(&me)
                .map(|m| m.roles.clone())
                .unwrap_or_default();
            let writable = |id: u64| {
                authority::channel_permissions(g, me, &roles, id)
                    .contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
            };
            let mut text: Vec<_> = g
                .sorted_channels()
                .into_iter()
                .filter(|c| c.r#type == ChannelType::GuildText && writable(c.id.get()))
                .collect();
            text.sort_by_key(|c| c.position.unwrap_or(0));
            let staff = text.iter().find(|c| {
                c.name.as_deref().is_some_and(|n| {
                    ["staff", "mod", "admin", "bot-commands", "bots"]
                        .iter()
                        .any(|k| n.contains(k))
                })
            });
            staff
                .map(|c| c.id.get())
                .or(g.system_channel.filter(|id| writable(*id)))
                .or(text.first().map(|c| c.id.get()))
        })
        .flatten()
}

async fn ask(
    core: &Arc<AgentCore>,
    guild_id: u64,
    can_create: bool,
    category: &str,
    channel: &str,
) {
    let team: Vec<String> = core
        .hive
        .peers()
        .iter()
        .map(|p| {
            if p.user_id != 0 {
                format!("<@{}>", p.user_id)
            } else {
                p.name.clone()
            }
        })
        .collect();
    let request = if can_create {
        "May I create it? An admin can reply **yes** or **no** to this message.".to_string()
    } else {
        "I don't have the **Manage Channels** + **Manage Roles** permissions, so could an admin create it (private, with us agents allowed in)? Or give me those permissions and reply **yes** — I'll set it up.".to_string()
    };
    let text = format!(
        "Hi! I'm {}, one of the AI agents here ({}). We'd like a private **{category}** category with a **#{channel}** channel — visible only to us agents and admins — so we can coordinate and keep a log of everything we do.\n{request}",
        core.name(),
        team.join(", ")
    );
    // Asked where it tags the owner and top staff (or by DM to them).
    let posted = notify_leaders(core, guild_id, &text).await;
    core.hive.knowledge.guild_mut(guild_id, |k| {
        k.hq.request = Some(HqRequest {
            channel_id: posted.map(|p| p.0).unwrap_or(0),
            message_id: posted.map(|p| p.1).unwrap_or(0),
            agent_id: core.id.clone(),
            asked_at: now_ms(),
        });
    });
    core.feed(
        FeedKind::Coordination,
        "asked the server's leaders for a private HQ channel",
        None,
    );
}

const YES: &[&str] = &[
    "yes",
    "yeah",
    "yep",
    "yup",
    "sure",
    "ok",
    "okay",
    "go ahead",
    "do it",
    "approved",
    "fine",
    "go for it",
    "y",
];
const NO: &[&str] = &[
    "no", "nope", "nah", "don't", "dont", "not now", "denied", "n",
];

/// Watch for an admin's answer to the HQ request.
pub fn on_message(core: &Arc<AgentCore>, guild_id: u64, message: &Message) {
    let Some(request) = core
        .hive
        .knowledge
        .guild(guild_id, |k| k.hq.request.clone())
    else {
        return;
    };
    if request.agent_id != core.id || message.channel_id.get() != request.channel_id {
        return;
    }
    let replying = message
        .message_reference
        .and_then(|r| r.message_id)
        .is_some_and(|id| id.get() == request.message_id);
    let mentions_me = message
        .mentions
        .iter()
        .any(|u| u.id.get() == core.world.my_id());
    let recent = now_ms() - request.asked_at < 30 * 60 * 1000;
    if !(replying || mentions_me || recent) {
        return;
    }
    let principal = core.principal_for(Some(guild_id), &message.author);
    let authorized = principal.level >= AccessLevel::Admin
        || principal.permissions.contains(Permissions::MANAGE_CHANNELS);
    if !authorized {
        return;
    }
    let text = message.content.to_lowercase();
    let yes = YES.iter().any(|w| mentions_word(&text, w));
    let no = NO.iter().any(|w| mentions_word(&text, w));
    if yes == no {
        return;
    }
    let core = Arc::clone(core);
    let channel = message.channel_id;
    let reply_to = message.id;
    tokio::spawn(async move {
        if yes {
            core.hive.knowledge.guild_mut(guild_id, |k| {
                k.hq.approved = true;
                k.hq.request = None;
            });
            let can_create = my_permissions(&core, guild_id)
                .contains(Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES);
            if can_create {
                let _ = core
                    .http
                    .create_message(channel)
                    .content("Thanks! Setting it up now.")
                    .reply(reply_to)
                    .allowed_mentions(AllowedMentions::none())
                    .await;
                create(&core, guild_id).await;
            } else {
                let _ = core
                    .http
                    .create_message(channel)
                    .content("Thanks! I still don't have Manage Channels + Manage Roles, though — once I do (or once someone creates the channel), I'll pick it up automatically.")
                    .reply(reply_to)
                    .allowed_mentions(AllowedMentions::none())
                    .await;
            }
        } else {
            core.hive.knowledge.guild_mut(guild_id, |k| {
                k.hq.declined_until = now_ms() + WEEK_MS;
                k.hq.request = None;
            });
            let _ = core
                .http
                .create_message(channel)
                .content("No problem — we'll coordinate without it. I won't ask again for a while.")
                .reply(reply_to)
                .allowed_mentions(AllowedMentions::none())
                .await;
            core.feed(
                FeedKind::Coordination,
                "admins declined the HQ channel",
                None,
            );
        }
    });
}

/// Create the private category and channel, with every agent allowed in.
pub async fn create(core: &Arc<AgentCore>, guild_id: u64) {
    let settings = core.hive.settings();
    let mut overwrites = vec![PermissionOverwrite {
        id: GenericId::new(guild_id),
        r#type: OverwriteType::Role,
        allow: Permissions::empty(),
        deny: Permissions::VIEW_CHANNEL,
    }];
    for peer in core.hive.peers() {
        if peer.user_id != 0 && peer.guilds.contains(&guild_id) {
            overwrites.push(PermissionOverwrite {
                id: GenericId::new(peer.user_id),
                r#type: OverwriteType::Member,
                allow: agent_permissions(),
                deny: Permissions::empty(),
            });
        }
    }
    let reason = "Private coordination space for the server's AI agents (approved by an admin)";
    let category = match core
        .http
        .create_guild_channel(GuildId::new(guild_id), settings.hq_category.clone())
        .kind(ChannelType::GuildCategory)
        .permission_overwrites(overwrites.clone())
        .reason(reason)
        .await
    {
        Ok(category) => category,
        Err(error) => {
            core.feed(
                FeedKind::Error,
                format!(
                    "couldn't create the HQ category: {}",
                    crate::tools::discord_error(error)
                ),
                None,
            );
            return;
        }
    };
    core.world.upsert_channel(&category);
    let channel = match core
        .http
        .create_guild_channel(GuildId::new(guild_id), settings.hq_channel.trim_start_matches('#').to_owned())
        .kind(ChannelType::GuildText)
        .parent_id(category.id)
        .topic("Private coordination between the AI agents. Admins can read and give instructions here.")
        .permission_overwrites(overwrites)
        .reason(reason)
        .await
    {
        Ok(channel) => channel,
        Err(error) => {
            core.feed(FeedKind::Error, format!("couldn't create the HQ channel: {}", crate::tools::discord_error(error)), None);
            return;
        }
    };
    core.world.upsert_channel(&channel);
    core.hive.knowledge.guild_mut(guild_id, |k| {
        k.hq.channel_id = Some(channel.id.get());
        k.hq.category_id = Some(category.id.get());
    });
    let roster: Vec<String> = core
        .hive
        .peers()
        .iter()
        .filter(|p| p.guilds.contains(&guild_id))
        .map(|p| format!("• <@{}> — {}", p.user_id, p.specialty))
        .collect();
    let _ = core
        .http
        .create_message(channel.id)
        .content(format!(
            "**Agent HQ is open.** Team:\n{}\nUse this channel to hand off requests (mention the agent), share moderation context, and plan. Admins can give us instructions here too.",
            roster.join("\n")
        ))
        .allowed_mentions(AllowedMentions::none())
        .await;
    core.feed(FeedKind::Coordination, "created the private agent HQ", None);
}

/// Make sure every agent in the guild can see an existing HQ.
async fn grant_peers(core: &Arc<AgentCore>, guild_id: u64, channel_id: u64, category: Option<u64>) {
    if !my_permissions(core, guild_id).contains(Permissions::MANAGE_ROLES) {
        return;
    }
    for peer in core.hive.peers() {
        if peer.user_id == 0 || !peer.guilds.contains(&guild_id) {
            continue;
        }
        let visible = core
            .world
            .with_guild(guild_id, |g| {
                let roles = g
                    .members
                    .get(&peer.user_id)
                    .map(|m| m.roles.clone())
                    .unwrap_or_default();
                authority::channel_permissions(g, peer.user_id, &roles, channel_id)
                    .contains(Permissions::VIEW_CHANNEL)
            })
            .unwrap_or(false);
        if visible {
            continue;
        }
        for target in std::iter::once(channel_id).chain(category) {
            let _ = core
                .http
                .edit_channel_permission(
                    ChannelId::new(target),
                    GenericId::new(peer.user_id),
                    OverwriteType::Member,
                )
                .allow(agent_permissions())
                .await;
        }
        core.feed(
            FeedKind::Coordination,
            format!("gave {} access to HQ", peer.name),
            None,
        );
    }
}
