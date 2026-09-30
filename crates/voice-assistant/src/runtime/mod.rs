//! `runtime` — the live voice loop, behind the `runtime` feature so the pure core builds/tests without a
//! native ONNX backend or audio device. It wires the sherpa-onnx wake/STT/TTS engines and cpal capture to
//! the main loop, and drives the `claude` CLI for the brain.
//!
//! The loop is deliberately blocking and single-threaded (audio and brain never overlap), a faithful port
//! of the Python `loop.run`: the only concurrency is the brain subprocess (its own process) and the board
//! webhook receiver (its own thread), which hand results back over channels. This avoids pulling in an
//! async runtime.

mod audio;
mod brain;
mod stt;
mod tts;
mod wake;

use std::time::Duration;

use crate::config::Config;
use crate::{chime, events};

use audio::{Capture, Playback};
use brain::BrainRunner;
use stt::Transcriber;
use tts::Synthesizer;
use wake::WakeSpotter;

/// The assembled engines + config for one running assistant.
struct Assistant {
    cfg: Config,
    cap: Capture,
    wake: WakeSpotter,
    stt: Transcriber,
    tts: Synthesizer,
    brain: BrainRunner,
    webhook: Option<events::WebhookReceiver>,
    /// Events drained while waiting for wake, held until [`Assistant::drain_proactive`] handles them (the
    /// channel is consume-on-read, so we can't peek — we buffer instead).
    pending_events: Vec<events::ProactiveEvent>,
}

/// Build every engine from config. `scope_guard_command` is how to re-invoke this binary as the FS-scope
/// hook (passed to the brain). Returns an error if any engine fails to initialize.
pub fn run(cfg: Config, scope_guard_command: String) -> Result<(), String> {
    // Register on the board + start the webhook receiver BEFORE the loop (best-effort — a down board
    // just means no proactive events; the assistant still works as a plain voice loop).
    let webhook = events::WebhookReceiver::start(&cfg.board.webhook_host, cfg.board.webhook_port);
    events::register(
        &cfg.brain.task_board_mcp_url,
        &cfg.board.agent,
        &cfg.assistant.name,
        &cfg.board.effective_webhook_url(),
    );

    // Open capture with retry-until-present rather than a fatal `?`: the daemon must stay up and wait for
    // the mic (operator requirement #239), never crash-loop when it's absent at startup.
    let cap = Capture::open_with_retry(&cfg.audio);
    let wake = WakeSpotter::new(&cfg.wake, cfg.audio.sample_rate)?;
    let stt = Transcriber::new(&cfg.stt, cfg.audio.sample_rate)?;
    let tts = Synthesizer::new(&cfg.tts)?;
    let brain = BrainRunner::new(
        cfg.brain.clone(),
        cfg.assistant.system_prompt.clone(),
        scope_guard_command,
    );

    let mut a = Assistant {
        cfg,
        cap,
        wake,
        stt,
        tts,
        brain,
        webhook,
        pending_events: Vec::new(),
    };
    a.main_loop();
    Ok(())
}

impl Assistant {
    /// Play int16 PCM at the chime sample rate (cue tones) — write a temp WAV, play it, clean up.
    fn play_cue(&self, samples: &[i16]) {
        if let Ok(path) = audio::write_wav(samples, chime::CHIME_SR) {
            audio::play_wav(&path, &self.cfg.audio.output_device);
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Speak `text`; if the wake phrase is heard during playback, kill it and return `true` (a barge-in).
    /// Port of the Python `speak_interruptible`, including the "arm only after a low streak" debounce so
    /// the wake that opened this turn (or stale activation) can't count as a barge-in.
    fn speak_interruptible(&mut self, text: &str) -> bool {
        let samples = self.tts.synth(text);
        if samples.is_empty() {
            return false;
        }
        let path = match audio::write_wav(&samples, self.tts.sample_rate()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[tts] wav write failed: {e}");
                return false;
            }
        };
        let mut playback = Playback::start(&path, &self.cfg.audio.output_device);
        self.wake.reset(); // the wake that opened this turn must not count as a barge-in

        const ARM_FRAMES: u32 = 3;
        let (mut armed, mut low_streak) = (false, 0u32);
        let mut interrupted = false;
        let per_frame = Duration::from_millis(200);
        while !playback.finished() {
            let Some(frame) = self.cap.next_frame(per_frame) else {
                continue;
            };
            let hit = self.wake.accept(&frame);
            if !armed {
                // Arm only once the score has been quiet for a few frames (clears stale activation; a
                // muted mic reads as silence, so it arms but nothing fires).
                low_streak = if hit { 0 } else { low_streak + 1 };
                if low_streak >= ARM_FRAMES {
                    armed = true;
                }
                continue;
            }
            if hit {
                interrupted = true;
                break;
            }
        }
        playback.stop();
        let _ = std::fs::remove_file(&path);
        interrupted
    }

    /// Handle a queued board event: a task the user waited on just finished — chime and proactively answer
    /// from the (resumed) brain session. Port of the Python `_handle_proactive`.
    fn handle_proactive(&mut self, ev: &events::ProactiveEvent) {
        eprintln!("[proactive] task {:?} done: {}", ev.task_id, ev.title);
        self.play_cue(&chime::ready()); // "I have something for you" cue
        let prompt = format!(
            "The '{}' docs you queued on the board just finished ingesting and are now searchable in \
             the knowledge base. Search them and briefly answer the question the user was waiting on. \
             If nothing relevant is found, say the ingest may not have worked.",
            ev.title
        );
        let reply = self.brain.ask(&prompt);
        eprintln!("[assistant/proactive] {reply}");
        self.speak_interruptible(&reply);
    }

    /// Handle every queued proactive event: first those buffered while waiting for wake, then any that
    /// arrived since. Called at idle moments.
    fn drain_proactive(&mut self) {
        let mut events = std::mem::take(&mut self.pending_events);
        if let Some(w) = self.webhook.as_ref() {
            events.extend(w.drain());
        }
        for ev in events {
            self.handle_proactive(&ev);
        }
    }

    /// The main loop: wake → chime → record → STT → brain → speak, with barge-in and follow-up. A direct
    /// port of the Python `loop.run`, minus its per-iteration try/except (each stage here already returns
    /// gracefully; a panic would be caught by the process supervisor).
    fn main_loop(&mut self) {
        self.play_cue(&chime::ready());
        eprintln!("[voice-assistant] ready — waiting for the wake phrase (Ctrl-C to quit)");

        let mut pending = false; // right after a barge-in: record immediately, skip the wake wait
        let mut conversing = false; // follow-up: keep the mic open, no wake phrase needed
        loop {
            let following = conversing; // this turn's record is a reopened follow-up mic
            if !(pending || conversing) {
                // Wait for the wake phrase, but wake early to service a queued board event.
                if self.wait_for_wake_or_event() == Woke::Event {
                    self.drain_proactive();
                    continue;
                }
                eprintln!("[wake]");
                self.play_cue(&chime::listening());
            }
            pending = false;
            conversing = false;

            let timeout = if following {
                self.cfg.audio.followup_timeout_secs
            } else {
                self.cfg.audio.start_timeout_secs
            };
            let samples = audio::record_until_silence(&self.cap, &self.cfg.audio, timeout);
            if samples.is_empty() {
                continue; // nothing said → back to waiting for the wake phrase
            }
            self.play_cue(&chime::done()); // acknowledge we heard you stop

            let text = self.stt.transcribe(&samples);
            eprintln!("[you] {text}");
            if text.is_empty() {
                continue;
            }

            let reply = self.brain.ask(&text);
            eprintln!("[assistant] {reply}");
            if self.speak_interruptible(&reply) {
                eprintln!("[barge-in]");
                self.play_cue(&chime::listening());
                pending = true;
            } else if reply.trim_end().ends_with('?') {
                // The assistant asked something → keep the floor open for a natural reply.
                eprintln!("[listening for follow-up]");
                self.play_cue(&chime::listening());
                conversing = true;
            }
        }
    }

    /// Block on wake frames until the wake phrase fires OR a board event is queued. Port of the Python
    /// `listen_for_wake` (which returns `"wake"`/`"event"`).
    fn wait_for_wake_or_event(&mut self) -> Woke {
        let per_frame = Duration::from_millis(500);
        loop {
            // If the capture device faulted (e.g. the mic was unplugged mid-run), don't spin on a dead
            // stream — rebuild it, blocking until the device returns, then carry on (operator req #239:
            // survive hot-unplug, never crash). Reset the wake stream so stale pre-unplug state can't
            // linger into the reconnected stream.
            if !self.cap.healthy() {
                eprintln!("[audio] capture device lost; reconnecting…");
                self.cap = Capture::open_with_retry(&self.cfg.audio);
                self.wake.reset();
                eprintln!("[audio] capture device reconnected");
            }
            // A queued board event wakes the loop even without the wake phrase. The channel is
            // consume-on-read, so buffer what we drain for `drain_proactive` to handle.
            if let Some(w) = self.webhook.as_ref() {
                let drained = w.drain();
                if !drained.is_empty() {
                    self.pending_events.extend(drained);
                    return Woke::Event;
                }
            }
            if let Some(frame) = self.cap.next_frame(per_frame)
                && self.wake.accept(&frame)
            {
                return Woke::Wake;
            }
        }
    }
}

/// Why [`Assistant::wait_for_wake_or_event`] returned.
#[derive(PartialEq)]
enum Woke {
    Wake,
    Event,
}
