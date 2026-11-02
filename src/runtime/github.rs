//! Watching GitHub repos: a new release or new commits wake the standing
//! task that watches them, which then does what it was told with the
//! details (post the link, sum up the highlights, …).
//!
//! A local app can't receive webhooks, so repos are checked every few
//! minutes. Requests carry the last ETag; GitHub answers "not modified"
//! without counting it against the rate limit (60 an hour without a
//! token, 5000 with one — set it in Settings).

use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use super::agent::AgentCore;
use crate::memory::{StandingTask, TaskTrigger, now_ms};

/// How often each watched repo is checked.
const EVERY_MS: i64 = 5 * 60_000;

/// "https://github.com/owner/repo/releases", "github.com/owner/repo",
/// "owner/repo" → "owner/repo".
pub fn parse_repo(text: &str) -> Option<String> {
    let t = text.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = match t.find("github.com/") {
        Some(i) => &t[i + "github.com/".len()..],
        None => t,
    };
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok(owner) && ok(name) && !owner.contains("..")).then(|| format!("{owner}/{name}"))
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("AxisAgents")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("http client")
    })
}

/// Rate limited until this moment (ms).
fn backoff() -> &'static Mutex<i64> {
    static UNTIL: OnceLock<Mutex<i64>> = OnceLock::new();
    UNTIL.get_or_init(Default::default)
}

pub enum Fetched {
    NotModified,
    Body(Value, Option<String>),
}

/// GET from the GitHub API, conditional on `etag`.
pub async fn api(path: &str, etag: &str, token: &str) -> Result<Fetched, String> {
    if *backoff().lock().expect("backoff poisoned") > now_ms() {
        return Err("GitHub rate limit reached — waiting for it to reset (a GitHub token in Settings raises the limit)".to_owned());
    }
    let mut request = client()
        .get(format!("https://api.github.com{path}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if !token.trim().is_empty() {
        request = request.bearer_auth(token.trim());
    }
    if !etag.is_empty() {
        request = request.header("If-None-Match", etag);
    }
    let response = request.send().await.map_err(|e| format!("couldn't reach GitHub: {e}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Fetched::NotModified);
    }
    let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    if matches!(status.as_u16(), 403 | 429) && header("x-ratelimit-remaining").as_deref() == Some("0") {
        let reset = header("x-ratelimit-reset").and_then(|r| r.parse::<i64>().ok()).map(|s| s * 1000).unwrap_or(now_ms() + 3_600_000);
        *backoff().lock().expect("backoff poisoned") = reset;
        return Err("GitHub rate limit reached — add a GitHub token in Settings to raise it".to_owned());
    }
    let etag = header("etag");
    match status.as_u16() {
        200 => Ok(Fetched::Body(response.json().await.map_err(|e| format!("bad answer from GitHub: {e}"))?, etag)),
        404 => Err("GitHub says that repo doesn't exist (or it's private — a token with access goes in Settings)".to_owned()),
        401 => Err("GitHub rejected the token in Settings".to_owned()),
        code => Err(format!("GitHub answered {code}")),
    }
}

/// What a repo looks like right now (when starting to watch it).
pub struct RepoNow {
    pub full_name: String,
    pub default_branch: String,
    /// (id, tag, name, url) of the newest published release.
    pub latest_release: Option<(u64, String, String, String)>,
    /// The two newest commits on the branch (newest first).
    pub latest_commits: Vec<String>,
}

pub async fn look(repo: &str, branch: Option<&str>, token: &str) -> Result<RepoNow, String> {
    let repo = parse_repo(repo).ok_or_else(|| format!("\"{repo}\" isn't a GitHub repo"))?;
    let repo = repo.as_str();
    let Fetched::Body(info, _) = api(&format!("/repos/{repo}"), "", token).await? else {
        return Err("GitHub didn't answer".to_owned());
    };
    let full_name = info["full_name"].as_str().unwrap_or(repo).to_owned();
    let default_branch = info["default_branch"].as_str().unwrap_or("main").to_owned();
    let latest_release = match api(&format!("/repos/{full_name}/releases?per_page=5"), "", token).await? {
        Fetched::Body(Value::Array(list), _) => list.iter().filter(|r| !r["draft"].as_bool().unwrap_or(false)).find_map(|r| {
            Some((
                r["id"].as_u64()?,
                r["tag_name"].as_str().unwrap_or("").to_owned(),
                r["name"].as_str().unwrap_or("").to_owned(),
                r["html_url"].as_str().unwrap_or("").to_owned(),
            ))
        }),
        _ => None,
    };
    let branch = branch.unwrap_or(&default_branch);
    let latest_commits = match api(&format!("/repos/{full_name}/commits?sha={branch}&per_page=2"), "", token).await {
        Ok(Fetched::Body(Value::Array(list), _)) => list.iter().filter_map(|c| c["sha"].as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    };
    Ok(RepoNow { full_name, default_branch, latest_release, latest_commits })
}

/// Release notes without the "Full Changelog: …/compare/…" line (people
/// don't want that link posted).
fn notes(body: &str) -> String {
    body.lines()
        .filter(|l| {
            let l = l.to_lowercase();
            !(l.contains("full changelog") || l.contains("/compare/"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// New releases since `last_seen` (an id), oldest first: (id, task input).
pub(crate) fn new_releases(list: &[Value], last_seen: u64, repo: &str) -> Vec<(u64, String)> {
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
    let mut fresh: Vec<&Value> = list
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false))
        .filter(|r| r["id"].as_u64().is_some_and(|id| id > last_seen))
        .collect();
    fresh.sort_by_key(|r| r["id"].as_u64().unwrap_or(0));
    // At most a few at once (e.g. after the app was off for a while).
    let skip = fresh.len().saturating_sub(3);
    fresh
        .iter()
        .skip(skip)
        .map(|r| {
            let body = notes(r["body"].as_str().unwrap_or(""));
            let heading = release_heading(repo_name, r);
            let text = format!(
                "A new release was just published.\nHeading: {heading}\nRelease: {}{}\nTag: {}\nLink: {}\nPublished: {}\nRelease notes:\n{}\n\n(Unless your instruction says otherwise: start with the heading line exactly as given, then the release link on its own line as a plain URL (no markdown, no badges — it shows a preview), then a few short bullet points of the highlights. Skip install/download steps. Don't add a full-changelog or compare link.)",
                r["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(r["tag_name"].as_str().unwrap_or("?")),
                if r["prerelease"].as_bool().unwrap_or(false) { " (pre-release)" } else { "" },
                r["tag_name"].as_str().unwrap_or("?"),
                r["html_url"].as_str().unwrap_or(""),
                r["published_at"].as_str().unwrap_or(""),
                if body.is_empty() { "(none)".to_owned() } else { crate::llm::truncate(&body, 5000) }
            );
            (r["id"].as_u64().unwrap_or(0), text)
        })
        .collect()
}

/// "# Sue v0.0.73": the repo's name and the version, big. Uses the
/// release's own name when it already says both.
fn release_heading(repo_name: &str, release: &Value) -> String {
    let tag = release["tag_name"].as_str().unwrap_or("");
    let name = release["name"].as_str().unwrap_or("").trim();
    let has_version = !tag.is_empty() && name.contains(tag.trim_start_matches('v'));
    let has_repo = name.to_lowercase().contains(&repo_name.to_lowercase());
    if has_version && has_repo {
        format!("# {name}")
    } else {
        let mut title: Vec<char> = repo_name.chars().collect();
        if let Some(first) = title.first_mut() {
            *first = first.to_ascii_uppercase();
        }
        format!("# {} {}", title.into_iter().collect::<String>(), if tag.is_empty() { name } else { tag })
    }
}

/// The heading a release input asks for ("Heading: # …").
pub fn heading_line(input: &str) -> Option<String> {
    input.lines().find_map(|l| l.strip_prefix("Heading: ")).map(|l| l.trim().to_owned()).filter(|l| l.starts_with('#'))
}

/// Did the watch's instruction ask for no heading?
pub fn no_heading(instruction: &str) -> bool {
    let t = instruction.to_lowercase();
    ["no heading", "no header", "no title", "without a heading", "without heading", "without a title", "no big text", "don't use #", "dont use #", "no #"]
        .iter()
        .any(|p| t.contains(p))
}

/// The newest release id in a list (where watching starts).
fn newest_release(list: &[Value]) -> u64 {
    list.iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false))
        .filter_map(|r| r["id"].as_u64())
        .max()
        .unwrap_or(0)
}

/// A GitHub post as it goes out: only the main link keeps its preview —
/// other links are wrapped in <…> (Discord doesn't embed those), and
/// full-changelog / compare lines are dropped.
pub fn tidy_post(text: &str, main_link: Option<&str>) -> String {
    let text = strip_images(text);
    // "[anything](release link)" → the plain link, so it shows a preview.
    let text = match main_link {
        Some(main) => unmask(&text, main),
        None => text,
    };
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            !(lower.contains("full changelog") || lower.contains("/compare/"))
        })
        .collect();
    let text = kept.join("\n");
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text.as_str();
    while let Some(at) = rest.find("http") {
        let (before, from) = rest.split_at(at);
        if !(from.starts_with("http://") || from.starts_with("https://")) {
            out.push_str(before);
            out.push_str("http");
            rest = &from[4..];
            continue;
        }
        let end = from
            .find(|c: char| c.is_whitespace() || matches!(c, '>' | ')' | '<' | '"' | '*' | '`'))
            .unwrap_or(from.len());
        let mut url = &from[..end];
        while url.ends_with(['.', ',', '!', '?', ':', ';']) {
            url = &url[..url.len() - 1];
        }
        let wrapped = before.ends_with('<') || before.ends_with("(<");
        let masked = before.ends_with("](");
        let main = main_link.is_some_and(|m| url.trim_end_matches('/') == m.trim_end_matches('/'));
        out.push_str(before);
        if wrapped || main {
            out.push_str(url);
        } else {
            out.push('<');
            out.push_str(url);
            out.push('>');
        }
        let _ = masked;
        rest = &from[url.len()..];
    }
    out.push_str(rest);
    out
}

/// Drop markdown images ("![badge](…)"): Discord shows them as raw text.
fn strip_images(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("![") {
        let after = &rest[at..];
        let Some(close) = after.find("](").and_then(|m| after[m..].find(')').map(|e| m + e + 1)) else {
            break;
        };
        out.push_str(&rest[..at]);
        rest = &after[close..];
    }
    out.push_str(rest);
    // A link that only wrapped the image ("[](url)") becomes the plain url.
    while let Some(at) = out.find("[](") {
        let Some(end) = out[at..].find(')').map(|e| at + e) else {
            break;
        };
        let url = out[at + 3..end].trim_matches(['<', '>']).to_owned();
        out.replace_range(at..=end, &url);
    }
    out
}

/// "[text](url)" pointing at `url` → just the url.
fn unmask(text: &str, url: &str) -> String {
    let mut out = text.to_owned();
    for target in [format!("]({url})"), format!("](<{url}>)")] {
        while let Some(end) = out.find(&target) {
            let Some(start) = out[..end].rfind('[') else {
                break;
            };
            out.replace_range(start..end + target.len(), url);
        }
    }
    out
}

/// A GitHub task's post as it goes out: tidied links, and for releases
/// the "# Repo version" heading (unless the instruction says no) with the
/// release link right under it.
pub fn finish_post(text: &str, input: Option<&str>, instruction: &str) -> String {
    let main = input.and_then(main_link);
    let mut post = tidy_post(text, main.as_deref());
    let heading = input.and_then(heading_line);
    // The model sometimes writes a preamble or echoes a chat-log line
    // ("[02:54] Magi (you): # Sue …") before the real post: start at the
    // heading.
    if let Some(at) = heading.as_deref().and_then(|h| post.find(h)).filter(|at| *at > 0) {
        post = post[at..].to_owned();
    }
    // Lines that are only some other link (often a garbled copy of the
    // release link) are noise.
    post = post
        .lines()
        .filter(|l| {
            let t = l.trim().trim_matches(['<', '>']);
            let only_link = (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(' ');
            !only_link || main.as_deref() == Some(t)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(heading) = heading {
        if !post.trim_start().starts_with('#') && !no_heading(instruction) {
            post = format!("{heading}\n{}", post.trim_start());
        }
    }
    if let Some(main) = main.filter(|m| !post.contains(m.as_str())) {
        post = match post.split_once('\n') {
            Some((first, rest)) if first.starts_with('#') => format!("{first}\n{main}\n{rest}"),
            _ => format!("{main}\n{post}"),
        };
    }
    post
}

/// The release link in a task input ("Link: …").
pub fn main_link(input: &str) -> Option<String> {
    input.lines().find_map(|l| l.strip_prefix("Link: ")).map(|l| l.trim().to_owned()).filter(|l| !l.is_empty())
}

/// New commits (newest first from GitHub) down to `last_seen`, as one
/// task input; None when there's nothing new.
fn new_commits(repo: &str, branch: &str, list: &[Value], last_seen: &str) -> (Option<String>, Option<String>) {
    let newest = list.first().and_then(|c| c["sha"].as_str()).map(str::to_owned);
    if newest.as_deref() == Some(last_seen) || newest.is_none() {
        return (None, newest);
    }
    let fresh: Vec<&Value> = list.iter().take_while(|c| c["sha"].as_str() != Some(last_seen)).take(15).collect();
    let mut lines: Vec<String> = fresh
        .iter()
        .rev()
        .map(|c| {
            let sha = c["sha"].as_str().unwrap_or("");
            let message = c["commit"]["message"].as_str().unwrap_or("").lines().next().unwrap_or("").to_owned();
            let author = c["author"]["login"].as_str().or(c["commit"]["author"]["name"].as_str()).unwrap_or("someone");
            format!("- {} {} — {} ({})", &sha[..sha.len().min(7)], message, author, c["html_url"].as_str().unwrap_or(""))
        })
        .collect();
    if fresh.len() == 15 {
        lines.insert(0, "(only the newest 15 shown)".to_owned());
    }
    let compare = newest.as_deref().map(|n| format!("https://github.com/{repo}/compare/{last_seen}...{n}")).unwrap_or_default();
    let text = format!(
        "New commits were just pushed to {repo} ({branch}):\n{}\nAll changes: {compare}",
        lines.join("\n")
    );
    (Some(text), newest)
}

/// Repos being checked right now (a check can take minutes while the
/// model loads; don't start another for the same task).
fn in_flight() -> &'static Mutex<std::collections::HashSet<(String, u64)>> {
    static SET: OnceLock<Mutex<std::collections::HashSet<(String, u64)>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Check every repo this agent watches (from the scheduler, each minute;
/// each repo every few minutes, each in the background).
pub async fn tick(core: &Arc<AgentCore>) {
    let token = core.hive.settings().github_token;
    for guild_id in core.world.guild_ids() {
        let now = now_ms();
        let due: Vec<StandingTask> = core.hive.knowledge.guild_mut(guild_id, |k| {
            k.tasks
                .iter_mut()
                .filter(|t| t.agent_id == core.id && t.active)
                .filter_map(|t| match &mut t.trigger {
                    TaskTrigger::GitHub { checked, .. } if now - *checked >= EVERY_MS => {
                        *checked = now;
                        Some(t.clone())
                    }
                    _ => None,
                })
                .collect()
        });
        for task in due {
            let key = (core.id.clone(), task.id);
            if !in_flight().lock().expect("in flight poisoned").insert(key.clone()) {
                continue;
            }
            let core = Arc::clone(core);
            let token = token.clone();
            tokio::spawn(async move {
                check(&core, guild_id, task, &token).await;
                in_flight().lock().expect("in flight poisoned").remove(&key);
            });
        }
    }
}

/// Save how far a watch got (and the ETag once everything is handled).
fn save_progress(core: &AgentCore, guild_id: u64, task_id: u64, seen: &str, etag: Option<&str>, posted: u32) {
    core.hive.knowledge.guild_mut(guild_id, |k| {
        if let Some(t) = k.tasks.iter_mut().find(|t| t.id == task_id) {
            if let TaskTrigger::GitHub { last_seen, etag: stored, .. } = &mut t.trigger {
                *last_seen = seen.to_owned();
                if let Some(etag) = etag {
                    *stored = etag.to_owned();
                }
            }
            if posted > 0 {
                t.runs += posted;
                t.last_run = now_ms();
            }
        }
    });
}

async fn check(core: &Arc<AgentCore>, guild_id: u64, task: StandingTask, token: &str) {
    let TaskTrigger::GitHub { channel_id, repo, kind, branch, last_seen, etag, .. } = task.trigger.clone() else {
        return;
    };
    let path = if kind == "commits" {
        format!("/repos/{repo}/commits?sha={branch}&per_page=30")
    } else {
        format!("/repos/{repo}/releases?per_page=10")
    };
    let (list, new_etag) = match api(&path, &etag, token).await {
        Ok(Fetched::NotModified) => return,
        Ok(Fetched::Body(Value::Array(list), new_etag)) => (list, new_etag),
        Ok(Fetched::Body(..)) => return,
        Err(error) => {
            tracing::debug!(%error, %repo, "github check failed");
            return;
        }
    };
    // First look: remember where things are, post nothing.
    if last_seen.is_empty() {
        let seen = if kind == "commits" {
            list.first().and_then(|c| c["sha"].as_str()).unwrap_or_default().to_owned()
        } else {
            newest_release(&list).to_string()
        };
        save_progress(core, guild_id, task.id, &seen, new_etag.as_deref(), 0);
        return;
    }
    // Each new thing in order; it only counts as handled once the post went
    // out (the model may still be loading — then it's retried next check,
    // and the ETag isn't kept so the same list comes back).
    let items: Vec<(String, String)> = if kind == "commits" {
        match new_commits(&repo, &branch, &list, &last_seen) {
            (Some(text), Some(newest)) => vec![(newest, text)],
            _ => Vec::new(),
        }
    } else {
        let last: u64 = last_seen.parse().unwrap_or(0);
        new_releases(&list, last, &repo).into_iter().map(|(id, text)| (id.to_string(), text)).collect()
    };
    if items.is_empty() {
        save_progress(core, guild_id, task.id, &last_seen, new_etag.as_deref(), 0);
        return;
    }
    let total = items.len();
    let mut handled = 0u32;
    let mut seen = last_seen.clone();
    for (id, input) in items {
        core.feed(
            crate::runtime::live::FeedKind::System,
            format!("new on GitHub: {repo} — running task #{}", task.id),
            None,
        );
        // Refresh what the model can take (LM Studio may have just loaded
        // it with a different context size).
        core.refresh_model_info().await;
        if !super::tasks::run_task_now(core, guild_id, channel_id, task.clone(), None, Some(input)).await {
            core.feed(
                crate::runtime::live::FeedKind::Error,
                format!("couldn't post the {repo} update yet — trying again in a few minutes"),
                None,
            );
            break;
        }
        handled += 1;
        seen = id;
        save_progress(core, guild_id, task.id, &seen, None, 1);
    }
    if handled as usize == total {
        save_progress(core, guild_id, task.id, &seen, new_etag.as_deref(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_release_link_embeds() {
        let post = "Sue v0.0.73 is out: https://github.com/MagiGun/sue/releases/tag/v0.0.73\n- New brush (see https://magigun.github.io/sue/).\nAlready quiet: <https://example.com/x>\n[docs](https://example.com/docs)\nFull changelog: https://github.com/MagiGun/sue/compare/v0.0.72...v0.0.73";
        let tidy = tidy_post(post, Some("https://github.com/MagiGun/sue/releases/tag/v0.0.73"));
        assert!(tidy.contains("out: https://github.com/MagiGun/sue/releases/tag/v0.0.73\n"), "{tidy}");
        assert!(tidy.contains("(see <https://magigun.github.io/sue/>)."), "{tidy}");
        assert!(tidy.contains("<https://example.com/x>") && !tidy.contains("<<"), "{tidy}");
        assert!(tidy.contains("[docs](<https://example.com/docs>)"), "{tidy}");
        assert!(!tidy.to_lowercase().contains("changelog") && !tidy.contains("/compare/"), "{tidy}");
        assert_eq!(main_link("Release: x\nLink: https://a/b\nTag: v"), Some("https://a/b".to_owned()));
        let badge = "# Sue v0.0.73\n\n[![Release](https://img.shields.io/badge/release-v0.0.73-blue)](https://github.com/MagiGun/sue/releases/tag/v0.0.73)\n\n- New brush";
        let tidy = tidy_post(badge, Some("https://github.com/MagiGun/sue/releases/tag/v0.0.73"));
        assert_eq!(tidy, "# Sue v0.0.73\n\nhttps://github.com/MagiGun/sue/releases/tag/v0.0.73\n\n- New brush");
        let masked = tidy_post("See [the release](https://a/rel) now", Some("https://a/rel"));
        assert_eq!(masked, "See https://a/rel now");
    }

    #[test]
    fn repos_from_links() {
        assert_eq!(parse_repo("https://github.com/rust-lang/rust/releases").as_deref(), Some("rust-lang/rust"));
        assert_eq!(parse_repo("github.com/ggml-org/llama.cpp").as_deref(), Some("ggml-org/llama.cpp"));
        assert_eq!(parse_repo("tokio-rs/tokio").as_deref(), Some("tokio-rs/tokio"));
        assert_eq!(parse_repo("https://github.com/owner/repo.git").as_deref(), Some("owner/repo"));
        assert_eq!(parse_repo("just words"), None);
    }

    #[test]
    fn only_new_things_count() {
        let releases = vec![
            json!({ "id": 30, "tag_name": "v3", "name": "Three", "html_url": "u3", "body": "big stuff" }),
            json!({ "id": 20, "tag_name": "v2", "html_url": "u2", "draft": true }),
            json!({ "id": 10, "tag_name": "v1", "html_url": "u1" }),
        ];
        let fresh = new_releases(&releases, 10, "o/r");
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].0, 30);
        assert!(fresh[0].1.contains("Three") && fresh[0].1.contains("big stuff"));
        assert_eq!(newest_release(&releases), 30);
        assert!(new_releases(&releases, 30, "o/r").is_empty());
        let with_log = vec![json!({ "id": 40, "tag_name": "v4", "html_url": "u4", "body": "- new brush\n\n**Full Changelog**: https://github.com/o/r/compare/v3...v4" })];
        let text = &new_releases(&with_log, 30, "MagiGun/sue")[0].1;
        assert_eq!(heading_line(text).as_deref(), Some("# Sue v4"));
        let named = vec![json!({ "id": 50, "tag_name": "v0.0.73", "name": "Sue v0.0.73", "html_url": "u5" })];
        assert_eq!(heading_line(&new_releases(&named, 30, "MagiGun/sue")[0].1).as_deref(), Some("# Sue v0.0.73"));
        assert!(no_heading("post the link, no heading please"));
        assert!(!no_heading("Post the release link and sum up the highlights."));
        assert!(text.contains("new brush") && !text.contains("/compare/"));

        let commits = vec![
            json!({ "sha": "ccc3333", "commit": { "message": "fix crash\n\nlong" }, "author": { "login": "ann" }, "html_url": "c3" }),
            json!({ "sha": "bbb2222", "commit": { "message": "add thing" }, "author": { "login": "bo" }, "html_url": "c2" }),
            json!({ "sha": "aaa1111", "commit": { "message": "old" }, "html_url": "c1" }),
        ];
        let (text, newest) = new_commits("o/r", "main", &commits, "aaa1111");
        let text = text.unwrap();
        assert!(text.find("add thing").unwrap() < text.find("fix crash").unwrap(), "oldest first");
        assert!(!text.contains("old ("));
        assert_eq!(newest.as_deref(), Some("ccc3333"));
        assert!(new_commits("o/r", "main", &commits, "ccc3333").0.is_none());
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Real GitHub (no token): repo lookup, new-release input, and that an
    /// unchanged check comes back "not modified".
    #[tokio::test]
    #[ignore]
    async fn lab_github_live() {
        let now = look("https://github.com/ggml-org/llama.cpp/releases", None, "").await.expect("look");
        println!("repo {} · branch {} · latest {:?} · commits {:?}", now.full_name, now.default_branch, now.latest_release, now.latest_commits);
        let path = format!("/repos/{}/releases?per_page=3", now.full_name);
        let Fetched::Body(Value::Array(list), etag) = api(&path, "", "").await.expect("releases") else { panic!() };
        let second = list[1]["id"].as_u64().unwrap();
        let inputs = new_releases(&list, second, &now.full_name);
        println!("\nnew since the one before: {}\n---\n{}\n---", inputs.len(), crate::llm::truncate(&inputs[0].1, 700));
        let etag = etag.expect("etag");
        match api(&path, &etag, "").await.expect("again") {
            Fetched::NotModified => println!("second check: not modified (doesn't count against the limit)"),
            Fetched::Body(..) => println!("second check: changed (or no ETag support)"),
        }
        let commits = format!("/repos/{}/commits?sha={}&per_page=5", now.full_name, now.default_branch);
        let Fetched::Body(Value::Array(list), _) = api(&commits, "", "").await.expect("commits") else { panic!() };
        let old = list[3]["sha"].as_str().unwrap().to_owned();
        let (text, _) = new_commits(&now.full_name, &now.default_branch, &list, &old);
        println!("\ncommits input:\n{}", crate::llm::truncate(&text.unwrap(), 900));
    }
}

#[cfg(test)]
mod finish {
    use super::*;

    #[test]
    fn posts_start_at_the_heading_with_one_link() {
        let input = "A new release was just published.\nHeading: # Sue v0.0.73\nLink: https://github.com/MagiGun/sue/releases/tag/v0.0.73\n";
        let echoed = "Here's the update for Sue v0.0.73:\n\n[02:54] Magi (you): # Sue v0.0.73\nhttps://github.com/MagiGun/sue/releases/tag/v0.0.73\n\n- New tools";
        let post = finish_post(echoed, Some(input), "Post the release link");
        assert!(post.starts_with("# Sue v0.0.73\nhttps://github.com/MagiGun/sue/releases/tag/v0.0.73"), "{post}");
        let garbled = "# Sue v0.0.73\nhttps://github.com/MagiGun/sue/releases/tag/v0.0.73\n<https://github.com/MagiGun/s/releases/tag/v0.0.73>\n\n- New tools";
        let post = finish_post(garbled, Some(input), "");
        assert!(!post.contains("MagiGun/s/"), "{post}");
        let plain = "- New tools";
        let post = finish_post(plain, Some(input), "");
        assert_eq!(post, "# Sue v0.0.73\nhttps://github.com/MagiGun/sue/releases/tag/v0.0.73\n- New tools");
        assert_eq!(finish_post(plain, Some(input), "no heading"), "https://github.com/MagiGun/sue/releases/tag/v0.0.73\n- New tools");
    }
}
