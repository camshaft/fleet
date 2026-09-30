//! `runtime::audio` — microphone capture (cpal) with an energy VAD, and speaker playback.
//!
//! Capture mirrors the Python `record_until_silence`: pull fixed frames, RMS each one, start on the first
//! loud frame, stop after enough trailing quiet (but only once real speech has been heard), and give up if
//! nothing is said within a start window. cpal delivers audio on its own callback thread, so a frame
//! channel bridges it to the blocking VAD loop.
//!
//! Playback mirrors the Python TTS: write a temp WAV and hand it to an external player (`paplay`/`pw-play`/
//! `aplay`) — `sounddevice.play()` hung on the box, and cpal output has the same class of driver trouble,
//! so the external-player path is the validated one. Playback is exposed blocking ([`play_wav`]) and async
//! ([`Playback`]) so the loop can barge-in and kill it. With `[audio].output_device` set, playback pins to
//! a single deterministic `aplay -D <device>` (bypassing player auto-selection + the ALSA `default` PCM,
//! which is a dead PipeWire sink for a session-less system service — #296).

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::config::Audio;
use crate::retry::next_backoff;

/// The external players tried in order (first that exists wins), matching the Python `_PLAYERS`.
const PLAYERS: &[&[&str]] = &[&["paplay"], &["pw-play"], &["aplay", "-q"]];

/// The player invocations to try, in order. With an explicit `output_device`, use ONE deterministic
/// `aplay -q -D <device>` — this bypasses player auto-selection, PATH ordering, and the ALSA `default`
/// PCM (a dead PipeWire sink for a session-less service, #296). Empty → the best-effort default list.
fn players(output_device: &str) -> Vec<Vec<String>> {
    if output_device.is_empty() {
        PLAYERS
            .iter()
            .map(|p| p.iter().map(|s| s.to_string()).collect())
            .collect()
    } else {
        vec![vec![
            "aplay".to_string(),
            "-q".to_string(),
            "-D".to_string(),
            output_device.to_string(),
        ]]
    }
}

/// Open the configured input device (name-substring match, else the host default) and start an int16 mono
/// capture stream at `sample_rate`, delivering `frame`-sized chunks over the returned channel. The stream
/// stays alive as long as the returned [`Capture`] is held.
pub struct Capture {
    _stream: cpal::Stream,
    frames: Receiver<Vec<i16>>,
    frame: usize,
    /// Set by cpal's error callback when the stream faults (typically `StreamError::DeviceNotAvailable`
    /// on a hot-unplug). The loop polls [`healthy`](Self::healthy) and rebuilds the capture when it flips.
    dead: Arc<AtomicBool>,
}

impl Capture {
    /// Open capture per the [`Audio`] config. Errors if no input device is available or the stream can't
    /// be built at the requested rate. See [`open_with_retry`](Self::open_with_retry) for the non-fatal
    /// path the daemon actually uses.
    pub fn open(audio: &Audio) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = pick_input(&host, &audio.input_device)
            .ok_or_else(|| "no input device available".to_string())?;
        if let Ok(name) = device.name() {
            eprintln!("[audio] capture device: {name}");
        }
        let cfg = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(audio.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        // cpal hands us arbitrary-sized buffers on its callback thread; re-chunk to exact frames so the
        // VAD sees the same geometry the wake/STT models expect.
        let frame = audio.frame;
        let mut acc: Vec<i16> = Vec::with_capacity(frame * 2);
        // A device fault (unplug) is delivered to the error callback, not the data callback, so record it
        // on a shared flag the main loop can see and act on (reconnect) rather than crashing.
        let dead = Arc::new(AtomicBool::new(false));
        let dead_cb = dead.clone();
        let err_fn = move |e| {
            eprintln!("[audio] capture stream error: {e}");
            dead_cb.store(true, Ordering::Relaxed);
        };
        let stream = device
            .build_input_stream(
                &cfg,
                move |data: &[i16], _| {
                    acc.extend_from_slice(data);
                    while acc.len() >= frame {
                        let chunk: Vec<i16> = acc.drain(..frame).collect();
                        // A full channel means the consumer stalled; drop rather than block the callback.
                        let _ = tx.send(chunk);
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("build_input_stream: {e}"))?;
        stream.play().map_err(|e| format!("stream.play: {e}"))?;
        Ok(Self {
            _stream: stream,
            frames: rx,
            frame,
            dead,
        })
    }

    /// Open capture, retrying with capped backoff until it succeeds — NEVER fatal. This is the operator's
    /// requirement (#239): with no mic present at startup the daemon must stay up and keep trying, then
    /// begin the loop the moment a device appears, instead of exiting and letting the supervisor
    /// crash-loop. Blocks until a device is open.
    pub fn open_with_retry(audio: &Audio) -> Self {
        let mut backoff = Duration::ZERO;
        loop {
            match Self::open(audio) {
                Ok(c) => return c,
                Err(e) => {
                    backoff = next_backoff(backoff);
                    eprintln!(
                        "[audio] capture open failed ({e}); retrying in {backoff:?} (waiting for a device)"
                    );
                    std::thread::sleep(backoff);
                }
            }
        }
    }

    /// True while the capture stream is healthy; flips to false once cpal reports a stream error (e.g. the
    /// device was unplugged). The loop uses this to trigger a reconnect.
    pub fn healthy(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }

    /// Block for the next frame, up to `timeout`. `None` on timeout or if the stream has ended.
    pub fn next_frame(&self, timeout: Duration) -> Option<Vec<i16>> {
        match self.frames.recv_timeout(timeout) {
            Ok(f) => Some(f),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// The configured frame size (samples).
    pub fn frame(&self) -> usize {
        self.frame
    }
}

/// Pick the input device whose name contains `want` (case-insensitive), else the host default. An empty
/// `want` goes straight to the default.
///
/// NOTE (#279): cpal reports ALSA **PCM names** (e.g. `sysdefault:CARD=USB`, `front:CARD=USB,DEV=0`) from
/// `device.name()`, NOT the human card description ("Jabra SPEAK 410 USB") — so `want` must match the PCM
/// name. When nothing matches we LOG the available names so an operator can see exactly what to configure,
/// rather than silently falling back to `default` (which on some hosts can't even be opened).
fn pick_input(host: &cpal::Host, want: &str) -> Option<cpal::Device> {
    if want.is_empty() {
        return host.default_input_device();
    }
    let want_lc = want.to_lowercase();
    let devices: Vec<cpal::Device> = match host.input_devices() {
        Ok(devs) => devs.collect(),
        Err(e) => {
            eprintln!("[audio] could not enumerate input devices ({e}); using the default device");
            return host.default_input_device();
        }
    };
    if let Some(d) = devices.iter().find(|d| {
        d.name()
            .map(|n| n.to_lowercase().contains(&want_lc))
            .unwrap_or(false)
    }) {
        return Some(d.clone());
    }
    // No match — surface what IS available so the misconfiguration is self-diagnosing (the substring must
    // match a listed PCM name; the human device label is not what cpal exposes).
    let names: Vec<String> = devices.iter().filter_map(|d| d.name().ok()).collect();
    eprintln!(
        "[audio] no input device matches {want:?}; available input devices: [{}]. Falling back to the \
         default device — set audio.input_device to a substring of one of the names above (e.g. \
         \"CARD=USB\" for a USB mic).",
        names.join(", ")
    );
    host.default_input_device()
}

/// int16 RMS of one frame — the energy VAD's speech/silence measure (ported from the Python `np.sqrt(
/// np.mean(frame**2))`).
pub fn rms(frame: &[i16]) -> f64 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / frame.len() as f64).sqrt()
}

/// Record from `cap` until ~`silence_secs` of quiet follows real speech, giving up if nothing is said
/// within `start_timeout`. Returns int16 mono samples (empty if nothing was said). Direct port of the
/// Python `record_until_silence` VAD state machine.
pub fn record_until_silence(cap: &Capture, audio: &Audio, start_timeout: f64) -> Vec<i16> {
    let sr = audio.sample_rate as f64;
    let frame = cap.frame() as f64;
    let max_frames = (audio.max_utterance_secs * sr / frame) as usize;
    let need = (audio.silence_secs * sr / frame) as usize;
    let min_speech = ((audio.min_speech_secs * sr / frame) as usize).max(1);
    let start_frames = (start_timeout * sr / frame) as usize;
    // A generous per-frame wait: at 80 ms frames, 1 s covers any scheduling jitter without hanging.
    let per_frame = Duration::from_secs(1);

    let mut frames: Vec<i16> = Vec::new();
    let (mut silence, mut speech, mut started) = (0usize, 0usize, false);
    for i in 0..max_frames {
        let Some(f) = cap.next_frame(per_frame) else {
            break;
        };
        frames.extend_from_slice(&f);
        if rms(&f) >= audio.vad_rms {
            speech += 1;
            silence = 0;
            started = true;
        } else if started {
            silence += 1;
        }
        // Nobody started talking within the start window → give up (a reopened follow-up mic must not
        // hang when the user says nothing).
        if !started && i >= start_frames {
            break;
        }
        // Stop only after real speech AND a solid trailing pause (so mid-sentence pauses don't cut off).
        if started && speech >= min_speech && silence >= need {
            break;
        }
    }
    if started { frames } else { Vec::new() }
}

/// Write int16 mono PCM to a temp WAV at `sr` and return its path (a hand-rolled 44-byte header — no wav
/// crate needed for mono PCM16). Mirrors the Python `_write_wav`.
pub fn write_wav(samples: &[i16], sr: u32) -> std::io::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("voice-assistant-{}.wav", wav_nonce()));
    let mut f = std::fs::File::create(&path)?;
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sr * 2;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // PCM fmt chunk size
    f.write_all(&1u16.to_le_bytes())?; // audio format = PCM
    f.write_all(&1u16.to_le_bytes())?; // channels = mono
    f.write_all(&sr.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits per sample
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(path)
}

/// A per-process-monotonic filename nonce. Avoids `Date`/`rand` deps; a static counter is enough to keep
/// concurrent temp WAVs distinct within one run.
fn wav_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed)
}

/// Play a WAV file, blocking until done. With `output_device` set, uses `aplay -D <device>`; else the
/// best-effort player list. Best-effort (a box with no working player just stays silent). Mirrors the
/// Python `_play_blocking`.
pub fn play_wav(path: &std::path::Path, output_device: &str) {
    let list = players(output_device);
    for player in &list {
        let mut cmd = Command::new(&player[0]);
        cmd.args(&player[1..])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.status() {
            Ok(s) if s.success() => return,
            // A player that RAN but exited nonzero (e.g. paplay into a dead PipeWire sink) falls through
            // to the next candidate rather than giving up (#296). With an explicit output_device there's
            // only the one aplay entry, so this simply ends the loop.
            Ok(_) => continue,
            Err(_) => continue, // not installed → try the next player
        }
    }
    let tried: Vec<&str> = list.iter().map(|p| p[0].as_str()).collect();
    eprintln!("[audio] no working audio player (tried: {})", tried.join(", "));
}

/// A killable background playback (for barge-in). Mirrors the Python `play_async` + terminate/kill.
pub struct Playback {
    child: Option<Child>,
}

impl Playback {
    /// Start playing `path` in the background. With `output_device` set, uses `aplay -D <device>`; else
    /// the best-effort player list. Returns a handle even if no player is found (then
    /// [`finished`](Self::finished) is immediately true).
    pub fn start(path: &std::path::Path, output_device: &str) -> Self {
        for player in players(output_device) {
            let child = Command::new(&player[0])
                .args(&player[1..])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(c) = child {
                return Self { child: Some(c) };
            }
        }
        Self { child: None }
    }

    /// True once playback has ended on its own (or never started).
    pub fn finished(&mut self) -> bool {
        match &mut self.child {
            None => true,
            Some(c) => matches!(c.try_wait(), Ok(Some(_)) | Err(_)),
        }
    }

    /// Stop playback now (barge-in): terminate, then kill if it doesn't exit promptly.
    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero_and_of_full_scale_is_large() {
        assert_eq!(rms(&[0, 0, 0, 0]), 0.0);
        assert!(rms(&[]) == 0.0);
        let loud = [i16::MAX; 8];
        assert!(rms(&loud) > 30000.0);
    }

    #[test]
    fn write_wav_emits_a_riff_header() {
        let path = write_wav(&[0, 100, -100, 200], 22050).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // 44-byte header + 4 samples * 2 bytes
        assert_eq!(bytes.len(), 44 + 8);
        let _ = std::fs::remove_file(&path);
    }
}
