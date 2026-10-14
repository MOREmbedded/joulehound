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

/// How long to let the device settle after a state change before trusting what
/// arrives to belong to the new state.
const SETTLE: Duration = Duration::from_millis(250);

/// How long to wait for the device to acknowledge a publish.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for a pub/sub round trip.
const PING_TIMEOUT: Duration = Duration::from_secs(15);

/// Every topic that enables a data port.
const ALL_STREAM_CTRL: [&str; 4] = [
    topics::S_CURRENT_CTRL,
    topics::S_VOLTAGE_CTRL,
    topics::S_POWER_CTRL,
    topics::S_STATS_CTRL,
];

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
    /// Signals to deliver. Requesting power implicitly enables current and
    /// voltage, since power is computed on the host.
    pub signals: Vec<Signal>,
    /// Output sample rate in Hz.
    ///
    /// Must divide 2 MHz exactly and lie between [`MIN_SAMPLE_RATE`] and
    /// [`MAX_SAMPLE_RATE`]. Rates below the maximum are produced by the
    /// instrument's own decimation filter, so they cost host CPU and USB
    /// bandwidth proportionally rather than being filtered after the fact.
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
            signals: vec![Signal::Current, Signal::Voltage, Signal::Power],
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
        if self.signals.is_empty() {
            return Err(Error::Config {
                reason: "no signals selected".into(),
            });
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
        (BASE_RATE / self.sample_rate).max(1)
    }

    /// Decimation factor the instrument applies to the analog signals.
    pub fn instrument_decimation(&self) -> u32 {
        (MAX_SAMPLE_RATE / self.sample_rate).max(1)
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
    config: Option<StreamConfig>,
    params: HashMap<String, Value>,
    statistics: Option<Statistics>,
    /// Value carried by the most recent pong, used as a round-trip barrier.
    last_pong: Option<u32>,
    next_ping: u32,
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
            config: None,
            params: HashMap::new(),
            statistics: None,
            last_pong: None,
            next_ping: 1,
            time_map: TimeMap::new(),
            counters: Counters::default(),
            expect_frame_id: None,
            streaming: false,
        };
        dev.await_connect(Duration::from_millis(3000))?;

        // The instrument is not ready for configuration the instant it
        // acknowledges the connect: it still has its metadata and retained
        // values to publish. Walking through that exchange and closing it with
        // a ping gives a barrier, so a `stream_start` issued immediately after
        // `open` cannot be dropped on the floor.
        // Let the device finish its own post-connect chatter before returning.
        // Requesting the full metadata and value dump here was tried and
        // removed: those are hundreds of messages, and configuration published
        // on their heels was being dropped. Commands that need the topic tree
        // ask for it explicitly instead.
        // Follow the reference driver's open sequence. Connecting does not
        // leave the instrument in a state the host can rely on, and publishing
        // only the few topics a capture needs leaves everything else as the
        // previous session left it — which makes streaming fail to start.
        dev.restore_defaults()?;
        Ok(dev)
    }

    /// Push the known-good parameter set and leave the device quiet.
    ///
    /// Mirrors the reference driver: request metadata, barrier, restore every
    /// parameter, request the retained values, barrier. The barriers matter —
    /// the instrument processes messages in order, so a returned pong proves
    /// the restore has been applied before anything is enabled.
    fn restore_defaults(&mut self) -> Result<()> {
        self.request_metadata()?;
        self.barrier();

        for param in crate::defaults::DEFAULTS {
            self.publish(param.topic, param.value.clone())?;
        }

        self.request_values()?;
        self.barrier();

        self.group.reset();
        Ok(())
    }

    /// Wait for the device to catch up, logging rather than failing if it does
    /// not answer in time.
    ///
    /// A missed pong is not proof of a broken device, and failing the open over
    /// one would turn a slow reply into an unusable instrument.
    fn barrier(&mut self) {
        if let Err(e) = self.ping(PING_TIMEOUT) {
            log::debug!("no sequencing pong: {e}");
        }
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
        self.publish_raw(js220_proto::pubsub::TOPIC_PING, Value::U32(value))?;

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

    /// Publish and wait for the device's return-code acknowledgement.
    ///
    /// Every publish is answered on `{topic}#` with 0 or an error code. A
    /// publish issued while the device is still busy with its own post-connect
    /// traffic can be dropped silently, so waiting for the ack is the only way
    /// to know a setting actually landed.
    pub fn publish_confirmed(
        &mut self,
        topic: &str,
        value: Value,
        timeout: Duration,
    ) -> Result<()> {
        let ack = format!("{topic}#");
        // Drop any ack from an earlier publish so a stale one is not mistaken
        // for the answer to this one.
        self.params.remove(&ack);
        self.publish(topic, value)?;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.poll(Duration::from_millis(20))?;
            if let Some(v) = self.params.get(&ack) {
                let code = v.as_i64().unwrap_or(0);
                if code != 0 {
                    return Err(Error::Config {
                        reason: format!("device rejected {topic} with code {code}"),
                    });
                }
                return Ok(());
            }
        }
        Err(Error::Timeout {
            context: "a publish acknowledgement",
        })
    }

    /// Publish, waiting for the acknowledgement but tolerating its absence.
    ///
    /// An explicit error code from the device is a real failure and is
    /// reported. A missing acknowledgement is not: the setting may still have
    /// been applied, and `stream_start` verifies the outcome by checking that
    /// samples actually arrive. Failing here would turn a slow reply into a
    /// dead stream.
    fn publish_checked(&mut self, topic: &str, value: Value) -> Result<()> {
        match self.publish_confirmed(topic, value, ACK_TIMEOUT) {
            Err(Error::Timeout { .. }) => {
                log::debug!("no acknowledgement for {topic}; continuing");
                Ok(())
            }
            other => other,
        }
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

        // Rates below the maximum are produced by the instrument's decimation
        // filter, so the host never sees the samples it would have discarded.
        let n = config.instrument_decimation();
        if n > 1 {
            self.publish("s/dwnN/N", Value::U32(n))?;
            self.publish("s/gpi/+/dwnN/N", Value::U32(config.decimate()))?;
        }

        let device_signals = config.device_signals();
        // Bridge at most a second of dropout before re-anchoring; a longer gap
        // is not a dropout, it is a stale or bogus sample id.
        let max_fill = config.sample_rate as usize;
        self.group = SignalGroup::new(&device_signals, config.decimate(), max_fill);
        self.power.clear();
        self.scratch.clear();
        self.expect_frame_id = None;

        let ctrl_topics: Vec<&str> = device_signals
            .iter()
            .filter_map(|s| match s {
                Signal::Current => Some(topics::S_CURRENT_CTRL),
                Signal::Voltage => Some(topics::S_VOLTAGE_CTRL),
                Signal::Power => None,
            })
            .collect();
        // Resize the bulk IN queue *before* enabling anything. Retuning cancels
        // every pending transfer, so doing it after the enables would pull the
        // device's buffers away the instant it started sending and stall the
        // stream it had just acknowledged.
        self.link.set_streaming_transfers();

        for topic in &ctrl_topics {
            // The instrument declares the ctrl topics as u8; publishing the
            // type it expects avoids relying on its coercion rules.
            self.publish_checked(topic, Value::U8(1))?;
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

        // Acknowledgement is not the same as data. Let the ports spin up, then
        // check that every requested signal is actually producing before
        // handing control back, so a dead port surfaces here rather than as a
        // silently empty capture.
        self.drain(SETTLE);
        self.group.reset();
        self.drain(SETTLE);
        if !self.streams_are_live(&device_signals) {
            return Err(Error::Timeout {
                context: "samples to start arriving after enabling the streams",
            });
        }
        self.group.reset();
        Ok(())
    }

    /// True when every requested signal has delivered at least one sample.
    fn streams_are_live(&self, signals: &[Signal]) -> bool {
        signals.iter().all(|s| {
            self.group
                .counters(*s)
                .is_some_and(|c| c.samples > 0)
        })
    }

    /// Stop streaming and disable every data port.
    pub fn stream_stop(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        self.streaming = false;
        self.link.set_config_transfers();
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
                log::debug!("timesync #{} answered", self.counters.timesyncs);
            }
            Port0Body::TimeMap(tm) => {
                log::debug!("timemap counter={} rate={}", tm.counter, tm.counter_rate_hz());
                self.time_map
                    .update(tm.utc_q30, tm.counter, tm.counter_rate_hz());
            }
            Port0Body::Echo(_) => {}
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
                    self.last_pong = msg.value.as_i64().map(|v| v as u32);
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

        let signal = match index {
            pstream::IDX_CURRENT => Signal::Current,
            pstream::IDX_VOLTAGE => Signal::Voltage,
            pstream::IDX_POWER => Signal::Power,
            _ => {
                self.counters.ignored_frames += 1;
                return;
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
        pstream::decode_f32(data, &mut scratch);
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

    /// Tell the device the host is going away and tear down the link.
    ///
    /// Deliberately does not publish stream-disable settings first. The
    /// disconnect request is what stops the instrument streaming; pushing
    /// configuration into a device that is about to be disconnected only risks
    /// leaving it half-applied for the next session.
    pub fn close(mut self) -> Result<()> {
        self.streaming = false;
        // Small transfers again so the disable acknowledgements are not stuck
        // waiting for a large buffer to fill.
        self.link.set_config_transfers();
        for topic in ALL_STREAM_CTRL {
            let _ = self.publish(topic, Value::U8(0));
        }
        let disconnect = self.link.disconnect();
        self.link.cancel_in();
        disconnect
    }
}

impl Drop for Js220 {
    fn drop(&mut self) {
        // The enables are retained on the device, so leaving them set would
        // have it streaming into nothing until something else turns it off.
        // Order matters: stop the ports and disconnect before reclaiming the
        // transfers, or the endpoint can be left halted for the next session.
        self.streaming = false;
        self.link.set_config_transfers();
        for topic in ALL_STREAM_CTRL {
            let _ = self.publish(topic, Value::U8(0));
        }
        let _ = self.link.disconnect();
        self.link.cancel_in();
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
    fn instrument_decimation_is_relative_to_the_one_msps_ceiling() {
        assert_eq!(cfg(1_000_000).instrument_decimation(), 1);
        assert_eq!(cfg(500_000).instrument_decimation(), 2);
        assert_eq!(cfg(1_000).instrument_decimation(), 1000);
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
