//! App settings persisted next to the executable.

use std::path::PathBuf;

use crate::agent::AgentConfig;
use crate::commands_model::UserCommand;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub application_id: String,
    pub commands: Vec<UserCommand>,
    pub seeded: bool,
    pub save_token: bool,
    pub token: String,
    pub agent: AgentConfig,
}

impl AppConfig {
    pub fn seed_built_ins(&mut self) {
        if !self.seeded {
            self.commands = crate::commands_model::default_commands();
            self.seeded = true;
        }
    }
}

pub fn default_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axis_bot_studio.json")
}

pub fn load(path: &std::path::Path) -> AppConfig {
    let mut config: AppConfig = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if !config.save_token {
        config.token.clear();
    }
    config
}

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
                behavior: Default::default(),
            }],
            seeded: true,
            save_token: true,
            token: "secret".to_owned(),
            agent: AgentConfig::default(),
        };
        let path = std::env::temp_dir().join("axis_bot_studio_test.json");
        save(&path, &config).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, config);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_file_yields_default() {
        let path = std::env::temp_dir().join("axis_bs_missing.json");
        let config = load(&path);
        assert_eq!(config, AppConfig::default());
    }

    #[test]
    fn token_cleared_when_save_disabled() {
        let path = std::env::temp_dir().join("axis_bs_token.json");
        let config = AppConfig {
            application_id: "1".to_owned(),
            save_token: false,
            token: "should-not-survive".to_owned(),
            seeded: true,
            ..Default::default()
        };
        save(&path, &config).unwrap();
        let loaded = load(&path);
        assert!(
            loaded.token.is_empty(),
            "token must not survive when save is off"
        );
        let _ = std::fs::remove_file(path);
    }
}
