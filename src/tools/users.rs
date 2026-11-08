//! Numbered user selection: the model picks people from a list by number
//! instead of writing usernames or IDs by hand.
//!
//! Typing an ID from memory is where models get things wrong (a wrong
//! digit bans the wrong person). The flow here is mechanical:
//! `find_users` lists numbered candidates (display name, nickname, username, ID);
//! every tool that takes a user also accepts `#N` — or a comma/range
//! list like `#2,5-7` where several are allowed — resolved against that
//! list by the app, so the model never transcribes an ID.
//! In addition, `pick_users` allows explicitly inspecting and verifying
//! selected members from the candidate list before acting.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use serde_json::Value;

use super::ToolCtx;

/// One candidate as the model sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub user_id: u64,
    pub username: String,
    pub display: String,
    pub nick: Option<String>,
    pub bot: bool,
}

/// Per-channel numbered candidate lists (the last find_users result).
fn lists() -> &'static Mutex<HashMap<u64, Vec<Candidate>>> {
    static LISTS: OnceLock<Mutex<HashMap<u64, Vec<Candidate>>>> = OnceLock::new();
    LISTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Replace the channel's candidate list. Returns the numbered listing.
pub fn set(channel_id: u64, candidates: Vec<Candidate>) -> String {
    let mut out = String::from("Found:\n");
    for (index, candidate) in candidates.iter().enumerate() {
        let nick_str = match &candidate.nick {
            Some(n) if n != &candidate.username => format!(" (nick: {n})"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "[{}] {}{} — @{} (ID {}){}\n",
            index + 1,
            candidate.display,
            nick_str,
            candidate.username,
            candidate.user_id,
            if candidate.bot { " [bot]" } else { "" },
        ));
    }
    out.push_str(
        "Pick by number: give `user: #2` (or a list like `user: #1,3-5`) to any tool that takes a user, or inspect with `pick_users` — never retype the name or ID.\n",
    );
    lists()
        .lock()
        .expect("user lists poisoned")
        .insert(channel_id, candidates);
    out
}

/// Resolve a selection query (e.g. "#2", "2", "#1,3-5", "1-3", "all") against the channel's candidate list.
/// Plain non-selection queries (names, raw snowflake IDs) return `Ok(None)` so callers fall back to standard lookup.
pub fn resolve_candidates(
    channel_id: u64,
    query: &str,
    many: bool,
) -> Result<Option<Vec<Candidate>>, String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let has_hash = trimmed.starts_with('#');
    let select = trimmed.strip_prefix('#').unwrap_or(trimmed).trim();

    let candidates = {
        let lists = lists().lock().expect("user lists poisoned");
        match lists.get(&channel_id) {
            Some(c) => c.clone(),
            None => {
                if has_hash {
                    return Err(
                        "#N needs a find_users list first — call find_users, then pick".to_owned(),
                    );
                } else {
                    return Ok(None);
                }
            }
        }
    };

    if candidates.is_empty() {
        if has_hash {
            return Err("candidate list is empty — call find_users first".to_owned());
        } else {
            return Ok(None);
        }
    }

    let is_all = select.eq_ignore_ascii_case("all");

    // If it doesn't have '#', verify if it looks like a selection or something else (e.g. name or snowflake)
    if !has_hash && !is_all {
        let valid_chars = select
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == '-' || c.is_whitespace());
        if !valid_chars {
            return Ok(None);
        }
        let mut looks_like_selection = true;
        for part in select.split([',', ' ']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((from, to)) = part.split_once('-') {
                let (Ok(from), Ok(to)) = (from.trim().parse::<usize>(), to.trim().parse::<usize>())
                else {
                    looks_like_selection = false;
                    break;
                };
                if from == 0 || to < from || to > candidates.len() {
                    looks_like_selection = false;
                    break;
                }
            } else if let Ok(n) = part.parse::<usize>() {
                if n == 0 || n > candidates.len() {
                    looks_like_selection = false;
                    break;
                }
            } else {
                looks_like_selection = false;
                break;
            }
        }
        if !looks_like_selection {
            return Ok(None);
        }
    }

    let mut picked: Vec<usize> = Vec::new();
    if is_all {
        picked.extend(1..=candidates.len());
    } else {
        for part in select.split([',', ' ']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((from, to)) = part.split_once('-') {
                let (Ok(from), Ok(to)) = (from.trim().parse::<usize>(), to.trim().parse::<usize>())
                else {
                    return Err(format!("# range {part:?} is not numbers"));
                };
                if from == 0 || to < from || to > candidates.len() {
                    return Err(format!(
                        "# range {part:?} is outside 1-{}",
                        candidates.len()
                    ));
                }
                picked.extend(from..=to);
            } else {
                let Ok(n) = part.parse::<usize>() else {
                    return Err(format!("#{part} is not a number"));
                };
                if n == 0 || n > candidates.len() {
                    return Err(format!(
                        "#{n} is outside 1-{} — the list from your last find_users",
                        candidates.len()
                    ));
                }
                picked.push(n);
            }
        }
    }
    picked.sort_unstable();
    picked.dedup();
    if picked.is_empty() {
        return Err("nothing selected — give #N, a list like #1,3-5, or #all".to_owned());
    }
    if !many && picked.len() > 1 {
        return Err(format!(
            "that selected {} people but the tool takes one — give a single #N",
            picked.len()
        ));
    }
    Ok(Some(
        picked
            .into_iter()
            .map(|n| candidates[n - 1].clone())
            .collect(),
    ))
}

/// Resolve a `#N`-style selection against the channel's list.
/// `many` false: exactly one; `many` true: any count. Plain names/IDs are
/// NOT resolved here — callers fall back to their own lookup for those.
pub fn resolve(channel_id: u64, query: &str, many: bool) -> Result<Option<Vec<u64>>, String> {
    resolve_candidates(channel_id, query, many)
        .map(|opt| opt.map(|list| list.into_iter().map(|c| c.user_id).collect()))
}

/// Inspect and select users from the find_users candidate list.
pub async fn pick_users(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let pick = args
        .get("pick")
        .and_then(Value::as_str)
        .ok_or("give the `pick` argument (e.g. '2', '#2', '1,3', '2-5', or 'all')")?;
    let candidates = resolve_candidates(ctx.channel_id, pick, true)?.ok_or_else(|| {
        "no matching candidates found in the list; call find_users first".to_owned()
    })?;

    let mut out = format!(
        "Selected {} member{}:\n",
        candidates.len(),
        if candidates.len() == 1 { "" } else { "s" }
    );
    for candidate in &candidates {
        let nick_str = match &candidate.nick {
            Some(n) if n != &candidate.username => format!(" (nick: {n})"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "- {}{} — @{} (ID {}){}\n",
            candidate.display,
            nick_str,
            candidate.username,
            candidate.user_id,
            if candidate.bot { " [bot]" } else { "" },
        ));
    }
    if candidates.len() == 1 {
        let c = &candidates[0];
        out.push_str(&format!(
            "You can refer to this user in tools with `user: \"#{}\"` or their ID `{}`.",
            c.user_id, c.user_id
        ));
    } else {
        out.push_str("You can refer to these users with `#N` or their IDs in other tools.");
    }
    Ok(out)
}

/// The tool entry: search members and list them numbered.
pub async fn find_users(ctx: &ToolCtx, args: &Value) -> Result<String, String> {
    let guild_id = ctx.require_guild()?;
    let raw_query = args
        .get("like")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let query = raw_query
        .trim_start_matches("<@!")
        .trim_start_matches("<@")
        .trim_end_matches('>')
        .trim_start_matches('@')
        .to_lowercase();
    let members: Vec<(u64, String, Option<String>, bool)> = ctx
        .core
        .world
        .with_guild(guild_id, |g| {
            g.members
                .values()
                .map(|m| (m.user_id, m.username.clone(), m.nick.clone(), m.bot))
                .collect()
        })
        .unwrap_or_default();
    let mut candidates: Vec<Candidate> = Vec::new();
    for (user_id, username, nick, bot) in members {
        let id_text = user_id.to_string();
        let display = nick.clone().unwrap_or_else(|| username.clone());
        let matches = query.is_empty()
            || username.to_lowercase().contains(&query)
            || display.to_lowercase().contains(&query)
            || nick
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains(&query)
            || id_text.contains(&query);
        if matches {
            candidates.push(Candidate {
                user_id,
                username,
                display,
                nick,
                bot,
            });
        }
    }
    if candidates.is_empty() {
        return Ok(format!("nobody matches {query:?}"));
    }
    candidates.sort_by(|a, b| a.display.to_lowercase().cmp(&b.display.to_lowercase()));
    let shown: Vec<Candidate> = candidates.iter().take(25).cloned().collect();
    let more = candidates.len() - shown.len();
    let mut listing = set(ctx.channel_id, shown);
    if more > 0 {
        listing.push_str(&format!("({more} more not shown — refine the search)\n"));
    }
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<Candidate> {
        (1..=5)
            .map(|n| Candidate {
                user_id: 100 + n,
                username: format!("user{n}"),
                display: format!("Member {n}"),
                nick: if n == 2 {
                    Some("Nick2".to_owned())
                } else {
                    None
                },
                bot: n == 3,
            })
            .collect()
    }

    #[test]
    fn numbered_selection_resolves() {
        let list = candidates();
        set(777, list);
        assert_eq!(
            resolve(777, "#2", false).unwrap(),
            Some(vec![102]),
            "single pick with #"
        );
        assert_eq!(
            resolve(777, "2", false).unwrap(),
            Some(vec![102]),
            "single pick without #"
        );
        assert_eq!(
            resolve(777, "#1,3-5", true).unwrap(),
            Some(vec![101, 103, 104, 105]),
            "comma + range with #"
        );
        assert_eq!(
            resolve(777, "1,3-5", true).unwrap(),
            Some(vec![101, 103, 104, 105]),
            "comma + range without #"
        );
        assert_eq!(
            resolve(777, "#all", true).unwrap(),
            Some(vec![101, 102, 103, 104, 105])
        );
        assert_eq!(
            resolve(777, "all", true).unwrap(),
            Some(vec![101, 102, 103, 104, 105])
        );
    }

    #[test]
    fn non_selection_returns_none_for_caller_fallback() {
        assert_eq!(resolve(777, "somebody", false).unwrap(), None);
        assert_eq!(resolve(777, "80351110224678912", false).unwrap(), None);
    }

    #[test]
    fn out_of_range_and_multi_rejected() {
        set(778, candidates());
        assert!(resolve(778, "#9", false).is_err());
        assert!(resolve(778, "#1,2", false).is_err(), "one tool, two picks");
        assert!(resolve(779, "#1", false).is_err(), "no list yet");
    }

    #[test]
    fn listing_is_numbered_with_ids() {
        let listing = set(780, candidates());
        assert!(listing.contains("[1] Member 1 — @user1 (ID 101)"));
        assert!(listing.contains("[2] Member 2 (nick: Nick2) — @user2 (ID 102)"));
        assert!(listing.contains("[3] Member 3 — @user3 (ID 103) [bot]"));
        assert!(listing.contains("user: #2"));
    }
}
