//! Raw sample retention, aggregated per view.
//!
//! Folding samples into fixed buckets as they arrive makes a plot cheap but
//! permanently coarse: zooming in cannot reveal detail that was averaged away
//! at capture time, so a 1 Msps signal reads as a solid block however far you
//! zoom. Keeping the recent samples and aggregating them to the *current*
//! view instead means zooming in shows the real waveform, down to individual
//! samples, which is the whole point of a 1 Msps instrument.
//!
//! Retention is bounded in samples, not seconds, so the memory cost is fixed
//! regardless of sample rate. Anything older than the raw window is still
//! drawn from the coarse [`crate::buffer::SignalTrace`].

use crate::buffer::{Bucket, RangeStats};

/// A ring of raw samples with their absolute sample indices.
#[derive(Debug)]
pub struct RawTrace {
    data: Vec<f32>,
    /// Index into `data` of the oldest sample.
    head: usize,
    len: usize,
    capacity: usize,
    /// Absolute sample index of the oldest retained sample.
    first_index: u64,
    /// Seconds between consecutive samples.
    sample_period: f64,
}

impl RawTrace {
    /// Retain up to `capacity` samples.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            data: vec![0.0; capacity],
            head: 0,
            len: 0,
            capacity,
            first_index: 0,
            sample_period: 1e-6,
        }
    }

    /// Set the spacing between samples and discard what was retained, since
    /// the existing samples no longer describe the same timeline.
    pub fn reconfigure(&mut self, capacity: usize, sample_period: f64) {
        let capacity = capacity.max(1);
        if capacity != self.capacity {
            self.data = vec![0.0; capacity];
            self.capacity = capacity;
        }
        self.sample_period = if sample_period > 0.0 {
            sample_period
        } else {
            1e-6
        };
        self.clear();
    }

    /// Drop every retained sample.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.first_index = 0;
    }

    /// Number of samples retained.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when nothing is retained.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Seconds between consecutive samples.
    pub fn sample_period(&self) -> f64 {
        self.sample_period
    }

    /// Absolute sample index just past the newest retained sample.
    fn end_index(&self) -> u64 {
        self.first_index + self.len as u64
    }

    /// Append `samples`, whose first element has absolute index `first_index`.
    ///
    /// A gap or a jump backwards means the timeline restarted, so the ring is
    /// cleared rather than splicing unrelated samples together.
    pub fn push(&mut self, samples: &[f32], first_index: u64) {
        if samples.is_empty() {
            return;
        }
        if self.len == 0 {
            self.first_index = first_index;
        } else if first_index != self.end_index() {
            self.clear();
            self.first_index = first_index;
        }

        for &v in samples {
            let slot = (self.head + self.len) % self.capacity;
            self.data[slot] = v;
            if self.len < self.capacity {
                self.len += 1;
            } else {
                // Full: the write landed on the oldest sample, so the window
                // slides forward by one.
                self.head = (self.head + 1) % self.capacity;
                self.first_index += 1;
            }
        }
    }

    /// Sample at offset `i` from the oldest retained sample.
    fn at(&self, i: usize) -> f32 {
        self.data[(self.head + i) % self.capacity]
    }

    /// Time of the sample with absolute index `index`.
    fn time_of(&self, index: u64) -> f64 {
        index as f64 * self.sample_period
    }

    /// Time range covered, in seconds.
    pub fn time_span(&self) -> Option<(f64, f64)> {
        if self.len == 0 {
            return None;
        }
        Some((
            self.time_of(self.first_index),
            self.time_of(self.end_index() - 1),
        ))
    }

    /// Convert a time to an offset from the oldest retained sample, clamped to
    /// what is actually retained.
    fn offset_of_time(&self, t: f64) -> usize {
        if self.sample_period <= 0.0 {
            return 0;
        }
        let index = (t / self.sample_period).floor();
        if index <= self.first_index as f64 {
            return 0;
        }
        let offset = index - self.first_index as f64;
        (offset as usize).min(self.len.saturating_sub(1))
    }

    /// True when the retained samples cover the whole of `t0..=t1`.
    pub fn covers(&self, t0: f64, t1: f64) -> bool {
        match self.time_span() {
            Some((first, last)) => first <= t0 && last >= t1,
            None => false,
        }
    }

    /// Aggregate the samples in `t0..=t1` into at most `columns` buckets.
    ///
    /// Each bucket keeps the true minimum and maximum of the samples it
    /// covers, so a single-sample spike survives however far out the view is.
    /// When the range holds fewer samples than columns, one bucket is emitted
    /// per sample and the plot shows the actual waveform.
    pub fn aggregate(&self, t0: f64, t1: f64, columns: usize, out: &mut Vec<Bucket>) {
        out.clear();
        if self.len == 0 || columns == 0 {
            return;
        }
        let (lo, hi) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        // Offsets are clamped to what is retained, so a range entirely before
        // or after the window would otherwise collapse onto an end sample and
        // draw a phantom bucket there.
        let Some((first, last)) = self.time_span() else {
            return;
        };
        if hi < first || lo > last {
            return;
        }
        let start = self.offset_of_time(lo.max(first));
        let end = self.offset_of_time(hi.min(last));
        if end < start {
            return;
        }
        let count = end - start + 1;
        let columns = columns.min(count);
        if columns == 0 {
            return;
        }

        out.reserve(columns);
        for c in 0..columns {
            // Split the range evenly; integer arithmetic keeps the columns
            // contiguous with no gaps or overlaps.
            let a = start + count * c / columns;
            let b = start + count * (c + 1) / columns;
            let b = b.max(a + 1).min(start + count);

            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            let mut sum = 0.0f64;
            let mut n = 0u32;
            for i in a..b {
                let v = self.at(i);
                if !v.is_finite() {
                    continue;
                }
                min = min.min(v);
                max = max.max(v);
                sum += v as f64;
                n += 1;
            }
            if n == 0 {
                continue;
            }
            let mid = self.first_index + (a + b) as u64 / 2;
            out.push(Bucket {
                t: self.time_of(mid),
                min,
                max,
                mean: (sum / n as f64) as f32,
            });
        }
    }

    /// The sample nearest `t`, if it is retained.
    pub fn value_at(&self, t: f64) -> Option<f32> {
        let (first, last) = self.time_span()?;
        if t < first || t > last {
            return None;
        }
        Some(self.at(self.offset_of_time(t)))
    }

    /// Aggregate every retained sample between `t0` and `t1`, in either order.
    pub fn range_stats(&self, t0: f64, t1: f64) -> Option<RangeStats> {
        let (lo, hi) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        let (first, last) = self.time_span()?;
        if hi < first || lo > last {
            return None;
        }
        let start = self.offset_of_time(lo.max(first));
        let end = self.offset_of_time(hi.min(last));
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut sum = 0.0f64;
        let mut sum_sq = 0.0f64;
        let mut n = 0usize;
        for i in start..=end {
            let v = self.at(i);
            if !v.is_finite() {
                continue;
            }
            min = min.min(v);
            max = max.max(v);
            sum += v as f64;
            sum_sq += (v as f64) * (v as f64);
            n += 1;
        }
        if n == 0 {
            return None;
        }
        let mean = sum / n as f64;
        let var = (sum_sq / n as f64 - mean * mean).max(0.0);
        Some(RangeStats {
            mean: mean as f32,
            std: var.sqrt() as f32,
            rms: (sum_sq / n as f64).sqrt() as f32,
            min,
            max,
            buckets: n,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 1e-3; // 1 ksps keeps the arithmetic readable

    fn trace(capacity: usize) -> RawTrace {
        let mut t = RawTrace::new(capacity);
        t.reconfigure(capacity, P);
        t
    }

    #[test]
    fn a_new_trace_is_empty() {
        let t = trace(16);
        assert!(t.is_empty());
        assert!(t.time_span().is_none());
        assert!(t.value_at(0.0).is_none());
    }

    #[test]
    fn samples_are_retained_with_their_times() {
        let mut t = trace(16);
        t.push(&[1.0, 2.0, 3.0], 0);
        assert_eq!(t.len(), 3);
        let (first, last) = t.time_span().unwrap();
        assert!((first - 0.0).abs() < 1e-12);
        assert!((last - 2.0 * P).abs() < 1e-12);
    }

    #[test]
    fn contiguous_pushes_join_up() {
        let mut t = trace(16);
        t.push(&[1.0, 2.0], 0);
        t.push(&[3.0, 4.0], 2);
        assert_eq!(t.len(), 4);
        assert_eq!(t.value_at(3.0 * P), Some(4.0));
    }

    #[test]
    fn a_discontinuity_restarts_the_window() {
        let mut t = trace(16);
        t.push(&[1.0, 2.0], 0);
        // Jump forward: the samples between are missing, so splicing them
        // together would misreport time.
        t.push(&[9.0], 100);
        assert_eq!(t.len(), 1);
        assert_eq!(t.value_at(100.0 * P), Some(9.0));
    }

    #[test]
    fn the_ring_keeps_the_newest_samples() {
        let mut t = trace(4);
        let s: Vec<f32> = (0..10).map(|i| i as f32).collect();
        t.push(&s, 0);
        assert_eq!(t.len(), 4);
        let (first, last) = t.time_span().unwrap();
        assert!((first - 6.0 * P).abs() < 1e-12, "oldest should be index 6");
        assert!((last - 9.0 * P).abs() < 1e-12);
        assert_eq!(t.value_at(9.0 * P), Some(9.0));
        assert_eq!(t.value_at(6.0 * P), Some(6.0));
    }

    #[test]
    fn aggregate_returns_one_bucket_per_sample_when_zoomed_in() {
        // This is what makes a real waveform visible rather than a block.
        let mut t = trace(64);
        t.push(&[0.0, 1.0, 0.0, -1.0], 0);
        let mut out = Vec::new();
        t.aggregate(0.0, 3.0 * P, 100, &mut out);
        assert_eq!(out.len(), 4, "one bucket per sample");
        assert_eq!(out[1].mean, 1.0);
        assert_eq!(out[3].mean, -1.0);
    }

    #[test]
    fn aggregate_folds_into_the_requested_columns() {
        let mut t = trace(1024);
        let s: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        t.push(&s, 0);
        let mut out = Vec::new();
        t.aggregate(0.0, 999.0 * P, 10, &mut out);
        assert_eq!(out.len(), 10);
        assert_eq!(out[0].min, 0.0);
        assert_eq!(out[9].max, 999.0);
    }

    #[test]
    fn aggregate_preserves_a_single_sample_spike() {
        let mut t = trace(4096);
        let mut s = vec![0.0f32; 4000];
        s[1234] = 99.0;
        t.push(&s, 0);
        let mut out = Vec::new();
        t.aggregate(0.0, 3999.0 * P, 20, &mut out);
        let peak = out.iter().map(|b| b.max).fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(peak, 99.0, "the spike must survive aggregation");
    }

    #[test]
    fn aggregate_columns_are_contiguous_and_ordered() {
        let mut t = trace(1024);
        let s: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        t.push(&s, 0);
        let mut out = Vec::new();
        t.aggregate(0.0, 999.0 * P, 17, &mut out);
        assert!(out.windows(2).all(|w| w[1].t > w[0].t), "times must increase");
    }

    #[test]
    fn aggregate_outside_the_window_yields_nothing() {
        let mut t = trace(64);
        t.push(&[1.0; 10], 0);
        let mut out = Vec::new();
        t.aggregate(100.0, 200.0, 10, &mut out);
        assert!(out.is_empty(), "a range after the window draws nothing");
        t.aggregate(-200.0, -100.0, 10, &mut out);
        assert!(out.is_empty(), "a range before the window draws nothing");
    }

    #[test]
    fn aggregate_clips_a_range_that_only_partly_overlaps() {
        let mut t = trace(64);
        t.push(&[1.0; 10], 0);
        let mut out = Vec::new();
        // Asking for far more than exists must still draw what does.
        t.aggregate(-100.0, 100.0, 10, &mut out);
        assert!(!out.is_empty());
        let (first, last) = t.time_span().unwrap();
        assert!(out.iter().all(|b| b.t >= first && b.t <= last));
    }

    #[test]
    fn aggregate_ignores_non_finite_samples() {
        let mut t = trace(64);
        t.push(&[1.0, f32::NAN, 1.0, 1.0], 0);
        let mut out = Vec::new();
        t.aggregate(0.0, 3.0 * P, 1, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].min, 1.0);
        assert_eq!(out[0].max, 1.0);
    }

    #[test]
    fn covers_reports_whether_the_window_holds_the_range() {
        let mut t = trace(16);
        t.push(&[1.0; 10], 0);
        assert!(t.covers(P, 5.0 * P));
        assert!(!t.covers(-1.0, 5.0 * P), "before the oldest sample");
        assert!(!t.covers(P, 100.0), "after the newest sample");
    }

    #[test]
    fn range_stats_aggregate_retained_samples() {
        let mut t = trace(64);
        t.push(&[1.0, 2.0, 3.0, 4.0], 0);
        let r = t.range_stats(0.0, 3.0 * P).unwrap();
        assert_eq!(r.min, 1.0);
        assert_eq!(r.max, 4.0);
        assert!((r.mean - 2.5).abs() < 1e-6);
        assert_eq!(r.buckets, 4);
        assert!((r.p2p() - 3.0).abs() < 1e-6);
        // rms of 1,2,3,4 is sqrt(30/4) = 2.7386
        assert!((r.rms - 2.738_6).abs() < 1e-3, "rms {}", r.rms);
        // population std of 1,2,3,4 is sqrt(1.25) = 1.1180
        assert!((r.std - 1.118_0).abs() < 1e-3, "std {}", r.std);
    }

    #[test]
    fn range_stats_accept_either_order_and_clamp() {
        let mut t = trace(64);
        t.push(&[1.0, 2.0, 3.0, 4.0], 0);
        let a = t.range_stats(0.0, 3.0 * P).unwrap();
        let b = t.range_stats(3.0 * P, -5.0).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn reconfigure_resizes_and_clears() {
        let mut t = trace(8);
        t.push(&[1.0; 8], 0);
        t.reconfigure(32, 1e-6);
        assert!(t.is_empty());
        assert!((t.sample_period() - 1e-6).abs() < 1e-15);
    }

    #[test]
    fn a_degenerate_sample_period_falls_back_rather_than_dividing_by_zero() {
        let mut t = RawTrace::new(8);
        t.reconfigure(8, 0.0);
        t.push(&[1.0, 2.0], 0);
        assert!(t.sample_period() > 0.0);
        assert!(t.time_span().is_some());
    }
}
