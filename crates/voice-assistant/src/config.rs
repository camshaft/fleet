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

use bridge_core::ChannelLink;
use serde::Deserialize;

/// The whole config: one table per subsystem. Each is `#[serde(default)]` so an omitted table is the
/// all-defaults table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub wake: Wake,
    pub stt: Stt,
    pub tts: Tts,
    pub board: Board,
    pub audio: Audio,
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

/// Speech-to-text via sherpa-onnx Whisper. CPU by default (`provider = "cpu"`) — stable everywhere and
/// fast enough for short utterances (small.en). Set `provider = "cuda"` for GPU, but only when the box's
/// onnxruntime/cuDNN CUDA provider actually has kernels for the card's arch (e.g. a Pascal 1080 Ti /
/// sm_61 has none in the bundled build → GPU Whisper aborts, so CPU is the right default).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stt {
    /// Directory holding the Whisper ONNX model files (`<name>-encoder.onnx`, `<name>-decoder.onnx`,
    /// `<name>-tokens.txt`).
    pub model_dir: PathBuf,
    /// Whisper model basename inside `model_dir` (e.g. `small.en` → `small.en-encoder.onnx`, …).
    pub model: String,
    /// Inference provider: `cpu` (default — stable everywhere) or `cuda` (GPU, only if the box's provider
    /// has kernels for the card's arch).
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
            provider: "cpu".to_string(),
            num_threads: 2,
            language: "en".to_string(),
            initial_prompt: String::new(),
        }
    }
}

// ─────────────────────────────── [tts] ───────────────────────────────

/// Text-to-speech via sherpa-onnx Kokoro. The Kokoro model package bundles the phonemization data
/// (espeak-ng-data + lexicons); British and American voices ship in the same package selected by
/// `speaker_id`. A multi-lingual Kokoro model (>= v1.0) additionally REQUIRES a `lexicon` or `lang` (it
/// aborts init otherwise) — see those fields.
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
    /// Kokoro lexicon file(s) — comma-separated, resolved relative to `model_dir` (a component with a
    /// leading `/` is taken as an absolute path). REQUIRED for a multi-lingual Kokoro model (>= v1.0),
    /// which aborts init without a `lexicon` or `lang`; empty is fine for the old single-lang Kokoro.
    /// English example: `"lexicon-gb-en.txt,lexicon-us-en.txt"`. Don't add the `zh` lexicon unless you
    /// also stage its jieba dict (`dict/`).
    pub lexicon: String,
    /// espeak-ng language for Kokoro G2P (e.g. `"en-us"`) — an alternative to `lexicon` for satisfying a
    /// multi-lingual Kokoro model. Empty → not set.
    pub lang: String,
}

impl Default for Tts {
    fn default() -> Self {
        Self {
            model_dir: home_share("voice-assistant/kokoro"),
            speaker_id: 0,
            speed: 1.0,
            provider: "cpu".to_string(),
            num_threads: 2,
            lexicon: String::new(),
            lang: String::new(),
        }
    }
}

impl Tts {
    /// The Kokoro `lexicon` argument: the comma-separated `lexicon` entries resolved against `model_dir`
    /// (an entry with a leading `/` is taken as absolute), or `None` when unset. sherpa accepts a
    /// comma-separated lexicon path list.
    pub fn resolved_lexicon(&self) -> Option<String> {
        let dir = self.model_dir.to_string_lossy();
        let joined = self
            .lexicon
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| {
                if p.starts_with('/') {
                    p.to_string()
                } else {
                    format!("{dir}/{p}")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        (!joined.is_empty()).then_some(joined)
    }

    /// The Kokoro `lang` argument, or `None` when unset.
    pub fn lang_opt(&self) -> Option<String> {
        let t = self.lang.trim();
        (!t.is_empty()).then(|| t.to_string())
    }
}

// ─────────────────────────────── [board] ───────────────────────────────

/// The voice bridge's board wiring (#316 / Doc #18). The daemon is a transport bridge: a finalized
/// transcript is posted to a board voice channel, and George (a board-native voice agent) replies there;
/// the daemon polls the firehose and speaks those replies. All of this is the shared `bridge-core`
/// contract — this table is just the voice-side config for it. No in-process brain, no MCP, no webhook.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Board {
    /// The board's token-less localhost REST base (the firehose poll + attributed post live here).
    pub board_api: String,
    /// This bridge's own board agent id — the INBOUND `sender`. Deliberately NOT an outbound author on the
    /// voice channel, so its own posted transcripts don't reflect back out as speech.
    pub bridge_agent: String,
    /// The speaker's external id: transcripts are attributed `external_author = "voice:<speaker>"`.
    pub speaker: String,
    /// The external voice channel id (e.g. `voice:green`) the speaker's transcripts post into; mapped to a
    /// board channel via `channel_map` (and any board-registered `voice` links).
    pub voice_channel: String,
    /// The bridge's local state dir — the firehose cursor is persisted here so a restart resumes without
    /// re-speaking or gapping.
    pub state_dir: PathBuf,
    /// Static board↔voice channel links (`[[board.channel_map]]`). Merged with board-registered `voice`
    /// links at runtime; empty is valid (dormant until a link is registered).
    #[serde(default)]
    pub channel_map: Vec<ChannelLink>,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            board_api: "http://127.0.0.1:8079/api".to_string(),
            bridge_agent: "voice-bridge".to_string(),
            speaker: "operator".to_string(),
            voice_channel: "voice:local".to_string(),
            state_dir: home_share("voice-assistant"),
            channel_map: Vec::new(),
        }
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
    /// PLAYBACK output device (an ALSA device string, e.g. `plughw:CARD=USB`). When set, replies + cues
    /// play via a single deterministic `aplay -D <output_device>`, bypassing player auto-selection and the
    /// ALSA `default` PCM — which on a session-less system service is often a dead PipeWire sink (silent).
    /// Empty → the best-effort player list (paplay/pw-play/aplay), unchanged.
    pub output_device: String,
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
            output_device: String::new(),
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
        assert_eq!(cfg.audio.sample_rate, 16000);
        assert_eq!(cfg.audio.frame, 1280);
        assert_eq!(cfg.audio.output_device, "");
        assert_eq!(cfg.stt.provider, "cpu");
        assert_eq!(cfg.wake.threshold, 0.25);
        // The board bridge defaults: local REST, the voice-bridge agent, an empty (dormant) channel map.
        assert_eq!(cfg.board.board_api, "http://127.0.0.1:8079/api");
        assert_eq!(cfg.board.bridge_agent, "voice-bridge");
        assert_eq!(cfg.board.speaker, "operator");
        assert_eq!(cfg.board.voice_channel, "voice:local");
        assert!(cfg.board.channel_map.is_empty());
    }

    #[test]
    fn partial_tables_default_the_rest() {
        let cfg = parse(
            r#"
            [board]
            speaker = "operator"

            [audio]
            vad_rms = 700.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.board.speaker, "operator");
        // untouched keys in a present table keep their defaults
        assert_eq!(cfg.board.bridge_agent, "voice-bridge");
        assert_eq!(cfg.audio.vad_rms, 700.0);
        assert_eq!(cfg.audio.sample_rate, 16000);
        // an absent table is all-defaults
        assert_eq!(cfg.stt.model, "small.en");
    }

    #[test]
    fn every_table_round_trips() {
        let cfg = parse(
            r#"
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
            [board]
            board_api = "http://board.local/api"
            bridge_agent = "vb"
            speaker = "operator"
            voice_channel = "voice:green"
            state_dir = "/var/lib/voice-assistant"
            [[board.channel_map]]
            board_channel_id = 50
            external_channel = "voice:green"
            [audio]
            input_device = "Jabra"
            silence_secs = 3.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.wake.threshold, 0.4);
        assert_eq!(cfg.stt.model, "medium.en");
        assert_eq!(cfg.stt.provider, "cpu");
        assert_eq!(cfg.tts.speaker_id, 24);
        assert_eq!(cfg.board.board_api, "http://board.local/api");
        assert_eq!(cfg.board.bridge_agent, "vb");
        assert_eq!(cfg.board.speaker, "operator");
        assert_eq!(cfg.board.voice_channel, "voice:green");
        assert_eq!(cfg.board.state_dir, PathBuf::from("/var/lib/voice-assistant"));
        assert_eq!(cfg.board.channel_map.len(), 1);
        assert_eq!(cfg.board.channel_map[0].board_channel_id, 50);
        assert_eq!(cfg.board.channel_map[0].external_channel, "voice:green");
        assert_eq!(cfg.audio.input_device, "Jabra");
    }

    #[test]
    fn board_channel_map_accepts_the_bridge_core_link_shape() {
        // The link rows parse via bridge_core::ChannelLink (external_channel, or the slack_channel alias).
        let cfg = parse(
            r#"
            [[board.channel_map]]
            board_channel_id = 7
            external_channel = "voice:green"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.board.channel_map.len(), 1);
        assert_eq!(cfg.board.channel_map[0].board_channel_id, 7);
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields: a typo'd knob is an error, not a silently-ignored setting.
        assert!(parse("[audio]\nsampel_rate = 8000\n").is_err());
    }

    #[test]
    fn tts_lexicon_resolves_against_model_dir_and_lang_is_optional() {
        let cfg = parse(
            r#"
            [tts]
            model_dir = "/models/kokoro"
            lexicon = "lexicon-gb-en.txt, lexicon-us-en.txt, /abs/extra.txt"
            lang = "en-us"
            "#,
        )
        .unwrap();
        // Relative entries join to model_dir; an absolute entry (leading /) passes through; ws trimmed.
        assert_eq!(
            cfg.tts.resolved_lexicon().as_deref(),
            Some("/models/kokoro/lexicon-gb-en.txt,/models/kokoro/lexicon-us-en.txt,/abs/extra.txt")
        );
        assert_eq!(cfg.tts.lang_opt().as_deref(), Some("en-us"));
    }

    #[test]
    fn tts_lexicon_and_lang_unset_by_default() {
        // The old single-lang Kokoro needs neither — an empty config leaves both unset (no crash arg).
        let cfg = parse("").unwrap();
        assert_eq!(cfg.tts.resolved_lexicon(), None);
        assert_eq!(cfg.tts.lang_opt(), None);
    }

    #[test]
    fn audio_output_device_defaults_empty_and_round_trips() {
        assert_eq!(parse("").unwrap().audio.output_device, "");
        let cfg = parse("[audio]\noutput_device = \"plughw:CARD=USB\"\n").unwrap();
        assert_eq!(cfg.audio.output_device, "plughw:CARD=USB");
    }
}
