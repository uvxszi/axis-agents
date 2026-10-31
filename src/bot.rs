//! The bridge between the GUI thread and the Axis bot.
//!
//! The GUI thread is owned by eframe; the bot needs a Tokio runtime. A
//! supervisor thread owns the runtime and mediates everything:
//!
//! - GUI → bot: [`BotCommand`]s via `blocking_send` (the GUI is never in
//!   an async context),
//! - bot → GUI: [`BotEvent`]s polled with `try_recv` once per frame.
//!
//! The command registry lives behind a shared lock so commands can be
//! edited and re-synced while the bot is connected.

use std::sync::{Arc, Mutex, RwLock};

use axis::model::Interaction;
use axis::prelude::*;
use axis_commands::CommandRegistry;

/// Commands the GUI can send to the supervisor.
#[derive(Debug, Clone)]
pub enum BotCommand {
    /// Build the client and start the gateway shard.
    Connect { token: String, application_id: u64 },
    /// Close the gateway gracefully and stop.
    Disconnect,
    /// Send a message through the REST client.
    SendMessage { channel_id: u64, content: String },
    /// Bulk-overwrite Discord's global commands with the current registry.
    SyncCommands,
}

/// Connection state reported to the GUI.
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

/// Events the GUI polls.
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
    ActionFailed(String),
}

/// The state shared between the supervisor and the running bot task.
#[derive(Default)]
struct Shared {
    /// The REST client of the currently running bot, plus its application
    /// ID — REST actions from the supervisor (send message, sync
    /// commands) go through this.
    http: Mutex<Option<(Arc<Http>, u64)>>,
    /// The live command registry; swapped by the GUI through
    /// [`BotHandle::update_registry`].
    registry: RwLock<CommandRegistry>,
}

/// A handle to the supervised bot.
pub struct BotHandle {
    cmd_tx: tokio::sync::mpsc::Sender<BotCommand>,
    pub events: tokio::sync::mpsc::UnboundedReceiver<BotEvent>,
    shared: Arc<Shared>,
}

impl BotHandle {
    /// Spawn the supervisor thread with its own Tokio runtime.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(32);
        let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared::default());
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

    /// Send a command from the GUI thread.
    pub fn send(&self, command: BotCommand) {
        // `blocking_send` is legal here: the GUI thread is never inside an
        // async context.
        if let Err(error) = self.cmd_tx.blocking_send(command) {
            tracing::debug!(%error, "bot supervisor is gone; command dropped");
        }
    }

    /// Replace the live command registry (takes effect on the next
    /// interaction).
    pub fn update_registry(&self, registry: CommandRegistry) {
        *self
            .shared
            .registry
            .write()
            .expect("registry lock poisoned") = registry;
    }

    /// Whether a bot is currently running (a REST client is attached).
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

    // One graceful-shutdown signal per running bot.
    let mut shutdown_tx: Option<tokio::sync::watch::Sender<bool>> = None;

    while let Some(command) = cmd_rx.recv().await {
        match command {
            BotCommand::Connect {
                token,
                application_id,
            } => {
                if shutdown_tx.is_some() {
                    emit(BotEvent::Log("a bot is already running".to_owned()));
                    continue;
                }
                let (shutdown_tx_new, shutdown_rx) = tokio::sync::watch::channel(false);
                shutdown_tx = Some(shutdown_tx_new);
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
        .intents(GatewayIntents::unprivileged() | GatewayIntents::MESSAGE_CONTENT)
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

    // Gateway events → GUI. Each closure owns a cloned sender and clones
    // it per call (`send` takes `&self`), keeping them `Fn`.
    let ready_tx = event_tx.clone();
    client.on_ready(move |_ctx, ready| {
        let tx = ready_tx.clone();
        async move {
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
    client.on_message_create(move |_ctx, message| {
        let tx = message_tx.clone();
        async move {
            let _ = tx.send(BotEvent::Message {
                author: message.author.username.clone(),
                bot: message.author.bot.unwrap_or(false),
                channel_id: message.channel_id.get(),
                content: message.content.clone(),
            });
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

    // Interactions → the live registry (cloned out of the lock before any
    // await so the future stays Send).
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
            let user = invocation_user(&interaction);
            let _ = tx.send(BotEvent::Interaction {
                command: name,
                user,
            });

            let registry = {
                let guard = registry_slot.read().expect("registry lock poisoned");
                guard.clone()
            };
            registry.handle(ctx.http.clone(), interaction).await
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
    // `interaction_tx` moved into the handler; report the final status
    // through a fresh sender on the same channel.
    let _ = event_tx_for_status.send(BotEvent::Status(outcome));
}

/// The invoking user's name, from a DM user or the guild member.
fn invocation_user(interaction: &Interaction) -> String {
    interaction
        .user
        .as_ref()
        .or_else(|| {
            interaction
                .member
                .as_ref()
                .and_then(|member| member.user.as_ref())
        })
        .map(|user| user.username.clone())
        .unwrap_or_else(|| "unknown".to_owned())
}
