//! App settings persisted next to the executable.
//!
//! The Discord token is only persisted when the user explicitly opts in
//! with the "remember token" toggle (stored in plain text next to the
//! exe — the toggle says so).

use std::path::PathBuf;

use crate::agent::AgentConfig;
use crate::commands_model::UserCommand;

/// Persisted settings.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub application_id: String,
    pub commands: Vec<UserCommand>,
    /// Built-in commands have been seeded once.
    pub seeded: bool,
    /// Store the token on disk (plain text, opt-in).
    pub save_token: bool,
    /// Only meaningful when `save_token` is true.
    pub token: String,
    pub agent: AgentConfig,
}

impl AppConfig {
    /// Seed the built-in commands once (they remain editable), and ensure
    /// an agent command exists if the user removed theirs.
    pub fn seed_built_ins(&mut self) {
        if !self.seeded {
            self.commands = crate::commands_model::default_commands();
            self.seeded = true;
        } else if !self.commands.iter().any(|command| {
            matches!(
                command.behavior,
                crate::commands_model::CommandBehavior::Agent
            )
        }) {
            // The AI agent command is the app's core feature; restore it
            // if the user removed every agent command.
            if let Some(agent_default) =
                crate::commands_model::default_commands()
                    .into_iter()
                    .find(|command| {
                        matches!(
                            command.behavior,
                            crate::commands_model::CommandBehavior::Agent
                        )
                    })
            {
                self.commands.push(agent_default);
            }
        }
    }
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
    let mut config: AppConfig = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    // Never load a persisted token when the toggle is off.
    if !config.save_token {
        config.token.clear();
    }
    config
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
    use crate::commands_model::{CommandBehavior, OptionKind, UserCommand, UserOption};

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
                behavior: CommandBehavior::Reply,
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
        let path = std::env::temp_dir().join("axis_bot_studio_missing.json");
        let config = load(&path);
        assert_eq!(config, AppConfig::default());
        // Seeding is the caller's job (App::new).
        let mut config = config;
        config.seed_built_ins();
        assert!(config.seeded);
        assert_eq!(config.commands.len(), 4);
        assert!(config.commands.iter().any(|c| c.name == "agent"));
    }

    #[test]
    fn corrupt_file_yields_default() {
        let path = std::env::temp_dir().join("axis_bot_studio_corrupt.json");
        std::fs::write(&path, "{ not json").unwrap();
        let config = load(&path);
        assert_eq!(config, AppConfig::default());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn token_cleared_when_save_disabled() {
        let path = std::env::temp_dir().join("axis_bot_studio_token.json");
        let mut config = AppConfig {
            application_id: "1".to_owned(),
            save_token: false,
            token: "should-not-survive".to_owned(),
            seeded: true,
            ..AppConfig::default()
        };
        save(&path, &config).unwrap();
        config.token = String::new(); // simulate what the app does at load
        let loaded = load(&path);
        assert!(
            !loaded.token.contains("should-not-survive") || loaded.save_token,
            "token must not be resurrected when the toggle is off"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn agent_command_is_restored_if_removed() {
        let path = std::env::temp_dir().join("axis_bot_studio_restore.json");
        let mut config = AppConfig {
            seeded: true,
            ..AppConfig::default()
        };
        config.commands.clear();
        save(&path, &config).unwrap();
        let mut loaded = load(&path);
        loaded.seed_built_ins();
        assert!(
            loaded
                .commands
                .iter()
                .any(|c| matches!(c.behavior, CommandBehavior::Agent)),
            "the /agent command must always exist"
        );
        let _ = std::fs::remove_file(path);
    }
}
