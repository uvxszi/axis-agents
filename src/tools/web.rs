//! Web search and page fetching.
//!
//! `fetch_url` is reachable by anyone who can talk to an agent, so it
//! refuses private, loopback, and link-local addresses by default (an
//! SSRF guard): a Discord user must not be able to make your agent read
//! your router's admin page.

use std::net::IpAddr;

use serde_json::Value;

use crate::config::{SearchConfig, SearchProvider};

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub async fn search(
    config: &SearchConfig,
    query: &str,
) -> Result<(Vec<SearchResult>, Option<String>), String> {
    let limit = config.max_results.clamp(1, 10) as usize;
    let client = crate::llm::http_client();
    let timeout = std::time::Duration::from_secs(20);
    match config.provider {
        SearchProvider::DuckDuckGo => {
            let html = client
                .post("https://html.duckduckgo.com/html/")
                .timeout(timeout)
                .header(
                    "User-Agent",
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AxisAgents",
                )
                .form(&[("q", query)])
                .send()
                .await
                .map_err(|e| format!("search failed: {e}"))?
                .text()
                .await
                .map_err(|e| e.to_string())?;
            let results = parse_duckduckgo(&html, limit);
            if results.is_empty() && html.contains("anomaly") {
                return Err("DuckDuckGo asked for a captcha; try again later or configure another search provider".to_owned());
            }
            Ok((results, None))
        }
        SearchProvider::Brave => {
            if config.api_key.trim().is_empty() {
                return Err("Brave Search needs an API key (Settings → Web)".to_owned());
            }
            let body: Value = client
                .get("https://api.search.brave.com/res/v1/web/search")
                .timeout(timeout)
                .header("X-Subscription-Token", config.api_key.trim())
                .header("Accept", "application/json")
                .query(&[("q", query), ("count", &limit.to_string())])
                .send()
                .await
                .map_err(|e| format!("search failed: {e}"))?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            let results = body
                .pointer("/web/results")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .take(limit)
                        .map(|r| SearchResult {
                            title: str_field(r, "title"),
                            url: str_field(r, "url"),
                            snippet: strip_tags(&str_field(r, "description")),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok((results, None))
        }
        SearchProvider::Tavily => {
            if config.api_key.trim().is_empty() {
                return Err("Tavily needs an API key (Settings → Web)".to_owned());
            }
            let body: Value = client
                .post("https://api.tavily.com/search")
                .timeout(timeout)
                .json(&serde_json::json!({
                    "api_key": config.api_key.trim(),
                    "query": query,
                    "max_results": limit,
                    "include_answer": true,
                }))
                .send()
                .await
                .map_err(|e| format!("search failed: {e}"))?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            let results = body
                .get("results")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|r| SearchResult {
                            title: str_field(r, "title"),
                            url: str_field(r, "url"),
                            snippet: str_field(r, "content"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let answer = body
                .get("answer")
                .and_then(Value::as_str)
                .filter(|a| !a.is_empty())
                .map(str::to_owned);
            Ok((results, answer))
        }
        SearchProvider::SearXng => {
            let base = config.searxng_url.trim().trim_end_matches('/');
            let body: Value = client
                .get(format!("{base}/search"))
                .timeout(timeout)
                .query(&[("q", query), ("format", "json")])
                .send()
                .await
                .map_err(|e| format!("search failed: {e}"))?
                .json()
                .await
                .map_err(|e| format!("SearXNG returned no JSON (enable the json format): {e}"))?;
            let results = body
                .get("results")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .take(limit)
                        .map(|r| SearchResult {
                            title: str_field(r, "title"),
                            url: str_field(r, "url"),
                            snippet: str_field(r, "content"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok((results, None))
        }
    }
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Parse DuckDuckGo's HTML results page.
pub fn parse_duckduckgo(html: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = html;
    while results.len() < limit {
        let Some(start) = rest.find("class=\"result__a\"") else {
            break;
        };
        let block = &rest[start..];
        let next = block[1..]
            .find("class=\"result__a\"")
            .map(|n| n + 1)
            .unwrap_or(block.len());
        let item = &block[..next];
        rest = &block[next..];

        let href = attribute_before(item, "href=\"").unwrap_or_default();
        let title = item
            .find('>')
            .and_then(|gt| {
                item[gt + 1..]
                    .find("</a>")
                    .map(|end| &item[gt + 1..gt + 1 + end])
            })
            .map(|t| decode_entities(&strip_tags(t)))
            .unwrap_or_default();
        let snippet = item
            .find("class=\"result__snippet\"")
            .and_then(|s| {
                let after = &item[s..];
                let gt = after.find('>')?;
                let end = after[gt + 1..]
                    .find("</a>")
                    .or_else(|| after[gt + 1..].find("</div>"))?;
                Some(decode_entities(&strip_tags(&after[gt + 1..gt + 1 + end])))
            })
            .unwrap_or_default();
        let url = resolve_ddg_link(&href);
        if !url.is_empty() && !title.is_empty() && !url.contains("duckduckgo.com/y.js") {
            results.push(SearchResult {
                title: title.trim().to_owned(),
                url,
                snippet: snippet.trim().to_owned(),
            });
        }
    }
    results
}

fn attribute_before(text: &str, key: &str) -> Option<String> {
    let start = text.find(key)? + key.len();
    let end = text[start..].find('"')?;
    Some(decode_entities(&text[start..start + end]))
}

fn resolve_ddg_link(href: &str) -> String {
    let href = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    };
    if let Some(position) = href.find("uddg=") {
        let encoded = &href[position + 5..];
        let encoded = encoded.split('&').next().unwrap_or(encoded);
        return percent_decode(encoded);
    }
    href
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';').filter(|s| *s <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" | "#x27" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "rsquo" | "lsquo" => Some('\''),
            "rdquo" | "ldquo" => Some('"'),
            _ => entity
                .strip_prefix("#x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|n| n.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Readable text from an HTML page: drops scripts/styles/nav chrome,
/// keeps paragraph breaks.
pub fn html_to_text(html: &str) -> (Option<String>, String) {
    let lower = html.to_lowercase();
    let title = lower.find("<title").and_then(|start| {
        let open_end = lower[start..].find('>')? + start + 1;
        let close = lower[open_end..].find("</title>")? + open_end;
        Some(decode_entities(html[open_end..close].trim()))
    });
    let mut cleaned = String::with_capacity(html.len());
    let mut i = 0;
    let skip_tags = [
        "script", "style", "noscript", "svg", "head", "nav", "footer", "form",
    ];
    while i < html.len() {
        if lower[i..].starts_with('<') {
            if let Some(tag) = skip_tags.iter().find(|tag| {
                lower[i + 1..].starts_with(*tag)
                    && lower[i + 1 + tag.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == '>' || c.is_whitespace())
            }) {
                let close = format!("</{tag}");
                if let Some(end) = lower[i..].find(&close) {
                    let end = i + end;
                    i = lower[end..]
                        .find('>')
                        .map(|gt| end + gt + 1)
                        .unwrap_or(html.len());
                    continue;
                }
            }
            let block = [
                "<br", "<p", "</p", "<div", "</div", "<li", "<h1", "<h2", "<h3", "<h4", "<tr",
                "</h",
            ];
            if block.iter().any(|b| lower[i..].starts_with(b)) {
                cleaned.push('\n');
            }
            if lower[i..].starts_with("<li") {
                cleaned.push_str("• ");
            }
            match lower[i..].find('>') {
                Some(gt) => i += gt + 1,
                None => break,
            }
            continue;
        }
        let next = lower[i..].find('<').map(|n| i + n).unwrap_or(html.len());
        cleaned.push_str(&html[i..next]);
        i = next;
    }
    let text = decode_entities(&cleaned);
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(&line);
            out.push('\n');
        }
    }
    (title, out.trim().to_owned())
}

pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64 // CGNAT
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(IpAddr::V4(v4)))
        }
    }
}

async fn check_target(url: &reqwest::Url, allow_private: bool) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only http and https URLs can be fetched".to_owned());
    }
    if allow_private {
        return Ok(());
    }
    let host = url.host_str().ok_or("the URL has no host")?;
    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return Err("refusing to fetch a local address".to_owned());
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addresses = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|e| format!("could not resolve {host}: {e}"))?;
    for address in addresses {
        if is_private_ip(address.ip()) {
            return Err("refusing to fetch a private network address".to_owned());
        }
    }
    Ok(())
}

/// Whether a URL is a reachable, public image right now (what Discord
/// needs to embed it). Reads only the first bytes.
pub async fn is_live_image(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    if check_target(&parsed, false).await.is_err() {
        return false;
    }
    let client = crate::llm::http_client();
    let request = client
        .get(parsed)
        .header("Range", "bytes=0-2047")
        .header(
            "User-Agent",
            "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
        )
        .timeout(std::time::Duration::from_secs(8));
    match request.send().await {
        Ok(response) => {
            let ok = response.status().is_success();
            let image = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| t.starts_with("image/"));
            ok && image
        }
        Err(_) => false,
    }
}

/// Download raw bytes (images…) with the same SSRF guard as `fetch`.
pub async fn fetch_bytes(
    url: &str,
    allow_private: bool,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let mut current = reqwest::Url::parse(url.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (compatible; AxisAgents/0.2)")
        .build()
        .map_err(|e| e.to_string())?;
    for _hop in 0..5 {
        check_target(&current, allow_private).await?;
        let reply = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        if reply.status().is_redirection() {
            let location = reply
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without a location")?;
            current = current.join(location).map_err(|e| e.to_string())?;
            continue;
        }
        if !reply.status().is_success() {
            return Err(format!("the server answered HTTP {}", reply.status()));
        }
        use futures_util::StreamExt;
        let mut stream = reply.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
            if body.len() > max_bytes {
                return Err("the file is too large".to_owned());
            }
        }
        return Ok(body);
    }
    Err("too many redirects".to_owned())
}

pub async fn fetch(url: &str, allow_private: bool, max_chars: usize) -> Result<String, String> {
    let mut current = reqwest::Url::parse(url.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (compatible; AxisAgents/0.2; +https://discord.com)")
        .build()
        .map_err(|e| e.to_string())?;
    // Follow redirects manually so every hop passes the SSRF check.
    let mut response = None;
    for _hop in 0..5 {
        check_target(&current, allow_private).await?;
        let reply = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("fetch failed: {e}"))?;
        if reply.status().is_redirection() {
            let location = reply
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without a location")?;
            current = current.join(location).map_err(|e| e.to_string())?;
            continue;
        }
        response = Some(reply);
        break;
    }
    let response = response.ok_or("too many redirects")?;
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !status.is_success() {
        return Err(format!("the site answered HTTP {status}"));
    }
    if !(content_type.is_empty()
        || content_type.contains("text")
        || content_type.contains("html")
        || content_type.contains("json")
        || content_type.contains("xml"))
    {
        return Err(format!("not a text page ({content_type})"));
    }
    // Read at most ~2 MB.
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        body.extend_from_slice(&chunk);
        if body.len() > 2_000_000 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&body);
    let (title, content) = if content_type.contains("html") || text.trim_start().starts_with('<') {
        html_to_text(&text)
    } else {
        (None, text.into_owned())
    };
    let mut out = String::new();
    if let Some(title) = title {
        out.push_str(&format!("Title: {title}\n"));
    }
    out.push_str(&format!("URL: {current}\n\n"));
    out.push_str(&crate::llm::truncate(&content, max_chars));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_are_detected() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn fetch_refuses_localhost() {
        let error = fetch("http://localhost:1234/v1/models", false, 1000)
            .await
            .unwrap_err();
        assert!(error.contains("local"), "{error}");
        let error = fetch("http://127.0.0.1/", false, 1000).await.unwrap_err();
        assert!(error.contains("private"), "{error}");
        let error = fetch("file:///etc/passwd", false, 1000).await.unwrap_err();
        assert!(error.contains("http"), "{error}");
    }

    #[test]
    fn html_is_cleaned() {
        let (title, text) = html_to_text(
            "<html><head><title>Hi &amp; bye</title><style>x{}</style></head><body><nav>menu</nav>\
             <p>First&nbsp;para</p><script>alert(1)</script><ul><li>one</li><li>two</li></ul></body></html>",
        );
        assert_eq!(title.as_deref(), Some("Hi & bye"));
        assert!(text.contains("First para"));
        assert!(text.contains("• one"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("menu"));
    }

    #[test]
    fn duckduckgo_results_parse() {
        let html = r#"<div><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=x">Rust <b>Programming</b> Language</a>
            <a class="result__snippet" href="x">A language empowering <b>everyone</b>.</a></div>
            <div><a rel="nofollow" class="result__a" href="https://example.com/">Example</a></div>"#;
        let results = parse_duckduckgo(html, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].snippet, "A language empowering everyone.");
        assert_eq!(results[1].url, "https://example.com/");
    }

    #[test]
    fn entities_and_percent_decoding() {
        assert_eq!(
            decode_entities("a &lt;b&gt; &#65;&#x42; &bogus"),
            "a <b> AB &bogus"
        );
        assert_eq!(percent_decode("a%20b%2Fc+d%"), "a b/c d%");
    }
}
