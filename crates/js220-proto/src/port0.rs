//! Administration messages (port 0): connect, echo and time synchronization.
//!
//! Every port 0 payload starts with a 4-byte header:
//!
//! ```text
//! offset  size  field
//!      0     1  op      (Port0Op)
//!      1     1  status  (0, or an error code)
//!      2     2  arg
//!      4     n  operation-specific payload
//! ```

use alloc::vec::Vec;

/// Size of the port 0 header.
pub const HEADER_LEN: usize = 4;

/// Vendor control requests issued on endpoint 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CtrlOp {
    /// Bring up the link: power the sensor, start controller/sensor
    /// communication and configure the instrument. The device answers
    /// asynchronously with a [`Port0Op::Connect`] frame on bulk IN.
    Connect = 1,
    /// Announce an orderly host disconnect. Also used immediately before
    /// `Connect` to flush any queued bulk IN data.
    Disconnect = 2,
}

/// `wValue` for [`CtrlOp::Connect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u16)]
pub enum ConnectMode {
    /// Reset the instrument to its default configuration.
    #[default]
    Reset = 0,
    /// Preserve the existing configuration where possible.
    Keep = 1,
}

/// Operations carried on port 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Port0Op {
    /// Response to [`CtrlOp::Connect`].
    Connect = 1,
    /// Loopback: the device echoes the header and payload back unchanged.
    Echo = 2,
    /// Clock synchronization exchange, initiated by the device.
    TimeSync = 3,
    /// Sample-counter to UTC mapping, pushed by the device.
    TimeMap = 4,
}

impl Port0Op {
    /// Map a wire opcode to a [`Port0Op`].
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Connect,
            2 => Self::Echo,
            3 => Self::TimeSync,
            4 => Self::TimeMap,
            _ => return None,
        })
    }
}

/// A `major.minor.patch` version packed as `major8.minor8.patch16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Version {
    /// Major version.
    pub major: u8,
    /// Minor version.
    pub minor: u8,
    /// Patch version.
    pub patch: u16,
}

impl Version {
    /// Unpack from the wire encoding.
    pub const fn from_u32(v: u32) -> Self {
        Self {
            major: (v >> 24) as u8,
            minor: ((v >> 16) & 0xff) as u8,
            patch: (v & 0xffff) as u16,
        }
    }

    /// Pack into the wire encoding.
    pub const fn to_u32(self) -> u32 {
        ((self.major as u32) << 24) | ((self.minor as u32) << 16) | self.patch as u32
    }
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Payload of a [`Port0Op::Connect`] response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectInfo {
    /// Protocol version. Only the major number is meaningful for compatibility.
    pub protocol_version: Version,
    /// Application identifier.
    pub app_id: u32,
    /// Controller firmware version.
    pub fw_version: Version,
    /// PCB version.
    pub hw_version: Version,
    /// Sensor FPGA version; zero when the sensor is not yet powered.
    pub fpga_version: Version,
}

/// Payload of a [`Port0Op::TimeSync`] exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeSync {
    /// Device counter sampled just before it sent the request.
    pub start_count: u64,
    /// Host UTC time just after it received the request.
    pub utc_recv: i64,
    /// Host UTC time just before it sent the response.
    pub utc_send: i64,
    /// Device counter sampled just after it received the response. The device
    /// may leave this zero.
    pub end_count: u64,
}

/// Payload of a [`Port0Op::TimeMap`] announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeMap {
    /// UTC timestamp in 34Q30 fixed point.
    pub utc_q30: i64,
    /// Sample counter value corresponding to `utc_q30`.
    pub counter: u64,
    /// Estimated counter rate in Hz, 32Q32 fixed point.
    pub counter_rate_q32: u64,
}

impl TimeMap {
    /// The mapping instant as seconds since the UTC epoch used by the device.
    pub fn utc_seconds(&self) -> f64 {
        self.utc_q30 as f64 / (1u64 << 30) as f64
    }

    /// The estimated counter rate in Hz.
    pub fn counter_rate_hz(&self) -> f64 {
        self.counter_rate_q32 as f64 / (1u64 << 32) as f64
    }
}

/// A decoded port 0 message.
#[derive(Debug, Clone, PartialEq)]
pub struct Port0Msg {
    /// The operation.
    pub op: Port0Op,
    /// Zero on success, otherwise an error code.
    pub status: u8,
    /// Operation-specific argument.
    pub arg: u16,
    /// The decoded body.
    pub body: Port0Body,
}

/// The operation-specific body of a port 0 message.
#[derive(Debug, Clone, PartialEq)]
pub enum Port0Body {
    /// Connect succeeded and reported these versions.
    Connect(ConnectInfo),
    /// Connect failed; the status field carries the reason.
    ConnectFailed,
    /// Echo payload, returned verbatim.
    Echo(Vec<u8>),
    /// Time synchronization request or response.
    TimeSync(TimeSync),
    /// Sample-counter to UTC mapping.
    TimeMap(TimeMap),
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("caller checked length"))
}

fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("caller checked length"))
}

impl Port0Msg {
    /// Parse a port 0 message from a frame payload.
    pub fn decode(payload: &[u8]) -> Result<Self, Port0Error> {
        if payload.len() < HEADER_LEN {
            return Err(Port0Error::Short {
                have: payload.len(),
                need: HEADER_LEN,
            });
        }
        let raw_op = payload[0];
        let op = Port0Op::from_u8(raw_op).ok_or(Port0Error::UnknownOp { op: raw_op })?;
        let status = payload[1];
        let arg = u16::from_le_bytes([payload[2], payload[3]]);
        let data = &payload[HEADER_LEN..];

        let need = |n: usize| -> Result<(), Port0Error> {
            if data.len() < n {
                Err(Port0Error::Short {
                    have: data.len() + HEADER_LEN,
                    need: n + HEADER_LEN,
                })
            } else {
                Ok(())
            }
        };

        let body = match op {
            Port0Op::Connect => {
                if status != 0 || data.is_empty() {
                    Port0Body::ConnectFailed
                } else {
                    need(20)?;
                    Port0Body::Connect(ConnectInfo {
                        protocol_version: Version::from_u32(rd_u32(data, 0)),
                        app_id: rd_u32(data, 4),
                        fw_version: Version::from_u32(rd_u32(data, 8)),
                        hw_version: Version::from_u32(rd_u32(data, 12)),
                        fpga_version: Version::from_u32(rd_u32(data, 16)),
                    })
                }
            }
            Port0Op::Echo => Port0Body::Echo(data.to_vec()),
            Port0Op::TimeSync => {
                need(40)?;
                // Index 0 is reserved and always zero.
                Port0Body::TimeSync(TimeSync {
                    start_count: rd_u64(data, 8),
                    utc_recv: rd_u64(data, 16) as i64,
                    utc_send: rd_u64(data, 24) as i64,
                    end_count: rd_u64(data, 32),
                })
            }
            Port0Op::TimeMap => {
                need(32)?;
                Port0Body::TimeMap(TimeMap {
                    utc_q30: rd_u64(data, 8) as i64,
                    counter: rd_u64(data, 16),
                    counter_rate_q32: rd_u64(data, 24),
                })
            }
        };
        Ok(Self {
            op,
            status,
            arg,
            body,
        })
    }
}

/// Build the bulk OUT response to a device time synchronization request.
///
/// `start_count` is copied from the request; `utc_recv` and `utc_send` are the
/// host timestamps bracketing this turnaround. The device fills in the final
/// counter itself, so it is sent as zero.
pub fn encode_timesync_response(start_count: u64, utc_recv: i64, utc_send: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 40);
    out.push(Port0Op::TimeSync as u8);
    out.push(0); // status
    out.extend_from_slice(&0u16.to_le_bytes()); // arg
    out.extend_from_slice(&0u64.to_le_bytes()); // reserved
    out.extend_from_slice(&start_count.to_le_bytes());
    out.extend_from_slice(&utc_recv.to_le_bytes());
    out.extend_from_slice(&utc_send.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // device fills in end_count
    out
}

/// Build a bulk OUT echo request carrying `data`.
pub fn encode_echo(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.push(Port0Op::Echo as u8);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Failures while decoding a port 0 message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port0Error {
    /// The payload was shorter than the operation requires.
    Short {
        /// Bytes available.
        have: usize,
        /// Bytes required.
        need: usize,
    },
    /// The opcode was not recognized.
    UnknownOp {
        /// The unrecognized opcode.
        op: u8,
    },
}

impl core::fmt::Display for Port0Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short { have, need } => write!(f, "port0 message too short: {have} < {need}"),
            Self::UnknownOp { op } => write!(f, "unknown port0 opcode {op}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Port0Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(op: u8, status: u8, arg: u16) -> Vec<u8> {
        let mut v = alloc::vec![op, status];
        v.extend_from_slice(&arg.to_le_bytes());
        v
    }

    #[test]
    fn version_packs_as_major8_minor8_patch16() {
        let v = Version {
            major: 1,
            minor: 2,
            patch: 300,
        };
        assert_eq!(v.to_u32(), 0x0102_012c);
        assert_eq!(Version::from_u32(0x0102_012c), v);
        assert_eq!(v.to_string(), "1.2.300");
    }

    #[test]
    fn connect_response_decodes_all_versions() {
        let mut p = hdr(Port0Op::Connect as u8, 0, 0);
        for v in [0x0100_0000u32, 0xabcd_1234, 0x0103_0007, 0x0200_0001, 0] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        let msg = Port0Msg::decode(&p).unwrap();
        assert_eq!(msg.op, Port0Op::Connect);
        let Port0Body::Connect(info) = msg.body else {
            panic!("expected connect body");
        };
        assert_eq!(info.protocol_version.major, 1);
        assert_eq!(info.app_id, 0xabcd_1234);
        assert_eq!(info.fw_version, Version::from_u32(0x0103_0007));
        assert_eq!(info.hw_version, Version::from_u32(0x0200_0001));
        assert_eq!(info.fpga_version, Version::default());
    }

    #[test]
    fn connect_failure_is_reported_without_a_payload() {
        let p = hdr(Port0Op::Connect as u8, 7, 0);
        let msg = Port0Msg::decode(&p).unwrap();
        assert_eq!(msg.status, 7);
        assert_eq!(msg.body, Port0Body::ConnectFailed);
    }

    #[test]
    fn timesync_request_decodes_and_response_mirrors_the_counter() {
        let mut p = hdr(Port0Op::TimeSync as u8, 0, 0);
        p.extend_from_slice(&0u64.to_le_bytes());
        p.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        p.extend_from_slice(&[0u8; 24]);
        let msg = Port0Msg::decode(&p).unwrap();
        let Port0Body::TimeSync(ts) = msg.body else {
            panic!("expected timesync body");
        };
        assert_eq!(ts.start_count, 0x1122_3344_5566_7788);

        let resp = encode_timesync_response(ts.start_count, 111, 222);
        let back = Port0Msg::decode(&resp).unwrap();
        let Port0Body::TimeSync(ts2) = back.body else {
            panic!("expected timesync body");
        };
        assert_eq!(ts2.start_count, ts.start_count);
        assert_eq!(ts2.utc_recv, 111);
        assert_eq!(ts2.utc_send, 222);
        assert_eq!(ts2.end_count, 0);
    }

    #[test]
    fn timemap_fixed_point_converts_to_floats() {
        let mut p = hdr(Port0Op::TimeMap as u8, 0, 0);
        p.extend_from_slice(&0u64.to_le_bytes());
        p.extend_from_slice(&((5i64) << 30).to_le_bytes());
        p.extend_from_slice(&1_000_000u64.to_le_bytes());
        p.extend_from_slice(&(2_000_000u64 << 32).to_le_bytes());
        let msg = Port0Msg::decode(&p).unwrap();
        let Port0Body::TimeMap(tm) = msg.body else {
            panic!("expected timemap body");
        };
        assert_eq!(tm.counter, 1_000_000);
        assert!((tm.utc_seconds() - 5.0).abs() < 1e-9);
        assert!((tm.counter_rate_hz() - 2_000_000.0).abs() < 1e-3);
    }

    #[test]
    fn echo_returns_the_payload_verbatim() {
        let req = encode_echo(&[1, 2, 3, 4]);
        let msg = Port0Msg::decode(&req).unwrap();
        assert_eq!(msg.body, Port0Body::Echo(alloc::vec![1, 2, 3, 4]));
    }

    #[test]
    fn unknown_opcode_is_rejected() {
        assert_eq!(
            Port0Msg::decode(&hdr(99, 0, 0)).unwrap_err(),
            Port0Error::UnknownOp { op: 99 }
        );
    }

    #[test]
    fn truncated_timemap_is_rejected() {
        let mut p = hdr(Port0Op::TimeMap as u8, 0, 0);
        p.extend_from_slice(&[0u8; 16]);
        assert!(matches!(
            Port0Msg::decode(&p).unwrap_err(),
            Port0Error::Short { .. }
        ));
    }

    #[test]
    fn empty_payload_is_rejected() {
        assert!(matches!(
            Port0Msg::decode(&[]).unwrap_err(),
            Port0Error::Short { .. }
        ));
    }
}
