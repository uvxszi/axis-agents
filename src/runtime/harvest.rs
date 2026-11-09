//! Roaming harvest storage: durable people-notes and channel summaries
//! gathered while idle. Notes merge with what's already known (append,
//! deduped) — never overwrite a person's notes wholesale.

use std::sync::Arc;

use crate::runtime::agent::AgentCore;

/// Add a durable fact about a person, merging with their existing notes.
pub async fn note_person(core: &Arc<AgentCore>, name: &str, fact: &str) {
    // Find the member by display/username across guilds.
    let found: Option<(u64, String)> = core.world.guild_ids().into_iter().find_map(|g| {
        core.world
            .with_guild(g, |state| {
                state.members.values().find_map(|m| {
                    let matches = m.username.eq_ignore_ascii_case(name)
                        || m.nick
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                        || m.global_name
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(name));
                    matches.then(|| (m.user_id, m.username.clone()))
                })
            })
            .flatten()
    });
    let Some((user_id, username)) = found else {
        return;
    };
    let existing = {
        let notes = std::cell::RefCell::new(Vec::<String>::new());
        core.hive.knowledge.all_guilds(|k| {
            if let Some(note) = k.people.get(&user_id) {
                notes.borrow_mut().push(note.notes.clone());
            }
        });
        notes.into_inner().into_iter().next().unwrap_or_default()
    };
    // Dedup: skip if the fact is already in there (loose contains).
    if existing
        .to_lowercase()
        .contains(&fact.to_lowercase().chars().take(40).collect::<String>())
    {
        return;
    }
    let merged = if existing.is_empty() {
        fact.to_owned()
    } else {
        format!("{existing} {fact}")
    };
    core.hive.knowledge.all_guilds_mut(|k| {
        let note = k
            .people
            .entry(user_id)
            .or_insert(crate::memory::PersonNote {
                name: username.clone(),
                notes: String::new(),
                updated: crate::memory::now_ms(),
                strikes: vec![],
                opted_out: false,
            });
        note.notes = crate::llm::truncate(&merged, 900).to_owned();
        note.updated = crate::memory::now_ms();
    });
}

/// Record a channel summary into long-term memory.
pub async fn note_channel(core: &Arc<AgentCore>, channel_id: u64, summary: &str) {
    core.memory.add(crate::memory::Memory {
        id: 0,
        text: format!("channel #{channel_id} lately: {summary}"),
        kind: crate::memory::MemoryKind::Fact,
        about_user: None,
        about_name: None,
        guild_id: None,
        source: "idle roaming".to_owned(),
        created: crate::memory::now_ms(),
        last_used: 0,
        uses: 0,
        importance: 2,
        embedding: vec![],
    });
    let _ = core;
}
