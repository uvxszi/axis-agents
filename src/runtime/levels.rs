//! Leveling (XP for messages and voice time, level-up messages, role
//! rewards, rank cards) and server statistics (members, joins, messages,
//! activity, voice minutes), plus who may change these settings.
//!
//! Knowledge is shared by every agent in the hive, so counting happens
//! once per event: messages are counted by whichever agent archives them
//! first, and the per-minute voice/member sampling is claimed once.

use std::sync::Arc;

use axis::model::Message;
use axis::prelude::*;

use super::agent::AgentCore;
use super::authority::{Principal, PrincipalKind};
use super::hive::ClaimKind;
use crate::config::AccessLevel;
use crate::memory::{DayStats, Leveling, now_ms};

const KEEP_DAYS: usize = 120;

pub fn today() -> String {
    day_of(now_ms())
}

pub fn day_of(ms: i64) -> String {
    Timestamp::from_unix_millis(ms)
        .map(|t| t.to_string().chars().take(10).collect())
        .unwrap_or_default()
}

fn hour_of(ms: i64) -> usize {
    ((ms / 3_600_000) % 24) as usize
}

/// May this person change the agents' server settings? The owner always;
/// then whoever the owner allowed — or, when the owner hasn't restricted
/// it, admins.
pub fn can_manage(core: &AgentCore, guild_id: u64, who: &Principal) -> bool {
    if who.kind == PrincipalKind::Operator || who.level >= AccessLevel::Owner {
        return true;
    }
    let access = core
        .hive
        .knowledge
        .guild(guild_id, |k| k.settings_access.clone());
    if access.is_restricted() {
        access.users.contains(&who.user_id) || who.roles.iter().any(|r| access.roles.contains(r))
    } else {
        who.level >= AccessLevel::Admin
    }
}

fn with_day(stats: &mut crate::memory::ServerStats, edit: impl FnOnce(&mut DayStats)) {
    let day = stats.days.entry(today()).or_default();
    if day.hours.len() != 24 {
        day.hours = vec![0; 24];
    }
    edit(day);
    while stats.days.len() > KEEP_DAYS {
        let oldest = stats.days.keys().next().cloned();
        if let Some(oldest) = oldest {
            stats.days.remove(&oldest);
        }
    }
}

/// A new message from a person (called once per message).
pub fn on_message(core: &Arc<AgentCore>, guild_id: u64, message: &Message) {
    if message.author.bot.unwrap_or(false) {
        return;
    }
    let user_id = message.author.id.get();
    let channel_id = message.channel_id.get();
    let now = now_ms();
    let name = message
        .member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .or_else(|| message.author.global_name.clone())
        .unwrap_or_else(|| message.author.username.clone());
    let level_up = core.hive.knowledge.guild_mut(guild_id, |k| {
        if k.stats.collecting() && k.stats.track_messages {
            with_day(&mut k.stats, |d| {
                d.messages += 1;
                *d.channels.entry(channel_id).or_default() += 1;
                d.hours[hour_of(now)] += 1;
                if !d.active.contains(&user_id) {
                    d.active.push(user_id);
                }
            });
        }
        let l = &mut k.leveling;
        if !l.enabled || l.paused || l.ignored_channels.contains(&channel_id) {
            return None;
        }
        let entry = l.members.entry(user_id).or_default();
        entry.messages += 1;
        entry.name = name.clone();
        if now - entry.last_xp < l.cooldown_secs as i64 * 1000 {
            return None;
        }
        entry.last_xp = now;
        let before = Leveling::progress(entry.xp).0;
        let (low, high) = (l.xp_min.min(l.xp_max), l.xp_max.max(l.xp_min));
        entry.xp += fastrand::u32(low..=high) as u64;
        let after = Leveling::progress(entry.xp).0;
        (after > before).then_some(after)
    });
    if let Some(level) = level_up {
        let core = Arc::clone(core);
        tokio::spawn(
            async move { leveled_up(&core, guild_id, user_id, level, Some(channel_id)).await },
        );
    }
}

/// Role rewards and the level-up message.
async fn leveled_up(
    core: &AgentCore,
    guild_id: u64,
    user_id: u64,
    level: u32,
    channel: Option<u64>,
) {
    let l = core.hive.knowledge.guild(guild_id, |k| k.leveling.clone());
    let has: Vec<u64> = core
        .world
        .with_guild(guild_id, |g| {
            g.members.get(&user_id).map(|m| m.roles.clone())
        })
        .flatten()
        .unwrap_or_default();
    let earned: Vec<_> = l.rewards.iter().filter(|r| r.level <= level).collect();
    let top = earned.iter().max_by_key(|r| r.level).map(|r| r.role_id);
    let mut new_roles = Vec::new();
    for reward in &earned {
        let keep = l.stack_rewards || Some(reward.role_id) == top;
        let path = format!(
            "/guilds/{guild_id}/members/{user_id}/roles/{}",
            reward.role_id
        );
        if keep && !has.contains(&reward.role_id) {
            if core
                .http
                .raw("PUT", &path, None, Some(format!("level {level} reward")))
                .await
                .is_ok()
            {
                new_roles.push(reward.role_id);
            }
        } else if !keep && has.contains(&reward.role_id) {
            let _ = core
                .http
                .raw(
                    "DELETE",
                    &path,
                    None,
                    Some("replaced by a higher level reward".to_owned()),
                )
                .await;
        }
    }
    let name = l
        .members
        .get(&user_id)
        .map(|e| e.name.clone())
        .unwrap_or_default();
    let mut text = l
        .message_for(user_id)
        .replace("{user}", &format!("<@{user_id}>"))
        .replace("{name}", &name)
        .replace("{level}", &level.to_string());
    if !new_roles.is_empty() {
        text.push_str(&format!(
            " You unlocked {}.",
            new_roles
                .iter()
                .map(|r| format!("<@&{r}>"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // The server's announce setting, unless this person chose their own.
    let announce = l.announce_for(user_id);
    let target = match announce {
        "off" => return,
        "dm" => match core.http.create_dm(UserId::new(user_id)).await {
            Ok(dm) => Some(dm.id.get()),
            Err(_) => None,
        },
        "channel" if l.announce_channel != 0 => Some(l.announce_channel),
        _ => channel.or((l.announce_channel != 0).then_some(l.announce_channel)),
    };
    // Not in a channel it was told never to chat in.
    let target = target.filter(|t| announce == "dm" || core.barred(guild_id, *t).is_none());
    if let Some(target) = target {
        let _ = core
            .http
            .create_message(ChannelId::new(target))
            .content(text)
            .allowed_mentions(axis::model::AllowedMentions::users())
            .await;
    }
}

/// Every minute: voice time (XP and stats) and the member count. Claimed
/// once per guild per minute across all agents.
pub async fn tick(core: &AgentCore) {
    let minute = (now_ms() / 60_000) as u64;
    for guild_id in core.world.guild_ids() {
        let key = guild_id ^ minute.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if !core.hive.claim_once(key, ClaimKind::Stats) {
            continue;
        }
        // People in voice with at least one other person.
        let in_voice: Vec<u64> = core
            .world
            .with_guild(guild_id, |g| {
                let person = |u: &u64| g.members.get(u).is_none_or(|m| !m.bot);
                let mut by_channel: std::collections::HashMap<u64, Vec<u64>> = Default::default();
                for (user, channel) in &g.voice {
                    if person(user) {
                        by_channel.entry(*channel).or_default().push(*user);
                    }
                }
                by_channel
                    .into_values()
                    .filter(|v| v.len() >= 2)
                    .flatten()
                    .collect()
            })
            .unwrap_or_default();
        let members = core
            .world
            .with_guild(guild_id, |g| g.member_count.max(g.members.len() as u64))
            .unwrap_or(0);
        let names: Vec<(u64, String)> = core
            .world
            .with_guild(guild_id, |g| {
                in_voice
                    .iter()
                    .filter_map(|u| g.members.get(u).map(|m| (*u, m.display_name().to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let level_ups = core.hive.knowledge.guild_mut(guild_id, |k| {
            if k.stats.collecting() {
                let track_voice = k.stats.track_voice;
                let track_members = k.stats.track_members;
                with_day(&mut k.stats, |d| {
                    if track_members && members > 0 {
                        d.members = members;
                    }
                    if track_voice {
                        d.voice_minutes += in_voice.len() as u32;
                        for u in &in_voice {
                            if !d.active.contains(u) {
                                d.active.push(*u);
                            }
                        }
                    }
                });
            }
            let l = &mut k.leveling;
            let mut ups = Vec::new();
            if l.enabled && !l.paused && l.voice_xp_per_minute > 0 {
                for (user, name) in &names {
                    let entry = l.members.entry(*user).or_default();
                    entry.voice_minutes += 1;
                    entry.name = name.clone();
                    let before = Leveling::progress(entry.xp).0;
                    entry.xp += l.voice_xp_per_minute as u64;
                    let after = Leveling::progress(entry.xp).0;
                    if after > before {
                        ups.push((*user, after));
                    }
                }
            }
            ups
        });
        for (user, level) in level_ups {
            leveled_up(core, guild_id, user, level, None).await;
        }
    }
}

pub fn on_join(core: &AgentCore, guild_id: u64, user_id: u64, joined: bool) {
    let key = guild_id ^ user_id.rotate_left(17) ^ (now_ms() / 30_000) as u64 ^ u64::from(joined);
    if !core.hive.claim_once(key, ClaimKind::Stats) {
        return;
    }
    core.hive.knowledge.guild_mut(guild_id, |k| {
        if k.stats.collecting() && k.stats.track_joins {
            with_day(&mut k.stats, |d| {
                if joined {
                    d.joins += 1;
                } else {
                    d.leaves += 1;
                }
            });
        }
    });
}

/// A member's rank card and a one-line summary.
pub async fn rank_card(
    core: &AgentCore,
    guild_id: u64,
    user_id: u64,
) -> Result<(Vec<u8>, String), String> {
    let l = core.hive.knowledge.guild(guild_id, |k| k.leveling.clone());
    let entry = l.members.get(&user_id).cloned().unwrap_or_default();
    let (level, into, needed) = Leveling::progress(entry.xp);
    let rank = l.rank_of(user_id).unwrap_or(l.members.len() + 1);
    let (name, avatar) = core
        .world
        .with_guild(guild_id, |g| {
            g.members.get(&user_id).map(|m| {
                (
                    m.display_name().to_owned(),
                    crate::tools::images::avatar_url(
                        Some(guild_id),
                        user_id,
                        m.avatar.as_deref(),
                        m.guild_avatar.as_deref(),
                    ),
                )
            })
        })
        .flatten()
        .unwrap_or_else(|| {
            (
                if entry.name.is_empty() {
                    "member".to_owned()
                } else {
                    entry.name.clone()
                },
                String::new(),
            )
        });
    let png = crate::tools::rankcard::render(
        l.card_for(user_id),
        &crate::tools::rankcard::RankCardData {
            name: name.clone(),
            avatar_url: (!avatar.is_empty()).then_some(avatar),
            level,
            rank,
            xp_into: into,
            xp_needed: needed,
            total_xp: entry.xp,
        },
    )
    .await?;
    Ok((
        png,
        format!(
            "{name}: level {level}, rank #{rank}, {into}/{needed} XP to level {} ({} total, {} messages, {} voice minutes)",
            level + 1,
            entry.xp,
            entry.messages,
            entry.voice_minutes
        ),
    ))
}

pub fn leaderboard(core: &AgentCore, guild_id: u64, count: usize) -> String {
    let l = core.hive.knowledge.guild(guild_id, |k| k.leveling.clone());
    let mut entries: Vec<(&u64, &crate::memory::LevelEntry)> = l.members.iter().collect();
    entries.sort_by(|a, b| b.1.xp.cmp(&a.1.xp));
    if entries.is_empty() {
        return "Nobody has XP yet.".to_owned();
    }
    entries
        .iter()
        .take(count.clamp(1, 25))
        .enumerate()
        .map(|(i, (user, e))| {
            let medal = match i {
                0 => "🥇".to_owned(),
                1 => "🥈".to_owned(),
                2 => "🥉".to_owned(),
                n => format!("**{}.**", n + 1),
            };
            format!(
                "{medal} <@{user}> — level {} · {} XP",
                Leveling::progress(e.xp).0,
                e.xp
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Totals over the last `days` days, for answering questions.
pub fn stats_report(core: &AgentCore, guild_id: u64, days: usize) -> String {
    let stats = core.hive.knowledge.guild(guild_id, |k| k.stats.clone());
    let recent: Vec<(&String, &DayStats)> = stats
        .days
        .iter()
        .rev()
        .take(days.clamp(1, KEEP_DAYS))
        .collect();
    if recent.is_empty() {
        return "No statistics collected yet (they build up from now on).".to_owned();
    }
    let messages: u32 = recent.iter().map(|(_, d)| d.messages).sum();
    let joins: u32 = recent.iter().map(|(_, d)| d.joins).sum();
    let leaves: u32 = recent.iter().map(|(_, d)| d.leaves).sum();
    let voice: u32 = recent.iter().map(|(_, d)| d.voice_minutes).sum();
    let mut active: Vec<u64> = recent
        .iter()
        .flat_map(|(_, d)| d.active.iter().copied())
        .collect();
    active.sort_unstable();
    active.dedup();
    let mut channels: std::collections::BTreeMap<u64, u32> = Default::default();
    let mut hours = [0u32; 24];
    for (_, d) in &recent {
        for (c, n) in &d.channels {
            *channels.entry(*c).or_default() += n;
        }
        for (h, n) in d.hours.iter().enumerate().take(24) {
            hours[h] += n;
        }
    }
    let mut top: Vec<(u64, u32)> = channels.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let busiest = hours
        .iter()
        .enumerate()
        .max_by_key(|(_, n)| **n)
        .map(|(h, _)| h)
        .unwrap_or(0);
    let first = recent.last().map(|(_, d)| d.members).unwrap_or(0);
    let last = recent.first().map(|(_, d)| d.members).unwrap_or(0);
    let daily: Vec<String> = recent
        .iter()
        .take(14)
        .map(|(day, d)| {
            format!(
                "{day}: {} msgs, {} active, {} members, +{}/-{}, {} voice min",
                d.messages,
                d.active.len(),
                d.members,
                d.joins,
                d.leaves,
                d.voice_minutes
            )
        })
        .collect();
    format!(
        "Last {} day(s){}: {messages} messages from {} active people, {joins} joined / {leaves} left, members {first} → {last}, {voice} voice minutes. \
         Busiest hour (UTC): {busiest}:00. Top channels: {}.\nDaily (newest first):\n{}",
        recent.len(),
        if stats.paused {
            " (collection is PAUSED)"
        } else if !stats.enabled {
            " (collection is OFF)"
        } else {
            ""
        },
        active.len(),
        top.iter()
            .take(5)
            .map(|(c, n)| format!("<#{c}> {n}"))
            .collect::<Vec<_>>()
            .join(", "),
        daily.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_math_matches_the_usual_curve() {
        assert_eq!(Leveling::xp_for_next(0), 100);
        assert_eq!(Leveling::xp_for_next(1), 155);
        assert_eq!(Leveling::progress(0), (0, 0, 100));
        assert_eq!(Leveling::progress(100), (1, 0, 155));
        assert_eq!(Leveling::progress(300), (2, 45, 220));
    }

    #[test]
    fn days_are_dates() {
        assert_eq!(day_of(1_793_500_000_000).len(), 10);
        assert!(day_of(1_793_500_000_000).starts_with("2026-"));
    }
}
