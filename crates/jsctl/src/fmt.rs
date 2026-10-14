//! Human-readable number formatting.

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
