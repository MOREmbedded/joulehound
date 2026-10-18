//! Exporting what is on screen.
//!
//! The recorder writes the raw stream as it arrives; this writes the view
//! instead — the aggregated buckets for the span you are looking at, or the
//! span between the markers, which is usually what ends up in a report.
//!
//! Two formats, because they answer different needs: CSV goes anywhere, and
//! xlsx arrives as a real workbook with typed numbers, so a spreadsheet does
//! not have to guess at columns or mangle values on import.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rust_xlsxwriter::{Format as CellFormat, Workbook};

use crate::buffer::Bucket;

/// Anything that can go wrong writing an export.
#[derive(Debug)]
pub enum Error {
    /// The file could not be written.
    Io {
        /// The file that was being written.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The workbook writer rejected the data.
    Xlsx {
        /// The file that was being written.
        path: PathBuf,
        /// The underlying error.
        source: rust_xlsxwriter::XlsxError,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "writing {}: {source}", path.display()),
            Self::Xlsx { path, source } => write!(f, "writing {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Xlsx { source, .. } => Some(source),
        }
    }
}

/// Result alias for exports.
pub type Result<T> = std::result::Result<T, Error>;

/// Output format for an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Comma separated text.
    Csv,
    /// A real Excel workbook.
    Xlsx,
}

impl Format {
    /// Extension conventionally used for this format.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Xlsx => "xlsx",
        }
    }

    /// Pick the format a path is asking for, defaulting to CSV.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("xlsx") => Self::Xlsx,
            _ => Self::Csv,
        }
    }
}

/// One exported column: a signal name, its unit, and its buckets.
pub struct Column<'a> {
    /// Signal name, used as the column heading.
    pub name: &'a str,
    /// Unit, included in the heading so the numbers are not bare.
    pub unit: &'a str,
    /// Aggregated buckets, which must share the time axis of every column.
    pub buckets: &'a [Bucket],
}

/// How many rows an export would produce.
pub fn row_count(columns: &[Column<'_>]) -> usize {
    columns.iter().map(|c| c.buckets.len()).max().unwrap_or(0)
}

/// Write `columns` to `path`, choosing the format from its extension.
///
/// Each row carries the bucket time and, per signal, the mean with the
/// minimum and maximum it covers. Exporting only the mean would throw away
/// exactly the detail the envelope exists to preserve: a 2 us spike inside a
/// 2.5 ms bucket is usually the whole reason someone is looking.
pub fn write(path: &Path, columns: &[Column<'_>]) -> Result<usize> {
    match Format::from_path(path) {
        Format::Csv => write_csv(path, columns),
        Format::Xlsx => write_xlsx(path, columns),
    }
}

/// Column headings: one for the time axis, three per signal.
fn headings(columns: &[Column<'_>]) -> Vec<String> {
    let mut out = vec!["time_s".to_string()];
    for c in columns {
        let name = c.name.to_lowercase().replace(' ', "_");
        for part in ["mean", "min", "max"] {
            out.push(if c.unit.is_empty() {
                format!("{name}_{part}")
            } else {
                format!("{name}_{part}_{}", c.unit)
            });
        }
    }
    out
}

/// Row indices worth writing: those where at least one signal has data.
///
/// Aggregating a span at a finer resolution than the data supports leaves
/// empty buckets between the real ones. Writing those as rows of `NaN` would
/// turn a clean export into something the reader has to filter first.
fn rows_with_data(columns: &[Column<'_>]) -> Vec<usize> {
    (0..row_count(columns))
        .filter(|r| {
            columns
                .iter()
                .any(|c| c.buckets.get(*r).is_some_and(|b| !b.is_empty()))
        })
        .collect()
}

/// The time of row `r`, taken from the first column that reaches it.
fn row_time(columns: &[Column<'_>], r: usize) -> f64 {
    columns
        .iter()
        .find_map(|c| c.buckets.get(r).map(|b| b.t))
        .unwrap_or(f64::NAN)
}

fn write_csv(path: &Path, columns: &[Column<'_>]) -> Result<usize> {
    let rows = rows_with_data(columns);
    let write_all = || -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut out = std::io::BufWriter::new(file);
        writeln!(out, "{}", headings(columns).join(","))?;
        for &r in &rows {
            write!(out, "{:.9}", row_time(columns, r))?;
            for c in columns {
                match c.buckets.get(r) {
                    Some(b) if !b.is_empty() => {
                        write!(out, ",{:.9e},{:.9e},{:.9e}", b.mean, b.min, b.max)?
                    }
                    // A signal with no sample here is left blank rather than
                    // zero-filled, which would read as a measurement that
                    // never happened.
                    _ => write!(out, ",,,")?,
                }
            }
            writeln!(out)?;
        }
        out.flush()
    };

    write_all().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(rows.len())
}

fn write_xlsx(path: &Path, columns: &[Column<'_>]) -> Result<usize> {
    let rows = rows_with_data(columns);
    let write_all = || -> std::result::Result<(), rust_xlsxwriter::XlsxError> {
        let mut book = Workbook::new();
        let sheet = book.add_worksheet();
        let bold = CellFormat::new().set_bold();

        let headings = headings(columns);
        for (i, h) in headings.iter().enumerate() {
            sheet.write_string_with_format(0, i as u16, h, &bold)?;
        }
        // Wide enough for the headings, which are longer than the numbers.
        sheet.set_column_range_width(0, headings.len() as u16 - 1, 18.0)?;
        // So the headings stay visible while scrolling a long capture.
        sheet.set_freeze_panes(1, 0)?;

        for (emitted, &r) in rows.iter().enumerate() {
            let row = emitted as u32 + 1;
            sheet.write_number(row, 0, row_time(columns, r))?;
            for (ci, c) in columns.iter().enumerate() {
                let Some(b) = c.buckets.get(r) else { continue };
                if b.is_empty() {
                    // Left as an empty cell, which a chart skips rather than
                    // drawing a line down to zero.
                    continue;
                }
                let base = 1 + ci as u16 * 3;
                // Written as numbers, not text, so the spreadsheet can chart
                // and aggregate them without an import step.
                sheet.write_number(row, base, b.mean as f64)?;
                sheet.write_number(row, base + 1, b.min as f64)?;
                sheet.write_number(row, base + 2, b.max as f64)?;
            }
        }
        book.save(path)?;
        Ok(())
    };

    write_all().map_err(|source| Error::Xlsx {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buckets(values: &[f32]) -> Vec<Bucket> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| Bucket {
                t: i as f64 * 0.001,
                min: v - 1.0,
                max: v + 1.0,
                mean: *v,
            })
            .collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jsgui-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn the_format_comes_from_the_extension() {
        assert_eq!(Format::from_path(Path::new("a.xlsx")), Format::Xlsx);
        assert_eq!(Format::from_path(Path::new("a.XLSX")), Format::Xlsx);
        assert_eq!(Format::from_path(Path::new("a.csv")), Format::Csv);
        // Anything else is text, which is the safer default.
        assert_eq!(Format::from_path(Path::new("a")), Format::Csv);
        assert_eq!(Format::Csv.extension(), "csv");
        assert_eq!(Format::Xlsx.extension(), "xlsx");
    }

    #[test]
    fn the_row_count_follows_the_longest_column() {
        let a = buckets(&[1.0, 2.0, 3.0]);
        let b = buckets(&[1.0]);
        let cols = vec![
            Column {
                name: "A",
                unit: "A",
                buckets: &a,
            },
            Column {
                name: "B",
                unit: "V",
                buckets: &b,
            },
        ];
        assert_eq!(row_count(&cols), 3);
        assert_eq!(row_count(&[]), 0);
    }

    #[test]
    fn a_heading_carries_the_unit() {
        let a = buckets(&[1.0]);
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        assert_eq!(
            headings(&cols),
            vec!["time_s", "current_mean_A", "current_min_A", "current_max_A"]
        );
    }

    #[test]
    fn a_unitless_signal_gets_no_trailing_underscore() {
        // Raw ADC counts and GPI lines have no SI unit to print.
        let a = buckets(&[1.0]);
        let cols = vec![Column {
            name: "GPI 0",
            unit: "",
            buckets: &a,
        }];
        assert_eq!(headings(&cols)[1], "gpi_0_mean");
    }

    #[test]
    fn csv_has_a_heading_and_a_row_per_bucket() {
        let a = buckets(&[1.0, 2.0]);
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        let p = temp("basic.csv");
        assert_eq!(write(&p, &cols).unwrap(), 2);

        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "header plus two rows");
        assert_eq!(
            lines[0],
            "time_s,current_mean_A,current_min_A,current_max_A"
        );
        assert!(lines[1].starts_with("0.000000000,"));
        assert!(lines[2].starts_with("0.001000000,"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn csv_carries_the_envelope_not_just_the_mean() {
        // The min and max are the whole reason the plot is readable.
        let a = buckets(&[5.0]);
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        let p = temp("envelope.csv");
        write(&p, &cols).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let row = text.lines().nth(1).unwrap();
        let fields: Vec<&str> = row.split(',').collect();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[1].parse::<f64>().unwrap(), 5.0, "mean");
        assert_eq!(fields[2].parse::<f64>().unwrap(), 4.0, "min");
        assert_eq!(fields[3].parse::<f64>().unwrap(), 6.0, "max");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_short_column_is_left_blank_rather_than_zero_filled() {
        let a = buckets(&[1.0, 2.0]);
        let b = buckets(&[1.0]);
        let cols = vec![
            Column {
                name: "A",
                unit: "A",
                buckets: &a,
            },
            Column {
                name: "B",
                unit: "V",
                buckets: &b,
            },
        ];
        let p = temp("ragged.csv");
        write(&p, &cols).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let last = text.lines().last().unwrap();
        assert!(last.ends_with(",,,"), "expected blanks, got {last}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_time_axis_survives_a_ragged_first_column() {
        // The second column still has the row, so the time is not lost.
        let a = buckets(&[1.0]);
        let b = buckets(&[1.0, 2.0]);
        let cols = vec![
            Column {
                name: "A",
                unit: "A",
                buckets: &a,
            },
            Column {
                name: "B",
                unit: "V",
                buckets: &b,
            },
        ];
        assert_eq!(row_time(&cols, 1), 0.001);
    }

    fn gap() -> Bucket {
        Bucket {
            t: 0.001,
            min: f32::NAN,
            max: f32::NAN,
            mean: f32::NAN,
        }
    }

    #[test]
    fn a_row_no_signal_reached_is_not_written_at_all() {
        // Aggregating finer than the data supports leaves holes; they are not
        // measurements and do not belong in the file.
        let three = buckets(&[1.0, 2.0, 3.0]);
        let a = vec![three[0], gap(), three[2]];
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        let p = temp("gaps.csv");
        assert_eq!(write(&p, &cols).unwrap(), 2, "the hole is skipped");
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 3, "header plus two rows");
        assert!(!text.contains("NaN"), "a hole leaked into the file: {text}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_hole_in_one_signal_leaves_its_cells_blank() {
        // The other signal still has this row, so the row survives and the
        // gap shows as empty cells rather than a fabricated value.
        let a = vec![buckets(&[1.0])[0], gap()];
        let b = buckets(&[1.0, 2.0]);
        let cols = vec![
            Column {
                name: "A",
                unit: "A",
                buckets: &a,
            },
            Column {
                name: "B",
                unit: "V",
                buckets: &b,
            },
        ];
        let p = temp("hole.csv");
        assert_eq!(write(&p, &cols).unwrap(), 2);
        let text = std::fs::read_to_string(&p).unwrap();
        let last = text.lines().last().unwrap();
        let fields: Vec<&str> = last.split(',').collect();
        assert_eq!(fields.len(), 7);
        assert!(fields[1..4].iter().all(|f| f.is_empty()), "got {last}");
        assert_eq!(fields[4].parse::<f64>().unwrap(), 2.0, "B survives");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn an_all_holes_export_writes_only_a_header() {
        let a = vec![gap(), gap()];
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        let p = temp("allgaps.csv");
        assert_eq!(write(&p, &cols).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 1);
        let _ = std::fs::remove_file(&p);
    }

    /// Read one part out of a workbook with an independent zip reader, so the
    /// writer is not the only witness that the file is well formed.
    fn part(path: &Path, name: &str) -> String {
        use std::io::Read as _;
        let file = std::fs::File::open(path).unwrap();
        let mut archive = zip::ZipArchive::new(file).expect("not a readable zip archive");
        let mut entry = archive
            .by_name(name)
            .unwrap_or_else(|_| panic!("{name} missing from the workbook"));
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn xlsx_is_written_as_a_real_workbook() {
        let a = buckets(&[1.0, 2.0, 3.0]);
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        let p = temp("book.xlsx");
        assert_eq!(write(&p, &cols).unwrap(), 3);

        let bytes = std::fs::read(&p).unwrap();
        // xlsx is a zip archive; the magic proves it is not CSV wearing a
        // misleading extension.
        assert_eq!(&bytes[..2], b"PK", "not a zip archive");
        // The parts a spreadsheet actually looks for when opening a file.
        for name in [
            "[Content_Types].xml",
            "xl/workbook.xml",
            "xl/worksheets/sheet1.xml",
        ] {
            let _ = part(&p, name);
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn xlsx_stores_measurements_as_numbers_not_text() {
        // The point of the format: a spreadsheet can chart and average these
        // without an import step. Numeric cells carry no `t="s"` attribute
        // and hold the value directly.
        let a = buckets(&[2.5]);
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        let p = temp("typed.xlsx");
        write(&p, &cols).unwrap();

        let sheet = part(&p, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("<v>2.5</v>"), "mean not numeric: {sheet}");
        assert!(sheet.contains("<v>1.5</v>"), "min not numeric");
        assert!(sheet.contains("<v>3.5</v>"), "max not numeric");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn xlsx_keeps_the_headings_and_freezes_them() {
        let a = buckets(&[1.0]);
        let cols = vec![Column {
            name: "Current",
            unit: "A",
            buckets: &a,
        }];
        let p = temp("headings.xlsx");
        write(&p, &cols).unwrap();

        // Headings live in the shared string table.
        let strings = part(&p, "xl/sharedStrings.xml");
        for heading in ["time_s", "current_mean_A", "current_min_A", "current_max_A"] {
            assert!(strings.contains(heading), "{heading} missing: {strings}");
        }
        // So they stay visible while scrolling a long capture.
        let sheet = part(&p, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("frozen"), "header row not frozen: {sheet}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn exporting_nothing_still_writes_a_header() {
        let cols: Vec<Column<'_>> = Vec::new();
        let p = temp("empty.csv");
        assert_eq!(write(&p, &cols).unwrap(), 0);
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.trim(), "time_s");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_bad_path_names_the_file_it_could_not_write() {
        let p = std::env::temp_dir().join("no-such-dir-jsgui").join("x.csv");
        let err = write(&p, &[]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("x.csv"), "got {text}");
        assert!(std::error::Error::source(&err).is_some());
    }
}
