//! Peak and threshold detection over the displayed buckets.
//!
//! Finding the extremes of a trace by eye is slow and imprecise, so this
//! reports them directly: the tallest local maxima, and every region that
//! crosses a threshold.
//!
//! Detection runs over the aggregated buckets rather than the raw samples, so
//! it finds what is actually on screen. Zooming in therefore refines the
//! answer rather than changing it arbitrarily, and the cost does not grow with
//! the capture length.

use crate::buffer::Bucket;

/// One detected peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Peak {
    /// Time of the peak, in seconds.
    pub t: f64,
    /// Value at the peak.
    pub value: f32,
}

/// A contiguous run of buckets on the far side of a threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossing {
    /// Time the region starts, in seconds.
    pub start: f64,
    /// Time the region ends, in seconds.
    pub end: f64,
    /// The most extreme value reached inside it.
    pub peak: f32,
    /// Time of that extreme.
    pub peak_t: f64,
}

impl Crossing {
    /// How long the region lasts, in seconds.
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

/// Which side of a threshold counts as a crossing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Regions at or above the threshold.
    Above,
    /// Regions at or below the threshold.
    Below,
}

/// Find the `limit` tallest local maxima.
///
/// A bucket counts as a peak when its maximum is at least as large as every
/// bucket within `min_separation` seconds, which stops a single broad hump
/// being reported dozens of times. Results are ordered tallest first, so
/// taking the top few is just taking the front of the list.
pub fn find_peaks(buckets: &[Bucket], min_separation: f64, limit: usize) -> Vec<Peak> {
    if buckets.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut found: Vec<Peak> = Vec::new();
    for (i, b) in buckets.iter().enumerate() {
        if b.is_empty() {
            continue;
        }
        // A plateau would otherwise report every bucket in it; requiring a
        // strict rise on the left makes the first bucket of the plateau win.
        let mut is_peak = true;
        for (j, other) in buckets.iter().enumerate() {
            if i == j || other.is_empty() {
                continue;
            }
            if (other.t - b.t).abs() > min_separation {
                continue;
            }
            let taller = other.max > b.max;
            let equal_and_earlier = other.max == b.max && j < i;
            if taller || equal_and_earlier {
                is_peak = false;
                break;
            }
        }
        if is_peak {
            found.push(Peak {
                t: b.t,
                value: b.max,
            });
        }
    }

    found.sort_by(|a, b| b.value.total_cmp(&a.value));
    found.truncate(limit);
    found
}

/// Find every region where the trace crosses `threshold`.
///
/// Adjacent buckets on the far side are merged into one region, so a pulse is
/// reported once with its duration rather than as a scatter of samples.
pub fn find_crossings(buckets: &[Bucket], threshold: f32, edge: Edge) -> Vec<Crossing> {
    let mut out: Vec<Crossing> = Vec::new();
    let mut open: Option<Crossing> = None;

    for b in buckets {
        if b.is_empty() {
            continue;
        }
        // Compared against the bucket extreme on the relevant side, so a brief
        // excursion inside a bucket still counts rather than being averaged
        // away.
        let (value, crossed) = match edge {
            Edge::Above => (b.max, b.max >= threshold),
            Edge::Below => (b.min, b.min <= threshold),
        };

        match (&mut open, crossed) {
            (Some(region), true) => {
                region.end = b.t;
                let better = match edge {
                    Edge::Above => value > region.peak,
                    Edge::Below => value < region.peak,
                };
                if better {
                    region.peak = value;
                    region.peak_t = b.t;
                }
            }
            (Some(_), false) => {
                out.push(open.take().expect("checked present"));
            }
            (None, true) => {
                open = Some(Crossing {
                    start: b.t,
                    end: b.t,
                    peak: value,
                    peak_t: b.t,
                });
            }
            (None, false) => {}
        }
    }
    // A region still open at the end of the data is real; the trace simply has
    // not come back yet.
    if let Some(region) = open {
        out.push(region);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(t: f64, v: f32) -> Bucket {
        Bucket {
            t,
            min: v,
            max: v,
            mean: v,
        }
    }

    fn ramp(values: &[f32]) -> Vec<Bucket> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| bucket(i as f64, *v))
            .collect()
    }

    #[test]
    fn no_data_yields_no_peaks() {
        assert!(find_peaks(&[], 1.0, 5).is_empty());
    }

    #[test]
    fn a_limit_of_zero_yields_nothing() {
        let b = ramp(&[0.0, 5.0, 0.0]);
        assert!(find_peaks(&b, 0.5, 0).is_empty());
    }

    #[test]
    fn a_single_peak_is_found() {
        let b = ramp(&[0.0, 1.0, 5.0, 1.0, 0.0]);
        let p = find_peaks(&b, 1.5, 5);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].value, 5.0);
        assert_eq!(p[0].t, 2.0);
    }

    #[test]
    fn peaks_come_back_tallest_first() {
        let b = ramp(&[0.0, 3.0, 0.0, 9.0, 0.0, 6.0, 0.0]);
        let p = find_peaks(&b, 1.5, 5);
        let values: Vec<f32> = p.iter().map(|x| x.value).collect();
        assert_eq!(values, vec![9.0, 6.0, 3.0]);
    }

    #[test]
    fn the_limit_keeps_only_the_tallest() {
        let b = ramp(&[0.0, 3.0, 0.0, 9.0, 0.0, 6.0, 0.0]);
        let p = find_peaks(&b, 1.5, 2);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].value, 9.0);
        assert_eq!(p[1].value, 6.0);
    }

    #[test]
    fn separation_stops_one_hump_being_reported_many_times() {
        // A broad rise would otherwise yield a peak at every bucket near the
        // top.
        let b = ramp(&[0.0, 1.0, 2.0, 3.0, 2.9, 2.8, 1.0, 0.0]);
        let p = find_peaks(&b, 4.0, 10);
        assert_eq!(p.len(), 1, "got {p:?}");
        assert_eq!(p[0].value, 3.0);
    }

    #[test]
    fn a_plateau_reports_its_first_bucket_only() {
        let b = ramp(&[0.0, 5.0, 5.0, 5.0, 0.0]);
        let p = find_peaks(&b, 2.5, 10);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].t, 1.0, "the earliest bucket of the plateau wins");
    }

    #[test]
    fn empty_buckets_are_skipped() {
        let mut b = ramp(&[0.0, 5.0, 0.0]);
        b.insert(
            1,
            Bucket {
                t: 0.5,
                min: f32::NAN,
                max: f32::NAN,
                mean: f32::NAN,
            },
        );
        let p = find_peaks(&b, 1.5, 5);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].value, 5.0);
    }

    #[test]
    fn a_threshold_crossing_is_one_region_not_many() {
        let b = ramp(&[0.0, 2.0, 3.0, 2.0, 0.0]);
        let c = find_crossings(&b, 1.5, Edge::Above);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].start, 1.0);
        assert_eq!(c[0].end, 3.0);
        assert_eq!(c[0].peak, 3.0);
        assert_eq!(c[0].peak_t, 2.0);
        assert_eq!(c[0].duration(), 2.0);
    }

    #[test]
    fn separate_pulses_are_separate_regions() {
        let b = ramp(&[0.0, 2.0, 0.0, 2.0, 0.0]);
        let c = find_crossings(&b, 1.0, Edge::Above);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].start, 1.0);
        assert_eq!(c[1].start, 3.0);
    }

    #[test]
    fn a_region_still_open_at_the_end_is_reported() {
        // The trace has simply not come back down yet.
        let b = ramp(&[0.0, 0.0, 5.0, 5.0]);
        let c = find_crossings(&b, 1.0, Edge::Above);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].end, 3.0);
    }

    #[test]
    fn crossing_below_a_threshold_works_too() {
        // Brown-outs matter as much as surges.
        let b = ramp(&[5.0, 5.0, 1.0, 1.0, 5.0]);
        let c = find_crossings(&b, 2.0, Edge::Below);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].start, 2.0);
        assert_eq!(c[0].peak, 1.0, "the extreme below is the minimum");
    }

    #[test]
    fn a_brief_excursion_inside_a_bucket_still_counts() {
        // The bucket mean would hide this; the extreme must not.
        let b = vec![
            bucket(0.0, 0.0),
            Bucket {
                t: 1.0,
                min: 0.0,
                max: 9.0,
                mean: 0.1,
            },
            bucket(2.0, 0.0),
        ];
        let c = find_crossings(&b, 5.0, Edge::Above);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].peak, 9.0);
    }

    #[test]
    fn nothing_crosses_an_unreachable_threshold() {
        let b = ramp(&[0.0, 1.0, 2.0]);
        assert!(find_crossings(&b, 100.0, Edge::Above).is_empty());
        assert!(find_crossings(&b, -100.0, Edge::Below).is_empty());
    }

    #[test]
    fn a_value_exactly_on_the_threshold_counts() {
        let b = ramp(&[0.0, 2.0, 0.0]);
        assert_eq!(find_crossings(&b, 2.0, Edge::Above).len(), 1);
    }
}
