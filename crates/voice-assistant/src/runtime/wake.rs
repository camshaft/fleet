//! `runtime::wake` — custom wake-phrase detection via sherpa-onnx keyword spotting (KWS).
//!
//! The Python original used openWakeWord locked to the prebuilt `hey_jarvis` model. sherpa KWS spots an
//! arbitrary phrase declared in a keywords file, so the wake phrase is fully user-defined (config
//! `[wake].keywords_file`). KWS is a streaming transducer: feed 16 kHz int16 frames as f32, run the
//! `is_ready`/`decode` loop, and a non-empty `keyword` on the result is a detection. [`reset`] clears the
//! stream so one activation can't linger into the next listen (or into barge-in).

use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig, OnlineStream};

use crate::config::Wake;

/// The wake spotter plus its single streaming decode context.
pub struct WakeSpotter {
    spotter: KeywordSpotter,
    stream: OnlineStream,
    sample_rate: i32,
}

impl WakeSpotter {
    /// Build the KWS spotter from the [`Wake`] config (transducer model dir + keywords file).
    pub fn new(cfg: &Wake, sample_rate: u32) -> Result<Self, String> {
        let dir = cfg.model_dir.to_string_lossy();
        let mut config = KeywordSpotterConfig::default();
        config.model_config.transducer.encoder = Some(format!("{dir}/encoder.onnx"));
        config.model_config.transducer.decoder = Some(format!("{dir}/decoder.onnx"));
        config.model_config.transducer.joiner = Some(format!("{dir}/joiner.onnx"));
        config.model_config.tokens = Some(format!("{dir}/tokens.txt"));
        config.model_config.provider = Some(cfg.provider.clone());
        config.model_config.num_threads = cfg.num_threads;
        config.keywords_file = Some(cfg.keywords_file.to_string_lossy().into_owned());
        config.keywords_threshold = cfg.threshold;

        let spotter = KeywordSpotter::create(&config).ok_or_else(|| {
            "failed to create KeywordSpotter (check model_dir / keywords_file)".to_string()
        })?;
        let stream = spotter.create_stream();
        Ok(Self {
            spotter,
            stream,
            sample_rate: sample_rate as i32,
        })
    }

    /// Feed one frame of int16 samples and return `true` if the wake phrase fired. On a hit the stream is
    /// reset so the activation can't immediately re-trigger.
    pub fn accept(&mut self, frame: &[i16]) -> bool {
        let f32s: Vec<f32> = frame.iter().map(|&s| s as f32 / 32768.0).collect();
        self.stream.accept_waveform(self.sample_rate, &f32s);
        while self.spotter.is_ready(&self.stream) {
            self.spotter.decode(&self.stream);
        }
        if let Some(result) = self.spotter.get_result(&self.stream)
            && !result.keyword.is_empty()
        {
            self.reset();
            return true;
        }
        false
    }

    /// Clear the stream's rolling state (after a detection, or before arming barge-in) so a stale
    /// activation can't linger. Mirrors the Python `wake.reset()`.
    pub fn reset(&mut self) {
        self.spotter.reset(&self.stream);
    }
}
