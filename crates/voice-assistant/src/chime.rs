//! `chime` — the wake/done/ready cue tones, as pure sample synthesis (ported from the Python
//! `_note`/`_play_tones`). Kept backend-free: this module only *builds* the int16 PCM for a cue; the
//! `runtime` module plays it. That keeps the tone math unit-testable with no audio device.

/// The chime sample rate (Hz). The cues are synthesized at this rate independent of the capture rate.
pub const CHIME_SR: u32 = 22050;

/// One note: `(freq_hz, duration_s, amplitude)`.
pub type Note = (f32, f32, f32);

/// Synthesize one note into `out` as f32 samples in [-1, 1], with a short linear fade in/out so the
/// tones don't click (ported from the Python `_note`: fade-in 8 ms, fade-out 40 ms).
fn render_note(out: &mut Vec<f32>, sr: u32, freq: f32, dur: f32, amp: f32) {
    let n = (sr as f32 * dur) as usize;
    for i in 0..n {
        let t = i as f32 / sr as f32;
        let env = (t / 0.008).min((dur - t) / 0.04).clamp(0.0, 1.0);
        out.push(amp * env * (std::f32::consts::TAU * freq * t).sin());
    }
}

/// Render a sequence of notes to int16 PCM mono at [`CHIME_SR`]. Pure — no I/O.
pub fn render(notes: &[Note]) -> Vec<i16> {
    let mut f = Vec::new();
    for &(freq, dur, amp) in notes {
        render_note(&mut f, CHIME_SR, freq, dur, amp);
    }
    f.iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

/// A subtle RISING two-note "listening" cue (wake heard): A5 → D6.
pub fn listening() -> Vec<i16> {
    render(&[(880.0, 0.08, 0.18), (1174.7, 0.11, 0.18)])
}

/// A subtle FALLING two-note "got it" cue (detected you stopped): C6 → G5.
pub fn done() -> Vec<i16> {
    render(&[(1046.5, 0.07, 0.14), (784.0, 0.10, 0.14)])
}

/// A distinct rising three-note "online/ready" cue: C5 → E5 → G5.
pub fn ready() -> Vec<i16> {
    render(&[
        (523.25, 0.09, 0.16),
        (659.25, 0.09, 0.16),
        (784.0, 0.14, 0.16),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_have_sane_lengths() {
        // ~0.19s of audio for the two-note cues, ~0.32s for the three-note ready cue, at 22050 Hz.
        assert!((listening().len() as i32 - (0.19 * CHIME_SR as f32) as i32).abs() < 200);
        assert!((ready().len() as i32 - (0.32 * CHIME_SR as f32) as i32).abs() < 300);
    }

    #[test]
    fn samples_are_in_int16_range_and_start_quiet() {
        let s = ready();
        assert!(!s.is_empty());
        // The fade-in means the very first sample is ~silence, never a click at full amplitude.
        assert!(s[0].abs() < 3000);
        // Nothing clips past int16.
        assert!(s.iter().all(|&x| (i16::MIN..=i16::MAX).contains(&x)));
    }

    #[test]
    fn silent_note_when_amplitude_zero() {
        assert!(render(&[(440.0, 0.05, 0.0)]).iter().all(|&x| x == 0));
    }
}
