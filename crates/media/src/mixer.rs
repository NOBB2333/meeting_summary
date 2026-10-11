use opencap_core::{AudioChunk, AudioKind};

/// Timestamp-addressed mixer. Missing routes are silence; samples arriving after the
/// playout cursor are dropped. This prevents device-clock drift growing without bound.
pub struct AudioMixer {
    samples: Vec<[[f32; 2]; 2]>,
    tags: Vec<i64>,
    pub cursor: i64,
}
impl AudioMixer {
    pub fn new() -> Self {
        Self {
            samples: vec![[[0.0; 2]; 2]; 96_000],
            tags: vec![-1; 96_000],
            cursor: 0,
        }
    }
    pub fn push(&mut self, chunk: AudioChunk) {
        let route = usize::from(chunk.kind == AudioKind::Microphone);
        let first = chunk.pts_us * 48 / 1000;
        for (i, pair) in chunk.samples.chunks_exact(2).enumerate() {
            let index = first + i as i64;
            if index < self.cursor || index >= self.cursor + 96_000 {
                continue;
            }
            let slot = index as usize % self.samples.len();
            if self.tags[slot] != index {
                self.tags[slot] = index;
                self.samples[slot] = [[0.0; 2]; 2];
            }
            let sample = &mut self.samples[slot][route];
            sample[0] += if pair[0].is_finite() { pair[0] } else { 0.0 };
            sample[1] += if pair[1].is_finite() { pair[1] } else { 0.0 };
        }
    }
    pub fn clear_route(&mut self, kind: AudioKind) {
        let route = usize::from(kind == AudioKind::Microphone);
        for sample in &mut self.samples {
            sample[route] = [0.0; 2];
        }
    }
    pub fn take(&mut self, count: usize) -> Vec<[f32; 2]> {
        (0..count)
            .map(|_| {
                let slot = self.cursor as usize % self.samples.len();
                let value = if self.tags[slot] == self.cursor {
                    let routes = self.samples[slot];
                    [routes[0][0] + routes[1][0], routes[0][1] + routes[1][1]]
                } else {
                    [0.0; 2]
                };
                self.tags[slot] = -1;
                self.cursor += 1;
                [soft_limit(value[0]), soft_limit(value[1])]
            })
            .collect()
    }
}

/// Near-transparent soft limiter: sums below `THRESHOLD` pass unchanged, while
/// louder material bends smoothly toward 1.0 with `tanh`. A hard `clamp` here
/// clips single samples to a flat top, which is exactly what audible crackle is;
/// this keeps the ceiling without flattening the waveform.
pub fn soft_limit(value: f32) -> f32 {
    const THRESHOLD: f32 = 0.85;
    const RANGE: f32 = 1.0 - THRESHOLD;
    if !value.is_finite() {
        return 0.0;
    }
    if value.abs() <= THRESHOLD {
        return value;
    }
    let magnitude = value.abs();
    let bent = (THRESHOLD + RANGE * ((magnitude - THRESHOLD) / RANGE).tanh()).min(0.999_999);
    bent.copysign(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    use opencap_core::AudioKind;
    #[test]
    fn soft_limiter_keeps_quiet_signals_linear_and_bounds_loud_ones() {
        for value in [-0.6f32, -0.2, 0.0, 0.4, 0.85] {
            assert!(
                (soft_limit(value) - value).abs() < 1e-6,
                "linear region moved {value}"
            );
        }
        assert!(soft_limit(1.0) < 1.0);
        assert!(soft_limit(4.0) < 1.0);
        assert!(soft_limit(4.0) > 0.95);
        assert!(soft_limit(f32::NAN) == 0.0);
        assert!(soft_limit(-2.0).is_sign_negative());
    }

    #[test]
    fn summed_routes_never_hard_clip() {
        let mut m = AudioMixer::new();
        for kind in [AudioKind::System, AudioKind::Microphone] {
            m.push(AudioChunk {
                kind,
                pts_us: 0,
                samples: vec![1.0; 4],
            });
        }
        let output = m.take(2);
        for frame in output {
            for channel in frame {
                assert!(channel < 1.0 && channel > 0.9, "hard clipped to {channel}");
            }
        }
    }
    #[test]
    fn muting_clears_queued_route_without_interrupting_other_audio() {
        let mut m = AudioMixer::new();
        for (kind, level) in [(AudioKind::System, 0.2), (AudioKind::Microphone, 0.6)] {
            m.push(AudioChunk {
                kind,
                pts_us: 0,
                samples: vec![level; 8],
            });
        }
        assert_eq!(m.take(1), vec![[0.8; 2]]);
        m.clear_route(AudioKind::Microphone);
        assert_eq!(m.take(2), vec![[0.2; 2]; 2]);
        m.clear_route(AudioKind::System);
        assert_eq!(m.take(1), vec![[0.0; 2]]);
    }
    #[test]
    fn aligns_routes_fills_silence_and_limits_clipping() {
        let mut m = AudioMixer::new();
        for kind in [AudioKind::System, AudioKind::Microphone] {
            m.push(AudioChunk {
                kind,
                pts_us: 1000,
                samples: vec![0.8; 4],
            });
        }
        let result = m.take(50);
        assert_eq!(result[0], [0.0; 2]);
        assert!(
            result[48][0] > 0.99 && result[48][0] < 1.0,
            "{}",
            result[48][0]
        );
        m.push(AudioChunk {
            kind: AudioKind::System,
            pts_us: 0,
            samples: vec![1.0; 4],
        });
        assert_eq!(m.take(1), vec![[0.0; 2]]);
    }
}
