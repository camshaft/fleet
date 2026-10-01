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
/// If the idle wait sees NO capture frames for this long, treat the stream as silently dead and reconnect.
/// A live ALSA stream delivers frames continuously (ambient silence is ~zero-value samples, but frames keep
/// arriving at the sample rate), so a gap this long means a phantom/dead device — a hot-unplug cpal never
/// signaled via the error callback, or a default-device latch (task-575). Generous so a slow board poll or
/// scheduling jitter can't false-trip it.
const CAPTURE_LIVENESS_TIMEOUT: Duration = Duration::from_secs(15);
/// Startup readiness deadline (task_762). If the daemon hasn't reached "ready" within this window AND isn't
/// legitimately waiting for an absent device, a silent init hang is assumed (observed: a post-storm restart
/// wedging in the PipeWire/ALSA capture open — "active" but 0 frames read, no models loaded, no logs,
/// forever) and the process hard-exits for a clean systemd restart. Generous enough to cover a cold
/// capture-open + the wake/STT/TTS model loads + the board session; the legitimate wait-for-an-absent-mic is
/// exempted (via `awaiting_device`) so it never crash-loops (#239).
const STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(90);

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

    // Startup readiness watchdog (task_762): a silent init hang — a post-storm restart wedging inside the
    // PipeWire/ALSA capture open, "active" but never reaching ready — emits no error, so neither the -32
    // storm hard-exit (#448) nor systemd `Restart=` catches it; it just sits dead forever. Arm a watchdog
    // that forces a clean exit if we don't reach `ready` in time, so systemd relaunches a fresh process.
    // `awaiting_device` exempts the legitimate wait-for-an-absent-mic (#239) so that path never crash-loops.
    let ready = Arc::new(AtomicBool::new(false));
    let awaiting_device = Arc::new(AtomicBool::new(false));
    spawn_startup_watchdog(ready.clone(), awaiting_device.clone());

    // Open capture with retry-until-present rather than a fatal `?`: the daemon must stay up and wait for
    // the mic (operator requirement #239), never crash-loop when it's absent at startup.
    let cap = Capture::open_with_retry(&cfg.audio, Some(&*awaiting_device));
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
    // All hang-prone init (capture open + model loads + board session) is done — mark ready so the startup
    // watchdog (task_762) stands down. From here main_loop runs until a shutdown signal.
    ready.store(true, Ordering::Relaxed);
    a.main_loop();
    // main_loop returns only on a shutdown signal. A running-stream -32 storm instead hard-exits the
    // process directly from the capture error callback (a graceful drop deadlocks on the faulted fd — #448).
    // Returning here drops `a`, and with it the Capture stream, releasing the ALSA device before exit.
    Ok(())
}

/// Arm the startup readiness watchdog (task_762). On its own thread: if the daemon hasn't set `ready` within
/// [`STARTUP_READY_TIMEOUT`] AND isn't legitimately waiting for an absent device, assume a silent init wedge
/// (e.g. a post-storm restart hung in the PipeWire/ALSA capture open) and hard-exit so systemd relaunches a
/// fresh process — the same self-recovery the -32 storm hard-exit (#448) gives, for the no-error hang the
/// storm guard and systemd both miss. Returns once `ready` is observed (startup succeeded); a no-op after.
fn spawn_startup_watchdog(ready: Arc<AtomicBool>, awaiting_device: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("startup-watchdog".into())
        .spawn(move || {
            let mut deadline = Instant::now() + STARTUP_READY_TIMEOUT;
            loop {
                std::thread::sleep(Duration::from_secs(3));
                if ready.load(Ordering::Relaxed) {
                    return; // reached ready — startup succeeded, watchdog stands down
                }
                if awaiting_device.load(Ordering::Relaxed) {
                    // Legitimately blocked waiting for an absent/unsettled device (#239): not a wedge, so
                    // keep pushing the deadline — the daemon may wait for the mic arbitrarily long.
                    deadline = Instant::now() + STARTUP_READY_TIMEOUT;
                } else if Instant::now() >= deadline {
                    eprintln!(
                        "[voice-assistant] startup wedged: not ready within {STARTUP_READY_TIMEOUT:?} and \
                         not awaiting a device (likely a silent capture-init hang after a storm restart); \
                         exiting for a clean systemd restart (task_762)"
                    );
                    std::process::exit(1);
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!(
            "[voice-assistant] could not spawn startup watchdog ({e}); continuing without it"
        );
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
    fn main_loop(&mut self) {
        self.play_cue(&chime::ready());
        eprintln!("[voice-assistant] ready — waiting for the wake phrase (Ctrl-C to quit)");

        let mut pending = false; // right after a barge-in: record immediately, skip the wake wait
        let mut conversing = false; // follow-up: keep the mic open, no wake phrase needed
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                eprintln!(
                    "[voice-assistant] shutdown signal — releasing the capture device and exiting"
                );
                return;
            }
            let following = conversing; // this turn's record is a reopened follow-up mic
            if !(pending || conversing) {
                // Wait for the wake phrase, but wake early to speak a proactive board reply or to shut down.
                match self.wait_for_wake_or_event() {
                    Woke::Shutdown => {
                        eprintln!(
                            "[voice-assistant] shutdown signal — releasing the capture device and exiting"
                        );
                        return;
                    }
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
    /// Rebuild the capture stream, blocking until a device opens (operator req #239: survive a hot-unplug,
    /// never crash), and reset the wake model so stale pre-fault state can't linger. Shared by the idle
    /// wait's two fault-detection paths: the cpal error callback (`!healthy()`) and the liveness guard.
    fn reconnect_capture(&mut self, reason: &str) {
        eprintln!("[audio] {reason}; reconnecting…");
        self.cap = Capture::open_with_retry(&self.cfg.audio, None);
        self.wake.reset();
        eprintln!("[audio] capture device reconnected");
    }

    fn wait_for_wake_or_event(&mut self) -> Woke {
        let per_frame = Duration::from_millis(500);
        let mut next_poll = Instant::now(); // poll immediately on entry, then every PROACTIVE_POLL_EVERY
        let mut last_frame = Instant::now(); // liveness guard: when we last received capture audio
        loop {
            // A shutdown signal ends the idle wait promptly (this is where the daemon sits almost all the
            // time, so it is the state a deploy stop lands in) — return so the loop can drop the stream.
            if self.shutdown.load(Ordering::Relaxed) {
                return Woke::Shutdown;
            }
            // If the mic was unplugged mid-run cpal signals the error callback — rebuild the stream.
            if !self.cap.healthy() {
                self.reconnect_capture("capture device lost");
                last_frame = Instant::now();
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
            // Liveness guard: cpal does NOT always fire the error callback on a fault (the task-448 -32 saga
            // showed this), and a default-device latch never faults at all — either way the stream just
            // stops delivering frames while `healthy()` stays true. A live stream delivers them
            // continuously, so no frame for CAPTURE_LIVENESS_TIMEOUT means it is silently dead → reconnect
            // (re-enumerating, so a replugged device is picked up with no restart — task-575).
            match self.cap.next_frame(per_frame) {
                Some(frame) => {
                    last_frame = Instant::now();
                    if self.wake.accept(&frame) {
                        return Woke::Wake;
                    }
                }
                None => {
                    if last_frame.elapsed() > CAPTURE_LIVENESS_TIMEOUT {
                        self.reconnect_capture(&format!(
                            "no capture frames for {CAPTURE_LIVENESS_TIMEOUT:?} (stream silently dead)"
                        ));
                        last_frame = Instant::now();
                    }
                }
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
}
