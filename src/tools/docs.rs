//! Reading big things a piece at a time. A long tool result (a web page,
//! a transcript, code) becomes a document with an outline — sections by
//! headings, by functions and types for code, or by paragraph blocks —
//! and the agent opens one section at a time. A section that's still too
//! big opens as its own, finer outline, as deep as needed.

use std::sync::Mutex;

/// Longer results than this become documents.
pub const LONG: usize = 6_000;
/// A section shown in full is at most this long.
const SECTION_MAX: usize = 4_500;
const KEEP: usize = 24;

#[derive(Debug, Clone)]
pub struct Document {
    pub id: String,
    pub title: String,
    pub text: String,
}

#[derive(Default)]
pub struct Library {
    docs: Mutex<Vec<Document>>,
    drafts: Mutex<Vec<Draft>>,
    next: Mutex<u32>,
}

impl Library {
    pub fn add(&self, title: &str, text: String) -> Document {
        let id = {
            let mut next = self.next.lock().expect("docs poisoned");
            *next += 1;
            format!("doc{}", *next)
        };
        let doc = Document {
            id,
            title: title.to_owned(),
            text,
        };
        let mut docs = self.docs.lock().expect("docs poisoned");
        docs.push(doc.clone());
        if docs.len() > KEEP {
            docs.remove(0);
        }
        doc
    }

    pub fn get(&self, id: &str) -> Option<Document> {
        let id = id.trim().trim_start_matches('#').to_lowercase();
        let id = if id.starts_with("doc") {
            id
        } else {
            format!("doc{id}")
        };
        self.docs
            .lock()
            .expect("docs poisoned")
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }
}

/// A part of a document: a line range and a title.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub title: String,
    pub start: usize,
    pub end: usize,
}

fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn heading_level(line: &str) -> Option<usize> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    (hashes > 0 && hashes <= 6 && t[hashes..].starts_with(' ')).then_some(hashes)
}

/// Does a line start a definition (function, type, …) in common languages?
fn definition(line: &str) -> bool {
    let t = line.trim_start();
    const STARTS: [&str; 48] = [
        "pub const fn ",
        "pub const unsafe fn ",
        "pub unsafe fn ",
        "unsafe fn ",
        "const fn ",
        "pub(crate) const fn ",
        "pub(super) fn ",
        "pub(crate) async fn ",
        "fn ",
        "pub fn ",
        "pub(crate) fn ",
        "async fn ",
        "pub async fn ",
        "impl ",
        "impl<",
        "struct ",
        "pub struct ",
        "enum ",
        "pub enum ",
        "trait ",
        "pub trait ",
        "mod ",
        "pub mod ",
        "def ",
        "async def ",
        "class ",
        "function ",
        "async function ",
        "export function ",
        "export default ",
        "export class ",
        "export const ",
        "export async ",
        "interface ",
        "export interface ",
        "type ",
        "func ",
        "public ",
        "private ",
        "protected ",
        "static ",
        "const ",
        "let ",
        "var ",
        "macro_rules!",
        "#[test]",
        "@app.",
        "describe(",
    ];
    STARTS.iter().any(|s| t.starts_with(s))
}

fn title_of(line: &str) -> String {
    crate::llm::truncate(
        line.trim()
            .trim_start_matches('#')
            .trim()
            .trim_end_matches('{')
            .trim(),
        70,
    )
}

/// Split lines [start, end) into parts; very many parts are grouped into
/// runs (a huge impl with 100 methods → groups of ~20).
pub fn split(lines: &[&str], start: usize, end: usize) -> Vec<Part> {
    let parts = split_once(lines, start, end);
    if parts.len() <= 30 {
        return parts;
    }
    let per = parts.len().div_ceil(parts.len().div_ceil(20));
    parts
        .chunks(per)
        .map(|run| Part {
            title: format!("{} … {}", run[0].title, run[run.len() - 1].title),
            start: run[0].start,
            end: run[run.len() - 1].end,
        })
        .collect()
}

/// Split lines [start, end) into parts, using the coarsest structure that
/// gives more than one part.
fn split_once(lines: &[&str], start: usize, end: usize) -> Vec<Part> {
    let range = &lines[start..end];
    let make = |cuts: Vec<usize>, titles: &dyn Fn(usize) -> String| -> Vec<Part> {
        let mut parts = Vec::new();
        let mut cuts = cuts;
        if cuts.first() != Some(&0) {
            cuts.insert(0, 0);
        }
        for (i, cut) in cuts.iter().enumerate() {
            let next = cuts.get(i + 1).copied().unwrap_or(range.len());
            if next > *cut {
                parts.push(Part {
                    title: titles(*cut),
                    start: start + cut,
                    end: start + next,
                });
            }
        }
        parts
    };
    // Markdown headings: the shallowest level that actually divides it
    // (a lone "# Title" on top doesn't).
    let mut levels: Vec<usize> = range.iter().filter_map(|l| heading_level(l)).collect();
    levels.sort_unstable();
    levels.dedup();
    for level in levels {
        let cuts: Vec<usize> = range
            .iter()
            .enumerate()
            .filter(|(_, l)| heading_level(l) == Some(level))
            .map(|(i, _)| i)
            .collect();
        if cuts.len() > 1 || (cuts.len() == 1 && cuts[0] > 0) {
            return make(cuts, &|i| {
                if heading_level(range[i]).is_some() {
                    title_of(range[i])
                } else {
                    "(start)".to_owned()
                }
            });
        }
    }
    // Code: definitions at the shallowest indentation they appear at.
    let defs: Vec<(usize, usize)> = range
        .iter()
        .enumerate()
        .filter(|(_, l)| definition(l))
        .map(|(i, l)| (i, indent(l)))
        .collect();
    if let Some(min) = defs.iter().map(|(_, d)| *d).min() {
        let cuts: Vec<usize> = defs
            .iter()
            .filter(|(_, d)| *d == min)
            .map(|(i, _)| *i)
            .collect();
        // Skip the definition that opens this very range (we're inside it).
        let cuts: Vec<usize> = cuts
            .into_iter()
            .filter(|c| *c > 0 || defs.len() > 1)
            .collect();
        if cuts.len() > 1 || (cuts.len() == 1 && cuts[0] > 0) {
            return make(cuts, &|i| {
                if definition(range[i]) {
                    title_of(range[i])
                } else {
                    "(top of section)".to_owned()
                }
            });
        }
    }
    // Blocks separated by blank lines, grouped to a sensible size.
    let mut cuts = vec![0];
    let mut size = 0;
    for (i, line) in range.iter().enumerate() {
        size += line.len() + 1;
        if line.trim().is_empty() && size > SECTION_MAX / 3 && i + 1 < range.len() {
            cuts.push(i + 1);
            size = 0;
        }
    }
    if cuts.len() > 1 {
        return make(cuts, &|i| {
            title_of(
                range[i..]
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .copied()
                    .unwrap_or(""),
            )
        });
    }
    // Last resort: fixed line chunks.
    let chunk = (range.len() / 6).max(10);
    let cuts: Vec<usize> = (0..range.len()).step_by(chunk).collect();
    make(cuts, &|i| {
        title_of(
            range[i..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .copied()
                .unwrap_or(""),
        )
    })
}

fn size(lines: &[&str], part: &Part) -> usize {
    lines[part.start..part.end]
        .iter()
        .map(|l| l.len() + 1)
        .sum()
}

fn outline(doc: &Document, lines: &[&str], parts: &[Part], prefix: &str) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        let id = if prefix.is_empty() {
            (i + 1).to_string()
        } else {
            format!("{prefix}.{}", i + 1)
        };
        let chars = size(lines, part);
        out.push_str(&format!(
            "  {id}. {} — lines {}-{}, {}{}\n",
            if part.title.is_empty() {
                "(untitled)"
            } else {
                &part.title
            },
            part.start + 1,
            part.end,
            if chars >= 1000 {
                format!("{:.1}k chars", chars as f32 / 1000.0)
            } else {
                format!("{chars} chars")
            },
            if chars > SECTION_MAX {
                " (opens as a finer outline)"
            } else {
                ""
            }
        ));
    }
    let _ = doc;
    out
}

/// Turn a long result into a document and return what the agent sees:
/// a short preview and the outline.
pub fn store(library: &Library, title: &str, text: &str) -> String {
    let doc = library.add(title, text.to_owned());
    let lines: Vec<&str> = doc.text.lines().collect();
    let parts = split(&lines, 0, lines.len());
    let preview: String = doc.text.chars().take(500).collect();
    format!(
        "This is long ({} chars, {} lines), so it's saved as document {} — read it a section at a time with read_section (document: {}, section: a number from the outline). \
         Read only the sections you need (find_in_document finds where a name appears); big sections open into finer outlines.\n\nStart:\n{preview}…\n\nOutline:\n{}",
        doc.text.len(),
        lines.len(),
        doc.id,
        doc.id,
        outline(&doc, &lines, &parts, "")
    )
}

/// Where text appears in a document: matching lines with the section
/// path that holds each (deepest that's small enough to read).
pub fn find(library: &Library, doc_id: &str, needle: &str) -> Result<String, String> {
    let doc = library
        .get(doc_id)
        .ok_or_else(|| format!("no document {doc_id}"))?;
    let lines: Vec<&str> = doc.text.lines().collect();
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return Err("say what to look for".to_owned());
    }
    let mut hits = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.to_lowercase().contains(&needle) {
            continue;
        }
        // Walk down the outline to the section holding this line.
        let mut path = Vec::new();
        let (mut start, mut end) = (0, lines.len());
        loop {
            let parts = split(&lines, start, end);
            let Some((i, part)) = parts
                .iter()
                .enumerate()
                .find(|(_, p)| p.start <= index && index < p.end)
            else {
                break;
            };
            path.push((i + 1).to_string());
            if size(&lines, part) <= SECTION_MAX
                || parts.len() <= 1
                || part.end - part.start == end - start
            {
                break;
            }
            start = part.start;
            end = part.end;
        }
        hits.push(format!(
            "line {} (section {}): {}",
            index + 1,
            path.join("."),
            crate::llm::truncate(line.trim(), 120)
        ));
        if hits.len() >= 15 {
            break;
        }
    }
    Ok(if hits.is_empty() {
        format!("\"{needle}\" doesn't appear in {}", doc.id)
    } else {
        format!(
            "\"{needle}\" in {}:
{}
Open a section with read_section.",
            doc.id,
            hits.join(
                "
"
            )
        )
    })
}

/// Open a section ("3", "3.2", …): its text with line numbers, or — when
/// it's still too big — its own outline.
pub fn read(library: &Library, doc_id: &str, section: &str) -> Result<String, String> {
    let doc = library.get(doc_id).ok_or_else(|| {
        format!("no document {doc_id} (they're kept for a while; fetch it again)")
    })?;
    let lines: Vec<&str> = doc.text.lines().collect();
    let mut parts = split(&lines, 0, lines.len());
    let mut current: Option<Part> = None;
    let mut path = String::new();
    let section = section.trim().trim_matches('"');
    if !section.is_empty() && section != "0" && !section.eq_ignore_ascii_case("all") {
        for step in section.split('.') {
            let index: usize = step
                .trim()
                .parse()
                .map_err(|_| format!("sections are numbers like 2 or 2.3, not {section}"))?;
            let part = parts.get(index.wrapping_sub(1)).cloned().ok_or_else(|| {
                format!(
                    "there's no section {section} — the outline has {} parts at that level",
                    parts.len()
                )
            })?;
            path = if path.is_empty() {
                index.to_string()
            } else {
                format!("{path}.{index}")
            };
            parts = split(&lines, part.start, part.end);
            current = Some(part);
        }
    }
    let part = current.unwrap_or(Part {
        title: doc.title.clone(),
        start: 0,
        end: lines.len(),
    });
    if size(&lines, &part) <= SECTION_MAX || part.end - part.start <= 3 {
        let width = part.end.to_string().len();
        let body: Vec<String> = lines[part.start..part.end]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>width$}| {l}", part.start + i + 1))
            .collect();
        return Ok(format!(
            "{} section {} — {} (lines {}-{}):\n{}",
            doc.id,
            if path.is_empty() {
                "all".to_owned()
            } else {
                path.clone()
            },
            part.title,
            part.start + 1,
            part.end,
            crate::llm::truncate(&body.join("\n"), SECTION_MAX + 1500)
        ));
    }
    Ok(format!(
        "{} section {} — {} is still big ({} chars). Its parts:\n{}Open one with read_section (section: {}.N).",
        doc.id,
        if path.is_empty() {
            "all".to_owned()
        } else {
            path.clone()
        },
        part.title,
        size(&lines, &part),
        outline(&doc, &lines, &parts, &path),
        if path.is_empty() {
            "N".to_owned()
        } else {
            path
        }
    ))
}

// ---------------------------------------------------------------------------
// Writing big files
// ---------------------------------------------------------------------------

/// A file being written a section at a time.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub id: String,
    pub filename: String,
    pub created: i64,
    pub published: bool,
    /// (section title, content) in order.
    pub sections: Vec<(String, String)>,
}

impl Draft {
    pub fn assemble(&self) -> String {
        let mut out = String::new();
        for (_, content) in &self.sections {
            if content.is_empty() {
                continue;
            }
            out.push_str(content.trim_end_matches('\n'));
            out.push_str("\n\n");
        }
        out.trim_end().to_owned() + "\n"
    }

    pub fn overview(&self) -> String {
        let total = self.assemble().len();
        let lines: Vec<String> = self
            .sections
            .iter()
            .enumerate()
            .map(|(i, (title, content))| {
                format!(
                    "  {}. {} — {}",
                    i + 1,
                    title,
                    if content.is_empty() {
                        "☐ not written".to_owned()
                    } else {
                        format!(
                            "✓ {} lines, {} chars",
                            content.lines().count(),
                            content.len()
                        )
                    }
                )
            })
            .collect();
        format!(
            "{} ({}, {} chars so far):\n{}",
            self.id,
            self.filename,
            total,
            lines.join("\n")
        )
    }
}

impl Library {
    /// Drafts started since a time, as a checklist (kept in view across
    /// long jobs).
    pub fn drafts_since(&self, since: i64) -> Vec<String> {
        self.drafts
            .lock()
            .expect("docs poisoned")
            .iter()
            .filter(|d| d.created >= since)
            .map(|d| {
                format!(
                    "{}{}",
                    d.overview(),
                    if d.published { " — published" } else { "" }
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_splits_by_function_then_deeper() {
        let mut code = String::from("use std::io;\n\n");
        for f in 0..4 {
            code.push_str(&format!("fn function_{f}() {{\n"));
            for i in 0..80 {
                code.push_str(&format!(
                    "    let value_{i} = compute({i}, \"some fairly long text to make it big\");\n"
                ));
                if i % 20 == 19 {
                    code.push('\n');
                }
            }
            code.push_str("}\n\n");
        }
        let library = Library::default();
        let shown = store(&library, "code", &code);
        assert!(
            shown.contains("fn function_0()") && shown.contains("fn function_3()"),
            "{shown}"
        );
        // A function too big to show opens into blocks.
        let deeper = read(&library, "doc1", "2").unwrap();
        assert!(deeper.contains("2.1"), "{deeper}");
        let leaf = read(&library, "doc1", "2.1").unwrap();
        assert!(
            leaf.contains("| fn function_0()") || leaf.contains("| "),
            "{leaf}"
        );
    }

    #[test]
    fn markdown_splits_by_headings() {
        let text = "# Title\nintro\n## A\n".to_owned()
            + &"text a\n".repeat(400)
            + "## B\n"
            + &"text b\n".repeat(400);
        let library = Library::default();
        let shown = store(&library, "page", &text);
        assert!(shown.contains("A —") && shown.contains("B —"), "{shown}");
        assert!(read(&library, "doc1", "9").is_err());
    }
}
