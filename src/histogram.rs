use std::fmt;

// In terms of milliseconds
const BUCKETS_MILLIS: &[u64] = &[1000, 5_000, 10_000, 30_000, 100_000, u64::MAX];
const NUM_BUCKETS: usize = BUCKETS_MILLIS.len();

/// Really simple histogram for tracking allocation durations
/// Based on fixed buckets of <1s, <5s, <10s, <30s, <100s, longer
#[derive(Copy, Clone, Debug)]
pub struct MillisHistogram {
    counts: [u64; NUM_BUCKETS],
    sum: u64,
    count: u64, // Total number of events or allocations
}

impl Default for MillisHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl MillisHistogram {
    pub fn new() -> Self {
        Self {
            counts: [0; NUM_BUCKETS],
            sum: 0,
            count: 0,
        }
    }

    pub(crate) fn add_sample(&mut self, millis: u64) {
        self.count += 1;
        self.sum += millis;
        match BUCKETS_MILLIS.binary_search(&millis) {
            Ok(index) if index < NUM_BUCKETS => self.counts[index] += 1,
            Err(index) if index < NUM_BUCKETS => self.counts[index] += 1,
            _ => {}
        }
    }

    pub fn average_millis(&self) -> f64 {
        self.sum as f64 / self.count as f64
    }

    pub fn counts(&self) -> [u64; NUM_BUCKETS] {
        self.counts
    }

    // TODO: add percentile calculation
}

impl fmt::Display for MillisHistogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Histogram avg: {:.2}secs {{",
            self.average_millis() / 1000.0
        )?;
        for (bucket, count) in BUCKETS_MILLIS.iter().zip(self.counts) {
            write!(f, "{:.2}s: {}, ", *bucket as f64 / 1000.0, count)?;
        }
        write!(f, "}}")?;
        Ok(())
    }
}
