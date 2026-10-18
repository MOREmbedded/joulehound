//! Capture of the general purpose input lines.
//!
//! The GPI ports carry one bit per sample at the instrument's full 2 MHz base
//! rate, packed eight to a byte, and are timestamped with the same sample id
//! counter as current and voltage. That shared counter is what lets a logic
//! line be lined up against an analog trace.
//!
//! These lines are deliberately kept outside [`crate::signals::SignalGroup`].
//! That aligner exists to deliver rows in which every signal has a value at
//! the same instant, and it runs at the calibrated 1 Msps rate; GPI runs at
//! twice that and needs no row alignment, because the consumer places it on
//! the time axis from its sample id alone.

use js220_proto::stream::SampleIdUnwrap;

/// A run of consecutive GPI samples.
///
/// Samples are contiguous within a block: `bits[n]` was taken at sample id
/// `first_id + n`. A gap in the stream ends one block and starts another
/// rather than being papered over, so no consumer can mistake a dropout for a
/// line that simply held its level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpiBlock {
    /// Device sample id of `bits[0]`, counted at the 2 MHz base rate.
    pub first_id: u64,
    /// One sample per entry, 0 or 1.
    pub bits: Vec<u8>,
}

impl GpiBlock {
    /// Device sample id just past the last sample.
    pub fn end_id(&self) -> u64 {
        self.first_id + self.bits.len() as u64
    }
}

/// Running totals for one GPI line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpiCounters {
    /// Stream frames accepted.
    pub frames: u64,
    /// Samples decoded.
    pub samples: u64,
    /// Samples synthesized to bridge a short dropout.
    pub filled: u64,
    /// Dropouts too long to bridge, each of which started a new block.
    pub discontinuities: u64,
    /// Frames that carried nothing newer than what was already held.
    pub duplicates: u64,
}

/// One general purpose input line, accumulated as frames arrive.
#[derive(Debug)]
pub struct GpiChannel {
    channel: u8,
    unwrap: SampleIdUnwrap,
    /// Blocks awaiting collection. Normally one; a dropout adds another.
    blocks: Vec<GpiBlock>,
    /// Sample id just past the last sample held.
    next_id: u64,
    /// Longest dropout, in samples, worth bridging.
    max_fill: usize,
    counters: GpiCounters,
}

impl GpiChannel {
    /// Track `channel`, bridging dropouts of up to `max_fill` samples.
    pub fn new(channel: u8, max_fill: usize) -> Self {
        Self {
            channel,
            unwrap: SampleIdUnwrap::new(),
            blocks: Vec::new(),
            next_id: 0,
            max_fill,
            counters: GpiCounters::default(),
        }
    }

    /// The instrument's number for this line.
    pub fn channel(&self) -> u8 {
        self.channel
    }

    /// Running totals for this line.
    pub fn counters(&self) -> GpiCounters {
        self.counters
    }

    /// Whether anything is waiting to be collected.
    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|b| b.bits.is_empty())
    }

    /// Accept one decoded frame, timestamped with its raw 32-bit sample id.
    ///
    /// A short dropout is bridged by repeating the last level, which for a
    /// digital line is the physically right reconstruction: the line did not
    /// stop existing, the host stopped hearing about it. A long one is not
    /// bridged at all, because a line held for a tenth of a second on no
    /// evidence is an invention, not a measurement.
    pub fn push(&mut self, raw_id: u32, bits: &[u8]) {
        self.counters.frames += 1;
        if bits.is_empty() {
            return;
        }
        let id = self.unwrap.update(raw_id);

        let Some(block) = self.blocks.last_mut() else {
            self.start_block(id, bits);
            return;
        };

        let mut bits = bits;
        if id < self.next_id {
            // Overlap: the instrument re-sent samples already held. Keep what
            // is genuinely new.
            let seen = (self.next_id - id) as usize;
            if seen >= bits.len() {
                self.counters.duplicates += 1;
                return;
            }
            bits = &bits[seen..];
        } else if id > self.next_id {
            let gap = (id - self.next_id) as usize;
            if gap <= self.max_fill {
                let level = block.bits.last().copied().unwrap_or(0);
                block.bits.resize(block.bits.len() + gap, level);
                self.counters.filled += gap as u64;
                self.next_id = id;
            } else {
                self.counters.discontinuities += 1;
                self.start_block(id, bits);
                return;
            }
        }

        let block = self.blocks.last_mut().expect("checked present");
        block.bits.extend_from_slice(bits);
        self.next_id += bits.len() as u64;
        self.counters.samples += bits.len() as u64;
    }

    fn start_block(&mut self, id: u64, bits: &[u8]) {
        self.blocks.push(GpiBlock {
            first_id: id,
            bits: bits.to_vec(),
        });
        self.next_id = id + bits.len() as u64;
        self.counters.samples += bits.len() as u64;
    }

    /// Take everything accumulated, leaving the channel ready for more.
    pub fn take(&mut self) -> Vec<GpiBlock> {
        std::mem::take(&mut self.blocks)
    }

    /// Discard everything held and forget where the stream was.
    pub fn reset(&mut self) {
        self.blocks.clear();
        self.next_id = 0;
        self.unwrap.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> GpiChannel {
        GpiChannel::new(0, 64)
    }

    #[test]
    fn a_channel_remembers_its_number() {
        assert_eq!(GpiChannel::new(7, 16).channel(), 7, "the trigger input");
    }

    #[test]
    fn a_fresh_channel_holds_nothing() {
        let mut c = channel();
        assert!(c.is_empty());
        assert!(c.take().is_empty());
    }

    #[test]
    fn samples_arrive_in_one_block_with_their_sample_id() {
        let mut c = channel();
        c.push(100, &[0, 1, 1, 0]);
        let blocks = c.take();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].first_id, 100);
        assert_eq!(blocks[0].bits, vec![0, 1, 1, 0]);
        assert_eq!(blocks[0].end_id(), 104);
    }

    #[test]
    fn consecutive_frames_join_a_single_block() {
        let mut c = channel();
        c.push(0, &[1, 1]);
        c.push(2, &[0, 0]);
        let blocks = c.take();
        assert_eq!(blocks.len(), 1, "no reason to split a contiguous run");
        assert_eq!(blocks[0].bits, vec![1, 1, 0, 0]);
    }

    #[test]
    fn taking_leaves_the_channel_ready_for_more() {
        let mut c = channel();
        c.push(0, &[1, 1]);
        let _ = c.take();
        c.push(2, &[0, 0]);
        let blocks = c.take();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].first_id, 2, "the stream carried on");
        assert_eq!(blocks[0].bits, vec![0, 0]);
    }

    #[test]
    fn a_short_dropout_holds_the_last_level() {
        // A line does not stop existing because the host stopped hearing it.
        let mut c = channel();
        c.push(0, &[1, 1]);
        c.push(6, &[0]);
        let blocks = c.take();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].bits, vec![1, 1, 1, 1, 1, 1, 0]);
        assert_eq!(c.counters().filled, 4);
    }

    #[test]
    fn a_dropout_that_starts_a_block_holds_low() {
        // Nothing has been seen yet, so there is no level to hold.
        let mut c = channel();
        c.push(0, &[]);
        c.push(4, &[1]);
        let blocks = c.take();
        assert_eq!(
            blocks[0].first_id, 4,
            "no level is invented before the data"
        );
    }

    #[test]
    fn a_long_dropout_starts_a_new_block_instead_of_inventing_a_level() {
        let mut c = GpiChannel::new(0, 8);
        c.push(0, &[1, 1]);
        c.push(1000, &[0]);
        let blocks = c.take();
        assert_eq!(blocks.len(), 2, "the gap is real and must stay visible");
        assert_eq!(blocks[0].bits, vec![1, 1]);
        assert_eq!(blocks[1].first_id, 1000);
        assert_eq!(c.counters().discontinuities, 1);
        assert_eq!(c.counters().filled, 0, "nothing was invented");
    }

    #[test]
    fn a_long_dropout_does_not_discard_what_was_already_held() {
        let mut c = GpiChannel::new(0, 8);
        c.push(0, &[1, 1, 1]);
        c.push(9_999, &[0]);
        let blocks = c.take();
        assert_eq!(blocks[0].bits.len(), 3, "earlier samples survive the gap");
    }

    #[test]
    fn a_repeated_frame_contributes_nothing() {
        let mut c = channel();
        c.push(0, &[1, 0, 1, 0]);
        c.push(0, &[1, 0, 1, 0]);
        let blocks = c.take();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].bits, vec![1, 0, 1, 0]);
        assert_eq!(c.counters().duplicates, 1);
    }

    #[test]
    fn an_overlapping_frame_contributes_only_its_new_tail() {
        let mut c = channel();
        c.push(0, &[1, 1, 1, 1]);
        // Starts two samples back and carries two new ones.
        c.push(2, &[1, 1, 0, 0]);
        let blocks = c.take();
        assert_eq!(blocks[0].bits, vec![1, 1, 1, 1, 0, 0]);
    }

    #[test]
    fn an_empty_frame_is_counted_but_changes_nothing() {
        let mut c = channel();
        c.push(0, &[1]);
        c.push(1, &[]);
        assert_eq!(c.counters().frames, 2);
        assert_eq!(c.counters().samples, 1);
        let blocks = c.take();
        assert_eq!(blocks[0].bits, vec![1]);
    }

    #[test]
    fn the_sample_id_is_unwrapped_across_the_32_bit_boundary() {
        // At 2 Msps the counter wraps every 36 minutes, and a block whose
        // time ran backwards would land in the wrong place entirely.
        let mut c = channel();
        c.push(u32::MAX - 1, &[1, 1]);
        let first = c.take()[0].first_id;
        c.push(1, &[0, 0]);
        let second = c.take()[0].first_id;
        assert!(
            second > first,
            "ids went backwards across the wrap: {first} then {second}"
        );
        assert_eq!(second - first, 3);
    }

    #[test]
    fn counters_add_up_over_a_session() {
        let mut c = channel();
        c.push(0, &[1, 0]);
        c.push(2, &[1, 0]);
        let n = c.counters();
        assert_eq!(n.frames, 2);
        assert_eq!(n.samples, 4);
        assert_eq!(n.filled, 0);
        assert_eq!(n.discontinuities, 0);
    }

    #[test]
    fn reset_forgets_the_stream_position() {
        let mut c = channel();
        c.push(500, &[1, 1]);
        c.reset();
        assert!(c.is_empty());
        c.push(10, &[0]);
        let blocks = c.take();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].first_id, 10,
            "a restarted stream is not a backwards jump"
        );
    }
}
