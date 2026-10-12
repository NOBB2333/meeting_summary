use parking_lot::Mutex;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// All media timestamps share this clock. Pauses are removed, never encoded as gaps.
#[derive(Clone)]
pub struct RecordingClock(Arc<Mutex<ClockState>>);
struct ClockState {
    epoch: Instant,
    paused_at: Option<Instant>,
    paused: Duration,
    generation: u64,
}
impl Default for RecordingClock {
    fn default() -> Self {
        Self::new()
    }
}
impl RecordingClock {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(ClockState {
            epoch: Instant::now(),
            paused_at: None,
            paused: Duration::ZERO,
            generation: 0,
        })))
    }
    pub fn now_us(&self) -> i64 {
        self.at_us(Instant::now())
    }
    pub fn snapshot(&self) -> (i64, bool, u64) {
        let s = self.0.lock();
        let time = s
            .paused_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(s.epoch)
            .saturating_sub(s.paused)
            .as_micros() as i64;
        (time, s.paused_at.is_some(), s.generation)
    }
    pub fn at_us(&self, now: Instant) -> i64 {
        let s = self.0.lock();
        (s.paused_at
            .unwrap_or(now)
            .saturating_duration_since(s.epoch)
            .saturating_sub(s.paused))
        .as_micros() as i64
    }
    pub fn pause(&self) {
        self.pause_at(Instant::now());
    }
    fn pause_at(&self, now: Instant) {
        let mut s = self.0.lock();
        if s.paused_at.is_none() {
            s.paused_at = Some(now);
        }
    }
    pub fn resume(&self) {
        self.resume_at(Instant::now());
    }
    fn resume_at(&self, now: Instant) {
        let mut s = self.0.lock();
        if let Some(p) = s.paused_at.take() {
            s.paused += now.saturating_duration_since(p);
            s.generation += 1;
        }
    }
    pub fn is_paused(&self) -> bool {
        self.0.lock().paused_at.is_some()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_pause_and_resume_preserve_media_time() {
        let c = RecordingClock::new();
        let t = c.0.lock().epoch;
        c.pause_at(t + Duration::from_secs(2));
        c.pause_at(t + Duration::from_secs(3));
        assert_eq!(c.at_us(t + Duration::from_secs(9)), 2_000_000);
        c.resume_at(t + Duration::from_secs(10));
        c.resume_at(t + Duration::from_secs(11));
        assert_eq!(c.at_us(t + Duration::from_secs(13)), 5_000_000);
    }
}
