//! Host-side decimation.
//!
//! The instrument streams the analog signals at 1 Msps. It can decimate on its
//! own, but only under a filter mode this driver does not configure, so lower
//! output rates are produced here instead.
//!
//! Each output sample is the mean of `n` consecutive input samples. A box
//! average is a real low-pass — dropping every `n`th sample instead would
//! alias wideband noise straight into the measurement — and it preserves the
//! mean exactly, which is what charge and energy integrate.
//!
//! Buckets are keyed by absolute sample id rather than by arrival order, so
//! every signal lands on the same boundaries even if their frames arrive at
//! different times or one of them drops a frame.

/// Averages groups of `n` input samples into one output sample.
#[derive(Debug, Clone)]
pub struct Decimator {
    n: u64,
    /// Sample id step between consecutive input samples.
    input_step: u64,
    /// Absolute bucket index currently being accumulated.
    bucket: Option<u64>,
    sum: f64,
    count: u64,
}

impl Decimator {
    /// Average `n` input samples, each `input_step` sample ids apart.
    ///
    /// `n` of zero or one is treated as one, making this a pass-through.
    pub fn new(n: u32, input_step: u32) -> Self {
        Self {
            n: n.max(1) as u64,
            input_step: input_step.max(1) as u64,
            bucket: None,
            sum: 0.0,
            count: 0,
        }
    }

    /// Input samples per output sample.
    pub fn factor(&self) -> u64 {
        self.n
    }

    /// Sample id step between consecutive output samples.
    pub fn output_step(&self) -> u64 {
        self.n * self.input_step
    }

    /// True when no decimation is applied.
    pub fn is_passthrough(&self) -> bool {
        self.n == 1
    }

    /// Discard any partially accumulated bucket.
    pub fn reset(&mut self) {
        self.bucket = None;
        self.sum = 0.0;
        self.count = 0;
    }

    /// Feed `samples`, the first of which has sample id `first_id`.
    ///
    /// Appends completed output samples to `out` and returns the sample id of
    /// the first one appended, or `None` if no bucket completed. The returned
    /// id is the id of the first *input* sample of that bucket, so output ids
    /// stay on `output_step` boundaries.
    ///
    /// A bucket that is interrupted by a gap is dropped rather than averaged
    /// over fewer samples: a short average would be a differently-weighted
    /// measurement silently mixed in with the rest.
    pub fn process(&mut self, first_id: u64, samples: &[f32], out: &mut Vec<f32>) -> Option<u64> {
        if self.n == 1 {
            out.extend_from_slice(samples);
            return (!samples.is_empty()).then_some(first_id);
        }

        let span = self.output_step();
        let mut first_out: Option<u64> = None;

        for (i, &value) in samples.iter().enumerate() {
            let id = first_id + i as u64 * self.input_step;
            let bucket = id / span;

            match self.bucket {
                Some(current) if current == bucket => {}
                Some(_) | None => {
                    // Starting a different bucket means the previous one never
                    // filled; drop its partial sum.
                    self.bucket = Some(bucket);
                    self.sum = 0.0;
                    self.count = 0;
                }
            }

            self.sum += value as f64;
            self.count += 1;

            if self.count == self.n {
                if first_out.is_none() {
                    first_out = Some(bucket * span);
                }
                out.push((self.sum / self.n as f64) as f32);
                self.bucket = None;
                self.sum = 0.0;
                self.count = 0;
            }
        }

        first_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_copies_input_unchanged() {
        let mut d = Decimator::new(1, 2);
        assert!(d.is_passthrough());
        let mut out = Vec::new();
        assert_eq!(d.process(100, &[1.0, 2.0, 3.0], &mut out), Some(100));
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn passthrough_of_nothing_yields_nothing() {
        let mut d = Decimator::new(1, 2);
        let mut out = Vec::new();
        assert_eq!(d.process(0, &[], &mut out), None);
        assert!(out.is_empty());
    }

    #[test]
    fn zero_factor_is_treated_as_passthrough() {
        let d = Decimator::new(0, 2);
        assert!(d.is_passthrough());
        assert_eq!(d.factor(), 1);
    }

    #[test]
    fn averages_each_group() {
        // n = 4, step 2: ids 0,2,4,6 form one bucket spanning 8 ids.
        let mut d = Decimator::new(4, 2);
        assert_eq!(d.output_step(), 8);
        let mut out = Vec::new();
        let id = d.process(0, &[1.0, 2.0, 3.0, 4.0], &mut out);
        assert_eq!(id, Some(0));
        assert_eq!(out, vec![2.5]);
    }

    #[test]
    fn emits_one_output_per_full_bucket() {
        let mut d = Decimator::new(2, 2);
        let mut out = Vec::new();
        let samples: Vec<f32> = (0..8).map(|v| v as f32).collect();
        assert_eq!(d.process(0, &samples, &mut out), Some(0));
        assert_eq!(out, vec![0.5, 2.5, 4.5, 6.5]);
    }

    #[test]
    fn buckets_carry_across_frame_boundaries() {
        let mut d = Decimator::new(4, 2);
        let mut out = Vec::new();
        // Half a bucket, then the rest in a second frame.
        assert_eq!(d.process(0, &[1.0, 2.0], &mut out), None);
        assert!(out.is_empty(), "no bucket has completed yet");
        assert_eq!(d.process(4, &[3.0, 4.0], &mut out), Some(0));
        assert_eq!(out, vec![2.5]);
    }

    #[test]
    fn output_ids_land_on_bucket_boundaries() {
        let mut d = Decimator::new(4, 2);
        let mut out = Vec::new();
        // Start mid-bucket: ids 2,4,6 belong to bucket 0, which cannot fill.
        assert_eq!(d.process(2, &[1.0, 1.0, 1.0], &mut out), None);
        // Bucket 1 spans ids 8..16 and fills completely.
        let id = d.process(8, &[2.0, 2.0, 2.0, 2.0], &mut out).unwrap();
        assert_eq!(id, 8, "output id must be the bucket's first input id");
        assert_eq!(id % d.output_step(), 0);
        assert_eq!(out, vec![2.0]);
    }

    #[test]
    fn a_gap_drops_the_partial_bucket_rather_than_short_averaging() {
        let mut d = Decimator::new(4, 2);
        let mut out = Vec::new();
        d.process(0, &[9.0, 9.0], &mut out);
        // Jump to a later bucket; the half-filled one must not be emitted.
        let id = d.process(80, &[1.0, 1.0, 1.0, 1.0], &mut out).unwrap();
        assert_eq!(id, 80);
        assert_eq!(out, vec![1.0], "only the complete bucket is emitted");
    }

    #[test]
    fn two_signals_stay_on_the_same_boundaries() {
        // Same ids through two independent decimators must produce the same
        // output ids, which is what keeps the aligner's rows meaningful.
        let mut a = Decimator::new(4, 2);
        let mut b = Decimator::new(4, 2);
        let (mut oa, mut ob) = (Vec::new(), Vec::new());
        let ida = a.process(16, &[1.0, 1.0, 1.0, 1.0], &mut oa);
        let idb = b.process(16, &[5.0, 5.0, 5.0, 5.0], &mut ob);
        assert_eq!(ida, idb);
        assert_eq!(oa.len(), ob.len());
    }

    #[test]
    fn reset_discards_the_partial_bucket() {
        let mut d = Decimator::new(4, 2);
        let mut out = Vec::new();
        d.process(0, &[9.0, 9.0], &mut out);
        d.reset();
        assert_eq!(d.process(4, &[1.0, 1.0], &mut out), None);
        assert!(out.is_empty(), "the reset bucket must not complete");
    }

    #[test]
    fn the_average_preserves_the_mean() {
        // 1000 samples at 1 Msps averaged to 1 ksps: the overall mean must not
        // move, since charge and energy integrate it.
        let mut d = Decimator::new(1000, 2);
        let mut out = Vec::new();
        let samples: Vec<f32> = (0..1000).map(|i| if i % 2 == 0 { 0.0 } else { 1.0 }).collect();
        d.process(0, &samples, &mut out);
        assert_eq!(out.len(), 1);
        assert!((out[0] - 0.5).abs() < 1e-6, "got {}", out[0]);
    }

    #[test]
    fn large_factors_do_not_overflow_the_accumulator() {
        let mut d = Decimator::new(2000, 2);
        let mut out = Vec::new();
        let samples = vec![1e30f32; 2000];
        d.process(0, &samples, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].is_finite(), "accumulated in f64, got {}", out[0]);
    }
}
