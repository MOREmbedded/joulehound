//! Error type for the driver.

use std::fmt;

use js220_proto::frame::FrameError;
use js220_proto::port0::Port0Error;
use js220_proto::pubsub::PubSubError;
use js220_proto::stats::StatsError;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Anything that can go wrong talking to a JS220.
#[derive(Debug)]
pub enum Error {
    /// No matching device is attached.
    NotFound {
        /// The serial number that was requested, if any.
        serial: Option<String>,
    },
    /// The USB layer reported a failure.
    Usb {
        /// What the driver was attempting.
        context: &'static str,
        /// The underlying error.
        source: nusb::Error,
    },
    /// A USB transfer failed.
    Transfer {
        /// What the driver was attempting.
        context: &'static str,
        /// The underlying error.
        source: nusb::transfer::TransferError,
    },
    /// The device rejected a vendor control request.
    ControlRejected {
        /// The request that was rejected.
        request: u8,
        /// The status byte the device returned.
        status: u8,
    },
    /// The device did not respond within the allotted time.
    Timeout {
        /// What the driver was waiting for.
        context: &'static str,
    },
    /// The device reported an unsupported protocol major version.
    ProtocolVersion {
        /// The major version the device speaks.
        device: u8,
        /// The major version this driver implements.
        supported: u8,
    },
    /// A frame could not be decoded.
    Frame(FrameError),
    /// A port 0 message could not be decoded.
    Port0(Port0Error),
    /// A pub/sub message could not be decoded.
    PubSub(PubSubError),
    /// A statistics record could not be decoded.
    Stats(StatsError),
    /// A message was too large to fit in a frame.
    MessageTooLarge {
        /// The topic that could not be encoded.
        topic: String,
    },
    /// The requested configuration is not valid.
    Config {
        /// Why it was rejected.
        reason: String,
    },
    /// Writing captured data failed.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { serial: Some(s) } => write!(f, "no JS220 with serial number {s}"),
            Self::NotFound { serial: None } => write!(f, "no JS220 attached"),
            Self::Usb { context, source } => write!(f, "usb error while {context}: {source}"),
            Self::Transfer { context, source } => {
                write!(f, "transfer failed while {context}: {source}")
            }
            Self::ControlRejected { request, status } => write!(
                f,
                "device rejected control request {request} with status {status}"
            ),
            Self::Timeout { context } => write!(f, "timed out waiting for {context}"),
            Self::ProtocolVersion { device, supported } => write!(
                f,
                "device speaks protocol version {device}, this driver implements {supported}"
            ),
            Self::Frame(e) => write!(f, "{e}"),
            Self::Port0(e) => write!(f, "{e}"),
            Self::PubSub(e) => write!(f, "{e}"),
            Self::Stats(e) => write!(f, "{e}"),
            Self::MessageTooLarge { topic } => {
                write!(f, "value for topic {topic} does not fit in one frame")
            }
            Self::Config { reason } => write!(f, "invalid configuration: {reason}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Usb { source, .. } => Some(source),
            Self::Transfer { source, .. } => Some(source),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<FrameError> for Error {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

impl From<Port0Error> for Error {
    fn from(e: Port0Error) -> Self {
        Self::Port0(e)
    }
}

impl From<PubSubError> for Error {
    fn from(e: PubSubError) -> Self {
        Self::PubSub(e)
    }
}

impl From<StatsError> for Error {
    fn from(e: StatsError) -> Self {
        Self::Stats(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Attach a static context string to a `nusb` error.
pub(crate) trait UsbContext<T> {
    fn usb_context(self, context: &'static str) -> Result<T>;
}

impl<T> UsbContext<T> for std::result::Result<T, nusb::Error> {
    fn usb_context(self, context: &'static str) -> Result<T> {
        self.map_err(|source| Error::Usb { context, source })
    }
}

impl<T> UsbContext<T> for std::result::Result<T, nusb::transfer::TransferError> {
    fn usb_context(self, context: &'static str) -> Result<T> {
        self.map_err(|source| Error::Transfer { context, source })
    }
}
