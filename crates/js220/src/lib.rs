//! Fast host driver for the Joulescope JS220 precision energy analyzer.
//!
//! This crate owns the USB transport and the device session. Wire format
//! encoding and decoding lives in [`js220_proto`], which has no I/O and can be
//! tested without hardware.
//!
//! # Getting samples
//!
//! ```no_run
//! use std::time::Duration;
//! use js220::{Js220, Signal, StreamConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut dev = Js220::open_first()?;
//! println!("firmware {}", dev.connect_info().fw_version);
//!
//! dev.stream_start(StreamConfig {
//!     signals: vec![Signal::Current, Signal::Voltage, Signal::Power],
//!     sample_rate: 1_000_000,
//!     ..Default::default()
//! })?;
//!
//! loop {
//!     dev.poll(Duration::from_millis(100))?;
//!     let rows = dev.rows();
//!     let n = rows.len;
//!     if let Some(current) = rows.current {
//!         // `current[i]` is amperes at sample id
//!         // `rows.sample_id + i as u64 * rows.decimate as u64`
//!         let _ = current;
//!     }
//!     dev.consume(n);
//! }
//! # }
//! ```
//!
//! # Threading
//!
//! [`Js220`] is single-threaded by design: one thread owns the device and
//! drives [`Js220::poll`]. Moving decoded samples to another thread for
//! writing or display is the caller's choice, which keeps the hot path free of
//! locks. `jsctl capture` does exactly this, with a bounded channel to a
//! writer thread.
//!
//! # Where the throughput comes from
//!
//! - The bulk IN queue is deep and wide by default (8 transfers of 128 KiB),
//!   so the device always has somewhere to put data and the host wakes up far
//!   less often per megabyte.
//! - Completed transfers are parsed in place and the buffer goes straight back
//!   into the queue, so steady-state streaming does not allocate.
//! - Sample rates below 1 Msps use the instrument's own decimation filter, so
//!   the host never receives samples it would only discard.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod decimate;
pub mod defaults;
pub mod device;
pub mod error;
pub mod signals;
pub mod time;
pub mod usb;

pub use device::{
    Counters, CurrentRange, Js220, Rows, StreamConfig, VoltageRange, BASE_RATE, MAX_SAMPLE_RATE,
    MIN_SAMPLE_RATE,
};
pub use error::{Error, Result};
pub use signals::{Signal, SignalCounters};
pub use usb::{find, list, DeviceInfo, Mode, TransportConfig};

/// Re-export of the protocol crate, for callers that need the wire types.
pub use js220_proto as proto;
