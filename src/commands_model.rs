//! User-editable slash commands compiled into an Axis [`CommandRegistry`].

use axis::prelude::*;
use axis_commands::{CommandOption, CommandRegistry, Invocation};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CommandBehavior {
    #[default]
    Reply,
    Roll,
    Ping,
}

impl CommandBehavior {
    pub const ALL: [CommandBehavior; 3] = [
        CommandBehavior::Reply,
        CommandBehavior::Roll,
        CommandBehavior::Ping,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CommandBehavior::Reply => "reply template",
            CommandBehavior::Roll => "roll a die",
            CommandBehavior::Ping => "ping (latency)",
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UserOption {
    pub name: String,
    pub description: String,
    pub kind: OptionKind,
    pub required: bool,
}

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

pub fn default_commands() -> Vec<UserCommand> {
    vec![
        UserCommand {
            name: "ping".to_owned(),
            description: "Check the bot's latency".to_owned(),
            reply: String::new(),
            options: Vec::new(),
            behavior: CommandBehavior::Ping,
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
    ]
}

pub fn build_registry(commands: &[UserCommand]) -> (CommandRegistry, Result<(), AxisError>) {
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
                        let template = command.reply.clone();
                        cmd.handler(move |invocation| {
                            let template = template.clone();
                            async move {
                                invocation
                                    .reply()
                                    .content(render_reply(&template, &invocation))
                                    .await?;
                                Ok(())
                            }
                        });
                    }
                    CommandBehavior::Roll => {
                        let template = command.reply.clone();
                        cmd.handler(move |invocation| {
                            let template = template.clone();
                            async move {
                                let sides = invocation.opt::<i64>("sides")?.unwrap_or(6);
                                let roll = if sides > 0 {
                                    (rand_roll().unsigned_abs() % sides as u64) as i64 + 1
                                } else {
                                    1
                                };
                                let reply = render_reply(
                                    &template.replace("{roll}", &roll.to_string()),
                                    &invocation,
                                );
                                invocation.reply().content(reply).await?;
                                Ok(())
                            }
                        });
                    }
                    CommandBehavior::Ping => {
                        cmd.handler(move |invocation| async move {
                            let start = std::time::Instant::now();
                            let _ = invocation.http.get_gateway().await?;
                            let elapsed = start.elapsed().as_millis();
                            invocation
                                .reply()
                                .content(format!("Gateway latency: {elapsed}ms"))
                                .await?;
                            Ok(())
                        });
                    }
                }
            },
        );
    }
    let validation = registry.validate();
    (registry, validation)
}

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
                .and_then(|m| m.user.as_ref())
        })
        .map(|user| format!("<@{}>", user.id))
        .unwrap_or_else(|| "you".to_owned());
    reply = reply.replace("{user}", &user);
    reply
}

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

    fn make_invocation(options: serde_json::Value) -> Invocation {
        let interaction: Interaction = serde_json::from_value(json!({
            "id": "100", "application_id": "5", "type": 2,
            "data": { "id": "101", "name": "greet", "type": 1, "options": options },
            "user": { "id": "80351110224678912", "username": "Nelly", "discriminator": "0" },
            "token": "t", "version": 1 }))
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
    fn templates_substitute() {
        let invocation =
            make_invocation(json!([{ "name": "target", "type": 3, "value": "the moon" }]));
        assert_eq!(
            render_reply("Hello {user}, welcome to {target}!", &invocation),
            "Hello <@80351110224678912>, welcome to the moon!"
        );
    }

    #[test]
    fn defaults_are_correct() {
        let defaults = default_commands();
        assert_eq!(defaults.len(), 3);
        assert_eq!(defaults[0].behavior, CommandBehavior::Ping);
    }

    #[test]
    fn registry_validates() {
        let (registry, validation) = build_registry(&default_commands());
        validation.unwrap();
        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn name_rules() {
        assert!(valid_command_name("greet-user_2"));
        assert!(!valid_command_name("Greet"));
    }
}
