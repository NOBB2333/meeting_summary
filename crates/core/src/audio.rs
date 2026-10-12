use crate::RecordingClock;

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
        Some((first * 1000 + 47) / 48)
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
