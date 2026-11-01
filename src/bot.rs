//! The bridge between the GUI thread and the Axis bot.

use std::sync::{Arc, Mutex, RwLock};

use axis::prelude::*;
use axis::rest::Http;
use axis_commands::CommandRegistry;

use crate::agent::{AgentConfig, AgentState};

#[derive(Debug, Clone)]
pub enum BotCommand {
    Connect {
        token: String,
        application_id: u64,
        commands: Vec<crate::commands_model::UserCommand>,
    },
    Disconnect,
    SendMessage {
        channel_id: u64,
        content: String,
    },
    SyncCommands,
    FetchModels {
        base_url: String,
        api_key: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Offline,
    Connecting,
    Connected(String),
    Resumed,
    Reconnecting(Option<u16>),
    Failed(String),
    Disconnected(String),
}

#[derive(Debug, Clone)]
pub enum BotEvent {
    Status(Status),
    Log(String),
    Message {
        author: String,
        bot: bool,
        channel_id: u64,
        content: String,
    },
    Interaction {
        command: String,
        user: String,
    },
    CommandsSynced(Vec<String>),
    ModelsFetched(Vec<String>),
    ActionFailed(String),
}

struct Shared {
    http: Mutex<Option<(Arc<Http>, u64)>>,
    registry: Arc<RwLock<CommandRegistry>>,
    agent: Arc<AgentState>,
}

pub struct BotHandle {
    cmd_tx: tokio::sync::mpsc::Sender<BotCommand>,
    pub events: tokio::sync::mpsc::UnboundedReceiver<BotEvent>,
    shared: Arc<Shared>,
}

impl BotHandle {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(32);
        let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            http: Mutex::new(None),
            registry: Arc::new(RwLock::new(CommandRegistry::new())),
            agent: AgentState::new(2),
        });
        let shared_for_thread = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("axis-bot-supervisor".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(supervise(cmd_rx, event_tx, shared_for_thread));
            })
            .expect("supervisor thread");
        Self {
            cmd_tx,
            events,
            shared,
        }
    }

    pub fn send(&self, command: BotCommand) {
        if let Err(error) = self.cmd_tx.blocking_send(command) {
            tracing::debug!(%error, "bot supervisor is gone; command dropped");
        }
    }

    pub fn update_registry(&self, registry: CommandRegistry) {
        *self
            .shared
            .registry
            .write()
            .expect("registry lock poisoned") = registry;
    }

    pub fn update_agent(&self, config: AgentConfig) {
        *self
            .shared
            .agent
            .config
            .write()
            .expect("agent config lock poisoned") = config;
    }

    pub fn agent(&self) -> Arc<AgentState> {
        Arc::clone(&self.shared.agent)
    }

    pub fn is_running(&self) -> bool {
        self.shared
            .http
            .lock()
            .expect("http lock poisoned")
            .is_some()
    }
}

async fn supervise(
    mut cmd_rx: tokio::sync::mpsc::Receiver<BotCommand>,
    event_tx: tokio::sync::mpsc::UnboundedSender<BotEvent>,
    shared: Arc<Shared>,
) {
    let emit = |event: BotEvent| {
        let _ = event_tx.send(event);
    };
    let mut shutdown_tx: Option<tokio::sync::watch::Sender<bool>> = None;

    while let Some(command) = cmd_rx.recv().await {
        match command {
            BotCommand::Connect {
                token,
                application_id,
                commands,
            } => {
                if shutdown_tx.is_some() {
                    emit(BotEvent::Log("a bot is already running".to_owned()));
                    continue;
                }
                let (registry, validation) = crate::commands_model::build_registry(&commands);
                if let Err(error) = validation {
                    emit(BotEvent::ActionFailed(format!(
                        "some commands were not loaded: {error}"
                    )));
                }
                *shared.registry.write().expect("registry lock poisoned") = registry;
                let (shutdown_new, shutdown_rx) = tokio::sync::watch::channel(false);
                shutdown_tx = Some(shutdown_new);
                emit(BotEvent::Log("connecting…".to_owned()));
                emit(BotEvent::Status(Status::Connecting));
                let event_tx = event_tx.clone();
                let shared = Arc::clone(&shared);
                tokio::spawn(run_bot(
                    token,
                    application_id,
                    event_tx,
                    shutdown_rx,
                    shared,
                ));
            }
            BotCommand::Disconnect => {
                if let Some(shutdown) = shutdown_tx.take() {
                    emit(BotEvent::Log("disconnecting…".to_owned()));
                    let _ = shutdown.send(true);
                } else {
                    emit(BotEvent::Log("not connected".to_owned()));
                }
            }
            BotCommand::SendMessage {
                channel_id,
                content,
            } => {
                let Some((http, _)) = shared.http.lock().expect("http lock poisoned").clone()
                else {
                    emit(BotEvent::ActionFailed("not connected".to_owned()));
                    continue;
                };
                match http
                    .create_message(ChannelId::new(channel_id))
                    .content(&content)
                    .await
                {
                    Ok(message) => emit(BotEvent::Log(format!(
                        "sent message {} to channel {channel_id}",
                        message.id
                    ))),
                    Err(error) => emit(BotEvent::ActionFailed(format!(
                        "send to {channel_id} failed: {error}"
                    ))),
                }
            }
            BotCommand::SyncCommands => {
                let Some((http, application_id)) =
                    shared.http.lock().expect("http lock poisoned").clone()
                else {
                    emit(BotEvent::ActionFailed("not connected".to_owned()));
                    continue;
                };
                let registry = shared
                    .registry
                    .read()
                    .expect("registry lock poisoned")
                    .clone();
                match http
                    .bulk_overwrite_global_commands(
                        ApplicationId::new(application_id),
                        registry.to_fields(),
                    )
                    .await
                {
                    Ok(commands) => emit(BotEvent::CommandsSynced(
                        commands.iter().map(|c| c.name.clone()).collect(),
                    )),
                    Err(error) => emit(BotEvent::ActionFailed(format!(
                        "command sync failed: {error}"
                    ))),
                }
            }
            BotCommand::FetchModels { base_url, api_key } => {
                match crate::llm::list_models(&base_url, &api_key).await {
                    Ok(models) => emit(BotEvent::ModelsFetched(models)),
                    Err(error) => emit(BotEvent::ActionFailed(format!(
                        "model list failed: {error}"
                    ))),
                }
            }
        }
    }
}

async fn run_bot(
    token: String,
    application_id: u64,
    event_tx: tokio::sync::mpsc::UnboundedSender<BotEvent>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    shared: Arc<Shared>,
) {
    let mut client = match Client::builder(Token::new(token))
        .intents(
            GatewayIntents::unprivileged()
                | GatewayIntents::MESSAGE_CONTENT
                | GatewayIntents::GUILD_MEMBERS,
        )
        .build()
        .await
    {
        Ok(client) => client,
        Err(error) => {
            let _ = event_tx.send(BotEvent::Status(Status::Failed(format!(
                "client build failed: {error}"
            ))));
            return;
        }
    };

    *shared.http.lock().expect("http lock poisoned") = Some((client.http(), application_id));

    let ready_tx = event_tx.clone();
    let agent_ready = Arc::clone(&shared.agent);
    client.on_ready(move |_ctx, ready| {
        let tx = ready_tx.clone();
        let agent = Arc::clone(&agent_ready);
        async move {
            agent.set_bot_user(ready.user.id.get(), ready.user.username.clone());
            let _ = tx.send(BotEvent::Status(Status::Connected(
                ready.user.username.clone(),
            )));
            let _ = tx.send(BotEvent::Log(format!(
                "connected as {} (session {})",
                ready.user.username, ready.session_id
            )));
            Ok(())
        }
    });

    let message_tx = event_tx.clone();
    let agent_messages = Arc::clone(&shared.agent);
    let http_agent = client.http();
    client.on_message_create(move |ctx, message| {
        let tx = message_tx.clone();
        let agent = Arc::clone(&agent_messages);
        let http = Arc::clone(&http_agent);
        async move {
            let is_bot = message.author.bot.unwrap_or(false);
            let _ = tx.send(BotEvent::Message {
                author: message.author.username.clone(),
                bot: is_bot,
                channel_id: message.channel_id.get(),
                content: message.content.clone(),
            });

            let should_reply = agent.should_process(
                (
                    message.guild_id.map(|id| id.get()).unwrap_or(0),
                    message.channel_id.get(),
                ),
                message.author.id.get(),
                &message.content,
                is_bot,
            ) || agent.is_mentioned(&message.mentions);

            if should_reply {
                agent.activate_conversation(
                    (
                        message.guild_id.map(|id| id.get()).unwrap_or(0),
                        message.channel_id.get(),
                    ),
                    message.author.id.get(),
                );
                let agent = Arc::clone(&agent);
                let http = Arc::clone(&http);
                let scope = (
                    message.guild_id.map(|id| id.get()).unwrap_or(0),
                    message.channel_id.get(),
                );
                let invoker = (message.author.id.get(), message.author.username.clone());
                let prompt = message
                    .content
                    .split_once('>')
                    .map(|(_, rest)| rest.trim())
                    .unwrap_or(&message.content)
                    .to_owned();
                let typing_http = Arc::clone(&http);
                let typing_channel = message.channel_id;
                tokio::spawn(async move {
                    let _ = typing_http.trigger_typing(typing_channel).await;
                });
                tokio::spawn(async move {
                    // Resolve the member to get their nickname.
                    let nick = if scope.0 != 0 {
                        http.get_member(GuildId::new(scope.0), UserId::new(invoker.0))
                            .await
                            .ok()
                            .and_then(|member| member.nick)
                    } else {
                        None
                    };
                    let invoker_with_name = (invoker.0, invoker.1.clone());
                    match crate::agent::run_agent(
                        http.clone(),
                        &agent,
                        scope,
                        invoker_with_name,
                        nick,
                        prompt,
                    )
                    .await
                    {
                        Ok(Some(answer)) => {
                            for chunk in crate::agent::split_reply(&answer) {
                                if let Err(error) = http
                                    .create_message(message.channel_id)
                                    .content(&chunk)
                                    .await
                                {
                                    tracing::warn!(%error, "agent reply failed");
                                }
                            }
                        }
                        Ok(None) => { /* agent chose not to respond */ }
                        Err(error) => {
                            tracing::error!(%error, "agent invocation failed");
                            // Never send errors to Discord.
                        }
                    }
                });
            }

            let _ = ctx;
            Ok(())
        }
    });

    let lifecycle_tx = event_tx.clone();
    client.on_any(move |_ctx, event| {
        let tx = lifecycle_tx.clone();
        async move {
            match event {
                Event::Resumed => {
                    let _ = tx.send(BotEvent::Status(Status::Resumed));
                }
                Event::Reconnecting { close_code } => {
                    let _ = tx.send(BotEvent::Status(Status::Reconnecting(close_code)));
                }
                _ => {}
            }
            Ok(())
        }
    });

    let registry_slot = {
        let registry = shared.registry.read().expect("registry lock poisoned");
        Arc::new(RwLock::new(registry.clone()))
    };
    let interaction_tx = event_tx.clone();
    let event_tx_for_status = event_tx;
    client.on_interaction_create(move |ctx, interaction| {
        let tx = interaction_tx.clone();
        let registry_slot = Arc::clone(&registry_slot);
        async move {
            let name = interaction
                .command_data()
                .map(|data| data.name.clone())
                .unwrap_or_default();
            let user = interaction
                .user
                .as_ref()
                .or_else(|| interaction.member.as_ref().and_then(|m| m.user.as_ref()))
                .map(|user| user.username.clone())
                .unwrap_or_else(|| "unknown".to_owned());
            let _ = tx.send(BotEvent::Interaction {
                command: name,
                user,
            });

            let registry = {
                let guard = registry_slot.read().expect("registry lock poisoned");
                guard.clone()
            };
            if let Err(error) = registry.handle(ctx.http.clone(), interaction).await {
                tracing::error!(%error, "command execution failed");
            }
            Ok(())
        }
    });

    let start = client.start();
    tokio::pin!(start);
    let outcome = tokio::select! {
        result = &mut start => match result {
            Ok(()) => Status::Disconnected("stream ended".to_owned()),
            Err(error) => Status::Failed(format!("gateway stopped: {error}")),
        },
        _ = shutdown_rx.changed() => {
            client.shutdown();
            match (&mut start).await {
                Ok(()) => Status::Disconnected("stopped by user".to_owned()),
                Err(error) => Status::Failed(format!("graceful close failed: {error}")),
            }
        }
    };

    *shared.http.lock().expect("http lock poisoned") = None;
    let _ = event_tx_for_status.send(BotEvent::Status(outcome));
}
