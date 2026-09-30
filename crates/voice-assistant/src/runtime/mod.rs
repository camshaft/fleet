//! `runtime` — the live voice loop, behind the `runtime` feature so the pure core builds/tests without a
//! native ONNX backend or audio device. It wires the sherpa-onnx wake/STT/TTS engines and cpal capture to
//! the main loop, and bridges the loop to the board over `bridge-core` (#316 / Doc #18).
//!
//! The assistant is a transport BRIDGE, not an in-process brain: a finalized transcript is posted to a
//! board voice channel (INBOUND), and a board-native voice agent ("George") replies there; the loop polls
//! the firehose and speaks those replies (OUTBOUND) — George's turn answers and any proactive posts share
//! one path. The board contract is all `bridge-core`; the only voice-specific work here is audio.
//!
//! The loop stays deliberately blocking and single-threaded (audio and board I/O never overlap): the
//! board client is async (operator directive #370), so the loop drives it with `rt.block_on` on a
//! current-thread tokio runtime at the two points it needs — posting a transcript and polling replies.
//! No background task, no shared-state locking; the audio work (cpal callback, sherpa STT/TTS) runs on its
//! own threads as before.

mod audio;
mod stt;
mod tts;
mod wake;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::bridge::{SpokenReply, VoiceBridge};
use crate::chime;
use crate::config::Config;

use audio::{Capture, StreamPlayer};
use stt::Transcriber;
use tts::TtsWorker;
use wake::WakeSpotter;

/// How long to wait for George's reply after posting a transcript before giving up on this turn.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// Poll cadence for proactive replies (George posting unprompted) while idle-waiting for the wake phrase.
const PROACTIVE_POLL_EVERY: Duration = Duration::from_secs(2);
/// Sleep between reply polls while awaiting a turn's answer (keeps the poll from busy-spinning the board).
const REPLY_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// The assembled engines + config + board session for one running assistant.
struct Assistant {
    cfg: Config,
    cap: Capture,
    wake: WakeSpotter,
    stt: Transcriber,
    tts: TtsWorker,
    /// The async board session (INBOUND post / OUTBOUND poll) driven via [`Assistant::rt`].
    bridge: VoiceBridge,
    /// The current-thread tokio runtime the loop uses to drive the async [`bridge`](Self::bridge) calls.
    rt: tokio::runtime::Runtime,
    /// Set true by the SIGTERM/SIGINT handler. The loop polls it and returns so the [`Assistant`] (and its
    /// [`Capture`] cpal stream) drops normally, releasing the ALSA device before the process exits.
    shutdown: Arc<AtomicBool>,
    /// Replies drained while idle-waiting for wake, held until [`speak_pending_replies`](Self::speak_pending_replies)
    /// speaks them (poll is consume-on-read, so we buffer instead of peeking).
    pending_replies: Vec<SpokenReply>,
}

/// Build every engine + the board session from config, then run the loop. Returns an error only if an
/// engine fails to initialize; the loop itself never returns except on a shutdown signal.
pub fn run(cfg: Config) -> Result<(), String> {
    // A current-thread runtime is enough: the loop drives async board I/O sequentially via block_on, with
    // no spawned tasks. `enable_all` turns on the IO + time drivers reqwest needs.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    // Open capture with retry-until-present rather than a fatal `?`: the daemon must stay up and wait for
    // the mic (operator requirement #239), never crash-loop when it's absent at startup.
    let cap = Capture::open_with_retry(&cfg.audio);
    let wake = WakeSpotter::new(&cfg.wake, cfg.audio.sample_rate)?;
    let stt = Transcriber::new(&cfg.stt, cfg.audio.sample_rate)?;
    let tts = TtsWorker::new(&cfg.tts)?;

    // Build the board session INSIDE the runtime so reqwest's client binds to this runtime; then merge
    // board-registered voice links with the static config (best-effort), and on a first run advance the
    // cursor to the firehose head so we don't replay the whole board backlog as speech.
    let bridge = rt.block_on(async {
        let mut b = VoiceBridge::new(
            &cfg.board.board_api,
            cfg.board.voice_channel.clone(),
            cfg.board.bridge_agent.clone(),
            cfg.board.speaker.clone(),
            cfg.board.state_dir.clone(),
            &cfg.board.channel_map,
        );
        b.refresh_map(&cfg.board.channel_map).await;
        if b.is_fresh() {
            b.initialize_cursor_at_head().await;
        }
        b
    });

    // Install a graceful-shutdown flag. The default SIGTERM action (systemd stop / deploy restart) would
    // terminate abruptly with no snd_pcm_close, so the ALSA device release lags and the next instance can
    // storm errno -32 on open (#239). Catching it lets the loop return so the Capture stream drops cleanly.
    let shutdown = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        if let Err(e) = signal_hook::flag::register(sig, shutdown.clone()) {
            eprintln!(
                "[voice-assistant] could not install signal handler for {sig} ({e}); \
                 continuing without graceful shutdown"
            );
        }
    }

    let mut a = Assistant {
        cfg,
        cap,
        wake,
        stt,
        tts,
        bridge,
        rt,
        shutdown,
        pending_replies: Vec::new(),
    };
    let outcome = a.main_loop();
    // Drop the Assistant (and with it the Capture cpal stream = snd_pcm_close) to release the ALSA
    // device BEFORE the process exits — whether we're stopping cleanly or bailing on a storm.
    drop(a);
    match outcome {
        Outcome::Shutdown => Ok(()),
        // A running-stream -32 storm an in-process reopen can't clear (#448): return an error so `main`
        // exits nonzero and systemd's `Restart=on-failure` relaunches a fresh process, which reopens the
        // now-released device clean.
        Outcome::Storm => {
            Err("capture stream error storm; exiting for a clean systemd restart".to_string())
        }
    }
}

impl Assistant {
    /// Play int16 PCM at the chime sample rate (cue tones) — write a temp WAV, play it, clean up.
    fn play_cue(&self, samples: &[i16]) {
        if let Ok(path) = audio::write_wav(samples, chime::CHIME_SR) {
            audio::play_wav(&path, &self.cfg.audio.output_device);
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Speak `text`, STREAMING the synthesized audio to the player chunk-by-chunk so playback starts on the
    /// first chunk instead of after the whole reply is synthesized — batch Kokoro synth of the full reply is
    /// the dominant reply→speaker latency (#454) and grows with length. The synthesizer runs on its own
    /// thread ([`TtsWorker`]) and streams PCM chunks over a channel; this loop pumps them to the player as
    /// they arrive AND polls the mic for a barge-in, so synthesis of later audio overlaps playback and the
    /// wake model stays live. A wake heard during playback aborts synthesis, kills playback, and returns
    /// `true`. Includes the "arm only after a low streak" debounce so the wake that opened this turn (or
    /// stale activation) can't count as a barge-in.
    fn speak(&mut self, text: &str) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        let abort = Arc::new(AtomicBool::new(false));
        let chunks = self.tts.speak(text, abort.clone());
        let mut player = StreamPlayer::start(self.tts.sample_rate(), &self.cfg.audio.output_device);
        self.wake.reset(); // the wake that opened this turn must not count as a barge-in

        const ARM_FRAMES: u32 = 3;
        let (mut armed, mut low_streak) = (false, 0u32);
        let mut interrupted = false;
        let mut synth_done = false;
        let mut first_chunk = true;
        // A short mic-frame wait is the loop clock: each pass drains any ready synth chunks to the player,
        // then services one wake frame for barge-in.
        let per_frame = Duration::from_millis(100);
        loop {
            // Move all currently-available synth chunks to the player (non-blocking).
            if !synth_done {
                loop {
                    match chunks.try_recv() {
                        Ok(pcm) => {
                            if first_chunk {
                                eprintln!("[tts] first audio chunk");
                                first_chunk = false;
                            }
                            player.write(&pcm);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            // Synthesis ended: no more input, so close stdin and let the player drain its
                            // buffer and exit on its own.
                            synth_done = true;
                            player.finish();
                            break;
                        }
                    }
                }
            }
            // Done once synthesis has ended AND the player has drained + exited.
            if synth_done && player.finished() {
                break;
            }
            // Barge-in: service one wake frame. On a hit (once armed), abort synthesis and kill playback.
            if let Some(frame) = self.cap.next_frame(per_frame) {
                let hit = self.wake.accept(&frame);
                if !armed {
                    // Arm only once the score has been quiet for a few frames (clears stale activation; a
                    // muted mic reads as silence, so it arms but nothing fires).
                    low_streak = if hit { 0 } else { low_streak + 1 };
                    if low_streak >= ARM_FRAMES {
                        armed = true;
                    }
                } else if hit {
                    interrupted = true;
                    abort.store(true, Ordering::Relaxed); // tell the worker to stop synthesizing
                    player.stop();
                    break;
                }
            }
        }
        interrupted
    }

    /// Speak the replies that came in a batch (a turn's answer or a proactive drain). Returns `true` if a
    /// barge-in interrupted playback (the caller reopens the mic). Speaks them in firehose order.
    fn speak_replies(&mut self, replies: Vec<SpokenReply>) -> bool {
        for r in replies {
            eprintln!("[assistant] {}", r.text);
            if self.speak(&r.text) {
                return true;
            }
        }
        false
    }

    /// Speak everything buffered from the idle proactive poll (George posted unprompted). A "ready" cue
    /// precedes the batch so the operator knows the assistant has something to say.
    fn speak_pending_replies(&mut self) {
        let replies = std::mem::take(&mut self.pending_replies);
        if replies.is_empty() {
            return;
        }
        self.play_cue(&chime::ready());
        self.speak_replies(replies);
    }

    /// After posting a transcript, poll the firehose for George's reply until something arrives or
    /// [`REPLY_TIMEOUT`] elapses. Returns the replies (empty on timeout / shutdown).
    fn await_reply(&mut self) -> Vec<SpokenReply> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                return Vec::new();
            }
            match self.rt.block_on(self.bridge.poll_replies()) {
                Ok(r) if !r.is_empty() => return r,
                Ok(_) => {}
                Err(e) => eprintln!("[voice-bridge] reply poll failed: {e}"),
            }
            if Instant::now() >= deadline {
                eprintln!("[voice-bridge] no reply within {REPLY_TIMEOUT:?}");
                return Vec::new();
            }
            std::thread::sleep(REPLY_POLL_INTERVAL);
        }
    }

    /// The main loop: wake → chime → record → STT → post transcript → await reply → speak, with barge-in
    /// and follow-up. Proactive board replies are drained + spoken at idle.
    fn main_loop(&mut self) -> Outcome {
        self.play_cue(&chime::ready());
        eprintln!("[voice-assistant] ready — waiting for the wake phrase (Ctrl-C to quit)");

        let mut pending = false; // right after a barge-in: record immediately, skip the wake wait
        let mut conversing = false; // follow-up: keep the mic open, no wake phrase needed
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                eprintln!(
                    "[voice-assistant] shutdown signal — releasing the capture device and exiting"
                );
                return Outcome::Shutdown;
            }
            if self.cap.stormed() {
                return Outcome::Storm;
            }
            let following = conversing; // this turn's record is a reopened follow-up mic
            if !(pending || conversing) {
                // Wait for the wake phrase, but wake early to speak a proactive board reply or to shut down.
                match self.wait_for_wake_or_event() {
                    Woke::Shutdown => {
                        eprintln!(
                            "[voice-assistant] shutdown signal — releasing the capture device and exiting"
                        );
                        return Outcome::Shutdown;
                    }
                    Woke::Storm => return Outcome::Storm,
                    Woke::Event => {
                        self.speak_pending_replies();
                        continue;
                    }
                    Woke::Wake => {}
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

            // INBOUND: post the transcript to the board voice channel for George to answer.
            if let Err(e) = self.rt.block_on(self.bridge.post_transcript(&text)) {
                eprintln!("[voice-bridge] transcript post failed: {e}");
                continue;
            }
            // OUTBOUND: wait for George's reply on the firehose, then speak it.
            let replies = self.await_reply();
            if replies.is_empty() {
                continue;
            }
            let last = replies.last().map(|r| r.text.clone()).unwrap_or_default();
            if self.speak_replies(replies) {
                eprintln!("[barge-in]");
                self.play_cue(&chime::listening());
                pending = true;
            } else if last.trim_end().ends_with('?') {
                // The reply asked something → keep the mic open for a natural follow-up.
                eprintln!("[listening for follow-up]");
                self.play_cue(&chime::listening());
                conversing = true;
            }
        }
    }

    /// Block on wake frames until the wake phrase fires OR a proactive board reply is available. Polls the
    /// firehose on a cadence while listening, so George posting unprompted wakes the loop.
    fn wait_for_wake_or_event(&mut self) -> Woke {
        let per_frame = Duration::from_millis(500);
        let mut next_poll = Instant::now(); // poll immediately on entry, then every PROACTIVE_POLL_EVERY
        loop {
            // A shutdown signal ends the idle wait promptly (this is where the daemon sits almost all the
            // time, so it is the state a deploy stop lands in) — return so the loop can drop the stream.
            if self.shutdown.load(Ordering::Relaxed) {
                return Woke::Shutdown;
            }
            // A running-stream error storm can't be cleared by an in-process reopen — bail so the caller
            // exits for a clean systemd restart (#448).
            if self.cap.stormed() {
                return Woke::Storm;
            }
            // If the capture device faulted (e.g. the mic was unplugged mid-run), don't spin on a dead
            // stream — rebuild it, blocking until the device returns (operator req #239: survive hot-unplug,
            // never crash). Reset the wake stream so stale pre-unplug state can't linger.
            if !self.cap.healthy() {
                eprintln!("[audio] capture device lost; reconnecting…");
                self.cap = Capture::open_with_retry(&self.cfg.audio);
                self.wake.reset();
                eprintln!("[audio] capture device reconnected");
            }
            // A proactive board reply wakes the loop even without the wake phrase. Buffer what we drain for
            // speak_pending_replies to handle.
            if Instant::now() >= next_poll {
                next_poll = Instant::now() + PROACTIVE_POLL_EVERY;
                match self.rt.block_on(self.bridge.poll_replies()) {
                    Ok(r) if !r.is_empty() => {
                        self.pending_replies.extend(r);
                        return Woke::Event;
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("[voice-bridge] proactive poll failed: {e}"),
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
    /// A SIGTERM/SIGINT arrived while waiting; the caller should return and let the stream drop.
    Shutdown,
    /// The capture stream is storming (running-stream -32 flood); the caller should exit for a clean
    /// systemd restart, since an in-process reopen can't clear it (#448).
    Storm,
}

/// Why [`Assistant::main_loop`] returned — decides the process exit code.
enum Outcome {
    /// A shutdown signal (SIGTERM/SIGINT): stop cleanly, exit 0.
    Shutdown,
    /// A capture-stream error storm an in-process reopen can't clear: exit NONZERO so systemd's
    /// `Restart=on-failure` relaunches a fresh process (which reopens the device clean) — #448.
    Storm,
}
