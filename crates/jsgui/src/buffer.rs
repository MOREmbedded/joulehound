//! Rolling display buffer.
//!
//! At 1 Msps a few seconds of data is tens of millions of samples, far more
//! than a plot a thousand pixels wide can show. Sending all of it to the GUI
//! would cost more than the acquisition itself.
//!
//! Instead the acquisition thread folds samples into fixed-duration buckets as
//! they arrive, keeping the minimum, maximum and mean of each. The plot draws
//! the min/max envelope, so a narrow spike is still visible even when a bucket
//! covers thousands of samples — which is exactly what a plain decimated plot
//! would hide.

/// One bucket of aggregated samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    /// Seconds since capture start, at the bucket's centre.
    pub t: f64,
    /// Smallest sample in the bucket.
    pub min: f32,
    /// Largest sample in the bucket.
    pub max: f32,
    /// Mean of the samples in the bucket.
    pub mean: f32,
}

impl Bucket {
    /// True when the bucket holds no real samples.
    pub fn is_empty(&self) -> bool {
        self.min.is_nan() && self.max.is_nan()
    }
}

/// Aggregate of every bucket between two markers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RangeStats {
    /// Smallest sample in the span.
    pub min: f32,
    /// Largest sample in the span.
    pub max: f32,
    /// Mean of the bucket means in the span.
    pub mean: f32,
    /// Number of buckets contributing.
    pub buckets: usize,
}

/// A fixed-capacity ring of buckets for one signal.
#[derive(Debug)]
pub struct SignalTrace {
    buckets: Vec<Bucket>,
    /// Index of the oldest bucket.
    head: usize,
    len: usize,
    /// Accumulator for the bucket currently being filled.
    acc_min: f32,
    acc_max: f32,
    acc_sum: f64,
    acc_count: u64,
    acc_index: Option<u64>,
}

impl SignalTrace {
    /// Create a trace holding at most `capacity` buckets.
    pub fn new(capacity: usize) -> Self {
        Self {
            buckets: Vec::with_capacity(capacity.max(1)),
            head: 0,
            len: 0,
            acc_min: f32::INFINITY,
            acc_max: f32::NEG_INFINITY,
            acc_sum: 0.0,
            acc_count: 0,
            acc_index: None,
        }
    }

    /// Maximum number of buckets retained.
    pub fn capacity(&self) -> usize {
        self.buckets.capacity().max(1)
    }

    /// Number of buckets currently held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drop everything, including any partially filled bucket.
    pub fn clear(&mut self) {
        self.buckets.clear();
        self.head = 0;
        self.len = 0;
        self.reset_accumulator();
    }

    fn reset_accumulator(&mut self) {
        self.acc_min = f32::INFINITY;
        self.acc_max = f32::NEG_INFINITY;
        self.acc_sum = 0.0;
        self.acc_count = 0;
        self.acc_index = None;
    }

    fn push_bucket(&mut self, bucket: Bucket) {
        let cap = self.capacity();
        if self.buckets.len() < cap {
            self.buckets.push(bucket);
            self.len += 1;
        } else {
            // Overwrite the oldest and advance the window.
            let slot = (self.head + self.len) % cap;
            self.buckets[slot] = bucket;
            if self.len < cap {
                self.len += 1;
            } else {
                self.head = (self.head + 1) % cap;
            }
        }
    }

    fn flush(&mut self, bucket_seconds: f64) {
        let Some(index) = self.acc_index else {
            return;
        };
        if self.acc_count == 0 {
            self.reset_accumulator();
            return;
        }
        let bucket = Bucket {
            t: (index as f64 + 0.5) * bucket_seconds,
            min: self.acc_min,
            max: self.acc_max,
            mean: (self.acc_sum / self.acc_count as f64) as f32,
        };
        self.push_bucket(bucket);
        self.reset_accumulator();
    }

    /// Fold `samples` into the trace.
    ///
    /// `first_index` is the sample index of `samples[0]` counted from capture
    /// start, and `sample_period` is the spacing in seconds. NaN samples, which
    /// the aligner inserts to hold a gap open, are counted as present but
    /// excluded from the statistics so they do not poison a bucket's extremes.
    pub fn extend(
        &mut self,
        samples: &[f32],
        first_index: u64,
        sample_period: f64,
        bucket_seconds: f64,
    ) {
        if bucket_seconds <= 0.0 || sample_period <= 0.0 {
            return;
        }
        let per_bucket = (bucket_seconds / sample_period).max(1.0) as u64;

        for (i, &value) in samples.iter().enumerate() {
            let index = first_index + i as u64;
            let bucket_index = index / per_bucket;

            match self.acc_index {
                Some(current) if current == bucket_index => {}
                Some(_) => {
                    self.flush(bucket_seconds);
                    self.acc_index = Some(bucket_index);
                }
                None => self.acc_index = Some(bucket_index),
            }

            if value.is_finite() {
                self.acc_min = self.acc_min.min(value);
                self.acc_max = self.acc_max.max(value);
                self.acc_sum += value as f64;
                self.acc_count += 1;
            }
        }
    }

    /// Finish the in-progress bucket so it becomes visible.
    pub fn finish(&mut self, bucket_seconds: f64) {
        self.flush(bucket_seconds);
    }

    /// Iterate buckets from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = Bucket> + '_ {
        let cap = self.capacity();
        (0..self.len).map(move |i| self.buckets[(self.head + i) % cap])
    }

    /// The most recent bucket, if any.
    pub fn last(&self) -> Option<Bucket> {
        if self.len == 0 {
            return None;
        }
        let cap = self.capacity();
        Some(self.buckets[(self.head + self.len - 1) % cap])
    }

    /// Time span covered, in seconds.
    /// The bucket nearest `t`, if the trace covers it.
    ///
    /// Markers land wherever the pointer was, which will rarely be exactly on
    /// a bucket centre, so the nearest one is what the reader means.
    pub fn value_at(&self, t: f64) -> Option<Bucket> {
        let mut best: Option<(f64, Bucket)> = None;
        for b in self.iter() {
            if b.is_empty() {
                continue;
            }
            let d = (b.t - t).abs();
            match best {
                Some((bd, _)) if bd <= d => {}
                _ => best = Some((d, b)),
            }
        }
        best.map(|(_, b)| b)
    }

    /// Aggregate every bucket between `t0` and `t1`, in either order.
    ///
    /// This is what a pair of markers measures: the extremes are the true
    /// extremes of the span, taken from the per-bucket minima and maxima
    /// rather than from the means, so a spike inside the span is reported.
    pub fn range_stats(&self, t0: f64, t1: f64) -> Option<RangeStats> {
        let (lo, hi) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        let mut stats = RangeStats {
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            mean: 0.0,
            buckets: 0,
        };
        let mut sum = 0.0f64;
        for b in self.iter() {
            if b.is_empty() || b.t < lo || b.t > hi {
                continue;
            }
            stats.min = stats.min.min(b.min);
            stats.max = stats.max.max(b.max);
            sum += b.mean as f64;
            stats.buckets += 1;
        }
        if stats.buckets == 0 {
            return None;
        }
        stats.mean = (sum / stats.buckets as f64) as f32;
        Some(stats)
    }

    /// Time span covered, in seconds.
    pub fn time_span(&self) -> Option<(f64, f64)> {
        let first = self.iter().next()?;
        let last = self.last()?;
        Some((first.t, last.t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: f64 = 1e-6; // 1 Msps
    const BUCKET: f64 = 1e-3; // 1 ms per bucket => 1000 samples

    #[test]
    fn a_new_trace_is_empty() {
        let t = SignalTrace::new(10);
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert!(t.last().is_none());
        assert!(t.time_span().is_none());
    }

    #[test]
    fn a_full_bucket_records_min_max_and_mean() {
        let mut t = SignalTrace::new(10);
        let samples: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        t.extend(&samples, 0, PERIOD, BUCKET);
        t.finish(BUCKET);
        let b = t.last().unwrap();
        assert_eq!(b.min, 0.0);
        assert_eq!(b.max, 999.0);
        assert!((b.mean - 499.5).abs() < 0.01, "mean {}", b.mean);
    }

    #[test]
    fn bucket_time_is_its_centre() {
        let mut t = SignalTrace::new(10);
        t.extend(&[1.0; 1000], 0, PERIOD, BUCKET);
        t.finish(BUCKET);
        let b = t.last().unwrap();
        assert!((b.t - BUCKET / 2.0).abs() < 1e-9, "t = {}", b.t);
    }

    #[test]
    fn samples_split_across_bucket_boundaries() {
        let mut t = SignalTrace::new(10);
        // 2500 samples at 1000 per bucket: two complete buckets, one partial.
        let samples: Vec<f32> = (0..2500).map(|i| i as f32).collect();
        t.extend(&samples, 0, PERIOD, BUCKET);
        assert_eq!(t.len(), 2, "only complete buckets are visible");
        t.finish(BUCKET);
        assert_eq!(t.len(), 3, "finish flushes the partial bucket");
    }

    #[test]
    fn buckets_continue_across_separate_calls() {
        let mut t = SignalTrace::new(10);
        t.extend(&[1.0; 400], 0, PERIOD, BUCKET);
        t.extend(&[3.0; 600], 400, PERIOD, BUCKET);
        t.finish(BUCKET);
        assert_eq!(t.len(), 1, "one bucket spanning both calls");
        let b = t.last().unwrap();
        assert_eq!(b.min, 1.0);
        assert_eq!(b.max, 3.0);
        assert!((b.mean - (1.0 * 0.4 + 3.0 * 0.6)).abs() < 0.01);
    }

    #[test]
    fn a_spike_survives_aggregation() {
        // The whole point of min/max: one extreme sample among a thousand must
        // still be visible in the plot.
        let mut t = SignalTrace::new(10);
        let mut samples = vec![0.0f32; 1000];
        samples[500] = 42.0;
        t.extend(&samples, 0, PERIOD, BUCKET);
        t.finish(BUCKET);
        assert_eq!(t.last().unwrap().max, 42.0);
    }

    #[test]
    fn nan_samples_do_not_poison_the_statistics() {
        let mut t = SignalTrace::new(10);
        let mut samples = vec![1.0f32; 1000];
        samples[10] = f32::NAN;
        t.extend(&samples, 0, PERIOD, BUCKET);
        t.finish(BUCKET);
        let b = t.last().unwrap();
        assert_eq!(b.min, 1.0);
        assert_eq!(b.max, 1.0);
        assert!((b.mean - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_bucket_of_only_nan_is_marked_empty() {
        let mut t = SignalTrace::new(10);
        t.extend(&[f32::NAN; 1000], 0, PERIOD, BUCKET);
        t.finish(BUCKET);
        // The accumulator saw no finite sample, so nothing was recorded.
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn the_ring_discards_the_oldest_buckets() {
        let mut t = SignalTrace::new(4);
        for i in 0..6u64 {
            t.extend(&[i as f32; 1000], i * 1000, PERIOD, BUCKET);
        }
        t.finish(BUCKET);
        assert_eq!(t.len(), 4, "capacity is respected");
        let values: Vec<f32> = t.iter().map(|b| b.mean).collect();
        assert_eq!(values, vec![2.0, 3.0, 4.0, 5.0], "oldest dropped first");
    }

    #[test]
    fn iteration_runs_oldest_to_newest_after_wrapping() {
        let mut t = SignalTrace::new(3);
        for i in 0..5u64 {
            t.extend(&[i as f32; 1000], i * 1000, PERIOD, BUCKET);
        }
        t.finish(BUCKET);
        let times: Vec<f64> = t.iter().map(|b| b.t).collect();
        assert!(
            times.windows(2).all(|w| w[1] > w[0]),
            "times must increase: {times:?}"
        );
    }

    #[test]
    fn time_span_covers_oldest_to_newest() {
        let mut t = SignalTrace::new(10);
        for i in 0..3u64 {
            t.extend(&[1.0; 1000], i * 1000, PERIOD, BUCKET);
        }
        t.finish(BUCKET);
        let (first, last) = t.time_span().unwrap();
        assert!(first < last);
        assert!((last - first - 2.0 * BUCKET).abs() < 1e-9);
    }

    #[test]
    fn value_at_finds_the_nearest_bucket() {
        let mut t = SignalTrace::new(10);
        for i in 0..5u64 {
            t.extend(&[i as f32; 1000], i * 1000, PERIOD, BUCKET);
        }
        t.finish(BUCKET);
        // Bucket centres are at 0.5, 1.5, 2.5, 3.5, 4.5 ms.
        assert_eq!(t.value_at(0.0).unwrap().mean, 0.0);
        assert_eq!(t.value_at(0.0026).unwrap().mean, 2.0);
        assert_eq!(t.value_at(99.0).unwrap().mean, 4.0, "clamps to the last");
    }

    #[test]
    fn value_at_on_an_empty_trace_is_none() {
        assert!(SignalTrace::new(4).value_at(0.0).is_none());
    }

    #[test]
    fn range_stats_aggregate_the_span() {
        let mut t = SignalTrace::new(10);
        for i in 0..5u64 {
            t.extend(&[i as f32; 1000], i * 1000, PERIOD, BUCKET);
        }
        t.finish(BUCKET);
        // Buckets 1..=3 have means 1, 2, 3.
        let r = t.range_stats(0.0011, 0.0039).unwrap();
        assert_eq!(r.buckets, 3);
        assert_eq!(r.min, 1.0);
        assert_eq!(r.max, 3.0);
        assert!((r.mean - 2.0).abs() < 1e-6);
    }

    #[test]
    fn range_stats_accept_markers_in_either_order() {
        let mut t = SignalTrace::new(10);
        for i in 0..5u64 {
            t.extend(&[i as f32; 1000], i * 1000, PERIOD, BUCKET);
        }
        t.finish(BUCKET);
        let a = t.range_stats(0.0011, 0.0039).unwrap();
        let b = t.range_stats(0.0039, 0.0011).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn range_stats_report_true_extremes_not_bucket_means() {
        // A spike inside one bucket must survive into the marker readout.
        let mut t = SignalTrace::new(10);
        let mut samples = vec![0.0f32; 1000];
        samples[7] = 99.0;
        t.extend(&samples, 0, PERIOD, BUCKET);
        t.finish(BUCKET);
        let r = t.range_stats(0.0, 0.001).unwrap();
        assert_eq!(r.max, 99.0, "extremes come from bucket min/max");
    }

    #[test]
    fn range_stats_outside_the_data_are_none() {
        let mut t = SignalTrace::new(10);
        t.extend(&[1.0; 1000], 0, PERIOD, BUCKET);
        t.finish(BUCKET);
        assert!(t.range_stats(5.0, 6.0).is_none());
    }

    #[test]
    fn clear_resets_everything() {
        let mut t = SignalTrace::new(10);
        t.extend(&[1.0; 1500], 0, PERIOD, BUCKET);
        t.clear();
        assert!(t.is_empty());
        t.finish(BUCKET);
        assert!(t.is_empty(), "the partial bucket was discarded too");
    }

    #[test]
    fn degenerate_timing_is_ignored_rather_than_dividing_by_zero() {
        let mut t = SignalTrace::new(10);
        t.extend(&[1.0; 100], 0, 0.0, BUCKET);
        t.extend(&[1.0; 100], 0, PERIOD, 0.0);
        assert!(t.is_empty());
    }
}
