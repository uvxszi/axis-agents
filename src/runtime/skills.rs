//! Agent skills: procedures the agent is TAUGHT and keeps.
//!
//! A skill is a markdown document — a name, when to use it, and the
//! step-by-step method. Anyone trusted can teach one ("learn this…"), the
//! agent can write its own from experience, and every skill's name +
//! description sits in the system prompt so the agent knows what it knows;
//! the full method loads on demand. Skills persist on disk and survive
//! restarts. They carry judgment and process — never hardcoded logic.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

/// One taught skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Short id used with load_skill ("chess").
    pub name: String,
    /// When to use it — always visible to the agent.
    pub description: String,
    /// The method itself — loaded on demand.
    pub body: String,
    /// Who taught it.
    pub author: String,
    pub created: i64,
}

fn store() -> &'static Mutex<Vec<Skill>> {
    static SKILLS: OnceLock<Mutex<Vec<Skill>>> = OnceLock::new();
    SKILLS.get_or_init(|| Mutex::new(load_all()))
}

fn dir() -> PathBuf {
    crate::paths::data_dir().join("skills")
}

fn path_for(name: &str) -> PathBuf {
    dir().join(format!("{name}.md"))
}

/// Very small frontmatter: `---\nname: x\ndescription: y\n---\nbody`.
fn parse_skill(text: &str, author: String, created: i64) -> Option<Skill> {
    let rest = text.strip_prefix("---")?;
    let (front, body) = rest.split_once("---")?;
    let mut name = None;
    let mut description = None;
    let mut author = author;
    for line in front.lines() {
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("description:") {
            description = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("author:") {
            author = value.trim().to_owned();
        }
    }
    Some(Skill {
        name: name?,
        description: description.unwrap_or_default(),
        body: body.trim().to_owned(),
        author,
        created,
    })
}

fn dump_skill(skill: &Skill) -> String {
    format!(
        "---\nname: {}\ndescription: {}\n---\n{}\n",
        skill.name, skill.description, skill.body
    )
}

fn load_all() -> Vec<Skill> {
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir()) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "md") {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    if let Some(skill) = parse_skill(&text, "unknown".to_owned(), 0) {
                        skills.push(skill);
                    }
                }
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Every skill's name + description — injected into the system prompt.
pub fn summary() -> String {
    let skills = store().lock().expect("skills poisoned");
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n## Skills you have been taught\nCall load_skill (name) to read the full method before acting; follow it.\n",
    );
    for skill in skills.iter() {
        out.push_str(&format!("- {} — {}\n", skill.name, skill.description));
    }
    out
}

/// Read one skill's full method.
pub fn load(name: &str) -> Result<String, String> {
    let name = name.trim().to_lowercase();
    let skills = store().lock().expect("skills poisoned");
    let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| {
        format!(
            "no skill named {name:?} — you have: {}",
            skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    Ok(format!(
        "SKILL: {} (taught by {})\nWhen to use: {}\n\n{}",
        skill.name, skill.author, skill.description, skill.body
    ))
}

/// Teach (or update) a skill.
pub fn learn(name: &str, description: &str, body: &str, author: &str) -> Result<String, String> {
    let name = crate::llm::truncate(&name.trim().to_lowercase().replace(' ', "-"), 40);
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err("give a short skill name: lowercase letters, digits, dashes".to_owned());
    }
    if description.trim().len() < 8 {
        return Err("the description (when to use it) is required — one line".to_owned());
    }
    if body.trim().chars().count() < 40 {
        return Err(
            "the method itself is too short — write the actual steps you were taught".to_owned(),
        );
    }
    let skill = Skill {
        name: name.clone(),
        description: crate::llm::truncate(description.trim(), 200),
        body: body.trim().to_owned(),
        author: author.to_owned(),
        created: crate::memory::now_ms(),
    };
    let _ = std::fs::create_dir_all(dir());
    crate::paths::write_atomic(&path_for(&name), dump_skill(&skill).as_bytes())?;
    let replaced = store()
        .lock()
        .expect("skills poisoned")
        .iter()
        .any(|s| s.name == name);
    let mut skills = store().lock().expect("skills poisoned");
    skills.retain(|s| s.name != name);
    skills.push(skill);
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(format!(
        "{} the skill {name:?} — it is in your skills list from now on and you can load_skill it any time (survives restarts).",
        if replaced { "updated" } else { "learned" }
    ))
}

/// Drop a skill.
pub fn forget(name: &str) -> Result<String, String> {
    let name = name.trim().to_lowercase();
    let removed = {
        let mut skills = store().lock().expect("skills poisoned");
        let before = skills.len();
        skills.retain(|s| s.name != name);
        before != skills.len()
    };
    if removed {
        let _ = std::fs::remove_file(path_for(&name));
        Ok(format!("forgot the skill {name:?}"))
    } else {
        Err(format!("no skill named {name:?} to forget"))
    }
}

/// Seed the built-in skills on first run (only when missing — the
/// operator's edits on disk are never overwritten).
pub fn seed_defaults() {
    let _ = std::fs::create_dir_all(dir());
    for (name, description, body) in [
        (
            "chess",
            "Play or continue a chess game in chat: keep the real board as a text diagram in this exact format, show it after every move, and read it before any commentary.",
            "# Chess
THE BOARD is a text diagram, always in this exact format — ranks 8 down
to 1, files a-h left to right, uppercase = White, lowercase = black,
'.' = empty square, two spaces between the rank number and the row:

8  r n b q k b n r
7  p p p p p p p p
6  . . . . . . . .
5  . . . . . . . .
4  . . . . P . . .
3  . . . . . . . .
2  P P P P . P P P
1  R N B Q K B N R
   a b c d e f g h

RULES:
1. START A GAME by printing this diagram (starting position) and whose
   move it is.
2. BEFORE ANY COMMENT about a move (yours, theirs, suggested, or
   claimed): rebuild the CURRENT diagram from the last one shown, find
   the from-square, and check the to-square makes sense for THAT piece.
   If the from-square holds no such piece, say so — never invent or
   accept an impossible move.
3. AFTER AN AGREED MOVE: print the NEW diagram (the whole thing, same
   format), whose turn it is, then a short comment. The diagram is the
   truth — every claim (blunders, hangs, checks) must be visible IN it.
   No commentary without the diagram.
4. CAPTURES, checks, castling (king two squares + rook jump), en
   passant, promotion (a pawn reaching rank 8/1 becomes Q by default):
   apply them in the diagram.
5. Keep commentary to 1-2 lines; the diagram carries the state.

The point: never reason about the position from memory. Redraw the
diagram from the previous one, move by move, and show it.",
        ),
        (
            "draw-diagrams",
            "Draw diagrams, charts-of-anything, scenes, and illustrations with draw_image so they come out right the first times — layout first, layers, verify with vision, fix the named flaws.",
            "# Drawing diagrams and pictures
1. PLAN THE LAYOUT BEFORE ANY SHAPE. Decide canvas size first (detail
   needs room: 900x600 for diagrams with labels). Sketch on paper-in-
   your-head: list every element with its approximate center coordinates.
   Keep 60px margins; give every label room (text is ~0.6*SIZE px per
   character wide).
2. DRAW IN LAYERS, back to front:
   a. background / fill
   b. big structural shapes (bodies, orbits, boards, panels)
   c. connecting lines and arrows BETWEEN shapes (endpoints on edges,
      not centers)
   d. DETAILS (patterns, stripes, small circles)
   e. LABELS LAST — text drawn after shapes is never covered up.
3. GEOMETRY THAT MUST BE RIGHT:
   - orbits: `arc CX CY RX RY 0 360 gray width=2` — one per orbit, all
     sharing the same CX CY (the sun's center). Planets go ON their
     orbit: pick an angle per planet, position =
     (CX + RX*cos(a), CY + RY*sin(a)) — compute it, don't eyeball.
   - grids/boards: one rect per cell, computed in a loop BY YOU: x =
     X0 + col*TILE, y = Y0 + row*TILE. Check the count: an 8x8 board is
     64 rects. Get the alternation pattern right before writing them.
   - symmetry: mirror coordinates deliberately (left = CX - d,
     right = CX + d).
4. ALWAYS PASS goal: \"one sentence: what the finished picture must
   show\" — the app runs a vision check and hands you the concrete
   flaws. If it FAILs: fix exactly the listed flaws and redraw (same
   goal). Expect 2-3 rounds on anything complex; that is the process,
   not a failure. Only report the drawing when the check PASSES (the
   tool says so), and never attach a version the check rejected.",
        ),
    ] {
        let path = path_for(name);
        // Built-in skills follow their seed (this code) when unchanged by
        // a person; anything taught or edited by a person is never touched.
        if path.exists() {
            let skill = Skill {
                name: name.to_owned(),
                description: description.to_owned(),
                body: body.to_owned(),
                author: "built-in".to_owned(),
                created: crate::memory::now_ms(),
            };
            if let Ok(existing) = std::fs::read_to_string(&path) {
                if existing == dump_skill(&skill) {
                    continue;
                }
                // Old seeds on disk carry no author (or "unknown") — they
                // are built-ins and follow the updated seed. Anything a
                // person or the agent taught (a real author name) is
                // never touched.
                if let Some(parsed) = parse_skill(&existing, String::new(), 0) {
                    if parsed.author != "built-in" && !parsed.author.is_empty() {
                        continue;
                    }
                }
            }
            crate::paths::write_atomic(&path, dump_skill(&skill).as_bytes()).expect("seed skills");
            continue;
        }
        let skill = Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            body: body.to_owned(),
            author: "built-in".to_owned(),
            created: crate::memory::now_ms(),
        };
        crate::paths::write_atomic(&path, dump_skill(&skill).as_bytes()).expect("seed skills");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let skill = parse_skill(
            "---\nname: tea\ndescription: how to make tea\n---\nBoil water.\nSteep 3 min.",
            "tester".into(),
            0,
        )
        .expect("parses");
        assert_eq!(skill.name, "tea");
        assert!(skill.body.contains("Steep"));
    }

    #[test]
    fn learn_validates() {
        assert!(learn("", "when", "body body body body body", "t").is_err());
        assert!(learn("ok-name", "short", "tiny", "t").is_err());
        assert!(
            learn(
                "ok-name",
                "a perfectly fine description line",
                "step one\nstep two\nstep three with enough words to pass the length check",
                "tester"
            )
            .is_ok()
        );
        forget("ok-name").ok();
    }
}
