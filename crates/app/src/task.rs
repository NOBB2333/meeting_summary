use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct TaskClock {
    started: Option<Instant>,
    elapsed: Duration,
    progress_base: Option<(f64, Instant)>,
    progress_latest: Option<(f64, Instant)>,
}
impl TaskClock {
    pub fn start(&mut self) {
        *self = Self {
            started: Some(Instant::now()),
            ..Default::default()
        };
    }
    pub fn finish(&mut self) {
        if let Some(start) = self.started.take() {
            self.elapsed = start.elapsed();
        }
    }
    pub fn elapsed(&self) -> Duration {
        self.started.map_or(self.elapsed, |s| s.elapsed())
    }
    pub fn has_run(&self) -> bool {
        self.started.is_some() || !self.elapsed.is_zero()
    }
    pub fn progress(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        let value = value.clamp(0., 1.);
        let now = Instant::now();
        if let Some((previous, _)) = self.progress_latest {
            if value < previous {
                // A new phase may restart at zero; don't carry its old speed over.
                self.progress_base = None;
            } else if value == previous {
                // Repeated progress events do not mean the task advanced.
                return;
            }
        }
        if value > 0. && self.progress_base.is_none() {
            self.progress_base = Some((value, now));
        }
        self.progress_latest = Some((value, now));
    }
    pub fn stalled(&self) -> bool {
        self.started.is_some()
            && self
                .progress_latest
                .is_some_and(|(value, updated)| value < 1. && updated.elapsed().as_secs() > 15)
    }
    /// Exclude setup/download time; require measured advancement before showing an estimate.
    pub fn remaining(&self) -> Option<Duration> {
        self.started?;
        let (first, began) = self.progress_base?;
        let (last, updated) = self.progress_latest?;
        let seconds = updated.duration_since(began).as_secs_f64();
        if last >= 1. || last - first < 0.03 || seconds < 3. || self.stalled() {
            return None;
        }
        Some(Duration::from_secs_f64(
            (seconds * (1. - last) / (last - first)).min(86_400.),
        ))
    }
}

pub(super) fn duration_text(duration: Duration) -> String {
    let s = duration.as_secs();
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        format!("{} 分 {:02} 秒", s / 60, s % 60)
    } else {
        format!("{} 小时 {:02} 分", s / 3600, s / 60 % 60)
    }
}

pub(super) fn saved_duration(seconds: Option<f64>) -> Duration {
    Duration::from_secs_f64(
        seconds
            .filter(|value| value.is_finite())
            .unwrap_or(0.)
            .clamp(0., 86_400.),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_or_stalled_progress_has_no_fake_estimate() {
        let mut clock = TaskClock::default();
        clock.start();
        assert!(clock.remaining().is_none());
        clock.progress(f64::NAN);
        assert!(clock.remaining().is_none());
        clock.progress(0.1);
        assert!(clock.remaining().is_none());
        clock.finish();
        assert!(clock.remaining().is_none());
    }
    #[test]
    fn estimate_uses_measured_advancement_and_excludes_setup() {
        let now = Instant::now();
        let clock = TaskClock {
            started: Some(now - Duration::from_secs(120)),
            progress_base: Some((0.1, now - Duration::from_secs(10))),
            progress_latest: Some((0.6, now)),
            elapsed: Duration::ZERO,
        };
        assert_eq!(clock.remaining().unwrap().as_secs(), 8);
    }
    #[test]
    fn repeated_values_do_not_hide_a_stall_and_new_phases_reset_the_estimate() {
        let now = Instant::now();
        let mut clock = TaskClock {
            started: Some(now - Duration::from_secs(30)),
            progress_base: Some((0.1, now - Duration::from_secs(25))),
            progress_latest: Some((0.6, now - Duration::from_secs(20))),
            elapsed: Duration::ZERO,
        };
        clock.progress(0.6);
        assert!(clock.stalled());
        assert!(clock.remaining().is_none());
        clock.progress(0.2);
        assert!(!clock.stalled());
        assert!(clock.remaining().is_none());
        assert_eq!(clock.progress_base.unwrap().0, 0.2);
    }
}
