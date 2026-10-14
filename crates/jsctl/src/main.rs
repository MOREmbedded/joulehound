//! `jsctl` — command line control and capture for the Joulescope JS220.

mod fmt;
mod sink;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use js220::device::{CurrentRange, VoltageRange};
use js220::{Js220, Rows, Signal, StreamConfig, TransportConfig};

use sink::{Chunk, Format, Sink};

/// How long to block in a single poll before checking for shutdown.
const POLL_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Parser)]
#[command(
    name = "jsctl",
    version,
    about = "Control and capture from a Joulescope JS220",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Increase log verbosity. Repeat for more detail.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Subcommand)]
enum Command {
    /// List attached Joulescopes.
    List,
    /// Open a device and report its versions and settings.
    Info(DeviceArgs),
    /// Print the device's own topic tree: types, defaults and option tables.
    Topics(TopicsArgs),
    /// Issue a USB reset, recovering a device left in a bad state.
    Reset(DeviceArgs),
    /// Print on-instrument statistics as they arrive.
    Stats(StatsArgs),
    /// Stream samples to a file.
    Capture(CaptureArgs),
    /// Measure sustained streaming throughput without writing to disk.
    Bench(BenchArgs),
}

#[derive(Args, Clone)]
struct DeviceArgs {
    /// Serial number of the device to use. Defaults to the only one attached.
    #[arg(short, long)]
    serial: Option<String>,

    /// Bytes per bulk IN transfer. Rounded up to a multiple of 512.
    #[arg(long, default_value_t = 128 * 1024, value_name = "BYTES")]
    transfer_size: usize,

    /// Number of bulk IN transfers to keep queued.
    #[arg(long, default_value_t = 8, value_name = "N")]
    queue_depth: usize,
}

impl DeviceArgs {
    fn transport(&self) -> Result<TransportConfig> {
        if self.queue_depth == 0 {
            bail!("--queue-depth must be at least 1");
        }
        if self.transfer_size == 0 {
            bail!("--transfer-size must be at least 1");
        }
        Ok(TransportConfig {
            transfer_size: self.transfer_size,
            transfers_in_flight: self.queue_depth,
            ..Default::default()
        })
    }

    fn open(&self) -> Result<Js220> {
        let transport = self.transport()?;
        let dev = Js220::open_with(self.serial.as_deref(), transport)
            .context("opening the device")?;
        Ok(dev)
    }
}

#[derive(Args, Clone)]
struct StreamArgs {
    /// Output sample rate in Hz. Must divide 2000000 exactly.
    #[arg(short, long, default_value_t = 1_000_000, value_name = "HZ")]
    rate: u32,

    /// Signals to stream, comma separated: i, v, p.
    #[arg(long, default_value = "i,v,p", value_name = "LIST")]
    signals: String,

    /// Hold a fixed current range 0..=5 instead of auto-ranging.
    #[arg(long, value_name = "N")]
    current_range: Option<u8>,

    /// Hold a fixed voltage range instead of auto-ranging.
    #[arg(long, value_name = "RANGE")]
    voltage_range: Option<VoltageRangeArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum VoltageRangeArg {
    /// The 2 V range.
    #[value(name = "2v")]
    V2,
    /// The 15 V range.
    #[value(name = "15v")]
    V15,
}

impl StreamArgs {
    fn signals(&self) -> Result<Vec<Signal>> {
        let mut out = Vec::new();
        for part in self.signals.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let s = Signal::parse(part)
                .with_context(|| format!("unknown signal {part:?}; expected i, v or p"))?;
            if !out.contains(&s) {
                out.push(s);
            }
        }
        if out.is_empty() {
            bail!("--signals selected nothing");
        }
        Ok(out)
    }

    fn config(&self, statistics: bool, statistics_rate: f64) -> Result<StreamConfig> {
        let cfg = StreamConfig {
            signals: self.signals()?,
            sample_rate: self.rate,
            statistics,
            statistics_rate,
            current_range: match self.current_range {
                Some(n) => CurrentRange::Manual(n),
                None => CurrentRange::Auto,
            },
            voltage_range: match self.voltage_range {
                Some(VoltageRangeArg::V2) => VoltageRange::V2,
                Some(VoltageRangeArg::V15) => VoltageRange::V15,
                None => VoltageRange::Auto,
            },
        };
        cfg.validate().context("invalid stream configuration")?;
        Ok(cfg)
    }
}

#[derive(Args)]
struct TopicsArgs {
    #[command(flatten)]
    device: DeviceArgs,

    /// Only show topics containing this text.
    #[arg(short, long, value_name = "SUBSTRING")]
    filter: Option<String>,

    /// Print the raw metadata JSON instead of a summary.
    #[arg(long)]
    raw: bool,
}

#[derive(Args)]
struct StatsArgs {
    #[command(flatten)]
    device: DeviceArgs,

    /// Statistics block rate in Hz.
    #[arg(long, default_value_t = 2.0, value_name = "HZ")]
    stats_rate: f64,

    /// Stop after this many seconds. Runs until interrupted when unset.
    #[arg(short, long, value_name = "SECONDS")]
    duration: Option<f64>,
}

#[derive(Args)]
struct CaptureArgs {
    #[command(flatten)]
    device: DeviceArgs,

    #[command(flatten)]
    stream: StreamArgs,

    /// Output file.
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Bin)]
    format: Format,

    /// Stop after this many seconds. Runs until interrupted when unset.
    #[arg(short, long, value_name = "SECONDS")]
    duration: Option<f64>,

    /// Blocks the writer may fall behind before data is dropped.
    #[arg(long, default_value_t = 64, value_name = "N")]
    writer_depth: usize,
}

#[derive(Args)]
struct BenchArgs {
    #[command(flatten)]
    device: DeviceArgs,

    #[command(flatten)]
    stream: StreamArgs,

    /// How long to measure for.
    #[arg(short, long, default_value_t = 10.0, value_name = "SECONDS")]
    duration: f64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match cli.command {
        Command::List => cmd_list(),
        Command::Info(args) => cmd_info(&args),
        Command::Topics(args) => cmd_topics(&args),
        Command::Reset(args) => cmd_reset(&args),
        Command::Stats(args) => cmd_stats(&args),
        Command::Capture(args) => cmd_capture(&args),
        Command::Bench(args) => cmd_bench(&args),
    }
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
}

/// A flag set by Ctrl-C so the capture loops can shut down cleanly.
fn install_interrupt() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&flag);
    ctrlc::set_handler(move || handler_flag.store(true, Ordering::Relaxed))
        .context("installing the Ctrl-C handler")?;
    Ok(flag)
}

fn cmd_list() -> Result<()> {
    let devices = js220::list().context("enumerating devices")?;
    if devices.is_empty() {
        println!("No Joulescope devices found.");
        return Ok(());
    }
    println!("{:<16} {:<12} {:<8} PRODUCT", "SERIAL", "MODE", "PID");
    for d in &devices {
        println!(
            "{:<16} {:<12} {:<8} {}",
            d.serial(),
            match d.mode() {
                js220::Mode::Application => "application",
                js220::Mode::Updater => "updater",
            },
            format!("{:#06x}", d.product_id()),
            d.product().unwrap_or("-")
        );
    }
    Ok(())
}

fn cmd_reset(args: &DeviceArgs) -> Result<()> {
    let info = js220::find(args.serial.as_deref()).context("finding the device")?;
    eprintln!("resetting {} ...", info.serial());
    js220::usb::reset(&info).context("resetting the device")?;
    // The device re-enumerates, so it is briefly absent from the bus.
    std::thread::sleep(Duration::from_millis(2000));
    match js220::find(args.serial.as_deref()) {
        Ok(d) => println!("{} is back", d.serial()),
        Err(e) => println!("device has not reappeared yet ({e}); give it a moment"),
    }
    Ok(())
}

fn cmd_info(args: &DeviceArgs) -> Result<()> {
    let mut dev = args.open()?;

    // Give the device a moment to publish its retained topic values.
    dev.request_values().context("requesting device values")?;
    dev.drain(Duration::from_millis(1000));

    let info = dev.connect_info();
    println!("serial            {}", dev.serial());
    println!("protocol          {}", info.protocol_version);
    println!("controller fw     {}", info.fw_version);
    println!("hardware          {}", info.hw_version);
    println!("sensor fpga       {}", info.fpga_version);
    println!("app id            {:#010x}", info.app_id);

    let transport = dev.transport_config();
    println!(
        "bulk IN queue     {} x {} = {}",
        transport.transfers_in_flight,
        fmt::bytes(transport.effective_transfer_size() as u64),
        fmt::bytes(transport.queued_bytes() as u64)
    );

    let params = dev.params();
    if !params.is_empty() {
        println!("\n{} retained topics:", params.len());
        let mut topics: Vec<&String> = params.keys().collect();
        topics.sort();
        for t in topics {
            println!("  {:<28} {:?}", t, params[t]);
        }
    }
    Ok(())
}

/// Render one metadata document as a compact summary line.
///
/// The instrument describes every topic itself — type, default, and the option
/// table for enumerated values. Reading it from the device beats hardcoding
/// guesses: the current-range select topic, for instance, turns out to be a
/// bitmap rather than the plain index its documentation suggests.
fn summarize_metadata(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.replace('\n', " ");
    };
    let mut parts = Vec::new();
    if let Some(d) = v.get("dtype").and_then(|d| d.as_str()) {
        parts.push(format!("{d:<6}"));
    }
    if let Some(b) = v.get("brief").and_then(|b| b.as_str()) {
        parts.push(b.to_string());
    }
    if let Some(opts) = v.get("options").and_then(|o| o.as_array()) {
        let rendered: Vec<String> = opts
            .iter()
            .filter_map(|o| {
                let pair = o.as_array()?;
                let value = pair.first()?;
                let name = pair.get(1).and_then(|n| n.as_str()).unwrap_or("");
                Some(format!("{value}={name}"))
            })
            .collect();
        if !rendered.is_empty() {
            parts.push(format!("[{}]", rendered.join(", ")));
        }
    }
    parts.join("  ")
}

fn cmd_topics(args: &TopicsArgs) -> Result<()> {
    let mut dev = args.device.open()?;

    // The metadata dump is hundreds of messages, so this is requested only by
    // the commands that actually need it, and given time to arrive.
    dev.request_metadata().context("requesting metadata")?;
    dev.drain(Duration::from_millis(1500));
    dev.request_values().context("requesting values")?;
    dev.drain(Duration::from_millis(1000));

    let params = dev.params();

    // Metadata arrives on `topic$`, current values on `topic?`.
    let mut rows: Vec<(String, String, Option<String>)> = Vec::new();
    for (topic, value) in params {
        let Some(base) = topic.strip_suffix('$') else {
            continue;
        };
        if let Some(f) = &args.filter {
            if !base.contains(f.as_str()) {
                continue;
            }
        }
        let Some(json) = value.as_str() else { continue };
        let current = params
            .get(&format!("{base}?"))
            .map(|v| format!("{v:?}"));
        let described = if args.raw {
            json.replace('\n', " ")
        } else {
            summarize_metadata(json)
        };
        rows.push((base.to_string(), described, current));
    }
    rows.sort();

    if rows.is_empty() {
        println!("No matching topics. The device publishes metadata on connect; try without --filter.");
        return Ok(());
    }
    for (topic, described, current) in &rows {
        match current {
            Some(c) => println!("{topic:<28} = {c:<12} {described}"),
            None => println!("{topic:<28} {:<15} {described}", ""),
        }
    }
    println!("\n{} topics", rows.len());
    Ok(())
}

fn cmd_stats(args: &StatsArgs) -> Result<()> {
    let interrupted = install_interrupt()?;
    let mut dev = args.device.open()?;

    let mut cfg = StreamConfig {
        statistics: true,
        statistics_rate: args.stats_rate,
        ..Default::default()
    };
    // Statistics are computed on the instrument, so no sample streams are
    // enabled: that keeps the USB link almost idle, and enabling one alongside
    // them stops the instrument emitting statistics after the first record.
    cfg.signals = vec![];
    cfg.validate().context("invalid statistics configuration")?;
    dev.stream_start(cfg).context("starting statistics")?;

    println!(
        "{:>12}  {:>14} {:>14} {:>14} {:>14}",
        "signal", "mean", "std", "min", "max"
    );

    let start = Instant::now();
    let deadline = args.duration.map(|d| start + Duration::from_secs_f64(d));
    loop {
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        dev.poll(POLL_TIMEOUT)?;
        // Statistics records are the only thing this loop consumes, so the
        // sample buffers are dropped rather than left to grow.
        let n = dev.rows().len;
        dev.consume(n);

        let Some(s) = dev.take_statistics() else {
            continue;
        };
        println!(
            "\n[{}]  {} samples at {:.0} Hz",
            fmt::duration(start.elapsed().as_secs_f64()),
            s.block_sample_count,
            s.decimated_rate()
        );
        for (name, unit, v) in [
            ("current", "A", s.current),
            ("voltage", "V", s.voltage),
            ("power", "W", s.power),
        ] {
            println!(
                "{name:>12}  {:>14} {:>14} {:>14} {:>14}",
                fmt::si(v.avg, unit),
                fmt::si(v.std, unit),
                fmt::si(v.min, unit),
                fmt::si(v.max, unit)
            );
        }
        println!(
            "{:>12}  {:>14} {:>14}",
            "accumulated",
            fmt::si(s.charge, "C"),
            fmt::si(s.energy, "J")
        );
    }

    dev.stream_stop().context("stopping statistics")?;
    Ok(())
}

/// Give `path` the format's conventional extension when it has none, so
/// `-o capture` writes `capture.f32` rather than an extensionless file.
fn default_extension(path: &std::path::Path, format: Format) -> PathBuf {
    if path.extension().is_some() {
        return path.to_path_buf();
    }
    path.with_extension(format.extension())
}

/// Copy the aligned columns into `out` in row-major interleaved order.
fn interleave(rows: &Rows<'_>, out: &mut Vec<f32>) -> usize {
    let cols: Vec<&[f32]> = [rows.current, rows.voltage, rows.power]
        .into_iter()
        .flatten()
        .collect();
    if cols.is_empty() {
        return 0;
    }
    let n = cols.iter().map(|c| c.len()).min().unwrap_or(0).min(rows.len);
    out.reserve(n * cols.len());
    for row in 0..n {
        for c in &cols {
            out.push(c[row]);
        }
    }
    n
}

fn cmd_capture(args: &CaptureArgs) -> Result<()> {
    if args.writer_depth == 0 {
        bail!("--writer-depth must be at least 1");
    }
    let interrupted = install_interrupt()?;
    let mut dev = args.device.open()?;
    let cfg = args.stream.config(false, 2.0)?;
    let signals = cfg.signals.clone();
    let rate = cfg.sample_rate;

    let path = default_extension(&args.output, args.format);
    let mut out = Sink::spawn(&path, args.format, signals.clone(), rate, args.writer_depth)?;

    eprintln!(
        "capturing {} at {} Hz to {} ({:?})",
        signals
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
            .join(", "),
        rate,
        out.path().display(),
        args.format
    );
    if args.duration.is_none() {
        eprintln!("press Ctrl-C to stop");
    }

    dev.stream_start(cfg).context("starting the stream")?;

    let start = Instant::now();
    let deadline = args.duration.map(|d| start + Duration::from_secs_f64(d));
    loop {
        if interrupted.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        dev.poll(POLL_TIMEOUT)?;

        let mut buf = out.take_buffer();
        let rows_view = dev.rows();
        let sample_id = rows_view.sample_id;
        let decimate = rows_view.decimate;
        let n = interleave(&rows_view, &mut buf);
        dev.consume(n);

        if n == 0 {
            continue;
        }
        out.send(Chunk {
            sample_id,
            decimate,
            rows: n,
            data: buf,
        })?;
    }

    dev.stream_stop().context("stopping the stream")?;
    let elapsed = start.elapsed().as_secs_f64();
    let counters = *dev.counters();
    let stats = out.finish()?;

    eprintln!();
    eprintln!("captured {} rows in {}", stats.rows, fmt::duration(elapsed));
    eprintln!(
        "wrote    {} ({} per signal-second)",
        fmt::bytes(stats.bytes),
        fmt::bytes((stats.bytes as f64 / elapsed.max(1e-9)) as u64)
    );
    report_health(&counters, &dev, &signals, elapsed);
    if stats.dropped_blocks > 0 {
        eprintln!(
            "WARNING: dropped {} rows in {} blocks; the writer could not keep up",
            stats.dropped_rows, stats.dropped_blocks
        );
    }
    Ok(())
}

fn cmd_bench(args: &BenchArgs) -> Result<()> {
    let interrupted = install_interrupt()?;
    let mut dev = args.device.open()?;
    let cfg = args.stream.config(false, 2.0)?;
    let signals = cfg.signals.clone();
    let rate = cfg.sample_rate;
    let transport = *dev.transport_config();

    eprintln!(
        "benchmarking {} Hz for {:.1} s, bulk IN queue {} x {}",
        rate,
        args.duration,
        transport.transfers_in_flight,
        fmt::bytes(transport.effective_transfer_size() as u64)
    );

    dev.stream_start(cfg).context("starting the stream")?;

    // Let the pipeline fill before the clock starts, so the warm-up is not
    // charged against the measured window.
    let warmup = Instant::now() + Duration::from_millis(250);
    while Instant::now() < warmup {
        dev.poll(POLL_TIMEOUT)?;
        let n = dev.rows().len;
        dev.consume(n);
    }

    let before = *dev.counters();
    // Snapshot per-signal counters too: they accumulate from stream_start, so
    // only the delta over the measured window is a rate.
    let samples_before: Vec<u64> = signals
        .iter()
        .map(|s| dev.signal_counters(*s).map_or(0, |c| c.samples))
        .collect();
    let start = Instant::now();
    let deadline = start + Duration::from_secs_f64(args.duration);
    let mut rows_total: u64 = 0;
    while Instant::now() < deadline {
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        dev.poll(POLL_TIMEOUT)?;
        let n = dev.rows().len;
        rows_total += n as u64;
        dev.consume(n);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let after = *dev.counters();
    dev.stream_stop().context("stopping the stream")?;

    let bytes = after.bytes - before.bytes;
    let transfers = after.transfers - before.transfers;
    let frames = after.frames - before.frames;

    println!();
    println!("duration            {:.3} s", elapsed);
    println!(
        "bulk IN throughput  {}/s",
        fmt::bytes((bytes as f64 / elapsed) as u64)
    );
    println!("bulk IN total       {}", fmt::bytes(bytes));
    println!("transfers           {transfers} ({:.0}/s)", transfers as f64 / elapsed);
    println!("frames              {frames} ({:.0}/s)", frames as f64 / elapsed);
    println!(
        "aligned rows        {rows_total} ({:.0}/s, requested {rate}/s)",
        rows_total as f64 / elapsed
    );

    // Rate is reported from samples the device actually sent, never from row
    // count. The aligner fills gaps with NaN, so rows alone will happily
    // report a healthy rate for a stream that has stopped dead.
    // Only signals the device actually streams have counters. Power is
    // computed on the host, so including it here would make `min` zero and
    // report a perfectly healthy capture as having produced nothing.
    let real: u64 = signals
        .iter()
        .zip(&samples_before)
        .filter_map(|(s, before)| {
            dev.signal_counters(*s)
                .map(|c| c.samples.saturating_sub(*before))
        })
        .min()
        .unwrap_or(0);
    let filled: u64 = signals
        .iter()
        .filter_map(|s| dev.signal_counters(*s))
        .map(|c| c.filled)
        .max()
        .unwrap_or(0);
    println!(
        "real samples        {real} ({:.0}/s), {filled} synthesized over gaps",
        real as f64 / elapsed
    );
    if rate > 0 {
        let achieved = real as f64 / elapsed / rate as f64 * 100.0;
        println!("rate achieved       {achieved:.2}% of requested (real samples)");
    }
    report_health(&after, &dev, &signals, elapsed);
    Ok(())
}

fn report_health(counters: &js220::Counters, dev: &Js220, signals: &[Signal], _elapsed: f64) {
    eprintln!(
        "frames   {} ok, {} undecodable, {} sequence breaks",
        counters.frames, counters.frame_errors, counters.frame_id_breaks
    );
    // Printed for every signal, including the clean ones. Reporting only
    // anomalies hides the most important failure of all: a signal that
    // delivered nothing at all has no anomalies to report.
    for s in signals {
        let Some(c) = dev.signal_counters(*s) else {
            continue;
        };
        eprintln!(
            "{:<8} {} samples, {} filled over gaps, {} repeated, {} discontinuities",
            s.name(),
            c.samples,
            c.filled,
            c.skipped,
            c.discontinuities
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn stream_args(signals: &str, rate: u32) -> StreamArgs {
        StreamArgs {
            rate,
            signals: signals.into(),
            current_range: None,
            voltage_range: None,
        }
    }

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn signal_lists_parse_and_deduplicate() {
        let a = stream_args("i,v,p", 1_000_000);
        assert_eq!(
            a.signals().unwrap(),
            vec![Signal::Current, Signal::Voltage, Signal::Power]
        );

        let a = stream_args("i, i , V", 1_000_000);
        assert_eq!(a.signals().unwrap(), vec![Signal::Current, Signal::Voltage]);
    }

    #[test]
    fn an_unknown_signal_is_rejected_by_name() {
        let err = stream_args("i,q", 1_000_000).signals().unwrap_err().to_string();
        assert!(err.contains("\"q\""), "got {err}");
    }

    #[test]
    fn an_empty_signal_list_is_rejected() {
        assert!(stream_args("", 1_000_000).signals().is_err());
        assert!(stream_args(" , ", 1_000_000).signals().is_err());
    }

    #[test]
    fn an_invalid_rate_is_rejected_when_building_the_config() {
        assert!(stream_args("i", 300_000).config(false, 2.0).is_err());
        assert!(stream_args("i", 500_000).config(false, 2.0).is_ok());
    }

    #[test]
    fn range_arguments_map_onto_the_driver_enums() {
        let mut a = stream_args("i", 1_000_000);
        a.current_range = Some(3);
        a.voltage_range = Some(VoltageRangeArg::V15);
        let cfg = a.config(false, 2.0).unwrap();
        assert_eq!(cfg.current_range, CurrentRange::Manual(3));
        assert_eq!(cfg.voltage_range, VoltageRange::V15);
    }

    #[test]
    fn omitted_ranges_mean_auto() {
        let cfg = stream_args("i", 1_000_000).config(false, 2.0).unwrap();
        assert_eq!(cfg.current_range, CurrentRange::Auto);
        assert_eq!(cfg.voltage_range, VoltageRange::Auto);
    }

    #[test]
    fn zero_queue_depth_is_rejected() {
        let a = DeviceArgs {
            serial: None,
            transfer_size: 4096,
            queue_depth: 0,
        };
        assert!(a.transport().is_err());
    }

    #[test]
    fn zero_transfer_size_is_rejected() {
        let a = DeviceArgs {
            serial: None,
            transfer_size: 0,
            queue_depth: 4,
        };
        assert!(a.transport().is_err());
    }

    #[test]
    fn a_missing_extension_is_filled_in_from_the_format() {
        assert_eq!(
            default_extension(std::path::Path::new("cap"), Format::Bin),
            PathBuf::from("cap.f32")
        );
        assert_eq!(
            default_extension(std::path::Path::new("cap"), Format::Csv),
            PathBuf::from("cap.csv")
        );
    }

    #[test]
    fn an_explicit_extension_is_left_alone() {
        for name in ["cap.dat", "cap.csv", "run.1.bin"] {
            assert_eq!(
                default_extension(std::path::Path::new(name), Format::Bin),
                PathBuf::from(name)
            );
        }
    }

    #[test]
    fn interleave_writes_columns_in_row_major_order() {
        let i = [1.0f32, 2.0];
        let v = [3.0f32, 4.0];
        let p = [5.0f32, 6.0];
        let rows = Rows {
            sample_id: 0,
            decimate: 2,
            len: 2,
            current: Some(&i),
            voltage: Some(&v),
            power: Some(&p),
        };
        let mut out = Vec::new();
        assert_eq!(interleave(&rows, &mut out), 2);
        assert_eq!(out, vec![1.0, 3.0, 5.0, 2.0, 4.0, 6.0]);
    }

    #[test]
    fn interleave_handles_a_single_column() {
        let i = [7.0f32, 8.0];
        let rows = Rows {
            sample_id: 0,
            decimate: 2,
            len: 2,
            current: Some(&i),
            voltage: None,
            power: None,
        };
        let mut out = Vec::new();
        assert_eq!(interleave(&rows, &mut out), 2);
        assert_eq!(out, vec![7.0, 8.0]);
    }

    #[test]
    fn interleave_stops_at_the_shortest_column() {
        let i = [1.0f32, 2.0, 3.0];
        let v = [4.0f32];
        let rows = Rows {
            sample_id: 0,
            decimate: 2,
            len: 3,
            current: Some(&i),
            voltage: Some(&v),
            power: None,
        };
        let mut out = Vec::new();
        assert_eq!(interleave(&rows, &mut out), 1);
        assert_eq!(out, vec![1.0, 4.0]);
    }

    #[test]
    fn interleave_with_no_columns_produces_nothing() {
        let rows = Rows {
            sample_id: 0,
            decimate: 2,
            len: 5,
            current: None,
            voltage: None,
            power: None,
        };
        let mut out = Vec::new();
        assert_eq!(interleave(&rows, &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn interleave_appends_to_a_reused_buffer_without_clearing_it() {
        // The caller owns clearing, so a recycled buffer must already be empty.
        let i = [1.0f32];
        let rows = Rows {
            sample_id: 0,
            decimate: 2,
            len: 1,
            current: Some(&i),
            voltage: None,
            power: None,
        };
        let mut out = Vec::new();
        interleave(&rows, &mut out);
        interleave(&rows, &mut out);
        assert_eq!(out, vec![1.0, 1.0]);
    }
}
