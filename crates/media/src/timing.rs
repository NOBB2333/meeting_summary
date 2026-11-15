//! Fixed-memory stage timing summaries. Percentiles are bucket upper bounds.
use std::time::Duration;

const LIMITS_US: [u64; 15] = [
    100,
    250,
    500,
    1_000,
    2_000,
    4_000,
    8_000,
    16_000,
    33_000,
    66_000,
    125_000,
    250_000,
    500_000,
    1_000_000,
    u64::MAX,
];

#[derive(Default)]
pub(crate) struct TimingHistogram {
    buckets: [u64; LIMITS_US.len()],
    count: u64,
    sum_us: u128,
    max_us: u64,
}
impl TimingHistogram {
    pub fn record(&mut self, elapsed: Duration) {
        let us = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.count = self.count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(u128::from(us));
        self.max_us = self.max_us.max(us);
        let bucket = LIMITS_US
            .partition_point(|limit| *limit < us)
            .min(LIMITS_US.len() - 1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
    pub fn take(&mut self) -> serde_json::Value {
        let previous = std::mem::take(self);
        if previous.count == 0 {
            return serde_json::Value::Null;
        }
        let target = previous.count.saturating_mul(95).saturating_add(99) / 100;
        let mut cumulative = 0u64;
        let mut p95 = previous.max_us;
        for (index, count) in previous.buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= target {
                p95 = LIMITS_US[index].min(previous.max_us);
                break;
            }
        }
        serde_json::json!({"count":previous.count, "mean_ms": previous.sum_us as f64 / previous.count as f64 / 1000.,
            "p95_upper_ms":p95 as f64 / 1000., "max_ms":previous.max_us as f64 / 1000.})
    }
}
