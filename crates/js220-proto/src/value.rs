//! The tagged value type carried by publish/subscribe messages.
//!
//! On the wire a value is a one-byte type tag plus a payload. Scalars always
//! occupy 8 bytes (the device writes the full 64-bit union member regardless of
//! the declared width); strings and JSON are NUL-terminated UTF-8; binary is
//! raw bytes running to the end of the message.

use alloc::string::String;
use alloc::vec::Vec;

/// Wire type tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DType {
    /// No value. Also used to clear a retained value, and as the payload of a
    /// metadata or query request.
    Null = 0,
    /// NUL-terminated UTF-8 string.
    Str = 1,
    /// NUL-terminated UTF-8 JSON document.
    Json = 2,
    /// Raw bytes.
    Bin = 3,
    /// Constant binary message. Not produced by the JS220 pub/sub port.
    StdMsg = 4,
    /// Communication frame. Not produced by the JS220 pub/sub port.
    Frame = 5,
    /// 32-bit IEEE 754 float.
    F32 = 6,
    /// 64-bit IEEE 754 float.
    F64 = 7,
    /// Unsigned 8-bit integer.
    U8 = 8,
    /// Unsigned 16-bit integer.
    U16 = 9,
    /// Unsigned 32-bit integer.
    U32 = 10,
    /// Unsigned 64-bit integer.
    U64 = 11,
    /// Signed 8-bit integer.
    I8 = 12,
    /// Signed 16-bit integer.
    I16 = 13,
    /// Signed 32-bit integer.
    I32 = 14,
    /// Signed 64-bit integer.
    I64 = 15,
}

impl DType {
    /// Map a wire tag to a [`DType`], or `None` if the tag is not defined.
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Null,
            1 => Self::Str,
            2 => Self::Json,
            3 => Self::Bin,
            4 => Self::StdMsg,
            5 => Self::Frame,
            6 => Self::F32,
            7 => Self::F64,
            8 => Self::U8,
            9 => Self::U16,
            10 => Self::U32,
            11 => Self::U64,
            12 => Self::I8,
            13 => Self::I16,
            14 => Self::I32,
            15 => Self::I64,
            _ => return None,
        })
    }

    /// True for the variable-length, pointer-backed types.
    pub const fn is_variable_length(self) -> bool {
        matches!(self, Self::Str | Self::Json | Self::Bin)
    }
}

/// Value flag bits.
pub mod flags {
    /// Ask the device-side pub/sub instance to retain this value.
    pub const RETAIN: u8 = 1 << 0;
    /// The value points at a constant that remains valid indefinitely.
    pub const CONST: u8 = 1 << 1;
    /// The value owns heap memory that must be released.
    pub const HEAP: u8 = 1 << 7;
}

/// A decoded pub/sub value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// No value.
    Null,
    /// UTF-8 string.
    Str(String),
    /// UTF-8 JSON document.
    Json(String),
    /// Raw bytes.
    Bin(Vec<u8>),
    /// 32-bit float.
    F32(f32),
    /// 64-bit float.
    F64(f64),
    /// Unsigned 8-bit integer.
    U8(u8),
    /// Unsigned 16-bit integer.
    U16(u16),
    /// Unsigned 32-bit integer.
    U32(u32),
    /// Unsigned 64-bit integer.
    U64(u64),
    /// Signed 8-bit integer.
    I8(i8),
    /// Signed 16-bit integer.
    I16(i16),
    /// Signed 32-bit integer.
    I32(i32),
    /// Signed 64-bit integer.
    I64(i64),
}

impl Value {
    /// The wire tag for this value.
    pub const fn dtype(&self) -> DType {
        match self {
            Self::Null => DType::Null,
            Self::Str(_) => DType::Str,
            Self::Json(_) => DType::Json,
            Self::Bin(_) => DType::Bin,
            Self::F32(_) => DType::F32,
            Self::F64(_) => DType::F64,
            Self::U8(_) => DType::U8,
            Self::U16(_) => DType::U16,
            Self::U32(_) => DType::U32,
            Self::U64(_) => DType::U64,
            Self::I8(_) => DType::I8,
            Self::I16(_) => DType::I16,
            Self::I32(_) => DType::I32,
            Self::I64(_) => DType::I64,
        }
    }

    /// Interpret the value as an integer, widening or truncating as needed.
    ///
    /// Floats are truncated toward zero. Strings, JSON, binary and null have no
    /// integer interpretation and yield `None`.
    pub fn as_i64(&self) -> Option<i64> {
        Some(match *self {
            Self::U8(v) => v as i64,
            Self::U16(v) => v as i64,
            Self::U32(v) => v as i64,
            Self::U64(v) => v as i64,
            Self::I8(v) => v as i64,
            Self::I16(v) => v as i64,
            Self::I32(v) => v as i64,
            Self::I64(v) => v,
            Self::F32(v) => v as i64,
            Self::F64(v) => v as i64,
            _ => return None,
        })
    }

    /// Interpret the value as a float.
    pub fn as_f64(&self) -> Option<f64> {
        Some(match *self {
            Self::F32(v) => v as f64,
            Self::F64(v) => v,
            Self::U64(v) => v as f64,
            Self::I64(v) => v as f64,
            _ => self.as_i64()? as f64,
        })
    }

    /// Borrow the text of a string or JSON value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) | Self::Json(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Serialize the value payload, appending to `out`.
    ///
    /// Scalars are written as 8 little-endian bytes; strings and JSON are
    /// written with a trailing NUL; binary is written verbatim.
    pub fn encode_payload(&self, out: &mut Vec<u8>) {
        match self {
            Self::Null => {
                // The device still expects the 8-byte union slot for non-pointer
                // types, and null carries no data of its own.
                out.extend_from_slice(&0u64.to_le_bytes());
            }
            Self::Str(s) | Self::Json(s) => {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Self::Bin(b) => out.extend_from_slice(b),
            Self::F32(v) => Self::push_scalar(out, v.to_bits() as u64),
            Self::F64(v) => Self::push_scalar(out, v.to_bits()),
            Self::U8(v) => Self::push_scalar(out, *v as u64),
            Self::U16(v) => Self::push_scalar(out, *v as u64),
            Self::U32(v) => Self::push_scalar(out, *v as u64),
            Self::U64(v) => Self::push_scalar(out, *v),
            Self::I8(v) => Self::push_scalar(out, *v as i64 as u64),
            Self::I16(v) => Self::push_scalar(out, *v as i64 as u64),
            Self::I32(v) => Self::push_scalar(out, *v as i64 as u64),
            Self::I64(v) => Self::push_scalar(out, *v as u64),
        }
    }

    fn push_scalar(out: &mut Vec<u8>, bits: u64) {
        out.extend_from_slice(&bits.to_le_bytes());
    }

    /// Deserialize a value of type `dtype` from `data`.
    ///
    /// For scalars, `data` must hold at least 8 bytes. For strings and JSON the
    /// text runs to the first NUL, or to the end of `data` if unterminated. For
    /// binary, all of `data` is taken.
    pub fn decode_payload(dtype: DType, data: &[u8]) -> Result<Self, ValueError> {
        match dtype {
            DType::Null => Ok(Self::Null),
            DType::Str | DType::Json => {
                let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                let s = core::str::from_utf8(&data[..end])
                    .map_err(|_| ValueError::Utf8)?
                    .into();
                Ok(if dtype == DType::Str {
                    Self::Str(s)
                } else {
                    Self::Json(s)
                })
            }
            DType::Bin => Ok(Self::Bin(data.to_vec())),
            DType::StdMsg | DType::Frame => Err(ValueError::Unsupported { dtype }),
            _ => {
                if data.len() < 8 {
                    return Err(ValueError::Short {
                        have: data.len(),
                        need: 8,
                    });
                }
                let bits = u64::from_le_bytes(data[..8].try_into().expect("checked above"));
                Ok(match dtype {
                    DType::F32 => Self::F32(f32::from_bits(bits as u32)),
                    DType::F64 => Self::F64(f64::from_bits(bits)),
                    DType::U8 => Self::U8(bits as u8),
                    DType::U16 => Self::U16(bits as u16),
                    DType::U32 => Self::U32(bits as u32),
                    DType::U64 => Self::U64(bits),
                    DType::I8 => Self::I8(bits as i8),
                    DType::I16 => Self::I16(bits as i16),
                    DType::I32 => Self::I32(bits as i32),
                    DType::I64 => Self::I64(bits as i64),
                    // The pointer and null types are handled above.
                    _ => unreachable!("non-scalar dtype reached scalar decode"),
                })
            }
        }
    }
}

/// Failures while decoding a value payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueError {
    /// The payload was shorter than the type requires.
    Short {
        /// Bytes available.
        have: usize,
        /// Bytes required.
        need: usize,
    },
    /// String payload was not valid UTF-8.
    Utf8,
    /// The type tag was not recognized.
    UnknownType {
        /// The unrecognized tag.
        tag: u8,
    },
    /// The type is defined but carries no meaning on this transport.
    Unsupported {
        /// The type in question.
        dtype: DType,
    },
}

impl core::fmt::Display for ValueError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short { have, need } => write!(f, "value payload too short: {have} < {need}"),
            Self::Utf8 => write!(f, "value payload is not valid UTF-8"),
            Self::UnknownType { tag } => write!(f, "unknown value type tag {tag}"),
            Self::Unsupported { dtype } => write!(f, "unsupported value type {dtype:?}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ValueError {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn roundtrip(v: Value) {
        let mut buf = Vec::new();
        v.encode_payload(&mut buf);
        let got = Value::decode_payload(v.dtype(), &buf).unwrap();
        assert_eq!(got, v);
    }

    #[test]
    fn scalars_roundtrip() {
        roundtrip(Value::U8(0xa5));
        roundtrip(Value::U16(0xbeef));
        roundtrip(Value::U32(0xdead_beef));
        roundtrip(Value::U64(u64::MAX));
        roundtrip(Value::I8(-1));
        roundtrip(Value::I16(-300));
        roundtrip(Value::I32(i32::MIN));
        roundtrip(Value::I64(i64::MIN));
        roundtrip(Value::F32(1.5));
        roundtrip(Value::F64(-2.25));
    }

    #[test]
    fn text_and_binary_roundtrip() {
        roundtrip(Value::Str("on".into()));
        roundtrip(Value::Json("{\"a\":1}".into()));
        roundtrip(Value::Bin(vec![1, 2, 3, 0, 4]));
    }

    #[test]
    fn scalars_always_occupy_eight_bytes() {
        let mut buf = Vec::new();
        Value::U8(1).encode_payload(&mut buf);
        assert_eq!(buf.len(), 8);
        assert_eq!(buf, [1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn negative_scalars_sign_extend_into_the_union_slot() {
        let mut buf = Vec::new();
        Value::I8(-1).encode_payload(&mut buf);
        assert_eq!(buf, [0xff; 8]);
    }

    #[test]
    fn strings_are_nul_terminated() {
        let mut buf = Vec::new();
        Value::Str("on".into()).encode_payload(&mut buf);
        assert_eq!(buf, b"on\0");
    }

    #[test]
    fn string_decode_stops_at_nul_and_ignores_trailing_junk() {
        let v = Value::decode_payload(DType::Str, b"on\0\xff\xff").unwrap();
        assert_eq!(v, Value::Str("on".into()));
    }

    #[test]
    fn unterminated_string_decodes_to_end_of_payload() {
        let v = Value::decode_payload(DType::Str, b"on").unwrap();
        assert_eq!(v, Value::Str("on".into()));
    }

    #[test]
    fn short_scalar_payload_is_rejected() {
        assert_eq!(
            Value::decode_payload(DType::U32, &[1, 2, 3]).unwrap_err(),
            ValueError::Short { have: 3, need: 8 }
        );
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        assert_eq!(
            Value::decode_payload(DType::Str, &[0xff, 0xfe, 0]).unwrap_err(),
            ValueError::Utf8
        );
    }

    #[test]
    fn dtype_tags_match_the_wire_numbering() {
        assert_eq!(DType::from_u8(0), Some(DType::Null));
        assert_eq!(DType::from_u8(6), Some(DType::F32));
        assert_eq!(DType::from_u8(10), Some(DType::U32));
        assert_eq!(DType::from_u8(15), Some(DType::I64));
        assert_eq!(DType::from_u8(16), None);
    }

    #[test]
    fn numeric_coercions_widen_consistently() {
        assert_eq!(Value::U16(7).as_i64(), Some(7));
        assert_eq!(Value::I8(-7).as_i64(), Some(-7));
        assert_eq!(Value::F32(2.75).as_f64(), Some(2.75));
        assert_eq!(Value::Str("x".into()).as_i64(), None);
    }
}
