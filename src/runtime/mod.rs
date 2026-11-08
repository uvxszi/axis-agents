//! The runtime: every agent runs here at once, on one Tokio runtime.
//!
//! The GUI owns a [`Supervisor`] and calls it directly; work is spawned
//! onto the runtime and results come back through [`live::Live`].

pub mod activity;
pub mod agent;
pub mod authority;
pub mod brain;
pub mod commands;
pub mod community;
pub mod confirm;
pub mod context;
pub mod github;
pub mod hive;
pub mod hq;
#[cfg(test)]
mod lab;
pub mod levels;
pub mod live;
pub mod moderation;
pub mod pages;
pub mod reflect;
pub mod setup;
pub mod speech;
pub mod tasks;
pub mod tickets;
pub mod translate;
pub mod vision;
pub mod voice;
pub mod world;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

use axis::prelude::{Http, Token};
use tokio::sync::watch;

use crate::config::{AgentProfile, GlobalSettings, ModelConfig};
use crate::llm::Endpoint;
use crate::memory::MemoryStore;
use agent::AgentCore;
use hive::Hive;
use live::{AgentStatus, Live, Probe};

struct Running {
    core: Arc<AgentCore>,
    shutdown: watch::Sender<bool>,
    token: String,
}

pub struct Supervisor {
    pub hive: Arc<Hive>,
    pub live: Arc<Live>,
    runtime: tokio::runtime::Runtime,
    running: Mutex<HashMap<String, Running>>,
    memories: Arc<Mutex<HashMap<String, Arc<MemoryStore>>>>,
    consoles: Mutex<HashMap<String, Arc<AgentCore>>>,
}

impl Supervisor {
    pub fn new(settings: GlobalSettings) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("axis-agents")
            .build()
            .expect("tokio runtime");
        let live = Arc::new(Live::default());
        let hive = Hive::new(Arc::clone(&live), settings.clone());
        let supervisor = Self {
            hive,
            live,
            runtime,
            running: Mutex::new(HashMap::new()),
            memories: Arc::new(Mutex::new(HashMap::new())),
            consoles: Mutex::new(HashMap::new()),
        };
        supervisor.sync_mcp(settings);
        // Save memory and knowledge every few seconds, off the UI thread.
        let hive = Arc::clone(&supervisor.hive);
        let memories = Arc::clone(&supervisor.memories);
        supervisor.runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let hive = Arc::clone(&hive);
                let memories = Arc::clone(&memories);
                let _ = tokio::task::spawn_blocking(move || persist(&hive, &memories)).await;
            }
        });
        supervisor
    }

    /// Flush dirty memory and knowledge to disk now (at exit).
    pub fn persist(&self) {
        persist(&self.hive, &self.memories);
    }

    pub fn memory(&self, agent_id: &str) -> Arc<MemoryStore> {
        let mut memories = self.memories.lock().expect("memories poisoned");
        Arc::clone(memories.entry(agent_id.to_owned()).or_insert_with(|| {
            Arc::new(MemoryStore::load(
                crate::paths::agent_dir(agent_id).join("memory.json"),
            ))
        }))
    }

    pub fn core(&self, agent_id: &str) -> Option<Arc<AgentCore>> {
        self.running
            .lock()
            .expect("running poisoned")
            .get(agent_id)
            .map(|r| Arc::clone(&r.core))
    }

    /// The app's Apply button: register the commands as they're set now
    /// (rate-limited; unchanged ones aren't touched).
    pub fn apply_commands(&self, agent_id: &str) -> bool {
        let Some(core) = self.core(agent_id) else {
            return false;
        };
        self.runtime
            .spawn(async move { commands::request_sync(&core) });
        true
    }

    /// (settings at the last sync, what it did) for the app.
    pub fn commands_state(&self, agent_id: &str) -> Option<(Option<String>, String)> {
        let core = self.core(agent_id)?;
        let applied = core
            .command_sync
            .applied
            .lock()
            .expect("sync poisoned")
            .clone();
        let status = core
            .command_sync
            .status
            .lock()
            .expect("status poisoned")
            .clone();
        Some((applied, status))
    }

    /// The app's Apply button for the status line: update the presence
    /// (and the profile's About Me, when chosen).
    pub fn apply_status(&self, agent_id: &str) -> bool {
        let Some(core) = self.core(agent_id) else {
            return false;
        };
        let profile = core.profile();
        let text = agent::status_line(&profile);
        if let Some(messenger) = core.messenger.lock().expect("messenger poisoned").clone() {
            let _ = messenger.update_presence(agent::presence_for(&profile));
        }
        *core.shown_status.lock().expect("status poisoned") = text.clone();
        if profile.status_in_profile {
            self.runtime.spawn(async move {
                let result = core
                    .http
                    .raw(
                        "PATCH",
                        "/applications/@me",
                        Some(
                            serde_json::json!({ "description": crate::llm::truncate(&text, 400) }),
                        ),
                        None,
                    )
                    .await;
                match result {
                    Ok(_) => core.feed(live::FeedKind::System, "profile About Me updated", None),
                    Err(error) => core.feed(
                        live::FeedKind::Error,
                        format!("couldn't update the About Me: {error}"),
                        None,
                    ),
                }
            });
        }
        true
    }

    /// The status line Discord shows right now (None when not running).
    pub fn shown_status(&self, agent_id: &str) -> Option<String> {
        let core = self.core(agent_id)?;
        let shown = core.shown_status.lock().expect("status poisoned").clone();
        Some(shown)
    }

    pub fn is_running(&self, agent_id: &str) -> bool {
        self.running
            .lock()
            .expect("running poisoned")
            .contains_key(agent_id)
            && self.live.snapshot(agent_id).status.is_running()
    }

    pub fn start(&self, profile: &AgentProfile) {
        if profile.token.trim().is_empty() {
            self.live.agent(&profile.id, |l| {
                l.status = AgentStatus::Error("add the bot token on the Discord tab".to_owned())
            });
            return;
        }
        self.stop(&profile.id);
        let http = match Http::builder(Token::new(profile.token.trim().to_owned())).build() {
            Ok(http) => Arc::new(http),
            Err(error) => {
                self.live.agent(&profile.id, |l| {
                    l.status = AgentStatus::Error(error.to_string())
                });
                return;
            }
        };
        let core = AgentCore::new(
            profile.clone(),
            Arc::clone(&self.hive),
            self.memory(&profile.id),
            http,
            false,
        );
        let (shutdown, receiver) = watch::channel(false);
        let token = profile.token.trim().to_owned();
        self.runtime
            .spawn(agent::run(Arc::clone(&core), token.clone(), receiver));
        self.running.lock().expect("running poisoned").insert(
            profile.id.clone(),
            Running {
                core,
                shutdown,
                token,
            },
        );
    }

    pub fn stop(&self, agent_id: &str) {
        if let Some(running) = self
            .running
            .lock()
            .expect("running poisoned")
            .remove(agent_id)
        {
            let _ = running.shutdown.send(true);
            self.hive.update_peer(agent_id, |p| p.online = false);
        }
    }

    pub fn stop_all(&self) {
        let ids: Vec<String> = self
            .running
            .lock()
            .expect("running poisoned")
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.stop(&id);
        }
    }

    pub fn remove(&self, agent_id: &str) {
        self.stop(agent_id);
        self.hive.remove_peer(agent_id);
        self.consoles
            .lock()
            .expect("consoles poisoned")
            .remove(agent_id);
    }

    /// Apply edited settings to a running agent (restarting only when the
    /// token changed).
    pub fn update(&self, profile: &AgentProfile) {
        let restart = {
            let running = self.running.lock().expect("running poisoned");
            match running.get(&profile.id) {
                Some(running) => {
                    let model_changed = running.core.profile().model != profile.model;
                    running.core.set_profile(profile.clone());
                    self.hive.update_peer(&profile.id, |p| {
                        p.name = profile.name.clone();
                        p.aliases = profile.aliases.clone();
                        p.specialty = profile.specialty.clone();
                        p.moderates = profile.behavior.moderation.enabled;
                        p.learns = profile.behavior.learn_culture;
                    });
                    if model_changed {
                        let core = Arc::clone(&running.core);
                        self.runtime
                            .spawn(async move { core.refresh_model_info().await });
                    }
                    running.token != profile.token.trim()
                }
                None => false,
            }
        };
        if let Some(console) = self
            .consoles
            .lock()
            .expect("consoles poisoned")
            .get(&profile.id)
        {
            console.set_profile(profile.clone());
        }
        if restart {
            self.start(profile);
        }
    }

    pub fn set_paused(&self, agent_id: &str, paused: bool) {
        if let Some(core) = self.core(agent_id) {
            core.paused.store(paused, Ordering::Relaxed);
            if let Some(messenger) = core.messenger.lock().expect("messenger poisoned").clone() {
                let presence = if paused {
                    axis::prelude::Presence::status(axis::gateway::OnlineStatus::Idle)
                        .with_activity(axis::prelude::Activity::custom("⏸ paused"))
                } else {
                    // Back to what was last applied (not unapplied edits).
                    let shown = core.shown_status.lock().expect("status poisoned").clone();
                    axis::prelude::Presence::online()
                        .with_activity(axis::prelude::Activity::custom(shown))
                };
                let _ = messenger.update_presence(presence);
            }
        }
        self.live.agent(agent_id, |l| l.paused = paused);
    }

    pub fn update_settings(&self, settings: GlobalSettings) {
        let mcp_changed = self.hive.settings().mcp_servers != settings.mcp_servers;
        *self.hive.settings.write().expect("settings poisoned") = settings.clone();
        if mcp_changed {
            self.sync_mcp(settings);
        }
    }

    pub fn sync_mcp(&self, settings: GlobalSettings) {
        let hive = Arc::clone(&self.hive);
        self.runtime.spawn(async move {
            hive.mcp
                .sync(settings.mcp_servers.clone(), &hive.live)
                .await;
        });
    }

    pub fn fetch_models(&self, agent_id: &str, model: &ModelConfig) {
        let key = format!("{agent_id}#models");
        self.live
            .probes
            .lock()
            .expect("live poisoned")
            .insert(key.clone(), Probe::Pending);
        let endpoint = Endpoint::from_config(model);
        let live = Arc::clone(&self.live);
        self.runtime.spawn(async move {
            let probe = match endpoint.list_models().await {
                Ok(models) => Probe::Models(models),
                Err(error) => Probe::Failed(error),
            };
            live.probes
                .lock()
                .expect("live poisoned")
                .insert(key, probe);
            live.repaint();
        });
    }

    pub fn detect_model(&self, agent_id: &str, model: &ModelConfig) {
        let key = format!("{agent_id}#info");
        self.live
            .probes
            .lock()
            .expect("live poisoned")
            .insert(key.clone(), Probe::Pending);
        let endpoint = Endpoint::from_config(model);
        let name = model.model.clone();
        let live = Arc::clone(&self.live);
        let core = self.core(agent_id);
        self.runtime.spawn(async move {
            let info = endpoint.model_info(&name).await;
            if let Some(core) = core {
                *core.model_info.write().expect("model info poisoned") = info.clone();
            }
            live.agent_set_model_info(&key, info.clone());
            live.probes
                .lock()
                .expect("live poisoned")
                .insert(key, Probe::Info(info));
            live.repaint();
        });
    }

    /// Reload a model in LM Studio with a given context and parallel
    /// count (through LM Studio's `lms` command line tool).
    pub fn reload_lmstudio(
        &self,
        agent_id: &str,
        model: &ModelConfig,
        context: u32,
        parallel: u32,
    ) {
        let key = format!("{agent_id}#reload");
        self.live
            .probes
            .lock()
            .expect("live poisoned")
            .insert(key.clone(), Probe::Pending);
        let live = Arc::clone(&self.live);
        let name = model.model.clone();
        let endpoint = Endpoint::from_config(model);
        let core = self.core(agent_id);
        self.runtime.spawn(async move {
            let lms = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|home| {
                    std::path::PathBuf::from(home)
                        .join(".lmstudio")
                        .join("bin")
                        .join(if cfg!(windows) { "lms.exe" } else { "lms" })
                })
                .filter(|path| path.exists())
                .unwrap_or_else(|| std::path::PathBuf::from("lms"));
            let run = |args: Vec<String>| {
                let lms = lms.clone();
                async move {
                    let mut command = tokio::process::Command::new(&lms);
                    command.args(&args).stdin(std::process::Stdio::null());
                    #[cfg(windows)]
                    command.creation_flags(0x0800_0000);
                    command.output().await
                }
            };
            let _ = run(vec!["unload".into(), name.clone()]).await;
            let result = run(vec![
                "load".into(),
                name.clone(),
                "--context-length".into(),
                context.to_string(),
                "--parallel".into(),
                parallel.to_string(),
            ])
            .await;
            let probe = match result {
                Ok(out) if out.status.success() => {
                    let info = endpoint.model_info(&name).await;
                    if let Some(core) = core {
                        *core.model_info.write().expect("model info poisoned") = info.clone();
                    }
                    Probe::Info(info)
                }
                Ok(out) => Probe::Failed(format!(
                    "lms load failed: {}",
                    crate::llm::truncate(
                        &String::from_utf8_lossy(&out.stderr).trim().to_string(),
                        300
                    )
                )),
                Err(error) => Probe::Failed(format!("couldn't run LM Studio's lms tool: {error}")),
            };
            live.probes
                .lock()
                .expect("live poisoned")
                .insert(key, probe);
            live.repaint();
        });
    }

    /// Run a voice settings action (check, install, preview) in the
    /// background; its result lands in `live.probes[key]`.
    pub fn voice_task<F>(&self, key: String, task: F)
    where
        F: std::future::Future<Output = Result<String, String>> + Send + 'static,
    {
        self.live
            .probes
            .lock()
            .expect("live poisoned")
            .insert(key.clone(), Probe::Pending);
        let live = Arc::clone(&self.live);
        self.runtime.spawn(async move {
            let probe = match task.await {
                Ok(text) => Probe::Text(text),
                Err(error) => Probe::Failed(error),
            };
            live.probes
                .lock()
                .expect("live poisoned")
                .insert(key, probe);
            live.repaint();
        });
    }

    /// List the Windows voices into `live.probes[key]`.
    pub fn load_windows_voices(&self, key: String) {
        self.live
            .probes
            .lock()
            .expect("live poisoned")
            .insert(key.clone(), Probe::Pending);
        let live = Arc::clone(&self.live);
        self.runtime.spawn(async move {
            let voices = crate::runtime::speech::windows_voices().await;
            live.probes
                .lock()
                .expect("live poisoned")
                .insert(key, Probe::Models(voices));
            live.repaint();
        });
    }

    /// Chat with an agent locally. Uses the running agent when there is
    /// one (so it shares memory state), else an offline copy.
    pub fn console_send(&self, profile: &AgentProfile, text: String) {
        let core = match self.core(&profile.id) {
            Some(core) => core,
            None => {
                let mut consoles = self.consoles.lock().expect("consoles poisoned");
                Arc::clone(consoles.entry(profile.id.clone()).or_insert_with(|| {
                    let http = Arc::new(
                        Http::builder(Token::new("offline".to_owned()))
                            .build()
                            .expect("offline http client"),
                    );
                    let core = AgentCore::new(
                        profile.clone(),
                        Arc::clone(&self.hive),
                        self.memory(&profile.id),
                        http,
                        true,
                    );
                    self.hive.upsert_peer(agent::peer_from(profile));
                    core
                }))
            }
        };
        core.set_profile(profile.clone());
        let warm = Arc::clone(&core);
        self.runtime.spawn(async move {
            if warm
                .model_info
                .read()
                .expect("model info poisoned")
                .context_window
                .is_none()
            {
                warm.refresh_model_info().await;
            }
            brain::playground_turn(warm, text).await;
        });
    }

    pub fn console_clear(&self, agent_id: &str) {
        self.live
            .playground
            .lock()
            .expect("live poisoned")
            .remove(agent_id);
    }

    pub fn shutdown(&self) {
        self.stop_all();
        let hive = Arc::clone(&self.hive);
        self.runtime.block_on(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            hive.mcp.shutdown().await;
        });
        self.persist();
    }
}

fn persist(hive: &Hive, memories: &Mutex<HashMap<String, Arc<MemoryStore>>>) {
    hive.knowledge.save_dirty();
    let stores: Vec<(String, Arc<MemoryStore>)> = memories
        .lock()
        .expect("memories poisoned")
        .iter()
        .map(|(id, store)| (id.clone(), Arc::clone(store)))
        .collect();
    for (id, store) in stores {
        store.save_if_dirty();
        let count = store.len();
        hive.live.agent(&id, |l| l.memory_count = count);
    }
}

impl Live {
    fn agent_set_model_info(&self, key: &str, info: crate::llm::ModelInfo) {
        if let Some(id) = key.strip_suffix("#info") {
            self.agent(id, |l| l.model_info = Some(info));
        }
    }
}
