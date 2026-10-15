//! Human-readable number formatting.
//!
//! Shared in spirit with the CLI's copy; kept separate so the GUI can add
//! display-only helpers without changing what the CLI prints.

const PREFIXES: [(&str, f64); 9] = [
    ("T", 1e12),
    ("G", 1e9),
    ("M", 1e6),
    ("k", 1e3),
    ("", 1.0),
    ("m", 1e-3),
    ("u", 1e-6),
    ("n", 1e-9),
    ("p", 1e-12),
];

/// Format `value` with an SI prefix and `unit`, four significant digits.
///
/// Picks the prefix that puts the magnitude in `[1, 1000)`. Values outside the
/// prefix table fall back to scientific notation, and non-finite values are
/// rendered as their own name so a NaN in the data is visible rather than
/// silently printed as a number.
pub fn si(value: f64, unit: &str) -> String {
    if value.is_nan() {
        return format!("NaN {unit}");
    }
    if value.is_infinite() {
        let sign = if value.is_sign_negative() { "-" } else { "" };
        return format!("{sign}inf {unit}");
    }
    let mag = value.abs();
    if mag == 0.0 {
        return format!("0.000  {unit}");
    }
    for (prefix, scale) in PREFIXES {
        if mag >= scale {
            let scaled = value / scale;
            // A one-character prefix column keeps the table aligned.
            let pad = if prefix.is_empty() { " " } else { "" };
            return format!("{scaled:8.3} {prefix}{pad}{unit}");
        }
    }
    format!("{value:8.3e}  {unit}")
}

/// Format a byte count with a binary prefix.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Format a duration in seconds as `HH:MM:SS.mmm`.
pub fn duration(seconds: f64) -> String {
    let total_ms = (seconds * 1000.0).round().max(0.0) as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let m = (total_s / 60) % 60;
    let h = total_s / 3600;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Format an axis tick compactly with an SI prefix.
///
/// Axis labels are narrow, so this trims trailing zeros rather than padding to
/// a fixed width the way [`si`] does for table columns.
pub fn si_tick(value: f64, unit: &str) -> String {
    if value == 0.0 {
        return format!("0 {unit}");
    }
    if !value.is_finite() {
        return "—".into();
    }
    let mag = value.abs();
    for (prefix, scale) in PREFIXES {
        if mag >= scale {
            let scaled = value / scale;
            let text = format!("{scaled:.3}");
            let trimmed = text.trim_end_matches('0').trim_end_matches('.');
            return format!("{trimmed} {prefix}{unit}");
        }
    }
    format!("{value:.1e} {unit}")
}

/// Format a time position as `m:ss.mmm`, the axis format the instrument's own
/// software uses, falling back to seconds for very short spans.
pub fn clock(t: f64) -> String {
    if !t.is_finite() {
        return "—".into();
    }
    let sign = if t < 0.0 { "-" } else { "" };
    let a = t.abs();
    let minutes = (a / 60.0).floor() as u64;
    let seconds = a - minutes as f64 * 60.0;
    format!("{sign}{minutes}:{seconds:06.3}")
}

/// Format a time span for the window selector.
pub fn seconds(s: f64) -> String {
    if s < 0.001 {
        format!("{:.0} us", s * 1e6)
    } else if s < 1.0 {
        format!("{:.0} ms", s * 1000.0)
    } else {
        format!("{s:.0} s")
    }
}

/// Format a sample rate with an SI prefix, e.g. `1 MHz`.
pub fn rate(hz: u32) -> String {
    if hz >= 1_000_000 && hz % 1_000_000 == 0 {
        format!("{} MHz", hz / 1_000_000)
    } else if hz >= 1_000 && hz % 1_000 == 0 {
        format!("{} kHz", hz / 1_000)
    } else {
        format!("{hz} Hz")
    }
}

/// Format a large count compactly, e.g. `1.23 M`.
pub fn count(n: f64) -> String {
    let a = n.abs();
    if a >= 1e9 {
        format!("{:.2} G", n / 1e9)
    } else if a >= 1e6 {
        format!("{:.2} M", n / 1e6)
    } else if a >= 1e3 {
        format!("{:.1} k", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn si_picks_a_prefix_that_keeps_the_magnitude_readable() {
        assert!(si(0.001234, "A").contains("1.234 mA"));
        assert!(si(1.5, "V").contains("1.500  V"));
        assert!(si(1500.0, "W").contains("1.500 kW"));
        assert!(si(1.2e-9, "A").contains("1.200 nA"));
    }

    #[test]
    fn si_handles_negatives() {
        let s = si(-0.0025, "A");
        assert!(s.contains("-2.500 mA"), "got {s}");
    }

    #[test]
    fn si_renders_zero_without_a_prefix() {
        assert!(si(0.0, "A").contains("0.000  A"));
    }

    #[test]
    fn si_makes_non_finite_values_obvious() {
        assert_eq!(si(f64::NAN, "A"), "NaN A");
        assert_eq!(si(f64::INFINITY, "A"), "inf A");
        assert_eq!(si(f64::NEG_INFINITY, "A"), "-inf A");
    }

    #[test]
    fn si_falls_back_to_scientific_notation_below_the_table() {
        let s = si(1e-15, "A");
        assert!(s.contains('e'), "got {s}");
    }

    #[test]
    fn bytes_uses_binary_prefixes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(1024 * 1024 * 3 / 2), "1.50 MiB");
    }

    #[test]
    fn bytes_saturates_at_the_largest_prefix() {
        let huge = 1024u64.pow(5);
        assert!(bytes(huge).ends_with("TiB"));
    }

    #[test]
    fn axis_ticks_are_compact_and_trimmed() {
        assert_eq!(si_tick(0.0, "A"), "0 A");
        assert_eq!(si_tick(0.001, "A"), "1 mA");
        assert_eq!(si_tick(1.5e-6, "A"), "1.5 uA");
        assert_eq!(si_tick(-2.25, "V"), "-2.25 V");
    }

    #[test]
    fn axis_ticks_handle_non_finite_values() {
        assert_eq!(si_tick(f64::NAN, "A"), "—");
        assert_eq!(si_tick(f64::INFINITY, "A"), "—");
    }

    #[test]
    fn rates_render_with_a_prefix() {
        assert_eq!(rate(1_000_000), "1 MHz");
        assert_eq!(rate(500_000), "500 kHz");
        assert_eq!(rate(1_000), "1 kHz");
        assert_eq!(rate(1_500), "1500 Hz");
    }

    #[test]
    fn clock_renders_minutes_and_seconds() {
        assert_eq!(clock(0.0), "0:00.000");
        assert_eq!(clock(63.25), "1:03.250");
        assert_eq!(clock(600.5), "10:00.500");
        assert_eq!(clock(-1.5), "-0:01.500");
        assert_eq!(clock(f64::NAN), "—");
    }

    #[test]
    fn spans_render_in_sensible_units() {
        assert_eq!(seconds(0.0001), "100 us");
        assert_eq!(seconds(0.1), "100 ms");
        assert_eq!(seconds(0.5), "500 ms");
        assert_eq!(seconds(2.0), "2 s");
        assert_eq!(seconds(10.0), "10 s");
    }

    #[test]
    fn counts_render_compactly() {
        assert_eq!(count(999.0), "999");
        assert_eq!(count(1_500.0), "1.5 k");
        assert_eq!(count(2_500_000.0), "2.50 M");
        assert_eq!(count(3_000_000_000.0), "3.00 G");
    }

    #[test]
    fn durations_render_as_a_clock() {
        assert_eq!(duration(0.0), "00:00:00.000");
        assert_eq!(duration(1.5), "00:00:01.500");
        assert_eq!(duration(3661.25), "01:01:01.250");
    }

    #[test]
    fn negative_durations_clamp_to_zero() {
        assert_eq!(duration(-5.0), "00:00:00.000");
    }
}
