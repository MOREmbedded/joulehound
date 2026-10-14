//! Publish/subscribe messages (port 1).
//!
//! Layout of a pub/sub frame payload:
//!
//! ```text
//! offset  size  field
//!      0    32  topic, NUL-padded ASCII
//!     32     1  type   (DType tag)
//!     33     1  flags
//!     34     1  op     (application-specific operation)
//!     35     1  app    (application-specific data)
//!     36     n  value payload
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use crate::frame::PAYLOAD_MAX;
use crate::value::{DType, Value, ValueError};

/// Fixed width of the topic field.
pub const TOPIC_LEN: usize = 32;
/// Size of the fields preceding the value payload.
pub const HEADER_LEN: usize = TOPIC_LEN + 4;
/// Largest value payload that fits in one pub/sub frame.
pub const DATA_MAX: usize = PAYLOAD_MAX - HEADER_LEN;

/// A publish/subscribe message.
#[derive(Debug, Clone, PartialEq)]
pub struct PubSubMsg {
    /// Topic, without the device prefix. Truncated to 31 characters plus NUL.
    pub topic: String,
    /// The value being published.
    pub value: Value,
    /// Flag bits; see [`crate::value::flags`].
    pub flags: u8,
    /// Application-specific operation code.
    pub op: u8,
    /// Application-specific byte.
    pub app: u8,
}

impl PubSubMsg {
    /// A plain publish of `value` to `topic`, with no flags set.
    pub fn new(topic: impl Into<String>, value: Value) -> Self {
        Self {
            topic: topic.into(),
            value,
            flags: 0,
            op: 0,
            app: 0,
        }
    }

    /// Set the flag bits.
    pub fn with_flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    /// Serialize into a frame payload.
    ///
    /// Returns `None` if the value payload would overflow a single frame. The
    /// topic is silently truncated to fit its fixed field, matching the
    /// device-side encoder.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(HEADER_LEN + 8);
        let topic = self.topic.as_bytes();
        // Reserve one byte for the NUL terminator.
        let n = topic.len().min(TOPIC_LEN - 1);
        out.extend_from_slice(&topic[..n]);
        out.resize(TOPIC_LEN, 0);
        out.push(self.value.dtype() as u8);
        out.push(self.flags);
        out.push(self.op);
        out.push(self.app);
        self.value.encode_payload(&mut out);
        if out.len() > PAYLOAD_MAX {
            return None;
        }
        Some(out)
    }

    /// Parse a pub/sub message out of a frame payload.
    pub fn decode(payload: &[u8]) -> Result<Self, PubSubError> {
        if payload.len() < HEADER_LEN {
            return Err(PubSubError::Short {
                have: payload.len(),
                need: HEADER_LEN,
            });
        }
        let raw_topic = &payload[..TOPIC_LEN];
        let end = raw_topic
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(TOPIC_LEN);
        let topic = core::str::from_utf8(&raw_topic[..end])
            .map_err(|_| PubSubError::TopicUtf8)?
            .into();
        let tag = payload[TOPIC_LEN];
        let dtype = DType::from_u8(tag).ok_or(PubSubError::Value(ValueError::UnknownType { tag }))?;
        let value = Value::decode_payload(dtype, &payload[HEADER_LEN..]).map_err(PubSubError::Value)?;
        Ok(Self {
            topic,
            value,
            flags: payload[TOPIC_LEN + 1],
            op: payload[TOPIC_LEN + 2],
            app: payload[TOPIC_LEN + 3],
        })
    }
}

/// Failures while decoding a pub/sub message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PubSubError {
    /// The payload was shorter than the fixed header.
    Short {
        /// Bytes available.
        have: usize,
        /// Bytes required.
        need: usize,
    },
    /// The topic field was not valid UTF-8.
    TopicUtf8,
    /// The value payload could not be decoded.
    Value(ValueError),
}

impl core::fmt::Display for PubSubError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short { have, need } => write!(f, "pubsub message too short: {have} < {need}"),
            Self::TopicUtf8 => write!(f, "pubsub topic is not valid UTF-8"),
            Self::Value(e) => write!(f, "pubsub value: {e}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PubSubError {}

/// Topic suffixes and prefixes that carry meaning in the JS220 topic tree.
pub mod topic_chars {
    /// Prefix marking a topic as a command; commands are never retained.
    pub const COMMAND: char = '!';
    /// Suffix requesting the metadata for a topic.
    pub const METADATA_REQUEST: char = '%';
    /// Suffix carrying a metadata response.
    pub const METADATA_RESPONSE: char = '$';
    /// Suffix requesting the current value of a topic.
    pub const QUERY_REQUEST: char = '?';
    /// Suffix carrying a query response.
    pub const QUERY_RESPONSE: char = '&';
    /// Suffix carrying a return code.
    pub const RETURN_CODE: char = '#';
}

/// Publishing null to this topic asks the device to emit metadata for its
/// whole topic tree.
pub const TOPIC_METADATA_ALL: &str = "$";
/// Publishing null to this topic asks the device to emit every retained value.
pub const TOPIC_QUERY_ALL: &str = "?";
/// Round-trip probe; the device answers on [`TOPIC_PONG`].
pub const TOPIC_PING: &str = "c/!ping";
/// Reply topic for [`TOPIC_PING`].
pub const TOPIC_PONG: &str = "c/!pong";

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn encode_then_decode_roundtrips() {
        let msg = PubSubMsg::new("s/i/ctrl", Value::U32(1));
        let bytes = msg.encode().unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + 8);
        assert_eq!(PubSubMsg::decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn topic_field_is_fixed_width_and_nul_padded() {
        let bytes = PubSubMsg::new("c/!ping", Value::Null).encode().unwrap();
        assert_eq!(&bytes[..7], b"c/!ping");
        assert!(bytes[7..TOPIC_LEN].iter().all(|&b| b == 0));
        assert_eq!(bytes[TOPIC_LEN], DType::Null as u8);
    }

    #[test]
    fn overlong_topic_is_truncated_leaving_room_for_the_nul() {
        let long = "a".repeat(100);
        let bytes = PubSubMsg::new(long, Value::Null).encode().unwrap();
        assert_eq!(bytes[TOPIC_LEN - 1], 0, "topic must stay NUL-terminated");
        let decoded = PubSubMsg::decode(&bytes).unwrap();
        assert_eq!(decoded.topic.len(), TOPIC_LEN - 1);
    }

    #[test]
    fn flags_op_and_app_survive_the_roundtrip() {
        let msg = PubSubMsg {
            topic: "s/v/ctrl".into(),
            value: Value::U32(1),
            flags: crate::value::flags::RETAIN,
            op: 3,
            app: 9,
        };
        let decoded = PubSubMsg::decode(&msg.encode().unwrap()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn oversized_binary_value_is_refused() {
        let msg = PubSubMsg::new("x", Value::Bin(vec![0u8; DATA_MAX + 1]));
        assert!(msg.encode().is_none());
    }

    #[test]
    fn largest_fitting_binary_value_is_accepted() {
        let msg = PubSubMsg::new("x", Value::Bin(vec![0u8; DATA_MAX]));
        assert_eq!(msg.encode().unwrap().len(), PAYLOAD_MAX);
    }

    #[test]
    fn decode_rejects_truncated_header() {
        assert_eq!(
            PubSubMsg::decode(&[0u8; 10]).unwrap_err(),
            PubSubError::Short {
                have: 10,
                need: HEADER_LEN
            }
        );
    }

    #[test]
    fn decode_rejects_unknown_type_tag() {
        let mut bytes = PubSubMsg::new("x", Value::U32(0)).encode().unwrap();
        bytes[TOPIC_LEN] = 0x7f;
        assert_eq!(
            PubSubMsg::decode(&bytes).unwrap_err(),
            PubSubError::Value(ValueError::UnknownType { tag: 0x7f })
        );
    }

    #[test]
    fn topic_filling_the_whole_field_without_nul_still_decodes() {
        let mut bytes = PubSubMsg::new("x", Value::Null).encode().unwrap();
        bytes[..TOPIC_LEN].fill(b'z');
        let decoded = PubSubMsg::decode(&bytes).unwrap();
        assert_eq!(decoded.topic.len(), TOPIC_LEN);
    }
}
