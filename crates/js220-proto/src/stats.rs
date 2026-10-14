//! On-instrument statistics (stream port 14).
//!
//! The sensor accumulates per-block sums in fixed point and ships them as a
//! 192-byte record. Sums of squares and running integrals are 128-bit, which
//! Rust expresses natively as `i128` — the reference C driver carries a
//! hand-rolled 128-bit arithmetic module for the same job.
//!
//! Record layout:
//!
//! ```text
//! offset  size  field
//!      0     4  header          see StatsHeader
//!      4     4  sample_freq     undecimated samples per second
//!      8     8  block_sample_id first sample of this block
//!     16     8  accum_sample_id first sample of the running integral
//!     24     8  i_x1            sum(i)       Q31
//!     32     8  i_min           Q31
//!     40     8  i_max           Q31
//!     48     8  v_x1            sum(v)       Q31
//!     56     8  v_min           Q31
//!     64     8  v_max           Q31
//!     72     8  p_x1            sum(p)       Q27
//!     80     8  p_min           Q27
//!     88     8  p_max           Q27
//!     96    16  i_x2            sum(i*i)     Q62
//!    112    16  i_int           integral(i)  Q31
//!    128    16  v_x2            sum(v*v)     Q62
//!    144    16  v_int           reserved
//!    160    16  p_x2            sum(p*p)     Q54
//!    176    16  p_int           integral(p)  Q31
//! ```

/// Size of a raw statistics record.
pub const RECORD_LEN: usize = 192;

/// Fixed-point exponent for current and voltage sums, minima and maxima.
const Q_IV: u32 = 31;
/// Fixed-point exponent for power sums, minima and maxima.
const Q_P: u32 = 27;
/// Fixed-point exponent for the charge and energy integrals.
const Q_INT: u32 = 31;

/// The byte the device writes into the top of the header word: raw record,
/// version 1, decimation factor 2.
const HEADER_TAG: u32 = 0x92;

/// Per-signal summary over one block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SignalStats {
    /// Arithmetic mean over the block.
    pub avg: f64,
    /// Population standard deviation over the block.
    pub std: f64,
    /// Smallest sample in the block.
    pub min: f64,
    /// Largest sample in the block.
    pub max: f64,
}

/// A decoded statistics record.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Statistics {
    /// Number of decimated samples contributing to this block.
    pub block_sample_count: u32,
    /// On-instrument decimation factor from sample ids to computed samples.
    pub decimate_factor: u32,
    /// Undecimated sample rate in Hz.
    pub sample_freq: u32,
    /// Sample id of the first sample in this block.
    pub block_sample_id: u64,
    /// Sample id where the running integrals started accumulating.
    pub accum_sample_id: u64,
    /// Current statistics, in amperes.
    pub current: SignalStats,
    /// Voltage statistics, in volts.
    pub voltage: SignalStats,
    /// Power statistics, in watts.
    pub power: SignalStats,
    /// Accumulated charge in coulombs since `accum_sample_id`.
    pub charge: f64,
    /// Accumulated energy in joules since `accum_sample_id`.
    pub energy: f64,
}

impl Statistics {
    /// Duration of this block in seconds, or zero if the rate is unknown.
    pub fn block_duration(&self) -> f64 {
        let rate = self.decimated_rate();
        if rate == 0.0 {
            0.0
        } else {
            self.block_sample_count as f64 / rate
        }
    }

    /// The decimated sample rate in Hz.
    pub fn decimated_rate(&self) -> f64 {
        if self.decimate_factor == 0 {
            0.0
        } else {
            self.sample_freq as f64 / self.decimate_factor as f64
        }
    }
}

/// Failures while decoding a statistics record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsError {
    /// The payload was shorter than [`RECORD_LEN`].
    Short {
        /// Bytes available.
        have: usize,
    },
    /// The header word did not carry the expected record tag.
    BadHeader {
        /// The header word as received.
        header: u32,
    },
    /// The block contained no samples, so no average is defined.
    EmptyBlock,
}

impl core::fmt::Display for StatsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short { have } => {
                write!(f, "statistics record too short: {have} < {RECORD_LEN}")
            }
            Self::BadHeader { header } => {
                write!(f, "statistics header tag invalid: {header:#010x}")
            }
            Self::EmptyBlock => write!(f, "statistics block contains no samples"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for StatsError {}

fn rd_i64(b: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(b[off..off + 8].try_into().expect("caller checked length"))
}

fn rd_i128(b: &[u8], off: usize) -> i128 {
    i128::from_le_bytes(b[off..off + 16].try_into().expect("caller checked length"))
}

/// Convert a fixed-point value with `q` fractional bits to a float.
///
/// `i128 as f64` is correctly rounded and the divisor is an exact power of two,
/// so the only error is the single rounding of the conversion itself.
#[inline]
fn q_to_f64(x: i128, q: u32) -> f64 {
    x as f64 / (1u128 << q) as f64
}

/// Population standard deviation from the sum and sum of squares.
///
/// Computes `sqrt((x2 - x1^2 / n) / n)` entirely in 128-bit integers before the
/// single conversion to float, so no precision is lost to intermediate
/// rounding. Returns zero when accumulated rounding drives the variance
/// negative, which can happen for a constant signal.
fn std_dev(x1: i64, x2: i128, n: u64, q: u32) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let mean_sq = (x1 as i128) * (x1 as i128) / (n as i128);
    if x2 < mean_sq {
        return 0.0;
    }
    let variance = (x2 - mean_sq) / (n as i128);
    q_to_f64(variance, q * 2).sqrt()
}

/// Decode a raw statistics record.
pub fn decode(payload: &[u8]) -> Result<Statistics, StatsError> {
    if payload.len() < RECORD_LEN {
        return Err(StatsError::Short {
            have: payload.len(),
        });
    }
    let header = u32::from_le_bytes(payload[0..4].try_into().expect("checked above"));
    if (header >> 24) != HEADER_TAG {
        return Err(StatsError::BadHeader { header });
    }
    let block_sample_count = header & 0x00ff_ffff;
    if block_sample_count == 0 {
        return Err(StatsError::EmptyBlock);
    }
    let decimate_factor = (header >> 24) & 0x0f;
    let sample_freq = u32::from_le_bytes(payload[4..8].try_into().expect("checked above"));

    let n = block_sample_count as u64;
    // Dividing by `n << q` folds the block average and the fixed-point scale
    // into one exact power-of-two-scaled division.
    let avg_iv = 1.0 / ((n as f64) * (1u64 << Q_IV) as f64);
    let avg_p = 1.0 / ((n as f64) * (1u64 << Q_P) as f64);
    let mm_iv = 1.0 / (1u64 << Q_IV) as f64;
    let mm_p = 1.0 / (1u64 << Q_P) as f64;

    let i_x1 = rd_i64(payload, 24);
    let v_x1 = rd_i64(payload, 48);
    let p_x1 = rd_i64(payload, 72);
    let i_x2 = rd_i128(payload, 96);
    let v_x2 = rd_i128(payload, 128);
    let p_x2 = rd_i128(payload, 160);

    let decimated_freq = if decimate_factor == 0 {
        sample_freq as f64
    } else {
        sample_freq as f64 / decimate_factor as f64
    };
    let integral_scale = if decimated_freq == 0.0 {
        0.0
    } else {
        1.0 / decimated_freq
    };

    Ok(Statistics {
        block_sample_count,
        decimate_factor,
        sample_freq,
        block_sample_id: u64::from_le_bytes(payload[8..16].try_into().expect("checked above")),
        accum_sample_id: u64::from_le_bytes(payload[16..24].try_into().expect("checked above")),
        current: SignalStats {
            avg: i_x1 as f64 * avg_iv,
            std: std_dev(i_x1, i_x2, n, Q_IV),
            min: rd_i64(payload, 32) as f64 * mm_iv,
            max: rd_i64(payload, 40) as f64 * mm_iv,
        },
        voltage: SignalStats {
            avg: v_x1 as f64 * avg_iv,
            std: std_dev(v_x1, v_x2, n, Q_IV),
            min: rd_i64(payload, 56) as f64 * mm_iv,
            max: rd_i64(payload, 64) as f64 * mm_iv,
        },
        power: SignalStats {
            avg: p_x1 as f64 * avg_p,
            std: std_dev(p_x1, p_x2, n, Q_P),
            min: rd_i64(payload, 80) as f64 * mm_p,
            max: rd_i64(payload, 88) as f64 * mm_p,
        },
        charge: q_to_f64(rd_i128(payload, 112), Q_INT) * integral_scale,
        energy: q_to_f64(rd_i128(payload, 176), Q_INT) * integral_scale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Build a record from physical values, mirroring what the sensor would send.
    struct Builder {
        buf: Vec<u8>,
    }

    impl Builder {
        fn new(block_sample_count: u32, sample_freq: u32) -> Self {
            let mut buf = vec![0u8; RECORD_LEN];
            let header = (HEADER_TAG << 24) | (block_sample_count & 0x00ff_ffff);
            buf[0..4].copy_from_slice(&header.to_le_bytes());
            buf[4..8].copy_from_slice(&sample_freq.to_le_bytes());
            Self { buf }
        }

        fn i64_at(mut self, off: usize, v: i64) -> Self {
            self.buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
            self
        }

        fn i128_at(mut self, off: usize, v: i128) -> Self {
            self.buf[off..off + 16].copy_from_slice(&v.to_le_bytes());
            self
        }

        fn build(self) -> Vec<u8> {
            self.buf
        }
    }

    fn q31(v: f64) -> i64 {
        (v * (1u64 << 31) as f64).round() as i64
    }

    fn q27(v: f64) -> i64 {
        (v * (1u64 << 27) as f64).round() as i64
    }

    #[test]
    fn record_length_matches_the_documented_layout() {
        // 2 u32 + 2 u64 + 9 i64 + 6 i128
        assert_eq!(RECORD_LEN, 8 + 16 + 9 * 8 + 6 * 16);
    }

    #[test]
    fn averages_scale_by_block_count_and_q_factor() {
        // 1000 samples averaging 0.25 A, 5 V, 1.25 W.
        let n = 1000i64;
        let rec = Builder::new(n as u32, 2_000_000)
            .i64_at(24, q31(0.25) * n)
            .i64_at(48, q31(5.0) * n)
            .i64_at(72, q27(1.25) * n)
            .build();
        let s = decode(&rec).unwrap();
        assert!((s.current.avg - 0.25).abs() < 1e-9, "got {}", s.current.avg);
        assert!((s.voltage.avg - 5.0).abs() < 1e-9, "got {}", s.voltage.avg);
        assert!((s.power.avg - 1.25).abs() < 1e-9, "got {}", s.power.avg);
    }

    #[test]
    fn current_and_voltage_use_q31_while_power_uses_q27() {
        let rec = Builder::new(1, 1_000_000)
            .i64_at(32, q31(-0.5))
            .i64_at(40, q31(1.5))
            .i64_at(80, q27(-2.5))
            .i64_at(88, q27(3.5))
            .build();
        let s = decode(&rec).unwrap();
        assert!((s.current.min + 0.5).abs() < 1e-9);
        assert!((s.current.max - 1.5).abs() < 1e-9);
        assert!((s.power.min + 2.5).abs() < 1e-9);
        assert!((s.power.max - 3.5).abs() < 1e-9);
    }

    #[test]
    fn std_dev_of_a_constant_signal_is_zero() {
        let n = 100i64;
        let x = q31(0.25);
        let rec = Builder::new(n as u32, 2_000_000)
            .i64_at(24, x * n)
            .i128_at(96, (x as i128) * (x as i128) * (n as i128))
            .build();
        let s = decode(&rec).unwrap();
        assert!(s.current.std < 1e-9, "got {}", s.current.std);
    }

    #[test]
    fn std_dev_matches_a_two_level_signal() {
        // Half the samples at 0 A, half at 1 A: mean 0.5, population std 0.5.
        let n = 1000i128;
        let one = q31(1.0) as i128;
        let rec = Builder::new(n as u32, 2_000_000)
            .i64_at(24, (one * n / 2) as i64)
            .i128_at(96, one * one * (n / 2))
            .build();
        let s = decode(&rec).unwrap();
        assert!((s.current.avg - 0.5).abs() < 1e-9, "avg {}", s.current.avg);
        assert!((s.current.std - 0.5).abs() < 1e-6, "std {}", s.current.std);
    }

    #[test]
    fn negative_variance_from_rounding_clamps_to_zero() {
        let rec = Builder::new(10, 2_000_000)
            .i64_at(24, q31(1.0) * 10)
            .i128_at(96, 0)
            .build();
        let s = decode(&rec).unwrap();
        assert_eq!(s.current.std, 0.0);
    }

    #[test]
    fn charge_and_energy_divide_by_the_decimated_rate() {
        // 1 A integrated for 1_000_000 decimated samples at 1 Msps = 1 C.
        let rate = 1_000_000i128;
        let rec = Builder::new(1000, 2_000_000)
            .i128_at(112, (q31(1.0) as i128) * rate)
            .i128_at(176, (q27(0.0) as i128) * rate)
            .build();
        let s = decode(&rec).unwrap();
        assert_eq!(s.decimate_factor, 2);
        assert!((s.decimated_rate() - 1_000_000.0).abs() < 1e-6);
        assert!((s.charge - 1.0).abs() < 1e-6, "charge {}", s.charge);
    }

    #[test]
    fn block_duration_follows_the_decimated_rate() {
        let rec = Builder::new(500_000, 2_000_000).build();
        let s = decode(&rec).unwrap();
        assert!((s.block_duration() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn short_record_is_rejected() {
        assert_eq!(
            decode(&[0u8; 100]).unwrap_err(),
            StatsError::Short { have: 100 }
        );
    }

    #[test]
    fn wrong_header_tag_is_rejected() {
        let mut rec = Builder::new(10, 2_000_000).build();
        rec[3] = 0x11;
        assert!(matches!(
            decode(&rec).unwrap_err(),
            StatsError::BadHeader { .. }
        ));
    }

    #[test]
    fn empty_block_is_rejected_rather_than_dividing_by_zero() {
        let rec = Builder::new(0, 2_000_000).build();
        assert_eq!(decode(&rec).unwrap_err(), StatsError::EmptyBlock);
    }
}
