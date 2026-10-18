//! Live GUI for the Joulescope JS220.
//!
//! Acquisition runs on its own thread so the USB queue is drained even while
//! the window is busy rendering: a host that stops draining makes the device
//! drop samples silently.

#![warn(missing_docs)]
#![forbid(unsafe_code)]
// The GUI is a windowed application; a console would pop up behind it on
// Windows. Debug builds keep it so logging is visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod acquire;
mod app;
mod buffer;
mod export;
mod fmt;
mod peaks;
mod raw;

/// Decode the window icon, or go without one.
///
/// A missing icon is a cosmetic loss and never a reason to refuse to start,
/// so every failure here is swallowed.
fn window_icon() -> Option<std::sync::Arc<eframe::egui::IconData>> {
    // The 256 px source of the same mark compiled into the executable.
    let png = include_bytes!("../assets/icon.png");
    let decoded = image::load_from_memory(png).ok()?.into_rgba8();
    let (width, height) = decoded.dimensions();
    Some(std::sync::Arc::new(eframe::egui::IconData {
        rgba: decoded.into_raw(),
        width,
        height,
    }))
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,js220=info"))
        .init();

    // Deliberately not a full argument parser: a handful of switches do not
    // earn a dependency.
    let args: Vec<String> = std::env::args().collect();
    let autostart = args.iter().any(|a| a == "--start");
    let device_panel = args.iter().any(|a| a == "--device");
    let value = |name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        args.get(i + 1).cloned()
    };
    // Lets a capture that needs logic lines be started from a shortcut or a
    // script, without walking the menus first.
    let gpi = value("--gpi").map(|list| {
        list.split(',')
            .filter_map(|p| p.trim().parse::<u8>().ok())
            .collect::<Vec<u8>>()
    });
    let vref_3v3 = value("--gpio-vref").is_some_and(|v| v == "3v3");
    // Two times in seconds, for reproducing a measurement without hunting
    // for the same span by hand.
    let markers = value("--markers").and_then(|v| {
        let mut parts = v.split(',').filter_map(|p| p.trim().parse::<f64>().ok());
        Some((parts.next()?, parts.next()?))
    });

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([900.0, 560.0])
        .with_title("Joulescope JS220");
    // The icon embedded in the executable covers the file and the shortcut;
    // this is the one the window and the taskbar show.
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Joulescope JS220",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::App::new(
                cc,
                autostart,
                gpi.clone(),
                vref_3v3,
                markers,
                device_panel,
            )))
        }),
    )
}
