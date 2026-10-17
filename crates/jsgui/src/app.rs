//! The egui application.

use std::time::Duration;

use eframe::egui;
use egui_plot::{
    AxisHints, HLine, Legend, Line, MarkerShape, Plot, PlotBounds, PlotPoint, PlotPoints, Points,
    Polygon, Text, VLine,
};

use crate::acquire::{Acquisition, Command, LogLevel, Shared, Status};
use crate::buffer::{Bucket, RangeStats, SignalTrace};
use crate::fmt;
use crate::peaks::{self, Edge};
use crate::raw::RawTrace;
use js220::device::Source;

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
    /// `autostart` begins streaming immediately if a device is present.
    pub fn new(cc: &eframe::CreationContext<'_>, autostart: bool) -> Self {
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
            marker_a: None,
            marker_b: None,
            dragging: None,
            drag_last_x: 0.0,
            buckets: Vec::new(),
            show_peaks: false,
            peak_count: 5,
            threshold: [0.0; VIEWS.len()],
            threshold_on: [false; VIEWS.len()],
            threshold_edge: Edge::Above,
        };
        app.rescan();
        if autostart && app.serial.is_some() {
            app.acq.send(Command::Start {
                serial: app.serial.clone(),
                source: source_for(app.rate),
                rate: app.rate,
                statistics: app.statistics,
            });
        }
        app
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

    /// Place both markers around the middle of the current view.
    fn place_markers(&mut self) {
        let Some((lo, hi)) = self.view_x else { return };
        let span = hi - lo;
        self.marker_a = Some(lo + span * 0.35);
        self.marker_b = Some(lo + span * 0.65);
    }
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
                                        "Uncalibrated ADC counts: no gain, offset or                                          range correction applied"
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
                } else if ui
                    .add_enabled(self.serial.is_some(), egui::Button::new("▶ Start"))
                    .clicked()
                {
                    self.acq.send(Command::Start {
                        serial: self.serial.clone(),
                        source: source_for(self.rate),
                        rate: self.rate,
                        statistics: self.statistics,
                    });
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
                if ui
                    .button("A|B")
                    .on_hover_text("Place a pair of markers; drag them in the plot")
                    .clicked()
                {
                    self.place_markers();
                }
                if ui.button("A").on_hover_text("Place a single marker").clicked() {
                    if let Some((lo, hi)) = self.view_x {
                        self.marker_a = Some(lo + (hi - lo) * 0.5);
                        self.marker_b = None;
                    }
                }
                if ui.button("✖").on_hover_text("Remove the markers").clicked() {
                    self.marker_a = None;
                    self.marker_b = None;
                }

                ui.separator();
                if ui
                    .button("Clear")
                    .on_hover_text("Discard the captured history")
                    .clicked()
                {
                    self.acq.send(Command::ClearTraces);
                }

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
                    .add_enabled(running, button)
                    .on_hover_text(
                        "Open or close the current path. Switching off removes power                          from the target and will reset it.",
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
            let height = (ui.available_height() / shown.len() as f32) - HEADER;
            let columns = (ui.available_width() as usize).clamp(64, 4096);

            let coarse = [&shared.current, &shared.voltage, &shared.power];
            let raw = [&shared.raw_current, &shared.raw_voltage, &shared.raw_power];

            // Every plot shares one x range so the signals stay aligned, which
            // is what makes a marker meaningful across all three.
            let x_range = self.resolve_x_range(raw[I], coarse[I]);

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
                let outcome = plot_signal(
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
                );

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
                // Still overlapping the retained data: honour it.
                if hi >= first && lo <= last {
                    return (lo, hi);
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
) -> PlotOutcome {
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
        .custom_y_axes(vec![
            AxisHints::new_y().formatter(move |mark, _range| fmt::si_tick(mark.value, unit))
        ])
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

        let (stats_span, label) = match markers {
            (Some(a), Some(b)) => (Some((a, b)), format!("Δt {}", fmt::si((b - a).abs(), "s"))),
            (Some(a), None) => (Some((x.0, a)), "view → A".to_string()),
            _ => (Some((x.0, x.1)), "view".to_string()),
        };
        if let Some((a, b)) = stats_span {
            if let Some(stats) = span_stats(raw, coarse, a, b) {
                let text = stats_block(&label, &stats, view, (b - a).abs());
                // Inset far enough to clear the Y axis labels, which egui
                // draws inside the plot area.
                let pos = PlotPoint::new(
                    bounds.min()[0] + (bounds.max()[0] - bounds.min()[0]) * 0.055,
                    bounds.max()[1],
                );
                plot_ui.text(
                    Text::new(pos, text)
                        .anchor(egui::Align2::LEFT_TOP)
                        .color(egui::Color32::from_gray(205)),
                );
            }
        }
    });

    interact(ui, &response, x, y, markers, dragging)
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
    };

    let plot_x = |pos: egui::Pos2| response.transform.value_from_position(pos).x;

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
    let mut text = format!("{label}\n");
    for (name, value) in [
        ("avg", s.mean as f64),
        ("std", s.std as f64),
        ("rms", s.rms as f64),
        ("min", s.min as f64),
        ("max", s.max as f64),
        ("p2p", s.p2p() as f64),
    ] {
        text.push_str(&format!("{name} {}\n", fmt::si(value, view.unit)));
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
    fn the_default_record_path_has_the_binary_extension() {
        assert!(default_record_path().ends_with(".f32"));
    }
}
