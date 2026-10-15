//! JS220 bulk transfer framing.
//!
//! Every bulk IN and bulk OUT transfer is a whole number of fixed 512-byte
//! frames. A frame is a 4-byte little-endian header followed by up to 508
//! payload bytes; the remainder of the 512 bytes is undefined padding.
//!
//! Header bit layout:
//!
//! ```text
//! [15:0]  frame_id    increments by one per frame, wraps at 2^16
//! [24:16] length      payload length in bytes, 0..=511
//! [29:25] port_id     destination port, 0..=31
//! [31:30] reserved    zero
//! ```

/// Total size of one frame on the wire, header included.
pub const FRAME_LEN: usize = 512;
/// Size of the frame header.
pub const HEADER_LEN: usize = 4;
/// Largest payload that fits in a frame.
pub const PAYLOAD_MAX: usize = FRAME_LEN - HEADER_LEN;

/// Port carrying connect, echo and time synchronization messages.
pub const PORT_ADMIN: u8 = 0;
/// Port carrying publish/subscribe traffic.
pub const PORT_PUBSUB: u8 = 1;
/// Port carrying device log records.
pub const PORT_LOG: u8 = 2;
/// Port carrying memory read/write and firmware update.
pub const PORT_MEM: u8 = 3;
/// First port id used for sample streaming; stream port `n` is `PORT_STREAM_BASE + n`.
pub const PORT_STREAM_BASE: u8 = 16;

/// A decoded frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Sequence number, incremented by the sender for every frame.
    pub frame_id: u16,
    /// Payload length in bytes, excluding the header.
    pub length: u16,
    /// Destination port.
    pub port_id: u8,
}

impl FrameHeader {
    /// Decode a header from its 32-bit wire representation.
    #[inline]
    pub const fn from_u32(raw: u32) -> Self {
        Self {
            frame_id: (raw & 0xffff) as u16,
            length: ((raw >> 16) & 0x1ff) as u16,
            port_id: ((raw >> 25) & 0x1f) as u8,
        }
    }

    /// Encode this header into its 32-bit wire representation.
    ///
    /// `length` and `port_id` are masked to their field widths, matching the
    /// device-side packing helper.
    #[inline]
    pub const fn to_u32(self) -> u32 {
        (self.frame_id as u32)
            | (((self.length as u32) & 0x1ff) << 16)
            | (((self.port_id as u32) & 0x1f) << 25)
    }

    /// True when `length` fits in a single frame.
    #[inline]
    pub const fn is_length_valid(self) -> bool {
        (self.length as usize) <= PAYLOAD_MAX
    }

    /// True when this frame targets a sample streaming port.
    #[inline]
    pub const fn is_stream(self) -> bool {
        self.port_id >= PORT_STREAM_BASE
    }

    /// The stream index for a streaming frame, i.e. `port_id - 16`.
    #[inline]
    pub const fn stream_index(self) -> u8 {
        self.port_id & 0x0f
    }
}

/// A borrowed view of one frame: its header plus a slice of exactly
/// `header.length` payload bytes.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// The decoded header.
    pub header: FrameHeader,
    /// The payload, already truncated to the declared length.
    pub payload: &'a [u8],
}

/// Reasons a 512-byte block could not be read as a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The block was shorter than [`FRAME_LEN`].
    Short {
        /// Number of bytes actually available.
        len: usize,
    },
    /// The header declared a payload longer than a frame can hold.
    Length {
        /// The declared payload length.
        length: u16,
    },
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short { len } => {
                write!(f, "frame truncated: {len} bytes, need {FRAME_LEN}")
            }
            Self::Length { length } => {
                write!(f, "frame payload length {length} exceeds {PAYLOAD_MAX}")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for FrameError {}

impl<'a> Frame<'a> {
    /// Borrow the first frame out of `buf` without copying.
    ///
    /// `buf` must hold at least one whole 512-byte frame; trailing bytes are
    /// ignored so this can be called repeatedly over a multi-frame transfer.
    pub fn parse(buf: &'a [u8]) -> Result<Self, FrameError> {
        if buf.len() < FRAME_LEN {
            return Err(FrameError::Short { len: buf.len() });
        }
        let raw = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let header = FrameHeader::from_u32(raw);
        if !header.is_length_valid() {
            return Err(FrameError::Length {
                length: header.length,
            });
        }
        let end = HEADER_LEN + header.length as usize;
        Ok(Self {
            header,
            payload: &buf[HEADER_LEN..end],
        })
    }
}

/// Iterator over the frames in a bulk transfer buffer.
///
/// Yields one item per 512-byte block. A block whose header is malformed is
/// reported as an error and iteration continues with the next block, so a
/// single corrupt frame does not discard the rest of the transfer.
pub struct FrameIter<'a> {
    buf: &'a [u8],
}

impl<'a> FrameIter<'a> {
    /// Iterate the frames in `buf`.
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = Result<Frame<'a>, FrameError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        if self.buf.len() < FRAME_LEN {
            let err = FrameError::Short {
                len: self.buf.len(),
            };
            self.buf = &[];
            return Some(Err(err));
        }
        let (block, rest) = self.buf.split_at(FRAME_LEN);
        self.buf = rest;
        Some(Frame::parse(block))
    }
}

impl ExactSizeIterator for FrameIter<'_> {
    fn len(&self) -> usize {
        self.buf.len().div_ceil(FRAME_LEN)
    }
}

/// Write a frame header and payload into `out`, which must be [`FRAME_LEN`]
/// bytes. Padding after the payload is zeroed.
///
/// Returns `None` if `out` is the wrong size or `payload` is too long.
pub fn encode_frame(out: &mut [u8], frame_id: u16, port_id: u8, payload: &[u8]) -> Option<()> {
    if out.len() != FRAME_LEN || payload.len() > PAYLOAD_MAX {
        return None;
    }
    let header = FrameHeader {
        frame_id,
        length: payload.len() as u16,
        port_id,
    };
    out[..HEADER_LEN].copy_from_slice(&header.to_u32().to_le_bytes());
    let end = HEADER_LEN + payload.len();
    out[HEADER_LEN..end].copy_from_slice(payload);
    out[end..].fill(0);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips_through_u32() {
        for &(frame_id, length, port_id) in &[
            (0u16, 0u16, 0u8),
            (1, 4, 1),
            (0xffff, 511, 31),
            (0x1234, 508, 21),
        ] {
            let h = FrameHeader {
                frame_id,
                length,
                port_id,
            };
            assert_eq!(FrameHeader::from_u32(h.to_u32()), h);
        }
    }

    #[test]
    fn header_field_positions_match_wire_layout() {
        let raw = 0x0102u32 | (15 << 16) | (5 << 25);
        let h = FrameHeader::from_u32(raw);
        assert_eq!(h.frame_id, 0x0102);
        assert_eq!(h.length, 15);
        assert_eq!(h.port_id, 5);
    }

    #[test]
    fn reserved_bits_are_ignored_on_decode() {
        let raw = 0x1234u32 | (8 << 16) | (3 << 25) | (0b11 << 30);
        let h = FrameHeader::from_u32(raw);
        assert_eq!(h.port_id, 3);
        assert_eq!(h.length, 8);
    }

    #[test]
    fn encode_then_parse_preserves_payload() {
        let mut buf = [0xaau8; FRAME_LEN];
        let payload: Vec<u8> = (0..100u8).collect();
        encode_frame(&mut buf, 7, PORT_PUBSUB, &payload).unwrap();
        let f = Frame::parse(&buf).unwrap();
        assert_eq!(f.header.frame_id, 7);
        assert_eq!(f.header.port_id, PORT_PUBSUB);
        assert_eq!(f.payload, &payload[..]);
        assert!(buf[HEADER_LEN + payload.len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        let mut buf = [0u8; FRAME_LEN];
        assert!(encode_frame(&mut buf, 0, 1, &[0u8; PAYLOAD_MAX + 1]).is_none());
    }

    #[test]
    fn encode_rejects_wrong_sized_output() {
        let mut buf = [0u8; FRAME_LEN - 1];
        assert!(encode_frame(&mut buf, 0, 1, &[]).is_none());
    }

    #[test]
    fn parse_rejects_short_buffer() {
        let buf = [0u8; 16];
        assert_eq!(
            Frame::parse(&buf).unwrap_err(),
            FrameError::Short { len: 16 }
        );
    }

    #[test]
    fn parse_rejects_overlong_declared_length() {
        let mut buf = [0u8; FRAME_LEN];
        let raw = 511u32 << 16;
        buf[..4].copy_from_slice(&raw.to_le_bytes());
        assert_eq!(
            Frame::parse(&buf).unwrap_err(),
            FrameError::Length { length: 511 }
        );
    }

    #[test]
    fn iterator_walks_every_block_and_recovers_from_a_bad_one() {
        let mut buf = vec![0u8; FRAME_LEN * 3];
        encode_frame(&mut buf[..FRAME_LEN], 0, 1, &[1, 2, 3]).unwrap();
        let raw = 1u32 | (511 << 16);
        buf[FRAME_LEN..FRAME_LEN + 4].copy_from_slice(&raw.to_le_bytes());
        encode_frame(&mut buf[FRAME_LEN * 2..], 2, 1, &[9]).unwrap();

        let got: Vec<_> = FrameIter::new(&buf).collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_ref().unwrap().payload, &[1, 2, 3]);
        assert!(got[1].is_err());
        assert_eq!(got[2].as_ref().unwrap().payload, &[9]);
    }

    #[test]
    fn iterator_reports_trailing_partial_frame() {
        let buf = vec![0u8; FRAME_LEN + 7];
        let got: Vec<_> = FrameIter::new(&buf).collect();
        assert_eq!(got.len(), 2);
        assert!(got[0].is_ok());
        assert_eq!(got[1].unwrap_err(), FrameError::Short { len: 7 });
    }

    #[test]
    fn stream_index_masks_to_low_nibble() {
        let h = FrameHeader {
            frame_id: 0,
            length: 0,
            port_id: PORT_STREAM_BASE + 5,
        };
        assert!(h.is_stream());
        assert_eq!(h.stream_index(), 5);
    }
}
