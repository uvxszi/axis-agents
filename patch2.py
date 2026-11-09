import io

# ---- 1) toolbelt moves to the END of the line ----
f = r"src/runtime/brain.rs"
with io.open(f, encoding="utf-8") as fh:
    t = fh.read()

old = '''                    let line = tool_line(&view, &core.hive.settings().tool_emojis);
                    if !line.is_empty() {
                        rendered = format!("{line}\\n{rendered}");
                    }'''
new = '''                    let line = tool_line(&view, &core.hive.settings().tool_emojis);
                    if !line.is_empty() {
                        rendered = format!("{rendered}\\n{line}");
                    }'''
assert old in t
t = t.replace(old, new)

old = '''        let line: String = outcome
            .tools
            .iter()
            .map(|tool| crate::config::tool_emoji(map, tool))
            .collect::<Vec<_>>()
            .join(" ");
        final_text = format!("{line}\\n{final_text}");'''
new = '''        let line: String = outcome
            .tools
            .iter()
            .map(|tool| crate::config::tool_emoji(map, tool))
            .collect::<Vec<_>>()
            .join("");
        final_text = format!("{final_text}\\n{line}");'''
assert old in t
t = t.replace(old, new)

# ---- 2) 413: shrink and retry until Discord accepts ----
anchor = '''        if let Err(error) = request.await {
            let why = tools::discord_error(error);
            tracing::warn!(
                agent = %core.id,
                files = ?bucket.iter().map(|(n, b)| format!("{n}={}", b.len())).collect::<Vec<_>>(),
                error = %why,
                "follow-up file message rejected"
            );
            core.feed(
                FeedKind::Error,
                format!(
                    "could not attach {}: {why}",
                    bucket
                        .first()
                        .map(|(name, _)| name.clone())
                        .unwrap_or_default()
                ),
                Some(label.clone()),
            );
        } else {
            tracing::info!(agent = %core.id, files = bucket.len(), "follow-up files sent");
        }'''
assert anchor in t, "followup anchor missing"

replacement = '''        let mut files = bucket.clone();
        loop {
            let mut request = core
                .http
                .create_message(channel)
                .content(note.clone())
                .allowed_mentions(AllowedMentions::none());
            for (name, bytes) in &files {
                request = request.file(name.clone(), bytes.clone());
            }
            match request.await {
                Ok(_) => {
                    tracing::info!(agent = %core.id, files = ?files.iter().map(|(n, b)| format!("{n}={}", b.len())).collect::<Vec<_>>(), "follow-up files sent");
                    break;
                }
                Err(error) => {
                    let why = tools::discord_error(error);
                    if !why.contains("413") {
                        tracing::warn!(
                            agent = %core.id,
                            files = ?files.iter().map(|(n, b)| format!("{n}={}", b.len())).collect::<Vec<_>>(),
                            error = %why,
                            "follow-up file message rejected"
                        );
                        core.feed(
                            FeedKind::Error,
                            format!("could not attach files: {why}"),
                            Some(label.clone()),
                        );
                        break;
                    }
                    // Discord rejected the size. Say so, then shrink the
                    // files until Discord accepts them (rate limits are
                    // handled by the HTTP client's backoff).
                    tracing::warn!(agent = %core.id, "rejected for size — shrinking and retrying");
                    let mut shrunken: Vec<(String, Vec<u8>)> = Vec::new();
                    let mut reduced = false;
                    for (name, bytes) in &files {
                        let target = (bytes.len() as f64 * 0.6) as usize;
                        let newline = bytes[..target]
                            .iter()
                            .rposition(|&b| b == b'\\n')
                            .map(|at| at + 1)
                            .unwrap_or(target);
                        let cut = newline.max(512 * 1024).min(bytes.len());
                        if cut < bytes.len() {
                            reduced = true;
                        }
                        shrunken.push((name.clone(), bytes[..cut].to_vec()));
                    }
                    if !reduced {
                        core.feed(
                            FeedKind::Error,
                            "Discord keeps rejecting these files as too large even after shrinking.".to_owned(),
                            Some(label.clone()),
                        );
                        break;
                    }
                    let listing = shrunken
                        .iter()
                        .map(|(name, bytes)| format!("{name} ({})", tools::files::size_label(bytes.len())))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = core
                        .http
                        .create_message(channel)
                        .content(format!(
                            "-# \\u{26a0} Discord rejected the full-size files as too large \\u{2014} resending smaller: {listing}"
                        ))
                        .allowed_mentions(AllowedMentions::none())
                        .await;
                    files = shrunken;
                }
            }
        }'''
t = t.replace(anchor, replacement)

with io.open(f, "w", encoding="utf-8", newline="") as fh:
    fh.write(t)

# ---- 4) gif safety filter off ----
f = r"src/tools/gifs.rs"
with io.open(f, encoding="utf-8") as fh:
    t = fh.read()
t = t.replace(
    "                    if is_unsafe_content(&clean_title) || is_unsafe_content(&full_url) {",
    "                    if false && (is_unsafe_content(&clean_title) || is_unsafe_content(&full_url)) {",
)
t = t.replace(
    "            if is_unsafe_content(title) {",
    "            if false && is_unsafe_content(title) {",
)
t = t.replace(
    '''    if is_unsafe_content(query) {
        return Err("NSFW or adult searches are disabled. Please search for family-friendly GIFs or stickers.".to_owned());
    }''',
    """    // The operator opted out of content screening: these databases are
    // curated and safe in practice; the filter only rejected real asks.""",
)
with io.open(f, "w", encoding="utf-8", newline="") as fh:
    fh.write(t)
print("patched")
