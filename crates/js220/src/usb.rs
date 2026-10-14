//! USB enumeration and the bulk/control transport.

use std::time::Duration;

use nusb::transfer::{Buffer, Bulk, Completion, ControlIn, ControlType, In, Out, Recipient};
use nusb::{Endpoint, Interface, MaybeFuture};

use js220_proto as proto;
use js220_proto::frame::{encode_frame, FRAME_LEN, PAYLOAD_MAX};
use js220_proto::port0::{ConnectMode, CtrlOp};

use crate::error::{Error, Result, UsbContext};

/// Which firmware image a device is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal measurement firmware.
    Application,
    /// Bootloader, used for firmware updates.
    Updater,
}

/// An attached JS220.
#[derive(Clone)]
pub struct DeviceInfo {
    inner: nusb::DeviceInfo,
    serial: String,
    mode: Mode,
}

impl DeviceInfo {
    /// The device serial number. Devices that do not report one are listed with
    /// a placeholder derived from their bus address, so they remain selectable.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Which firmware image the device is running.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The USB product id.
    pub fn product_id(&self) -> u16 {
        self.inner.product_id()
    }

    /// The reported bus speed, if the platform exposes it.
    pub fn speed(&self) -> Option<nusb::Speed> {
        self.inner.speed()
    }

    /// The manufacturer string, if present.
    pub fn manufacturer(&self) -> Option<&str> {
        self.inner.manufacturer_string()
    }

    /// The product string, if present.
    pub fn product(&self) -> Option<&str> {
        self.inner.product_string()
    }

    pub(crate) fn raw(&self) -> &nusb::DeviceInfo {
        &self.inner
    }
}

impl std::fmt::Debug for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceInfo")
            .field("serial", &self.serial)
            .field("mode", &self.mode)
            .field("product_id", &self.inner.product_id())
            .finish()
    }
}

/// Enumerate every attached JS220, in application or updater mode.
pub fn list() -> Result<Vec<DeviceInfo>> {
    let devices = nusb::list_devices().wait().usb_context("listing devices")?;
    let mut out = Vec::new();
    for d in devices {
        if d.vendor_id() != proto::USB_VID {
            continue;
        }
        let mode = match d.product_id() {
            proto::USB_PID_APP => Mode::Application,
            proto::USB_PID_UPDATER => Mode::Updater,
            _ => continue,
        };
        let serial = d.serial_number().map(str::to_owned).unwrap_or_else(|| {
            // Without a serial string the device still needs a stable handle for
            // selection, so fall back to its bus position.
            format!("bus{}-addr{}", d.bus_id(), d.device_address())
        });
        out.push(DeviceInfo {
            inner: d,
            serial,
            mode,
        });
    }
    out.sort_by(|a, b| a.serial.cmp(&b.serial));
    Ok(out)
}

/// Find one device, optionally filtered by serial number.
pub fn find(serial: Option<&str>) -> Result<DeviceInfo> {
    let devices = list()?;
    match serial {
        Some(s) => devices
            .into_iter()
            .find(|d| d.serial() == s)
            .ok_or_else(|| Error::NotFound {
                serial: Some(s.to_owned()),
            }),
        None => devices
            .into_iter()
            .next()
            .ok_or(Error::NotFound { serial: None }),
    }
}

/// Issue a USB port reset to the device.
///
/// This is the software equivalent of unplugging and replugging it. Use it when
/// a device has been left in a bad state by an interrupted session: the
/// instrument keeps streaming configuration across a mere disconnect, so a
/// reset is sometimes the only way back to a known state without physical
/// access.
///
/// The device re-enumerates afterwards and may briefly disappear, so give it a
/// moment before opening it again.
pub fn reset(info: &DeviceInfo) -> Result<()> {
    let device = info
        .raw()
        .open()
        .wait()
        .usb_context("opening device for reset")?;
    device.reset().wait().usb_context("resetting device")?;
    Ok(())
}

/// Tuning for the bulk IN pipeline.
#[derive(Debug, Clone, Copy)]
pub struct TransportConfig {
    /// Bytes per bulk IN transfer. Rounded up to a whole number of frames.
    ///
    /// Larger transfers mean fewer completions per second and less per-transfer
    /// overhead, at the cost of latency and pinned memory.
    pub transfer_size: usize,
    /// How many bulk IN transfers to keep queued.
    ///
    /// The device must never find the queue empty or it will drop samples, so
    /// this needs to cover the worst-case scheduling delay of the consumer.
    pub transfers_in_flight: usize,
    /// Bytes per bulk IN transfer while configuring rather than streaming.
    ///
    /// Every frame the device sends is exactly 512 bytes, which is also the
    /// endpoint's maximum packet size, so a transfer is never terminated early
    /// by a short packet: it completes only once it is full. A large transfer
    /// therefore holds a trickle of configuration replies hostage until enough
    /// of them accumulate to fill it. Configuration uses a small transfer so
    /// replies arrive promptly; streaming switches to `transfer_size`, where
    /// the data rate fills buffers in milliseconds.
    pub config_transfer_size: usize,
    /// Timeout applied to control transfers.
    pub control_timeout: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            // 128 KiB is 256 frames; at the 1 Msps ceiling of ~8 MB/s this is
            // ~60 completions per second per queued transfer.
            transfer_size: 128 * 1024,
            transfers_in_flight: 8,
            // Eight frames: enough to batch a burst of replies, small enough
            // that a single reply is never delayed by more than a few frames.
            config_transfer_size: 8 * FRAME_LEN,
            control_timeout: Duration::from_millis(1000),
        }
    }
}

impl TransportConfig {
    /// The streaming transfer size: rounded up to a whole frame and at least
    /// one frame.
    pub fn effective_transfer_size(&self) -> usize {
        Self::round_to_frames(self.transfer_size)
    }

    /// The configuration transfer size, rounded the same way.
    pub fn effective_config_transfer_size(&self) -> usize {
        Self::round_to_frames(self.config_transfer_size)
    }

    fn round_to_frames(n: usize) -> usize {
        n.div_ceil(FRAME_LEN).max(1) * FRAME_LEN
    }

    /// Bytes of host memory the IN queue will pin.
    pub fn queued_bytes(&self) -> usize {
        self.effective_transfer_size() * self.transfers_in_flight.max(1)
    }
}

/// An open USB link to a JS220: one claimed interface plus its bulk endpoints.
pub struct Link {
    #[allow(dead_code)] // held so the interface outlives its endpoints
    interface: Interface,
    ep_in: Endpoint<Bulk, In>,
    ep_out: Endpoint<Bulk, Out>,
    out_frame_id: u16,
    cfg: TransportConfig,
    transfer_size: usize,
}

impl Link {
    /// Open `info` and claim the data interface.
    pub fn open(info: &DeviceInfo, cfg: TransportConfig) -> Result<Self> {
        let device = info.raw().open().wait().usb_context("opening device")?;
        let interface = device
            .claim_interface(proto::USB_INTERFACE)
            .wait()
            .usb_context("claiming interface 0")?;
        let ep_in = interface
            .endpoint::<Bulk, In>(proto::EP_BULK_IN)
            .usb_context("opening bulk IN endpoint")?;
        let ep_out = interface
            .endpoint::<Bulk, Out>(proto::EP_BULK_OUT)
            .usb_context("opening bulk OUT endpoint")?;

        // Start small so the connect response and configuration replies are
        // delivered as they arrive rather than waiting for a large transfer to
        // fill. `set_streaming_transfers` switches over once data is flowing.
        let transfer_size = cfg.effective_config_transfer_size();
        let mut link = Self {
            interface,
            ep_in,
            ep_out,
            out_frame_id: 0,
            cfg,
            transfer_size,
        };

        // A session that was interrupted mid-stream can leave an endpoint
        // halted, and a halted bulk IN pipe delivers nothing while every
        // control request still succeeds. Clearing both is harmless when they
        // are already healthy.
        link.clear_halts();
        Ok(link)
    }

    /// Clear any halt condition on both bulk endpoints.
    fn clear_halts(&mut self) {
        if let Err(e) = self.ep_in.clear_halt().wait() {
            log::debug!("clearing bulk IN halt: {e}");
        }
        if let Err(e) = self.ep_out.clear_halt().wait() {
            log::debug!("clearing bulk OUT halt: {e}");
        }
    }

    /// The transport tuning in effect.
    pub fn config(&self) -> &TransportConfig {
        &self.cfg
    }

    /// Issue a vendor control request and return the device status byte.
    fn control(&mut self, request: u8, value: u16) -> Result<u8> {
        let data = self
            .interface
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request,
                    value,
                    index: 0,
                    length: 1,
                },
                self.cfg.control_timeout,
            )
            .wait()
            .usb_context("issuing vendor control request")?;
        data.first()
            .copied()
            .ok_or(Error::ControlRejected { request, status: 0 })
    }

    /// Ask the device to bring up the sensor link.
    ///
    /// The device acknowledges here and then reports the outcome
    /// asynchronously with a port 0 connect frame on bulk IN.
    pub fn connect(&mut self, mode: ConnectMode) -> Result<()> {
        let status = self.control(CtrlOp::Connect as u8, mode as u16)?;
        if status != 0 {
            return Err(Error::ControlRejected {
                request: CtrlOp::Connect as u8,
                status,
            });
        }
        Ok(())
    }

    /// Tell the device the host is going away.
    ///
    /// Also used immediately before [`Link::connect`] to flush anything the
    /// device still has queued for bulk IN. Errors are returned but are safe to
    /// ignore in that flushing role.
    pub fn disconnect(&mut self) -> Result<()> {
        let status = self.control(CtrlOp::Disconnect as u8, 0)?;
        if status != 0 {
            return Err(Error::ControlRejected {
                request: CtrlOp::Disconnect as u8,
                status,
            });
        }
        Ok(())
    }

    /// Switch the bulk IN queue to the large streaming transfer size.
    ///
    /// Re-queues every buffer, so call this once configuration is complete and
    /// before throughput matters.
    pub fn set_streaming_transfers(&mut self) {
        self.retune(self.cfg.effective_transfer_size());
    }

    /// Switch the bulk IN queue back to the small configuration size.
    pub fn set_config_transfers(&mut self) {
        self.retune(self.cfg.effective_config_transfer_size());
    }

    fn retune(&mut self, size: usize) {
        if self.transfer_size == size {
            return;
        }
        log::debug!("bulk IN transfer size {} -> {size}", self.transfer_size);
        self.cancel_in();
        self.transfer_size = size;
        self.prime();
    }

    /// Bytes per queued bulk IN transfer right now.
    pub fn transfer_size(&self) -> usize {
        self.transfer_size
    }

    /// Fill the bulk IN queue up to the configured depth.
    ///
    /// Call this before the device starts streaming and after draining
    /// completions, so the device always has somewhere to put data.
    pub fn prime(&mut self) {
        while self.ep_in.pending() < self.cfg.transfers_in_flight {
            let buf = self.ep_in.allocate(self.transfer_size);
            self.ep_in.submit(buf);
        }
    }

    /// Number of bulk IN transfers currently queued.
    pub fn pending(&self) -> usize {
        self.ep_in.pending()
    }

    /// Wait for the next completed bulk IN transfer.
    ///
    /// Returns `Ok(None)` on timeout. The caller owns the returned buffer and
    /// should hand it back to [`Link::recycle`] once the data has been
    /// consumed, which requeues it without reallocating.
    pub fn next_in(&mut self, timeout: Duration) -> Result<Option<Completion>> {
        let Some(completion) = self.ep_in.wait_next_complete(timeout) else {
            return Ok(None);
        };
        if let Err(source) = completion.status {
            return Err(Error::Transfer {
                context: "receiving bulk IN data",
                source,
            });
        }
        Ok(Some(completion))
    }

    /// Return a consumed IN buffer to the queue.
    pub fn recycle(&mut self, mut buf: Buffer) {
        // A completed IN buffer carries the received length; restore the full
        // request so the next transfer uses the whole allocation.
        buf.clear();
        buf.set_requested_len(self.transfer_size);
        self.ep_in.submit(buf);
    }

    /// Cancel every queued bulk IN transfer and drain their completions.
    pub fn cancel_in(&mut self) {
        self.ep_in.cancel_all();
        // Cancelled transfers still complete; drain them so the endpoint can be
        // dropped or re-primed cleanly.
        while self.ep_in.pending() > 0 {
            if self
                .ep_in
                .wait_next_complete(Duration::from_millis(250))
                .is_none()
            {
                break;
            }
        }
    }

    /// Send one frame on bulk OUT and wait for it to complete.
    pub fn send_frame(&mut self, port_id: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > PAYLOAD_MAX {
            return Err(Error::Config {
                reason: format!(
                    "bulk OUT payload of {} bytes exceeds the {PAYLOAD_MAX} byte frame limit",
                    payload.len()
                ),
            });
        }
        let mut frame = vec![0u8; FRAME_LEN];
        encode_frame(&mut frame, self.out_frame_id, port_id, payload)
            .expect("frame sized and payload length checked above");
        self.out_frame_id = self.out_frame_id.wrapping_add(1);

        self.ep_out.submit(Buffer::from(frame));
        let completion = self
            .ep_out
            .wait_next_complete(self.cfg.control_timeout)
            .ok_or(Error::Timeout {
                context: "bulk OUT completion",
            })?;
        completion
            .status
            .usb_context("sending bulk OUT frame")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_size_rounds_up_to_whole_frames() {
        let cfg = TransportConfig {
            transfer_size: FRAME_LEN + 1,
            ..Default::default()
        };
        assert_eq!(cfg.effective_transfer_size(), FRAME_LEN * 2);
    }

    #[test]
    fn transfer_size_is_never_zero() {
        let cfg = TransportConfig {
            transfer_size: 0,
            ..Default::default()
        };
        assert_eq!(cfg.effective_transfer_size(), FRAME_LEN);
    }

    #[test]
    fn exact_multiples_are_left_alone() {
        let cfg = TransportConfig {
            transfer_size: FRAME_LEN * 64,
            ..Default::default()
        };
        assert_eq!(cfg.effective_transfer_size(), FRAME_LEN * 64);
    }

    #[test]
    fn default_transfer_size_is_a_whole_number_of_frames() {
        let cfg = TransportConfig::default();
        assert_eq!(cfg.transfer_size % FRAME_LEN, 0);
        assert_eq!(cfg.effective_transfer_size(), cfg.transfer_size);
    }

    #[test]
    fn queued_bytes_accounts_for_the_whole_pipeline() {
        let cfg = TransportConfig {
            transfer_size: 64 * 1024,
            transfers_in_flight: 4,
            ..Default::default()
        };
        assert_eq!(cfg.queued_bytes(), 256 * 1024);
    }

    #[test]
    fn queued_bytes_treats_a_zero_depth_as_one() {
        let cfg = TransportConfig {
            transfer_size: FRAME_LEN,
            transfers_in_flight: 0,
            ..Default::default()
        };
        assert_eq!(cfg.queued_bytes(), FRAME_LEN);
    }
}
