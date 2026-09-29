//! `config` — the voice-assistant's TOML configuration (operator mandate seq-1377: TOML file, NOT
//! environment variables). This replaces the entire `SA_*` env surface of the Python original; every
//! former env knob is a TOML key here with the SAME built-in default, so a host with no config behaves
//! exactly as the defaults describe. The file is located like fleet's own config: a `--config <path>`
//! override, else `$XDG_CONFIG_HOME/voice-assistant/config.toml`, else `$HOME/.config/voice-assistant/
//! config.toml`. `HOME`/`XDG_CONFIG_HOME` are OS-standard *locators*, not assistant knobs — they only
//! find the file. See `config.example.toml` for the documented surface.
//!
//! Every table and key is optional: an absent file, table, or key falls back to the default at its use
//! site (via `#[serde(default)]` + per-field default fns), so partial configs are fine.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

/// The whole config: one table per subsystem. Each is `#[serde(default)]` so an omitted table is the
/// all-defaults table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub assistant: Assistant,
    pub wake: Wake,
    pub stt: Stt,
    pub tts: Tts,
    pub brain: Brain,
    pub board: Board,
    pub audio: Audio,
}

// ─────────────────────────────── [assistant] ───────────────────────────────

/// Identity + the spoken persona's system prompt. The name is a plain knob (the Python original
/// hard-coded a single assistant name throughout — here it is data, not code).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Assistant {
    /// The assistant's spoken name — used in the board identity default and available to the prompt.
    pub name: String,
    /// The system prompt that shapes every reply. Replies are spoken, so the default asks for brevity.
    /// De-personalized: no owner/shop specifics baked in — edit this key to tailor the persona.
    pub system_prompt: String,
}

impl Default for Assistant {
    fn default() -> Self {
        Self {
            name: "assistant".to_string(),
            system_prompt: default_system_prompt(),
        }
    }
}

/// The default spoken-assistant prompt. Kept generic (the Python original's owner/shop framing is gone);
/// tailor it in `[assistant].system_prompt`. It still encodes the *behaviors* the loop depends on: a
/// spoken reply is short, ending on a question reopens the mic, and the knowledge base / task board /
/// surfaces are reachable via their MCP tools.
fn default_system_prompt() -> String {
    "You are a personal voice assistant. Your replies are spoken aloud, so be extremely brief: at most \
     two short sentences. Give the direct answer, then offer to go deeper (like 'Want the specifics?') \
     rather than dumping details that weren't asked for. No markdown, no lists, no preamble, and don't \
     repeat the question back. For anything the user asks that your knowledge base might cover, search \
     it (a single kb_search with no collection spans every collection) and answer from what you find; \
     don't ask to clarify a term, just look it up. To walk through a procedure or answer 'what's next', \
     find the starting page, then use kb_read_pages (with that result's col and path) to read the \
     following pages in order. You can also search the web for current events or general knowledge that \
     isn't the user's own work — prefer the knowledge base for their stuff, the web for the wider \
     world. Never invent or assume; if you're unsure or find nothing, say so. Only use the remember \
     tool for a fact the user has clearly and directly stated. You also manage a task board (projects \
     and tasks with status, comments, and assignments); when asked to capture, track, update, or \
     review tasks, use the task-board tools. When something is better seen than spoken — a link, a PDF, \
     an image, a snippet, or a longer answer — push it to a surface with send_item (list_surfaces to \
     see what's registered) and say briefly you've put it up. When you end with a question, the mic \
     reopens automatically so the user can reply without the wake word — natural for a quick \
     back-and-forth. Don't be chatty and don't over-confirm; just answer."
        .to_string()
}

// ─────────────────────────────── [wake] ───────────────────────────────

/// Wake-word detection via sherpa-onnx keyword spotting (KWS). Unlike the Python original's fixed
/// openWakeWord model, KWS matches an arbitrary phrase declared in `keywords_file`, so the wake phrase
/// is fully user-defined.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Wake {
    /// Directory holding the KWS transducer model files (encoder/decoder/joiner + tokens).
    pub model_dir: PathBuf,
    /// The keywords file (sherpa KWS format) declaring the wake phrase(s) to spot.
    pub keywords_file: PathBuf,
    /// Detection score threshold; higher = fewer false wakes. sherpa KWS default is ~0.25.
    pub threshold: f32,
    /// Inference provider: `cpu` (default — wake is tiny) or `cuda`.
    pub provider: String,
    /// ONNX intra-op threads.
    pub num_threads: i32,
}

impl Default for Wake {
    fn default() -> Self {
        Self {
            model_dir: home_share("voice-assistant/kws"),
            keywords_file: home_share("voice-assistant/kws/keywords.txt"),
            threshold: 0.25,
            provider: "cpu".to_string(),
            num_threads: 1,
        }
    }
}

// ─────────────────────────────── [stt] ───────────────────────────────

/// Speech-to-text via sherpa-onnx Whisper. GPU by default (`provider = "cuda"`) — the original ran
/// faster-whisper on the box's GPU; sherpa's CUDA build runs on the same card.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stt {
    /// Directory holding the Whisper ONNX model files (`<name>-encoder.onnx`, `<name>-decoder.onnx`,
    /// `<name>-tokens.txt`).
    pub model_dir: PathBuf,
    /// Whisper model basename inside `model_dir` (e.g. `small.en` → `small.en-encoder.onnx`, …).
    pub model: String,
    /// Inference provider: `cuda` (default, GPU) or `cpu`.
    pub provider: String,
    /// ONNX intra-op threads (CPU fallback / non-GPU ops).
    pub num_threads: i32,
    /// Decode language (`en`). Empty → Whisper auto-detects.
    pub language: String,
    /// Decoder priming text to bias jargon (so domain terms aren't mis-heard). This is the one place
    /// domain vocabulary lives; edit it for your world. Empty → no prompt.
    pub initial_prompt: String,
}

impl Default for Stt {
    fn default() -> Self {
        Self {
            model_dir: home_share("voice-assistant/whisper"),
            model: "small.en".to_string(),
            provider: "cuda".to_string(),
            num_threads: 2,
            language: "en".to_string(),
            initial_prompt: String::new(),
        }
    }
}

// ─────────────────────────────── [tts] ───────────────────────────────

/// Text-to-speech via sherpa-onnx Kokoro. The Kokoro model package bundles the phonemization data
/// (espeak-ng-data + lexicons), so no separate G2P wiring is needed; British and American voices ship
/// in the same package selected by `speaker_id`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tts {
    /// Directory holding the Kokoro model package: `model.onnx`, `voices.bin`, `tokens.txt`, the
    /// `espeak-ng-data/` dir, and the `lexicon*.txt` files.
    pub model_dir: PathBuf,
    /// Kokoro speaker id (voice). Kokoro ships many; pick a British-male id for the original's voice.
    pub speaker_id: i32,
    /// Speaking rate multiplier (1.0 = natural).
    pub speed: f32,
    /// Inference provider: `cpu` (default — Kokoro is fast enough on CPU for short replies) or `cuda`.
    pub provider: String,
    /// ONNX intra-op threads.
    pub num_threads: i32,
}

impl Default for Tts {
    fn default() -> Self {
        Self {
            model_dir: home_share("voice-assistant/kokoro"),
            speaker_id: 0,
            speed: 1.0,
            provider: "cpu".to_string(),
            num_threads: 2,
        }
    }
}

// ─────────────────────────────── [brain] ───────────────────────────────

/// The Claude brain: the `claude` CLI driven with the three MCP servers, an allowed-tool set, and a
/// filesystem-scope guard. Mirrors the Python Agent-SDK wiring (same MCP URLs, same tool policy).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Brain {
    /// The `claude` CLI binary (on `PATH` by name, or an absolute path).
    pub claude_bin: String,
    /// Model override; empty → the CLI/account default.
    pub model: String,
    /// Knowledge-base MCP server URL (HTTP).
    pub kb_mcp_url: String,
    /// Task-board MCP server URL (HTTP).
    pub task_board_mcp_url: String,
    /// Surfaced (browser-surface) MCP server URL (HTTP).
    pub surfaced_mcp_url: String,
    /// Allowed tool patterns (MCP server wildcards + built-in tools). Server wildcards auto-enable new
    /// tools without editing this list — matches the Python `ALLOWED_TOOLS`.
    pub allowed_tools: Vec<String>,
    /// Filesystem reads are confined to this tree by the PreToolUse scope guard (Read/Glob/Grep).
    pub projects_dir: PathBuf,
    /// Max agent turns per query (bounds a runaway tool loop).
    pub max_turns: u32,
    /// Hard timeout for one brain turn, in seconds — a hung tool/API call must not wedge the loop.
    pub ask_timeout_secs: f64,
}

impl Default for Brain {
    fn default() -> Self {
        Self {
            claude_bin: "claude".to_string(),
            model: String::new(),
            kb_mcp_url: "http://localhost:8077/mcp".to_string(),
            task_board_mcp_url: "http://localhost:8079/mcp".to_string(),
            surfaced_mcp_url: "http://localhost:8787/surfaced/mcp".to_string(),
            allowed_tools: vec![
                "mcp__knowledge-base".to_string(),
                "mcp__task-board".to_string(),
                "mcp__surfaced".to_string(),
                "WebSearch".to_string(),
                "WebFetch".to_string(),
                "Read".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
            ],
            projects_dir: home_dir().join("Projects"),
            max_turns: 8,
            ask_timeout_secs: 90.0,
        }
    }
}

// ─────────────────────────────── [board] ───────────────────────────────

/// The task board identity + the webhook the board pushes events to (so a queued task finishing can
/// trigger a proactive spoken answer). Mirrors the Python `BOARD_*` knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Board {
    /// This assistant's agent id on the board.
    pub agent: String,
    /// Host the local webhook receiver binds.
    pub webhook_host: String,
    /// Port the local webhook receiver binds.
    pub webhook_port: u16,
    /// The webhook URL registered with the board (defaults to `http://<host>:<port>/hook`).
    pub webhook_url: Option<String>,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            agent: "assistant".to_string(),
            webhook_host: "127.0.0.1".to_string(),
            webhook_port: 8076,
            webhook_url: None,
        }
    }
}

impl Board {
    /// The effective webhook URL: the explicit `webhook_url`, else `http://<host>:<port>/hook`.
    pub fn effective_webhook_url(&self) -> String {
        self.webhook_url
            .clone()
            .unwrap_or_else(|| format!("http://{}:{}/hook", self.webhook_host, self.webhook_port))
    }
}

// ─────────────────────────────── [audio] ───────────────────────────────

/// Capture geometry + energy-VAD tuning. Values match the Python original (validated on the box).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// Capture sample rate (Hz). 16 kHz is what the wake/STT models expect.
    pub sample_rate: u32,
    /// Frame size in samples (80 ms @ 16 kHz = 1280 — the wake model's frame).
    pub frame: usize,
    /// Input device name substring to pin capture to (empty → the system default input).
    pub input_device: String,
    /// Quiet needed to END an utterance (seconds) — long enough to survive a thinking pause.
    pub silence_secs: f64,
    /// Minimum real speech before an utterance can end (seconds).
    pub min_speech_secs: f64,
    /// Give up if no speech begins within this window after the wake (seconds).
    pub start_timeout_secs: f64,
    /// More patient window for a reopened follow-up mic (seconds).
    pub followup_timeout_secs: f64,
    /// Hard cap on one utterance (seconds).
    pub max_utterance_secs: f64,
    /// int16 RMS speech/silence floor for the energy VAD.
    pub vad_rms: f64,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            frame: 1280,
            input_device: String::new(),
            silence_secs: 2.5,
            min_speech_secs: 0.3,
            start_timeout_secs: 5.0,
            followup_timeout_secs: 10.0,
            max_utterance_secs: 25.0,
            vad_rms: 500.0,
        }
    }
}

// ─────────────────────────────── loading ───────────────────────────────

static CONFIG: OnceLock<Config> = OnceLock::new();
static PATH_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// `$HOME`, else `/` if unset (only used to build defaults; a real host always has `HOME`).
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `$HOME/.local/share/<rel>` — the default asset location (parallels the Python `~/.local/share`).
fn home_share(rel: &str) -> PathBuf {
    home_dir().join(".local/share").join(rel)
}

/// The default config path: `$XDG_CONFIG_HOME/voice-assistant/config.toml`, else
/// `$HOME/.config/voice-assistant/config.toml`, else `None`. These env vars are OS-standard *locators*,
/// not assistant knobs.
fn default_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("voice-assistant/config.toml"));
    }
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|h| PathBuf::from(h).join(".config/voice-assistant/config.toml"))
}

/// Record the `--config <path>` override before the first [`get`]. A no-op once the config is loaded.
pub fn set_path(path: Option<PathBuf>) {
    let _ = PATH_OVERRIDE.set(path);
}

/// Parse a config from TOML text. Unlike fleet's silent fallback, an unparseable config is an ERROR the
/// caller must surface — a misconfigured voice loop should refuse to start, not silently ignore knobs.
pub fn parse(toml_text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(toml_text)
}

/// The loaded config (parsed once). Reads the `--config` override else the default path; an absent file
/// yields the all-defaults config. An unparseable file is a hard error (returned to the caller).
pub fn load() -> Result<&'static Config, String> {
    // OnceLock has no fallible get_or_init on stable; parse eagerly then store.
    if let Some(cfg) = CONFIG.get() {
        return Ok(cfg);
    }
    let path = PATH_OVERRIDE.get().cloned().flatten().or_else(default_path);
    let cfg = match path {
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(text) => parse(&text)
                .map_err(|e| format!("config {} is not valid TOML: {e}", p.display()))?,
            Err(_) => Config::default(), // absent file → defaults (the common case)
        },
        None => Config::default(),
    };
    Ok(CONFIG.get_or_init(|| cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        let cfg = parse("").unwrap();
        assert_eq!(cfg.assistant.name, "assistant");
        assert_eq!(cfg.audio.sample_rate, 16000);
        assert_eq!(cfg.audio.frame, 1280);
        assert_eq!(cfg.stt.provider, "cuda");
        assert_eq!(cfg.wake.threshold, 0.25);
        assert_eq!(cfg.brain.max_turns, 8);
        assert!(
            cfg.brain
                .allowed_tools
                .contains(&"mcp__knowledge-base".to_string())
        );
    }

    #[test]
    fn partial_tables_default_the_rest() {
        let cfg = parse(
            r#"
            [assistant]
            name = "Jarvis"

            [audio]
            vad_rms = 700.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.assistant.name, "Jarvis");
        // untouched keys in a present table keep their defaults
        assert_eq!(cfg.audio.vad_rms, 700.0);
        assert_eq!(cfg.audio.sample_rate, 16000);
        // an absent table is all-defaults
        assert_eq!(cfg.stt.model, "small.en");
    }

    #[test]
    fn every_table_round_trips() {
        let cfg = parse(
            r#"
            [assistant]
            name = "V"
            system_prompt = "Be brief."
            [wake]
            threshold = 0.4
            provider = "cuda"
            [stt]
            model = "medium.en"
            provider = "cpu"
            language = "en"
            initial_prompt = "Voron, Klipper."
            [tts]
            speaker_id = 24
            speed = 1.1
            [brain]
            model = "opus"
            max_turns = 12
            ask_timeout_secs = 120.0
            allowed_tools = ["mcp__knowledge-base", "WebSearch"]
            [board]
            agent = "v"
            webhook_port = 9000
            [audio]
            input_device = "Jabra"
            silence_secs = 3.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.assistant.system_prompt, "Be brief.");
        assert_eq!(cfg.wake.threshold, 0.4);
        assert_eq!(cfg.stt.model, "medium.en");
        assert_eq!(cfg.stt.provider, "cpu");
        assert_eq!(cfg.tts.speaker_id, 24);
        assert_eq!(cfg.brain.model, "opus");
        assert_eq!(cfg.brain.max_turns, 12);
        assert_eq!(cfg.brain.allowed_tools.len(), 2);
        assert_eq!(cfg.board.agent, "v");
        assert_eq!(cfg.board.webhook_port, 9000);
        assert_eq!(cfg.audio.input_device, "Jabra");
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields: a typo'd knob is an error, not a silently-ignored setting.
        assert!(parse("[audio]\nsampel_rate = 8000\n").is_err());
    }

    #[test]
    fn webhook_url_defaults_from_host_and_port() {
        let cfg = parse("[board]\nwebhook_host = \"127.0.0.1\"\nwebhook_port = 8076\n").unwrap();
        assert_eq!(
            cfg.board.effective_webhook_url(),
            "http://127.0.0.1:8076/hook"
        );
        let explicit = parse("[board]\nwebhook_url = \"http://x/y\"\n").unwrap();
        assert_eq!(explicit.board.effective_webhook_url(), "http://x/y");
    }
}
