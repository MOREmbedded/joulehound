//! The egui application.

use std::time::Duration;

use eframe::egui;
use egui_plot::{AxisHints, Legend, Line, Plot, PlotBounds, PlotPoints, Polygon, VLine};

use crate::acquire::{Acquisition, Command, LogLevel, Shared, Status};
use crate::buffer::SignalTrace;
use crate::fmt;

/// Sample rates offered in the toolbar. Every one divides 2 MHz exactly.
const RATES: [u32; 7] = [1_000, 10_000, 50_000, 100_000, 200_000, 500_000, 1_000_000];

/// Time spans offered for the live view, in seconds.
const WINDOWS: [f64; 6] = [0.1, 0.5, 1.0, 2.0, 5.0, 10.0];

/// One plotted signal.
struct SignalView {
    name: &'static str,
    unit: &'static str,
    colour: egui::Color32,
}

const VIEWS: [SignalView; 3] = [
    SignalView {
        name: "Current",
        unit: "A",
        colour: egui::Color32::from_rgb(255, 170, 60),
    },
    SignalView {
        name: "Voltage",
        unit: "V",
        colour: egui::Color32::from_rgb(90, 190, 255),
    },
    SignalView {
        name: "Power",
        unit: "W",
        colour: egui::Color32::from_rgb(130, 230, 130),
    },
];

/// Which marker the next plot click places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerTool {
    /// Clicks pan the plot rather than placing markers.
    Off,
    /// Place marker A.
    A,
    /// Place marker B.
    B,
}

/// Colour of marker A.
const MARKER_A: egui::Color32 = egui::Color32::from_rgb(255, 235, 120);
/// Colour of marker B.
const MARKER_B: egui::Color32 = egui::Color32::from_rgb(255, 130, 200);

/// Application state.
pub struct App {
    acq: Acquisition,
    serial: Option<String>,
    devices: Vec<String>,
    rate: u32,
    statistics: bool,
    record_path: String,
    show_log: bool,
    /// Per-plot: keep the view pinned to the newest data and auto-scale Y.
    /// Cleared as soon as the user pans or zooms that plot.
    follow: [bool; VIEWS.len()],
    /// Seconds of history shown while following.
    window_seconds: f64,
    /// Marker positions in seconds, shared by every plot so they stay aligned.
    marker_a: Option<f64>,
    marker_b: Option<f64>,
    tool: MarkerTool,
}

impl App {
    /// Build the application and start the acquisition thread.
    ///
    /// `autostart` begins streaming immediately if a device is present, which
    /// is convenient for launching straight into a measurement.
    pub fn new(cc: &eframe::CreationContext<'_>, autostart: bool) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            acq: Acquisition::spawn(crate::acquire::BUCKETS),
            serial: None,
            devices: Vec::new(),
            rate: 1_000_000,
            statistics: true,
            record_path: default_record_path(),
            show_log: true,
            follow: [true; VIEWS.len()],
            window_seconds: 2.0,
            marker_a: None,
            marker_b: None,
            tool: MarkerTool::Off,
        };
        app.rescan();
        if autostart && app.serial.is_some() {
            app.acq.send(Command::Start {
                serial: app.serial.clone(),
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
        // Acquisition runs independently, so redraw steadily rather than only
        // on input. 30 Hz is smooth and leaves the CPU to the USB thread.
        ctx.request_repaint_after(Duration::from_millis(33));

        let snapshot = self.acq.shared().clone();
        let shared = snapshot.lock().expect("shared snapshot poisoned");

        self.toolbar(ctx, &shared);
        self.status_bar(ctx, &shared);
        if self.show_log {
            self.log_panel(ctx, &shared);
        }
        self.measurements(ctx, &shared);
        self.plots(ctx, &shared);
    }
}

impl App {
    fn toolbar(&mut self, ctx: &egui::Context, shared: &Shared) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.heading("JS220");
                ui.separator();

                let running = self.running(shared);
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("device")
                        .width(110.0)
                        .selected_text(self.serial.clone().unwrap_or_else(|| "no device".into()))
                        .show_ui(ui, |ui| {
                            for d in &self.devices {
                                ui.selectable_value(&mut self.serial, Some(d.clone()), d);
                            }
                        });
                    if ui.button("Rescan").clicked() {
                        self.rescan();
                    }
                    egui::ComboBox::from_id_salt("rate")
                        .width(90.0)
                        .selected_text(fmt::rate(self.rate))
                        .show_ui(ui, |ui| {
                            for r in RATES {
                                ui.selectable_value(&mut self.rate, r, fmt::rate(r));
                            }
                        });
                    ui.checkbox(&mut self.statistics, "Stats");
                });

                ui.separator();
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
                        rate: self.rate,
                        statistics: self.statistics,
                    });
                }
                if ui.button("Clear").clicked() {
                    self.acq.send(Command::ClearTraces);
                }
                ui.separator();
                ui.label("Window:");
                egui::ComboBox::from_id_salt("window")
                    .width(80.0)
                    .selected_text(fmt::seconds(self.window_seconds))
                    .show_ui(ui, |ui| {
                        for w in WINDOWS {
                            ui.selectable_value(&mut self.window_seconds, w, fmt::seconds(w));
                        }
                    });
                if ui
                    .button("Auto-fit")
                    .on_hover_text("Resume follow and auto-scale on every plot")
                    .clicked()
                {
                    self.follow = [true; VIEWS.len()];
                }

                ui.separator();
                ui.label("Markers:");
                // Placing from the toolbar means a marker can always be
                // created without hitting a precise spot in a plot first;
                // the arm toggles then reposition it by clicking.
                let span = shared.current.time_span();
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Set A").color(MARKER_A),
                    ))
                    .on_hover_text("Place A in the visible window")
                    .clicked()
                {
                    self.marker_a = default_marker(span, self.window_seconds, 2.0 / 3.0);
                }
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Set B").color(MARKER_B),
                    ))
                    .on_hover_text("Place B in the visible window")
                    .clicked()
                {
                    self.marker_b = default_marker(span, self.window_seconds, 1.0 / 3.0);
                }
                marker_toggle(ui, &mut self.tool, MarkerTool::A, "↔A", MARKER_A);
                marker_toggle(ui, &mut self.tool, MarkerTool::B, "↔B", MARKER_B);
                if ui
                    .button("Clear ✖")
                    .on_hover_text("Remove both markers")
                    .clicked()
                {
                    self.marker_a = None;
                    self.marker_b = None;
                    self.tool = MarkerTool::Off;
                }

                ui.separator();
                ui.add(
                    egui::TextEdit::singleline(&mut self.record_path)
                        .desired_width(180.0)
                        .hint_text("capture.f32"),
                );
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
                    Status::Failed(e) => {
                        (format!("error: {e}"), egui::Color32::from_rgb(240, 120, 120))
                    }
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
                if shared.filled > 0 || shared.frame_errors > 0 || shared.frame_breaks > 0 {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 200, 90),
                        format!(
                            "{} filled · {} bad frames · {} breaks",
                            shared.filled, shared.frame_errors, shared.frame_breaks
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

    fn measurements(&mut self, ctx: &egui::Context, shared: &Shared) {
        egui::SidePanel::right("measurements")
            .default_width(380.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.marker_readout(ui, shared);
                    ui.add_space(8.0);
                    ui.separator();
                    ui.heading("Instrument statistics");
                    ui.separator();
                    match &shared.statistics {
                        Some(stats) => {
                            egui::Grid::new("stats").num_columns(5).striped(true).show(
                                ui,
                                |ui| {
                                    ui.label("");
                                    ui.strong("mean");
                                    ui.strong("std");
                                    ui.strong("min");
                                    ui.strong("max");
                                    ui.end_row();
                                    for (name, unit, s) in [
                                        ("Current", "A", stats.current),
                                        ("Voltage", "V", stats.voltage),
                                        ("Power", "W", stats.power),
                                    ] {
                                        ui.strong(name);
                                        ui.label(fmt::si(s.avg, unit));
                                        ui.label(fmt::si(s.std, unit));
                                        ui.label(fmt::si(s.min, unit));
                                        ui.label(fmt::si(s.max, unit));
                                        ui.end_row();
                                    }
                                },
                            );
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.strong("Charge");
                                ui.label(fmt::si(stats.charge, "C"));
                                ui.separator();
                                ui.strong("Energy");
                                ui.label(fmt::si(stats.energy, "J"));
                            });
                            ui.add_space(4.0);
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 170, 80),
                                "Power statistics unverified: the instrument reports a \
                                 static minimum and zero spread with the power port off.",
                            );
                        }
                        None if self.statistics => {
                            ui.weak("waiting for the first statistics block…");
                        }
                        None => {
                            ui.weak("statistics disabled");
                        }
                    }
                });
            });
    }

    /// The marker panel: values at A and B, and what lies between them.
    fn marker_readout(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        ui.add_space(6.0);
        ui.heading("Markers");
        ui.separator();

        let traces = [&shared.current, &shared.voltage, &shared.power];

        if self.marker_a.is_none() && self.marker_b.is_none() {
            ui.weak("Pick A or B in the toolbar, then click a plot.");
            ui.weak("Markers line up across all three signals.");
            return;
        }

        ui.horizontal(|ui| {
            if let Some(a) = self.marker_a {
                ui.colored_label(MARKER_A, format!("A  {a:.6} s"));
            }
            if let Some(b) = self.marker_b {
                ui.colored_label(MARKER_B, format!("B  {b:.6} s"));
            }
        });

        if let (Some(a), Some(b)) = (self.marker_a, self.marker_b) {
            let dt = (b - a).abs();
            ui.horizontal(|ui| {
                ui.strong("Δt");
                ui.label(fmt::si(dt, "s"));
                if dt > 0.0 {
                    ui.separator();
                    ui.strong("1/Δt");
                    ui.label(fmt::si(1.0 / dt, "Hz"));
                }
            });
        }
        ui.add_space(4.0);

        egui::Grid::new("markers")
            .num_columns(5)
            .striped(true)
            .show(ui, |ui| {
                ui.label("");
                ui.strong("at A");
                ui.strong("at B");
                ui.strong("Δ B−A");
                ui.strong("mean A→B");
                ui.end_row();

                for (view, trace) in VIEWS.iter().zip(traces) {
                    ui.colored_label(view.colour, view.name);

                    let va = self.marker_a.and_then(|t| trace.value_at(t));
                    let vb = self.marker_b.and_then(|t| trace.value_at(t));
                    label_or_dash(ui, va.map(|b| b.mean as f64), view.unit);
                    label_or_dash(ui, vb.map(|b| b.mean as f64), view.unit);
                    label_or_dash(
                        ui,
                        match (va, vb) {
                            (Some(a), Some(b)) => Some((b.mean - a.mean) as f64),
                            _ => None,
                        },
                        view.unit,
                    );
                    let span = match (self.marker_a, self.marker_b) {
                        (Some(a), Some(b)) => trace.range_stats(a, b),
                        _ => None,
                    };
                    label_or_dash(ui, span.map(|s| s.mean as f64), view.unit);
                    ui.end_row();
                }
            });

        let (Some(a), Some(b)) = (self.marker_a, self.marker_b) else {
            return;
        };

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Between markers").strong());
        egui::Grid::new("between")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                ui.label("");
                ui.strong("min");
                ui.strong("max");
                ui.strong("peak-peak");
                ui.end_row();
                for (view, trace) in VIEWS.iter().zip(traces) {
                    ui.colored_label(view.colour, view.name);
                    match trace.range_stats(a, b) {
                        Some(s) => {
                            ui.label(fmt::si(s.min as f64, view.unit));
                            ui.label(fmt::si(s.max as f64, view.unit));
                            ui.label(fmt::si((s.max - s.min) as f64, view.unit));
                        }
                        None => {
                            ui.weak("—");
                            ui.weak("—");
                            ui.weak("—");
                        }
                    }
                    ui.end_row();
                }
            });

        // Charge and energy over the span, integrated from the span mean.
        if let (Some(i), Some(p)) = (
            shared.current.range_stats(a, b),
            shared.power.range_stats(a, b),
        ) {
            let dt = (b - a).abs();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.strong("Charge A→B");
                ui.label(fmt::si(i.mean as f64 * dt, "C"));
                ui.separator();
                ui.strong("Energy A→B");
                ui.label(fmt::si(p.mean as f64 * dt, "J"));
            });
        }
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
            // Each plot sits in a group with a header row and margins; without
            // allowing for those the last plot is pushed off the bottom.
            const GROUP_OVERHEAD: f32 = 34.0;
            let available = ui.available_height();
            let height = (available / VIEWS.len() as f32) - GROUP_OVERHEAD;

            let traces = [&shared.current, &shared.voltage, &shared.power];
            for (i, (view, trace)) in VIEWS.iter().zip(traces).enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(view.colour, egui::RichText::new(view.name).strong());
                        ui.weak(format!("({})", view.unit));
                        if let Some((first, last)) = trace.time_span() {
                            ui.weak(format!("· {first:.3}–{last:.3} s"));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("Fit")
                                .on_hover_text("Scale to the data and resume following")
                                .clicked()
                            {
                                self.follow[i] = true;
                            }
                            ui.toggle_value(&mut self.follow[i], "Follow")
                                .on_hover_text("Keep newest data in view and auto-scale Y");
                        });
                    });

                    let outcome = plot_signal(
                        ui,
                        view,
                        trace,
                        height,
                        self.follow[i],
                        self.window_seconds,
                        self.tool,
                        (self.marker_a, self.marker_b),
                    );
                    // Any pan or zoom means the user wants manual control.
                    if outcome.interacted {
                        self.follow[i] = false;
                    }
                    if let Some(t) = outcome.clicked_at {
                        match self.tool {
                            MarkerTool::A => self.marker_a = Some(t),
                            MarkerTool::B => self.marker_b = Some(t),
                            MarkerTool::Off => {}
                        }
                    }
                });
            }
        });
    }
}

/// Position for a marker placed from the toolbar.
///
/// `back` is how far into the visible window to sit, as a fraction measured
/// back from the newest sample, so A and B land apart and both inside the view.
fn default_marker(span: Option<(f64, f64)>, window_seconds: f64, back: f64) -> Option<f64> {
    let (first, last) = span?;
    let t = last - window_seconds * back;
    Some(t.clamp(first, last))
}

fn marker_toggle(
    ui: &mut egui::Ui,
    tool: &mut MarkerTool,
    which: MarkerTool,
    label: &str,
    colour: egui::Color32,
) {
    let selected = *tool == which;
    let text = egui::RichText::new(label).color(colour).strong();
    if ui
        .selectable_label(selected, text)
        .on_hover_text("Click or drag in a plot to place this marker")
        .clicked()
    {
        // Clicking the active tool turns it off, so dragging pans again.
        *tool = if selected { MarkerTool::Off } else { which };
    }
}

fn label_or_dash(ui: &mut egui::Ui, value: Option<f64>, unit: &str) {
    match value {
        Some(v) => {
            ui.label(fmt::si(v, unit));
        }
        None => {
            ui.weak("—");
        }
    }
}

/// What the user did to a plot this frame.
struct PlotOutcome {
    /// The user panned or zoomed, so following should stop.
    interacted: bool,
    /// The user positioned a marker at this time.
    clicked_at: Option<f64>,
}

/// Draw one signal as a min/max envelope with the mean overlaid.
///
/// Panning and zooming are always available on both axes. While `follow` is
/// set the view is driven to the newest data each frame; the caller clears
/// `follow` as soon as the user takes over, so the instrument never fights the
/// hand that is scaling it.
#[allow(clippy::too_many_arguments)]
fn plot_signal(
    ui: &mut egui::Ui,
    view: &SignalView,
    trace: &SignalTrace,
    height: f32,
    follow: bool,
    window_seconds: f64,
    tool: MarkerTool,
    markers: (Option<f64>, Option<f64>),
) -> PlotOutcome {
    let placing = tool != MarkerTool::Off;
    let unit = view.unit;

    let plot = Plot::new(view.name)
        .height(height.max(80.0))
        .legend(Legend::default())
        // Scaling is always the user's to take; `follow` only decides whether
        // the view is driven while they are not holding it. While a marker
        // tool is active, dragging positions the marker instead of panning.
        .allow_zoom(true)
        .allow_scroll(true)
        .allow_boxed_zoom(!placing)
        .allow_drag(!placing)
        .auto_bounds(egui::Vec2b::FALSE)
        .x_axis_label("seconds")
        // No axis label: the rotated text collides with the tick values, and
        // the plot header already names the unit.
        .custom_y_axes(vec![AxisHints::new_y()
            .formatter(move |mark, _range| fmt::si_tick(mark.value, unit))])
        .label_formatter(move |name, value| {
            if name.is_empty() {
                format!("{:.6} s\n{}", value.x, fmt::si(value.y, unit))
            } else {
                format!("{name}\n{:.6} s\n{}", value.x, fmt::si(value.y, unit))
            }
        });

    let response = plot.show(ui, |plot_ui| {
        if !trace.is_empty() {
            // The envelope is drawn as one polygon: forward along the maxima,
            // back along the minima. This keeps a single-sample spike visible
            // even when a bucket covers thousands of samples.
            let mut upper: Vec<[f64; 2]> = Vec::with_capacity(trace.len());
            let mut lower: Vec<[f64; 2]> = Vec::with_capacity(trace.len());
            let mut mean: Vec<[f64; 2]> = Vec::with_capacity(trace.len());
            for b in trace.iter() {
                if b.is_empty() {
                    continue;
                }
                upper.push([b.t, b.max as f64]);
                lower.push([b.t, b.min as f64]);
                mean.push([b.t, b.mean as f64]);
            }
            if !mean.is_empty() {
                if follow {
                    if let Some(bounds) = follow_bounds(&upper, &lower, window_seconds) {
                        plot_ui.set_plot_bounds(bounds);
                    }
                }
                let mut envelope = upper.clone();
                envelope.extend(lower.iter().rev().copied());
                plot_ui.polygon(
                    Polygon::new(PlotPoints::from(envelope))
                        .name("min…max")
                        .fill_color(view.colour.gamma_multiply(0.18))
                        .stroke(egui::Stroke::NONE),
                );
                plot_ui.line(
                    Line::new(PlotPoints::from(mean))
                        .name("mean")
                        .color(egui::Color32::WHITE)
                        .width(1.0_f32),
                );
            }
        }

        // Markers are drawn in every plot at the same time position, which is
        // what makes them useful: one click lines up all three signals.
        if let Some(a) = markers.0 {
            plot_ui.vline(VLine::new(a).name("A").color(MARKER_A).width(1.5_f32));
        }
        if let Some(b) = markers.1 {
            plot_ui.vline(VLine::new(b).name("B").color(MARKER_B).width(1.5_f32));
        }
    });

    let r = &response.response;
    // Only a real wheel event counts as the user taking over. `smooth_scroll`
    // keeps decaying after a scroll and `zoom_delta` is not exactly 1.0 after a
    // window resize, so either of those would silently cancel follow and leave
    // the plot stranded on stale bounds.
    let scrolled = r.hovered() && ui.input(|i| i.raw_scroll_delta.y.abs() > 0.0);
    let interacted = (r.dragged() && !placing) || scrolled;

    // While a marker tool is active, clicking or dragging positions it.
    let clicked_at = if placing && (r.clicked() || r.dragged()) {
        r.interact_pointer_pos()
            .map(|p| response.transform.value_from_position(p).x)
    } else {
        None
    };

    PlotOutcome {
        interacted,
        clicked_at,
    }
}

/// Bounds showing the last `window_seconds` of data, with the Y axis scaled to
/// what is actually visible in that span.
///
/// Scaling Y to the whole buffer rather than the visible span would let a
/// spike scrolled off the left keep the trace squashed long after it is gone.
fn follow_bounds(upper: &[[f64; 2]], lower: &[[f64; 2]], window_seconds: f64) -> Option<PlotBounds> {
    let last_t = upper.last()?[0];
    let first_t = (last_t - window_seconds).max(upper.first()?[0]);
    // A window with no width would make the plot degenerate.
    let x_max = last_t.max(first_t + 1e-6);

    let mut y_min = f64::INFINITY;
    let mut y_max = f64::NEG_INFINITY;
    for (hi, lo) in upper.iter().zip(lower) {
        if hi[0] < first_t {
            continue;
        }
        y_max = y_max.max(hi[1]);
        y_min = y_min.min(lo[1]);
    }
    if !y_min.is_finite() || !y_max.is_finite() {
        return None;
    }

    // Pad so the trace does not touch the frame, and give a flat signal a
    // visible band instead of a zero-height axis.
    let span = (y_max - y_min).abs();
    let pad = if span > 0.0 {
        span * 0.08
    } else {
        y_max.abs().max(1e-9) * 0.1
    };
    Some(PlotBounds::from_min_max(
        [first_t, y_min - pad],
        [x_max, y_max + pad],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_rate_divides_the_base_rate() {
        for r in RATES {
            assert_eq!(js220::BASE_RATE % r, 0, "{r} Hz does not divide the base rate");
            assert!((js220::MIN_SAMPLE_RATE..=js220::MAX_SAMPLE_RATE).contains(&r));
        }
    }

    #[test]
    fn rates_are_offered_in_increasing_order() {
        assert!(RATES.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn every_offered_window_is_positive_and_ordered() {
        assert!(WINDOWS.iter().all(|w| *w > 0.0));
        assert!(WINDOWS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn there_is_one_view_per_signal() {
        assert_eq!(VIEWS.len(), js220::Signal::ALL.len());
    }

    #[test]
    fn view_and_marker_colours_are_distinct() {
        for (i, a) in VIEWS.iter().enumerate() {
            for b in VIEWS.iter().skip(i + 1) {
                assert_ne!(a.colour, b.colour, "{} and {} share a colour", a.name, b.name);
            }
        }
        assert_ne!(MARKER_A, MARKER_B);
    }

    #[test]
    fn follow_bounds_track_the_window_and_visible_extremes() {
        let upper: Vec<[f64; 2]> = (0..100).map(|i| [i as f64 * 0.1, 1.0]).collect();
        let lower: Vec<[f64; 2]> = (0..100).map(|i| [i as f64 * 0.1, -1.0]).collect();
        let b = follow_bounds(&upper, &lower, 2.0).unwrap();
        // The last sample is at t = 9.9, so a 2 s window starts at 7.9.
        assert!((b.min()[0] - 7.9).abs() < 1e-9, "x min {}", b.min()[0]);
        assert!((b.max()[0] - 9.9).abs() < 1e-9, "x max {}", b.max()[0]);
        assert!(b.min()[1] < -1.0 && b.max()[1] > 1.0, "y must be padded");
    }

    #[test]
    fn follow_bounds_ignore_extremes_outside_the_window() {
        // A spike early on must not keep squashing the trace once it has
        // scrolled out of view.
        let mut upper: Vec<[f64; 2]> = (0..100).map(|i| [i as f64 * 0.1, 1.0]).collect();
        upper[0][1] = 1000.0;
        let lower: Vec<[f64; 2]> = (0..100).map(|i| [i as f64 * 0.1, -1.0]).collect();
        let b = follow_bounds(&upper, &lower, 1.0).unwrap();
        assert!(
            b.max()[1] < 2.0,
            "y max {} still includes the spike",
            b.max()[1]
        );
    }

    #[test]
    fn follow_bounds_give_a_flat_signal_a_visible_band() {
        let upper: Vec<[f64; 2]> = (0..10).map(|i| [i as f64, 5.0]).collect();
        let lower = upper.clone();
        let b = follow_bounds(&upper, &lower, 10.0).unwrap();
        assert!(b.max()[1] > b.min()[1], "a flat signal needs a non-zero span");
    }

    #[test]
    fn follow_bounds_need_data() {
        assert!(follow_bounds(&[], &[], 1.0).is_none());
    }

    #[test]
    fn toolbar_markers_land_inside_the_visible_window() {
        let span = Some((0.0, 10.0));
        let a = default_marker(span, 2.0, 2.0 / 3.0).unwrap();
        let b = default_marker(span, 2.0, 1.0 / 3.0).unwrap();
        assert!(a < b, "A should sit to the left of B");
        assert!(a >= 8.0 && b <= 10.0, "both inside the last 2 s: {a}, {b}");
    }

    #[test]
    fn toolbar_markers_clamp_to_the_available_data() {
        // A window wider than the captured data must not place a marker
        // before the trace starts.
        let a = default_marker(Some((4.0, 5.0)), 60.0, 2.0 / 3.0).unwrap();
        assert!((4.0..=5.0).contains(&a), "clamped to data, got {a}");
    }

    #[test]
    fn toolbar_markers_need_data() {
        assert!(default_marker(None, 1.0, 0.5).is_none());
    }

    #[test]
    fn the_default_record_path_has_the_binary_extension() {
        assert!(default_record_path().ends_with(".f32"));
    }
}
