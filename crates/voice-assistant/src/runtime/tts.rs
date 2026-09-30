//! `runtime::tts` — text-to-speech via sherpa-onnx Kokoro, streamed off a dedicated worker thread.
//!
//! The sherpa `OfflineTts` handle is non-`Send` and its `generate` call BLOCKS while it synthesizes, so it
//! lives entirely on its own thread ([`TtsWorker`]): the main audio loop submits reply text and receives
//! int16 PCM chunks over a channel AS THEY ARE SYNTHESIZED (sherpa's generate callback fires per chunk),
//! then streams them straight to the player. That drops time-to-first-audio to the first chunk instead of
//! the whole reply, overlaps later synthesis with playback, and — because synth is off the main thread —
//! leaves the main thread free to run the wake model for barge-in (#454). Set the request's `abort` flag to
//! stop synthesis mid-stream (a barge-in): the callback returns `false` and generation ends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use sherpa_onnx::{GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig};

use crate::config::Tts;

/// One synthesis request handed to the worker thread.
struct Request {
    /// The text to synthesize (already the reply body; trimmed on the worker).
    text: String,
    /// Set by the caller to abort synthesis mid-stream on a barge-in — the generate callback checks it and
    /// returns `false`, ending generation early.
    abort: Arc<AtomicBool>,
    /// Where the worker streams int16 PCM chunks as they synthesize; dropped when generation ends, so the
    /// receiver observing `Disconnected` is the end-of-stream signal.
    chunk_tx: Sender<Vec<i16>>,
}

/// A persistent Kokoro synthesizer pinned to its own thread. Submit text with [`speak`](Self::speak) and
/// consume the returned receiver of PCM chunks; the sherpa handle never leaves the worker thread (so its
/// non-`Send`-ness is a non-issue) and the main loop stays free to poll the mic for barge-in.
pub struct TtsWorker {
    req_tx: Sender<Request>,
    sample_rate: u32,
    _handle: JoinHandle<()>,
}

impl TtsWorker {
    /// Spawn the worker, create the Kokoro `OfflineTts` ON that thread, and block until it reports its
    /// output sample rate (or the init error) — so a bad `tts.model_dir` fails at startup exactly like the
    /// old batch synthesizer did. The model dir holds `model.onnx`, `voices.bin`, `tokens.txt`,
    /// `espeak-ng-data/`, and the lexicon files; a multi-lingual Kokoro model (>= v1.0) additionally needs
    /// `tts.lexicon` or `tts.lang` set.
    pub fn new(cfg: &Tts) -> Result<Self, String> {
        let cfg = cfg.clone();
        let (init_tx, init_rx) = channel::<Result<u32, String>>();
        let (req_tx, req_rx) = channel::<Request>();
        let handle = std::thread::Builder::new()
            .name("voice-tts".into())
            .spawn(move || tts_thread(cfg, init_tx, req_rx))
            .map_err(|e| format!("spawn tts thread: {e}"))?;
        // Block on the worker's init result: propagates a model-dir error, and blocks the daemon startup
        // until the model is loaded (same behavior as the old synchronous `Synthesizer::new`).
        let sample_rate = init_rx
            .recv()
            .map_err(|_| "tts worker exited during init".to_string())??;
        Ok(Self {
            req_tx,
            sample_rate,
            _handle: handle,
        })
    }

    /// The synthesizer's output sample rate (Hz) — needed to configure the raw-PCM player.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Submit `text` for synthesis. Returns a receiver of int16 PCM chunks streamed as they are generated;
    /// the channel closes (sender dropped) when synthesis finishes or is aborted. Set `abort` to stop
    /// early. If the worker is gone the channel is immediately closed (the caller then plays nothing).
    pub fn speak(&self, text: &str, abort: Arc<AtomicBool>) -> Receiver<Vec<i16>> {
        let (chunk_tx, chunk_rx) = channel::<Vec<i16>>();
        let _ = self.req_tx.send(Request {
            text: text.to_string(),
            abort,
            chunk_tx,
        });
        chunk_rx
    }
}

/// The worker thread body: build the synthesizer once, then serve requests until the request channel
/// closes (the `TtsWorker` was dropped at shutdown).
fn tts_thread(cfg: Tts, init_tx: Sender<Result<u32, String>>, req_rx: Receiver<Request>) {
    let tts = match build_tts(&cfg) {
        Ok(t) => t,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    let sample_rate = tts.sample_rate() as u32;
    if init_tx.send(Ok(sample_rate)).is_err() {
        return; // the caller gave up before we finished loading
    }
    drop(init_tx);

    for req in req_rx {
        let text = req.text.trim();
        if text.is_empty() {
            continue; // nothing to synthesize; dropping chunk_tx ends the (empty) stream
        }
        let gen_cfg = GenerationConfig {
            sid: cfg.speaker_id,
            speed: cfg.speed,
            ..Default::default()
        };
        let abort = req.abort;
        let chunk_tx = req.chunk_tx;
        // The generate callback (fired per synthesized chunk) converts f32 -> int16 and streams it out.
        // Returning false stops generation: on a barge-in (abort set) or once the consumer drops the
        // receiver (turn over). The closure owns `abort` + `chunk_tx` (both Send + 'static), satisfying
        // generate_with_config's `F: 'static` bound.
        let callback = move |samples: &[f32], _progress: f32| -> bool {
            if abort.load(Ordering::Relaxed) {
                return false;
            }
            let pcm: Vec<i16> = samples
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            chunk_tx.send(pcm).is_ok() && !abort.load(Ordering::Relaxed)
        };
        let _ = tts.generate_with_config(text, &gen_cfg, Some(callback));
        // `callback` (and the `chunk_tx` it owns) is dropped here -> the receiver sees `Disconnected`,
        // which the speak loop treats as end-of-stream.
    }
}

/// Create the Kokoro `OfflineTts` from config. Called ON the worker thread so the non-`Send` handle is
/// born and stays there.
fn build_tts(cfg: &Tts) -> Result<OfflineTts, String> {
    let dir = cfg.model_dir.to_string_lossy();
    let config = OfflineTtsConfig {
        model: sherpa_onnx::OfflineTtsModelConfig {
            kokoro: OfflineTtsKokoroModelConfig {
                model: Some(format!("{dir}/model.onnx")),
                voices: Some(format!("{dir}/voices.bin")),
                tokens: Some(format!("{dir}/tokens.txt")),
                data_dir: Some(format!("{dir}/espeak-ng-data")),
                lexicon: cfg.resolved_lexicon(),
                lang: cfg.lang_opt(),
                ..Default::default()
            },
            num_threads: cfg.num_threads,
            debug: false,
            provider: Some(cfg.provider.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    OfflineTts::create(&config)
        .ok_or_else(|| "failed to create OfflineTts (check tts.model_dir)".to_string())
}
