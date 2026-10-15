//! The egui application.

use std::time::Duration;

use eframe::egui;
use egui_plot::{
    AxisHints, Legend, Line, Plot, PlotBounds, PlotPoint, PlotPoints, Polygon, Text, VLine,
};

use crate::acquire::{Acquisition, Command, LogLevel, Shared, Status};
use crate::buffer::{Bucket, RangeStats, SignalTrace};
use crate::fmt;
use crate::raw::RawTrace;

/// Sample rates offered in the toolbar. Every one divides 2 MHz exactly.
const RATES: [u32; 7] = [1_000, 10_000, 50_000, 100_000, 200_000, 500_000, 1_000_000];

/// Time spans offered for the live view, in seconds.
const WINDOWS: [f64; 8] = [0.001, 0.01, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0];

/// Index of the current signal, which drives the shared time axis.
const I: usize = 0;

/// One plotted signal.
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
    /// Per-plot: keep the newest data in view and auto-scale Y.
    follow: [bool; VIEWS.len()],
    /// Which signals are plotted at all.
    visible: [bool; VIEWS.len()],
    /// Seconds of history shown while following.
    window_seconds: f64,
    /// The x range each plot is showing, carried between frames so the data
    /// can be aggregated to the view before it is drawn.
    view_x: Option<(f64, f64)>,
    marker_a: Option<f64>,
    marker_b: Option<f64>,
    /// Which marker a drag is currently moving.
    dragging: Option<Marker>,
    /// Scratch for aggregated buckets, reused every frame.
    buckets: Vec<Bucket>,
}

/// Identifies one of the two markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    A,
    B,
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
            visible: [true; VIEWS.len()],
            window_seconds: 1.0,
            view_x: None,
            marker_a: None,
            marker_b: None,
            dragging: None,
            buckets: Vec::new(),
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
                    egui::ComboBox::from_id_salt("rate")
                        .width(84.0)
                        .selected_text(fmt::rate(self.rate))
                        .show_ui(ui, |ui| {
                            for r in RATES {
                                ui.selectable_value(&mut self.rate, r, fmt::rate(r));
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
                        rate: self.rate,
                        statistics: self.statistics,
                    });
                }

                ui.separator();
                ui.label("Signals:");
                for (i, view) in VIEWS.iter().enumerate() {
                    let label = egui::RichText::new(&view.unit[..1]).color(view.colour).strong();
                    ui.toggle_value(&mut self.visible[i], label)
                        .on_hover_text(view.name);
                }

                ui.separator();
                ui.label("X:");
                egui::ComboBox::from_id_salt("window")
                    .width(76.0)
                    .selected_text(fmt::seconds(self.window_seconds))
                    .show_ui(ui, |ui| {
                        for w in WINDOWS {
                            ui.selectable_value(&mut self.window_seconds, w, fmt::seconds(w));
                        }
                    });
                if ui.button("🔍−").on_hover_text("Show more time").clicked() {
                    self.window_seconds = (self.window_seconds * 2.0).min(60.0);
                    self.follow = [true; VIEWS.len()];
                }
                if ui.button("🔍+").on_hover_text("Show less time").clicked() {
                    self.window_seconds = (self.window_seconds / 2.0).max(1e-4);
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
            let shown: Vec<usize> = (0..VIEWS.len()).filter(|i| self.visible[*i]).collect();
            if shown.is_empty() {
                ui.centered_and_justified(|ui| ui.weak("No signals selected"));
                return;
            }
            const HEADER: f32 = 26.0;
            let height = (ui.available_height() / shown.len() as f32) - HEADER;
            let columns = (ui.available_width() as usize).clamp(64, 4096);

            let coarse = [&shared.current, &shared.voltage, &shared.power];
            let raw = [
                &shared.raw_current,
                &shared.raw_voltage,
                &shared.raw_power,
            ];

            // Every plot shares one x range so the signals stay aligned, which
            // is what makes a marker meaningful across all three.
            let x_range = self.resolve_x_range(raw[I], coarse[I]);

            for &i in &shown {
                let view = &VIEWS[i];
                ui.horizontal(|ui| {
                    ui.colored_label(view.colour, egui::RichText::new(view.name).strong());
                    ui.weak(format!("({})", view.unit));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Fit").on_hover_text("Resume following").clicked() {
                            self.follow[i] = true;
                        }
                        ui.toggle_value(&mut self.follow[i], "Follow");
                        // Say where the plotted points came from, so a coarse
                        // trace is never mistaken for the real waveform.
                        let (source, detail) = describe_source(raw[i], x_range, columns);
                        ui.weak(detail).on_hover_text(source);
                    });
                });

                aggregate_for_view(raw[i], coarse[i], x_range, columns, &mut self.buckets);
                let outcome = plot_signal(
                    ui,
                    view,
                    &self.buckets,
                    raw[i],
                    coarse[i],
                    height,
                    x_range,
                    (self.marker_a, self.marker_b),
                    self.dragging,
                );

                if outcome.interacted {
                    self.follow = [false; VIEWS.len()];
                }
                if let Some(bounds) = outcome.bounds {
                    self.view_x = Some(bounds);
                }
                if let Some((marker, t)) = outcome.drag {
                    self.dragging = Some(marker);
                    match marker {
                        Marker::A => self.marker_a = Some(t),
                        Marker::B => self.marker_b = Some(t),
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
        let available = raw.time_span().or_else(|| coarse.time_span());
        let Some((first, last)) = available else {
            return self.view_x.unwrap_or((0.0, self.window_seconds));
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
        (last - self.window_seconds, last)
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
                format!("{per_column} samples/px · {} held", fmt::count(raw.len() as f64)),
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
        out.reserve(coarse.len());
        out.extend(coarse.iter().filter(|b| !b.is_empty()));
    }
}

/// Statistics for the span a marker pair encloses, or the whole view.
fn span_stats(raw: &RawTrace, coarse: &SignalTrace, a: f64, b: f64) -> Option<RangeStats> {
    raw.range_stats(a, b).or_else(|| coarse.range_stats(a, b))
}

/// What the user did to a plot this frame.
struct PlotOutcome {
    interacted: bool,
    bounds: Option<(f64, f64)>,
    drag: Option<(Marker, f64)>,
}

/// How close, in screen points, the pointer must be to grab a marker.
const GRAB_RADIUS: f32 = 10.0;

#[allow(clippy::too_many_arguments)]
fn plot_signal(
    ui: &mut egui::Ui,
    view: &SignalView,
    buckets: &[Bucket],
    raw: &RawTrace,
    coarse: &SignalTrace,
    height: f32,
    x: (f64, f64),
    markers: (Option<f64>, Option<f64>),
    dragging: Option<Marker>,
) -> PlotOutcome {
    let unit = view.unit;
    let plot = Plot::new(view.name)
        .height(height.max(70.0))
        .legend(Legend::default())
        .allow_zoom(true)
        .allow_scroll(true)
        .allow_drag(true)
        .allow_boxed_zoom(true)
        .auto_bounds(egui::Vec2b::FALSE)
        .custom_x_axes(vec![AxisHints::new_x()
            .formatter(|mark, _range| fmt::clock(mark.value))])
        .custom_y_axes(vec![AxisHints::new_y()
            .formatter(move |mark, _range| fmt::si_tick(mark.value, unit))])
        .label_formatter(move |name, value| {
            if name.is_empty() {
                format!("{}\n{}", fmt::clock(value.x), fmt::si(value.y, unit))
            } else {
                format!("{name}\n{}\n{}", fmt::clock(value.x), fmt::si(value.y, unit))
            }
        });

    let mut drag = None;

    let response = plot.show(ui, |plot_ui| {
        // Drive the x range; Y is scaled to what is actually in view so a
        // spike that has scrolled away stops squashing the trace.
        let (y_min, y_max) = y_bounds(buckets);
        plot_ui.set_plot_bounds(PlotBounds::from_min_max([x.0, y_min], [x.1, y_max]));

        if !buckets.is_empty() {
            let mut upper: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            let mut lower: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            let mut mean: Vec<[f64; 2]> = Vec::with_capacity(buckets.len());
            for b in buckets {
                upper.push([b.t, b.max as f64]);
                lower.push([b.t, b.min as f64]);
                mean.push([b.t, b.mean as f64]);
            }
            // The envelope only means something when a column covers more
            // than one sample; drawn always, it just thickens the trace.
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

        // Markers: a shaded span between a pair, and the statistics for
        // whatever span is in force, drawn beside it as the instrument's own
        // software does.
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
        for (t, name, colour) in [
            (markers.0, "A", MARKER_A),
            (markers.1, "B", MARKER_B),
        ] {
            let Some(t) = t else { continue };
            plot_ui.vline(VLine::new(t).name(name).color(colour).width(1.4_f32));
            // The instantaneous value under the marker, which is what the
            // line is pointing at.
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
                // draws inside the plot area and would otherwise overlap the
                // first characters of every line.
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

    // Markers are dragged directly: grab the one under the pointer, or keep
    // moving the one already held.
    let r = &response.response;
    if let Some(pos) = r.interact_pointer_pos() {
        if r.dragged() {
            let t = response.transform.value_from_position(pos).x;
            let held = dragging.or_else(|| nearest_marker(&response.transform, markers, pos));
            if let Some(m) = held {
                drag = Some((m, t));
            }
        }
    }

    // Taking over the view needs a deliberate gesture. A bare `dragged()` also
    // fires for synthetic pointer events during a window move or resize, which
    // silently stranded the plot on a range the data had long since left.
    const DRAG_SLOP: f32 = 2.0;
    let panned =
        r.dragged() && drag.is_none() && r.drag_delta().length() > DRAG_SLOP && r.hovered();
    let scrolled = r.hovered() && ui.input(|i| i.raw_scroll_delta.y.abs() > 0.0);
    let bounds = response.transform.bounds();

    PlotOutcome {
        interacted: panned || scrolled,
        bounds: Some((bounds.min()[0], bounds.max()[0])),
        drag,
    }
}

/// The marker within grabbing distance of `pos`, if any.
fn nearest_marker(
    transform: &egui_plot::PlotTransform,
    markers: (Option<f64>, Option<f64>),
    pos: egui::Pos2,
) -> Option<Marker> {
    let mut best: Option<(f32, Marker)> = None;
    for (t, which) in [(markers.0, Marker::A), (markers.1, Marker::B)] {
        let Some(t) = t else { continue };
        let screen_x = transform.position_from_point(&PlotPoint::new(t, 0.0)).x;
        let d = (screen_x - pos.x).abs();
        if d <= GRAB_RADIUS && best.map_or(true, |(bd, _)| d < bd) {
            best = Some((d, which));
        }
    }
    best.map(|(_, m)| m)
}

/// Trace the min/max band as one continuous polyline.
///
/// Filling it as a polygon looks obvious but renders wrongly: egui fills only
/// convex shapes, and an envelope is deeply non-convex, so the fill comes out
/// as a fan of diagonals across the plot. Walking each column's min and max
/// alternately draws the same band as vertical strokes, which is how a scope
/// renders min/max anyway, and costs one polyline instead of thousands of
/// quads.
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
    fn every_offered_rate_divides_the_base_rate() {
        for r in RATES {
            assert_eq!(js220::BASE_RATE % r, 0, "{r} Hz does not divide the base rate");
            assert!((js220::MIN_SAMPLE_RATE..=js220::MAX_SAMPLE_RATE).contains(&r));
        }
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
        assert_eq!(VIEWS[0].integral_unit, Some("C"), "current integrates to charge");
        assert_eq!(VIEWS[2].integral_unit, Some("J"), "power integrates to energy");
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

    #[test]
    fn y_bounds_pad_the_data() {
        let b = vec![
            Bucket { t: 0.0, min: -1.0, max: 1.0, mean: 0.0 },
            Bucket { t: 1.0, min: -2.0, max: 3.0, mean: 0.5 },
        ];
        let (lo, hi) = y_bounds(&b);
        assert!(lo < -2.0 && hi > 3.0);
    }

    #[test]
    fn y_bounds_give_a_flat_signal_a_band() {
        let b = vec![Bucket { t: 0.0, min: 5.0, max: 5.0, mean: 5.0 }];
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
        assert!(!text.contains('∫'), "voltage has no useful integral:\n{text}");
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
