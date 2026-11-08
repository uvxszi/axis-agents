//! Community features built on buttons and dropdowns: self-assign role
//! menus and giveaways.

use std::sync::Arc;

use axis::model::{Interaction, InteractionData};
use axis::prelude::*;
use serde_json::{Value, json};

use super::agent::AgentCore;
use super::live::FeedKind;
use crate::memory::{Giveaway, Vote, now_ms};

pub const ROLE_BUTTON: &str = "axis_roles:";
pub const ROLE_MENU: &str = "axis_roles_menu";
pub const GIVEAWAY_ENTER: &str = "axis_giveaway:enter";
pub const VOTE_BUTTON: &str = "axis_vote:";

fn api_error(error: AxisError) -> String {
    crate::tools::discord_error(error)
}

/// One role in a menu.
pub struct MenuRole {
    pub role_id: u64,
    pub label: String,
    pub emoji: String,
    pub description: String,
}

fn emoji_json(emoji: &str) -> Option<Value> {
    let emoji = emoji.trim();
    if emoji.is_empty() {
        return None;
    }
    if let Some(inner) = emoji.strip_prefix('<').and_then(|e| e.strip_suffix('>')) {
        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() == 3 {
            return Some(json!({ "name": parts[1], "id": parts[2], "animated": parts[0] == "a" }));
        }
    }
    Some(json!({ "name": emoji }))
}

/// The message body of a role menu: buttons (toggle one role each) or a
/// dropdown (pick any of them).
pub fn role_menu_body(
    title: &str,
    message: &str,
    roles: &[MenuRole],
    dropdown: bool,
    color: u32,
) -> Value {
    let components = if dropdown {
        let options: Vec<Value> = roles
            .iter()
            .take(25)
            .map(|r| {
                let mut o = json!({ "label": r.label, "value": r.role_id.to_string() });
                if !r.description.is_empty() {
                    o["description"] = json!(crate::llm::truncate(&r.description, 100));
                }
                if let Some(e) = emoji_json(&r.emoji) {
                    o["emoji"] = e;
                }
                o
            })
            .collect();
        json!([{ "type": 1, "components": [{
            "type": 3, "custom_id": ROLE_MENU, "placeholder": "Pick your roles",
            "min_values": 0, "max_values": options.len().max(1), "options": options
        }]}])
    } else {
        let rows: Vec<Value> = roles
            .chunks(5)
            .take(5)
            .map(|chunk| {
                let buttons: Vec<Value> = chunk
                    .iter()
                    .map(|r| {
                        let mut b = json!({ "type": 2, "style": 2, "label": r.label, "custom_id": format!("{ROLE_BUTTON}{}", r.role_id) });
                        if let Some(e) = emoji_json(&r.emoji) {
                            b["emoji"] = e;
                        }
                        b
                    })
                    .collect();
                json!({ "type": 1, "components": buttons })
            })
            .collect();
        Value::Array(rows)
    };
    json!({
        "embeds": [{ "title": title, "description": message, "color": color }],
        "components": components,
        "allowed_mentions": { "parse": [] },
    })
}

async fn set_role(
    core: &AgentCore,
    guild_id: u64,
    user_id: u64,
    role_id: u64,
    give: bool,
) -> Result<(), String> {
    core.http
        .raw(
            if give { "PUT" } else { "DELETE" },
            &format!("/guilds/{guild_id}/members/{user_id}/roles/{role_id}"),
            None,
            Some("role menu".to_owned()),
        )
        .await
        .map(|_| ())
        .map_err(api_error)
}

fn member_roles(interaction: &Interaction) -> Vec<u64> {
    interaction
        .member
        .as_ref()
        .map(|m| m.roles.iter().map(|r| r.get()).collect())
        .unwrap_or_default()
}

/// Role menus and giveaways. Returns false when the component isn't ours.
pub async fn on_component(core: &Arc<AgentCore>, interaction: &Interaction) -> bool {
    let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
        return false;
    };
    let id = data.custom_id.as_str();
    if !(id.starts_with(ROLE_BUTTON)
        || id == ROLE_MENU
        || id == GIVEAWAY_ENTER
        || id.starts_with(VOTE_BUTTON))
    {
        return false;
    }
    let (Some(guild_id), Some(user)) = (
        interaction.guild_id.map(|g| g.get()),
        interaction
            .member
            .as_ref()
            .and_then(|m| m.user.clone())
            .or_else(|| interaction.user.clone()),
    ) else {
        return true;
    };
    let user_id = user.id.get();
    let has = member_roles(interaction);
    let reply = if let Some(role) = id
        .strip_prefix(ROLE_BUTTON)
        .and_then(|r| r.parse::<u64>().ok())
    {
        let give = !has.contains(&role);
        set_role(core, guild_id, user_id, role, give)
            .await
            .map(|()| {
                format!(
                    "{} <@&{role}>",
                    if give { "✅ Gave you" } else { "➖ Removed" }
                )
            })
    } else if id == ROLE_MENU {
        // Every role this dropdown offers, from the message itself.
        let offered: Vec<u64> = interaction
            .message
            .as_ref()
            .map(|m| {
                m.components
                    .iter()
                    .flat_map(|row| {
                        row.get("components")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default()
                    })
                    .flat_map(|c| {
                        c.get("options")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default()
                    })
                    .filter_map(|o| {
                        o.get("value")
                            .and_then(Value::as_str)
                            .and_then(|v| v.parse().ok())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let chosen: Vec<u64> = data
            .values
            .clone()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.parse().ok())
            .collect();
        let mut changes = Vec::new();
        let mut error = None;
        for role in offered {
            let want = chosen.contains(&role);
            if want != has.contains(&role) {
                match set_role(core, guild_id, user_id, role, want).await {
                    Ok(()) => changes.push(format!("{}<@&{role}>", if want { "+" } else { "−" })),
                    Err(e) => error = Some(e),
                }
            }
        }
        match error {
            Some(e) => Err(e),
            None if changes.is_empty() => Ok("No changes.".to_owned()),
            None => Ok(format!("Updated your roles: {}", changes.join(" "))),
        }
    } else if id.starts_with(VOTE_BUTTON) {
        cast_vote(core, guild_id, interaction, user_id, id).await
    } else {
        enter_giveaway(core, guild_id, interaction, user_id, &has)
    };
    let data = axis::model::InteractionMessageCallbackData {
        content: Some(match reply {
            Ok(text) => text,
            Err(error) => format!("Couldn't do that: {error}"),
        }),
        flags: Some(axis::model::MessageFlags::EPHEMERAL),
        ..Default::default()
    };
    let _ = core
        .http
        .respond_to_interaction(interaction.id, interaction.token.clone(), data)
        .await;
    true
}

fn enter_giveaway(
    core: &AgentCore,
    guild_id: u64,
    interaction: &Interaction,
    user_id: u64,
    roles: &[u64],
) -> Result<String, String> {
    let message_id = interaction
        .message
        .as_ref()
        .map(|m| m.id.get())
        .unwrap_or(0);
    core.hive.knowledge.guild_mut(guild_id, |k| {
        let Some(g) = k.giveaways.iter_mut().find(|g| g.message_id == message_id) else {
            return Err("that giveaway isn't running anymore".to_owned());
        };
        if g.ended {
            return Err("this giveaway has ended".to_owned());
        }
        if g.required_role != 0 && !roles.contains(&g.required_role) {
            return Err(format!("you need <@&{}> to enter", g.required_role));
        }
        if let Some(i) = g.entrants.iter().position(|e| *e == user_id) {
            g.entrants.remove(i);
            Ok(format!("You left the giveaway for **{}**.", g.prize))
        } else {
            g.entrants.push(user_id);
            Ok(format!(
                "🎉 You're in for **{}**! ({} entries) Press again to leave.",
                g.prize,
                g.entrants.len()
            ))
        }
    })
}

fn giveaway_body(g: &Giveaway) -> Value {
    let description = if g.ended {
        if g.picked.is_empty() {
            format!("Ended — no valid entries.\nHosted by <@{}>", g.host_id)
        } else {
            format!(
                "Ended — winner{}: {}\nHosted by <@{}>",
                if g.picked.len() == 1 { "" } else { "s" },
                g.picked
                    .iter()
                    .map(|w| format!("<@{w}>"))
                    .collect::<Vec<_>>()
                    .join(", "),
                g.host_id
            )
        }
    } else {
        format!(
            "Ends <t:{}:R> · {} winner{}{}\nHosted by <@{}>\nPress the button to enter.",
            g.ends / 1000,
            g.winners,
            if g.winners == 1 { "" } else { "s" },
            if g.required_role != 0 {
                format!(" · requires <@&{}>", g.required_role)
            } else {
                String::new()
            },
            g.host_id
        )
    };
    json!({
        "embeds": [{ "title": format!("🎉 {}", g.prize), "description": description, "color": if g.ended { 0x99AAB5 } else { 0xF1C40F } }],
        "components": [{ "type": 1, "components": [{
            "type": 2, "style": 1, "label": if g.ended { "Ended" } else { "Enter" }, "emoji": { "name": "🎉" },
            "custom_id": GIVEAWAY_ENTER, "disabled": g.ended
        }]}],
        "allowed_mentions": { "parse": [] },
    })
}

pub async fn start_giveaway(
    core: &AgentCore,
    guild_id: u64,
    mut g: Giveaway,
) -> Result<Giveaway, String> {
    g.agent_id = core.id.clone();
    let posted = core
        .http
        .raw(
            "POST",
            &format!("/channels/{}/messages", g.channel_id),
            Some(giveaway_body(&g)),
            None,
        )
        .await
        .map_err(api_error)?;
    g.message_id = posted
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    core.hive
        .knowledge
        .guild_mut(guild_id, |k| k.giveaways.push(g.clone()));
    Ok(g)
}

fn draw(entrants: &[u64], winners: u32, exclude: &[u64]) -> Vec<u64> {
    let mut pool: Vec<u64> = entrants
        .iter()
        .copied()
        .filter(|e| !exclude.contains(e))
        .collect();
    let mut picked = Vec::new();
    while !pool.is_empty() && picked.len() < winners as usize {
        picked.push(pool.swap_remove(fastrand::usize(..pool.len())));
    }
    picked
}

/// End a giveaway now (or reroll an ended one); returns the winners.
pub async fn finish(
    core: &AgentCore,
    guild_id: u64,
    message_id: u64,
    reroll: bool,
) -> Result<Vec<u64>, String> {
    let g = core.hive.knowledge.guild_mut(guild_id, |k| {
        let g = k
            .giveaways
            .iter_mut()
            .find(|g| g.message_id == message_id)?;
        let previous = if reroll { g.picked.clone() } else { Vec::new() };
        g.picked = draw(&g.entrants, if reroll { 1 } else { g.winners }, &previous);
        if reroll {
            let mut all = previous;
            all.extend(g.picked.iter().copied());
            g.picked = all;
        }
        g.ended = true;
        Some(g.clone())
    });
    let g = g.ok_or("no giveaway on that message")?;
    let _ = core
        .http
        .raw(
            "PATCH",
            &format!("/channels/{}/messages/{}", g.channel_id, g.message_id),
            Some(giveaway_body(&g)),
            None,
        )
        .await;
    let new: Vec<u64> = if reroll {
        g.picked.last().copied().into_iter().collect()
    } else {
        g.picked.clone()
    };
    let text = if new.is_empty() {
        format!(
            "The giveaway for **{}** ended with no valid entries.",
            g.prize
        )
    } else {
        format!(
            "🎉 Congratulations {}! You won **{}**!",
            new.iter()
                .map(|w| format!("<@{w}>"))
                .collect::<Vec<_>>()
                .join(", "),
            g.prize
        )
    };
    let _ = core
        .http
        .raw(
            "POST",
            &format!("/channels/{}/messages", g.channel_id),
            Some(json!({
                "content": text,
                "message_reference": { "message_id": g.message_id.to_string(), "fail_if_not_exists": false },
                "allowed_mentions": { "users": new.iter().map(|w| w.to_string()).collect::<Vec<_>>() },
            })),
            None,
        )
        .await;
    core.feed(
        FeedKind::System,
        format!("giveaway for {} ended", g.prize),
        None,
    );
    Ok(new)
}

/// End giveaways that are due (called every minute).
pub async fn tick(core: &AgentCore) {
    let now = now_ms();
    for guild_id in core.world.guild_ids() {
        let due: Vec<u64> = core.hive.knowledge.guild(guild_id, |k| {
            k.giveaways
                .iter()
                .filter(|g| g.agent_id == core.id && !g.ended && g.ends <= now)
                .map(|g| g.message_id)
                .collect()
        });
        for message_id in due {
            let _ = finish(core, guild_id, message_id, false).await;
        }
        let due_votes: Vec<u64> = core.hive.knowledge.guild(guild_id, |k| {
            k.votes
                .iter()
                .filter(|v| v.agent_id == core.id && !v.ended && v.ends > 0 && v.ends <= now)
                .map(|v| v.message_id)
                .collect()
        });
        for message_id in due_votes {
            let _ = finish_vote(core, guild_id, message_id).await;
        }
    }
}

// --- Interactive button-based voting -----------------------------------------------

fn option_emoji(idx: usize) -> &'static str {
    const EMOJIS: [&str; 25] = [
        "1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣", "🔟", "🇦", "🇧", "🇨", "🇩", "🇪", "🇫",
        "🇬", "🇭", "🇮", "🇯", "🇰", "🇱", "🇲", "🇳", "🇴",
    ];
    EMOJIS.get(idx).copied().unwrap_or("🔹")
}

fn progress_bar(count: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return "░".repeat(width);
    }
    let filled = ((count as f64 / total as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn vote_body(v: &Vote) -> Value {
    let total = v.votes.len();
    let tallies = v.tally();

    let title = if v.ended {
        format!("📊 {} [Ended]", v.question)
    } else {
        format!("📊 {}", v.question)
    };

    let mut desc = String::new();
    if v.ended {
        desc.push_str(&format!("Vote concluded · Total votes: **{total}**\n\n"));
        if total == 0 {
            desc.push_str("*No votes were cast.*");
        } else {
            let max_votes = tallies.iter().copied().max().unwrap_or(0);
            desc.push_str("**Final Results:**\n");
            for (idx, opt) in v.options.iter().enumerate() {
                let count = tallies[idx];
                let pct = if total > 0 {
                    (count as f64 / total as f64 * 100.0).round() as u32
                } else {
                    0
                };
                let winner_badge = if count == max_votes && max_votes > 0 {
                    " 🏆"
                } else {
                    ""
                };
                let bar = progress_bar(count, total, 10);
                desc.push_str(&format!(
                    "{} **{}** — **{}** vote{} ({}%){}\n`{}`\n",
                    option_emoji(idx),
                    opt,
                    count,
                    if count == 1 { "" } else { "s" },
                    pct,
                    winner_badge,
                    bar,
                ));
            }
            if v.anonymous {
                desc.push_str("\n🔒 *Anonymous ballot unsealed.*");
            }
        }
    } else {
        let time_str = if v.ends > 0 {
            format!("Ends <t:{}:R>", v.ends / 1000)
        } else {
            "Open vote".to_owned()
        };
        desc.push_str(&format!("{time_str} · Hosted by <@{}>\n", v.host_id));
        if v.anonymous {
            desc.push_str("🔒 **Anonymous Vote**: Choices are secret until the vote ends.\n\n");
            desc.push_str(&format!("Total votes cast: **{total}**\n\n"));
            desc.push_str("**Options:**\n");
            for (idx, opt) in v.options.iter().enumerate() {
                desc.push_str(&format!("{} {}\n", option_emoji(idx), opt));
            }
        } else {
            desc.push_str(&format!("Total votes: **{total}**\n\n"));
            desc.push_str("**Options:**\n");
            for (idx, opt) in v.options.iter().enumerate() {
                let count = tallies[idx];
                let pct = if total > 0 {
                    (count as f64 / total as f64 * 100.0).round() as u32
                } else {
                    0
                };
                let bar = progress_bar(count, total, 10);
                desc.push_str(&format!(
                    "{} **{}** — {} vote{} ({}%)\n`{}`\n",
                    option_emoji(idx),
                    opt,
                    count,
                    if count == 1 { "" } else { "s" },
                    pct,
                    bar
                ));
            }
        }
        desc.push_str("\n*Click a button below to cast or change your vote.*");
    }

    let embed_color = if v.ended {
        0x95A5A6
    } else if v.anonymous {
        0x9B59B6
    } else {
        0x3498DB
    };

    let mut rows: Vec<Value> = Vec::new();
    for (chunk_idx, chunk) in v.options.chunks(5).enumerate() {
        let mut buttons: Vec<Value> = Vec::new();
        for (i, opt) in chunk.iter().enumerate() {
            let opt_idx = chunk_idx * 5 + i;
            let label = if opt.len() > 70 {
                format!("{}. {}…", opt_idx + 1, &opt[..67])
            } else {
                format!("{}. {}", opt_idx + 1, opt)
            };
            buttons.push(json!({
                "type": 2,
                "style": if v.ended { 2 } else { 1 },
                "label": label,
                "custom_id": format!("{VOTE_BUTTON}{opt_idx}"),
                "disabled": v.ended,
            }));
        }
        rows.push(json!({ "type": 1, "components": buttons }));
    }

    json!({
        "embeds": [{
            "title": title,
            "description": desc,
            "color": embed_color,
        }],
        "components": rows,
        "allowed_mentions": { "parse": [] },
    })
}

pub async fn start_vote(core: &AgentCore, guild_id: u64, mut v: Vote) -> Result<Vote, String> {
    v.agent_id = core.id.clone();
    let posted = core
        .http
        .raw(
            "POST",
            &format!("/channels/{}/messages", v.channel_id),
            Some(vote_body(&v)),
            None,
        )
        .await
        .map_err(api_error)?;
    v.message_id = posted
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    core.hive
        .knowledge
        .guild_mut(guild_id, |k| k.votes.push(v.clone()));
    Ok(v)
}

async fn cast_vote(
    core: &AgentCore,
    guild_id: u64,
    interaction: &Interaction,
    user_id: u64,
    id: &str,
) -> Result<String, String> {
    let opt_idx: usize = id
        .strip_prefix(VOTE_BUTTON)
        .and_then(|s| s.parse().ok())
        .ok_or("invalid vote option")?;
    let message_id = interaction
        .message
        .as_ref()
        .map(|m| m.id.get())
        .unwrap_or(0);

    let (reply_msg, updated) = core.hive.knowledge.guild_mut(guild_id, |k| {
        let Some(v) = k.votes.iter_mut().find(|v| v.message_id == message_id) else {
            return Err("that vote isn't active anymore".to_owned());
        };
        if v.ended {
            return Err("this vote has already ended".to_owned());
        }
        if opt_idx >= v.options.len() {
            return Err("invalid vote option".to_owned());
        }
        let opt_name = v.options[opt_idx].clone();
        let prev = v.votes.insert(user_id, opt_idx);
        let msg = match prev {
            Some(p) if p == opt_idx => format!("You are currently voting for **{opt_name}**."),
            Some(_) => format!("Changed your vote to **{opt_name}**."),
            None => format!("Voted for **{opt_name}**!"),
        };
        let final_msg = if v.anonymous {
            format!("{msg}\n🔒 *Your vote is anonymous and will only be revealed at the end.*")
        } else {
            msg
        };
        Ok((final_msg, v.clone()))
    })?;

    // Update the message in Discord
    let _ = core
        .http
        .raw(
            "PATCH",
            &format!(
                "/channels/{}/messages/{}",
                updated.channel_id, updated.message_id
            ),
            Some(vote_body(&updated)),
            None,
        )
        .await;

    Ok(reply_msg)
}

pub async fn finish_vote(core: &AgentCore, guild_id: u64, message_id: u64) -> Result<Vote, String> {
    let v = core.hive.knowledge.guild_mut(guild_id, |k| {
        let v = k.votes.iter_mut().find(|v| v.message_id == message_id)?;
        v.ended = true;
        Some(v.clone())
    });
    let v = v.ok_or("no vote found on that message")?;

    // Edit message with final results and disabled buttons
    let _ = core
        .http
        .raw(
            "PATCH",
            &format!("/channels/{}/messages/{}", v.channel_id, v.message_id),
            Some(vote_body(&v)),
            None,
        )
        .await;

    // Post announcement in channel
    let total = v.votes.len();
    let tallies = v.tally();
    let announcement = if total == 0 {
        format!(
            "📊 The vote on **{}** ended with no votes cast.",
            v.question
        )
    } else {
        let max_votes = tallies.iter().copied().max().unwrap_or(0);
        let winners: Vec<&str> = v
            .options
            .iter()
            .enumerate()
            .filter(|(idx, _)| tallies[*idx] == max_votes)
            .map(|(_, name)| name.as_str())
            .collect();
        if winners.len() == 1 {
            let pct = (max_votes as f64 / total as f64 * 100.0).round() as u32;
            format!(
                "📊 The vote on **{}** has concluded!\n🏆 Winner: **{}** with {} vote{} ({}% of {} total votes)!",
                v.question,
                winners[0],
                max_votes,
                if max_votes == 1 { "" } else { "s" },
                pct,
                total
            )
        } else {
            let pct = (max_votes as f64 / total as f64 * 100.0).round() as u32;
            format!(
                "📊 The vote on **{}** ended in a tie!\n🏆 Tied winners: **{}** with {} votes each ({}% of {} total votes)!",
                v.question,
                winners.join(" and "),
                max_votes,
                pct,
                total
            )
        }
    };
    let _ = core
        .http
        .raw(
            "POST",
            &format!("/channels/{}/messages", v.channel_id),
            Some(json!({
                "content": announcement,
                "allowed_mentions": { "parse": [] },
            })),
            None,
        )
        .await;

    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_unique_winners() {
        let picked = draw(&[1, 2, 3, 4], 2, &[]);
        assert_eq!(picked.len(), 2);
        assert_ne!(picked[0], picked[1]);
        assert_eq!(draw(&[1, 2], 5, &[]).len(), 2);
        assert_eq!(draw(&[1, 2], 1, &[1]), vec![2]);
    }

    #[test]
    fn role_menus_render_buttons_or_a_dropdown() {
        let roles = vec![MenuRole {
            role_id: 5,
            label: "Gamer".into(),
            emoji: "🎮".into(),
            description: String::new(),
        }];
        let buttons = role_menu_body("Roles", "Pick", &roles, false, 0);
        assert_eq!(
            buttons["components"][0]["components"][0]["custom_id"],
            "axis_roles:5"
        );
        let menu = role_menu_body("Roles", "Pick", &roles, true, 0);
        assert_eq!(
            menu["components"][0]["components"][0]["options"][0]["value"],
            "5"
        );
    }

    #[test]
    fn vote_body_renders_buttons_and_embed() {
        let mut v = Vote {
            agent_id: "agent1".into(),
            channel_id: 100,
            message_id: 200,
            question: "Best language?".into(),
            options: vec!["Rust".into(), "Python".into(), "TypeScript".into()],
            host_id: 50,
            ends: 1_800_000_000_000,
            anonymous: false,
            votes: std::collections::HashMap::from([(1, 0), (2, 0), (3, 1)]),
            ended: false,
        };

        let body = vote_body(&v);
        // Verify buttons
        let rows = body["components"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let buttons = rows[0]["components"].as_array().unwrap();
        assert_eq!(buttons.len(), 3);
        assert_eq!(buttons[0]["custom_id"], "axis_vote:0");
        assert_eq!(buttons[1]["custom_id"], "axis_vote:1");
        assert_eq!(buttons[2]["custom_id"], "axis_vote:2");
        assert_eq!(buttons[0]["disabled"], false);

        // Verify description has live tally for non-anonymous
        let desc = body["embeds"][0]["description"].as_str().unwrap();
        assert!(desc.contains("Total votes: **3**"));
        assert!(desc.contains("Rust"));
        assert!(desc.contains("67%"));

        // Now test anonymous vote while running: options breakdown must be hidden
        v.anonymous = true;
        let anon_body = vote_body(&v);
        let anon_desc = anon_body["embeds"][0]["description"].as_str().unwrap();
        assert!(anon_desc.contains("Anonymous Vote"));
        assert!(anon_desc.contains("Total votes cast: **3**"));
        assert!(
            !anon_desc.contains("67%"),
            "live breakdown must be hidden in anonymous mode"
        );

        // When ended, anonymous vote reveals the final results and disables all buttons
        v.ended = true;
        let ended_body = vote_body(&v);
        let ended_desc = ended_body["embeds"][0]["description"].as_str().unwrap();
        assert!(ended_desc.contains("Final Results"));
        assert!(ended_desc.contains("67%"));
        assert!(ended_desc.contains("Anonymous ballot unsealed"));
        let ended_buttons = ended_body["components"][0]["components"]
            .as_array()
            .unwrap();
        assert_eq!(ended_buttons[0]["disabled"], true);
    }
}
