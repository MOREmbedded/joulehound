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

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("Joulescope JS220"),
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
