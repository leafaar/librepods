//! AGC (Automatic Gain Control) for audio processing.
//!
//! A peak envelope follower sets the gain that brings speech to TARGET. The time
//! constants are chosen for voice: the envelope and the gain must move slowly
//! compared with one voice period (2-10 ms), or the gain changes inside each
//! period and distorts the waveform. A separate fast level detector gates gain
//! updates, so pauses and background noise below NOISE_FLOOR hold the gain
//! instead of pulling it up to MAX_GAIN.

pub struct Agc {
    // Slow peak envelope that sets the gain.
    envelope: f32,
    // Fast peak level that only decides whether speech is present.
    gate_level: f32,
    gain: f32,
    attack: f32,
    release: f32,
    gate_release: f32,
    smoothing: f32,
}

// One-pole smoothing coefficient for time constant `tau_secs` at `sample_rate`.
fn coefficient(tau_secs: f64, sample_rate: u32) -> f32 {
    (1.0 - (-1.0 / (tau_secs * f64::from(sample_rate))).exp()) as f32
}

impl Agc {
    const TARGET: f32 = 0.25; // ≈ -12 dBFS
    const MAX_GAIN: f32 = 8.0; // +18 dB
    // ≈ -40 dBFS peak: quieter than speech, louder than a typical mic noise floor.
    const NOISE_FLOOR: f32 = 0.01;
    const ATTACK_SECS: f64 = 0.008;
    const RELEASE_SECS: f64 = 0.4;
    const GATE_RELEASE_SECS: f64 = 0.05;
    const GAIN_SECS: f64 = 0.08;

    pub fn new(sample_rate: u32) -> Self {
        Self {
            envelope: 0.0,
            gate_level: 0.0,
            gain: 1.0,
            attack: coefficient(Self::ATTACK_SECS, sample_rate),
            release: coefficient(Self::RELEASE_SECS, sample_rate),
            gate_release: coefficient(Self::GATE_RELEASE_SECS, sample_rate),
            smoothing: coefficient(Self::GAIN_SECS, sample_rate),
        }
    }

    pub fn process(&mut self, samples: &mut [i16]) {
        for s in samples {
            let x = f32::from(*s) / 32768.0;
            let level = x.abs();

            if level > self.envelope {
                self.envelope += (level - self.envelope) * self.attack;
            } else {
                self.envelope += (level - self.envelope) * self.release;
            }
            if level > self.gate_level {
                self.gate_level += (level - self.gate_level) * self.attack;
            } else {
                self.gate_level += (level - self.gate_level) * self.gate_release;
            }

            // Hold the gain while nothing but noise is coming in.
            if self.gate_level >= Self::NOISE_FLOOR {
                let desired = (Self::TARGET / self.envelope.max(1e-4)).min(Self::MAX_GAIN);
                self.gain += (desired - self.gain) * self.smoothing;
            }

            // Soft limiter: tanh keeps |y| <= 1, so the cast below cannot overflow
            // (and `as` saturates regardless).
            let y = (x * self.gain).tanh();
            *s = (y * 32767.0) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 64_000;

    fn tone(amplitude: f32, secs: f32) -> Vec<i16> {
        let n = (secs * RATE as f32) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let x = amplitude * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                (x * 32767.0) as i16
            })
            .collect()
    }

    // Deterministic white noise with peak `amplitude` (xorshift32).
    fn noise(amplitude: f32, secs: f32) -> Vec<i16> {
        let n = (secs * RATE as f32) as usize;
        let mut state: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let u = state as f32 / u32::MAX as f32 * 2.0 - 1.0;
                (u * amplitude * 32767.0) as i16
            })
            .collect()
    }

    fn peak(samples: &[i16]) -> f32 {
        samples
            .iter()
            .map(|&s| (f32::from(s) / 32768.0).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn steady_tone_converges_near_target() {
        for amplitude in [0.05, 0.25, 0.8] {
            let mut agc = Agc::new(RATE);
            let mut pcm = tone(amplitude, 3.0);
            agc.process(&mut pcm);
            let tail = &pcm[pcm.len() - RATE as usize / 4..];
            let p = peak(tail);
            assert!(
                (0.2..=0.3).contains(&p),
                "amplitude {amplitude}: output peak {p}"
            );
        }
    }

    #[test]
    fn silence_and_noise_floor_do_not_raise_gain() {
        let mut agc = Agc::new(RATE);
        let mut pcm = vec![0i16; RATE as usize * 5];
        agc.process(&mut pcm);
        assert!(agc.gain <= 1.0, "silence gain {}", agc.gain);

        let mut agc = Agc::new(RATE);
        let mut pcm = noise(0.005, 5.0);
        agc.process(&mut pcm);
        assert!(agc.gain <= 1.0, "noise gain {}", agc.gain);
    }

    #[test]
    fn pause_after_speech_holds_gain() {
        let mut agc = Agc::new(RATE);
        let mut speech = tone(0.1, 2.0);
        agc.process(&mut speech);
        let speaking = agc.gain;

        let mut pause = noise(0.005, 5.0);
        agc.process(&mut pause);
        assert!(
            agc.gain <= speaking * 1.25,
            "gain went from {speaking} to {} in the pause",
            agc.gain
        );
        assert!(agc.gain < Agc::MAX_GAIN / 2.0);
    }

    #[test]
    fn output_stays_in_range_without_wrapping() {
        let mut agc = Agc::new(RATE);
        // Drive the gain up with a quiet tone, then hit it with full scale.
        let mut quiet = tone(0.035, 2.0);
        agc.process(&mut quiet);
        assert!(agc.gain > 4.0, "gain {}", agc.gain);

        let input: Vec<i16> = (0..RATE as usize / 10)
            .map(|i| if i % 64 < 32 { i16::MAX } else { i16::MIN })
            .collect();
        let mut pcm = input.clone();
        agc.process(&mut pcm);
        for (&x, &y) in input.iter().zip(&pcm) {
            assert!(y != i16::MIN && (y > 0) == (x > 0), "{x} -> {y}");
        }
    }
}
