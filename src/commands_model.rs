//! The app's command model: user-editable slash commands (reply
//! templates, dice rolls, AI-agent invocations) compiled into an Axis
//! [`CommandRegistry`].

use std::sync::Arc;

use axis::prelude::*;
use axis_commands::{CommandOption, CommandRegistry, Invocation};

use crate::agent::{self, AgentState};

/// What a command does when invoked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CommandBehavior {
    /// Reply with a template (`{option_name}` / `{user}` placeholders).
    #[default]
    Reply,
    /// Roll a die with the `sides` option.
    Roll,
    /// Run the AI agent with the `message` option (or the raw text).
    Agent,
}

impl CommandBehavior {
    pub const ALL: [CommandBehavior; 3] = [
        CommandBehavior::Reply,
        CommandBehavior::Roll,
        CommandBehavior::Agent,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CommandBehavior::Reply => "reply template",
            CommandBehavior::Roll => "roll a die",
            CommandBehavior::Agent => "AI agent",
        }
    }
}

/// The type of a user-declared option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OptionKind {
    String,
    Integer,
    Boolean,
}

impl OptionKind {
    pub const ALL: [OptionKind; 3] = [OptionKind::String, OptionKind::Integer, OptionKind::Boolean];

    pub fn label(self) -> &'static str {
        match self {
            OptionKind::String => "text",
            OptionKind::Integer => "number",
            OptionKind::Boolean => "true/false",
        }
    }
}

/// One option of a user-declared command.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UserOption {
    pub name: String,
    pub description: String,
    pub kind: OptionKind,
    pub required: bool,
}

/// A user-declared command.
///
/// - `Reply`: the reply text is a template (`{option_name}` / `{user}`).
/// - `Roll`: rolls a die with the `sides` option; reply may use `{roll}`.
/// - `Agent`: runs the AI agent; reply is ignored (the answer is the
///   model's, sent via followup).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UserCommand {
    pub name: String,
    pub description: String,
    pub reply: String,
    #[serde(default)]
    pub options: Vec<UserOption>,
    #[serde(default)]
    pub behavior: CommandBehavior,
}

/// The default command set, seeded on first run (all editable afterwards).
pub fn default_commands() -> Vec<UserCommand> {
    vec![
        UserCommand {
            name: "ping".to_owned(),
            description: "Check the bot is alive".to_owned(),
            reply: "Pong! 🏓".to_owned(),
            options: Vec::new(),
            behavior: CommandBehavior::Reply,
        },
        UserCommand {
            name: "echo".to_owned(),
            description: "Repeat after you".to_owned(),
            reply: "{text}".to_owned(),
            options: vec![UserOption {
                name: "text".to_owned(),
                description: "What to echo".to_owned(),
                kind: OptionKind::String,
                required: true,
            }],
            behavior: CommandBehavior::Reply,
        },
        UserCommand {
            name: "roll".to_owned(),
            description: "Roll a die".to_owned(),
            reply: "🎲 {roll}".to_owned(),
            options: vec![UserOption {
                name: "sides".to_owned(),
                description: "How many sides (default 6)".to_owned(),
                kind: OptionKind::Integer,
                required: false,
            }],
            behavior: CommandBehavior::Roll,
        },
        UserCommand {
            name: "agent".to_owned(),
            description: "Ask the AI agent".to_owned(),
            reply: String::new(),
            options: vec![UserOption {
                name: "message".to_owned(),
                description: "What to ask".to_owned(),
                kind: OptionKind::String,
                required: true,
            }],
            behavior: CommandBehavior::Agent,
        },
    ]
}

/// Build the live registry from the command list.
///
/// `agent` is required for commands with [`CommandBehavior::Agent`]; such
/// commands without it reply that the agent is unavailable.
pub fn build_registry(
    commands: &[UserCommand],
    agent: Option<&Arc<AgentState>>,
) -> (CommandRegistry, Result<(), AxisError>) {
    let mut registry = CommandRegistry::new();

    for command in commands {
        let command = command.clone();
        registry.command(
            command.name.clone(),
            command.description.clone(),
            move |cmd| {
                for option in &command.options {
                    let mut builder = match option.kind {
                        OptionKind::String => {
                            CommandOption::string(option.name.clone(), option.description.clone())
                        }
                        OptionKind::Integer => {
                            CommandOption::integer(option.name.clone(), option.description.clone())
                        }
                        OptionKind::Boolean => {
                            CommandOption::boolean(option.name.clone(), option.description.clone())
                        }
                    };
                    if option.required {
                        builder = builder.required();
                    }
                    cmd.option(builder.finish());
                }

                match command.behavior {
                    CommandBehavior::Reply => {
                        let reply_template = command.reply.clone();
                        cmd.handler(move |invocation| {
                            let reply_template = reply_template.clone();
                            async move {
                                let reply = render_reply(&reply_template, &invocation);
                                invocation.reply().content(reply).await?;
                                Ok(())
                            }
                        });
                    }
                    CommandBehavior::Roll => {
                        let reply_template = command.reply.clone();
                        cmd.handler(move |invocation| {
                            let reply_template = reply_template.clone();
                            async move {
                                let sides = invocation.opt::<i64>("sides")?.unwrap_or(6);
                                let roll = if sides > 0 {
                                    (rand_roll().unsigned_abs() % sides as u64) as i64 + 1
                                } else {
                                    1
                                };
                                let reply = render_reply(
                                    &reply_template.replace("{roll}", &roll.to_string()),
                                    &invocation,
                                );
                                invocation.reply().content(reply).await?;
                                Ok(())
                            }
                        });
                    }
                    CommandBehavior::Agent => {
                        let agent = agent.map(Arc::clone);
                        let reply_template = command.reply.clone();
                        cmd.handler(move |invocation| {
                            let agent = agent.clone();
                            let reply_template = reply_template.clone();
                            async move {
                                let Some(agent) = agent else {
                                    invocation
                                        .reply()
                                        .content("The AI agent is not available.")
                                        .ephemeral(true)
                                        .await?;
                                    return Ok(());
                                };
                                // Defer first: LLM calls can outlast the
                                // 3-second interaction window.
                                invocation.defer(false).await?;
                                let scope = (
                                    invocation
                                        .interaction
                                        .guild_id
                                        .map(|id| id.get())
                                        .unwrap_or(0),
                                    invocation
                                        .interaction
                                        .channel_id
                                        .map(|id| id.get())
                                        .unwrap_or(0),
                                );
                                let invoker = invocation
                                    .interaction
                                    .user
                                    .as_ref()
                                    .or_else(|| {
                                        invocation
                                            .interaction
                                            .member
                                            .as_ref()
                                            .and_then(|member| member.user.as_ref())
                                    })
                                    .map(|user| (user.id.get(), user.username.clone()))
                                    .unwrap_or((0, "unknown".to_owned()));
                                let question =
                                    invocation.opt::<String>("message")?.unwrap_or_default();
                                match agent::run_agent(
                                    invocation.http.clone(),
                                    &agent,
                                    scope,
                                    invoker,
                                    question,
                                )
                                .await
                                {
                                    Ok(answer) => {
                                        for chunk in agent::split_reply(&answer) {
                                            invocation.followup().content(chunk).await?;
                                        }
                                    }
                                    Err(error) => {
                                        invocation
                                            .followup()
                                            .content(format!("⚠ {error}"))
                                            .ephemeral(true)
                                            .await?;
                                    }
                                }
                                let _ = reply_template;
                                Ok(())
                            }
                        });
                    }
                }
            },
        );
    }

    let validation = registry.validate();
    (registry, validation)
}

/// Render a reply template: `{name}` for each supplied option, `{user}`
/// for the invoker's mention, `{roll}` is substituted before this runs.
pub fn render_reply(template: &str, invocation: &Invocation) -> String {
    let mut reply = template.to_owned();
    for option in invocation.scope_options() {
        let Some(value) = option.value.as_ref() else {
            continue;
        };
        let rendered = match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        reply = reply.replace(&format!("{{{}}}", option.name), &rendered);
    }
    let user = invocation
        .interaction
        .user
        .as_ref()
        .or_else(|| {
            invocation
                .interaction
                .member
                .as_ref()
                .and_then(|member| member.user.as_ref())
        })
        .map(|user| format!("<@{}>", user.id))
        .unwrap_or_else(|| "you".to_owned());
    reply = reply.replace("{user}", &user);
    reply
}

/// A tiny xorshift so the roll command needs no rand dependency; this is
/// a dice roll, not cryptography.
pub fn rand_roll() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or(0);
    let mut state = u64::from(nanos) | 1;
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    (state >> 8) as i64
}

/// Check a candidate command name against Discord's rules (mirrored from
/// the registry validation, run in the UI before a command is added).
pub fn valid_command_name(name: &str) -> bool {
    let chars = name.chars().count();
    (1..=32).contains(&chars)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub fn valid_description(description: &str) -> bool {
    let chars = description.chars().count();
    (1..=100).contains(&chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axis::model::Interaction;
    use serde_json::json;
    use std::sync::Arc;

    fn interaction_with_options(options: serde_json::Value) -> Invocation {
        let interaction: Interaction = serde_json::from_value(json!({
            "id": "100", "application_id": "5", "type": 2,
            "data": {
                "id": "101", "name": "greet", "type": 1, "options": options
            },
            "user": { "id": "80351110224678912", "username": "Nelly", "discriminator": "0" },
            "token": "t", "version": 1
        }))
        .unwrap();
        let data = interaction.command_data().unwrap().clone();
        Invocation {
            http: Arc::new(Http::builder(Token::new("t".to_owned())).build().unwrap()),
            interaction,
            data,
            path: Vec::new(),
        }
    }

    #[test]
    fn templates_substitute_options_and_user() {
        let invocation = interaction_with_options(json!([
            { "name": "target", "type": 3, "value": "the moon" }
        ]));
        let reply = render_reply("Hello {user}, welcome to {target}!", &invocation);
        assert_eq!(reply, "Hello <@80351110224678912>, welcome to the moon!");
    }

    #[test]
    fn templates_keep_unknown_placeholders() {
        let invocation = interaction_with_options(json!([]));
        let reply = render_reply("value: {missing}", &invocation);
        assert_eq!(reply, "value: {missing}");
    }

    #[test]
    fn defaults_seed_four_builtins() {
        let defaults = default_commands();
        assert_eq!(defaults.len(), 4);
        assert_eq!(defaults[0].name, "ping");
        assert_eq!(defaults[3].behavior, CommandBehavior::Agent);
    }

    #[test]
    fn registry_from_model_passes_validation() {
        let commands = vec![UserCommand {
            name: "greet".to_owned(),
            description: "Greet someone".to_owned(),
            reply: "Hi {user}, welcome to {place}!".to_owned(),
            options: vec![
                UserOption {
                    name: "place".to_owned(),
                    description: "Where".to_owned(),
                    kind: OptionKind::String,
                    required: true,
                },
                UserOption {
                    name: "loud".to_owned(),
                    description: "All caps".to_owned(),
                    kind: OptionKind::Boolean,
                    required: false,
                },
            ],
            behavior: CommandBehavior::Reply,
        }];
        let (registry, validation) = build_registry(&commands, None);
        validation.unwrap();
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn agent_commands_build_without_state() {
        // The Agent behavior must be declarable even without an agent
        // (it replies "not available" at runtime).
        let (registry, validation) = build_registry(&default_commands(), None);
        validation.unwrap();
        assert_eq!(registry.len(), 4);
    }

    #[test]
    fn registry_rejects_broken_commands() {
        let commands = vec![UserCommand {
            name: "Broken Name".to_owned(),
            description: "x".to_owned(),
            reply: String::new(),
            options: Vec::new(),
            behavior: CommandBehavior::Reply,
        }];
        let (_, validation) = build_registry(&commands, None);
        assert!(validation.is_err());
    }

    #[test]
    fn name_rules() {
        assert!(valid_command_name("greet-user_2"));
        assert!(!valid_command_name("Greet"));
        assert!(!valid_command_name("has space"));
        assert!(!valid_command_name(""));
    }
}
