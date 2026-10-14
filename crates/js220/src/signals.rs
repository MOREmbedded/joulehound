//! Per-signal accumulation and alignment.
//!
//! Current, voltage and power arrive on separate streaming ports, in separate
//! frames, and the device does not promise they arrive together. Each frame
//! carries the sample id of its first sample, so the host can place every
//! signal on one timeline and hand out only the span where all of the requested
//! signals are present.
//!
//! Gaps are filled with NaN rather than silently closed, so a dropped frame
//! shifts nothing downstream: row `n` always corresponds to the same instant on
//! every signal.

use js220_proto::stream::SampleIdUnwrap;

/// The three aligned analog signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// Current, in amperes.
    Current,
    /// Voltage, in volts.
    Voltage,
    /// Power, in watts.
    Power,
}

impl Signal {
    /// Every alignable signal, in column order.
    pub const ALL: [Signal; 3] = [Signal::Current, Signal::Voltage, Signal::Power];

    /// Index into a [`SignalGroup`]'s channel array.
    pub const fn index(self) -> usize {
        match self {
            Self::Current => 0,
            Self::Voltage => 1,
            Self::Power => 2,
        }
    }

    /// Short lower-case name, used for CSV headers and CLI arguments.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Voltage => "voltage",
            Self::Power => "power",
        }
    }

    /// SI unit symbol.
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Current => "A",
            Self::Voltage => "V",
            Self::Power => "W",
        }
    }

    /// Parse a signal name, accepting both the long and single-letter forms.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "i" | "current" => Self::Current,
            "v" | "voltage" => Self::Voltage,
            "p" | "power" => Self::Power,
            _ => return None,
        })
    }
}

/// Counters describing how cleanly a signal has streamed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SignalCounters {
    /// Samples appended from the device.
    pub samples: u64,
    /// Samples synthesized as NaN to cover a gap in the sample id sequence.
    pub filled: u64,
    /// Samples discarded because they repeated data already accepted.
    pub skipped: u64,
    /// Frames whose sample id ran backwards past data already delivered.
    pub discontinuities: u64,
}

#[derive(Debug, Default)]
struct Channel {
    data: Vec<f32>,
    unwrap: SampleIdUnwrap,
    /// Sample id the next appended sample will carry.
    next_id: u64,
    primed: bool,
    counters: SignalCounters,
}

/// Accumulates the analog signals onto a shared timeline.
#[derive(Debug)]
pub struct SignalGroup {
    channels: [Option<Channel>; 3],
    /// Sample id corresponding to index 0 of every channel buffer.
    base: Option<u64>,
    /// Sample id increment between consecutive samples.
    decimate: u32,
    /// Largest gap that will be bridged with NaN before the group re-anchors.
    max_fill: usize,
    /// Times the group re-anchored because a gap exceeded `max_fill`.
    resyncs: u64,
}

impl SignalGroup {
    /// Create a group for `enabled`, where consecutive samples are `decimate`
    /// sample ids apart.
    ///
    /// `decimate` of zero is treated as one so a misconfigured rate cannot
    /// cause a division by zero on the hot path.
    ///
    /// `max_fill` bounds how many samples will be synthesized to bridge a gap.
    /// The fill count comes from sample ids the device reports, so leaving it
    /// unbounded lets one stale or corrupt frame allocate arbitrarily much
    /// memory. A gap larger than this is treated as a discontinuity: the
    /// timeline re-anchors on the new sample id instead.
    pub fn new(enabled: &[Signal], decimate: u32, max_fill: usize) -> Self {
        let mut channels: [Option<Channel>; 3] = [None, None, None];
        for s in enabled {
            channels[s.index()] = Some(Channel::default());
        }
        Self {
            channels,
            base: None,
            decimate: decimate.max(1),
            max_fill,
            resyncs: 0,
        }
    }

    /// How many times the timeline re-anchored after an oversized gap.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Drop all buffered data and re-anchor the shared timeline at `id`.
    fn resync(&mut self, id: u64) {
        for channel in self.channels.iter_mut().flatten() {
            channel.data.clear();
            channel.next_id = id;
            channel.primed = true;
            channel.counters.discontinuities += 1;
        }
        self.base = Some(id);
        self.resyncs += 1;
    }

    /// The signals this group is accumulating.
    pub fn enabled(&self) -> impl Iterator<Item = Signal> + '_ {
        Signal::ALL
            .into_iter()
            .filter(move |s| self.channels[s.index()].is_some())
    }

    /// Sample id of the first row currently buffered.
    pub fn base_sample_id(&self) -> Option<u64> {
        self.base
    }

    /// Sample id spacing between consecutive rows.
    pub fn decimate(&self) -> u32 {
        self.decimate
    }

    /// Counters for one signal, or `None` if it is not enabled.
    pub fn counters(&self, signal: Signal) -> Option<SignalCounters> {
        self.channels[signal.index()]
            .as_ref()
            .map(|c| c.counters)
    }

    /// Append a frame's worth of samples for `signal`.
    ///
    /// `raw_id` is the 32-bit sample id from the frame; it is widened
    /// internally. Samples that repeat already-delivered data are dropped, and
    /// a forward jump is covered with NaN so alignment is preserved.
    pub fn push(&mut self, signal: Signal, raw_id: u32, samples: &[f32]) {
        let decimate = self.decimate as u64;
        let index = signal.index();
        let Some(channel) = self.channels[index].as_mut() else {
            return;
        };
        let id = channel.unwrap.update(raw_id);

        // The first frame on any channel anchors the whole group's timeline.
        let base = *self.base.get_or_insert(id);

        if !channel.primed {
            channel.primed = true;
            channel.next_id = base;
        }

        // A gap far larger than any real dropout means the timeline no longer
        // relates to what the device is sending — a frame left over from a
        // previous session, or a counter that jumped. Re-anchor rather than
        // synthesizing millions of samples to bridge it.
        let next_id = channel.next_id;
        if id > next_id && (id - next_id) / decimate > self.max_fill as u64 {
            log::debug!(
                "{} sample id jumped from {next_id} to {id}; re-anchoring",
                signal.name()
            );
            self.resync(id);
        }

        let Some(channel) = self.channels[index].as_mut() else {
            return;
        };

        let mut samples = samples;
        let mut write_id = id;

        if write_id < channel.next_id {
            // Overlap: drop the portion we already have.
            let overlap = ((channel.next_id - write_id) / decimate) as usize;
            if overlap >= samples.len() {
                channel.counters.skipped += samples.len() as u64;
                channel.counters.discontinuities += 1;
                return;
            }
            channel.counters.skipped += overlap as u64;
            samples = &samples[overlap..];
            write_id = channel.next_id;
        } else if write_id > channel.next_id {
            // Gap: hold the timeline open with NaN.
            let missing = ((write_id - channel.next_id) / decimate) as usize;
            channel.data.resize(channel.data.len() + missing, f32::NAN);
            channel.counters.filled += missing as u64;
        }

        channel.data.extend_from_slice(samples);
        channel.counters.samples += samples.len() as u64;
        channel.next_id = write_id + samples.len() as u64 * decimate;
    }

    /// Number of rows for which every enabled signal has data.
    pub fn ready(&self) -> usize {
        let mut n = usize::MAX;
        let mut any = false;
        for channel in self.channels.iter().flatten() {
            any = true;
            n = n.min(channel.data.len());
        }
        if any {
            n
        } else {
            0
        }
    }

    /// Borrow the buffered samples for one signal.
    pub fn buffered(&self, signal: Signal) -> &[f32] {
        self.channels[signal.index()]
            .as_ref()
            .map_or(&[], |c| c.data.as_slice())
    }

    /// Drop the first `n` rows from every enabled signal and advance the base
    /// sample id. Call this after consuming what [`Self::ready`] reported.
    pub fn consume(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        for channel in self.channels.iter_mut().flatten() {
            let take = n.min(channel.data.len());
            channel.data.drain(..take);
        }
        if let Some(base) = self.base.as_mut() {
            *base += n as u64 * self.decimate as u64;
        }
    }

    /// Discard all buffered data and forget the timeline, so the next frame
    /// re-anchors the group. Counters are preserved.
    pub fn reset(&mut self) {
        for channel in self.channels.iter_mut().flatten() {
            channel.data.clear();
            channel.unwrap.reset();
            channel.primed = false;
            channel.next_id = 0;
        }
        self.base = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: u32 = 2; // 1 Msps: two sample ids per sample
    const MAX_FILL: usize = 1000;

    fn group() -> SignalGroup {
        SignalGroup::new(&[Signal::Current, Signal::Voltage], D, MAX_FILL)
    }

    #[test]
    fn signal_names_and_units_are_stable() {
        assert_eq!(Signal::Current.name(), "current");
        assert_eq!(Signal::Current.unit(), "A");
        assert_eq!(Signal::Power.unit(), "W");
    }

    #[test]
    fn signal_parsing_accepts_long_and_short_forms() {
        assert_eq!(Signal::parse("i"), Some(Signal::Current));
        assert_eq!(Signal::parse("CURRENT"), Some(Signal::Current));
        assert_eq!(Signal::parse(" v "), Some(Signal::Voltage));
        assert_eq!(Signal::parse("p"), Some(Signal::Power));
        assert_eq!(Signal::parse("x"), None);
    }

    #[test]
    fn indices_are_distinct_and_in_range() {
        let mut seen = [false; 3];
        for s in Signal::ALL {
            assert!(!seen[s.index()]);
            seen[s.index()] = true;
        }
    }

    #[test]
    fn nothing_is_ready_before_both_signals_arrive() {
        let mut g = group();
        g.push(Signal::Current, 1000, &[1.0, 2.0]);
        assert_eq!(g.ready(), 0, "voltage has not arrived yet");
        g.push(Signal::Voltage, 1000, &[3.0]);
        assert_eq!(g.ready(), 1, "limited by the shorter signal");
    }

    #[test]
    fn ready_tracks_the_shorter_signal() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0, 2.0, 3.0]);
        g.push(Signal::Voltage, 0, &[4.0, 5.0]);
        assert_eq!(g.ready(), 2);
        assert_eq!(g.buffered(Signal::Current)[..2], [1.0, 2.0]);
        assert_eq!(g.buffered(Signal::Voltage), [4.0, 5.0]);
    }

    #[test]
    fn consume_advances_the_base_sample_id_by_the_decimation() {
        let mut g = group();
        g.push(Signal::Current, 100, &[1.0, 2.0]);
        g.push(Signal::Voltage, 100, &[3.0, 4.0]);
        assert_eq!(g.base_sample_id(), Some(100));
        g.consume(2);
        assert_eq!(g.base_sample_id(), Some(100 + 2 * D as u64));
        assert_eq!(g.ready(), 0);
    }

    #[test]
    fn consume_of_zero_is_a_no_op() {
        let mut g = group();
        g.push(Signal::Current, 10, &[1.0]);
        g.push(Signal::Voltage, 10, &[2.0]);
        g.consume(0);
        assert_eq!(g.ready(), 1);
        assert_eq!(g.base_sample_id(), Some(10));
    }

    #[test]
    fn a_gap_is_filled_with_nan_so_rows_stay_aligned() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0]);
        // Skip two samples: next expected id is 2, this frame says 6.
        g.push(Signal::Current, 6, &[9.0]);
        let data = g.buffered(Signal::Current);
        assert_eq!(data.len(), 4);
        assert_eq!(data[0], 1.0);
        assert!(data[1].is_nan() && data[2].is_nan());
        assert_eq!(data[3], 9.0);
        assert_eq!(g.counters(Signal::Current).unwrap().filled, 2);
    }

    #[test]
    fn overlapping_frames_drop_only_the_repeated_prefix() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0, 2.0, 3.0]);
        // Resends from id 2, which is sample index 1.
        g.push(Signal::Current, 2, &[2.0, 3.0, 4.0]);
        assert_eq!(g.buffered(Signal::Current), [1.0, 2.0, 3.0, 4.0]);
        let c = g.counters(Signal::Current).unwrap();
        assert_eq!(c.skipped, 2);
        assert_eq!(c.samples, 4);
    }

    #[test]
    fn a_fully_repeated_frame_is_dropped_entirely() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0, 2.0]);
        g.push(Signal::Current, 0, &[1.0, 2.0]);
        assert_eq!(g.buffered(Signal::Current), [1.0, 2.0]);
        let c = g.counters(Signal::Current).unwrap();
        assert_eq!(c.skipped, 2);
        assert_eq!(c.discontinuities, 1);
    }

    #[test]
    fn a_late_starting_signal_is_padded_to_the_group_base() {
        let mut g = group();
        g.push(Signal::Current, 100, &[1.0, 2.0, 3.0]);
        // Voltage shows up two samples later.
        g.push(Signal::Voltage, 104, &[7.0]);
        let v = g.buffered(Signal::Voltage);
        assert_eq!(v.len(), 3, "padded back to the group base");
        assert!(v[0].is_nan() && v[1].is_nan());
        assert_eq!(v[2], 7.0);
        assert_eq!(g.ready(), 3);
    }

    #[test]
    fn sample_ids_are_widened_across_the_wrap() {
        let mut g = SignalGroup::new(&[Signal::Current], D, MAX_FILL);
        g.push(Signal::Current, 0xffff_fffc, &[1.0, 2.0]);
        // Wraps to 0; two samples at decimate 2 is exactly the next id.
        g.push(Signal::Current, 0x0000_0000, &[3.0]);
        assert_eq!(g.buffered(Signal::Current), [1.0, 2.0, 3.0]);
        assert_eq!(g.counters(Signal::Current).unwrap().filled, 0);
    }

    #[test]
    fn disabled_signals_are_ignored_on_push() {
        let mut g = SignalGroup::new(&[Signal::Current], D, MAX_FILL);
        g.push(Signal::Power, 0, &[1.0]);
        assert!(g.buffered(Signal::Power).is_empty());
        assert!(g.counters(Signal::Power).is_none());
    }

    #[test]
    fn an_empty_group_is_never_ready() {
        let g = SignalGroup::new(&[], D, MAX_FILL);
        assert_eq!(g.ready(), 0);
    }

    #[test]
    fn enabled_lists_only_configured_signals() {
        let g = SignalGroup::new(&[Signal::Power, Signal::Current], D, MAX_FILL);
        let got: Vec<_> = g.enabled().collect();
        assert_eq!(got, vec![Signal::Current, Signal::Power]);
    }

    #[test]
    fn reset_clears_data_but_keeps_counters() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0, 2.0]);
        g.push(Signal::Voltage, 0, &[3.0, 4.0]);
        let before = g.counters(Signal::Current).unwrap();
        g.reset();
        assert_eq!(g.ready(), 0);
        assert_eq!(g.base_sample_id(), None);
        assert_eq!(g.counters(Signal::Current).unwrap(), before);

        // Re-anchors on the next frame.
        g.push(Signal::Current, 500, &[5.0]);
        assert_eq!(g.base_sample_id(), Some(500));
    }

    #[test]
    fn an_oversized_gap_re_anchors_instead_of_filling() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0, 2.0]);
        g.push(Signal::Voltage, 0, &[3.0, 4.0]);

        // Jump far beyond max_fill, as a stale frame from an earlier session
        // or a corrupt sample id would.
        let far = (MAX_FILL as u32 + 10_000) * D;
        g.push(Signal::Current, far, &[9.0]);

        assert_eq!(g.resyncs(), 1);
        assert_eq!(g.base_sample_id(), Some(far as u64));
        assert_eq!(
            g.buffered(Signal::Current),
            [9.0],
            "only the new frame survives the re-anchor"
        );
        assert!(g.buffered(Signal::Voltage).is_empty());
        assert!(
            g.counters(Signal::Current).unwrap().filled < MAX_FILL as u64,
            "must not synthesize the whole gap"
        );
    }

    #[test]
    fn a_gap_within_the_limit_is_still_filled() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0]);
        let within = 100 * D;
        g.push(Signal::Current, within, &[2.0]);
        assert_eq!(g.resyncs(), 0);
        assert_eq!(g.buffered(Signal::Current).len(), 101);
        assert_eq!(g.counters(Signal::Current).unwrap().filled, 99);
    }

    #[test]
    fn re_anchoring_keeps_every_channel_on_one_timeline() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0]);
        g.push(Signal::Voltage, 0, &[2.0]);
        let far = (MAX_FILL as u32 + 50) * D;
        g.push(Signal::Current, far, &[7.0]);
        g.push(Signal::Voltage, far, &[8.0]);
        assert_eq!(g.ready(), 1);
        assert_eq!(g.buffered(Signal::Current), [7.0]);
        assert_eq!(g.buffered(Signal::Voltage), [8.0]);
    }

    #[test]
    fn zero_decimation_is_clamped_to_one() {
        let g = SignalGroup::new(&[Signal::Current], 0, MAX_FILL);
        assert_eq!(g.decimate(), 1);
    }

    #[test]
    fn consume_beyond_the_buffer_is_clamped() {
        let mut g = group();
        g.push(Signal::Current, 0, &[1.0]);
        g.push(Signal::Voltage, 0, &[2.0]);
        g.consume(100);
        assert_eq!(g.ready(), 0);
        assert!(g.buffered(Signal::Current).is_empty());
    }
}
