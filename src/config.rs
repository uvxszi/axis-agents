//! Everything the app persists about agents and global settings.
//!
//! Stored as JSON in the data directory (see [`crate::paths`]). Every
//! struct is `#[serde(default)]`, so configs written by older versions
//! load cleanly and new fields take their defaults. Saves are atomic
//! (write to a temp file, then rename), so a crash mid-save never leaves
//! a half-written config behind.

use serde::{Deserialize, Serialize};

use crate::llm::ProviderKind;

pub const CONFIG_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub agents: Vec<AgentProfile>,
    pub settings: GlobalSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            agents: Vec::new(),
            settings: GlobalSettings::default(),
        }
    }
}

/// One agent: its own Discord bot, persona, model, and behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentProfile {
    /// Stable ID used for data folders; never shown to Discord.
    pub id: String,
    /// Start this agent when the app starts.
    pub autostart: bool,
    pub name: String,
    /// Other names people use for this agent (trigger words).
    pub aliases: Vec<String>,
    /// Personality, voice, and standing instructions.
    pub persona: String,
    /// One line other agents see: what this agent is best at.
    pub specialty: String,
    /// UI accent color.
    pub accent: [u8; 3],
    /// Custom status shown under the bot's name.
    pub status_text: String,
    /// Also put the status line in the bot's profile (About Me), which
    /// stays visible while it's offline.
    #[serde(default)]
    pub status_in_profile: bool,
    /// The bot token (Developer Portal → Bot).
    pub token: String,
    pub model: ModelConfig,
    pub behavior: BehaviorConfig,
    pub tools: ToolConfig,
    pub voice: VoiceConfig,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            id: new_id(),
            autostart: true,
            name: "Nova".to_owned(),
            aliases: Vec::new(),
            persona: "You are friendly, sharp, and genuinely helpful. You speak casually, like a \
                      regular member of the server who happens to know a lot. You keep things \
                      short unless someone asks for depth."
                .to_owned(),
            specialty: "general help and conversation".to_owned(),
            accent: [124, 108, 255],
            status_text: String::new(),
            status_in_profile: false,
            token: String::new(),
            model: ModelConfig::default(),
            behavior: BehaviorConfig::default(),
            tools: ToolConfig::default(),
            voice: VoiceConfig::default(),
        }
    }
}

impl AgentProfile {
    /// The words that summon this agent: its name and aliases, lowercase.
    pub fn trigger_words(&self) -> Vec<String> {
        std::iter::once(self.name.as_str())
            .chain(self.aliases.iter().map(String::as_str))
            .map(|word| word.trim().to_lowercase())
            .filter(|word| word.chars().count() >= 2)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub provider: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    /// Max tokens the model may generate per call.
    pub max_output_tokens: u32,
    /// Most tokens a thinking model may spend thinking before it answers
    /// (LM Studio); 0 = automatic (as much as the reply itself).
    pub thinking_budget: u32,
    /// The model's context window; 0 = detect automatically.
    pub context_window: u32,
    /// Send images to the model (multimodal models only).
    pub vision: bool,
    pub tool_mode: ToolMode,
    /// How much a thinking model (Qwen 3.5, DeepSeek R1, …) reasons
    /// before answering, per kind of work.
    pub thinking: ThinkingConfig,
    pub streaming: bool,
    /// Prompt size: auto picks compact prompts for small context windows.
    pub prompt_style: PromptStyle,
    /// Optional embeddings model for semantic memory search.
    pub embedding_model: String,
    /// Optional small vision model (same server) that describes images
    /// for the main model.
    pub vision_model: String,
    /// Optional cheaper model for background work (summaries, culture
    /// notes, moderation checks); empty = use the main model.
    pub background_model: String,
    pub request_timeout_secs: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::LmStudio,
            base_url: ProviderKind::LmStudio.default_base_url().to_owned(),
            api_key: String::new(),
            model: String::new(),
            temperature: 0.7,
            max_output_tokens: 1024,
            thinking_budget: 0,
            context_window: 0,
            vision: false,
            tool_mode: ToolMode::Auto,
            thinking: ThinkingConfig::default(),
            streaming: true,
            prompt_style: PromptStyle::Auto,
            embedding_model: String::new(),
            vision_model: String::new(),
            background_model: String::new(),
            request_timeout_secs: 180,
        }
    }
}

/// Where the agent's name has to appear for the message to count as
/// addressed to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamePosition {
    /// The name anywhere in the message counts.
    #[default]
    Anywhere,
    /// Only when the name is the first or second word ("magi help",
    /// "hey magi, ...") — talking *about* the agent doesn't summon it.
    Start,
}

/// Thinking for reasoning models, sent as `reasoning_effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningMode {
    /// Whatever the model does by default.
    Default,
    /// No thinking: much faster, slightly less careful.
    Off,
    Low,
    Medium,
    High,
}

impl ReasoningMode {
    pub const ALL: [ReasoningMode; 5] = [
        ReasoningMode::Default,
        ReasoningMode::Off,
        ReasoningMode::Low,
        ReasoningMode::Medium,
        ReasoningMode::High,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ReasoningMode::Default => "Model default",
            ReasoningMode::Off => "Off (fastest)",
            ReasoningMode::Low => "Low",
            ReasoningMode::Medium => "Medium",
            ReasoningMode::High => "High",
        }
    }
    pub fn effort(self) -> Option<&'static str> {
        match self {
            ReasoningMode::Default => None,
            ReasoningMode::Off => Some("none"),
            ReasoningMode::Low => Some("low"),
            ReasoningMode::Medium => Some("medium"),
            ReasoningMode::High => Some("high"),
        }
    }
}

/// Thinking per kind of work. Off is several times faster on local
/// thinking models; the default keeps the model's own behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThinkingConfig {
    /// Answering someone who talks to the agent (and HQ, /ask).
    pub replies: ReasoningMode,
    /// Joining a conversation nobody asked it into.
    pub chiming_in: ReasoningMode,
    /// Standing tasks (forum checks, schedules, welcomes).
    pub tasks: ReasoningMode,
    /// Looking at images.
    pub vision: ReasoningMode,
    /// Summaries, culture notes, moderation reviews.
    pub background: ReasoningMode,
    /// Talking in a voice channel (thinking adds a long pause).
    pub voice: ReasoningMode,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self {
            replies: ReasoningMode::Default,
            chiming_in: ReasoningMode::Off,
            tasks: ReasoningMode::Off,
            vision: ReasoningMode::Off,
            background: ReasoningMode::Off,
            voice: ReasoningMode::Off,
        }
    }
}

/// Voice channels: hearing people and talking back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Listen to people in the voice channel and answer out loud.
    pub listen: bool,
    pub reply: VoiceReply,
    /// Stop talking when someone talks over the agent.
    pub interruptible: bool,
    /// Also post what the agent says in the voice channel's text chat.
    pub post_transcript: bool,
    /// Keep a live transcript in the call's text chat: what it hears and
    /// says, as small silent lines (`-#`).
    pub live_transcript: bool,
    /// Leave once no people are left in the channel.
    pub leave_when_alone: bool,
    /// How long a pause (ms) means someone finished talking.
    pub pause_ms: u32,
    /// Loudness (RMS) above which a frame counts as speech. Lower hears
    /// quieter speech (and more noise); higher needs louder talkers.
    pub speech_rms: u16,
    /// Loudness (RMS) someone must reach to interrupt the agent; 0 means
    /// the same as `speech_rms`.
    pub interrupt_rms: u16,
    /// Interruptions must be this loud for this long (rolling ms window),
    /// so stray claps don't cut the agent off.
    pub interrupt_window_ms: u64,
    /// How long after the agent speaks to keep dropping transcripts that
    /// just echo its own voice back through open mics.
    pub echo_guard_secs: u64,
    pub stt: SttConfig,
    pub tts: TtsConfig,
    /// Python used for local Whisper and Piper.
    pub python: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            listen: true,
            reply: VoiceReply::Conversation,
            interruptible: true,
            post_transcript: false,
            live_transcript: false,
            leave_when_alone: true,
            pause_ms: 1300,
            speech_rms: 450,
            interrupt_rms: 0,
            interrupt_window_ms: 600,
            echo_guard_secs: 8,
            stt: SttConfig::default(),
            tts: TtsConfig::default(),
            python: "python".to_owned(),
        }
    }
}

/// When the agent answers what it hears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceReply {
    /// Only when its name is said.
    NameOnly,
    /// When its name is said, in an ongoing back-and-forth, or when it's
    /// the only one there to talk to.
    Conversation,
}

impl VoiceReply {
    pub const ALL: [Self; 2] = [Self::Conversation, Self::NameOnly];
    pub fn label(self) -> &'static str {
        match self {
            Self::NameOnly => "Only when its name is said",
            Self::Conversation => "Name, ongoing conversation, or one-on-one",
        }
    }
}

/// Speech-to-text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    pub engine: SttEngine,
    /// Local Whisper model: tiny, base, small, medium, large-v3, or an
    /// English-only variant like base.en.
    pub whisper_model: String,
    /// Language code (en, de…); empty = detect.
    pub language: String,
    /// OpenAI-compatible endpoint (OpenAI, Groq, a local Whisper server).
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            engine: SttEngine::LocalWhisper,
            whisper_model: "base".to_owned(),
            language: String::new(),
            base_url: "https://api.openai.com/v1".to_owned(),
            api_key: String::new(),
            model: "whisper-1".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SttEngine {
    /// faster-whisper on this PC (Python).
    LocalWhisper,
    /// An OpenAI-compatible /audio/transcriptions endpoint.
    OpenAi,
}

impl SttEngine {
    pub const ALL: [Self; 2] = [Self::LocalWhisper, Self::OpenAi];
    pub fn label(self) -> &'static str {
        match self {
            Self::LocalWhisper => "Whisper on this PC",
            Self::OpenAi => "OpenAI-compatible API",
        }
    }
}

/// Text-to-speech.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    pub engine: TtsEngine,
    /// Kokoro voice, e.g. af_heart (model downloaded on first use).
    pub kokoro_voice: String,
    /// Piper voice, e.g. en_US-lessac-medium (downloaded on first use).
    pub piper_voice: String,
    /// Windows voice name; empty = the system default.
    pub windows_voice: String,
    /// 0.5 – 2.0.
    pub speed: f32,
    /// 0.1 – 2.0.
    pub volume: f32,
    /// OpenAI-compatible /audio/speech endpoint (OpenAI, Kokoro-FastAPI…).
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            engine: TtsEngine::Kokoro,
            kokoro_voice: "af_heart".to_owned(),
            piper_voice: "en_US-lessac-medium".to_owned(),
            windows_voice: String::new(),
            speed: 1.0,
            volume: 1.0,
            base_url: "https://api.openai.com/v1".to_owned(),
            api_key: String::new(),
            model: "tts-1".to_owned(),
            voice: "alloy".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TtsEngine {
    /// Kokoro: small, natural-sounding AI voices on this PC.
    Kokoro,
    /// Piper: small neural voices that run on the CPU.
    Piper,
    /// The voices built into Windows.
    Windows,
    /// An OpenAI-compatible /audio/speech endpoint.
    OpenAi,
}

impl TtsEngine {
    pub const ALL: [Self; 4] = [Self::Kokoro, Self::Piper, Self::Windows, Self::OpenAi];
    pub fn label(self) -> &'static str {
        match self {
            Self::Kokoro => "Kokoro (natural AI voices, on this PC)",
            Self::Piper => "Piper (small AI voices, on this PC)",
            Self::Windows => "Windows voices",
            Self::OpenAi => "OpenAI-compatible API",
        }
    }
}

/// How tools are offered to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolMode {
    /// Native function calling; falls back to text protocol if the
    /// provider rejects tools.
    Auto,
    /// Always native function calling.
    Native,
    /// A text protocol in the prompt — for models without tool support.
    Text,
    /// No tools at all.
    Off,
}

impl ToolMode {
    pub const ALL: [ToolMode; 4] = [
        ToolMode::Auto,
        ToolMode::Native,
        ToolMode::Text,
        ToolMode::Off,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ToolMode::Auto => "Auto (native, text fallback)",
            ToolMode::Native => "Native function calling",
            ToolMode::Text => "Text protocol (small models)",
            ToolMode::Off => "Off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptStyle {
    Auto,
    Full,
    Compact,
}

impl PromptStyle {
    pub const ALL: [PromptStyle; 3] = [PromptStyle::Auto, PromptStyle::Full, PromptStyle::Compact];
    pub fn label(self) -> &'static str {
        match self {
            PromptStyle::Auto => "Auto",
            PromptStyle::Full => "Full (large models)",
            PromptStyle::Compact => "Compact (small models)",
        }
    }
}

/// How the agent shows that it is working on a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingStyle {
    /// Typing indicator only; the reply appears when done.
    TypingOnly,
    /// A "thinking…" message that is edited into the answer.
    Placeholder,
    /// The placeholder also shows tool steps and the answer as it streams.
    Live,
    /// Like Live, plus the model's reasoning when it exposes any.
    LiveWithReasoning,
}

impl ThinkingStyle {
    pub const ALL: [ThinkingStyle; 4] = [
        ThinkingStyle::TypingOnly,
        ThinkingStyle::Placeholder,
        ThinkingStyle::Live,
        ThinkingStyle::LiveWithReasoning,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ThinkingStyle::TypingOnly => "Typing indicator only",
            ThinkingStyle::Placeholder => "\"Thinking…\" message, edited into the reply",
            ThinkingStyle::Live => "Live: show steps and stream the reply",
            ThinkingStyle::LiveWithReasoning => "Live + show model reasoning",
        }
    }
    pub fn uses_placeholder(self) -> bool {
        !matches!(self, ThinkingStyle::TypingOnly)
    }
}

/// What the agent may do on its own initiative (not on someone's request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Autonomy {
    /// Only talks; never acts unless a person with the right permissions asks.
    Conversational,
    /// May react, open threads, and pin on its own.
    Helper,
    /// May also moderate rule-breakers (delete, warn, short timeouts).
    Moderator,
    /// May also manage channels and roles when it judges it necessary.
    Manager,
}

impl Autonomy {
    pub const ALL: [Autonomy; 4] = [
        Autonomy::Conversational,
        Autonomy::Helper,
        Autonomy::Moderator,
        Autonomy::Manager,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Autonomy::Conversational => "Conversational — acts only when asked",
            Autonomy::Helper => "Helper — reacts, threads, pins on its own",
            Autonomy::Moderator => "Moderator — also moderates on its own",
            Autonomy::Manager => "Manager — also manages channels & roles",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyLength {
    Brief,
    Normal,
    Detailed,
}

impl ReplyLength {
    pub const ALL: [ReplyLength; 3] = [
        ReplyLength::Brief,
        ReplyLength::Normal,
        ReplyLength::Detailed,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ReplyLength::Brief => "Brief",
            ReplyLength::Normal => "Normal",
            ReplyLength::Detailed => "Detailed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub respond_to_mentions: bool,
    pub respond_to_name: bool,
    /// Where in a message the agent's name counts as being called:
    /// anywhere in the text, or only as the first or second word.
    #[serde(default)]
    pub name_position: NamePosition,
    pub respond_to_replies: bool,
    pub respond_in_dms: bool,
    /// 0–100: how often the agent joins conversations it was not
    /// addressed in.
    pub chattiness: u8,
    /// On start, work through what piled up while it was off (missed
    /// schedule ticks, new GitHub releases). Off = start from now.
    #[serde(default = "default_true")]
    pub catch_up_on_start: bool,
    /// After talking with someone, keep listening to them (without a new
    /// mention) for this long.
    pub conversation_window_secs: u64,
    /// Wait this long for follow-up messages before answering.
    pub debounce_ms: u64,
    pub thinking_style: ThinkingStyle,
    /// Add "thought for 3.1s · 2 tools" under replies.
    pub show_footer: bool,
    /// React with this emoji while working (empty = no reaction).
    pub working_reaction: String,
    /// Show the thinking message in small text (`-#`) instead of normal size.
    pub small_thinking: bool,
    /// Long answers get page buttons (otherwise: several messages).
    pub paginate: bool,
    /// Emojis in the thinking message. Custom emojis (`<a:name:id>`) work
    /// only for the bot that owns them.
    pub emojis: ThinkingEmojis,
    pub reply_length: ReplyLength,
    pub max_tool_rounds: u32,
    /// Only these channel IDs (empty = everywhere the bot can see).
    pub allowed_channels: Vec<u64>,
    pub ignored_channels: Vec<u64>,
    /// Answer bots that are not part of this app's agent team.
    pub reply_to_other_bots: bool,
    pub autonomy: Autonomy,
    pub moderation: ModerationConfig,
    /// Observe the server and keep notes on its culture and people.
    pub learn_culture: bool,
    /// Reflect on the culture after this many new messages.
    pub reflect_every_messages: u32,
    /// Use the private agent coordination channel.
    pub use_hq: bool,
    /// Register the essential slash commands (/ask, /memories, …).
    pub slash_commands: bool,
    /// Built-in commands turned off (by name).
    pub disabled_commands: Vec<String>,
    /// Your own commands that reply with fixed text.
    pub custom_commands: Vec<CustomCommand>,
}

/// What a custom command does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandMode {
    /// Reply with the text ({user}, {server}, {channel}, {input} filled in).
    #[default]
    Reply,
    /// The agent carries out the text as an instruction, with its tools.
    Agent,
}

impl CommandMode {
    pub const ALL: [CommandMode; 2] = [CommandMode::Reply, CommandMode::Agent];

    pub fn label(self) -> &'static str {
        match self {
            CommandMode::Reply => "reply with the text",
            CommandMode::Agent => "AI does what the text says",
        }
    }
}

/// A custom slash command set in the app.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    /// The reply, or the instruction (Agent mode).
    pub reply: String,
    pub mode: CommandMode,
    /// Only the person who used it sees the reply.
    pub private: bool,
    /// Adds an "input" text option ({input}).
    pub takes_input: bool,
    /// Servers it's registered in; empty = every server (and DMs).
    pub servers: Vec<u64>,
}

impl CustomCommand {
    /// Discord's rule for command names.
    pub fn valid_name(name: &str) -> bool {
        (1..=32).contains(&name.chars().count())
            && name
                .chars()
                .all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    }
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            respond_to_mentions: true,
            respond_to_name: true,
            name_position: Default::default(),
            respond_to_replies: true,
            respond_in_dms: true,
            chattiness: 15,
            catch_up_on_start: true,
            conversation_window_secs: 180,
            debounce_ms: 1200,
            thinking_style: ThinkingStyle::Live,
            show_footer: true,
            working_reaction: String::new(),
            small_thinking: false,
            paginate: true,
            emojis: ThinkingEmojis::default(),
            reply_length: ReplyLength::Normal,
            max_tool_rounds: 8,
            allowed_channels: Vec::new(),
            ignored_channels: Vec::new(),
            reply_to_other_bots: false,
            autonomy: Autonomy::Helper,
            moderation: ModerationConfig::default(),
            learn_culture: true,
            reflect_every_messages: 120,
            use_hq: true,
            slash_commands: true,
            disabled_commands: Vec::new(),
            custom_commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThinkingEmojis {
    /// Next to "thinking…".
    pub thinking: String,
    /// A tool step that is pulling up information or acting.
    pub working: String,
    /// A step that reads: past chats, memory, websites, docs.
    pub reading: String,
    /// The "thought for 3.2s" footer.
    pub thought: String,
}

impl Default for ThinkingEmojis {
    fn default() -> Self {
        Self {
            thinking: "💭".to_owned(),
            working: "⏳".to_owned(),
            reading: "📖".to_owned(),
            thought: "💭".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModerationConfig {
    /// Watch messages for rule-breaking.
    pub enabled: bool,
    /// Ask the model to confirm before acting on a flagged message.
    pub llm_review: bool,
    /// Words/phrases that are always removed (case-insensitive).
    pub blocked_words: Vec<String>,
    /// Remove Discord invite links from non-staff.
    pub block_invites: bool,
    /// Distinct user mentions in one message that count as mass-mention.
    pub max_mentions: u32,
    /// Messages within 8 seconds that count as flooding.
    pub flood_messages: u32,
    /// Identical messages within 60 seconds that count as spam.
    pub duplicate_messages: u32,
    /// Timeout length for repeat offenders.
    pub timeout_minutes: u32,
    /// Strikes (within 24h) before a timeout.
    pub strikes_before_timeout: u32,
    /// Channel name for the moderation log (empty = HQ only).
    pub log_channel: String,
    /// Extra rules beyond the server's rules channel.
    pub extra_rules: String,
}

impl Default for ModerationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            llm_review: true,
            blocked_words: Vec::new(),
            block_invites: false,
            max_mentions: 6,
            flood_messages: 6,
            duplicate_messages: 3,
            timeout_minutes: 10,
            strikes_before_timeout: 3,
            log_channel: "mod-log".to_owned(),
            extra_rules: String::new(),
        }
    }
}

/// Which tool groups the agent may use. Every tool is additionally gated
/// by the requester's real Discord permissions at call time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    pub server_info: bool,
    pub messaging: bool,
    pub reactions: bool,
    pub threads: bool,
    pub polls: bool,
    pub voice: bool,
    pub moderation: bool,
    pub roles: bool,
    pub channels: bool,
    pub web_search: bool,
    pub web_fetch: bool,
    pub memory: bool,
    pub coordination: bool,
    pub utilities: bool,
    pub images: bool,
    pub mcp: bool,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            server_info: true,
            messaging: true,
            reactions: true,
            threads: true,
            polls: true,
            voice: true,
            moderation: true,
            roles: true,
            channels: true,
            web_search: true,
            web_fetch: true,
            memory: true,
            coordination: true,
            utilities: true,
            images: true,
            mcp: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    pub search: SearchConfig,
    /// Let `fetch_url` reach private/LAN addresses (off: SSRF guard).
    pub allow_private_fetch: bool,
    pub mcp_servers: Vec<McpServerConfig>,
    pub hq_category: String,
    pub hq_channel: String,
    /// Concurrent requests per LLM endpoint (shared by all agents on it).
    pub endpoint_parallelism: u32,
    /// Consecutive agent-to-agent messages in a public channel before
    /// agents stop replying to each other (loop guard).
    pub max_agent_chain: u32,
    pub ui_scale: f32,
    /// Optional GitHub token for watching repos (higher rate limit,
    /// private repos).
    pub github_token: String,
    /// Every operational knob in one place — no magic number stays buried
    /// in code. Editable in Settings, live for every agent.
    pub tunables: Tunables,
}

/// The knobs. Defaults match Discord's real limits and the behavior the
/// probe runs verified against a live small model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tunables {
    /// One file upload by this bot. 25 MiB is Discord's hard API cap —
    /// the 8/10 MB figures are human limits and must never be used.
    pub upload_limit_bytes: usize,
    /// Discord's cap applies to the WHOLE request — every file on the
    /// message plus the message payload together. Files are sized and
    /// batched to stay this far under the cap, so a "25 MB" ask produces
    /// the largest file that actually sends.
    pub attach_headroom_bytes: usize,
    /// Total sandbox size per channel before the oldest files are pruned.
    pub sandbox_limit_bytes: u64,
    /// Most parts an oversized duplicate is split into for uploads.
    pub max_duplicate_parts: usize,
    /// Rewrites of a still-short file before the app fills it itself.
    pub file_rewrite_autofill: u32,
    /// "Not finished" pushes before the app fills the file itself.
    pub size_nudge_limit: u32,
    /// Pushes for promised work that never ran a tool.
    pub follow_through_limit: u32,
    /// Scratchpad entries kept per channel.
    pub scratch_entries: usize,
    /// Scratchpad chars per entry.
    pub scratch_entry_chars: usize,
    /// Entries recall_scratch shows at once.
    pub scratch_recall: usize,
    /// Process-journal entries kept after jobs finish.
    pub process_journal_entries: usize,
    /// Message contents remembered for edit-dedup.
    pub sent_content_entries: usize,
    /// Minimum ms between status-message edits.
    pub edit_min_interval_ms: u64,
    /// A line-break cut within this many bytes of the target still counts
    /// as exact; beyond it the file is cut mid-line to hit the byte.
    pub exact_size_tolerance_bytes: usize,
    /// Text longer than this many chars…
    pub prose_min_chars: usize,
    /// …with at least this many words reads as prose (which `repeat`
    /// refuses and write_section handles).
    pub prose_min_words: usize,
    /// Temp files (the `_tmp/` scratch area) older than this are removed.
    pub temp_file_lifetime_hours: u64,
    /// run_script execution cap, seconds.
    pub script_timeout_secs: u64,
}

impl Default for Tunables {
    fn default() -> Self {
        Self {
            upload_limit_bytes: 25 * 1024 * 1024,
            attach_headroom_bytes: 256 * 1024,
            sandbox_limit_bytes: 128 * 1024 * 1024,
            max_duplicate_parts: 10,
            file_rewrite_autofill: 2,
            size_nudge_limit: 3,
            follow_through_limit: 2,
            scratch_entries: 120,
            scratch_entry_chars: 1600,
            scratch_recall: 15,
            process_journal_entries: 300,
            sent_content_entries: 512,
            edit_min_interval_ms: 1400,
            exact_size_tolerance_bytes: 64,
            prose_min_chars: 240,
            prose_min_words: 40,
            temp_file_lifetime_hours: 24,
            script_timeout_secs: 30,
        }
    }
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            search: SearchConfig::default(),
            allow_private_fetch: false,
            mcp_servers: Vec::new(),
            hq_category: "🤖 Agent HQ".to_owned(),
            hq_channel: "agent-hq".to_owned(),
            endpoint_parallelism: 2,
            max_agent_chain: 4,
            ui_scale: 1.0,
            github_token: String::new(),
            tunables: Tunables::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchProvider {
    DuckDuckGo,
    Brave,
    Tavily,
    SearXng,
}

impl SearchProvider {
    pub const ALL: [SearchProvider; 4] = [
        SearchProvider::DuckDuckGo,
        SearchProvider::Brave,
        SearchProvider::Tavily,
        SearchProvider::SearXng,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SearchProvider::DuckDuckGo => "DuckDuckGo (no key)",
            SearchProvider::Brave => "Brave Search API",
            SearchProvider::Tavily => "Tavily",
            SearchProvider::SearXng => "SearXNG (self-hosted)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    pub api_key: String,
    pub searxng_url: String,
    pub max_results: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: SearchProvider::DuckDuckGo,
            api_key: String::new(),
            searxng_url: "http://localhost:8888".to_owned(),
            max_results: 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpTransport {
    Stdio,
    Http,
}

/// Who may trigger a tool (or an MCP server's tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AccessLevel {
    Everyone,
    /// Anyone with at least one role.
    Trusted,
    /// Manage Messages, Kick, Ban, or Moderate Members.
    Moderator,
    Admin,
    Owner,
}

impl AccessLevel {
    pub const ALL: [AccessLevel; 5] = [
        AccessLevel::Everyone,
        AccessLevel::Trusted,
        AccessLevel::Moderator,
        AccessLevel::Admin,
        AccessLevel::Owner,
    ];
    pub fn label(self) -> &'static str {
        match self {
            AccessLevel::Everyone => "Everyone",
            AccessLevel::Trusted => "Members with a role",
            AccessLevel::Moderator => "Moderators",
            AccessLevel::Admin => "Admins",
            AccessLevel::Owner => "Server owner only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Who may cause these tools to run. MCP tools can touch your PC, so
    /// this defaults to admins.
    pub access: AccessLevel,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: "server".to_owned(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            url: String::new(),
            headers: Vec::new(),
            access: AccessLevel::Admin,
        }
    }
}

fn default_true() -> bool {
    true
}

pub fn new_id() -> String {
    format!("{:016x}", fastrand::u64(..))
}

pub fn load(path: &std::path::Path) -> Option<AppConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<AppConfig>(&text) {
        Ok(mut config) => {
            for agent in &mut config.agents {
                if agent.id.is_empty() {
                    agent.id = new_id();
                }
            }
            config.version = CONFIG_VERSION;
            Some(config)
        }
        Err(error) => {
            // Keep the unreadable file for the user instead of silently
            // overwriting it on the next save.
            tracing::error!(%error, "config is unreadable; backing it up");
            let backup = path.with_extension("broken.json");
            let _ = std::fs::copy(path, backup);
            None
        }
    }
}

pub fn save(path: &std::path::Path, config: &AppConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config)
        .map_err(|error| format!("config serialization failed: {error}"))?;
    crate::paths::write_atomic(path, text.as_bytes())
}

/// Import the single agent from the old `axis_bot_studio.json` (written by
/// the previous version of this app next to the exe), if present.
pub fn migrate_legacy() -> Option<AppConfig> {
    let legacy = std::env::current_exe()
        .ok()?
        .parent()?
        .join("axis_bot_studio.json");
    let text = std::fs::read_to_string(&legacy).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let agent = value.get("agent")?;
    let mut profile = AgentProfile::default();
    if let Some(name) = agent.get("name").and_then(|v| v.as_str()) {
        if !name.trim().is_empty() {
            profile.name = name.trim().to_owned();
        }
    }
    if let Some(instructions) = agent.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.trim().is_empty() {
            profile.persona = instructions.trim().to_owned();
        }
    }
    if let Some(url) = agent.get("base_url").and_then(|v| v.as_str()) {
        profile.model.base_url = url.to_owned();
    }
    if let Some(model) = agent.get("model").and_then(|v| v.as_str()) {
        if model != "local-model" {
            profile.model.model = model.to_owned();
        }
    }
    if let Some(key) = agent.get("api_key").and_then(|v| v.as_str()) {
        profile.model.api_key = key.to_owned();
    }
    if let Some(token) = value.get("token").and_then(|v| v.as_str()) {
        profile.token = token.to_owned();
    }
    profile.autostart = false;
    Some(AppConfig {
        agents: vec![profile],
        ..AppConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_tolerates_missing_fields() {
        let mut config = AppConfig::default();
        config.agents.push(AgentProfile {
            token: "secret".to_owned(),
            aliases: vec!["nov".to_owned()],
            ..AgentProfile::default()
        });
        let path = std::env::temp_dir().join(format!("axis_agents_cfg_{}.json", new_id()));
        save(&path, &config).unwrap();
        assert_eq!(load(&path).unwrap(), config);

        std::fs::write(&path, r#"{ "agents": [ { "name": "Old" } ] }"#).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.agents[0].name, "Old");
        assert!(!loaded.agents[0].id.is_empty());
        assert_eq!(loaded.agents[0].behavior, BehaviorConfig::default());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn broken_config_is_backed_up_not_lost() {
        let path = std::env::temp_dir().join(format!("axis_agents_bad_{}.json", new_id()));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_none());
        let backup = path.with_extension("broken.json");
        assert!(backup.exists());
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(backup);
    }

    #[test]
    fn trigger_words_include_aliases() {
        let profile = AgentProfile {
            name: "Nova".to_owned(),
            aliases: vec!["  Nov ".to_owned(), "x".to_owned()],
            ..AgentProfile::default()
        };
        assert_eq!(profile.trigger_words(), vec!["nova", "nov"]);
    }
}
