//! The acquisition thread.
//!
//! The GUI must never block on USB, and the USB loop must never block on
//! rendering: if the host stops draining the bulk IN queue the device drops
//! samples silently. So acquisition owns the device on its own thread and the
//! two communicate only through a command channel and a shared snapshot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use js220::device::Source;
use js220::proto::stats::Statistics;
use js220::{GpioVref, Js220, Signal, StreamConfig, TransportConfig};

use crate::buffer::SignalTrace;
use crate::raw::RawTrace;

/// How long a single poll blocks before the loop checks for commands.
const POLL: Duration = Duration::from_millis(50);

/// Maximum log lines retained.
const LOG_LIMIT: usize = 400;

/// Resolution of the long history, in seconds per bucket.
///
/// Fine enough that a millisecond-scale feature still shows when the whole
/// capture is on screen, coarse enough that minutes of history cost little.
pub const HISTORY_BUCKET_SECONDS: f64 = 0.0025;

/// Display buckets retained per signal.
///
/// At the bucket resolution above this is about eleven minutes of history for
/// 4 MB per signal, which is what lets the view accumulate from the start of
/// the capture instead of sliding a fixed window over it.
pub const BUCKETS: usize = 256 * 1024;

/// Raw samples retained per signal for zooming into the real waveform.
///
/// Bounded in samples rather than seconds so the memory cost is fixed at any
/// sample rate: 4 M samples is 16 MB per signal, and covers four seconds at
/// the 1 Msps ceiling or far longer at lower rates.
pub const RAW_SAMPLES: usize = 4 << 20;

/// Raw samples retained per GPI line.
///
/// A quarter of the analog depth, which at the 2 MHz the lines always run at
/// is half a second of edge-accurate history for 4 MB a line. Allocated only
/// for lines actually enabled, so an unused input costs nothing.
pub const GPI_RAW_SAMPLES: usize = 1 << 20;

/// Commands the GUI sends to the acquisition thread.
#[derive(Debug, Clone)]
pub enum Command {
    /// Open a device and begin streaming.
    Start {
        /// Serial number, or `None` for the first device found.
        serial: Option<String>,
        /// Which acquisition path to stream.
        source: Source,
        /// Requested output sample rate in Hz, for the calibrated path.
        rate: u32,
        /// Enable the on-instrument statistics stream.
        statistics: bool,
        /// General purpose input lines to capture alongside the signals.
        gpi: Vec<u8>,
        /// What those lines compare against.
        gpio_vref: GpioVref,
    },
    /// Stop streaming, leaving the device open and the target powered.
    Stop,
    /// Release the device entirely.
    Disconnect,
    /// Change which general purpose inputs are captured, mid-capture.
    SetGpi {
        /// The lines to capture from now on.
        channels: Vec<u8>,
    },
    /// Discard the displayed history.
    ClearTraces,
    /// Open or close the current path to the target.
    SetTargetPower {
        /// True to close the path and power the target.
        on: bool,
    },
    /// Begin writing samples to `path`.
    RecordStart {
        /// Destination file.
        path: std::path::PathBuf,
    },
    /// Stop writing samples.
    RecordStop,
    /// Shut the thread down.
    Quit,
}

/// Connection state, as the GUI should display it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No device open.
    Idle,
    /// Opening and configuring.
    Connecting,
    /// Device open and holding the target's supply, but not streaming.
    Open,
    /// Streaming.
    Running,
    /// Stopped after an error.
    Failed(String),
}

impl Status {
    /// Short label for display.
    pub fn label(&self) -> &str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Running => "running",
            Self::Open => "connected",
            Self::Failed(_) => "error",
        }
    }
}

/// One captured general purpose input line, ready to plot.
///
/// Held in the same two representations as an analog signal — a coarse
/// history and a raw ring — so a logic line pans and zooms exactly like the
/// traces above it. Levels are stored as 0.0 and 1.0 so one set of
/// aggregation code serves both.
#[derive(Debug)]
pub struct GpiTrace {
    /// The instrument's number for this line: 0..=3, or 7 for the trigger.
    pub channel: u8,
    /// Aggregated history.
    pub coarse: SignalTrace,
    /// Recent samples, for zooming in on an edge.
    pub raw: RawTrace,
}

impl GpiTrace {
    fn new(channel: u8, buckets: usize) -> Self {
        let mut raw = RawTrace::new(GPI_RAW_SAMPLES);
        // GPI always runs at the base rate, whatever the analog path is doing.
        raw.reconfigure(GPI_RAW_SAMPLES, 1.0 / js220::BASE_RATE as f64);
        Self {
            channel,
            coarse: SignalTrace::new(buckets),
            raw,
        }
    }
}

/// Everything the GUI reads to draw a frame.
#[derive(Debug)]
pub struct Shared {
    /// Current connection state.
    pub status: Status,
    /// Device serial number once known.
    pub serial: Option<String>,
    /// Firmware, hardware and FPGA versions, formatted for display.
    pub versions: Option<String>,
    /// Aggregated current trace, in amperes.
    pub current: SignalTrace,
    /// Aggregated voltage trace, in volts.
    pub voltage: SignalTrace,
    /// Aggregated power trace, in watts.
    pub power: SignalTrace,
    /// Recent raw current samples, for zooming in.
    pub raw_current: RawTrace,
    /// Recent raw voltage samples.
    pub raw_voltage: RawTrace,
    /// Recent raw power samples.
    pub raw_power: RawTrace,
    /// Captured general purpose input lines, in the order requested.
    pub gpi: Vec<GpiTrace>,
    /// Most recent on-instrument statistics.
    pub statistics: Option<Statistics>,
    /// Sustained bulk IN rate in bytes per second.
    pub throughput: f64,
    /// Samples delivered per second, measured from real samples.
    pub sample_rate: f64,
    /// Total samples received this session.
    pub samples: u64,
    /// Samples synthesized to bridge gaps.
    pub filled: u64,
    /// Frames that failed to decode.
    pub frame_errors: u64,
    /// Breaks in the device's frame sequence.
    pub frame_breaks: u64,
    /// Seconds of data captured.
    pub elapsed: f64,
    /// Rows written to the recording, if one is active.
    pub recorded_rows: u64,
    /// Path being recorded to.
    pub recording: Option<std::path::PathBuf>,
    /// Rolling log, oldest first.
    pub log: Vec<LogLine>,
    /// Seconds covered by one display bucket.
    pub bucket_seconds: f64,
    /// The acquisition path currently streaming, which decides whether the
    /// samples are physical units or raw counts.
    pub source: Source,
    /// Whether the current path to the target is closed.
    pub target_power: bool,
}

/// One line in the GUI log pane.
#[derive(Debug, Clone)]
pub struct LogLine {
    /// Seconds since the GUI started.
    pub t: f64,
    /// Severity, for colouring.
    pub level: LogLevel,
    /// Message text.
    pub text: String,
}

/// Log severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Routine progress.
    Info,
    /// Something degraded but recoverable.
    Warn,
    /// An operation failed.
    Error,
}

impl Shared {
    /// Create an empty snapshot holding `buckets` display buckets per signal.
    pub fn new(buckets: usize) -> Self {
        Self {
            status: Status::Idle,
            serial: None,
            versions: None,
            current: SignalTrace::new(buckets),
            voltage: SignalTrace::new(buckets),
            power: SignalTrace::new(buckets),
            raw_current: RawTrace::new(RAW_SAMPLES),
            raw_voltage: RawTrace::new(RAW_SAMPLES),
            raw_power: RawTrace::new(RAW_SAMPLES),
            gpi: Vec::new(),
            statistics: None,
            throughput: 0.0,
            sample_rate: 0.0,
            samples: 0,
            filled: 0,
            frame_errors: 0,
            frame_breaks: 0,
            elapsed: 0.0,
            recorded_rows: 0,
            recording: None,
            log: Vec::new(),
            bucket_seconds: 0.001,
            source: Source::Calibrated,
            target_power: true,
        }
    }

    fn push_log(&mut self, level: LogLevel, text: impl Into<String>, started: Instant) {
        self.log.push(LogLine {
            t: started.elapsed().as_secs_f64(),
            level,
            text: text.into(),
        });
        if self.log.len() > LOG_LIMIT {
            let excess = self.log.len() - LOG_LIMIT;
            self.log.drain(..excess);
        }
    }

    fn clear_traces(&mut self) {
        self.current.clear();
        self.voltage.clear();
        self.power.clear();
        self.raw_current.clear();
        self.raw_voltage.clear();
        self.raw_power.clear();
        for line in &mut self.gpi {
            line.coarse.clear();
            line.raw.clear();
        }
    }

    /// Point the raw traces at a new sample rate, discarding what they hold.
    fn reconfigure_raw(&mut self, sample_period: f64) {
        for t in [
            &mut self.raw_current,
            &mut self.raw_voltage,
            &mut self.raw_power,
        ] {
            t.reconfigure(RAW_SAMPLES, sample_period);
        }
    }
}

/// Handle the GUI holds on the acquisition thread.
pub struct Acquisition {
    tx: Sender<Command>,
    shared: Arc<Mutex<Shared>>,
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Acquisition {
    /// Spawn the acquisition thread.
    pub fn spawn(buckets: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::new(buckets)));
        let running = Arc::new(AtomicBool::new(true));

        let worker_shared = Arc::clone(&shared);
        let worker_running = Arc::clone(&running);
        let handle = std::thread::Builder::new()
            .name("js220-acquire".into())
            .spawn(move || run(rx, worker_shared, worker_running))
            .expect("spawning the acquisition thread");

        Self {
            tx,
            shared,
            running,
            handle: Some(handle),
        }
    }

    /// Send a command, ignoring failure if the thread has already exited.
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }

    /// Borrow the shared snapshot.
    pub fn shared(&self) -> &Arc<Mutex<Shared>> {
        &self.shared
    }
}

impl Drop for Acquisition {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::Quit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Per-session state while a device is open.
///
/// The device is kept open across stop and start. Closing it would cut power
/// to the target: the instrument's own defaults leave the current range mode
/// off, which opens the ranging path the target sits in series with, and
/// every open restores those defaults. Reopening also costs seconds.
struct Session {
    device: Js220,
    /// Whether the instrument is currently streaming. False means the device
    /// is open and idle, not closed.
    streaming: bool,
    rate: u32,
    sample_period: f64,
    /// Index of the next sample, counted from capture start.
    next_index: u64,
    /// Device sample id of row 0, which is what puts GPI on the same time
    /// axis as the analog traces. The instrument counts both at 2 MHz, so a
    /// GPI sample id minus this one is its position in base samples.
    id0: Option<u64>,
    /// Scratch for converting GPI bits into plottable levels.
    gpi_levels: Vec<f32>,
    started: Instant,
    last_stats: Instant,
    last_bytes: u64,
    last_samples: u64,
    recorder: Option<Recorder>,
}

struct Recorder {
    writer: std::io::BufWriter<std::fs::File>,
    rows: u64,
    path: std::path::PathBuf,
}

fn run(rx: Receiver<Command>, shared: Arc<Mutex<Shared>>, running: Arc<AtomicBool>) {
    let started = Instant::now();
    let mut session: Option<Session> = None;

    while running.load(Ordering::Relaxed) {
        // Commands first, so a stop request is honoured promptly.
        loop {
            match rx.try_recv() {
                // `session` is dropped on return, which closes the device.
                Ok(Command::Quit) => return,
                Ok(cmd) => handle_command(cmd, &mut session, &shared, started),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // An open but idle device is left alone; it is held so that starting
        // again is immediate and does not disturb the target.
        let Some(active) = session.as_mut().filter(|s| s.streaming) else {
            // Nothing to do; sleep briefly rather than spin.
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };

        if let Err(e) = pump(active, &shared, started) {
            {
                let mut s = shared.lock().expect("shared snapshot poisoned");
                s.push_log(LogLevel::Error, format!("streaming stopped: {e}"), started);
                s.status = Status::Failed(e.to_string());
            }
            // Closing the device here also stops the instrument streaming.
            session = None;
            continue;
        }
    }
}

fn handle_command(
    cmd: Command,
    session: &mut Option<Session>,
    shared: &Arc<Mutex<Shared>>,
    started: Instant,
) {
    match cmd {
        Command::Start {
            serial,
            source,
            rate,
            statistics,
            gpi,
            gpio_vref,
        } => {
            {
                let mut s = shared.lock().expect("shared snapshot poisoned");
                s.status = Status::Connecting;
                s.push_log(LogLevel::Info, "opening device", started);
                s.clear_traces();
                // Rebuilt rather than cleared: a different set of lines was
                // asked for, and a trace for a line nobody enabled would sit
                // on screen for the rest of the session showing nothing.
                s.gpi = gpi.iter().map(|c| GpiTrace::new(*c, BUCKETS)).collect();
                s.samples = 0;
                s.filled = 0;
                s.elapsed = 0.0;
            }
            // An already-open device is reused rather than reopened. Closing
            // and reopening takes seconds and, because the instrument's
            // defaults leave the current path open, interrupts the target's
            // supply — so pressing Stop and Start would reset whatever is
            // being measured.
            let wanted = serial.as_deref();
            let reusable = session
                .as_ref()
                .is_some_and(|s| wanted.map_or(true, |w| s.device.serial() == w));
            if !reusable {
                // A different device was asked for; let go of this one.
                *session = None;
            }

            let outcome = match session.as_mut() {
                Some(open) => begin_stream(open, source, rate, statistics, &gpi, gpio_vref),
                None => match open_session(wanted) {
                    Ok(mut fresh) => {
                        let r = begin_stream(&mut fresh, source, rate, statistics, &gpi, gpio_vref);
                        if r.is_ok() {
                            *session = Some(fresh);
                        }
                        r
                    }
                    Err(e) => Err(e),
                },
            };

            match outcome {
                Ok(()) => {
                    let open = session.as_ref().expect("set on success");
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.serial = Some(open.device.serial().to_owned());
                    let info = open.device.connect_info();
                    s.versions = Some(format!(
                        "fw {} · hw {} · fpga {}",
                        info.fw_version, info.hw_version, info.fpga_version
                    ));
                    // One bucket per pixel column is plenty; tie it to the
                    // rate so the window covers a sensible span at any rate.
                    let effective = match source {
                        Source::RawAdc => js220::BASE_RATE,
                        Source::Calibrated => rate,
                    };
                    s.source = source;
                    s.target_power = true;
                    s.bucket_seconds = bucket_seconds_for(effective);
                    s.reconfigure_raw(1.0 / effective.max(1) as f64);
                    s.status = Status::Running;
                    s.push_log(LogLevel::Info, format!("streaming at {rate} Hz"), started);
                }
                Err(e) => {
                    // A device that failed to configure is not trustworthy;
                    // let go of it so the next attempt starts clean.
                    *session = None;
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.push_log(LogLevel::Error, format!("start failed: {e}"), started);
                    s.status = Status::Failed(e.to_string());
                }
            }
        }
        Command::Stop => {
            // The ports are closed but the device stays open, so the target
            // keeps its supply and starting again is immediate.
            if let Some(open) = session.as_mut() {
                open.streaming = false;
                open.recorder = None;
                if let Err(e) = open.device.stream_stop() {
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.push_log(LogLevel::Warn, format!("stop reported {e}"), started);
                }
            }
            let mut s = shared.lock().expect("shared snapshot poisoned");
            s.status = Status::Open;
            s.recording = None;
            s.push_log(LogLevel::Info, "stopped; device still open", started);
        }
        Command::Disconnect => {
            // Explicitly letting go of the device, which does disturb the
            // target: the instrument reverts to its own defaults, and those
            // leave the current path open.
            *session = None;
            let mut s = shared.lock().expect("shared snapshot poisoned");
            s.status = Status::Idle;
            s.recording = None;
            s.serial = None;
            s.versions = None;
            s.push_log(LogLevel::Info, "disconnected", started);
        }
        Command::SetGpi { channels } => {
            let Some(open) = session.as_mut() else {
                // Nothing open: the selection is applied when streaming
                // starts, so there is nothing to do and nothing to warn about.
                return;
            };
            match open.device.set_gpi(&channels) {
                Ok(()) => {
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    // Lines that stay keep the samples they have; only the
                    // ones added or dropped change.
                    s.gpi.retain(|t| channels.contains(&t.channel));
                    for &c in &channels {
                        if !s.gpi.iter().any(|t| t.channel == c) {
                            s.gpi.push(GpiTrace::new(c, BUCKETS));
                        }
                    }
                    s.gpi.sort_by_key(|t| t.channel);
                    s.push_log(
                        LogLevel::Info,
                        format!("capturing GPI {channels:?}"),
                        started,
                    );
                }
                Err(e) => {
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.push_log(LogLevel::Error, format!("GPI change failed: {e}"), started);
                }
            }
        }
        Command::ClearTraces => {
            let mut s = shared.lock().expect("shared snapshot poisoned");
            s.clear_traces();
        }
        Command::SetTargetPower { on } => {
            let Some(active) = session.as_mut() else {
                let mut s = shared.lock().expect("shared snapshot poisoned");
                s.push_log(
                    LogLevel::Warn,
                    "not connected; cannot switch power",
                    started,
                );
                return;
            };
            match active.device.set_target_power(on) {
                Ok(()) => {
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.target_power = on;
                    s.push_log(
                        LogLevel::Info,
                        if on {
                            "target power on"
                        } else {
                            "target power OFF"
                        },
                        started,
                    );
                }
                Err(e) => {
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.push_log(
                        LogLevel::Error,
                        format!("power switch failed: {e}"),
                        started,
                    );
                }
            }
        }
        Command::RecordStart { path } => {
            let Some(active) = session.as_mut() else {
                let mut s = shared.lock().expect("shared snapshot poisoned");
                s.push_log(LogLevel::Warn, "not streaming; cannot record", started);
                return;
            };
            match std::fs::File::create(&path) {
                Ok(file) => {
                    active.recorder = Some(Recorder {
                        writer: std::io::BufWriter::with_capacity(1 << 20, file),
                        rows: 0,
                        path: path.clone(),
                    });
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.recorded_rows = 0;
                    s.recording = Some(path.clone());
                    s.push_log(
                        LogLevel::Info,
                        format!("recording to {}", path.display()),
                        started,
                    );
                }
                Err(e) => {
                    let mut s = shared.lock().expect("shared snapshot poisoned");
                    s.push_log(LogLevel::Error, format!("cannot record: {e}"), started);
                }
            }
        }
        Command::RecordStop => {
            if let Some(active) = session.as_mut() {
                if let Some(rec) = active.recorder.take() {
                    finish_recording(rec, shared, started);
                }
            }
            let mut s = shared.lock().expect("shared snapshot poisoned");
            s.recording = None;
        }
        Command::Quit => {}
    }
}

fn finish_recording(mut rec: Recorder, shared: &Arc<Mutex<Shared>>, started: Instant) {
    use std::io::Write as _;
    let outcome = rec.writer.flush();
    let mut s = shared.lock().expect("shared snapshot poisoned");
    match outcome {
        Ok(()) => s.push_log(
            LogLevel::Info,
            format!("recorded {} rows to {}", rec.rows, rec.path.display()),
            started,
        ),
        Err(e) => s.push_log(LogLevel::Error, format!("recording failed: {e}"), started),
    }
}

/// Display bucket width for a given rate.
///
/// Fixed, with a floor of one sample per bucket so a low rate is never
/// aggregated below its own resolution. Holding the resolution constant means
/// the history covers a predictable span of time whatever the sample rate.
fn bucket_seconds_for(rate: u32) -> f64 {
    let sample_period = 1.0 / rate.max(1) as f64;
    HISTORY_BUCKET_SECONDS.max(sample_period)
}

/// Open a device and hold it idle.
fn open_session(serial: Option<&str>) -> js220::Result<Session> {
    Ok(Session {
        device: Js220::open_with(serial, TransportConfig::default())?,
        streaming: false,
        rate: js220::MAX_SAMPLE_RATE,
        sample_period: 1.0 / js220::MAX_SAMPLE_RATE as f64,
        next_index: 0,
        id0: None,
        gpi_levels: Vec::new(),
        started: Instant::now(),
        last_stats: Instant::now(),
        last_bytes: 0,
        last_samples: 0,
        recorder: None,
    })
}

/// Configure an already-open device and begin streaming.
fn begin_stream(
    session: &mut Session,
    source: Source,
    rate: u32,
    statistics: bool,
    gpi: &[u8],
    gpio_vref: GpioVref,
) -> js220::Result<()> {
    // Raw ADC runs at its own fixed rate; the selector only governs the
    // calibrated path.
    let effective = match source {
        Source::RawAdc => js220::BASE_RATE,
        Source::Calibrated => rate,
    };
    let cfg = StreamConfig {
        source,
        signals: vec![Signal::Current, Signal::Voltage, Signal::Power],
        sample_rate: rate,
        statistics,
        statistics_rate: 2.0,
        gpi: gpi.to_vec(),
        gpio_vref,
        ..Default::default()
    };
    session.device.stream_start(cfg)?;

    // The time axis restarts with the capture, so everything counted from it
    // restarts too.
    session.rate = effective;
    session.sample_period = 1.0 / effective as f64;
    session.next_index = 0;
    session.id0 = None;
    session.started = Instant::now();
    session.last_stats = Instant::now();
    session.last_bytes = session.device.counters().bytes;
    session.last_samples = 0;
    session.streaming = true;
    Ok(())
}

fn pump(session: &mut Session, shared: &Arc<Mutex<Shared>>, started: Instant) -> js220::Result<()> {
    session.device.poll(POLL)?;

    let bucket_seconds = bucket_seconds_for(session.rate);
    let n;
    {
        let rows = session.device.rows();
        n = rows.len;
        if n > 0 {
            let first_index = session.next_index;
            // Anchor the two time bases on the first batch: row 0 and the
            // device sample id it carries are the same instant.
            if session.id0.is_none() {
                session.id0 = Some(rows.sample_id);
            }
            let mut guard = shared.lock().expect("shared snapshot poisoned");
            // Both representations are fed: the coarse trace is the long
            // history, the raw ring is what makes zooming in show the real
            // waveform rather than a pre-averaged block.
            if let Some(c) = rows.current {
                guard
                    .current
                    .extend(c, first_index, session.sample_period, bucket_seconds);
                guard.raw_current.push(c, first_index);
            }
            if let Some(v) = rows.voltage {
                guard
                    .voltage
                    .extend(v, first_index, session.sample_period, bucket_seconds);
                guard.raw_voltage.push(v, first_index);
            }
            if let Some(p) = rows.power {
                guard
                    .power
                    .extend(p, first_index, session.sample_period, bucket_seconds);
                guard.raw_power.push(p, first_index);
            }
            guard.current.finish(bucket_seconds);
            guard.voltage.finish(bucket_seconds);
            guard.power.finish(bucket_seconds);
            drop(guard);

            if let Some(rec) = session.recorder.as_mut() {
                write_rows(rec, &rows, n);
            }
            session.next_index += n as u64;
        }
    }
    session.device.consume(n);
    drain_gpi(session, shared);

    if let Some(stats) = session.device.take_statistics() {
        shared.lock().expect("shared snapshot poisoned").statistics = Some(stats);
    }

    // Refresh the headline numbers a few times a second rather than every poll.
    if session.last_stats.elapsed() >= Duration::from_millis(250) {
        let window = session.last_stats.elapsed().as_secs_f64();
        session.last_stats = Instant::now();
        let counters = *session.device.counters();
        let real = session
            .device
            .signal_counters(Signal::Current)
            .map_or(0, |c| c.samples);
        let filled = session
            .device
            .signal_counters(Signal::Voltage)
            .map_or(0, |c| c.filled);

        let mut s = shared.lock().expect("shared snapshot poisoned");
        s.throughput = (counters.bytes - session.last_bytes) as f64 / window;
        s.sample_rate = (real - session.last_samples) as f64 / window;
        s.samples = real;
        s.filled = filled;
        s.frame_errors = counters.frame_errors;
        s.frame_breaks = counters.frame_id_breaks;
        s.elapsed = session.started.elapsed().as_secs_f64();
        s.bucket_seconds = bucket_seconds;
        if let Some(rec) = session.recorder.as_ref() {
            s.recorded_rows = rec.rows;
        }
        let _ = started;
        session.last_bytes = counters.bytes;
        session.last_samples = real;
    }
    Ok(())
}

/// Move the GPI samples waiting on the device into the shared traces.
///
/// Always drained, even before the time bases are anchored, so the device's
/// buffers cannot grow without bound while the analog path is still warming
/// up. Until that anchor exists there is nowhere to put the samples, so they
/// are discarded rather than placed at a guessed time.
fn drain_gpi(session: &mut Session, shared: &Arc<Mutex<Shared>>) {
    let blocks = session.device.take_gpi();
    if blocks.is_empty() {
        return;
    }
    let Some(id0) = session.id0 else {
        return;
    };

    // GPI runs at the base rate on both acquisition paths.
    let period = 1.0 / js220::BASE_RATE as f64;
    let bucket_seconds = HISTORY_BUCKET_SECONDS.max(period);
    let mut levels = std::mem::take(&mut session.gpi_levels);
    let mut guard = shared.lock().expect("shared snapshot poisoned");

    for (channel, blocks) in blocks {
        let Some(trace) = guard.gpi.iter_mut().find(|t| t.channel == channel) else {
            continue;
        };
        for block in blocks {
            // Samples from before the anchor belong to a time the display
            // does not cover.
            let Some(offset) = block.first_id.checked_sub(id0) else {
                continue;
            };
            levels.clear();
            levels.extend(block.bits.iter().map(|b| *b as f32));
            trace.coarse.extend(&levels, offset, period, bucket_seconds);
            trace.raw.push(&levels, offset);
        }
        trace.coarse.finish(bucket_seconds);
    }
    drop(guard);
    session.gpi_levels = levels;
}

fn write_rows(rec: &mut Recorder, rows: &js220::Rows<'_>, n: usize) {
    use std::io::Write as _;
    let mut buf: Vec<u8> = Vec::with_capacity(n * 12);
    for i in 0..n {
        for column in [rows.current, rows.voltage, rows.power]
            .into_iter()
            .flatten()
        {
            let v = column.get(i).copied().unwrap_or(f32::NAN);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    if rec.writer.write_all(&buf).is_ok() {
        rec.rows += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_resolution_is_the_same_at_every_usable_rate() {
        for rate in [1_000, 10_000, 100_000, 1_000_000] {
            assert!(
                (bucket_seconds_for(rate) - HISTORY_BUCKET_SECONDS).abs() < 1e-12,
                "rate {rate}"
            );
        }
    }

    #[test]
    fn a_bucket_never_holds_less_than_one_sample() {
        // At low rates the sample period is wider than an even division of the
        // window, so the floor takes over.
        assert!((bucket_seconds_for(1) - 1.0).abs() < 1e-12);
        assert!(bucket_seconds_for(100) >= 1.0 / 100.0);
    }

    #[test]
    fn the_history_spans_minutes_not_seconds() {
        // Accumulating from the start of a capture is only useful if the
        // history actually reaches back that far.
        let span = BUCKETS as f64 * bucket_seconds_for(1_000_000);
        assert!(span > 600.0, "only {span} s of history");
    }

    #[test]
    fn bucket_width_is_never_zero_or_negative() {
        for rate in [0, 1, 1_000, 2_000_000] {
            assert!(bucket_seconds_for(rate) > 0.0, "rate {rate}");
        }
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(Status::Idle.label(), "idle");
        assert_eq!(Status::Running.label(), "running");
        assert_eq!(Status::Failed("x".into()).label(), "error");
    }

    #[test]
    fn the_log_is_bounded() {
        let mut s = Shared::new(10);
        let t = Instant::now();
        for i in 0..(LOG_LIMIT + 50) {
            s.push_log(LogLevel::Info, format!("line {i}"), t);
        }
        assert_eq!(s.log.len(), LOG_LIMIT);
        assert!(
            s.log[0].text.contains(&format!("line {}", 50)),
            "oldest lines are dropped first, got {}",
            s.log[0].text
        );
    }

    #[test]
    fn clearing_traces_empties_every_signal() {
        let mut s = Shared::new(10);
        s.current.extend(&[1.0; 2000], 0, 1e-6, 1e-3);
        s.current.finish(1e-3);
        assert!(!s.current.is_empty());
        s.clear_traces();
        assert!(s.current.is_empty());
        assert!(s.voltage.is_empty());
        assert!(s.power.is_empty());
    }
}
