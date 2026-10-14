//! The JS220 timebase.
//!
//! Timestamps are `i64` values in 34Q30 fixed point: 34 integer bits of
//! seconds and 30 fractional bits, giving sub-nanosecond resolution over a
//! range of roughly ±272 years. The epoch is 2018-01-01T00:00:00Z rather than
//! the POSIX epoch.

use std::time::{SystemTime, UNIX_EPOCH};

/// Number of fractional bits in a device timestamp.
pub const TIME_Q: u32 = 30;

/// One second in device time units.
pub const SECOND: i64 = 1 << TIME_Q;

/// Seconds between the POSIX epoch and the device epoch of 2018-01-01Z.
pub const EPOCH_UNIX_OFFSET_SECONDS: i64 = 1_514_764_800;

/// Convert seconds to a device timestamp.
pub fn from_seconds(seconds: f64) -> i64 {
    (seconds * SECOND as f64) as i64
}

/// Convert a device timestamp to seconds.
pub fn to_seconds(t: i64) -> f64 {
    t as f64 / SECOND as f64
}

/// Convert a device timestamp to POSIX seconds.
pub fn to_unix_seconds(t: i64) -> f64 {
    to_seconds(t) + EPOCH_UNIX_OFFSET_SECONDS as f64
}

/// Convert POSIX seconds to a device timestamp.
pub fn from_unix_seconds(unix: f64) -> i64 {
    from_seconds(unix - EPOCH_UNIX_OFFSET_SECONDS as f64)
}

/// The current wall clock as a device timestamp.
///
/// Returns zero if the system clock is set before the device epoch, which is
/// the only case `SystemTime` arithmetic can fail here.
pub fn now() -> i64 {
    let Ok(d) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    from_unix_seconds(d.as_secs_f64())
}

/// Maps device sample ids onto wall-clock time.
///
/// Built from the periodic time map the device pushes on port 0. Until the
/// first map arrives this is unset and conversions return `None`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimeMap {
    offset_time: i64,
    offset_counter: u64,
    counter_rate: f64,
    valid: bool,
}

impl TimeMap {
    /// An unset map.
    pub const fn new() -> Self {
        Self {
            offset_time: 0,
            offset_counter: 0,
            counter_rate: 0.0,
            valid: false,
        }
    }

    /// Update from a device time map announcement.
    ///
    /// A non-positive counter rate is rejected, since it would make the mapping
    /// meaningless and could divide by zero downstream.
    pub fn update(&mut self, utc_q30: i64, counter: u64, counter_rate: f64) -> bool {
        // `is_finite` rejects NaN first, so the comparison below only ever sees
        // an ordered value.
        if !counter_rate.is_finite() || counter_rate <= 0.0 {
            return false;
        }
        self.offset_time = utc_q30;
        self.offset_counter = counter;
        self.counter_rate = counter_rate;
        self.valid = true;
        true
    }

    /// True once a map has been received.
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// The counter rate in Hz, or zero when unset.
    pub fn counter_rate(&self) -> f64 {
        self.counter_rate
    }

    /// Convert a sample id to a device timestamp.
    pub fn sample_to_time(&self, sample_id: u64) -> Option<i64> {
        if !self.valid {
            return None;
        }
        let delta = sample_id as i128 - self.offset_counter as i128;
        let seconds = delta as f64 / self.counter_rate;
        Some(self.offset_time + from_seconds(seconds))
    }

    /// Convert a sample id to POSIX seconds.
    pub fn sample_to_unix_seconds(&self, sample_id: u64) -> Option<f64> {
        self.sample_to_time(sample_id).map(to_unix_seconds)
    }

    /// Convert a device timestamp to the nearest sample id.
    pub fn time_to_sample(&self, t: i64) -> Option<u64> {
        if !self.valid {
            return None;
        }
        let seconds = to_seconds(t - self.offset_time);
        let delta = (seconds * self.counter_rate).round() as i128;
        let id = self.offset_counter as i128 + delta;
        u64::try_from(id).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_second_is_two_to_the_thirty() {
        assert_eq!(SECOND, 1_073_741_824);
        assert_eq!(from_seconds(1.0), SECOND);
        assert!((to_seconds(SECOND) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn seconds_roundtrip_through_fixed_point() {
        for s in [0.0, 1.0, -1.0, 1234.5, 1e-6] {
            let back = to_seconds(from_seconds(s));
            assert!((back - s).abs() < 1e-9, "{s} came back as {back}");
        }
    }

    #[test]
    fn the_epoch_is_2018_not_1970() {
        // Device time zero is 2018-01-01T00:00:00Z.
        assert!((to_unix_seconds(0) - 1_514_764_800.0).abs() < 1e-6);
    }

    #[test]
    fn unix_seconds_roundtrip() {
        let unix = 1_700_000_000.0;
        let back = to_unix_seconds(from_unix_seconds(unix));
        assert!((back - unix).abs() < 1e-6, "got {back}");
    }

    #[test]
    fn now_lands_after_the_epoch() {
        assert!(now() > 0, "wall clock should be after 2018");
    }

    #[test]
    fn an_unset_map_converts_nothing() {
        let m = TimeMap::new();
        assert!(!m.is_valid());
        assert_eq!(m.sample_to_time(100), None);
        assert_eq!(m.time_to_sample(100), None);
    }

    #[test]
    fn sample_ids_map_forward_at_the_counter_rate() {
        let mut m = TimeMap::new();
        assert!(m.update(0, 1_000_000, 2_000_000.0));
        // One second later is 2_000_000 counts on.
        let t = m.sample_to_time(3_000_000).unwrap();
        assert!((to_seconds(t) - 1.0).abs() < 1e-6, "got {}", to_seconds(t));
    }

    #[test]
    fn sample_ids_before_the_anchor_map_backwards() {
        let mut m = TimeMap::new();
        m.update(from_seconds(10.0), 2_000_000, 2_000_000.0);
        let t = m.sample_to_time(1_000_000).unwrap();
        assert!((to_seconds(t) - 9.5).abs() < 1e-6, "got {}", to_seconds(t));
    }

    #[test]
    fn time_maps_back_to_the_sample_id_it_came_from() {
        let mut m = TimeMap::new();
        m.update(from_seconds(5.0), 1_234_567, 2_000_000.0);
        for id in [1_234_567u64, 2_000_000, 10_000_000] {
            let t = m.sample_to_time(id).unwrap();
            assert_eq!(m.time_to_sample(t).unwrap(), id, "id {id}");
        }
    }

    #[test]
    fn a_nonpositive_counter_rate_is_rejected() {
        let mut m = TimeMap::new();
        assert!(!m.update(0, 0, 0.0));
        assert!(!m.update(0, 0, -1.0));
        assert!(!m.update(0, 0, f64::NAN));
        assert!(!m.is_valid());
    }

    #[test]
    fn mapping_stays_exact_far_from_the_anchor() {
        let mut m = TimeMap::new();
        m.update(0, 0, 2_000_000.0);
        // One hour of samples at 2 Msps.
        let id = 2_000_000u64 * 3600;
        let secs = to_seconds(m.sample_to_time(id).unwrap());
        assert!((secs - 3600.0).abs() < 1e-3, "got {secs}");
    }

    #[test]
    fn a_negative_mapped_sample_id_is_refused_rather_than_wrapping() {
        let mut m = TimeMap::new();
        m.update(0, 0, 2_000_000.0);
        assert_eq!(m.time_to_sample(from_seconds(-10.0)), None);
    }
}
