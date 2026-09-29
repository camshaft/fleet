//! `runtime::stt` — speech-to-text via sherpa-onnx offline Whisper, on the GPU (`provider = "cuda"`).
//!
//! Direct analog of the Python faster-whisper path: a persistent recognizer, fed a full utterance of f32
//! mono samples, decoded to text. sherpa's offline recognizer takes a fresh stream per utterance
//! (`create_stream` → `accept_waveform` → `decode` → `get_result`).

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineWhisperModelConfig};

use crate::config::Stt;

/// A persistent Whisper recognizer.
pub struct Transcriber {
    recognizer: OfflineRecognizer,
    sample_rate: i32,
}

impl Transcriber {
    /// Build the recognizer from the [`Stt`] config. Model files are `<model_dir>/<model>-encoder.onnx`,
    /// `-decoder.onnx`, `-tokens.txt` (sherpa's Whisper naming).
    pub fn new(cfg: &Stt, sample_rate: u32) -> Result<Self, String> {
        let dir = cfg.model_dir.to_string_lossy();
        let m = &cfg.model;
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.whisper = OfflineWhisperModelConfig {
            encoder: Some(format!("{dir}/{m}-encoder.onnx")),
            decoder: Some(format!("{dir}/{m}-decoder.onnx")),
            language: Some(cfg.language.clone()),
            task: Some("transcribe".to_string()),
            tail_paddings: 0,
            enable_token_timestamps: false,
            enable_segment_timestamps: false,
        };
        config.model_config.tokens = Some(format!("{dir}/{m}-tokens.txt"));
        config.model_config.provider = Some(cfg.provider.clone());
        config.model_config.num_threads = cfg.num_threads;

        let recognizer = OfflineRecognizer::create(&config).ok_or_else(|| {
            "failed to create OfflineRecognizer (check stt.model_dir / model)".to_string()
        })?;
        Ok(Self {
            recognizer,
            sample_rate: sample_rate as i32,
        })
    }

    /// Transcribe one utterance of int16 mono samples. Empty input → empty string (mirrors the Python
    /// guard).
    pub fn transcribe(&self, samples: &[i16]) -> String {
        if samples.is_empty() {
            return String::new();
        }
        let f32s: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(self.sample_rate, &f32s);
        self.recognizer.decode(&stream);
        stream
            .get_result()
            .map(|r| r.text.trim().to_string())
            .unwrap_or_default()
    }
}
