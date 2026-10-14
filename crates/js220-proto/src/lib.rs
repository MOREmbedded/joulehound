//! Wire protocol codec for the Joulescope JS220 precision energy analyzer.
//!
//! This crate is pure encoding and decoding: it performs no I/O, owns no
//! threads, and holds no device handles. Everything here can be exercised
//! without hardware, which is why the bulk of the test suite lives at this
//! layer. The [`js220`](https://docs.rs/js220) crate layers USB transport and
//! device state on top.
//!
//! # Layering
//!
//! ```text
//! bulk transfer  -> frame::FrameIter   -> one Frame per 512 bytes
//!   port 0       -> port0::Port0Msg    -> connect, echo, time sync
//!   port 1       -> pubsub::PubSubMsg  -> topic + value
//!   port >= 16   -> stream::*          -> sample id + channel data
//!     index 14   -> stats::Statistics  -> on-instrument statistics
//! ```
//!
//! # Example
//!
//! Walk a bulk IN transfer and pick out the current samples:
//!
//! ```
//! use js220_proto::frame::{FrameIter, PORT_STREAM_BASE};
//! use js220_proto::stream::{self, IDX_CURRENT};
//!
//! # fn demo(transfer: &[u8]) {
//! let mut samples: Vec<f32> = Vec::new();
//! let mut sample_id = stream::SampleIdUnwrap::new();
//!
//! for frame in FrameIter::new(transfer).flatten() {
//!     if frame.header.port_id != PORT_STREAM_BASE + IDX_CURRENT {
//!         continue;
//!     }
//!     if let Some((raw_id, data)) = stream::split_sample_id(frame.payload) {
//!         sample_id.update(raw_id);
//!         stream::decode_f32(data, &mut samples);
//!     }
//! }
//! # }
//! ```
//!
//! # Endianness
//!
//! Every multi-byte field is little-endian. Decoders read through
//! `from_le_bytes` rather than casting structs, so they are correct on
//! big-endian hosts and tolerate unaligned buffers.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod frame;
pub mod port0;
pub mod pubsub;
pub mod stats;
pub mod stream;
pub mod topics;
pub mod value;

/// USB vendor id shared by every Joulescope.
pub const USB_VID: u16 = 0x16d0;
/// USB product id of the JS220 in application mode.
pub const USB_PID_APP: u16 = 0x10ba;
/// USB product id of the JS220 in bootloader/updater mode.
pub const USB_PID_UPDATER: u16 = 0x10b9;

/// Bulk IN endpoint address.
pub const EP_BULK_IN: u8 = 0x82;
/// Bulk OUT endpoint address.
pub const EP_BULK_OUT: u8 = 0x01;
/// USB configuration to select.
pub const USB_CONFIGURATION: u8 = 1;
/// USB interface to claim.
pub const USB_INTERFACE: u8 = 0;

/// Device interface GUID advertised by the JS220 in application mode, used to
/// find the device through WinUSB on Windows.
pub const WINUSB_GUID_APP: &str = "{e27188c8-98ff-41de-be50-653324c6b19c}";
/// Device interface GUID advertised by the JS220 updater.
pub const WINUSB_GUID_UPDATER: &str = "{0149f96d-54af-4a91-88a9-1652cb1ecc1f}";

/// Protocol major version this crate implements.
pub const PROTOCOL_VERSION_MAJOR: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_addresses_carry_the_expected_directions() {
        assert_eq!(EP_BULK_IN & 0x80, 0x80, "bulk IN must have the IN bit set");
        assert_eq!(EP_BULK_OUT & 0x80, 0, "bulk OUT must have the IN bit clear");
    }

    #[test]
    fn app_and_updater_product_ids_differ() {
        assert_ne!(USB_PID_APP, USB_PID_UPDATER);
    }
}
