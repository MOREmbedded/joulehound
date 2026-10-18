//! The egui application.

use std::time::Duration;

use eframe::egui;
use egui_plot::{
    AxisHints, HLine, Legend, Line, MarkerShape, Plot, PlotBounds, PlotPoint, PlotPoints, Points,
    Polygon, Text, VLine,
};

use crate::acquire::{Acquisition, Command, LogLevel, Shared, Status};
use crate::buffer::{Bucket, RangeStats, SignalTrace};
use crate::export::{self, Column};
use crate::fmt;
use crate::peaks::{self, Edge};
use crate::raw::RawTrace;
use js220::device::{GpioVref, Source};

/// Sample rates offered in the toolbar. Every one divides 2 MHz exactly.
const RATES: [u32; 8] = [
    1_000, 10_000, 50_000, 100_000, 200_000, 500_000, 1_000_000,
    // Only the raw ADC ports reach this, and only as uncalibrated counts.
    2_000_000,
];

/// Time spans offered for the live view, in seconds.
const WINDOWS: [f64; 8] = [0.001, 0.01, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0];

/// Index of the current signal, which drives the shared time axis.
const I: usize = 0;

/// One plotted signal.
#[derive(Clone)]
struct SignalView {
    name: &'static str,
    unit: &'static str,
    /// Unit of the time integral, where one is meaningful.
    integral_unit: Option<&'static str>,
    colour: egui::Color32,
}

const VIEWS: [SignalView; 3] = [
    SignalView {
        name: "Current",
        unit: "A",
        integral_unit: Some("C"),
        colour: egui::Color32::from_rgb(255, 196, 70),
    },
    SignalView {
        name: "Voltage",
        unit: "V",
        // The time integral of a voltage has no useful meaning here.
        integral_unit: None,
        colour: egui::Color32::from_rgb(80, 200, 255),
    },
    SignalView {
        name: "Power",
        unit: "W",
        integral_unit: Some("J"),
        colour: egui::Color32::from_rgb(230, 120, 220),
    },
];

/// The acquisition path a rate implies.
///
/// Only the raw ADC ports reach the full base rate, so asking for it is
/// asking for them.
fn source_for(rate: u32) -> Source {
    if rate >= js220::BASE_RATE {
        Source::RawAdc
    } else {
        Source::Calibrated
    }
}

/// Label a rate, naming the trade when it is not a calibrated measurement.
fn rate_label(rate: u32) -> String {
    match source_for(rate) {
        Source::RawAdc => format!("{} raw ADC", fmt::rate(rate)),
        Source::Calibrated => fmt::rate(rate),
    }
}

/// Relabel the views for the acquisition path in use.
///
/// Raw ADC counts are not amperes or volts and must never be drawn as though
/// they were, so the names, units and the integral all change with the source.
fn views_for(source: Source, channels: (u8, u8)) -> Vec<SignalView> {
    match source {
        Source::Calibrated => VIEWS.to_vec(),
        Source::RawAdc => vec![
            SignalView {
                name: adc_name(channels.0),
                unit: "counts",
                integral_unit: None,
                colour: VIEWS[0].colour,
            },
            SignalView {
                name: adc_name(channels.1),
                unit: "counts",
                integral_unit: None,
                colour: VIEWS[1].colour,
            },
        ],
    }
}

/// Name for an ADC channel. The instrument publishes no metadata saying what
/// each one measures, so they are identified by number and nothing more.
fn adc_name(channel: u8) -> &'static str {
    match channel {
        0 => "ADC 0",
        1 => "ADC 1",
        2 => "ADC 2",
        _ => "ADC 3",
    }
}

/// Colour of peak markers.
const PEAK: egui::Color32 = egui::Color32::from_rgb(255, 120, 120);
/// Colour of the threshold level and its regions.
const THRESHOLD: egui::Color32 = egui::Color32::from_rgb(255, 200, 80);

/// Colour of marker A.
const MARKER_A: egui::Color32 = egui::Color32::from_rgb(255, 235, 120);
/// Colour of marker B.
const MARKER_B: egui::Color32 = egui::Color32::from_rgb(140, 255, 180);

/// Application state.
pub struct App {
    acq: Acquisition,
    serial: Option<String>,
    devices: Vec<String>,
    rate: u32,
    statistics: bool,
    record_path: String,
    show_log: bool,
    /// Keep the newest data in view. Shared, because the plots share one
    /// time axis.
    follow: [bool; VIEWS.len()],
    /// Per-plot: scale Y to the data in view. Independent of `follow`, so
    /// zooming Y does not stop the trace tracking live data, and following
    /// live data does not stamp out a Y zoom.
    auto_y: [bool; VIEWS.len()],
    /// Per-plot Y range, owned here rather than by the plot widget.
    view_y: [Option<(f64, f64)>; VIEWS.len()],
    /// Which signals are plotted at all.
    visible: [bool; VIEWS.len()],
    /// Seconds of history shown while following, or None to accumulate the
    /// whole capture the way the instrument software does.
    window_seconds: Option<f64>,
    /// The x range each plot is showing, carried between frames so the data
    /// can be aggregated to the view before it is drawn.
    view_x: Option<(f64, f64)>,
    marker_a: Option<f64>,
    marker_b: Option<f64>,
    /// What the current drag is moving, if anything.
    dragging: Option<Grab>,
    /// Plot x at the previous drag frame, so movement is applied
    /// incrementally and survives a zoom mid-drag.
    drag_last_x: f64,
    /// Scratch for aggregated buckets, reused every frame.
    buckets: Vec<Bucket>,
    /// Mark the tallest peaks on every plot.
    show_peaks: bool,
    /// How many peaks to mark.
    peak_count: usize,
    /// Per-plot threshold level, and whether it is active.
    threshold: [f64; VIEWS.len()],
    threshold_on: [bool; VIEWS.len()],
    /// Which side of the threshold counts.
    threshold_edge: Edge,
    /// The x range actually drawn last frame, which is what "export the view"
    /// means. Distinct from `view_x`, which is only set once the user has
    /// panned or zoomed.
    drawn_x: Option<(f64, f64)>,
    /// How many rows an export aggregates the span into.
    export_rows: usize,
    /// Result of the last export, shown in the status bar.
    export_status: Option<String>,
    /// Which general purpose inputs to capture on the next start.
    ///
    /// Applied when streaming begins rather than live: the ports are enabled
    /// as part of the stream configuration, and toggling one mid-capture
    /// would leave a trace whose history covers a different set of lines
    /// than its tail.
    gpi_enabled: [bool; js220::proto::stream::GPI_CHANNELS.len()],
    /// Which captured lines are drawn. Separate from capture, so hiding a
    /// row costs nothing and loses nothing.
    gpi_visible: [bool; js220::proto::stream::GPI_CHANNELS.len()],
    /// What the inputs compare against.
    gpio_vref: GpioVref,
    /// Height of each logic plot, which needs far less room than a waveform.
    gpi_height: f32,
    /// Time under the pointer on whichever plot it is over, so markers can be
    /// placed where you are looking rather than at a fixed fraction of the
    /// view.
    hover_t: Option<f64>,
    /// Time the marker context menu was opened at.
    menu_t: Option<f64>,
}

/// Which span an export covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportSpan {
    /// Everything currently on screen.
    View,
    /// Only what lies between the two markers.
    Markers,
}

/// Identifies one of the two markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    A,
    B,
}

/// What a press on a plot took hold of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grab {
    /// One marker, moved on its own.
    Marker(Marker),
    /// The span between the markers, moved rigidly.
    Span,
}

impl App {
    /// Build the application and start the acquisition thread.
    ///
    /// `autostart` begins streaming immediately if a device is present, and
    /// `gpi` pre-selects general purpose inputs to capture with it.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        autostart: bool,
        gpi: Option<Vec<u8>>,
        vref_3v3: bool,
        markers: Option<(f64, f64)>,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            acq: Acquisition::spawn(crate::acquire::BUCKETS),
            serial: None,
            devices: Vec::new(),
            rate: 1_000_000,
            statistics: true,
            record_path: default_record_path(),
            show_log: false,
            follow: [true; VIEWS.len()],
            auto_y: [true; VIEWS.len()],
            view_y: [None; VIEWS.len()],
            visible: [true; VIEWS.len()],
            window_seconds: None,
            view_x: None,
            marker_a: markers.map(|(a, _)| a),
            marker_b: markers.map(|(_, b)| b),
            dragging: None,
            drag_last_x: 0.0,
            buckets: Vec::new(),
            show_peaks: false,
            peak_count: 5,
            threshold: [0.0; VIEWS.len()],
            threshold_on: [false; VIEWS.len()],
            threshold_edge: Edge::Above,
            drawn_x: None,
            // Enough to resolve detail in a spreadsheet without producing a
            // file that takes a minute to open.
            export_rows: 20_000,
            export_status: None,
            gpi_enabled: gpi_selection(gpi.as_deref()),
            gpi_visible: [true; js220::proto::stream::GPI_CHANNELS.len()],
            gpio_vref: if vref_3v3 {
                GpioVref::V3p3
            } else {
                GpioVref::External
            },
            gpi_height: 46.0,
            hover_t: None,
            menu_t: None,
        };
        app.rescan();
        if autostart && app.serial.is_some() {
            app.acq.send(app.start_command());
        }
        app
    }

    /// Whether a captured line is drawn.
    fn gpi_shown(&self, channel: u8) -> bool {
        js220::proto::stream::GPI_CHANNELS
            .iter()
            .position(|c| *c == channel)
            .is_some_and(|i| self.gpi_visible[i])
    }

    /// The general purpose input channels currently selected.
    fn gpi_channels(&self) -> Vec<u8> {
        js220::proto::stream::GPI_CHANNELS
            .iter()
            .zip(self.gpi_enabled)
            .filter(|(_, on)| *on)
            .map(|(c, _)| *c)
            .collect()
    }

    /// The command that begins streaming with the current settings.
    fn start_command(&self) -> Command {
        Command::Start {
            serial: self.serial.clone(),
            source: source_for(self.rate),
            rate: self.rate,
            statistics: self.statistics,
            gpi: self.gpi_channels(),
            gpio_vref: self.gpio_vref,
        }
    }

    fn rescan(&mut self) {
        self.devices = match js220::list() {
            Ok(list) => list.into_iter().map(|d| d.serial().to_owned()).collect(),
            Err(_) => Vec::new(),
        };
        if self.serial.is_none() {
            self.serial = self.devices.first().cloned();
        }
    }

    fn running(&self, shared: &Shared) -> bool {
        matches!(shared.status, Status::Running | Status::Connecting)
    }

    /// Whether a device is open, whether or not it is streaming.
    ///
    /// An open device holds the target's supply, so controls that act on the
    /// instrument stay available after Stop.
    fn connected(&self, shared: &Shared) -> bool {
        matches!(
            shared.status,
            Status::Running | Status::Connecting | Status::Open
        )
    }

    /// The time span an export would cover, if there is one.
    fn export_range(&self, span: ExportSpan) -> Option<(f64, f64)> {
        export_range(span, self.drawn_x, (self.marker_a, self.marker_b))
    }

    /// Write the visible signals over `span` to a file the user picks.
    ///
    /// The span is re-aggregated at the export resolution rather than reusing
    /// the on-screen buckets, so the file does not inherit the width of the
    /// window it happened to be exported from.
    fn export(&mut self, shared: &Shared, span: ExportSpan) {
        let Some((lo, hi)) = self.export_range(span) else {
            self.export_status = Some(match span {
                ExportSpan::View => "Nothing captured to export".to_string(),
                ExportSpan::Markers => "Place two markers first".to_string(),
            });
            return;
        };

        let views = views_for(shared.source, (0, 1));
        let coarse = [&shared.current, &shared.voltage, &shared.power];
        let raw = [&shared.raw_current, &shared.raw_voltage, &shared.raw_power];

        // Collected before the dialog opens: the data is borrowed from the
        // shared snapshot, and the dialog blocks for as long as the user
        // takes to answer.
        let mut data: Vec<(&'static str, &'static str, Vec<Bucket>)> = Vec::new();
        for (i, view) in views.iter().enumerate() {
            if !self.visible[i] {
                continue;
            }
            let mut buckets = Vec::new();
            aggregate_for_view(raw[i], coarse[i], (lo, hi), self.export_rows, &mut buckets);
            data.push((view.name, view.unit, buckets));
        }
        // Logic lines go in the same file, on the same time axis: lining an
        // edge up against a current step is the whole reason to capture both.
        for line in shared.gpi.iter().filter(|t| self.gpi_shown(t.channel)) {
            let mut buckets = Vec::new();
            aggregate_for_view(
                &line.raw,
                &line.coarse,
                (lo, hi),
                self.export_rows,
                &mut buckets,
            );
            data.push((gpi_name(line.channel), "", buckets));
        }
        if data.iter().all(|(_, _, b)| b.is_empty()) {
            self.export_status = Some("No samples in that span".to_string());
            return;
        }

        let Some(path) = rfd::FileDialog::new()
            .set_title("Export measurements")
            .add_filter("Comma separated values", &[export::Format::Csv.extension()])
            .add_filter("Excel workbook", &[export::Format::Xlsx.extension()])
            .set_file_name(format!("joulescope.{}", export::Format::Csv.extension()))
            .save_file()
        else {
            // Cancelled: say nothing, because the user already knows.
            return;
        };

        let columns: Vec<Column<'_>> = data
            .iter()
            .map(|(name, unit, buckets)| Column {
                name,
                unit,
                buckets,
            })
            .collect();
        self.export_status = Some(match export::write(&path, &columns) {
            Ok(rows) => format!(
                "Exported {rows} rows x {} signals to {}",
                columns.len(),
                path.display()
            ),
            Err(e) => format!("Export failed: {e}"),
        });
    }

    /// Where a marker should go when no particular time was named: under the
    /// pointer if it is over a plot, otherwise the middle of what is drawn.
    ///
    /// This used to read `view_x`, which is only set once the view has been
    /// panned or zoomed. On a fresh capture it is `None`, so the marker
    /// buttons silently did nothing at all until you happened to drag the
    /// plot first.
    fn marker_default(&self) -> Option<f64> {
        marker_default(self.hover_t, self.drawn_x)
    }

    /// Place a pair of markers around `t`, spanning a tenth of the view.
    fn place_pair(&mut self, t: f64) {
        let span = self.drawn_x.map_or(1.0, |(lo, hi)| (hi - lo) * 0.05);
        self.marker_a = Some(t - span);
        self.marker_b = Some(t + span);
    }

    /// Place a single marker at `t`, dropping any second one.
    fn place_single(&mut self, t: f64) {
        self.marker_a = Some(t);
        self.marker_b = None;
    }

    /// Marker actions, offered by right-clicking any plot.
    fn marker_menu(&mut self, ui: &mut egui::Ui) {
        let t = self.menu_t;
        let here = |t: Option<f64>| match t {
            Some(t) => format!(" at {}", fmt::clock(t)),
            None => String::new(),
        };
        ui.label(egui::RichText::new(format!("Markers{}", here(t))).strong());
        if ui
            .add_enabled(t.is_some(), egui::Button::new("Pair here"))
            .clicked()
        {
            if let Some(t) = t {
                self.place_pair(t);
            }
            ui.close_menu();
        }
        if ui
            .add_enabled(t.is_some(), egui::Button::new("Single marker here"))
            .clicked()
        {
            if let Some(t) = t {
                self.place_single(t);
            }
            ui.close_menu();
        }
        ui.separator();
        if ui
            .add_enabled(t.is_some(), egui::Button::new("Move A here"))
            .clicked()
        {
            self.marker_a = t;
            ui.close_menu();
        }
        if ui
            .add_enabled(t.is_some(), egui::Button::new("Move B here"))
            .clicked()
        {
            self.marker_b = t;
            ui.close_menu();
        }
        ui.separator();
        if ui
            .add_enabled(
                self.marker_a.is_some() || self.marker_b.is_some(),
                egui::Button::new("Clear markers"),
            )
            .clicked()
        {
            self.marker_a = None;
            self.marker_b = None;
            ui.close_menu();
        }
    }
}

/// Hold a panned or zoomed view inside the span that actually has data.
///
/// Zooming out stops at the whole capture rather than carrying on into empty
/// time, and panning stops at either end. Widening past what exists simply
/// shows all of it, which is what "zoom out" is being asked for anyway.
fn clamp_view(view: (f64, f64), first: f64, last: f64) -> (f64, f64) {
    let (lo, hi) = view;
    // A capture with no width, or times that are not comparable, has nothing
    // to clamp against.
    if last <= first || !last.is_finite() || !first.is_finite() {
        return view;
    }
    let width = hi - lo;
    if width >= last - first {
        return (first, last);
    }
    if lo < first {
        return (first, first + width);
    }
    if hi > last {
        return (last - width, last);
    }
    view
}

/// Where a plot's measurement block goes, and how it is anchored.
///
/// With a pair of markers down it sits just right of the span, as the
/// instrument software does it, so the numbers are next to the region they
/// describe. When that would run off the right edge it flips to the left of
/// the span instead, because a block half off screen reads as missing. With
/// no span it falls back to the top left corner, clear of the axis labels.
fn stats_anchor(
    bounds: &egui_plot::PlotBounds,
    markers: (Option<f64>, Option<f64>),
) -> (PlotPoint, egui::Align2) {
    let (lo, hi) = (bounds.min()[0], bounds.max()[0]);
    let width = hi - lo;
    let gap = width * 0.008;
    let top = bounds.max()[1];
    match markers {
        (Some(a), Some(b)) => {
            let right = a.max(b);
            // Roughly what the block occupies; enough to tell whether it
            // would be cut off.
            if right + gap + width * 0.16 <= hi {
                (PlotPoint::new(right + gap, top), egui::Align2::LEFT_TOP)
            } else {
                (PlotPoint::new(a.min(b) - gap, top), egui::Align2::RIGHT_TOP)
            }
        }
        _ => (
            PlotPoint::new(lo + width * 0.012, top),
            egui::Align2::LEFT_TOP,
        ),
    }
}

/// The measurements reported for a marker span, in the order shown.
#[allow(clippy::type_complexity)]
const FIELDS: &[(&str, fn(&RangeStats) -> f64)] = &[
    ("avg", |s| s.mean as f64),
    ("std", |s| s.std as f64),
    ("rms", |s| s.rms as f64),
    ("min", |s| s.min as f64),
    ("max", |s| s.max as f64),
    ("p2p", |s| s.p2p() as f64),
];

/// Where a marker goes when no particular time was named.
///
/// The pointer wins when it is over the drawn range. It is deliberately not
/// cleared when the pointer leaves a plot: reaching the toolbar means moving
/// off the plot, and a button that disabled itself on the way to being
/// clicked would be useless. A remembered position that has since scrolled
/// out of view is dropped, since placing a marker there would put it
/// somewhere nobody can see.
fn marker_default(hover: Option<f64>, drawn: Option<(f64, f64)>) -> Option<f64> {
    let (lo, hi) = drawn?;
    match hover {
        Some(t) if t >= lo && t <= hi => Some(t),
        _ => Some(lo + (hi - lo) * 0.5),
    }
}

/// Turn a list of channel numbers into the per-channel selection flags.
///
/// Numbers the instrument does not have are ignored rather than rejected:
/// this comes from a command line, and refusing to start over a typo in an
/// optional switch would be worse than not capturing that line.
fn gpi_selection(channels: Option<&[u8]>) -> [bool; js220::proto::stream::GPI_CHANNELS.len()] {
    let mut out = [false; js220::proto::stream::GPI_CHANNELS.len()];
    let Some(channels) = channels else { return out };
    for (i, c) in js220::proto::stream::GPI_CHANNELS.iter().enumerate() {
        out[i] = channels.contains(c);
    }
    out
}

fn default_record_path() -> String {
    std::env::current_dir()
        .unwrap_or_default()
        .join("capture.f32")
        .to_string_lossy()
        .into_owned()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(33));
        let snapshot = self.acq.shared().clone();
        let shared = snapshot.lock().expect("shared snapshot poisoned");

        // Delete clears the markers from anywhere, so there is never a
        // question of which button does it.
        if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            self.marker_a = None;
            self.marker_b = None;
        }

        self.toolbar(ctx, &shared);
        self.status_bar(ctx, &shared);
        if self.show_log {
            self.log_panel(ctx, &shared);
        }
        self.plots(ctx, &shared);
    }
}

impl App {
    fn toolbar(&mut self, ctx: &egui::Context, shared: &Shared) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let running = self.running(shared);
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("device")
                        .width(100.0)
                        .selected_text(self.serial.clone().unwrap_or_else(|| "no device".into()))
                        .show_ui(ui, |ui| {
                            for d in &self.devices {
                                ui.selectable_value(&mut self.serial, Some(d.clone()), d);
                            }
                        });
                    if ui.button("⟳").on_hover_text("Rescan for devices").clicked() {
                        self.rescan();
                    }
                    // One control, because a rate is what the user is
                    // choosing. 2 MHz is only reachable through the raw ADC
                    // ports, so picking it selects that path; the label says
                    // so rather than hiding it behind a second selector.
                    egui::ComboBox::from_id_salt("rate")
                        .width(150.0)
                        .selected_text(rate_label(self.rate))
                        .show_ui(ui, |ui| {
                            for r in RATES {
                                ui.selectable_value(&mut self.rate, r, rate_label(r))
                                    .on_hover_text(if r >= js220::BASE_RATE {
                                        "Uncalibrated ADC counts: no gain, offset or range correction applied"
                                    } else {
                                        "Calibrated amperes and volts"
                                    });
                            }
                        });
                });

                if running {
                    if ui.button("⏹ Stop").clicked() {
                        self.acq.send(Command::Stop);
                    }
                } else {
                    if ui
                        .add_enabled(self.serial.is_some(), egui::Button::new("▶ Start"))
                        .clicked()
                    {
                        self.acq.send(self.start_command());
                    }
                    // Stopping leaves the device open so the target keeps its
                    // supply; this is how you let go of it deliberately.
                    if ui
                        .add_enabled(self.connected(shared), egui::Button::new("⏏"))
                        .on_hover_text(
                            "Release the instrument. The target loses power: the JS220 reverts to its own defaults on disconnect, and those open the current path.",
                        )
                        .clicked()
                    {
                        self.acq.send(Command::Disconnect);
                    }
                }

                ui.separator();
                ui.label("Signals:");
                for (i, view) in views_for(source_for(self.rate), (0, 1)).iter().enumerate() {
                    let short = match source_for(self.rate) {
                        Source::Calibrated => view.unit[..1].to_string(),
                        Source::RawAdc => i.to_string(),
                    };
                    let label = egui::RichText::new(short).color(view.colour).strong();
                    ui.toggle_value(&mut self.visible[i], label)
                        .on_hover_text(view.name);
                }

                ui.separator();
                ui.label("X:");
                let before = self.window_seconds;
                egui::ComboBox::from_id_salt("window")
                    .width(76.0)
                    .selected_text(match self.window_seconds {
                        Some(w) => fmt::seconds(w),
                        None => "All".to_string(),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.window_seconds, None, "All")
                            .on_hover_text("Accumulate the whole capture");
                        for w in WINDOWS {
                            ui.selectable_value(
                                &mut self.window_seconds,
                                Some(w),
                                fmt::seconds(w),
                            );
                        }
                    });
                if self.window_seconds != before {
                    // Picking a span is a request to see that span of live
                    // data, not to keep staring at a frozen range of some
                    // other width.
                    self.follow = [true; VIEWS.len()];
                }
                if ui.button("🔍−").on_hover_text("Show more time").clicked() {
                    self.window_seconds = match self.window_seconds {
                        // Widening past the longest span is the whole capture.
                        Some(w) if w >= 60.0 => None,
                        Some(w) => Some(w * 2.0),
                        None => None,
                    };
                    self.follow = [true; VIEWS.len()];
                }
                if ui.button("🔍+").on_hover_text("Show less time").clicked() {
                    let current = self.window_seconds.unwrap_or_else(|| {
                        self.view_x.map_or(1.0, |(lo, hi)| hi - lo)
                    });
                    self.window_seconds = Some((current / 2.0).max(1e-4));
                    self.follow = [true; VIEWS.len()];
                }
                if ui
                    .button("⟲ Auto")
                    .on_hover_text("Resume following the newest data")
                    .clicked()
                {
                    self.follow = [true; VIEWS.len()];
                }

                ui.separator();
                ui.toggle_value(&mut self.show_peaks, "Peaks")
                    .on_hover_text("Mark the tallest peaks on every plot");
                if self.show_peaks {
                    ui.add(
                        egui::DragValue::new(&mut self.peak_count)
                            .range(1..=50)
                            .speed(0.2)
                            .prefix("top "),
                    );
                }
                egui::ComboBox::from_id_salt("edge")
                    .width(72.0)
                    .selected_text(match self.threshold_edge {
                        Edge::Above => "above",
                        Edge::Below => "below",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.threshold_edge, Edge::Above, "above");
                        ui.selectable_value(&mut self.threshold_edge, Edge::Below, "below");
                    });

                ui.separator();
                ui.label("Markers:");
                let at = self.marker_default();
                if ui
                    .add_enabled(at.is_some(), egui::Button::new("A|B"))
                    .on_hover_text(
                        "Place a pair around the pointer, or the middle of the view. Drag them in any plot; right-click a plot for more.",
                    )
                    .clicked()
                {
                    if let Some(t) = at {
                        self.place_pair(t);
                    }
                }
                if ui
                    .add_enabled(at.is_some(), egui::Button::new("A"))
                    .on_hover_text("Place a single marker under the pointer")
                    .clicked()
                {
                    if let Some(t) = at {
                        self.place_single(t);
                    }
                }
                if ui
                    .button("✖")
                    .on_hover_text("Remove the markers (or press Delete)")
                    .clicked()
                {
                    self.marker_a = None;
                    self.marker_b = None;
                }

                ui.separator();
                if ui
                    .button("Clear")
                    .on_hover_text(
                        "Discard the captured history. The markers go with it, since they would be left pointing at data that no longer exists.",
                    )
                    .clicked()
                {
                    self.acq.send(Command::ClearTraces);
                    self.marker_a = None;
                    self.marker_b = None;
                }

                ui.separator();
                let before = self.gpi_enabled;
                ui.menu_button("Logic", |ui| {
                    ui.label("Capture these inputs:");
                    for (i, channel) in js220::proto::stream::GPI_CHANNELS.iter().enumerate() {
                        let label = if *channel == 7 {
                            "IN 7 (trigger)".to_string()
                        } else {
                            format!("IN {channel}")
                        };
                        ui.checkbox(&mut self.gpi_enabled[i], label);
                    }
                    ui.separator();
                    // Changing this mid-capture would restart the stream, so
                    // it is the one setting here that waits.
                    ui.add_enabled_ui(!running, |ui| {
                        ui.label("Compare against:");
                        ui.radio_value(&mut self.gpio_vref, GpioVref::External, "External Vref")
                            .on_hover_text(
                                "Senses against the reference you drive on the Vref pin. With nothing connected there the inputs read a fixed level and no edge ever appears.",
                            );
                        ui.radio_value(&mut self.gpio_vref, GpioVref::V3p3, "3.3 V")
                            .on_hover_text("The instrument supplies the reference itself");
                    });
                    if running {
                        ui.weak("Vref applies on the next start");
                    }
                });
                if self.gpi_enabled != before && running {
                    // The GPI ports are independent of the analog ones, so
                    // this takes effect immediately and the capture so far
                    // is kept.
                    self.acq.send(Command::SetGpi {
                        channels: self.gpi_channels(),
                    });
                }
                // Show and hide, which is not the same as capture: a hidden
                // line keeps recording, so it is still there when you want
                // to look at it.
                for (i, channel) in js220::proto::stream::GPI_CHANNELS.iter().enumerate() {
                    if !self.gpi_enabled[i] {
                        continue;
                    }
                    let label = egui::RichText::new(format!("{channel}")).color(LOGIC);
                    ui.toggle_value(&mut self.gpi_visible[i], label)
                        .on_hover_text(format!("Show {}", gpi_name(*channel)));
                }
                if self.gpi_enabled.iter().any(|v| *v) {
                    ui.add(
                        egui::DragValue::new(&mut self.gpi_height)
                            .range(24.0..=160.0)
                            .speed(1.0)
                            .suffix(" px"),
                    )
                    .on_hover_text("Height of each logic plot");
                }

                ui.separator();
                ui.menu_button("💾 Export", |ui| {
                    ui.label("Aggregate the span into");
                    ui.add(
                        egui::DragValue::new(&mut self.export_rows)
                            .range(100..=1_000_000)
                            .speed(50.0)
                            .suffix(" rows"),
                    )
                    .on_hover_text(
                        "Each row carries the mean, minimum and maximum of one time slice, so short spikes survive the export.",
                    );
                    ui.separator();
                    let markers = self.export_range(ExportSpan::Markers).is_some();
                    if ui
                        .add_enabled(
                            self.drawn_x.is_some(),
                            egui::Button::new("The visible span…"),
                        )
                        .on_hover_text("Every signal shown, over the time range on screen")
                        .clicked()
                    {
                        ui.close_menu();
                        self.export(shared, ExportSpan::View);
                    }
                    if ui
                        .add_enabled(markers, egui::Button::new("Between the markers…"))
                        .on_hover_text("Only what lies between marker A and marker B")
                        .clicked()
                    {
                        ui.close_menu();
                        self.export(shared, ExportSpan::Markers);
                    }
                    ui.separator();
                    ui.weak("The extension picks the format: .csv or .xlsx");
                });

                ui.separator();
                if shared.recording.is_some() {
                    if ui.button("⏹ Rec").clicked() {
                        self.acq.send(Command::RecordStop);
                    }
                } else if ui
                    .add_enabled(running, egui::Button::new("⏺ Rec"))
                    .on_hover_text("Interleaved little-endian f32: current, voltage, power")
                    .clicked()
                {
                    self.acq.send(Command::RecordStart {
                        path: std::path::PathBuf::from(self.record_path.clone()),
                    });
                }
                ui.separator();
                // Sitting in series with the target makes the ranging MOSFETs
                // its supply switch, so this really does cut power to whatever
                // is attached. Coloured and labelled accordingly rather than
                // looking like a display option.
                let on = shared.target_power;
                let (label, colour) = if on {
                    ("⏻ Target ON", egui::Color32::from_rgb(120, 220, 120))
                } else {
                    ("⏻ Target OFF", egui::Color32::from_rgb(240, 120, 120))
                };
                let button = egui::Button::new(egui::RichText::new(label).color(colour).strong());
                if ui
                    .add_enabled(self.connected(shared), button)
                    .on_hover_text(
                        "Open or close the current path through the instrument. The JS220 sits in series with the target, so opening it should cut the target's supply and reset it — confirm at the target, not from the current reading, which measures nothing once the path is open.",
                    )
                    .clicked()
                {
                    self.acq.send(Command::SetTargetPower { on: !on });
                }

                ui.toggle_value(&mut self.show_log, "Log");
            });
            ui.add_space(4.0);
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context, shared: &Shared) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (text, colour) = match &shared.status {
                    Status::Idle => ("idle".to_string(), egui::Color32::GRAY),
                    Status::Open => (
                        "connected".to_string(),
                        egui::Color32::from_rgb(140, 200, 255),
                    ),
                    Status::Connecting => (
                        "connecting…".to_string(),
                        egui::Color32::from_rgb(230, 200, 90),
                    ),
                    Status::Running => (
                        "streaming".to_string(),
                        egui::Color32::from_rgb(120, 220, 120),
                    ),
                    Status::Failed(e) => (
                        format!("error: {e}"),
                        egui::Color32::from_rgb(240, 120, 120),
                    ),
                };
                ui.colored_label(colour, format!("● {text}"))
                    .on_hover_text(shared.status.label());
                ui.separator();
                if let Some(s) = &shared.serial {
                    ui.label(s);
                    ui.separator();
                }
                if let Some(v) = &shared.versions {
                    ui.weak(v);
                    ui.separator();
                }
                ui.label(format!("{}/s", fmt::bytes(shared.throughput as u64)));
                ui.separator();
                ui.label(format!("{} samples/s", fmt::count(shared.sample_rate)));
                ui.separator();
                ui.label(fmt::duration(shared.elapsed));
                if let Some(stats) = &shared.statistics {
                    ui.separator();
                    ui.label(format!(
                        "∫ {} · {}",
                        fmt::si(stats.charge, "C"),
                        fmt::si(stats.energy, "J")
                    ));
                }
                if shared.frame_errors > 0 || shared.frame_breaks > 0 {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 200, 90),
                        format!(
                            "{} bad frames · {} breaks",
                            shared.frame_errors, shared.frame_breaks
                        ),
                    );
                }
                if let Some(p) = &shared.recording {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(240, 120, 120),
                        format!("⏺ {} rows", fmt::count(shared.recorded_rows as f64)),
                    );
                    ui.weak(p.display().to_string());
                }
                if let Some((a, b)) = self.export_range(ExportSpan::Markers) {
                    ui.separator();
                    let dt = b - a;
                    ui.colored_label(MARKER_A, "A");
                    ui.weak(fmt::clock(a));
                    ui.colored_label(MARKER_B, "B");
                    ui.weak(fmt::clock(b));
                    ui.strong(format!("Δt {}", fmt::si(dt, "s")));
                    if dt > 0.0 {
                        // The rate a repeating feature would have, which is
                        // what the span measures whenever it brackets one.
                        ui.label(format!("1/Δt {}", fmt::si(1.0 / dt, "Hz")));
                    }
                }
                if let Some(message) = self.export_status.clone() {
                    ui.separator();
                    let failed = message.starts_with("Export failed");
                    let colour = if failed {
                        egui::Color32::from_rgb(240, 120, 120)
                    } else {
                        egui::Color32::from_rgb(140, 200, 255)
                    };
                    ui.colored_label(colour, &message);
                    // Dismissible, because a path is long and the bar is
                    // otherwise the only place live numbers are read.
                    if ui.small_button("✖").on_hover_text("Dismiss").clicked() {
                        self.export_status = None;
                    }
                }
            });
        });
    }

    fn log_panel(&mut self, ctx: &egui::Context, shared: &Shared) {
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(110.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Log");
                    ui.weak(format!("({} lines)", shared.log.len()));
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &shared.log {
                            let colour = match line.level {
                                LogLevel::Info => egui::Color32::GRAY,
                                LogLevel::Warn => egui::Color32::from_rgb(230, 200, 90),
                                LogLevel::Error => egui::Color32::from_rgb(240, 120, 120),
                            };
                            ui.horizontal(|ui| {
                                ui.weak(fmt::duration(line.t));
                                ui.colored_label(colour, &line.text);
                            });
                        }
                    });
            });
    }

    fn plots(&mut self, ctx: &egui::Context, shared: &Shared) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let views = views_for(shared.source, (0, 1));
            let shown: Vec<usize> = (0..views.len()).filter(|i| self.visible[*i]).collect();
            if shown.is_empty() {
                ui.centered_and_justified(|ui| ui.weak("No signals selected"));
                return;
            }
            const HEADER: f32 = 26.0;
            // Logic rows take a fixed slice off the top of the budget; the
            // waveforms divide what is left. Dividing everything between the
            // waveforms first would push the logic rows off the bottom.
            let logic_rows = shared
                .gpi
                .iter()
                .filter(|t| self.gpi_shown(t.channel))
                .count();
            let logic = logic_rows as f32 * (self.gpi_height + 4.0);
            let available = (ui.available_height() - logic).max(120.0);
            let height = (available / shown.len() as f32) - HEADER;
            let columns = (ui.available_width() as usize).clamp(64, 4096);

            let coarse = [&shared.current, &shared.voltage, &shared.power];
            let raw = [&shared.raw_current, &shared.raw_voltage, &shared.raw_power];

            // Every plot shares one x range so the signals stay aligned, which
            // is what makes a marker meaningful across all three.
            let x_range = self.resolve_x_range(raw[I], coarse[I]);
            // What "export the view" refers to, whether or not the user has
            // taken the axis over.
            self.drawn_x = Some(x_range);

            for &i in &shown {
                let view = &VIEWS[i];
                ui.horizontal(|ui| {
                    ui.colored_label(view.colour, egui::RichText::new(view.name).strong());
                    ui.weak(format!("({})", view.unit));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("Fit")
                            .on_hover_text("Track live data and scale Y to it")
                            .clicked()
                        {
                            self.follow[i] = true;
                            self.auto_y[i] = true;
                        }
                        ui.toggle_value(&mut self.auto_y[i], "Auto Y")
                            .on_hover_text("Scale Y to the data in view");
                        // Entered in the plot unit, so a current limit is
                        // typed in amperes and a power limit in watts.
                        ui.add(
                            egui::DragValue::new(&mut self.threshold[i])
                                .speed(0.001)
                                .suffix(format!(" {}", view.unit)),
                        );
                        ui.toggle_value(&mut self.threshold_on[i], "Thresh")
                            .on_hover_text("Mark where the trace crosses the level");
                        ui.toggle_value(&mut self.follow[i], "Follow")
                            .on_hover_text("Keep the newest data in view");
                        // Say where the plotted points came from, so a coarse
                        // trace is never mistaken for the real waveform.
                        let (source, detail) = describe_source(raw[i], x_range, columns);
                        ui.weak(detail).on_hover_text(source);
                    });
                });

                aggregate_for_view(raw[i], coarse[i], x_range, columns, &mut self.buckets);
                // Y is auto-scaled to what is on screen unless the user has
                // taken it over, in which case their range is preserved.
                let y_range = if self.auto_y[i] {
                    let y = y_bounds(&self.buckets);
                    self.view_y[i] = Some(y);
                    y
                } else {
                    let y = self.view_y[i].unwrap_or_else(|| y_bounds(&self.buckets));
                    self.view_y[i] = Some(y);
                    y
                };

                let analysis = Analysis {
                    peaks: self.show_peaks.then_some(self.peak_count),
                    threshold: self.threshold_on[i].then_some(self.threshold[i] as f32),
                    edge: self.threshold_edge,
                };
                let (outcome, resp) = plot_signal(
                    ui,
                    view,
                    &self.buckets,
                    raw[i],
                    coarse[i],
                    height,
                    x_range,
                    y_range,
                    (self.marker_a, self.marker_b),
                    self.dragging,
                    analysis,
                    Some(&i) == shown.first(),
                );
                if let Some(t) = outcome.hover_t {
                    self.hover_t = Some(t);
                }
                if let Some(t) = outcome.secondary_t {
                    self.menu_t = Some(t);
                }
                resp.context_menu(|ui| self.marker_menu(ui));

                if let Some(x) = outcome.new_x {
                    // Panning or zooming time stops the trace chasing the
                    // live edge, on every plot, since they share the axis.
                    self.follow = [false; VIEWS.len()];
                    self.view_x = Some(x);
                }
                if let Some(y) = outcome.new_y {
                    // Y is this plot's alone.
                    self.auto_y[i] = false;
                    self.view_y[i] = Some(y);
                }
                if let Some((grab, t)) = outcome.press {
                    self.dragging = grab;
                    self.drag_last_x = t;
                }
                if let (Some(grab), Some(t)) = (self.dragging, outcome.drag_x) {
                    // Applied as a delta rather than by snapping the marker to
                    // the pointer, so grabbing a line off-centre does not jump
                    // it, and a zoom mid-drag stays consistent.
                    let dx = t - self.drag_last_x;
                    self.drag_last_x = t;
                    match grab {
                        Grab::Marker(Marker::A) => {
                            self.marker_a = self.marker_a.map(|v| v + dx);
                        }
                        Grab::Marker(Marker::B) => {
                            self.marker_b = self.marker_b.map(|v| v + dx);
                        }
                        Grab::Span => {
                            self.marker_a = self.marker_a.map(|v| v + dx);
                            self.marker_b = self.marker_b.map(|v| v + dx);
                        }
                    }
                }
            }

            // Logic lines last, under the waveforms they explain, sharing
            // the same time axis and the same markers.
            for i in 0..shared.gpi.len() {
                let line = &shared.gpi[i];
                if !self.gpi_shown(line.channel) {
                    continue;
                }
                aggregate_for_view(&line.raw, &line.coarse, x_range, columns, &mut self.buckets);
                let (outcome, resp) = plot_logic(
                    ui,
                    line.channel,
                    &self.buckets,
                    &line.raw,
                    self.gpi_height,
                    x_range,
                    (self.marker_a, self.marker_b),
                    self.dragging,
                );
                if let Some(t) = outcome.hover_t {
                    self.hover_t = Some(t);
                }
                if let Some(t) = outcome.secondary_t {
                    self.menu_t = Some(t);
                }
                resp.context_menu(|ui| self.marker_menu(ui));
                if let Some(x) = outcome.new_x {
                    self.follow = [false; VIEWS.len()];
                    self.view_x = Some(x);
                }
                if let Some((grab, t)) = outcome.press {
                    self.dragging = grab;
                    self.drag_last_x = t;
                }
                if let (Some(grab), Some(t)) = (self.dragging, outcome.drag_x) {
                    let dx = t - self.drag_last_x;
                    self.drag_last_x = t;
                    match grab {
                        Grab::Marker(Marker::A) => self.marker_a = self.marker_a.map(|v| v + dx),
                        Grab::Marker(Marker::B) => self.marker_b = self.marker_b.map(|v| v + dx),
                        Grab::Span => {
                            self.marker_a = self.marker_a.map(|v| v + dx);
                            self.marker_b = self.marker_b.map(|v| v + dx);
                        }
                    }
                }
            }

            if ctx.input(|input| input.pointer.any_released()) {
                self.dragging = None;
            }
        });
    }

    /// The x range to draw, either following the newest data or whatever the
    /// user has panned and zoomed to.
    ///
    /// A held view that the data has scrolled away from is snapped back to the
    /// live edge. Leaving it would show an empty plot with no obvious way out,
    /// which is worse than losing the position.
    fn resolve_x_range(&mut self, raw: &RawTrace, coarse: &SignalTrace) -> (f64, f64) {
        // The oldest data lives in the coarse history, which reaches back
        // minutes; the raw ring only holds the last few seconds. Taking the
        // start from raw would make "All" show those few seconds and sweep
        // exactly like a fixed window.
        let (raw_span, coarse_span) = (raw.time_span(), coarse.time_span());
        let first = match (coarse_span, raw_span) {
            (Some((f, _)), _) => f,
            (None, Some((f, _))) => f,
            (None, None) => {
                return self
                    .view_x
                    .unwrap_or((0.0, self.window_seconds.unwrap_or(1.0)))
            }
        };
        let last = match (raw_span, coarse_span) {
            (Some((_, l)), _) => l,
            (None, Some((_, l))) => l,
            (None, None) => first,
        };
        if !self.follow.iter().any(|f| *f) {
            if let Some((lo, hi)) = self.view_x {
                // Still overlapping the retained data: honour it, held
                // inside the capture. Zooming out used to run off the front
                // of it and show half a screen of time before the capture
                // began, which can never hold anything.
                if hi >= first && lo <= last {
                    let held = clamp_view((lo, hi), first, last);
                    self.view_x = Some(held);
                    return held;
                }
                self.follow = [true; VIEWS.len()];
            }
        }
        match self.window_seconds {
            // Accumulate: grow from the start of the capture, the way the
            // instrument software does, rather than sliding a fixed window
            // so the trace appears to sweep past forever.
            None => (first, last.max(first + 1e-6)),
            Some(w) => ((last - w).max(first), last),
        }
    }
}

/// Describe where the drawn points came from, for the per-plot badge.
fn describe_source(raw: &RawTrace, x: (f64, f64), columns: usize) -> (&'static str, String) {
    if raw.is_empty() {
        return ("No samples yet", String::new());
    }
    if raw.covers(x.0, x.1) {
        let span = (x.1 - x.0).max(0.0);
        let samples = (span / raw.sample_period().max(1e-12)).round() as u64;
        let per_column = samples / columns.max(1) as u64;
        if per_column <= 1 {
            (
                "Every sample is drawn individually",
                format!("raw · {} held", fmt::count(raw.len() as f64)),
            )
        } else {
            (
                "Each column shows the true minimum and maximum of the samples it covers",
                format!(
                    "{per_column} samples/px · {} held",
                    fmt::count(raw.len() as f64)
                ),
            )
        }
    } else {
        (
            "Beyond the raw retention window; drawn from the coarse history",
            "history".to_string(),
        )
    }
}

/// Fill `out` with buckets covering `x`, preferring raw samples.
fn aggregate_for_view(
    raw: &RawTrace,
    coarse: &SignalTrace,
    x: (f64, f64),
    columns: usize,
    out: &mut Vec<Bucket>,
) {
    if !raw.is_empty() && raw.covers(x.0, x.1) {
        raw.aggregate(x.0, x.1, columns, out);
        if !out.is_empty() {
            return;
        }
    }
    out.clear();
    if !coarse.is_empty() {
        coarse.aggregate(x.0, x.1, columns, out);
    }
}

/// The time span an export covers, given what is drawn and where the markers
/// are.
///
/// A marker export needs two distinct markers: one marker bounds nothing, and
/// two in the same place bound an empty span.
fn export_range(
    span: ExportSpan,
    drawn: Option<(f64, f64)>,
    markers: (Option<f64>, Option<f64>),
) -> Option<(f64, f64)> {
    match span {
        ExportSpan::View => drawn.filter(|(lo, hi)| hi > lo),
        ExportSpan::Markers => match markers {
            (Some(a), Some(b)) if a != b => Some((a.min(b), a.max(b))),
            _ => None,
        },
    }
}

/// Statistics for the span a marker pair encloses, or the whole view.
fn span_stats(raw: &RawTrace, coarse: &SignalTrace, a: f64, b: f64) -> Option<RangeStats> {
    raw.range_stats(a, b).or_else(|| coarse.range_stats(a, b))
}

/// Detection settings applied to a plot.
#[derive(Debug, Clone, Copy)]
struct Analysis {
    /// Mark this many of the tallest peaks.
    peaks: Option<usize>,
    /// Mark where the trace crosses this level.
    threshold: Option<f32>,
    /// Which side of the level counts.
    edge: Edge,
}

/// What the user did to a plot this frame.
struct PlotOutcome {
    /// A time range the user panned or zoomed to.
    new_x: Option<(f64, f64)>,
    /// A Y range the user panned or zoomed to.
    new_y: Option<(f64, f64)>,
    /// A press happened here: what it took hold of, and where in time.
    press: Option<(Option<Grab>, f64)>,
    /// Where the pointer is, while something is held.
    drag_x: Option<f64>,
    /// Time under the pointer, whenever it is over this plot. This is what
    /// "place a marker here" means.
    hover_t: Option<f64>,
    /// Time under the pointer when the right button went down, kept so the
    /// context menu can act on where it was opened rather than on where the
    /// pointer has since moved.
    secondary_t: Option<f64>,
}

/// How close, in screen points, the pointer must be to grab a marker.
const GRAB_RADIUS: f32 = 14.0;

#[allow(clippy::too_many_arguments)]
fn plot_signal(
    ui: &mut egui::Ui,
    view: &SignalView,
    buckets: &[Bucket],
    raw: &RawTrace,
    coarse: &SignalTrace,
    height: f32,
    x: (f64, f64),
    y: (f64, f64),
    markers: (Option<f64>, Option<f64>),
    dragging: Option<Grab>,
    analysis: Analysis,
    span_label: bool,
) -> (PlotOutcome, egui::Response) {
    let unit = view.unit;
    let plot = Plot::new(view.name)
        .height(height.max(70.0))
        .legend(Legend::default())
        // The viewport is driven from here rather than by the widget. Letting
        // the widget pan and zoom as well means it applies the gesture and
        // this code overwrites it on the same frame, so an axis being
        // auto-scaled simply refuses to move. That is why Y could not be
        // zoomed at all.
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .auto_bounds(egui::Vec2b::FALSE)
        .custom_x_axes(vec![
            AxisHints::new_x().formatter(|mark, _range| fmt::clock(mark.value))
        ])
        .custom_y_axes(vec![AxisHints::new_y()
            .min_thickness(Y_GUTTER)
            .formatter(move |mark, _range| fmt::si_tick(mark.value, unit))])
        .label_formatter(move |name, value| {
            if name.is_empty() {
                format!("{}\n{}", fmt::clock(value.x), fmt::si(value.y, unit))
            } else {
                format!(
                    "{name}\n{}\n{}",
                    fmt::clock(value.x),
                    fmt::si(value.y, unit)
                )
            }
        });

    let response = plot.show(ui, |plot_ui| {
        plot_ui.set_plot_bounds(PlotBounds::from_min_max([x.0, y.0], [x.1, y.1]));

        if !buckets.is_empty() {
            let mut upper: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            let mut lower: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            let mut mean: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            for b in buckets {
                upper.push([b.t, b.max as f64]);
                lower.push([b.t, b.min as f64]);
                mean.push([b.t, b.mean as f64]);
            }
            let spread = upper
                .iter()
                .zip(&lower)
                .any(|(hi, lo)| (hi[1] - lo[1]).abs() > f64::EPSILON);
            if spread {
                plot_ui.line(
                    Line::new(PlotPoints::from(envelope_zigzag(&upper, &lower)))
                        .name("min…max")
                        .color(view.colour.gamma_multiply(0.45))
                        .width(1.0_f32),
                );
            }
            plot_ui.line(
                Line::new(PlotPoints::from(mean))
                    .name(view.name)
                    .color(view.colour)
                    .width(1.2_f32),
            );
        }

        let bounds = plot_ui.plot_bounds();

        // Threshold first, so the level and its regions sit under the markers
        // rather than hiding them.
        if let Some(level) = analysis.threshold {
            plot_ui.hline(
                HLine::new(level as f64)
                    .name("threshold")
                    .color(THRESHOLD)
                    .width(1.0_f32),
            );
            let crossings = peaks::find_crossings(buckets, level, analysis.edge);
            if !crossings.is_empty() {
                // How long the trace spent past the level is usually the
                // point of setting one, so it is reported rather than left to
                // be counted by eye.
                let total: f64 = crossings.iter().map(|c| c.duration()).sum();
                let worst = crossings.iter().map(|c| c.peak).fold(f32::NAN, |a, b| {
                    if a.is_nan() {
                        b
                    } else {
                        match analysis.edge {
                            Edge::Above => a.max(b),
                            Edge::Below => a.min(b),
                        }
                    }
                });
                plot_ui.text(
                    Text::new(
                        PlotPoint::new(
                            bounds.max()[0] - (bounds.max()[0] - bounds.min()[0]) * 0.012,
                            level as f64,
                        ),
                        format!(
                            "{} crossing(s) · {} total · peak {}",
                            crossings.len(),
                            fmt::si(total, "s"),
                            fmt::si(worst as f64, unit)
                        ),
                    )
                    .anchor(egui::Align2::RIGHT_BOTTOM)
                    .color(THRESHOLD),
                );
            }
            for c in crossings {
                // A single-bucket region would be invisible as a rectangle,
                // so give it the width of one bucket.
                let w = (bounds.max()[0] - bounds.min()[0]) * 0.001;
                let (lo, hi) = (c.start - w, c.end + w);
                plot_ui.polygon(
                    Polygon::new(PlotPoints::from(vec![
                        [lo, bounds.min()[1]],
                        [hi, bounds.min()[1]],
                        [hi, bounds.max()[1]],
                        [lo, bounds.max()[1]],
                    ]))
                    .name("over threshold")
                    .fill_color(THRESHOLD.gamma_multiply(0.15))
                    .stroke(egui::Stroke::NONE),
                );
            }
        }

        if let Some(limit) = analysis.peaks {
            // Separation is a fraction of the view, so zooming in resolves
            // peaks that were merged when zoomed out.
            let separation = (bounds.max()[0] - bounds.min()[0]) * 0.02;
            let found = peaks::find_peaks(buckets, separation, limit);
            if !found.is_empty() {
                plot_ui.points(
                    Points::new(PlotPoints::from(
                        found
                            .iter()
                            .map(|p| [p.t, p.value as f64])
                            .collect::<Vec<_>>(),
                    ))
                    .name("peaks")
                    .shape(MarkerShape::Diamond)
                    .radius(4.0_f32)
                    .color(PEAK),
                );
                for p in &found {
                    plot_ui.text(
                        Text::new(
                            PlotPoint::new(p.t, p.value as f64),
                            fmt::si(p.value as f64, unit),
                        )
                        .anchor(egui::Align2::CENTER_BOTTOM)
                        .color(PEAK),
                    );
                }
            }
        }

        if let (Some(a), Some(b)) = markers {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            plot_ui.polygon(
                Polygon::new(PlotPoints::from(vec![
                    [lo, bounds.min()[1]],
                    [hi, bounds.min()[1]],
                    [hi, bounds.max()[1]],
                    [lo, bounds.max()[1]],
                ]))
                .name("A…B")
                .fill_color(egui::Color32::from_rgba_unmultiplied(80, 150, 90, 38))
                .stroke(egui::Stroke::NONE),
            );
            // Once, on the topmost plot: the span is the same in all of them,
            // and repeating it in each would be noise.
            if span_label {
                plot_ui.text(
                    Text::new(
                        PlotPoint::new((lo + hi) * 0.5, bounds.max()[1]),
                        fmt::si(hi - lo, "s"),
                    )
                    .anchor(egui::Align2::CENTER_TOP)
                    .color(egui::Color32::from_gray(225)),
                );
            }
        }
        for (t, name, colour) in [(markers.0, "A", MARKER_A), (markers.1, "B", MARKER_B)] {
            let Some(t) = t else { continue };
            plot_ui.vline(VLine::new(t).name(name).color(colour).width(1.4_f32));
            if let Some(v) = raw.value_at(t) {
                plot_ui.text(
                    Text::new(
                        PlotPoint::new(t, bounds.min()[1]),
                        format!("{name} {}", fmt::si(v as f64, unit)),
                    )
                    .anchor(egui::Align2::LEFT_BOTTOM)
                    .color(colour),
                );
            }
        }

        // Measurements sit beside the span they describe, inside the plot
        // and in this signal's own units, which is where the instrument
        // software puts them and where the eye already is. A bottom panel
        // was tried and is worse: it puts the numbers a long way from the
        // region they belong to.
        let (stats_span, label) = match markers {
            (Some(a), Some(b)) => (Some((a.min(b), a.max(b))), String::new()),
            (Some(a), None) => (Some((x.0, a)), "view → A".to_string()),
            _ => (Some((x.0, x.1)), "view".to_string()),
        };
        if let Some((a, b)) = stats_span {
            if let Some(stats) = span_stats(raw, coarse, a, b) {
                let text = stats_block(&label, &stats, view, (b - a).abs());
                let (pos, align) = stats_anchor(&bounds, markers);
                plot_ui.text(
                    Text::new(pos, text)
                        .anchor(align)
                        .color(egui::Color32::from_gray(205)),
                );
            }
        }
    });

    let outcome = interact(ui, &response, x, y, markers, dragging);
    (outcome, response.response)
}

/// Colour of a logic trace.
const LOGIC: egui::Color32 = egui::Color32::from_rgb(140, 230, 170);

/// Width reserved for the Y axis on every plot, in points.
///
/// Fixed rather than fitted to the labels. egui_plot sizes the gutter from
/// the text it is about to draw, so "600 mA" reserved more room than "5 V"
/// and each plot's data area started at a different x — which put the same
/// instant at a different place on screen in each one, and made a marker
/// span visibly lean. A shared time axis is only shared if it lines up.
const Y_GUTTER: f32 = 58.0;

/// Y range a logic plot draws in, leaving room above and below the two levels.
const LOGIC_Y: (f64, f64) = (-0.2, 1.2);

/// Draw one general purpose input as a logic trace.
///
/// Rendered from the bucket minimum and maximum rather than the mean: where a
/// bucket covers an edge the two differ, and walking between them draws the
/// transition as a vertical at the right instant. Drawing the mean would turn
/// a 50% duty cycle into a flat line at half height, which is a waveform the
/// input never had.
#[allow(clippy::too_many_arguments)]
fn plot_logic(
    ui: &mut egui::Ui,
    channel: u8,
    buckets: &[Bucket],
    raw: &RawTrace,
    height: f32,
    x: (f64, f64),
    markers: (Option<f64>, Option<f64>),
    dragging: Option<Grab>,
) -> (PlotOutcome, egui::Response) {
    let name = gpi_name(channel);
    let plot = Plot::new(format!("gpi{channel}"))
        .height(height.max(24.0))
        // The Y axis is drawn only to reserve the same gutter the waveforms
        // have: without it the logic rows start further left and the markers
        // no longer line up with the traces they explain. Two levels need no
        // tick labels, so it carries none.
        .show_axes([false, true])
        .custom_y_axes(vec![AxisHints::new_y()
            .min_thickness(Y_GUTTER)
            .formatter(|_, _| String::new())])
        .show_grid([true, false])
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .auto_bounds(egui::Vec2b::FALSE)
        .label_formatter(move |_, value| {
            format!(
                "{name}
{}
{}",
                fmt::clock(value.x),
                logic_level(value.y)
            )
        });

    let response = plot.show(ui, |plot_ui| {
        plot_ui.set_plot_bounds(PlotBounds::from_min_max([x.0, LOGIC_Y.0], [x.1, LOGIC_Y.1]));
        let bounds = plot_ui.plot_bounds();

        if let (Some(a), Some(b)) = markers {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            plot_ui.polygon(
                Polygon::new(PlotPoints::from(vec![
                    [lo, bounds.min()[1]],
                    [hi, bounds.min()[1]],
                    [hi, bounds.max()[1]],
                    [lo, bounds.max()[1]],
                ]))
                .name("A…B")
                .fill_color(egui::Color32::from_rgba_unmultiplied(80, 150, 90, 38))
                .stroke(egui::Stroke::NONE),
            );
        }

        if !buckets.is_empty() {
            let mut upper: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            let mut lower: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            for b in buckets {
                if b.is_empty() {
                    continue;
                }
                upper.push([b.t, b.max as f64]);
                lower.push([b.t, b.min as f64]);
            }
            if !upper.is_empty() {
                plot_ui.line(
                    Line::new(PlotPoints::from(envelope_zigzag(&upper, &lower)))
                        .name(name)
                        .color(LOGIC)
                        .width(1.4_f32),
                );
            }
        }

        for (t, label, colour) in [(markers.0, "A", MARKER_A), (markers.1, "B", MARKER_B)] {
            let Some(t) = t else { continue };
            plot_ui.vline(VLine::new(t).name(label).color(colour).width(1.4_f32));
            // The level under the marker, read from the samples rather than
            // the drawn buckets, so it stays exact when zoomed out.
            if let Some(v) = raw.value_at(t) {
                plot_ui.text(
                    Text::new(
                        PlotPoint::new(t, bounds.min()[1]),
                        format!("{label} {}", logic_level(v as f64)),
                    )
                    .anchor(egui::Align2::LEFT_BOTTOM)
                    .color(colour),
                );
            }
        }

        // The line's name and what it is doing, inside the plot because the
        // axes are hidden to keep these rows short.
        let span = match markers {
            (Some(a), Some(b)) => (a.min(b), a.max(b)),
            (Some(a), None) => (x.0, a),
            _ => x,
        };
        let summary = match logic_summary(buckets, span) {
            Some(s) => format!("{name}  {s}"),
            None => name.to_string(),
        };
        plot_ui.text(
            Text::new(
                PlotPoint::new(
                    bounds.min()[0] + (bounds.max()[0] - bounds.min()[0]) * 0.004,
                    bounds.max()[1],
                ),
                summary,
            )
            .anchor(egui::Align2::LEFT_TOP)
            .color(LOGIC),
        );
    });

    // The Y range is fixed, so a Y gesture has nothing to act on; the X half
    // behaves exactly as it does on a waveform, which is what keeps the axis
    // and the markers shared.
    let mut outcome = interact(ui, &response, x, LOGIC_Y, markers, dragging);
    outcome.new_y = None;
    (outcome, response.response)
}

/// Display name for a general purpose input.
fn gpi_name(channel: u8) -> &'static str {
    match channel {
        0 => "IN 0",
        1 => "IN 1",
        2 => "IN 2",
        3 => "IN 3",
        _ => "IN 7",
    }
}

/// Describe a logic level for the hover readout.
fn logic_level(y: f64) -> &'static str {
    if y >= 0.5 {
        "high"
    } else {
        "low"
    }
}

/// Summarize a logic trace over `span`: edges seen and time spent high.
///
/// Counted from the buckets on screen, so it describes what is drawn. A
/// bucket whose minimum and maximum differ held both levels, and counts as at
/// least one edge.
fn logic_summary(buckets: &[Bucket], span: (f64, f64)) -> Option<String> {
    let mut edges = 0usize;
    let mut high = 0usize;
    let mut total = 0usize;
    let mut previous: Option<f32> = None;
    for b in buckets {
        if b.is_empty() || b.t < span.0 || b.t > span.1 {
            continue;
        }
        total += 1;
        if b.mean >= 0.5 {
            high += 1;
        }
        // A bucket holding both levels covered an edge; so did one whose
        // level differs from the bucket before it.
        let within = b.min != b.max;
        let between = previous.is_some_and(|p| p != b.max);
        if within || between {
            edges += 1;
        }
        previous = Some(b.max);
    }
    if total == 0 {
        return None;
    }
    let duty = high as f64 / total as f64 * 100.0;
    Some(format!("{edges} edge(s) · {duty:.0}% high"))
}

/// Apply the pan, zoom and marker gestures to the ranges this code owns.
///
/// Zooming scales about the pointer so whatever is under the cursor stays
/// put. Holding shift restricts a wheel zoom to time, ctrl to the value axis;
/// unmodified it zooms both.
fn interact(
    ui: &egui::Ui,
    response: &egui_plot::PlotResponse<()>,
    x: (f64, f64),
    y: (f64, f64),
    markers: (Option<f64>, Option<f64>),
    dragging: Option<Grab>,
) -> PlotOutcome {
    let r = &response.response;
    let mut out = PlotOutcome {
        new_x: None,
        new_y: None,
        press: None,
        drag_x: None,
        hover_t: None,
        secondary_t: None,
    };

    let plot_x = |pos: egui::Pos2| response.transform.value_from_position(pos).x;

    if r.hovered() {
        out.hover_t = r.hover_pos().map(plot_x);
        if ui.input(|i| i.pointer.secondary_pressed()) {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                out.secondary_t = Some(plot_x(pos));
            }
        }
    }

    // What a press takes hold of is decided at the moment the button goes
    // down, not once the drag is under way. egui only reports a drag after
    // the pointer has moved past its threshold, by which point the cursor has
    // already left a marker it was aimed at, and the grab would miss.
    if r.hovered() && ui.input(|i| i.pointer.primary_pressed()) {
        if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
            out.press = Some((grab_at(&response.transform, markers, pos), plot_x(pos)));
        }
    }

    // The press must have begun on this plot, so the synthetic pointer events
    // a window move or resize produces cannot be mistaken for a gesture.
    let gesture = r.dragged() && r.is_pointer_button_down_on();

    if gesture {
        if let Some(pos) = r.interact_pointer_pos() {
            // Something is held: report where the pointer is and let the
            // caller move it. The view stays put.
            if dragging.is_some() {
                out.drag_x = Some(plot_x(pos));
                return out;
            }
        }
        let d = r.drag_delta();
        if d.length() > 0.0 {
            let per_px = response.transform.dvalue_dpos();
            let dx = d.x as f64 * per_px[0];
            let dy = d.y as f64 * per_px[1];
            out.new_x = Some((x.0 - dx, x.1 - dx));
            out.new_y = Some((y.0 - dy, y.1 - dy));
            return out;
        }
    }

    // Tell the user what is grabbable before they press.
    if dragging.is_none() {
        if let Some(pos) = r.hover_pos() {
            if grab_at(&response.transform, markers, pos).is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }
    }

    let scroll = ui.input(|i| i.raw_scroll_delta.y);
    if r.hovered() && scroll.abs() > 0.0 {
        let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
        let factor = (-scroll as f64 * 0.0015).exp();
        let anchor = r
            .hover_pos()
            .map(|p| response.transform.value_from_position(p));
        let (ax, ay) = match anchor {
            Some(p) => (p.x, p.y),
            None => ((x.0 + x.1) * 0.5, (y.0 + y.1) * 0.5),
        };
        if !ctrl {
            out.new_x = Some(zoom_about(x, ax, factor));
        }
        if !shift {
            out.new_y = Some(zoom_about(y, ay, factor));
        }
    }

    out
}

/// What lies under `pos`: a marker, the span between them, or nothing.
///
/// A marker wins over the span, so the edges of a selection stay adjustable
/// rather than being swallowed by the region they bound.
fn grab_at(
    transform: &egui_plot::PlotTransform,
    markers: (Option<f64>, Option<f64>),
    pos: egui::Pos2,
) -> Option<Grab> {
    let screen_x = |t: f64| transform.position_from_point(&PlotPoint::new(t, 0.0)).x;

    let mut best: Option<(f32, Marker)> = None;
    for (t, which) in [(markers.0, Marker::A), (markers.1, Marker::B)] {
        let Some(t) = t else { continue };
        let d = (screen_x(t) - pos.x).abs();
        if d <= GRAB_RADIUS && best.map_or(true, |(bd, _)| d < bd) {
            best = Some((d, which));
        }
    }
    if let Some((_, m)) = best {
        return Some(Grab::Marker(m));
    }

    if let (Some(a), Some(b)) = markers {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        if pos.x > screen_x(lo) && pos.x < screen_x(hi) {
            return Some(Grab::Span);
        }
    }
    None
}

/// Scale `range` by `factor` while keeping `anchor` at the same position.
fn zoom_about(range: (f64, f64), anchor: f64, factor: f64) -> (f64, f64) {
    let width = range.1 - range.0;
    if width <= 0.0 || !factor.is_finite() || factor <= 0.0 {
        return range;
    }
    // A hard floor stops repeated zooming collapsing the range to nothing,
    // which would leave the plot undrawable with no way back.
    let new_width = (width * factor).clamp(1e-9, 1e9);
    let frac = ((anchor - range.0) / width).clamp(0.0, 1.0);
    let lo = anchor - frac * new_width;
    (lo, lo + new_width)
}

/// Trace the min/max band as one continuous polyline.
///
/// Filling it as a polygon looks obvious but renders wrongly: egui fills only
/// convex shapes, and an envelope is deeply non-convex, so the fill comes out
/// as a fan of diagonals across the plot. Walking each column from its minimum
/// to its maximum draws the same band as vertical strokes, which is how a
/// scope renders min/max anyway, and costs one polyline instead of thousands
/// of quads.
fn envelope_zigzag(upper: &[[f64; 2]], lower: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut out = Vec::with_capacity(upper.len() * 2);
    for (i, (hi, lo)) in upper.iter().zip(lower).enumerate() {
        // Alternating direction keeps the joins between columns short, so the
        // connecting segments do not streak across the plot.
        if i % 2 == 0 {
            out.push([hi[0], lo[1]]);
            out.push([hi[0], hi[1]]);
        } else {
            out.push([hi[0], hi[1]]);
            out.push([hi[0], lo[1]]);
        }
    }
    out
}

/// Y bounds covering the buckets, with padding.
fn y_bounds(buckets: &[Bucket]) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for b in buckets {
        lo = lo.min(b.min as f64);
        hi = hi.max(b.max as f64);
    }
    if !lo.is_finite() || !hi.is_finite() {
        return (-1.0, 1.0);
    }
    let span = hi - lo;
    let pad = if span > 0.0 {
        span * 0.08
    } else {
        hi.abs().max(1e-9) * 0.1
    };
    (lo - pad, hi + pad)
}

/// The statistics block drawn inside the plot, mirroring the fields the
/// instrument's own software reports.
fn stats_block(label: &str, s: &RangeStats, view: &SignalView, dt: f64) -> String {
    let mut text = String::new();
    if !label.is_empty() {
        text.push_str(label);
        text.push('\n');
    }
    for (name, read) in FIELDS {
        text.push_str(&format!("{name} {}\n", fmt::si(read(s), view.unit)));
    }
    if let Some(unit) = view.integral_unit {
        text.push_str(&format!("∫   {}", fmt::si(s.mean as f64 * dt, unit)));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_rate_is_one_the_instrument_can_produce() {
        for r in RATES {
            assert_eq!(
                js220::BASE_RATE % r,
                0,
                "{r} Hz does not divide the base rate"
            );
            match source_for(r) {
                // Calibrated rates come from the analog ports, which stop at
                // the 1 Msps ceiling.
                Source::Calibrated => assert!(
                    (js220::MIN_SAMPLE_RATE..=js220::MAX_SAMPLE_RATE).contains(&r),
                    "{r} Hz is outside the calibrated range"
                ),
                // Only the base rate itself is reachable from the ADC ports.
                Source::RawAdc => assert_eq!(r, js220::BASE_RATE),
            }
        }
    }

    #[test]
    fn only_the_base_rate_selects_the_raw_path() {
        assert_eq!(source_for(1_000_000), Source::Calibrated);
        assert_eq!(source_for(500_000), Source::Calibrated);
        assert_eq!(source_for(js220::BASE_RATE), Source::RawAdc);
    }

    #[test]
    fn the_raw_rate_is_labelled_as_uncalibrated() {
        // The one entry that is not a measurement has to say so in the list,
        // or it reads as simply the fastest setting.
        let label = rate_label(js220::BASE_RATE);
        assert!(label.contains("raw"), "got {label}");
        assert!(!rate_label(1_000_000).contains("raw"));
    }

    #[test]
    fn the_rate_list_offers_the_full_base_rate() {
        // This is the whole point of the merged control: 2 MHz is reachable
        // from the rate list rather than hidden behind a second selector.
        assert!(RATES.contains(&js220::BASE_RATE));
    }

    #[test]
    fn offered_windows_are_ordered_and_positive() {
        assert!(WINDOWS.iter().all(|w| *w > 0.0));
        assert!(WINDOWS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn there_is_one_view_per_signal() {
        assert_eq!(VIEWS.len(), js220::Signal::ALL.len());
        assert_eq!(VIEWS[0].name, "Current");
        assert_eq!(VIEWS[1].name, "Voltage");
        assert_eq!(VIEWS[2].name, "Power");
    }

    #[test]
    fn only_current_and_power_integrate_to_something_meaningful() {
        assert_eq!(
            VIEWS[0].integral_unit,
            Some("C"),
            "current integrates to charge"
        );
        assert_eq!(
            VIEWS[2].integral_unit,
            Some("J"),
            "power integrates to energy"
        );
        assert_eq!(VIEWS[1].integral_unit, None, "voltage does not");
    }

    #[test]
    fn colours_are_distinct() {
        for (i, a) in VIEWS.iter().enumerate() {
            for b in VIEWS.iter().skip(i + 1) {
                assert_ne!(a.colour, b.colour);
            }
        }
        assert_ne!(MARKER_A, MARKER_B);
    }

    #[test]
    fn the_envelope_walks_each_column_as_a_vertical_stroke() {
        let upper = [[0.0, 2.0], [1.0, 3.0]];
        let lower = [[0.0, -2.0], [1.0, -3.0]];
        let z = envelope_zigzag(&upper, &lower);
        assert_eq!(z.len(), 4);
        // Both points of a column share its x, so the stroke is vertical.
        assert_eq!(z[0][0], z[1][0]);
        assert_eq!(z[2][0], z[3][0]);
        // Each column spans its full min..max.
        assert_eq!((z[0][1], z[1][1]), (-2.0, 2.0));
        // Alternating direction keeps the join between columns short.
        assert_eq!((z[2][1], z[3][1]), (3.0, -3.0));
    }

    #[test]
    fn the_envelope_of_nothing_is_empty() {
        assert!(envelope_zigzag(&[], &[]).is_empty());
    }

    /// A transform mapping plot x 0..100 onto screen x 0..1000.
    fn transform() -> egui_plot::PlotTransform {
        egui_plot::PlotTransform::new(
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1000.0, 100.0)),
            PlotBounds::from_min_max([0.0, 0.0], [100.0, 10.0]),
            false,
        )
    }

    fn at(x: f32) -> egui::Pos2 {
        egui::pos2(x, 50.0)
    }

    #[test]
    fn a_press_on_a_marker_grabs_it() {
        let t = transform();
        // Markers at plot 20 and 60 are screen 200 and 600.
        let m = (Some(20.0), Some(60.0));
        assert_eq!(grab_at(&t, m, at(200.0)), Some(Grab::Marker(Marker::A)));
        assert_eq!(grab_at(&t, m, at(600.0)), Some(Grab::Marker(Marker::B)));
    }

    #[test]
    fn a_marker_can_be_grabbed_slightly_off_centre() {
        // Hitting a one-pixel line exactly is not reasonable to ask.
        let t = transform();
        let m = (Some(20.0), None);
        assert_eq!(
            grab_at(&t, m, at(200.0 + GRAB_RADIUS - 1.0)),
            Some(Grab::Marker(Marker::A))
        );
        assert_eq!(
            grab_at(&t, m, at(200.0 - GRAB_RADIUS + 1.0)),
            Some(Grab::Marker(Marker::A))
        );
    }

    #[test]
    fn a_press_well_away_from_a_marker_grabs_nothing() {
        let t = transform();
        let m = (Some(20.0), None);
        assert_eq!(grab_at(&t, m, at(200.0 + GRAB_RADIUS * 3.0)), None);
    }

    #[test]
    fn a_press_between_the_markers_grabs_the_span() {
        let t = transform();
        let m = (Some(20.0), Some(60.0));
        assert_eq!(grab_at(&t, m, at(400.0)), Some(Grab::Span));
    }

    #[test]
    fn a_marker_wins_over_the_span_it_bounds() {
        // Otherwise the edges of a selection could never be adjusted.
        let t = transform();
        let m = (Some(20.0), Some(60.0));
        assert_eq!(
            grab_at(&t, m, at(200.0 + 2.0)),
            Some(Grab::Marker(Marker::A))
        );
    }

    #[test]
    fn the_span_needs_both_markers() {
        let t = transform();
        assert_eq!(grab_at(&t, (Some(20.0), None), at(400.0)), None);
        assert_eq!(grab_at(&t, (None, None), at(400.0)), None);
    }

    #[test]
    fn the_span_is_found_whichever_order_the_markers_are_in() {
        let t = transform();
        assert_eq!(
            grab_at(&t, (Some(60.0), Some(20.0)), at(400.0)),
            Some(Grab::Span)
        );
    }

    #[test]
    fn zooming_keeps_the_anchor_under_the_pointer() {
        // Zoom out by 2x about the midpoint: the range doubles around it.
        let (lo, hi) = zoom_about((0.0, 10.0), 5.0, 2.0);
        assert!((lo + 5.0).abs() < 1e-9, "lo {lo}");
        assert!((hi - 15.0).abs() < 1e-9, "hi {hi}");
        // The anchor stays at the same fraction of the range, which is what
        // keeps the point under the cursor still.
        assert!(((5.0 - lo) / (hi - lo) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn zooming_about_an_edge_holds_that_edge() {
        let (lo, hi) = zoom_about((0.0, 10.0), 0.0, 0.5);
        assert!((lo - 0.0).abs() < 1e-9, "the left edge must not move: {lo}");
        assert!((hi - 5.0).abs() < 1e-9, "hi {hi}");
    }

    #[test]
    fn zooming_out_widens_and_in_narrows() {
        let out = zoom_about((0.0, 1.0), 0.5, 4.0);
        assert!(out.1 - out.0 > 1.0);
        let inn = zoom_about((0.0, 1.0), 0.5, 0.25);
        assert!(inn.1 - inn.0 < 1.0);
    }

    #[test]
    fn zooming_cannot_collapse_the_range() {
        // Repeated aggressive zoom must leave something drawable.
        let mut r = (0.0, 1.0);
        for _ in 0..200 {
            r = zoom_about(r, 0.5, 0.1);
        }
        assert!(r.1 - r.0 > 0.0, "range collapsed to {r:?}");
        assert!((r.1 - r.0).is_finite());
    }

    #[test]
    fn zooming_rejects_a_degenerate_range_or_factor() {
        assert_eq!(zoom_about((1.0, 1.0), 1.0, 2.0), (1.0, 1.0));
        assert_eq!(zoom_about((0.0, 1.0), 0.5, 0.0), (0.0, 1.0));
        assert_eq!(zoom_about((0.0, 1.0), 0.5, f64::NAN), (0.0, 1.0));
    }

    #[test]
    fn y_bounds_pad_the_data() {
        let b = vec![
            Bucket {
                t: 0.0,
                min: -1.0,
                max: 1.0,
                mean: 0.0,
            },
            Bucket {
                t: 1.0,
                min: -2.0,
                max: 3.0,
                mean: 0.5,
            },
        ];
        let (lo, hi) = y_bounds(&b);
        assert!(lo < -2.0 && hi > 3.0);
    }

    #[test]
    fn y_bounds_give_a_flat_signal_a_band() {
        let b = vec![Bucket {
            t: 0.0,
            min: 5.0,
            max: 5.0,
            mean: 5.0,
        }];
        let (lo, hi) = y_bounds(&b);
        assert!(hi > lo, "a flat signal still needs a visible band");
    }

    #[test]
    fn y_bounds_of_nothing_are_finite() {
        let (lo, hi) = y_bounds(&[]);
        assert!(lo.is_finite() && hi.is_finite() && hi > lo);
    }

    #[test]
    fn the_stats_block_lists_every_field() {
        let s = RangeStats {
            mean: 1.0,
            std: 0.5,
            rms: 1.1,
            min: 0.0,
            max: 2.0,
            buckets: 10,
        };
        let text = stats_block("Δt 1 s", &s, &VIEWS[0], 1.0);
        for field in ["avg", "std", "rms", "min", "max", "p2p", "∫"] {
            assert!(text.contains(field), "missing {field} in:\n{text}");
        }
    }

    #[test]
    fn the_stats_block_omits_a_meaningless_integral() {
        let s = RangeStats {
            mean: 5.0,
            std: 0.0,
            rms: 5.0,
            min: 5.0,
            max: 5.0,
            buckets: 1,
        };
        let text = stats_block("view", &s, &VIEWS[1], 1.0);
        assert!(
            !text.contains('∫'),
            "voltage has no useful integral:\n{text}"
        );
    }

    #[test]
    fn aggregating_prefers_raw_samples_when_they_cover_the_view() {
        let mut raw = RawTrace::new(1024);
        raw.reconfigure(1024, 1e-3);
        raw.push(&[0.0, 5.0, 0.0, -5.0], 0);
        let coarse = SignalTrace::new(8);
        let mut out = Vec::new();
        aggregate_for_view(&raw, &coarse, (0.0, 3e-3), 100, &mut out);
        assert_eq!(out.len(), 4, "one bucket per raw sample");
    }

    #[test]
    fn aggregating_falls_back_to_history_outside_the_raw_window() {
        let raw = RawTrace::new(16);
        let mut coarse = SignalTrace::new(8);
        coarse.extend(&[1.0; 2000], 0, 1e-6, 1e-3);
        coarse.finish(1e-3);
        let mut out = Vec::new();
        aggregate_for_view(&raw, &coarse, (0.0, 10.0), 100, &mut out);
        assert!(!out.is_empty(), "history must still be drawn");
    }

    #[test]
    fn the_whole_export_path_carries_a_spike_through_to_the_file() {
        // The path the Export button actually takes: raw samples, aggregated
        // to the export resolution, written out. A 1-sample spike must still
        // be in the file after being folded into a bucket, because that is
        // the measurement someone exports to prove.
        let mut raw = RawTrace::new(4096);
        raw.reconfigure(4096, 1e-6);
        let mut samples = vec![0.1f32; 1000];
        samples[500] = 7.5;
        raw.push(&samples, 0);

        let coarse = SignalTrace::new(8);
        let mut buckets = Vec::new();
        // Deliberately coarser than the data, so the spike has to survive
        // being averaged with its neighbours.
        aggregate_for_view(&raw, &coarse, (0.0, 9e-4), 50, &mut buckets);
        assert!(!buckets.is_empty());

        let columns = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &buckets,
        }];
        let path = std::env::temp_dir().join(format!("jsgui-path-{}.csv", std::process::id()));
        let rows = export::write(&path, &columns).unwrap();
        assert!(rows > 0);

        let text = std::fs::read_to_string(&path).unwrap();
        let peak = text
            .lines()
            .skip(1)
            .filter_map(|l| l.split(',').nth(3))
            .filter_map(|f| f.parse::<f64>().ok())
            .fold(f64::MIN, f64::max);
        assert!(
            (peak - 7.5).abs() < 1e-3,
            "the spike was averaged away: peak column reads {peak}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exporting_the_view_uses_what_was_drawn() {
        let r = export_range(ExportSpan::View, Some((1.0, 5.0)), (None, None));
        assert_eq!(r, Some((1.0, 5.0)));
    }

    #[test]
    fn exporting_the_view_needs_something_drawn() {
        assert_eq!(export_range(ExportSpan::View, None, (None, None)), None);
        // A collapsed axis would export an empty span.
        assert_eq!(
            export_range(ExportSpan::View, Some((2.0, 2.0)), (None, None)),
            None
        );
    }

    #[test]
    fn exporting_the_markers_ignores_the_drawn_range() {
        // Markers routinely sit inside the view; the export must follow them,
        // not the window.
        let r = export_range(
            ExportSpan::Markers,
            Some((0.0, 100.0)),
            (Some(3.0), Some(7.0)),
        );
        assert_eq!(r, Some((3.0, 7.0)));
    }

    #[test]
    fn marker_order_does_not_matter_to_an_export() {
        let a = export_range(ExportSpan::Markers, None, (Some(7.0), Some(3.0)));
        let b = export_range(ExportSpan::Markers, None, (Some(3.0), Some(7.0)));
        assert_eq!(a, b);
        assert_eq!(a, Some((3.0, 7.0)));
    }

    #[test]
    fn one_marker_bounds_nothing_to_export() {
        assert_eq!(
            export_range(ExportSpan::Markers, Some((0.0, 1.0)), (Some(3.0), None)),
            None
        );
        assert_eq!(
            export_range(ExportSpan::Markers, Some((0.0, 1.0)), (None, None)),
            None
        );
    }

    #[test]
    fn two_markers_in_the_same_place_bound_nothing() {
        assert_eq!(
            export_range(ExportSpan::Markers, None, (Some(4.0), Some(4.0))),
            None
        );
    }

    fn logic(values: &[(f32, f32)]) -> Vec<Bucket> {
        values
            .iter()
            .enumerate()
            .map(|(i, (min, max))| Bucket {
                t: i as f64,
                min: *min,
                max: *max,
                mean: (min + max) / 2.0,
            })
            .collect()
    }

    fn bounds(lo: f64, hi: f64) -> egui_plot::PlotBounds {
        egui_plot::PlotBounds::from_min_max([lo, 0.0], [hi, 1.0])
    }

    #[test]
    fn a_view_inside_the_capture_is_left_alone() {
        assert_eq!(clamp_view((5.0, 10.0), 0.0, 20.0), (5.0, 10.0));
    }

    #[test]
    fn zooming_out_stops_at_the_whole_capture() {
        // Not half a screen of time from before the capture began.
        assert_eq!(clamp_view((-30.0, 50.0), 0.0, 20.0), (0.0, 20.0));
    }

    #[test]
    fn panning_off_the_front_slides_back_keeping_its_width() {
        let (lo, hi) = clamp_view((-5.0, 5.0), 0.0, 20.0);
        assert_eq!((lo, hi), (0.0, 10.0));
    }

    #[test]
    fn panning_off_the_end_slides_back_keeping_its_width() {
        let (lo, hi) = clamp_view((18.0, 28.0), 0.0, 20.0);
        assert_eq!((lo, hi), (10.0, 20.0));
    }

    #[test]
    fn a_capture_with_no_width_is_left_alone() {
        // Nothing to clamp against before the first samples land.
        assert_eq!(clamp_view((1.0, 2.0), 5.0, 5.0), (1.0, 2.0));
    }

    #[test]
    fn measurements_sit_to_the_right_of_the_span() {
        // Beside the region they describe, as the instrument software does.
        let b = bounds(0.0, 100.0);
        let (pos, align) = stats_anchor(&b, (Some(10.0), Some(20.0)));
        assert_eq!(align, egui::Align2::LEFT_TOP);
        assert!(pos.x > 20.0, "placed at {}", pos.x);
        assert_eq!(pos.y, 1.0, "aligned to the top of the plot");
    }

    #[test]
    fn measurements_flip_left_rather_than_run_off_the_edge() {
        // A span ending at the right edge leaves no room on that side, and a
        // block half off screen reads as missing.
        let b = bounds(0.0, 100.0);
        let (pos, align) = stats_anchor(&b, (Some(80.0), Some(99.0)));
        assert_eq!(align, egui::Align2::RIGHT_TOP);
        assert!(pos.x < 80.0, "placed at {}", pos.x);
    }

    #[test]
    fn marker_order_does_not_move_the_measurements() {
        let b = bounds(0.0, 100.0);
        let forward = stats_anchor(&b, (Some(10.0), Some(20.0)));
        let reversed = stats_anchor(&b, (Some(20.0), Some(10.0)));
        assert_eq!(forward.0.x, reversed.0.x);
        assert_eq!(forward.1, reversed.1);
    }

    #[test]
    fn without_a_span_the_measurements_go_top_left() {
        let b = bounds(0.0, 100.0);
        let (pos, align) = stats_anchor(&b, (None, None));
        assert_eq!(align, egui::Align2::LEFT_TOP);
        assert!(pos.x > 0.0 && pos.x < 5.0, "clear of the axis: {}", pos.x);
        // A single marker has no span, so it reads like no span at all.
        assert_eq!(
            stats_anchor(&b, (Some(50.0), None)).1,
            egui::Align2::LEFT_TOP
        );
    }

    #[test]
    fn every_reported_field_is_distinct_and_named() {
        let names: Vec<&str> = FIELDS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["avg", "std", "rms", "min", "max", "p2p"]);
        assert!(names.iter().all(|n| !n.is_empty()));
    }

    #[test]
    fn the_reported_fields_read_the_values_they_name() {
        let s = RangeStats {
            mean: 2.0,
            std: 0.5,
            rms: 2.25,
            min: 1.0,
            max: 5.0,
            buckets: 10,
        };
        let by = |want: &str| {
            FIELDS
                .iter()
                .find(|(n, _)| *n == want)
                .map(|(_, f)| f(&s))
                .unwrap()
        };
        assert_eq!(by("avg"), 2.0);
        assert_eq!(by("std"), 0.5);
        assert_eq!(by("rms"), 2.25);
        assert_eq!(by("min"), 1.0);
        assert_eq!(by("max"), 5.0);
        // Peak to peak is the span, not either end of it.
        assert_eq!(by("p2p"), 4.0);
    }

    #[test]
    fn a_marker_goes_under_the_pointer_when_it_is_in_view() {
        assert_eq!(marker_default(Some(3.0), Some((0.0, 10.0))), Some(3.0));
    }

    #[test]
    fn a_marker_falls_back_to_the_middle_of_what_is_drawn() {
        // This is the case that used to place nothing at all, because it
        // read a range that is only set once the view has been panned.
        assert_eq!(marker_default(None, Some((0.0, 10.0))), Some(5.0));
    }

    #[test]
    fn a_remembered_pointer_outside_the_view_is_ignored() {
        // The capture scrolled on; placing a marker back there would put it
        // somewhere nobody can see.
        assert_eq!(marker_default(Some(-5.0), Some((0.0, 10.0))), Some(5.0));
        assert_eq!(marker_default(Some(99.0), Some((0.0, 10.0))), Some(5.0));
    }

    #[test]
    fn nothing_drawn_means_nowhere_to_place_a_marker() {
        assert_eq!(marker_default(Some(3.0), None), None);
        assert_eq!(marker_default(None, None), None);
    }

    #[test]
    fn no_gpi_switch_selects_nothing() {
        assert_eq!(gpi_selection(None), [false; 5]);
        assert_eq!(gpi_selection(Some(&[])), [false; 5]);
    }

    #[test]
    fn a_gpi_switch_selects_the_channels_it_names() {
        // GPI_CHANNELS is [0, 1, 2, 3, 7].
        assert_eq!(
            gpi_selection(Some(&[0, 7])),
            [true, false, false, false, true]
        );
    }

    #[test]
    fn a_channel_the_instrument_lacks_is_ignored_not_fatal() {
        // From a command line a typo should cost that one line, not the run.
        assert_eq!(
            gpi_selection(Some(&[4, 5, 1])),
            [false, true, false, false, false]
        );
    }

    #[test]
    fn every_gpi_channel_has_a_display_name() {
        for &c in &js220::proto::stream::GPI_CHANNELS {
            assert!(!gpi_name(c).is_empty(), "channel {c}");
        }
        assert_eq!(
            gpi_name(7),
            "IN 7",
            "the trigger input is named like the rest"
        );
    }

    #[test]
    fn a_logic_level_reads_from_the_midpoint() {
        assert_eq!(logic_level(0.0), "low");
        assert_eq!(logic_level(0.49), "low");
        assert_eq!(logic_level(0.5), "high");
        assert_eq!(logic_level(1.0), "high");
    }

    #[test]
    fn a_logic_summary_of_nothing_is_nothing() {
        assert!(logic_summary(&[], (0.0, 10.0)).is_none());
        // Every bucket outside the span counts as nothing too.
        let b = logic(&[(0.0, 0.0)]);
        assert!(logic_summary(&b, (100.0, 200.0)).is_none());
    }

    #[test]
    fn a_flat_low_line_reports_no_edges() {
        let b = logic(&[(0.0, 0.0), (0.0, 0.0), (0.0, 0.0)]);
        let s = logic_summary(&b, (0.0, 10.0)).unwrap();
        assert!(s.contains("0 edge"), "got {s}");
        assert!(s.contains("0% high"), "got {s}");
    }

    #[test]
    fn a_flat_high_line_is_high_all_of_the_time() {
        let b = logic(&[(1.0, 1.0), (1.0, 1.0)]);
        let s = logic_summary(&b, (0.0, 10.0)).unwrap();
        assert!(s.contains("100% high"), "got {s}");
    }

    #[test]
    fn a_level_change_between_buckets_is_an_edge() {
        let b = logic(&[(0.0, 0.0), (1.0, 1.0)]);
        let s = logic_summary(&b, (0.0, 10.0)).unwrap();
        assert!(s.contains("1 edge"), "got {s}");
        assert!(s.contains("50% high"), "got {s}");
    }

    #[test]
    fn a_bucket_holding_both_levels_covered_an_edge() {
        // Zoomed out, a whole pulse can land inside one bucket. Counting only
        // changes between buckets would report a toggling line as flat.
        let b = logic(&[(0.0, 1.0)]);
        let s = logic_summary(&b, (0.0, 10.0)).unwrap();
        assert!(s.contains("1 edge"), "got {s}");
    }

    #[test]
    fn the_summary_follows_the_marker_span_not_the_whole_trace() {
        let b = logic(&[(0.0, 0.0), (1.0, 1.0), (1.0, 1.0), (0.0, 0.0)]);
        // Buckets 1 and 2 only: both high, no edge between them.
        let s = logic_summary(&b, (1.0, 2.0)).unwrap();
        assert!(s.contains("100% high"), "got {s}");
        assert!(s.contains("0 edge"), "got {s}");
    }

    #[test]
    fn a_hole_in_a_logic_trace_is_not_counted_as_a_level() {
        let mut b = logic(&[(0.0, 0.0), (0.0, 0.0)]);
        b.push(Bucket {
            t: 2.0,
            min: f32::NAN,
            max: f32::NAN,
            mean: f32::NAN,
        });
        let s = logic_summary(&b, (0.0, 10.0)).unwrap();
        assert!(s.contains("0 edge"), "a gap is not an edge: {s}");
    }

    #[test]
    fn the_logic_y_range_leaves_room_around_both_levels() {
        assert!(LOGIC_Y.0 < 0.0, "low must not sit on the frame");
        assert!(LOGIC_Y.1 > 1.0, "high must not sit on the frame");
    }

    #[test]
    fn the_default_record_path_has_the_binary_extension() {
        assert!(default_record_path().ends_with(".f32"));
    }
}
