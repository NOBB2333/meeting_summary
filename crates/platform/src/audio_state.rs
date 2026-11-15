//! Route metadata is changed by workers; callbacks update only an atomic clock.
use opencap_core::{AudioKind, AudioRouteHealth, AudioRouteStatus};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct RouteTelemetry {
    status: Mutex<AudioRouteStatus>,
    // High 16 bits are the clock generation; low 48 bits are timestamp+1.
    // Zero means unknown. Out-of-range values remain unknown, never truncated.
    last_sample: AtomicU64,
}

impl RouteTelemetry {
    pub(crate) fn new(kind: AudioKind, requested: Option<String>) -> Self {
        Self {
            status: Mutex::new(AudioRouteStatus {
                kind,
                follow_default: requested.is_none(),
                requested_device_id: requested,
                resolved_device_id: None,
                resolved_device_name: None,
                health: AudioRouteHealth::Starting,
                last_sample_at_us: None,
                reason: None,
            }),
            last_sample: AtomicU64::new(0),
        }
    }

    pub(crate) fn starting(&self) {
        let mut status = self.status.lock();
        status.health = AudioRouteHealth::Starting;
        status.resolved_device_id = None;
        status.resolved_device_name = None;
        status.reason = None;
        self.last_sample.store(0, Ordering::Release);
    }

    pub(crate) fn opened(&self, id: String, name: String) {
        let mut status = self.status.lock();
        status.resolved_device_id = Some(id);
        status.resolved_device_name = Some(name);
        status.health = AudioRouteHealth::Open;
        status.reason = None;
    }

    pub(crate) fn waiting(&self, reason: String) {
        let mut status = self.status.lock();
        status.health = AudioRouteHealth::Waiting;
        status.resolved_device_id = None;
        status.resolved_device_name = None;
        status.reason = Some(reason);
        self.last_sample.store(0, Ordering::Release);
    }

    pub(crate) fn sample(&self, timestamp_us: i64, generation: u64) {
        const TIME_MASK: u64 = (1u64 << 48) - 1;
        let value = u64::try_from(timestamp_us)
            .ok()
            .and_then(|time| time.checked_add(1))
            .filter(|time| *time <= TIME_MASK && generation <= u16::MAX as u64)
            .map(|time| (generation << 48) | time);
        if let Some(value) = value {
            // Generations and timestamps are monotonic. A late callback must
            // not replace a newer sample with an earlier generation or time.
            self.last_sample.fetch_max(value, Ordering::AcqRel);
        } else {
            self.last_sample.store(0, Ordering::Release);
        }
    }

    pub(crate) fn clear_sample(&self) {
        self.last_sample.store(0, Ordering::Release);
    }

    pub(crate) fn snapshot(&self, generation: u64) -> AudioRouteStatus {
        // Read route state and its sample clock while holding the same lock
        // used by starting/waiting, so a disconnect cannot be paired with
        // the preceding route's open state and an already-cleared clock.
        let guard = self.status.lock();
        let mut status = guard.clone();
        let sample = self.last_sample.load(Ordering::Acquire);
        let time = sample & ((1u64 << 48) - 1);
        status.last_sample_at_us =
            (status.health == AudioRouteHealth::Open && time != 0 && sample >> 48 == generation)
                .then_some(time.saturating_sub(1) as i64);
        status
    }

    pub(crate) fn stopped_snapshot(&self, generation: u64) -> AudioRouteStatus {
        let mut status = self.snapshot(generation);
        status.health = AudioRouteHealth::Waiting;
        status.resolved_device_id = None;
        status.resolved_device_name = None;
        status.last_sample_at_us = None;
        status.reason = Some("采集线程已退出，等待重试".into());
        status
    }
}
