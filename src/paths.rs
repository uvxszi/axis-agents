//! Where the app keeps its files, and crash-safe writes.

use std::path::{Path, PathBuf};

/// The data directory: `%APPDATA%\AxisAgents` on Windows,
/// `$XDG_CONFIG_HOME/axis-agents` or `~/.config/axis-agents` elsewhere.
/// `AXIS_AGENTS_HOME` overrides it (portable installs, tests).
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AXIS_AGENTS_HOME") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    let name = if cfg!(windows) {
        "AxisAgents"
    } else {
        "axis-agents"
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join(name)
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn agent_dir(agent_id: &str) -> PathBuf {
    data_dir().join("agents").join(sanitize(agent_id))
}

pub fn shared_dir() -> PathBuf {
    data_dir().join("shared")
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write a file atomically: write a sibling temp file, then rename it
/// over the target. Creates parent directories.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp{}", fastrand::u32(..)));
    std::fs::write(&tmp, bytes)
        .map_err(|error| format!("could not write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not replace {}: {error}", path.display())
    })
}

/// Read and parse a JSON file, `None` when missing or unreadable.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not parse data file");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content() {
        let path = std::env::temp_dir()
            .join(format!("axis_paths_{}", fastrand::u32(..)))
            .join("file.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ids_are_sanitized() {
        assert_eq!(sanitize("../evil"), "___evil");
    }
}
