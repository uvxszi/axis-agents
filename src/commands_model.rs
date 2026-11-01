//! The app's command model: user-defined slash commands backed by a
//! reply-template, plus built-in commands, compiled into an Axis
//! [`CommandRegistry`].

use axis::prelude::*;
use axis_commands::{CommandOption, CommandRegistry, Invocation};

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

/// A user-declared command: a name, a description, and a reply template.
///
/// The reply may reference option values with `{option_name}` and the
/// invoker with `{user}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UserCommand {
    pub name: String,
    pub description: String,
    pub reply: String,
    #[serde(default)]
    pub options: Vec<UserOption>,
}

/// Build the live registry: built-ins first, then the user's commands.
///
/// Returns the registry and a validation result — declaration mistakes
/// surface here instead of at Discord.
pub fn build_registry(commands: &[UserCommand]) -> (CommandRegistry, Result<(), AxisError>) {
    let mut registry = CommandRegistry::new();

    registry.command("ping", "Check the bot is alive", |cmd| {
        cmd.handler(|invocation| async move {
            invocation.reply().content("Pong! 🏓").await?;
            Ok(())
        });
    });

    registry.command("echo", "Repeat after you", |cmd| {
        cmd.option(
            CommandOption::string("text", "What to echo")
                .required()
                .max_length(500)
                .finish(),
        )
        .handler(|invocation| async move {
            let text: String = invocation.arg("text")?;
            invocation.reply().content(text).await?;
            Ok(())
        });
    });

    registry.command("roll", "Roll a die", |cmd| {
        cmd.option(
            CommandOption::integer("sides", "How many sides (default 6)")
                .min_value(2.0)
                .max_value(1000.0)
                .finish(),
        )
        .handler(|invocation| async move {
            let sides = invocation.opt::<i64>("sides")?.unwrap_or(6);
            let roll = if sides > 0 {
                (rand_roll() % sides) + 1
            } else {
                1
            };
            invocation.reply().content(format!("🎲 {roll}")).await?;
            Ok(())
        });
    });

    for user_command in commands {
        let command = user_command.clone();
        registry.command(
            user_command.name.clone(),
            user_command.description.clone(),
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
                let reply_template = command.reply.clone();
                cmd.handler(move |invocation| {
                    let reply_template = reply_template.clone();
                    async move {
                        let reply = render_reply(&reply_template, &invocation);
                        invocation.reply().content(reply).await?;
                        Ok(())
                    }
                });
            },
        );
    }

    let validation = registry.validate();
    (registry, validation)
}

/// Render a reply template: `{name}` for each supplied option, `{user}`
/// for the invoker's mention.
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
fn rand_roll() -> i64 {
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
        let interaction: Interaction = serde_json::from_value(serde_json::json!({
            "id": "100", "application_id": "5", "type": 2,
            "data": { "id": "101", "name": "greet", "type": 1, "options": options },
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
        }];
        let (registry, validation) = build_registry(&commands);
        validation.unwrap();
        // ping/echo/roll + the user command.
        assert_eq!(registry.len(), 4);
    }

    #[test]
    fn connect_always_yields_built_ins_plus_user_commands() {
        // Regression: the app once dispatched through an EMPTY registry
        // because the connect-time build was never installed. The
        // connect path uses build_registry — it must contain the
        // built-ins even with zero user commands, and every user command
        // with one.
        let (registry, validation) = build_registry(&[]);
        validation.unwrap();
        assert_eq!(registry.len(), 3, "built-ins must exist with no commands");

        let commands = vec![UserCommand {
            name: "greet".to_owned(),
            description: "Greet".to_owned(),
            reply: "Hi {user}".to_owned(),
            options: Vec::new(),
        }];
        let (registry, validation) = build_registry(&commands);
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
        }];
        let (_, validation) = build_registry(&commands);
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
