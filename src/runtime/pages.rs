//! Page buttons on long answers: ◀ ▶ flip through, "Show all" posts the
//! rest as plain messages.

use std::sync::Arc;

use axis::model::{Interaction, InteractionData};
use serde_json::json;

use super::agent::AgentCore;

async fn respond(core: &AgentCore, interaction: &Interaction, kind: u8, data: serde_json::Value) {
    let _ = core
        .http
        .raw(
            "POST",
            &format!("/interactions/{}/{}/callback", interaction.id, interaction.token),
            Some(json!({ "type": kind, "data": data })),
            None,
        )
        .await;
}

pub async fn on_component(core: &Arc<AgentCore>, interaction: &Interaction) -> bool {
    let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
        return false;
    };
    let Some(action) = data.custom_id.strip_prefix("axis_page:") else {
        return false;
    };
    let Some(message) = &interaction.message else {
        return true;
    };
    let pages = core.pages.lock().expect("pages poisoned").get(&message.id.get()).cloned();
    let Some(pages) = pages else {
        respond(core, interaction, 4, json!({ "content": "These pages expired (I was restarted) — ask me again.", "flags": 64 })).await;
        return true;
    };
    // Which page is showing: the "n/total" label on the buttons.
    let current = message
        .components
        .first()
        .and_then(|row| row.pointer("/components/1/label"))
        .and_then(|l| l.as_str())
        .and_then(|l| l.split('/').next())
        .and_then(|n| n.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .saturating_sub(1);
    let index = match action {
        "next" => (current + 1).min(pages.len() - 1),
        "prev" => current.saturating_sub(1),
        "all" => {
            // Post the remaining pages as regular messages.
            respond(core, interaction, 6, json!({})).await;
            for page in pages.iter().skip(current + 1) {
                let _ = core
                    .http
                    .raw(
                        "POST",
                        &format!("/channels/{}/messages", message.channel_id),
                        Some(json!({ "content": page, "allowed_mentions": { "parse": [] } })),
                        None,
                    )
                    .await;
            }
            return true;
        }
        _ => current,
    };
    respond(
        core,
        interaction,
        7,
        json!({
            "content": pages[index],
            "components": super::brain::page_buttons(index, pages.len()),
            "allowed_mentions": { "parse": [] },
        }),
    )
    .await;
    true
}
