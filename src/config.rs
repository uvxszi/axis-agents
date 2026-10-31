//! App settings persisted next to the executable.
//!
//! The Discord token is deliberately never persisted — it lives only in
//! memory for the current session.

use std::path::PathBuf;

use crate::commands_model::UserCommand;

/// Persisted settings.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub application_id: String,
    #[serde(default)]
    pub commands: Vec<UserCommand>,
}

/// `axis_bot_studio.json` next to the executable.
pub fn default_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axis_bot_studio.json")
}

/// Load the config; a missing or unreadable file yields the default.
pub fn load(path: &std::path::Path) -> AppConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Save the config; the error is surfaced to the GUI log.
pub fn save(path: &std::path::Path, config: &AppConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config)
        .map_err(|error| format!("config serialization failed: {error}"))?;
    std::fs::write(path, text).map_err(|error| format!("config write failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands_model::{OptionKind, UserCommand, UserOption};

    #[test]
    fn roundtrips_through_json() {
        let config = AppConfig {
            application_id: "12345".to_owned(),
            commands: vec![UserCommand {
                name: "greet".to_owned(),
                description: "Greet".to_owned(),
                reply: "Hi {user}".to_owned(),
                options: vec![UserOption {
                    name: "place".to_owned(),
                    description: "Where".to_owned(),
                    kind: OptionKind::String,
                    required: true,
                }],
            }],
        };
        let path = std::env::temp_dir().join("axis_bot_studio_test.json");
        save(&path, &config).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, config);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_file_yields_default() {
        let path = std::env::temp_dir().join("axis_bot_studio_definitely_missing.json");
        let config = load(&path);
        assert_eq!(config, AppConfig::default());
    }

    #[test]
    fn corrupt_file_yields_default() {
        let path = std::env::temp_dir().join("axis_bot_studio_corrupt.json");
        std::fs::write(&path, "{ not json").unwrap();
        let config = load(&path);
        assert_eq!(config, AppConfig::default());
        let _ = std::fs::remove_file(path);
    }
}
