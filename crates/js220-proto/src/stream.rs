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
    /// 4-bit values, two to a byte, low nibble first.
    U4,
    /// 1-bit values, eight to a byte, least significant bit first.
    U1,
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
    port(
        "s/adc/0/ctrl",
        "s/adc/0/!data",
        Signal::Adc(0),
        Encoding::I16,
        1,
    ),
    port(
        "s/adc/1/ctrl",
        "s/adc/1/!data",
        Signal::Adc(1),
        Encoding::I16,
        1,
    ),
    port(
        "s/adc/2/ctrl",
        "s/adc/2/!data",
        Signal::Adc(2),
        Encoding::I16,
        1,
    ),
    port(
        "s/adc/3/ctrl",
        "s/adc/3/!data",
        Signal::Adc(3),
        Encoding::I16,
        1,
    ),
    port(
        "s/i/range/ctrl",
        "s/i/range/!data",
        Signal::CurrentRange,
        Encoding::U4,
        1,
    ),
    port("s/i/ctrl", "s/i/!data", Signal::Current, Encoding::F32, 2),
    port("s/v/ctrl", "s/v/!data", Signal::Voltage, Encoding::F32, 2),
    port("s/p/ctrl", "s/p/!data", Signal::Power, Encoding::F32, 2),
    port(
        "s/gpi/0/ctrl",
        "s/gpi/0/!data",
        Signal::Gpi(0),
        Encoding::U1,
        1,
    ),
    port(
        "s/gpi/1/ctrl",
        "s/gpi/1/!data",
        Signal::Gpi(1),
        Encoding::U1,
        1,
    ),
    port(
        "s/gpi/2/ctrl",
        "s/gpi/2/!data",
        Signal::Gpi(2),
        Encoding::U1,
        1,
    ),
    port(
        "s/gpi/3/ctrl",
        "s/gpi/3/!data",
        Signal::Gpi(3),
        Encoding::U1,
        1,
    ),
    port(
        "s/gpi/7/ctrl",
        "s/gpi/7/!data",
        Signal::Gpi(7),
        Encoding::U1,
        1,
    ),
    port(
        "s/uart/0/ctrl",
        "s/uart/0/!data",
        Signal::Uart(0),
        Encoding::U8,
        1,
    ),
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
/// Stream index of the first general purpose input.
pub const IDX_GPI_FIRST: u8 = 8;
/// Stream index of the last general purpose input.
pub const IDX_GPI_LAST: u8 = 12;

/// General purpose input channels the instrument exposes, in port order.
///
/// Channels 0 through 3 are the external inputs; 7 is the trigger input. The
/// numbering is the instrument's own and is not contiguous.
pub const GPI_CHANNELS: [u8; 5] = [0, 1, 2, 3, 7];

/// Stream index carrying general purpose input `channel`.
pub fn gpi_index(channel: u8) -> Option<u8> {
    GPI_CHANNELS
        .iter()
        .position(|c| *c == channel)
        .map(|i| IDX_GPI_FIRST + i as u8)
}

/// The general purpose input channel a stream index carries.
pub fn gpi_channel(index: u8) -> Option<u8> {
    if !(IDX_GPI_FIRST..=IDX_GPI_LAST).contains(&index) {
        return None;
    }
    GPI_CHANNELS.get((index - IDX_GPI_FIRST) as usize).copied()
}

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
        out.push(f32::from_le_bytes(
            chunk.try_into().expect("chunks_exact(4)"),
        ));
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
        out.push(i16::from_le_bytes(
            chunk.try_into().expect("chunks_exact(2)"),
        ));
    }
    n
}

/// Samples packed into one byte, for a sub-byte element size.
pub const fn samples_per_byte(bits: u8) -> usize {
    (8 / bits) as usize
}

/// Number of samples a sub-byte payload carries.
///
/// The instrument sizes its frames in bytes, so the count follows from the
/// payload length and the element width rather than being carried explicitly:
/// `(bytes * 8) / bits`.
pub const fn packed_sample_count(bytes: usize, bits: u8) -> usize {
    (bytes * 8) / bits as usize
}

/// Unpack the 1-bit stream carried by the GPI ports.
///
/// Eight samples to a byte, least significant bit first, which is the order
/// the instrument's own host driver reads them in. Each sample appends a 0 or
/// a 1 to `out`.
///
/// There is no run-length encoding here. The port map declares these as
/// 1-bit unsigned elements and the frame carries them packed, so a payload
/// length alone fixes the sample count.
pub fn decode_u1_packed(data: &[u8], out: &mut Vec<u8>) -> usize {
    out.reserve(data.len() * 8);
    for byte in data {
        for bit in 0..8 {
            out.push((byte >> bit) & 1);
        }
    }
    data.len() * 8
}

/// Unpack the 4-bit stream carried by the current-range port.
///
/// Two samples to a byte, low nibble first, matching the bit order of
/// [`decode_u1_packed`].
pub fn decode_u4_packed(data: &[u8], out: &mut Vec<u8>) -> usize {
    out.reserve(data.len() * 2);
    for byte in data {
        out.push(byte & 0x0f);
        out.push(byte >> 4);
    }
    data.len() * 2
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
    fn u1_unpacks_eight_samples_a_byte_least_significant_first() {
        let mut out = Vec::new();
        assert_eq!(decode_u1_packed(&[0b0101_0101], &mut out), 8);
        assert_eq!(out, vec![1, 0, 1, 0, 1, 0, 1, 0]);
    }

    #[test]
    fn u1_keeps_byte_order_across_a_payload() {
        // A line that goes high for one sample must come back at exactly the
        // sample it went high at, whichever byte it lands in.
        let mut out = Vec::new();
        decode_u1_packed(&[0x00, 0b0000_1000], &mut out);
        assert_eq!(out.len(), 16);
        assert_eq!(out.iter().position(|&v| v == 1), Some(11));
    }

    #[test]
    fn u1_of_nothing_is_nothing() {
        let mut out = Vec::new();
        assert_eq!(decode_u1_packed(&[], &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn u4_unpacks_two_samples_a_byte_low_nibble_first() {
        let mut out = Vec::new();
        assert_eq!(decode_u4_packed(&[0x53], &mut out), 2);
        assert_eq!(out, vec![3, 5]);
    }

    #[test]
    fn the_sample_count_follows_from_the_payload_length() {
        // The instrument carries no explicit count: the frame length and the
        // element width fix it.
        assert_eq!(packed_sample_count(1, 1), 8);
        assert_eq!(packed_sample_count(64, 1), 512);
        assert_eq!(packed_sample_count(1, 4), 2);
        assert_eq!(packed_sample_count(0, 1), 0);
        assert_eq!(samples_per_byte(1), 8);
        assert_eq!(samples_per_byte(4), 2);
    }

    #[test]
    fn a_decoded_payload_is_as_long_as_the_count_predicts() {
        for len in [0usize, 1, 7, 64, 255] {
            let data = vec![0xa5u8; len];
            let mut out = Vec::new();
            assert_eq!(
                decode_u1_packed(&data, &mut out),
                packed_sample_count(len, 1)
            );
            assert_eq!(out.len(), packed_sample_count(len, 1));

            let mut out4 = Vec::new();
            assert_eq!(
                decode_u4_packed(&data, &mut out4),
                packed_sample_count(len, 4)
            );
            assert_eq!(out4.len(), packed_sample_count(len, 4));
        }
    }

    #[test]
    fn gpi_channels_and_stream_indices_agree_with_the_port_map() {
        for &channel in &GPI_CHANNELS {
            let index = gpi_index(channel).expect("every listed channel has a port");
            assert_eq!(gpi_channel(index), Some(channel), "round trip");
            assert_eq!(PORT_MAP[index as usize].signal, Signal::Gpi(channel));
            assert_eq!(
                PORT_MAP[index as usize].ctrl_topic,
                crate::topics::gpi_ctrl(channel)
            );
        }
    }

    #[test]
    fn channels_the_instrument_does_not_have_map_to_nothing() {
        // 4, 5 and 6 look plausible and are not there.
        for channel in [4u8, 5, 6, 8, 255] {
            assert_eq!(gpi_index(channel), None, "channel {channel}");
        }
        assert_eq!(gpi_channel(IDX_GPI_FIRST - 1), None);
        assert_eq!(gpi_channel(IDX_GPI_LAST + 1), None);
    }

    #[test]
    fn the_gpi_and_range_ports_declare_the_packed_encodings() {
        for (offset, port) in PORT_MAP
            .iter()
            .enumerate()
            .skip(IDX_GPI_FIRST as usize)
            .take(GPI_CHANNELS.len())
        {
            assert_eq!(port.encoding, Encoding::U1, "port {offset}");
            assert_eq!(port.decimate_min, 1, "GPI runs at the base rate");
        }
        assert_eq!(PORT_MAP[4].encoding, Encoding::U4);
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
