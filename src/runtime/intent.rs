//! One parser for what a request asks for.
//!
//! The app performs the mechanics of an ask (exact sizes, attachments,
//! copy-paste fills) — but it has to know WHAT was asked. That reading
//! happens here, once per job, in one place: every consumer (write_file,
//! duplicate_file, the brain's nudges) consults the same `RequestIntent`
//! instead of each running its own keyword matching.

/// What the person's message asks of the agent, as the app reads it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestIntent {
    /// The LAST size mentioned in the message — that one is the ask
    /// ("first a 1 MB draft then finish at 2 MB" → 2 MB).
    pub final_size: Option<usize>,
    /// The message wants the result attached/sent with the reply.
    pub wants_attachment: bool,
    /// The message describes copying/duplication to reach the goal
    /// ("copy-paste it until…", "repeat it…", "fill to…", "over and over").
    pub copy_fill: bool,
    /// File names the message mentions ("tale.md", "notes.txt").
    pub file_names: Vec<String>,
}

/// Pull a file-size ask out of a message: "a 25 MB file", "1mb",
/// "500 KB of notes", "100000 bytes". Sizes are binary (MB = 1024²) —
/// matching the upload cap.
fn size_mentions(text: &str) -> Vec<usize> {
    let unit_of = |word: &str| -> Option<u64> {
        match word.trim_matches(|c: char| !c.is_ascii_alphanumeric()) {
            "kb" | "kilobyte" | "kilobytes" => Some(1024),
            "mb" | "megabyte" | "megabytes" => Some(1024 * 1024),
            "gb" | "gigabyte" | "gigabytes" => Some(1024 * 1024 * 1024),
            "bytes" | "byte" => Some(1),
            _ => None,
        }
    };
    let mut found: Vec<usize> = Vec::new();
    let mut pending: Option<f64> = None;
    for word in text.to_lowercase().split_whitespace() {
        let cleaned = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
        // Fused form: "25mb".
        if let Some(at) = cleaned.find(|c: char| c.is_ascii_alphabetic()) {
            let (num, unit) = cleaned.split_at(at);
            if let (Ok(n), Some(mult)) = (num.trim().parse::<f64>(), unit_of(unit)) {
                found.push((n * mult as f64) as usize);
                pending = None;
                continue;
            }
        }
        if let Ok(n) = cleaned.parse::<f64>() {
            pending = Some(n);
        } else if let (Some(n), Some(mult)) = (pending, unit_of(cleaned)) {
            found.push((n * mult as f64) as usize);
            pending = None;
        } else {
            pending = None;
        }
    }
    found.into_iter().filter(|&bytes| bytes > 0).collect()
}

/// File names the text mentions: anything word-like with a known
/// extension ("save it as tale.md").
fn file_names(text: &str) -> Vec<String> {
    const EXTENSIONS: [&str; 10] = [
        "md", "txt", "csv", "json", "py", "log", "png", "jpg", "jpeg", "webp",
    ];
    let mut names = Vec::new();
    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-');
        if let Some((stem, ext)) = word.rsplit_once('.') {
            if !stem.is_empty()
                && stem.len() <= 64
                && EXTENSIONS.contains(&ext.to_lowercase().as_str())
                && !names.iter().any(|n: &String| n.eq_ignore_ascii_case(word))
            {
                names.push(word.to_owned());
            }
        }
    }
    names
}

/// Parse a request message once. All keyword reading lives here so it can
/// be tuned, tested, and trusted in one place.
pub fn parse(text: &str) -> RequestIntent {
    let lower = text.to_lowercase();
    let final_size = size_mentions(&lower).pop();
    let wants_attachment =
        (lower.contains("attach") || lower.contains("send it") || lower.contains("send the file"))
            && !lower.contains("don't attach")
            && !lower.contains("do not attach");
    let copy_fill = (lower.contains("copy") && lower.contains("paste"))
        || lower.contains("copy-paste")
        || lower.contains("copy paste")
        || lower.contains("duplicate")
        || lower.contains("repeat it")
        || lower.contains("repeat the")
        || lower.contains("fill it to")
        || lower.contains("fill to")
        || lower.contains("fill it up")
        || lower.contains("over and over")
        || lower.contains("again and again")
        || lower.contains("multiple copies")
        || lower.contains("several copies");
    RequestIntent {
        final_size,
        wants_attachment,
        copy_fill,
        file_names: file_names(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_and_last_wins() {
        assert_eq!(
            parse("write a story and copy it until the file is 25 MB").final_size,
            Some(25 * 1024 * 1024)
        );
        assert_eq!(parse("make a 1mb file").final_size, Some(1024 * 1024));
        assert_eq!(parse("about 500 KB of notes").final_size, Some(500 * 1024));
        assert_eq!(parse("100000 bytes please").final_size, Some(100_000));
        assert_eq!(parse("no size mentioned here").final_size, None);
        assert_eq!(
            parse("first a 1 MB draft then finish at 2 MB").final_size,
            Some(2 * 1024 * 1024),
            "the last mention is the ask"
        );
    }

    #[test]
    fn copy_fill_and_attachment_read() {
        assert!(parse("write tale.md then copy-paste it until 1 MB").copy_fill);
        assert!(parse("duplicate the story until the file is big").copy_fill);
        assert!(parse("repeat the tips until 500 KB").copy_fill);
        assert!(!parse("write a 60 KB story with chapters").copy_fill);
        assert!(parse("write notes.txt and attach it").wants_attachment);
        assert!(parse("make a file and send it to me").wants_attachment);
        assert!(!parse("write a story about the sea").wants_attachment);
        assert!(!parse("write it but don't attach the file").wants_attachment);
    }

    #[test]
    fn file_names_are_picked_up() {
        let intent = parse("save the tips as notes.txt and the story as tale.md");
        assert!(intent.file_names.iter().any(|n| n == "notes.txt"));
        assert!(intent.file_names.iter().any(|n| n == "tale.md"));
        assert!(parse("no files here").file_names.is_empty());
    }
}
