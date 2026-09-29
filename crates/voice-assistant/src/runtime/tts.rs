//! `runtime::tts` — text-to-speech via sherpa-onnx Kokoro. The Kokoro model package bundles its
//! phonemization data (espeak-ng-data + lexicons), so no separate G2P wiring is needed; the voice is
//! chosen by `speaker_id`. Synthesis returns int16 PCM + its sample rate, which [`runtime::audio`] writes
//! to a WAV and plays through an external player (matching the Python playback path).

use sherpa_onnx::{GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig};

use crate::config::Tts;

/// A persistent Kokoro synthesizer.
pub struct Synthesizer {
    tts: OfflineTts,
    speaker_id: i32,
    speed: f32,
}

impl Synthesizer {
    /// Build the synthesizer from the [`Tts`] config. The model dir holds `model.onnx`, `voices.bin`,
    /// `tokens.txt`, `espeak-ng-data/`, and the lexicon files (sherpa's Kokoro package layout).
    pub fn new(cfg: &Tts) -> Result<Self, String> {
        let dir = cfg.model_dir.to_string_lossy();
        let config = OfflineTtsConfig {
            model: sherpa_onnx::OfflineTtsModelConfig {
                kokoro: OfflineTtsKokoroModelConfig {
                    model: Some(format!("{dir}/model.onnx")),
                    voices: Some(format!("{dir}/voices.bin")),
                    tokens: Some(format!("{dir}/tokens.txt")),
                    data_dir: Some(format!("{dir}/espeak-ng-data")),
                    ..Default::default()
                },
                num_threads: cfg.num_threads,
                debug: false,
                provider: Some(cfg.provider.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let tts = OfflineTts::create(&config)
            .ok_or_else(|| "failed to create OfflineTts (check tts.model_dir)".to_string())?;
        Ok(Self {
            tts,
            speaker_id: cfg.speaker_id,
            speed: cfg.speed,
        })
    }

    /// The synthesizer's output sample rate (Hz) — needed to write the WAV header.
    pub fn sample_rate(&self) -> u32 {
        self.tts.sample_rate() as u32
    }

    /// Synthesize `text` to int16 mono PCM. Empty/whitespace text → no samples.
    pub fn synth(&self, text: &str) -> Vec<i16> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let cfg = GenerationConfig {
            sid: self.speaker_id,
            speed: self.speed,
            ..Default::default()
        };
        // No progress callback — annotate the concrete closure type so `Option<F>` can infer `F`.
        let Some(audio) =
            self.tts
                .generate_with_config(text, &cfg, None::<fn(&[f32], f32) -> bool>)
        else {
            return Vec::new();
        };
        audio
            .samples()
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect()
    }
}
