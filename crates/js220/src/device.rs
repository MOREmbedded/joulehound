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
            group: SignalGroup::new(&[], 2),
            power: Vec::new(),
            scratch: Vec::new(),
            config: None,
            params: HashMap::new(),
            statistics: None,
            time_map: TimeMap::new(),
            counters: Counters::default(),
            expect_frame_id: None,
            streaming: false,
        };
        dev.await_connect(Duration::from_millis(3000))?;
        Ok(dev)
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

        match config.current_range {
            CurrentRange::Auto => {
                self.publish(
                    topics::S_CURRENT_RANGE_MODE,
                    Value::Str(topics::current_range_mode::AUTO.into()),
                )?;
            }
            CurrentRange::Manual(n) => {
                if n > 5 {
                    return Err(Error::Config {
                        reason: format!("current range {n} is outside 0..=5"),
                    });
                }
                self.publish(
                    topics::S_CURRENT_RANGE_MODE,
                    Value::Str(topics::current_range_mode::MANUAL.into()),
                )?;
                self.publish(topics::S_CURRENT_RANGE_SELECT, Value::U32(n as u32))?;
            }
            CurrentRange::Off => {
                self.publish(
                    topics::S_CURRENT_RANGE_MODE,
                    Value::Str(topics::current_range_mode::OFF.into()),
                )?;
            }
        }

        match config.voltage_range {
            VoltageRange::Auto => {
                self.publish(
                    topics::S_VOLTAGE_RANGE_MODE,
                    Value::Str(topics::current_range_mode::AUTO.into()),
                )?;
            }
            VoltageRange::V2 | VoltageRange::V15 => {
                let sel = if config.voltage_range == VoltageRange::V2 {
                    topics::voltage_range::V2
                } else {
                    topics::voltage_range::V15
                };
                self.publish(
                    topics::S_VOLTAGE_RANGE_MODE,
                    Value::Str(topics::current_range_mode::MANUAL.into()),
                )?;
                self.publish(topics::S_VOLTAGE_RANGE_SELECT, Value::Str(sel.into()))?;
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
        self.group = SignalGroup::new(&device_signals, config.decimate());
        self.power.clear();
        self.scratch.clear();
        self.expect_frame_id = None;

        for signal in &device_signals {
            let topic = match signal {
                Signal::Current => topics::S_CURRENT_CTRL,
                Signal::Voltage => topics::S_VOLTAGE_CTRL,
                Signal::Power => continue,
            };
            self.publish(topic, Value::U32(1))?;
        }

        if config.statistics {
            self.publish(
                topics::S_STATS_SCNT,
                Value::U32(config.statistics_block_samples()),
            )?;
            self.publish(topics::S_STATS_CTRL, Value::U32(1))?;
        }

        self.link.prime();
        self.config = Some(config);
        self.streaming = true;
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
            if let Err(e) = self.publish(topic, Value::U32(0)) {
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
            }
            Port0Body::TimeMap(tm) => {
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
