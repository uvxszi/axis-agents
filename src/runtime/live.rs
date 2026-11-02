//! Live state the GUI renders: agent status, what each agent is thinking
//! right now, counters, and the activity feed.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use crate::llm::ModelInfo;
use crate::memory::now_ms;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Stopped,
    Starting,
    Online,
    Reconnecting,
    Error(String),
}

impl AgentStatus {
    pub fn label(&self) -> String {
        match self {
            AgentStatus::Stopped => "stopped".to_owned(),
            AgentStatus::Starting => "connecting…".to_owned(),
            AgentStatus::Online => "online".to_owned(),
            AgentStatus::Reconnecting => "reconnecting…".to_owned(),
            AgentStatus::Error(e) => format!("error: {e}"),
        }
    }
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            AgentStatus::Starting | AgentStatus::Online | AgentStatus::Reconnecting
        )
    }
}

/// One in-progress piece of thinking (a reply being generated).
#[derive(Debug, Clone)]
pub struct Thought {
    pub id: u64,
    pub channel: String,
    pub about: String,
    pub started: Instant,
    pub phase: String,
    pub reasoning: String,
    pub answer: String,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub messages_seen: u64,
    pub replies: u64,
    pub skipped: u64,
    pub tool_calls: u64,
    pub llm_calls: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub errors: u64,
    pub mod_actions: u64,
    pub memories_saved: u64,
}

#[derive(Debug, Clone)]
pub struct AgentLive {
    pub status: AgentStatus,
    pub bot_user: Option<(u64, String)>,
    pub application_id: Option<u64>,
    pub guilds: Vec<(u64, String)>,
    pub thoughts: Vec<Thought>,
    pub stats: Stats,
    pub started_at: Option<Instant>,
    pub model_info: Option<ModelInfo>,
    pub last_error: Option<String>,
    pub paused: bool,
    pub memory_count: usize,
    /// Voice channels the agent is in.
    pub voice_channels: Vec<String>,
    /// Recently heard in voice: (who, what).
    pub voice_heard: Vec<(String, String)>,
    /// Which connection attempt owns the status (restarts race the old
    /// connection's shutdown).
    pub run_id: u64,
}

impl Default for AgentLive {
    fn default() -> Self {
        Self {
            status: AgentStatus::Stopped,
            bot_user: None,
            application_id: None,
            guilds: Vec::new(),
            thoughts: Vec::new(),
            stats: Stats::default(),
            started_at: None,
            model_info: None,
            last_error: None,
            paused: false,
            memory_count: 0,
            voice_channels: Vec::new(),
            voice_heard: Vec::new(),
            run_id: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    Reply,
    Tool,
    Moderation,
    Memory,
    Coordination,
    Learning,
    System,
    Error,
}

impl FeedKind {
    pub fn icon(self) -> &'static str {
        match self {
            FeedKind::Reply => "💬",
            FeedKind::Tool => "🔧",
            FeedKind::Moderation => "🛡",
            FeedKind::Memory => "🧠",
            FeedKind::Coordination => "🤝",
            FeedKind::Learning => "📚",
            FeedKind::System => "⚙",
            FeedKind::Error => "⚠",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeedItem {
    pub at: i64,
    pub agent_id: String,
    pub agent_name: String,
    pub kind: FeedKind,
    pub text: String,
    pub channel: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaygroundRole {
    User,
    Agent,
    Note,
}

#[derive(Debug, Clone, Default)]
pub struct Playground {
    pub messages: Vec<(PlaygroundRole, String)>,
    pub busy: bool,
    pub thought: Option<Thought>,
}

#[derive(Debug, Clone)]
pub enum Probe {
    Pending,
    Models(Vec<String>),
    Info(ModelInfo),
    Text(String),
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct McpStatus {
    pub connected: bool,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

/// A change to an agent's settings made from the runtime (e.g. it
/// switched its own voice); the app applies and saves it.
pub type ProfilePatch = Box<dyn FnOnce(&mut crate::config::AgentProfile) + Send>;

#[derive(Default)]
pub struct Live {
    pub profile_patches: Mutex<Vec<(String, ProfilePatch)>>,
    agents: Mutex<HashMap<String, AgentLive>>,
    feed: Mutex<VecDeque<FeedItem>>,
    pub playground: Mutex<HashMap<String, Playground>>,
    pub probes: Mutex<HashMap<String, Probe>>,
    pub mcp: Mutex<HashMap<String, McpStatus>>,
    egui: Mutex<Option<egui::Context>>,
    thought_ids: AtomicU64,
}

impl Live {
    pub fn set_egui(&self, ctx: egui::Context) {
        *self.egui.lock().expect("live poisoned") = Some(ctx);
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.egui.lock().expect("live poisoned").as_ref() {
            ctx.request_repaint();
        }
    }

    pub fn agent<R>(&self, id: &str, f: impl FnOnce(&mut AgentLive) -> R) -> R {
        let result = {
            let mut agents = self.agents.lock().expect("live poisoned");
            f(agents.entry(id.to_owned()).or_default())
        };
        self.repaint();
        result
    }

    pub fn snapshot(&self, id: &str) -> AgentLive {
        self.agents
            .lock()
            .expect("live poisoned")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn stat(&self, id: &str, f: impl FnOnce(&mut Stats)) {
        self.agent(id, |live| f(&mut live.stats));
    }

    pub fn push_feed(&self, item: FeedItem) {
        {
            let mut feed = self.feed.lock().expect("live poisoned");
            feed.push_back(item);
            while feed.len() > 600 {
                feed.pop_front();
            }
        }
        self.repaint();
    }

    pub fn feed(
        &self,
        agent: &str,
        name: &str,
        kind: FeedKind,
        text: impl Into<String>,
        channel: Option<String>,
    ) {
        let text = text.into();
        match kind {
            FeedKind::Error => tracing::warn!(agent = name, "{text}"),
            _ => tracing::info!(agent = name, "{text}"),
        }
        self.push_feed(FeedItem {
            at: now_ms(),
            agent_id: agent.to_owned(),
            agent_name: name.to_owned(),
            kind,
            text,
            channel,
        });
    }

    pub fn feed_snapshot(&self, agent: Option<&str>, limit: usize) -> Vec<FeedItem> {
        let feed = self.feed.lock().expect("live poisoned");
        feed.iter()
            .rev()
            .filter(|item| agent.is_none_or(|a| item.agent_id == a))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn clear_feed(&self) {
        self.feed.lock().expect("live poisoned").clear();
    }

    pub fn begin_thought(&self, agent: &str, channel: String, about: String) -> u64 {
        let id = self.thought_ids.fetch_add(1, Ordering::Relaxed) + 1;
        self.agent(agent, |live| {
            live.thoughts.push(Thought {
                id,
                channel,
                about,
                started: Instant::now(),
                phase: "reading the conversation".to_owned(),
                reasoning: String::new(),
                answer: String::new(),
                steps: Vec::new(),
            })
        });
        id
    }

    pub fn update_thought(&self, agent: &str, id: u64, f: impl FnOnce(&mut Thought)) {
        self.agent(agent, |live| {
            if let Some(thought) = live.thoughts.iter_mut().find(|t| t.id == id) {
                f(thought);
            }
        });
    }

    pub fn end_thought(&self, agent: &str, id: u64) {
        self.agent(agent, |live| live.thoughts.retain(|t| t.id != id));
    }
}
