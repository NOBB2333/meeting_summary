use crate::RecordingClock;
use std::time::Instant;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioLevel {
    pub rms: f32,
    pub peak: f32,
}
impl AudioLevel {
    pub fn measure(samples: &[f32]) -> Self {
        let (mut sum, mut count, mut peak) = (0.0f64, 0usize, 0.0f32);
        for sample in samples.iter().copied().filter(|sample| sample.is_finite()) {
            let sample = sample.clamp(-1., 1.);
            sum += f64::from(sample) * f64::from(sample);
            count += 1;
            peak = peak.max(sample.abs());
        }
        Self {
            rms: if count == 0 {
                0.
            } else {
                (sum / count as f64).sqrt() as f32
            },
            peak,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioRouteLevels {
    pub input: AudioLevel,
    pub processed: AudioLevel,
}

/// Display-only meter. It never changes samples passed to the recorder.
/// RMS follows the short input window; the independent peak decays quickly.
#[derive(Debug, Clone, Default)]
pub struct AudioMeter {
    level: AudioLevel,
    updated: Option<Instant>,
}
impl AudioMeter {
    pub fn update(&mut self, samples: &[f32], now: Instant) {
        self.observe(AudioLevel::measure(samples), now);
    }
    pub fn observe(&mut self, level: AudioLevel, now: Instant) {
        let safe = |value: f32| {
            if value.is_finite() {
                value.clamp(0., 1.)
            } else {
                0.
            }
        };
        let rms = safe(level.rms);
        let peak = safe(level.peak);
        let dt = self.updated.map_or(0.04, |at| {
            now.saturating_duration_since(at).as_secs_f32().min(2.)
        });
        let release = (-dt / 0.35).exp();
        let attack = (-dt / 0.05).exp();
        self.level.rms = if rms > self.level.rms {
            rms + (self.level.rms - rms) * attack
        } else {
            rms + (self.level.rms - rms) * release
        };
        self.level.peak = (self.level.peak * release).max(peak);
        self.updated = Some(now);
    }
    pub fn level(&self, now: Instant) -> AudioLevel {
        let elapsed = self
            .updated
            .map_or(2., |at| now.saturating_duration_since(at).as_secs_f32());
        let decay = (-elapsed / 0.35).exp();
        let clear = |value: f32| if value < 0.001 { 0. } else { value };
        AudioLevel {
            rms: clear(self.level.rms * decay),
            peak: clear(self.level.peak * decay),
        }
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Keep packets contiguous in sample units when a device's callback timestamp
/// jitters. Real gaps, route restarts and pauses establish a fresh epoch.
#[derive(Default)]
pub struct AudioTimeline {
    next_sample: Option<i64>,
    generation: u64,
}
impl AudioTimeline {
    pub fn timestamp(
        &mut self,
        clock: &RecordingClock,
        latency_us: i64,
        frames: usize,
    ) -> Option<i64> {
        self.timestamp_with_generation(clock, latency_us, frames)
            .map(|(time, _)| time)
    }

    /// Return the clock generation from the same snapshot as the timestamp,
    /// so an asynchronous consumer can reject packets from an earlier epoch.
    pub fn timestamp_with_generation(
        &mut self,
        clock: &RecordingClock,
        latency_us: i64,
        frames: usize,
    ) -> Option<(i64, u64)> {
        let (now, paused, generation) = clock.snapshot();
        if paused {
            self.next_sample = None;
            return None;
        }
        let observed = (now - latency_us.clamp(0, 2_000_000)).max(0) * 48 / 1000;
        let first = match self.next_sample {
            Some(next) if generation == self.generation && (next - observed).abs() < 4_800 => next,
            _ => observed,
        };
        self.generation = generation;
        self.next_sample = Some(first + frames as i64);
        // Round upward so converting microseconds back to samples is lossless.
        Some(((first * 1000 + 47) / 48, generation))
    }
}

/// Preserve the part of a packet after the recording epoch. Clamping a negative
/// timestamp would shift the whole packet forward and overlap the next packet.
pub fn rebase_audio(chunk: &mut crate::AudioChunk, epoch_us: i64) {
    let relative = chunk.pts_us - epoch_us;
    if relative < 0 {
        let frames = ((-relative * 48 + 999) / 1000) as usize;
        chunk.samples.drain(..(frames * 2).min(chunk.samples.len()));
        chunk.pts_us = 0;
    } else {
        chunk.pts_us = relative;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_jitter_does_not_cut_or_overlap_packets() {
        let clock = RecordingClock::new();
        let mut timeline = AudioTimeline::default();
        let first = timeline.timestamp(&clock, 0, 480).unwrap();
        let second = timeline.timestamp(&clock, 0, 480).unwrap();
        assert_eq!(second - first, 10_000);
        clock.pause();
        assert!(timeline.timestamp(&clock, 0, 480).is_none());
        clock.resume();
        assert!(timeline.timestamp(&clock, 0, 480).unwrap() < second);
    }
    #[test]
    fn pre_epoch_packet_is_trimmed_instead_of_shifted() {
        let mut chunk = crate::AudioChunk {
            generation: 0,
            kind: crate::AudioKind::Microphone,
            pts_us: 0,
            samples: (0..192).map(|i| i as f32).collect(),
        };
        rebase_audio(&mut chunk, 1000);
        assert_eq!(chunk.pts_us, 0);
        assert_eq!(chunk.samples.len(), 96);
        assert_eq!(chunk.samples[0], 96.0);
    }
}
