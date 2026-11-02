//! Community features built on buttons and dropdowns: self-assign role
//! menus and giveaways.

use std::sync::Arc;

use axis::model::{Interaction, InteractionData};
use axis::prelude::*;
use serde_json::{Value, json};

use super::agent::AgentCore;
use super::live::FeedKind;
use crate::memory::{Giveaway, now_ms};

pub const ROLE_BUTTON: &str = "axis_roles:";
pub const ROLE_MENU: &str = "axis_roles_menu";
pub const GIVEAWAY_ENTER: &str = "axis_giveaway:enter";

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
pub fn role_menu_body(title: &str, message: &str, roles: &[MenuRole], dropdown: bool, color: u32) -> Value {
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

async fn set_role(core: &AgentCore, guild_id: u64, user_id: u64, role_id: u64, give: bool) -> Result<(), String> {
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
    if !(id.starts_with(ROLE_BUTTON) || id == ROLE_MENU || id == GIVEAWAY_ENTER) {
        return false;
    }
    let (Some(guild_id), Some(user)) = (
        interaction.guild_id.map(|g| g.get()),
        interaction.member.as_ref().and_then(|m| m.user.clone()).or_else(|| interaction.user.clone()),
    ) else {
        return true;
    };
    let user_id = user.id.get();
    let has = member_roles(interaction);
    let reply = if let Some(role) = id.strip_prefix(ROLE_BUTTON).and_then(|r| r.parse::<u64>().ok()) {
        let give = !has.contains(&role);
        set_role(core, guild_id, user_id, role, give)
            .await
            .map(|()| format!("{} <@&{role}>", if give { "✅ Gave you" } else { "➖ Removed" }))
    } else if id == ROLE_MENU {
        // Every role this dropdown offers, from the message itself.
        let offered: Vec<u64> = interaction
            .message
            .as_ref()
            .map(|m| {
                m.components
                    .iter()
                    .flat_map(|row| row.get("components").and_then(Value::as_array).cloned().unwrap_or_default())
                    .flat_map(|c| c.get("options").and_then(Value::as_array).cloned().unwrap_or_default())
                    .filter_map(|o| o.get("value").and_then(Value::as_str).and_then(|v| v.parse().ok()))
                    .collect()
            })
            .unwrap_or_default();
        let chosen: Vec<u64> = data.values.clone().unwrap_or_default().iter().filter_map(|v| v.parse().ok()).collect();
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
    let _ = core.http.respond_to_interaction(interaction.id, interaction.token.clone(), data).await;
    true
}

fn enter_giveaway(core: &AgentCore, guild_id: u64, interaction: &Interaction, user_id: u64, roles: &[u64]) -> Result<String, String> {
    let message_id = interaction.message.as_ref().map(|m| m.id.get()).unwrap_or(0);
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
            Ok(format!("🎉 You're in for **{}**! ({} entries) Press again to leave.", g.prize, g.entrants.len()))
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
                g.picked.iter().map(|w| format!("<@{w}>")).collect::<Vec<_>>().join(", "),
                g.host_id
            )
        }
    } else {
        format!(
            "Ends <t:{}:R> · {} winner{}{}\nHosted by <@{}>\nPress the button to enter.",
            g.ends / 1000,
            g.winners,
            if g.winners == 1 { "" } else { "s" },
            if g.required_role != 0 { format!(" · requires <@&{}>", g.required_role) } else { String::new() },
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

pub async fn start_giveaway(core: &AgentCore, guild_id: u64, mut g: Giveaway) -> Result<Giveaway, String> {
    g.agent_id = core.id.clone();
    let posted = core
        .http
        .raw("POST", &format!("/channels/{}/messages", g.channel_id), Some(giveaway_body(&g)), None)
        .await
        .map_err(api_error)?;
    g.message_id = posted.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0);
    core.hive.knowledge.guild_mut(guild_id, |k| k.giveaways.push(g.clone()));
    Ok(g)
}

fn draw(entrants: &[u64], winners: u32, exclude: &[u64]) -> Vec<u64> {
    let mut pool: Vec<u64> = entrants.iter().copied().filter(|e| !exclude.contains(e)).collect();
    let mut picked = Vec::new();
    while !pool.is_empty() && picked.len() < winners as usize {
        picked.push(pool.swap_remove(fastrand::usize(..pool.len())));
    }
    picked
}

/// End a giveaway now (or reroll an ended one); returns the winners.
pub async fn finish(core: &AgentCore, guild_id: u64, message_id: u64, reroll: bool) -> Result<Vec<u64>, String> {
    let g = core.hive.knowledge.guild_mut(guild_id, |k| {
        let g = k.giveaways.iter_mut().find(|g| g.message_id == message_id)?;
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
        .raw("PATCH", &format!("/channels/{}/messages/{}", g.channel_id, g.message_id), Some(giveaway_body(&g)), None)
        .await;
    let new: Vec<u64> = if reroll { g.picked.last().copied().into_iter().collect() } else { g.picked.clone() };
    let text = if new.is_empty() {
        format!("The giveaway for **{}** ended with no valid entries.", g.prize)
    } else {
        format!(
            "🎉 Congratulations {}! You won **{}**!",
            new.iter().map(|w| format!("<@{w}>")).collect::<Vec<_>>().join(", "),
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
    core.feed(FeedKind::System, format!("giveaway for {} ended", g.prize), None);
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
    }
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
        let roles = vec![MenuRole { role_id: 5, label: "Gamer".into(), emoji: "🎮".into(), description: String::new() }];
        let buttons = role_menu_body("Roles", "Pick", &roles, false, 0);
        assert_eq!(buttons["components"][0]["components"][0]["custom_id"], "axis_roles:5");
        let menu = role_menu_body("Roles", "Pick", &roles, true, 0);
        assert_eq!(menu["components"][0]["components"][0]["options"][0]["value"], "5");
    }
}
