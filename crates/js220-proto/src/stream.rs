//! Sample streaming ports (port 16 and above).
//!
//! Each streaming payload begins with a `u32` sample id followed by that
//! port's channel data. The sample id is always expressed at the 2 MHz base
//! rate regardless of the port's own output rate, and wraps roughly every
//! 2147 seconds; [`SampleIdUnwrap`] widens it back to a monotonic 64-bit
//! counter.

use alloc::vec::Vec;

/// Base sample rate of the sample id counter, in Hz.
pub const BASE_SAMPLE_RATE: u32 = 2_000_000;

/// Number of streaming ports.
pub const PORT_COUNT: usize = 16;

/// Size of the sample id prefix on every streaming payload.
pub const SAMPLE_ID_LEN: usize = 4;

/// The signal carried by a streaming port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Raw ADC counts for the given channel.
    Adc(u8),
    /// Current, in amperes.
    Current,
    /// Voltage, in volts.
    Voltage,
    /// Power, in watts.
    Power,
    /// Selected current range, 0..=5.
    CurrentRange,
    /// General purpose input; channel 7 is the trigger input.
    Gpi(u8),
    /// UART receive bytes.
    Uart(u8),
    /// On-instrument statistics records.
    Statistics,
    /// Reserved, unavailable.
    Reserved,
}

/// How a port's samples are represented on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// 32-bit IEEE 754 floats, uncompressed.
    F32,
    /// 16-bit signed integers, uncompressed.
    I16,
    /// 4-bit values, run-length encoded in 16-bit chunks.
    U4Rle,
    /// 1-bit values, run-length encoded in a byte-oriented scheme.
    U1Rle,
    /// 8-bit values, uncompressed.
    U8,
    /// A fixed-layout record rather than a sample sequence.
    Record,
}

/// Static description of one streaming port.
#[derive(Debug, Clone, Copy)]
pub struct PortDef {
    /// Topic that enables or disables the port.
    pub ctrl_topic: Option<&'static str>,
    /// Topic the device publishes data on.
    pub data_topic: Option<&'static str>,
    /// The signal this port carries.
    pub signal: Signal,
    /// The wire encoding.
    pub encoding: Encoding,
    /// Sample id increment per sample at the port's minimum decimation.
    ///
    /// Ports fixed at the 2 MHz base rate use 1; current, voltage and power
    /// top out at 1 Msps and so use 2.
    pub decimate_min: u32,
}

const fn port(
    ctrl_topic: &'static str,
    data_topic: &'static str,
    signal: Signal,
    encoding: Encoding,
    decimate_min: u32,
) -> PortDef {
    PortDef {
        ctrl_topic: Some(ctrl_topic),
        data_topic: Some(data_topic),
        signal,
        encoding,
        decimate_min,
    }
}

/// Description of every streaming port, indexed by `port_id - 16`.
pub static PORT_MAP: [PortDef; PORT_COUNT] = [
    port("s/adc/0/ctrl", "s/adc/0/!data", Signal::Adc(0), Encoding::I16, 1),
    port("s/adc/1/ctrl", "s/adc/1/!data", Signal::Adc(1), Encoding::I16, 1),
    port("s/adc/2/ctrl", "s/adc/2/!data", Signal::Adc(2), Encoding::I16, 1),
    port("s/adc/3/ctrl", "s/adc/3/!data", Signal::Adc(3), Encoding::I16, 1),
    port(
        "s/i/range/ctrl",
        "s/i/range/!data",
        Signal::CurrentRange,
        Encoding::U4Rle,
        1,
    ),
    port("s/i/ctrl", "s/i/!data", Signal::Current, Encoding::F32, 2),
    port("s/v/ctrl", "s/v/!data", Signal::Voltage, Encoding::F32, 2),
    port("s/p/ctrl", "s/p/!data", Signal::Power, Encoding::F32, 2),
    port("s/gpi/0/ctrl", "s/gpi/0/!data", Signal::Gpi(0), Encoding::U1Rle, 1),
    port("s/gpi/1/ctrl", "s/gpi/1/!data", Signal::Gpi(1), Encoding::U1Rle, 1),
    port("s/gpi/2/ctrl", "s/gpi/2/!data", Signal::Gpi(2), Encoding::U1Rle, 1),
    port("s/gpi/3/ctrl", "s/gpi/3/!data", Signal::Gpi(3), Encoding::U1Rle, 1),
    port("s/gpi/7/ctrl", "s/gpi/7/!data", Signal::Gpi(7), Encoding::U1Rle, 1),
    port("s/uart/0/ctrl", "s/uart/0/!data", Signal::Uart(0), Encoding::U8, 1),
    port(
        "s/stats/ctrl",
        "s/stats/value",
        Signal::Statistics,
        Encoding::Record,
        0,
    ),
    PortDef {
        ctrl_topic: None,
        data_topic: None,
        signal: Signal::Reserved,
        encoding: Encoding::Record,
        decimate_min: 0,
    },
];

/// Stream index of the current signal.
pub const IDX_CURRENT: u8 = 5;
/// Stream index of the voltage signal.
pub const IDX_VOLTAGE: u8 = 6;
/// Stream index of the power signal.
pub const IDX_POWER: u8 = 7;
/// Stream index of the UART signal.
pub const IDX_UART: u8 = 13;
/// Stream index of the statistics records.
pub const IDX_STATISTICS: u8 = 14;

/// Look up a port definition by stream index (`port_id - 16`).
pub fn port_def(index: u8) -> Option<&'static PortDef> {
    PORT_MAP.get(index as usize)
}

/// Split a streaming payload into its sample id and channel data.
pub fn split_sample_id(payload: &[u8]) -> Option<(u32, &[u8])> {
    if payload.len() < SAMPLE_ID_LEN {
        return None;
    }
    let (head, rest) = payload.split_at(SAMPLE_ID_LEN);
    Some((
        u32::from_le_bytes(head.try_into().expect("split at 4")),
        rest,
    ))
}

/// Widens the device's 32-bit sample id into a monotonic 64-bit counter.
///
/// The device counts internally in 56 bits but only transmits the low 32, so
/// the host must track wraparound. Differences are interpreted as signed, which
/// tolerates frames arriving slightly out of order as well as the wrap itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct SampleIdUnwrap {
    last: u64,
    started: bool,
}

impl SampleIdUnwrap {
    /// A fresh unwrapper that has not yet seen a sample id.
    pub const fn new() -> Self {
        Self {
            last: 0,
            started: false,
        }
    }

    /// Reset to the uninitialized state.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The most recently produced 64-bit sample id.
    pub const fn last(&self) -> u64 {
        self.last
    }

    /// Widen `raw` using the running state.
    ///
    /// The first call anchors the counter at `raw`. Subsequent calls add the
    /// signed 32-bit difference from the previous value, so a counter that
    /// wraps from `0xffff_fff0` to `0x10` advances by 32 rather than jumping
    /// backwards by four billion.
    pub fn update(&mut self, raw: u32) -> u64 {
        if !self.started {
            self.started = true;
            self.last = raw as u64;
            return self.last;
        }
        let delta = (raw.wrapping_sub(self.last as u32)) as i32;
        self.last = self.last.wrapping_add_signed(delta as i64);
        self.last
    }
}

/// Decode an uncompressed `f32` sample payload, appending to `out`.
///
/// Trailing bytes that do not form a whole sample are ignored. Returns the
/// number of samples appended.
pub fn decode_f32(data: &[u8], out: &mut Vec<f32>) -> usize {
    let n = data.len() / 4;
    out.reserve(n);
    for chunk in data.chunks_exact(4) {
        out.push(f32::from_le_bytes(chunk.try_into().expect("chunks_exact(4)")));
    }
    n
}

/// Decode an uncompressed `i16` sample payload, appending to `out`.
///
/// Returns the number of samples appended.
pub fn decode_i16(data: &[u8], out: &mut Vec<i16>) -> usize {
    let n = data.len() / 2;
    out.reserve(n);
    for chunk in data.chunks_exact(2) {
        out.push(i16::from_le_bytes(chunk.try_into().expect("chunks_exact(2)")));
    }
    n
}

/// Largest run a single `u4` run-length chunk can express.
pub const U4_RUN_MAX: u16 = 4096;

/// Decode the run-length encoded 4-bit current range stream.
///
/// Each 16-bit little-endian chunk is `zzzzzzzz_zzzzxxxx`: the low nibble is
/// the value and the upper 12 bits are the run length minus one, giving runs of
/// 1 to 4096 samples.
///
/// Note: current JS220 firmware ships this stream uncompressed, so this path is
/// exercised by the unit tests below rather than by hardware.
pub fn decode_u4_rle(data: &[u8], out: &mut Vec<u8>) -> usize {
    let mut n = 0;
    for chunk in data.chunks_exact(2) {
        let word = u16::from_le_bytes(chunk.try_into().expect("chunks_exact(2)"));
        let value = (word & 0x000f) as u8;
        let run = (word >> 4) as usize + 1;
        out.resize(out.len() + run, value);
        n += run;
    }
    n
}

/// Errors from the 1-bit run-length decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum U1Error {
    /// A long-run marker was cut off before its second byte.
    Truncated,
    /// The reserved `111` prefix was encountered.
    Reserved {
        /// The offending byte.
        byte: u8,
    },
}

impl core::fmt::Display for U1Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "u1 long-run marker truncated"),
            Self::Reserved { byte } => write!(f, "u1 reserved prefix in byte {byte:#04x}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for U1Error {}

/// Decode the run-length encoded 1-bit stream used by GPI and voltage range.
///
/// Three token forms, distinguished by their high bits:
///
/// ```text
/// 0xxxxxxx           seven literal samples, least significant bit first
/// 10xzzzzz           run of (z + 8) samples of value x, 8..=39
/// 110xzzzz zzzzzzzz  run of (z + 40) samples of value x, 40..=4135
/// ```
///
/// Note: current JS220 firmware ships these streams uncompressed, so this path
/// is exercised by the unit tests below rather than by hardware. The bit order
/// within a literal token follows the little-endian convention used everywhere
/// else in this protocol, but has not been confirmed against a device.
pub fn decode_u1_rle(data: &[u8], out: &mut Vec<u8>) -> Result<usize, U1Error> {
    let mut n = 0;
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b & 0x80 == 0 {
            // Literal: seven samples packed into the low bits.
            for bit in 0..7 {
                out.push((b >> bit) & 1);
            }
            n += 7;
            i += 1;
        } else if b & 0x40 == 0 {
            // 10xzzzzz
            let value = (b >> 5) & 1;
            let run = (b & 0x1f) as usize + 8;
            out.resize(out.len() + run, value);
            n += run;
            i += 1;
        } else if b & 0x20 == 0 {
            // 110xzzzz zzzzzzzz
            let Some(&lo) = data.get(i + 1) else {
                return Err(U1Error::Truncated);
            };
            let value = (b >> 4) & 1;
            let z = (((b & 0x0f) as usize) << 8) | lo as usize;
            let run = z + 40;
            out.resize(out.len() + run, value);
            n += run;
            i += 2;
        } else {
            return Err(U1Error::Reserved { byte: b });
        }
    }
    Ok(n)
}

/// Multiply current and voltage sample-wise into power.
///
/// Written as a slice-to-slice loop over a common length so the compiler can
/// elide bounds checks and vectorize the multiply.
pub fn compute_power(current: &[f32], voltage: &[f32], out: &mut Vec<f32>) -> usize {
    let n = current.len().min(voltage.len());
    out.reserve(n);
    for (i, v) in current[..n].iter().zip(&voltage[..n]) {
        out.push(i * v);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn port_map_matches_the_documented_assignments() {
        assert_eq!(PORT_MAP[IDX_CURRENT as usize].signal, Signal::Current);
        assert_eq!(PORT_MAP[IDX_VOLTAGE as usize].signal, Signal::Voltage);
        assert_eq!(PORT_MAP[IDX_POWER as usize].signal, Signal::Power);
        assert_eq!(PORT_MAP[IDX_STATISTICS as usize].signal, Signal::Statistics);
        assert_eq!(PORT_MAP[0].signal, Signal::Adc(0));
        assert_eq!(PORT_MAP[12].signal, Signal::Gpi(7));
        assert_eq!(PORT_MAP[15].signal, Signal::Reserved);
    }

    #[test]
    fn ivp_ports_decimate_by_two_others_by_one() {
        assert_eq!(PORT_MAP[IDX_CURRENT as usize].decimate_min, 2);
        assert_eq!(PORT_MAP[IDX_VOLTAGE as usize].decimate_min, 2);
        assert_eq!(PORT_MAP[IDX_POWER as usize].decimate_min, 2);
        assert_eq!(PORT_MAP[0].decimate_min, 1);
    }

    #[test]
    fn reserved_port_has_no_topics() {
        assert!(PORT_MAP[15].ctrl_topic.is_none());
        assert!(PORT_MAP[15].data_topic.is_none());
    }

    #[test]
    fn port_def_is_bounded() {
        assert!(port_def(0).is_some());
        assert!(port_def(15).is_some());
        assert!(port_def(16).is_none());
    }

    #[test]
    fn sample_id_is_split_off_the_front() {
        let payload = [0x78, 0x56, 0x34, 0x12, 0xaa, 0xbb];
        let (id, rest) = split_sample_id(&payload).unwrap();
        assert_eq!(id, 0x1234_5678);
        assert_eq!(rest, &[0xaa, 0xbb]);
    }

    #[test]
    fn split_sample_id_rejects_a_runt_payload() {
        assert!(split_sample_id(&[1, 2, 3]).is_none());
    }

    #[test]
    fn unwrap_anchors_on_the_first_value() {
        let mut u = SampleIdUnwrap::new();
        assert_eq!(u.update(12345), 12345);
        assert_eq!(u.last(), 12345);
    }

    #[test]
    fn unwrap_advances_monotonically() {
        let mut u = SampleIdUnwrap::new();
        u.update(100);
        assert_eq!(u.update(200), 200);
        assert_eq!(u.update(1000), 1000);
    }

    #[test]
    fn unwrap_carries_across_the_32_bit_boundary() {
        let mut u = SampleIdUnwrap::new();
        u.update(0xffff_fff0);
        let got = u.update(0x10);
        assert_eq!(got, 0x1_0000_0010, "expected a carry, got {got:#x}");
    }

    #[test]
    fn unwrap_survives_many_consecutive_wraps() {
        let mut u = SampleIdUnwrap::new();
        u.update(0);
        let step = 0x4000_0000u32;
        let mut expect = 0u64;
        for i in 1..=12u32 {
            expect += step as u64;
            assert_eq!(u.update(i.wrapping_mul(step)), expect);
        }
    }

    #[test]
    fn unwrap_treats_a_small_backward_step_as_reordering() {
        let mut u = SampleIdUnwrap::new();
        u.update(1000);
        assert_eq!(u.update(990), 990);
    }

    #[test]
    fn unwrap_reset_reanchors() {
        let mut u = SampleIdUnwrap::new();
        u.update(0xffff_0000);
        u.reset();
        assert_eq!(u.update(5), 5);
    }

    #[test]
    fn f32_samples_decode_little_endian() {
        let mut data = Vec::new();
        for v in [1.0f32, -2.5, 3.25] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        assert_eq!(decode_f32(&data, &mut out), 3);
        assert_eq!(out, vec![1.0, -2.5, 3.25]);
    }

    #[test]
    fn f32_decode_ignores_a_partial_trailing_sample() {
        let mut out = Vec::new();
        assert_eq!(decode_f32(&[0, 0, 0, 0, 1, 2], &mut out), 1);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn i16_samples_decode_signed() {
        let mut out = Vec::new();
        decode_i16(&[0xff, 0xff, 0x00, 0x80], &mut out);
        assert_eq!(out, vec![-1i16, i16::MIN]);
    }

    #[test]
    fn u4_rle_expands_runs() {
        // Chunks are `zzzzzzzz_zzzzxxxx`: run length minus one, then value.
        let chunk = |value: u16, run: u16| (value | ((run - 1) << 4)).to_le_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&chunk(3, 1));
        data.extend_from_slice(&chunk(5, 4096));
        let mut out = Vec::new();
        let n = decode_u4_rle(&data, &mut out);
        assert_eq!(n, 1 + 4096);
        assert_eq!(out[0], 3);
        assert!(out[1..].iter().all(|&v| v == 5));
    }

    #[test]
    fn u4_rle_run_length_bounds_match_the_spec() {
        let mut out = Vec::new();
        decode_u4_rle(&(0u16).to_le_bytes(), &mut out);
        assert_eq!(out.len(), 1, "minimum run is 1");
        out.clear();
        decode_u4_rle(&(0xfff0u16).to_le_bytes(), &mut out);
        assert_eq!(out.len(), U4_RUN_MAX as usize, "maximum run is 4096");
    }

    #[test]
    fn u1_literal_unpacks_seven_bits_lsb_first() {
        let mut out = Vec::new();
        // 0b0_1010101 -> 1,0,1,0,1,0,1
        assert_eq!(decode_u1_rle(&[0b0101_0101], &mut out).unwrap(), 7);
        assert_eq!(out, vec![1, 0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn u1_short_run_covers_eight_to_thirty_nine() {
        let mut out = Vec::new();
        // 10 x=1 z=0 -> eight ones
        decode_u1_rle(&[0b1010_0000], &mut out).unwrap();
        assert_eq!(out, vec![1; 8]);

        out.clear();
        // 10 x=0 z=31 -> thirty-nine zeros
        decode_u1_rle(&[0b1001_1111], &mut out).unwrap();
        assert_eq!(out, vec![0; 39]);
    }

    #[test]
    fn u1_long_run_covers_forty_to_four_thousand_one_hundred_thirty_five() {
        let mut out = Vec::new();
        // 110 x=1 z=0 -> forty ones
        decode_u1_rle(&[0b1101_0000, 0x00], &mut out).unwrap();
        assert_eq!(out, vec![1; 40]);

        out.clear();
        // 110 x=0 z=4095 -> 4135 zeros
        decode_u1_rle(&[0b1100_1111, 0xff], &mut out).unwrap();
        assert_eq!(out.len(), 4135);
        assert!(out.iter().all(|&v| v == 0));
    }

    #[test]
    fn u1_mixed_tokens_concatenate() {
        let mut out = Vec::new();
        let n = decode_u1_rle(&[0b0000_0001, 0b1010_0000], &mut out).unwrap();
        assert_eq!(n, 7 + 8);
        assert_eq!(out[0], 1);
        assert!(out[1..7].iter().all(|&v| v == 0));
        assert!(out[7..].iter().all(|&v| v == 1));
    }

    #[test]
    fn u1_truncated_long_run_is_an_error() {
        let mut out = Vec::new();
        assert_eq!(
            decode_u1_rle(&[0b1101_0000], &mut out).unwrap_err(),
            U1Error::Truncated
        );
    }

    #[test]
    fn u1_reserved_prefix_is_an_error() {
        let mut out = Vec::new();
        assert_eq!(
            decode_u1_rle(&[0b1110_0000], &mut out).unwrap_err(),
            U1Error::Reserved { byte: 0b1110_0000 }
        );
    }

    #[test]
    fn power_is_the_sample_wise_product() {
        let mut out = Vec::new();
        let n = compute_power(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0], &mut out);
        assert_eq!(n, 3);
        assert_eq!(out, vec![4.0, 10.0, 18.0]);
    }

    #[test]
    fn power_stops_at_the_shorter_input() {
        let mut out = Vec::new();
        assert_eq!(compute_power(&[1.0, 2.0], &[3.0], &mut out), 1);
        assert_eq!(out, vec![3.0]);
    }
}
