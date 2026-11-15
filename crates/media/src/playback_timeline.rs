//! Shared media-time primitives for playback. These do not decode or play audio.
use anyhow::{Result, ensure};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedSegment {
    pub id: u64,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// A static interval index; overlapping speakers can yield multiple active
/// rows. Gaps yield no active row rather than retaining the preceding one.
#[derive(Default)]
pub struct SegmentTimeline {
    entries: Vec<TimedSegment>,
    prefix_max_end: Vec<u64>,
}
impl SegmentTimeline {
    pub fn new(mut entries: Vec<TimedSegment>) -> Self {
        entries.retain(|entry| entry.start_ms < entry.end_ms);
        entries.sort_by_key(|entry| (entry.start_ms, entry.end_ms, entry.id));
        let mut end = 0;
        let prefix_max_end = entries
            .iter()
            .map(|entry| {
                end = end.max(entry.end_ms);
                end
            })
            .collect();
        Self {
            entries,
            prefix_max_end,
        }
    }
    pub fn active_at(&self, media_ms: u64) -> Vec<u64> {
        let end = self
            .entries
            .partition_point(|entry| entry.start_ms <= media_ms);
        let start = self.prefix_max_end[..end].partition_point(|max_end| *max_end <= media_ms);
        self.entries[start..end]
            .iter()
            .filter(|entry| entry.start_ms <= media_ms && media_ms < entry.end_ms)
            .map(|entry| entry.id)
            .collect()
    }
    pub fn selected_range(&self, ids: &[u64]) -> Option<(u64, u64)> {
        if ids.is_empty() {
            return None;
        }
        let selected: std::collections::BTreeSet<_> = ids.iter().copied().collect();
        let indices: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| selected.contains(&entry.id).then_some(index))
            .collect();
        if indices.len() != selected.len() {
            return None;
        }
        let first = *indices.first()?;
        let last = *indices.last()?;
        if last - first + 1 != indices.len() {
            return None;
        }
        Some((
            self.entries[first..=last]
                .iter()
                .map(|entry| entry.start_ms)
                .min()?,
            self.entries[first..=last]
                .iter()
                .map(|entry| entry.end_ms)
                .max()?,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackRange {
    pub start_ms: u64,
    pub end_ms: u64,
    pub repeat: bool,
}
impl PlaybackRange {
    pub fn new(start_ms: u64, end_ms: u64, repeat: bool) -> Result<Self> {
        ensure!(start_ms < end_ms, "播放范围必须有正长度");
        Ok(Self {
            start_ms,
            end_ms,
            repeat,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ScheduledAudio {
    start: Instant,
    output_duration: Duration,
    media_start_us: u64,
    media_duration_us: u64,
}

/// Tracks when actual PCM is expected to reach the output device, never when
/// it is decoded or queued. Callbacks supply the device's predicted playback
/// instant; underrun silence must not be submitted to this clock.
pub struct PresentationClock {
    generation: u64,
    played_us: u64,
    scheduled: VecDeque<ScheduledAudio>,
}
impl PresentationClock {
    pub fn new() -> Self {
        Self {
            generation: 0,
            played_us: 0,
            scheduled: VecDeque::new(),
        }
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Seek, output reset or close must also flush/recreate the device stream.
    pub fn reset(&mut self, media_ms: u64) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.scheduled.clear();
        self.played_us = media_ms.saturating_mul(1000);
        self.generation
    }
    pub fn schedule(
        &mut self,
        generation: u64,
        playback_at: Instant,
        media_start_us: u64,
        media_duration_us: u64,
        output_frames: u64,
        output_rate: u32,
    ) -> Result<bool> {
        if generation != self.generation {
            return Ok(false);
        }
        ensure!(
            output_frames > 0 && output_rate > 0 && media_duration_us > 0,
            "无效播放缓冲"
        );
        if let Some(last) = self.scheduled.back() {
            ensure!(
                playback_at >= last.start,
                "输出设备时钟回退，需要重建播放流"
            );
        }
        ensure!(self.scheduled.len() < 2048, "播放时钟队列已满");
        self.scheduled.push_back(ScheduledAudio {
            start: playback_at,
            output_duration: Duration::from_secs_f64(output_frames as f64 / f64::from(output_rate)),
            media_start_us,
            media_duration_us,
        });
        Ok(true)
    }
    pub fn position_ms(&mut self, now: Instant) -> u64 {
        while self
            .scheduled
            .front()
            .is_some_and(|item| now >= item.start + item.output_duration)
        {
            if let Some(item) = self.scheduled.pop_front() {
                self.played_us = self
                    .played_us
                    .max(item.media_start_us.saturating_add(item.media_duration_us));
            }
        }
        if let Some(item) = self.scheduled.front()
            && now >= item.start
        {
            let fraction =
                now.duration_since(item.start).as_secs_f64() / item.output_duration.as_secs_f64();
            let position = item
                .media_start_us
                .saturating_add((item.media_duration_us as f64 * fraction.clamp(0., 1.)) as u64);
            // Device timestamp estimates and rounding can overlap across
            // adjacent buffers. Never move a visible position backwards in
            // the same generation; explicit seeks call reset() instead.
            self.played_us = self.played_us.max(position);
            return self.played_us / 1000;
        }
        self.played_us / 1000
    }
    pub fn has_pending(&mut self, now: Instant) -> bool {
        self.position_ms(now);
        !self.scheduled.is_empty()
    }
}
impl Default for PresentationClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_and_overlaps_select_every_active_row() {
        let timeline = SegmentTimeline::new(vec![
            TimedSegment {
                id: 1,
                start_ms: 0,
                end_ms: 1000,
            },
            TimedSegment {
                id: 2,
                start_ms: 500,
                end_ms: 1500,
            },
            TimedSegment {
                id: 3,
                start_ms: 2000,
                end_ms: 3000,
            },
        ]);
        assert_eq!(timeline.active_at(0), vec![1]);
        assert_eq!(timeline.active_at(750), vec![1, 2]);
        assert_eq!(timeline.active_at(1200), vec![2]);
        // A gap must not keep the preceding sentence active.
        assert!(timeline.active_at(1800).is_empty());
        assert_eq!(timeline.active_at(2000), vec![3]);
        assert!(timeline.active_at(3000).is_empty());
    }

    #[test]
    fn selected_range_requires_contiguous_rows() {
        let timeline = SegmentTimeline::new(vec![
            TimedSegment {
                id: 1,
                start_ms: 0,
                end_ms: 1000,
            },
            TimedSegment {
                id: 2,
                start_ms: 1000,
                end_ms: 2000,
            },
            TimedSegment {
                id: 3,
                start_ms: 3000,
                end_ms: 4000,
            },
        ]);
        assert_eq!(timeline.selected_range(&[1, 2]), Some((0, 2000)));
        assert_eq!(timeline.selected_range(&[1, 3]), None);
        assert_eq!(timeline.selected_range(&[]), None);
        assert_eq!(timeline.selected_range(&[9]), None);
    }

    #[test]
    fn position_comes_from_device_time_not_queued_bytes() {
        let mut clock = PresentationClock::new();
        let generation = clock.reset(0);
        let start = Instant::now();
        clock
            .schedule(generation, start, 0, 1_000_000, 48_000, 48_000)
            .unwrap();
        // Nothing has reached the output device yet, so decoding/queueing a
        // full second must not advance the visible position.
        assert_eq!(clock.position_ms(start), 0);
        // Half way through the scheduled buffer the estimate is ~500 ms, not
        // the full buffer length.
        assert_eq!(clock.position_ms(start + Duration::from_millis(500)), 500);
        // After the buffer finished playing it is counted once.
        assert_eq!(clock.position_ms(start + Duration::from_millis(1500)), 1000);
        assert!(!clock.has_pending(start + Duration::from_millis(1500)));
    }

    #[test]
    fn seek_starts_a_new_generation_and_ignores_late_buffers() {
        let mut clock = PresentationClock::new();
        let first = clock.reset(0);
        let start = Instant::now();
        clock
            .schedule(first, start, 0, 1_000_000, 48_000, 48_000)
            .unwrap();
        let second = clock.reset(40_000);
        assert_ne!(first, second);
        let late = clock.schedule(
            first,
            start + Duration::from_millis(10),
            1_000_000,
            1_000_000,
            48_000,
            48_000,
        );
        assert!(!late.unwrap(), "旧 generation 的缓冲不能进入新时间轴");
        // The seek target is authoritative until new audio is actually played.
        assert_eq!(clock.position_ms(start + Duration::from_millis(20)), 40_000);
        clock
            .schedule(
                second,
                start + Duration::from_millis(20),
                40_000_000,
                500_000,
                24_000,
                48_000,
            )
            .unwrap();
        assert_eq!(
            clock.position_ms(start + Duration::from_millis(270)),
            40_250
        );
    }

    #[test]
    fn stale_overlapping_buffer_estimates_never_move_position_backwards() {
        let mut clock = PresentationClock::new();
        let generation = clock.reset(0);
        let start = Instant::now();
        clock
            .schedule(generation, start, 0, 1_000_000, 48_000, 48_000)
            .unwrap();
        clock
            .schedule(generation, start, 0, 1_000_000, 48_000, 48_000)
            .unwrap();
        let at = start + Duration::from_millis(800);
        assert_eq!(clock.position_ms(at), 800);
        assert_eq!(clock.position_ms(at), 800);
    }

    #[test]
    fn invalid_or_backwards_device_schedule_is_rejected() {
        let mut clock = PresentationClock::new();
        let generation = clock.reset(0);
        let start = Instant::now();
        assert!(clock.schedule(generation, start, 0, 0, 0, 48_000).is_err());
        clock
            .schedule(generation, start, 0, 100_000, 4_800, 48_000)
            .unwrap();
        assert!(
            clock
                .schedule(
                    generation,
                    start - Duration::from_millis(1),
                    100_000,
                    100_000,
                    4_800,
                    48_000
                )
                .is_err()
        );
    }
}
