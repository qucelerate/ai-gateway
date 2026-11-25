//! A latency histogram with bounded memory: log-linear buckets over
//! microseconds (64 sub-buckets per power of two, so a recorded value is
//! within 1.6% of the true one), and cheap to record into from
//! many tasks through a mutex.

use std::time::Duration;

const SUB_BUCKETS: u64 = 64;
const SUB_BITS: u32 = 6;
/// Values up to 2^40 µs (about 12 days) are kept exactly to a bucket.
const MAX_EXPONENT: u32 = 40;

#[derive(Clone, Debug)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            counts: vec![0; ((MAX_EXPONENT + 1) as u64 * SUB_BUCKETS) as usize],
            total: 0,
            max: 0,
            sum: 0,
        }
    }
}

/// The bucket of `value`: values below 64 have a bucket each; above that,
/// each power of two is split into 64 equal parts.
fn index(value: u64) -> usize {
    if value < SUB_BUCKETS {
        return value as usize;
    }
    let exponent = 63 - value.leading_zeros(); // >= SUB_BITS
    let shift = exponent - SUB_BITS;
    let sub = (value >> shift) - SUB_BUCKETS; // 0..64
    let bucket = (exponent - SUB_BITS + 1) as u64 * SUB_BUCKETS + sub;
    bucket.min(((MAX_EXPONENT + 1) as u64 * SUB_BUCKETS) - 1) as usize
}

/// The highest value that falls in bucket `bucket`.
fn upper(bucket: usize) -> u64 {
    let bucket = bucket as u64;
    if bucket < SUB_BUCKETS {
        return bucket;
    }
    let group = bucket / SUB_BUCKETS; // >= 1
    let sub = bucket % SUB_BUCKETS;
    let shift = group - 1;
    ((SUB_BUCKETS + sub + 1) << shift) - 1
}

impl Histogram {
    pub fn record(&mut self, value: Duration) {
        let micros = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        self.counts[index(micros)] += 1;
        self.total += 1;
        self.max = self.max.max(micros);
        self.sum += u128::from(micros);
    }

    pub fn count(&self) -> u64 {
        self.total
    }

    /// The value at `percent` (0 to 100), as the bucket's upper bound
    /// (never above the largest value recorded).
    pub fn percentile(&self, percent: f64) -> Duration {
        if self.total == 0 {
            return Duration::ZERO;
        }
        let rank = ((percent / 100.0) * self.total as f64).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (bucket, count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return Duration::from_micros(upper(bucket).min(self.max));
            }
        }
        Duration::from_micros(self.max)
    }

    pub fn max(&self) -> Duration {
        Duration::from_micros(self.max)
    }

    pub fn mean(&self) -> Duration {
        if self.total == 0 {
            return Duration::ZERO;
        }
        Duration::from_micros((self.sum / u128::from(self.total)) as u64)
    }

    /// p50, p90, p95, p99, p99.9 and max, in milliseconds, for a report.
    pub fn summary(&self) -> serde_json::Value {
        let ms = |d: Duration| (d.as_secs_f64() * 1e4).round() / 10.0;
        serde_json::json!({
            "count": self.total,
            "mean_ms": ms(self.mean()),
            "p50_ms": ms(self.percentile(50.0)),
            "p90_ms": ms(self.percentile(90.0)),
            "p95_ms": ms(self.percentile(95.0)),
            "p99_ms": ms(self.percentile(99.0)),
            "p999_ms": ms(self.percentile(99.9)),
            "max_ms": ms(self.max()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_ordered_and_contain_their_values() {
        let mut last = 0;
        for value in (0..5_000u64).chain([1 << 20, (1 << 20) + 12_345, 1 << 39]) {
            let bucket = index(value);
            assert!(bucket >= last, "{value}: buckets out of order");
            last = bucket;
            assert!(upper(bucket) >= value, "{value} above its bucket");
            // Within 1/64 of the value.
            assert!(upper(bucket) - value <= value / 64 + 1, "{value}");
        }
    }

    #[test]
    fn percentiles_are_within_the_bucket_precision() {
        let mut histogram = Histogram::default();
        for ms in 1..=1_000u64 {
            histogram.record(Duration::from_millis(ms));
        }
        let p95 = histogram.percentile(95.0).as_secs_f64() * 1e3;
        assert!((950.0..=966.0).contains(&p95), "{p95}");
        assert_eq!(histogram.percentile(100.0), Duration::from_millis(1_000));
        assert_eq!(histogram.count(), 1_000);
        histogram.record(Duration::from_secs(5));
        assert_eq!(histogram.max(), Duration::from_secs(5));
        assert_eq!(histogram.count(), 1_001);
    }

    #[test]
    fn an_empty_histogram_reports_zero() {
        let histogram = Histogram::default();
        assert_eq!(histogram.percentile(99.0), Duration::ZERO);
        assert_eq!(histogram.mean(), Duration::ZERO);
    }
}
