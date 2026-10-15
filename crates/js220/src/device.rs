//! Device session: opening, configuration, and the frame dispatch loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use js220_proto::frame::{self, FrameIter};
use js220_proto::port0::{ConnectInfo, ConnectMode, Port0Body, Port0Msg, Port0Op};
use js220_proto::pubsub::{PubSubMsg, TOPIC_METADATA_ALL, TOPIC_QUERY_ALL};
use js220_proto::stats::{self, Statistics};
use js220_proto::stream as pstream;
use js220_proto::topics;
use js220_proto::value::{flags, Value};

use crate::error::{Error, Result};
use crate::signals::{Signal, SignalGroup};
use crate::time::{self, TimeMap};
use crate::usb::{DeviceInfo, Link, Mode, TransportConfig};

/// Base rate of the sample id counter.
pub const BASE_RATE: u32 = pstream::BASE_SAMPLE_RATE;
/// Highest rate the analog signals can stream at.
pub const MAX_SAMPLE_RATE: u32 = 1_000_000;
/// Lowest rate the instrument can decimate to on its own.
pub const MIN_SAMPLE_RATE: u32 = 1_000;

/// How long to wait for a port 0 echo to come back.
const ECHO_TIMEOUT: Duration = Duration::from_millis(500);

/// How many echoes to try before declaring the output path dead.
const ECHO_ATTEMPTS: u32 = 10;

/// How long to let the device settle after a state change before trusting what
/// arrives to belong to the new state.
const SETTLE: Duration = Duration::from_millis(250);

// Range enumerations, taken from the options tables the instrument publishes in
// its own metadata. Run `jsctl topics` to print them from a live device.

/// `s/i/range/mode` value for auto-ranging.
const RANGE_MODE_AUTO: u8 = 4;
/// `s/i/range/mode` value for a host-selected fixed range.
const RANGE_MODE_MANUAL: u8 = 5;
/// `s/i/range/mode` value disabling range switching.
const RANGE_MODE_OFF: u8 = 0;

/// Convert a current range index to the instrument's select bitmap.
///
/// `s/i/range/select` is a bitmap, not an index: bit 7 enables manual
/// selection and one of bits 0..=5 picks the range. Index 0 is the 10 A range
/// and index 5 is the 18 µA range, so larger indices are *more* sensitive.
const fn current_range_select(index: u8) -> u8 {
    0x80 | (1 << index)
}

/// `s/v/range/mode` value for auto-ranging.
const V_RANGE_MODE_AUTO: u8 = 0;
/// `s/v/range/mode` value for a host-selected fixed range.
///
/// The voltage range mode is a different enumeration from the current range
/// mode: it is simply `0=auto, 1=manual`.
const V_RANGE_MODE_MANUAL: u8 = 1;
/// Voltage range selector for the 2 V range.
const V_RANGE_2V: u8 = 1;
/// Voltage range selector for the 15 V range.
const V_RANGE_15V: u8 = 0;

/// Pick a starting ping value unlikely to collide with a previous session's.
///
/// Only the low byte survives the device's round trip, so the seed has to fit
/// in a `u8`. That is a small space, but it still makes accepting a stale pong
/// from an earlier process unlikely rather than near-certain, which a fixed
/// start of 1 made it.
fn session_ping_seed() -> u8 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.subsec_nanos());
    (nanos % 251) as u8 + 1
}

/// Which acquisition path to stream.
///
/// The instrument has two, and they trade accuracy against rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    /// Calibrated current and voltage, up to [`MAX_SAMPLE_RATE`].
    ///
    /// This is the measurement path: gain, offset and range correction are
    /// applied on the instrument and the samples arrive in amperes and volts.
    #[default]
    Calibrated,
    /// Raw ADC counts at the full [`BASE_RATE`], twice the calibrated rate.
    ///
    /// Nothing is applied to these samples: no gain, no offset, no range
    /// correction, and no indication of which range was active. They are
    /// useful for seeing fast edges, not for absolute measurement, and must
    /// not be presented as amperes or volts.
    RawAdc,
}

impl Source {
    /// The rate this source streams at.
    pub const fn rate(self) -> u32 {
        match self {
            Self::Calibrated => MAX_SAMPLE_RATE,
            Self::RawAdc => BASE_RATE,
        }
    }

    /// Sample id step between consecutive samples from this source.
    pub const fn sample_step(self) -> u32 {
        BASE_RATE / self.rate()
    }

    /// Unit of the samples, for display.
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Calibrated => "",
            Self::RawAdc => "counts",
        }
    }
}

/// Current range selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurrentRange {
    /// The instrument picks the range.
    #[default]
    Auto,
    /// Hold a fixed range, 0 (most sensitive) through 5.
    Manual(u8),
    /// Disable range switching entirely.
    Off,
}

/// Voltage range selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoltageRange {
    /// The instrument picks the range.
    #[default]
    Auto,
    /// Hold the 2 V range.
    V2,
    /// Hold the 15 V range.
    V15,
}

/// What to stream and how.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Which of the instrument's two acquisition paths to stream.
    pub source: Source,
    /// Signals to deliver. Requesting power implicitly enables current and
    /// voltage, since power is computed on the host.
    ///
    /// Ignored for [`Source::RawAdc`], which delivers the ADC channels named
    /// by [`StreamConfig::adc_channels`] instead.
    pub signals: Vec<Signal>,
    /// The two ADC channels to stream for [`Source::RawAdc`].
    ///
    /// The instrument exposes four and publishes no metadata saying what each
    /// one measures, so they are identified by number only.
    pub adc_channels: (u8, u8),
    /// Output sample rate in Hz.
    ///
    /// Must divide 2 MHz exactly and lie between [`MIN_SAMPLE_RATE`] and
    /// [`MAX_SAMPLE_RATE`]. Rates below the maximum are produced by host-side
    /// averaging. [`Source::RawAdc`] ignores this and always runs at
    /// [`BASE_RATE`].
    pub sample_rate: u32,
    /// Enable the on-instrument statistics stream.
    pub statistics: bool,
    /// Statistics block rate in Hz, when statistics are enabled.
    pub statistics_rate: f64,
    /// Current range selection.
    pub current_range: CurrentRange,
    /// Voltage range selection.
    pub voltage_range: VoltageRange,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            source: Source::Calibrated,
            signals: vec![Signal::Current, Signal::Voltage, Signal::Power],
            adc_channels: (0, 1),
            sample_rate: MAX_SAMPLE_RATE,
            statistics: false,
            statistics_rate: 2.0,
            current_range: CurrentRange::Auto,
            voltage_range: VoltageRange::Auto,
        }
    }
}

impl StreamConfig {
    /// Reject configurations the instrument cannot produce.
    pub fn validate(&self) -> Result<()> {
        // Statistics are computed on the instrument and need no sample stream,
        // so a statistics-only session is legitimate and leaves the USB link
        // almost idle.
        if self.signals.is_empty() && !self.statistics {
            return Err(Error::Config {
                reason: "no signals selected and statistics are disabled".into(),
            });
        }
        if self.source == Source::RawAdc {
            for ch in [self.adc_channels.0, self.adc_channels.1] {
                if ch > 3 {
                    return Err(Error::Config {
                        reason: format!("ADC channel {ch} does not exist; the instrument has 0..=3"),
                    });
                }
            }
            if self.adc_channels.0 == self.adc_channels.1 {
                return Err(Error::Config {
                    reason: "the two ADC channels must differ".into(),
                });
            }
            // The rate is fixed by the source, so nothing below applies.
            return Ok(());
        }
        let fs = self.sample_rate;
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&fs) {
            return Err(Error::Config {
                reason: format!(
                    "sample rate {fs} Hz is outside {MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE}"
                ),
            });
        }
        if BASE_RATE % fs != 0 {
            return Err(Error::Config {
                reason: format!("sample rate {fs} Hz does not divide {BASE_RATE} Hz exactly"),
            });
        }
        if self.statistics && !(self.statistics_rate > 0.0 && self.statistics_rate <= 1000.0) {
            return Err(Error::Config {
                reason: format!(
                    "statistics rate {} Hz is outside 0..=1000",
                    self.statistics_rate
                ),
            });
        }
        Ok(())
    }

    /// Sample id increment between consecutive output samples.
    pub fn decimate(&self) -> u32 {
        match self.source {
            Source::RawAdc => 1,
            Source::Calibrated => (BASE_RATE / self.sample_rate).max(1),
        }
    }

    /// The rate samples actually arrive at.
    pub fn effective_rate(&self) -> u32 {
        match self.source {
            Source::RawAdc => BASE_RATE,
            Source::Calibrated => self.sample_rate,
        }
    }

    /// Number of instrument samples averaged into each output sample.
    ///
    /// Raw ADC is never averaged: the point of it is the unprocessed samples.
    pub fn host_decimation(&self) -> u32 {
        match self.source {
            Source::RawAdc => 1,
            Source::Calibrated => (MAX_SAMPLE_RATE / self.sample_rate).max(1),
        }
    }

    /// Signals that must actually be streamed from the device, which is the
    /// requested set with power replaced by its current and voltage operands.
    pub fn device_signals(&self) -> Vec<Signal> {
        let mut out = Vec::with_capacity(2);
        let wants_power = self.signals.contains(&Signal::Power);
        for s in [Signal::Current, Signal::Voltage] {
            if self.signals.contains(&s) || wants_power {
                out.push(s);
            }
        }
        out
    }

    /// Samples per statistics block, counted at 1 Msps.
    pub fn statistics_block_samples(&self) -> u32 {
        let n = (MAX_SAMPLE_RATE as f64 / self.statistics_rate).round();
        n.clamp(1.0, MAX_SAMPLE_RATE as f64 * 10.0) as u32
    }
}

/// Running totals for the receive path.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counters {
    /// Completed bulk IN transfers.
    pub transfers: u64,
    /// Bytes received over bulk IN.
    pub bytes: u64,
    /// Frames parsed.
    pub frames: u64,
    /// Frames that failed to parse.
    pub frame_errors: u64,
    /// Breaks in the device's frame id sequence, each meaning lost frames.
    pub frame_id_breaks: u64,
    /// Frames carrying sample data.
    pub stream_frames: u64,
    /// Frames on ports this driver does not consume.
    pub ignored_frames: u64,
    /// Statistics records decoded.
    pub statistics: u64,
    /// Time synchronization exchanges answered.
    pub timesyncs: u64,
}

/// A span of aligned samples.
#[derive(Debug)]
pub struct Rows<'a> {
    /// Sample id of the first row.
    pub sample_id: u64,
    /// Sample id increment per row.
    pub decimate: u32,
    /// Number of rows in each populated column.
    pub len: usize,
    /// Current in amperes, if requested.
    pub current: Option<&'a [f32]>,
    /// Voltage in volts, if requested.
    pub voltage: Option<&'a [f32]>,
    /// Power in watts, if requested.
    pub power: Option<&'a [f32]>,
}

/// An open JS220.
pub struct Js220 {
    link: Link,
    serial: String,
    connect_info: ConnectInfo,
    group: SignalGroup,
    /// Output buffer for host-computed power.
    power: Vec<f32>,
    /// Reused decode buffer for incoming sample frames.
    scratch: Vec<f32>,
    /// Reused decode buffer for raw ADC counts.
    adc_scratch: Vec<i16>,
    /// ADC channels standing in for the current and voltage slots while
    /// streaming [`Source::RawAdc`].
    adc_map: Option<(u8, u8)>,
    config: Option<StreamConfig>,
    params: HashMap<String, Value>,
    statistics: Option<Statistics>,
    /// Token from the most recent port 0 echo reply.
    last_echo: Option<u32>,
    /// Low byte of the most recent pong, used as a round-trip barrier.
    last_pong: Option<u8>,
    next_ping: u8,
    time_map: TimeMap,
    counters: Counters,
    expect_frame_id: Option<u16>,
    streaming: bool,
}

impl Js220 {
    /// Open the first attached JS220 in application mode.
    pub fn open_first() -> Result<Self> {
        Self::open_with(None, TransportConfig::default())
    }

    /// Open a device by serial number, or the first one when `serial` is `None`.
    pub fn open_with(serial: Option<&str>, transport: TransportConfig) -> Result<Self> {
        let info = crate::usb::find(serial)?;
        Self::open(&info, transport)
    }

    /// Open a specific device.
    pub fn open(info: &DeviceInfo, transport: TransportConfig) -> Result<Self> {
        if info.mode() != Mode::Application {
            return Err(Error::Config {
                reason: format!(
                    "device {} is in updater mode; this driver drives application mode only",
                    info.serial()
                ),
            });
        }
        let mut link = Link::open(info, transport)?;

        // Clear anything the device still has queued from a previous session.
        // A device that was never connected answers with an error here, which
        // is expected and harmless.
        if let Err(e) = link.disconnect() {
            log::debug!("pre-connect disconnect returned {e}");
        }

        // The device starts talking as soon as it is told to connect, so the
        // IN queue has to be armed first or the connect response is lost.
        link.prime();
        link.connect(ConnectMode::Reset)?;

        let mut dev = Self {
            link,
            serial: info.serial().to_owned(),
            connect_info: ConnectInfo::default(),
            group: SignalGroup::new(&[], 2, MAX_SAMPLE_RATE as usize),
            power: Vec::new(),
            scratch: Vec::new(),
            adc_scratch: Vec::new(),
            adc_map: None,
            config: None,
            params: HashMap::new(),
            statistics: None,
            last_echo: None,
            last_pong: None,
            // Seeded per session rather than from a fixed value. The device
            // can still have pongs queued from a previous process, and a
            // counter that always starts at 1 would accept one of those as the
            // answer to a ping it never saw.
            next_ping: session_ping_seed(),
            time_map: TimeMap::new(),
            counters: Counters::default(),
            expect_frame_id: None,
            streaming: false,
        };
        dev.await_connect(Duration::from_millis(3000))?;

        // Let the device finish its own post-connect chatter before returning.
        // Requesting the full metadata and value dump here was tried and
        // removed: those are hundreds of messages, and configuration published
        // on their heels was being dropped. Commands that need the topic tree
        // ask for it explicitly instead.
        dev.drain(SETTLE);

        // Prove the bulk OUT path actually reaches the device before trusting
        // it with configuration. Port 0 echo is a pure loopback: the device
        // returns the payload unchanged, so a completed round trip is positive
        // evidence that frames sent now are being acted on. A bulk OUT
        // transfer completing only means the host controller accepted it,
        // which says nothing about whether the device processed it.
        dev.sync_output()?;

        // Push the full known-good parameter set. Connecting does not leave
        // the instrument in a state the host can rely on, and a session that
        // configured only the few topics it needed leaves the rest as the
        // previous one left them — after which streaming stops starting at
        // all, and no amount of re-enabling recovers it. The reference driver
        // restores every parameter on open for this reason.
        dev.restore_defaults()?;
        Ok(dev)
    }

    /// Publish the captured known-good parameter set.
    fn restore_defaults(&mut self) -> Result<()> {
        for param in crate::defaults::DEFAULTS {
            self.publish(param.topic, param.value.clone())?;
        }
        // Let the instrument work through them before anything else is asked
        // of it; they are applied in order and the list is long.
        self.drain(SETTLE);
        self.group.reset();
        Ok(())
    }

    /// Round-trip port 0 echoes until one comes back.
    fn sync_output(&mut self) -> Result<()> {
        for attempt in 1..=ECHO_ATTEMPTS {
            let token = 0xA5A5_0000u32 | attempt;
            let payload = js220_proto::port0::encode_echo(&token.to_le_bytes());
            self.last_echo = None;
            self.link.send_frame(frame::PORT_ADMIN, &payload)?;

            let deadline = Instant::now() + ECHO_TIMEOUT;
            while Instant::now() < deadline {
                self.poll(Duration::from_millis(20))?;
                if self.last_echo == Some(token) {
                    log::debug!("bulk OUT confirmed after {attempt} echo(es)");
                    return Ok(());
                }
            }
            log::debug!("echo attempt {attempt} unanswered");
        }
        Err(Error::Timeout {
            context: "a port 0 echo confirming the output path",
        })
    }

    /// Poll for `duration`, discarding whatever arrives.
    ///
    /// Used to absorb traffic that belongs to the previous state of the device
    /// rather than to what the caller is about to ask for.
    pub fn drain(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            match self.poll(Duration::from_millis(20)) {
                Ok(_) => {}
                Err(e) => {
                    log::debug!("drain stopped: {e}");
                    return;
                }
            }
        }
        let n = self.group.ready();
        self.group.consume(n);
    }

    /// Round-trip a ping through the device's pub/sub engine.
    ///
    /// The device processes messages in order, so a returned pong proves every
    /// publish issued before it has been applied. This is the only barrier the
    /// protocol offers; without it a configuration change raced against the
    /// device's own startup traffic can be silently lost.
    pub fn ping(&mut self, timeout: Duration) -> Result<()> {
        let value = self.next_ping;
        self.next_ping = self.next_ping.wrapping_add(1).max(1);
        self.last_pong = None;
        self.publish_raw(js220_proto::pubsub::TOPIC_PING, Value::U32(value as u32))?;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.poll(Duration::from_millis(50))?;
            if self.last_pong == Some(value) {
                return Ok(());
            }
        }
        Err(Error::Timeout {
            context: "a ping response from the device",
        })
    }

    fn await_connect(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.poll(Duration::from_millis(100))? {
                continue;
            }
            if self.connect_info.protocol_version.to_u32() != 0 {
                let major = self.connect_info.protocol_version.major;
                if major != js220_proto::PROTOCOL_VERSION_MAJOR {
                    return Err(Error::ProtocolVersion {
                        device: major,
                        supported: js220_proto::PROTOCOL_VERSION_MAJOR,
                    });
                }
                return Ok(());
            }
        }
        Err(Error::Timeout {
            context: "the device connect response",
        })
    }

    /// The device serial number.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Versions reported when the link came up.
    pub fn connect_info(&self) -> &ConnectInfo {
        &self.connect_info
    }

    /// Receive-path totals.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// The sample id to wall clock mapping, once the device has sent one.
    pub fn time_map(&self) -> &TimeMap {
        &self.time_map
    }

    /// Transport tuning in effect.
    pub fn transport_config(&self) -> &TransportConfig {
        self.link.config()
    }

    /// The most recent retained value for `topic`, if the device has sent one.
    pub fn param(&self, topic: &str) -> Option<&Value> {
        self.params.get(topic)
    }

    /// Every retained value received so far.
    pub fn params(&self) -> &HashMap<String, Value> {
        &self.params
    }

    /// Take the most recent statistics record, if one has arrived since the
    /// last call.
    pub fn take_statistics(&mut self) -> Option<Statistics> {
        self.statistics.take()
    }

    /// Publish a value to a device topic.
    pub fn publish(&mut self, topic: &str, value: Value) -> Result<()> {
        let msg = PubSubMsg::new(topic, value).with_flags(flags::RETAIN);
        let payload = msg.encode().ok_or_else(|| Error::MessageTooLarge {
            topic: topic.to_owned(),
        })?;
        self.link.send_frame(frame::PORT_PUBSUB, &payload)
    }

    /// Ask the device to re-publish the metadata for its whole topic tree.
    pub fn request_metadata(&mut self) -> Result<()> {
        self.publish_raw(TOPIC_METADATA_ALL, Value::Null)
    }

    /// Ask the device to re-publish every retained value.
    pub fn request_values(&mut self) -> Result<()> {
        self.publish_raw(TOPIC_QUERY_ALL, Value::Null)
    }

    fn publish_raw(&mut self, topic: &str, value: Value) -> Result<()> {
        let payload = PubSubMsg::new(topic, value)
            .encode()
            .ok_or_else(|| Error::MessageTooLarge {
                topic: topic.to_owned(),
            })?;
        self.link.send_frame(frame::PORT_PUBSUB, &payload)
    }

    /// Configure and start streaming.
    pub fn stream_start(&mut self, config: StreamConfig) -> Result<()> {
        config.validate()?;
        if self.streaming {
            self.stream_stop()?;
        }

        // Auto-ranging must be requested explicitly. The instrument's own
        // default for `s/i/range/mode` is 0, which is *off*, and this driver
        // connects with the reset option, so every session starts with the
        // current front end not ranging. Leaving these topics alone yields a
        // link that connects and acknowledges everything while producing no
        // current samples at all.
        match config.current_range {
            CurrentRange::Auto => {
                self.publish(topics::S_CURRENT_RANGE_MODE, Value::U8(RANGE_MODE_AUTO))?;
            }
            CurrentRange::Manual(n) => {
                if n > 5 {
                    return Err(Error::Config {
                        reason: format!("current range {n} is outside 0..=5"),
                    });
                }
                self.publish(topics::S_CURRENT_RANGE_MODE, Value::U8(RANGE_MODE_MANUAL))?;
                self.publish(
                    topics::S_CURRENT_RANGE_SELECT,
                    Value::U8(current_range_select(n)),
                )?;
            }
            CurrentRange::Off => {
                self.publish(topics::S_CURRENT_RANGE_MODE, Value::U8(RANGE_MODE_OFF))?;
            }
        }

        match config.voltage_range {
            VoltageRange::Auto => {
                self.publish(topics::S_VOLTAGE_RANGE_MODE, Value::U8(V_RANGE_MODE_AUTO))?;
            }
            VoltageRange::V2 | VoltageRange::V15 => {
                let sel = if config.voltage_range == VoltageRange::V2 {
                    V_RANGE_2V
                } else {
                    V_RANGE_15V
                };
                self.publish(topics::S_VOLTAGE_RANGE_MODE, Value::U8(V_RANGE_MODE_MANUAL))?;
                self.publish(topics::S_VOLTAGE_RANGE_SELECT, Value::U8(sel))?;
            }
        }

        let device_signals = config.device_signals();
        // Bridge at most a few milliseconds of dropout. A longer gap is not a
        // dropout, it is a device that has stopped sending, and synthesizing
        // NaN across it manufactures rows that make a dead stream look like a
        // healthy one.
        let max_fill = (config.effective_rate() as usize / 100).max(16);
        // Calibrated streaming always runs at the 1 Msps ceiling and lower
        // rates are averaged on the host; the instrument's own decimation
        // needs a filter mode absent from its topic metadata, and setting the
        // divider alone silently produced no samples at all. Raw ADC runs at
        // the full base rate and is never averaged.
        self.group = SignalGroup::with_decimation(
            &device_signals,
            config.decimate(),
            max_fill,
            config.host_decimation(),
            config.source.sample_step(),
        );
        self.power.clear();
        self.scratch.clear();
        self.expect_frame_id = None;

        match config.source {
            Source::Calibrated => {
                self.adc_map = None;
                for signal in &device_signals {
                    let topic = match signal {
                        Signal::Current => topics::S_CURRENT_CTRL,
                        Signal::Voltage => topics::S_VOLTAGE_CTRL,
                        Signal::Power => continue,
                    };
                    // The instrument declares the ctrl topics as u8;
                    // publishing the type it expects avoids relying on its
                    // coercion rules.
                    self.publish(topic, Value::U8(1))?;
                }
            }
            Source::RawAdc => {
                // The two chosen ADC channels are carried in the current and
                // voltage slots so they get the same alignment and gap
                // handling; `adc_map` records which is which.
                let (a, b) = config.adc_channels;
                self.adc_map = Some((a, b));
                for ch in [a, b] {
                    let topic = topics::adc_ctrl(ch).ok_or_else(|| Error::Config {
                        reason: format!("ADC channel {ch} does not exist"),
                    })?;
                    self.publish(topic, Value::U8(1))?;
                }
            }
        }

        if config.statistics {
            self.publish(
                topics::S_STATS_SCNT,
                Value::U32(config.statistics_block_samples()),
            )?;
            self.publish(topics::S_STATS_CTRL, Value::U8(1))?;
        }

        self.link.prime();
        self.config = Some(config);
        self.streaming = true;

        // Give the instrument a moment to apply the settings, then discard
        // everything received up to this point: it was queued before the
        // stream was configured, or left over from a previous session. The
        // caller's first row is then genuinely the first row.
        self.drain(SETTLE);
        self.group.reset();
        Ok(())
    }

    /// Stop streaming and disable every data port.
    pub fn stream_stop(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        self.streaming = false;
        // Turn everything off even if one publish fails, so a partial failure
        // does not leave the instrument streaming into a queue nobody drains.
        let mut first_err = None;
        for topic in [
            topics::S_CURRENT_CTRL,
            topics::S_VOLTAGE_CTRL,
            topics::S_POWER_CTRL,
            topics::S_STATS_CTRL,
        ] {
            if let Err(e) = self.publish(topic, Value::U8(0)) {
                first_err.get_or_insert(e);
            }
        }
        // Raw mode leaves ADC ports enabled, which would keep the instrument
        // streaming into a queue nobody drains.
        for ch in 0..4u8 {
            if let Some(topic) = topics::adc_ctrl(ch) {
                if let Err(e) = self.publish(topic, Value::U8(0)) {
                    first_err.get_or_insert(e);
                }
            }
        }
        self.adc_map = None;
        self.group.reset();
        self.power.clear();
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// True while streaming is enabled.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Process one completed bulk IN transfer.
    ///
    /// Returns `Ok(false)` if `timeout` elapsed with nothing to do. The caller
    /// drives this in a loop; it is the only place received data is decoded.
    pub fn poll(&mut self, timeout: Duration) -> Result<bool> {
        let Some(completion) = self.link.next_in(timeout)? else {
            return Ok(false);
        };
        let data: &[u8] = &completion.buffer;
        self.counters.transfers += 1;
        self.counters.bytes += data.len() as u64;

        let mut responses: Vec<(u8, Vec<u8>)> = Vec::new();
        for result in FrameIter::new(data) {
            match result {
                Ok(f) => {
                    self.counters.frames += 1;
                    self.track_frame_id(f.header.frame_id);
                    self.dispatch(f.header.port_id, f.payload, &mut responses);
                }
                Err(e) => {
                    self.counters.frame_errors += 1;
                    log::debug!("frame decode failed: {e}");
                }
            }
        }
        self.link.recycle(completion.buffer);

        // Replies are sent after the buffer is back in the queue so the device
        // never stalls waiting for somewhere to put data.
        for (port, payload) in responses {
            self.link.send_frame(port, &payload)?;
        }
        Ok(true)
    }

    fn track_frame_id(&mut self, frame_id: u16) {
        if let Some(expect) = self.expect_frame_id {
            if expect != frame_id {
                self.counters.frame_id_breaks += 1;
                log::debug!("frame id break: expected {expect}, got {frame_id}");
            }
        }
        self.expect_frame_id = Some(frame_id.wrapping_add(1));
    }

    fn dispatch(&mut self, port_id: u8, payload: &[u8], responses: &mut Vec<(u8, Vec<u8>)>) {
        match port_id {
            frame::PORT_ADMIN => self.on_admin(payload, responses),
            frame::PORT_PUBSUB => self.on_pubsub(payload),
            frame::PORT_LOG | frame::PORT_MEM => {
                self.counters.ignored_frames += 1;
            }
            p if p >= frame::PORT_STREAM_BASE => self.on_stream(p - frame::PORT_STREAM_BASE, payload),
            _ => {
                self.counters.ignored_frames += 1;
            }
        }
    }

    fn on_admin(&mut self, payload: &[u8], responses: &mut Vec<(u8, Vec<u8>)>) {
        let msg = match Port0Msg::decode(payload) {
            Ok(m) => m,
            Err(e) => {
                log::debug!("port0 decode failed: {e}");
                return;
            }
        };
        match msg.body {
            Port0Body::Connect(info) => {
                log::info!(
                    "connected: protocol {}, fw {}, hw {}, fpga {}",
                    info.protocol_version,
                    info.fw_version,
                    info.hw_version,
                    info.fpga_version
                );
                self.connect_info = info;
            }
            Port0Body::ConnectFailed => {
                log::warn!("device reported connect failure, status {}", msg.status);
            }
            Port0Body::TimeSync(ts) => {
                // The device measures round-trip delay from this turnaround, so
                // the reply is built immediately and queued for the end of the
                // current transfer rather than deferred further.
                let recv = time::now();
                let payload =
                    js220_proto::port0::encode_timesync_response(ts.start_count, recv, time::now());
                responses.push((frame::PORT_ADMIN, payload));
                self.counters.timesyncs += 1;
            }
            Port0Body::TimeMap(tm) => {
                self.time_map
                    .update(tm.utc_q30, tm.counter, tm.counter_rate_hz());
            }
            Port0Body::Echo(data) => {
                if data.len() >= 4 {
                    let bytes: [u8; 4] = data[..4].try_into().expect("checked length");
                    self.last_echo = Some(u32::from_le_bytes(bytes));
                }
            }
        }
        if msg.op == Port0Op::Connect && msg.status != 0 {
            log::warn!("connect status {}", msg.status);
        }
    }

    fn on_pubsub(&mut self, payload: &[u8]) {
        match PubSubMsg::decode(payload) {
            Ok(msg) => {
                log::trace!("pubsub {} = {:?}", msg.topic, msg.value);
                if msg.topic == js220_proto::pubsub::TOPIC_PONG {
                    // Compared narrowly: the device echoes the ping through a
                    // narrower field than it was published in, so only the low
                    // byte reliably survives the round trip.
                    self.last_pong = msg.value.as_i64().map(|v| v as u8);
                }
                self.params.insert(msg.topic, msg.value);
            }
            Err(e) => log::debug!("pubsub decode failed: {e}"),
        }
    }

    fn on_stream(&mut self, index: u8, payload: &[u8]) {
        self.counters.stream_frames += 1;
        if index == pstream::IDX_STATISTICS {
            match stats::decode(payload) {
                Ok(s) => {
                    self.statistics = Some(s);
                    self.counters.statistics += 1;
                }
                Err(e) => log::debug!("statistics decode failed: {e}"),
            }
            return;
        }

        // In raw mode the ADC ports stand in for current and voltage, and
        // carry i16 counts rather than f32 units.
        let (signal, raw_adc) = match self.adc_map {
            Some((a, _)) if index == a => (Signal::Current, true),
            Some((_, b)) if index == b => (Signal::Voltage, true),
            Some(_) => {
                self.counters.ignored_frames += 1;
                return;
            }
            None => {
                let s = match index {
                    pstream::IDX_CURRENT => Signal::Current,
                    pstream::IDX_VOLTAGE => Signal::Voltage,
                    pstream::IDX_POWER => Signal::Power,
                    _ => {
                        self.counters.ignored_frames += 1;
                        return;
                    }
                };
                (s, false)
            }
        };

        let Some((raw_id, data)) = pstream::split_sample_id(payload) else {
            log::debug!("stream frame on index {index} too short for a sample id");
            return;
        };

        // Decode straight from the transfer buffer into a reused scratch
        // buffer, so the hot path neither allocates nor copies twice. The
        // buffer is moved out and back because `push` needs `&mut self.group`
        // while the samples are borrowed.
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        if raw_adc {
            // Widened to f32 so everything downstream is one representation.
            // These are ADC counts, not amperes or volts.
            let mut counts = std::mem::take(&mut self.adc_scratch);
            counts.clear();
            pstream::decode_i16(data, &mut counts);
            scratch.extend(counts.iter().map(|v| *v as f32));
            self.adc_scratch = counts;
        } else {
            pstream::decode_f32(data, &mut scratch);
        }
        self.group.push(signal, raw_id, &scratch);
        self.scratch = scratch;
    }

    /// Borrow the aligned rows currently available.
    ///
    /// Power is computed here from current and voltage when it was requested.
    /// Call [`Js220::consume`] with the number of rows actually used.
    pub fn rows(&mut self) -> Rows<'_> {
        let len = self.group.ready();
        let decimate = self.group.decimate();
        let sample_id = self.group.base_sample_id().unwrap_or(0);

        // Resolve the requested columns up front so the borrows below stay
        // confined to individual fields.
        let (want_i, want_v, want_p) = match self.config.as_ref() {
            // Raw ADC fills both slots and has no power to compute: the
            // counts carry no units to multiply.
            Some(c) if c.source == Source::RawAdc => (true, true, false),
            Some(c) => (
                c.signals.contains(&Signal::Current),
                c.signals.contains(&Signal::Voltage),
                c.signals.contains(&Signal::Power),
            ),
            None => (false, false, false),
        };

        let current = self.group.buffered(Signal::Current);
        let voltage = self.group.buffered(Signal::Voltage);

        self.power.clear();
        if want_p {
            pstream::compute_power(&current[..len], &voltage[..len], &mut self.power);
        }

        Rows {
            sample_id,
            decimate,
            len,
            current: want_i.then(|| &current[..len]),
            voltage: want_v.then(|| &voltage[..len]),
            power: want_p.then(|| &self.power[..]),
        }
    }

    /// Drop the first `n` aligned rows.
    pub fn consume(&mut self, n: usize) {
        self.group.consume(n);
    }

    /// Per-signal stream health counters.
    pub fn signal_counters(&self, signal: Signal) -> Option<crate::signals::SignalCounters> {
        self.group.counters(signal)
    }

    /// Stop streaming and tell the device the host is going away.
    pub fn close(mut self) -> Result<()> {
        let stop = self.stream_stop();
        self.link.cancel_in();
        let disconnect = self.link.disconnect();
        stop.and(disconnect)
    }
}

impl Drop for Js220 {
    fn drop(&mut self) {
        if self.streaming {
            let _ = self.stream_stop();
        }
        self.link.cancel_in();
        let _ = self.link.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(rate: u32) -> StreamConfig {
        StreamConfig {
            sample_rate: rate,
            ..Default::default()
        }
    }

    #[test]
    fn the_default_configuration_is_valid() {
        StreamConfig::default().validate().unwrap();
    }

    #[test]
    fn rates_must_divide_the_base_rate_exactly() {
        cfg(1_000_000).validate().unwrap();
        cfg(500_000).validate().unwrap();
        cfg(1_000).validate().unwrap();
        assert!(cfg(999).validate().is_err(), "below the minimum");
        assert!(cfg(2_000_000).validate().is_err(), "above the maximum");
        assert!(cfg(300_000).validate().is_err(), "does not divide 2 MHz");
    }

    #[test]
    fn an_empty_signal_list_is_rejected() {
        let c = StreamConfig {
            signals: vec![],
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn statistics_rate_is_bounded() {
        let mut c = StreamConfig {
            statistics: true,
            statistics_rate: 0.0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        c.statistics_rate = 2.0;
        c.validate().unwrap();
        c.statistics_rate = 5000.0;
        assert!(c.validate().is_err());
        // Ignored when statistics are off.
        c.statistics = false;
        c.statistics_rate = 0.0;
        c.validate().unwrap();
    }

    #[test]
    fn decimation_follows_the_requested_rate() {
        assert_eq!(cfg(1_000_000).decimate(), 2);
        assert_eq!(cfg(500_000).decimate(), 4);
        assert_eq!(cfg(1_000).decimate(), 2000);
    }

    #[test]
    fn host_decimation_is_relative_to_the_one_msps_ceiling() {
        assert_eq!(cfg(1_000_000).host_decimation(), 1);
        assert_eq!(cfg(500_000).host_decimation(), 2);
        assert_eq!(cfg(1_000).host_decimation(), 1000);
    }

    #[test]
    fn power_pulls_in_its_operands() {
        let c = StreamConfig {
            signals: vec![Signal::Power],
            ..Default::default()
        };
        assert_eq!(c.device_signals(), vec![Signal::Current, Signal::Voltage]);
    }

    #[test]
    fn requesting_current_alone_streams_only_current() {
        let c = StreamConfig {
            signals: vec![Signal::Current],
            ..Default::default()
        };
        assert_eq!(c.device_signals(), vec![Signal::Current]);
    }

    #[test]
    fn device_signals_never_include_power() {
        let c = StreamConfig::default();
        assert!(!c.device_signals().contains(&Signal::Power));
    }

    fn raw() -> StreamConfig {
        StreamConfig {
            source: Source::RawAdc,
            ..Default::default()
        }
    }

    #[test]
    fn raw_adc_runs_at_twice_the_calibrated_rate() {
        assert_eq!(Source::Calibrated.rate(), MAX_SAMPLE_RATE);
        assert_eq!(Source::RawAdc.rate(), BASE_RATE);
        assert_eq!(Source::RawAdc.rate(), 2 * Source::Calibrated.rate());
    }

    #[test]
    fn raw_adc_takes_every_sample_the_counter_ticks() {
        assert_eq!(Source::RawAdc.sample_step(), 1);
        assert_eq!(Source::Calibrated.sample_step(), 2);
    }

    #[test]
    fn raw_adc_ignores_the_rate_selector() {
        let c = StreamConfig {
            sample_rate: 1_000,
            ..raw()
        };
        // The requested rate is meaningless for this path.
        assert_eq!(c.effective_rate(), BASE_RATE);
        assert_eq!(c.decimate(), 1);
        assert_eq!(c.host_decimation(), 1, "raw samples are never averaged");
        c.validate().unwrap();
    }

    #[test]
    fn raw_adc_rejects_channels_that_do_not_exist() {
        let c = StreamConfig {
            adc_channels: (0, 4),
            ..raw()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn raw_adc_rejects_the_same_channel_twice() {
        let c = StreamConfig {
            adc_channels: (2, 2),
            ..raw()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn raw_adc_accepts_any_distinct_pair() {
        for pair in [(0, 1), (1, 0), (2, 3), (0, 3)] {
            let c = StreamConfig {
                adc_channels: pair,
                ..raw()
            };
            c.validate().unwrap();
        }
    }

    #[test]
    fn raw_counts_carry_no_physical_unit() {
        // Labelling these as amperes or volts would be a lie.
        assert_eq!(Source::RawAdc.unit(), "counts");
        assert_eq!(Source::Calibrated.unit(), "");
    }

    #[test]
    fn statistics_block_size_follows_the_requested_rate() {
        let c = StreamConfig {
            statistics: true,
            statistics_rate: 2.0,
            ..Default::default()
        };
        assert_eq!(c.statistics_block_samples(), 500_000);

        let c = StreamConfig {
            statistics_rate: 10.0,
            ..c
        };
        assert_eq!(c.statistics_block_samples(), 100_000);
    }

    #[test]
    fn statistics_block_size_is_never_zero() {
        let c = StreamConfig {
            statistics: true,
            statistics_rate: 1000.0,
            ..Default::default()
        };
        assert!(c.statistics_block_samples() >= 1);
    }
}
