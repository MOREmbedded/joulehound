//! Capture output: a writer thread fed by the acquisition thread.
//!
//! Formatting and disk I/O both cost far more than decoding a USB transfer, so
//! they run on their own thread. The acquisition thread hands over interleaved
//! rows and immediately gets an empty buffer back, so the steady state does no
//! allocation on either side.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use js220::Signal;

/// Output encoding for a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Text, one row per line, with a header naming each column.
    Csv,
    /// Little-endian `f32`, columns interleaved, no header.
    ///
    /// This is the fast path: it is what the device sends, so writing it costs
    /// one memcpy per block.
    Bin,
}

impl Format {
    /// File extension conventionally used for this format.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Bin => "f32",
        }
    }
}

/// A block of interleaved rows handed to the writer.
pub struct Chunk {
    /// Sample id of the first row.
    pub sample_id: u64,
    /// Sample id increment per row.
    pub decimate: u32,
    /// Number of rows.
    pub rows: usize,
    /// Row-major interleaved samples, `rows * columns` long.
    pub data: Vec<f32>,
}

/// What the writer managed to record.
#[derive(Debug, Clone, Copy, Default)]
pub struct SinkStats {
    /// Rows written.
    pub rows: u64,
    /// Bytes written, before any OS buffering.
    pub bytes: u64,
    /// Blocks dropped because the writer could not keep up.
    pub dropped_blocks: u64,
    /// Rows lost with those blocks.
    pub dropped_rows: u64,
}

/// Handle to the writer thread.
pub struct Sink {
    tx: Option<SyncSender<Chunk>>,
    recycle: Receiver<Vec<f32>>,
    handle: Option<JoinHandle<Result<SinkStats>>>,
    path: PathBuf,
    dropped_blocks: u64,
    dropped_rows: u64,
}

impl Sink {
    /// Start a writer thread for `path`.
    ///
    /// `depth` is how many blocks may be queued before the acquisition thread
    /// is forced to drop data rather than stall. Dropping is deliberate: a
    /// stalled acquisition thread stops draining the USB queue, which makes the
    /// device drop samples silently, whereas a dropped block here is counted
    /// and reported.
    pub fn spawn(
        path: &Path,
        format: Format,
        signals: Vec<Signal>,
        rate: u32,
        depth: usize,
    ) -> Result<Self> {
        let file = File::create(path)
            .with_context(|| format!("creating capture file {}", path.display()))?;
        let (tx, rx) = sync_channel::<Chunk>(depth);
        let (recycle_tx, recycle_rx) = sync_channel::<Vec<f32>>(depth + 2);

        let path_for_thread = path.to_path_buf();
        let handle = std::thread::Builder::new()
            .name("jsctl-writer".into())
            .spawn(move || {
                writer_loop(file, format, signals, rate, rx, recycle_tx)
                    .with_context(|| format!("writing {}", path_for_thread.display()))
            })
            .context("spawning the writer thread")?;

        Ok(Self {
            tx: Some(tx),
            recycle: recycle_rx,
            handle: Some(handle),
            path: path.to_path_buf(),
            dropped_blocks: 0,
            dropped_rows: 0,
        })
    }

    /// Where the capture is being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Take a buffer to fill, reusing a recycled one when available.
    pub fn take_buffer(&mut self) -> Vec<f32> {
        match self.recycle.try_recv() {
            Ok(mut v) => {
                v.clear();
                v
            }
            Err(_) => Vec::new(),
        }
    }

    /// Hand a filled block to the writer.
    ///
    /// Returns `false` if the block was dropped because the queue was full.
    pub fn send(&mut self, chunk: Chunk) -> Result<bool> {
        let Some(tx) = self.tx.as_ref() else {
            return Ok(false);
        };
        let rows = chunk.rows as u64;
        match tx.try_send(chunk) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => {
                self.dropped_blocks += 1;
                self.dropped_rows += rows;
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => {
                // The writer exited, which means it failed. Join to surface why.
                anyhow::bail!("writer thread stopped early");
            }
        }
    }

    /// Close the file and collect the writer's totals.
    pub fn finish(mut self) -> Result<SinkStats> {
        self.tx.take();
        let handle = self
            .handle
            .take()
            .expect("handle is only taken here, and finish consumes self");
        let mut stats = handle
            .join()
            .map_err(|_| anyhow::anyhow!("writer thread panicked"))??;
        stats.dropped_blocks = self.dropped_blocks;
        stats.dropped_rows = self.dropped_rows;
        Ok(stats)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // Finish normally consumes self; this only runs on an early return, and
        // just makes sure the thread is not left detached.
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn writer_loop(
    file: File,
    format: Format,
    signals: Vec<Signal>,
    rate: u32,
    rx: Receiver<Chunk>,
    recycle: SyncSender<Vec<f32>>,
) -> Result<SinkStats> {
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut stats = SinkStats::default();
    let ncols = signals.len();

    if format == Format::Csv {
        let mut header = String::from("sample_id,time_s");
        for s in &signals {
            header.push(',');
            header.push_str(s.name());
            header.push('_');
            header.push_str(s.unit());
        }
        header.push('\n');
        out.write_all(header.as_bytes())?;
        stats.bytes += header.len() as u64;
    }

    // Reused across blocks so formatting does not reallocate per row.
    let mut line = String::with_capacity(128);
    let mut raw: Vec<u8> = Vec::new();

    while let Ok(chunk) = rx.recv() {
        match format {
            Format::Bin => {
                let bytes = encode_le(&chunk.data, &mut raw);
                out.write_all(bytes)?;
                stats.bytes += bytes.len() as u64;
            }
            Format::Csv => {
                for row in 0..chunk.rows {
                    let sample_id = chunk.sample_id + row as u64 * chunk.decimate as u64;
                    let t = sample_id as f64 / js220::BASE_RATE as f64;
                    line.clear();
                    use std::fmt::Write as _;
                    let _ = write!(line, "{sample_id},{t:.9}");
                    for col in 0..ncols {
                        let v = chunk.data[row * ncols + col];
                        let _ = write!(line, ",{v:.9e}");
                    }
                    line.push('\n');
                    out.write_all(line.as_bytes())?;
                    stats.bytes += line.len() as u64;
                }
            }
        }
        stats.rows += chunk.rows as u64;
        // Hand the buffer back; if the producer is gone the send just fails.
        let _ = recycle.try_send(chunk.data);
    }

    out.flush()?;
    let _ = rate; // reserved for formats that record the rate in a header
    Ok(stats)
}

/// Encode `data` as little-endian bytes into the reusable buffer `out`.
///
/// Reinterpreting the slice would skip this copy, but would also emit
/// big-endian floats on a big-endian host and silently produce a file that no
/// reader could interpret. The copy is far cheaper than the write it feeds.
fn encode_le<'a>(data: &[f32], out: &'a mut Vec<u8>) -> &'a [u8] {
    out.clear();
    out.reserve(std::mem::size_of_val(data));
    for v in data {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_have_distinct_extensions() {
        assert_eq!(Format::Csv.extension(), "csv");
        assert_eq!(Format::Bin.extension(), "f32");
    }

    #[test]
    fn encoding_f32_produces_little_endian_bytes() {
        let mut buf = Vec::new();
        let bytes = encode_le(&[1.0f32, -2.5], &mut buf);
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[..4], &1.0f32.to_le_bytes());
        assert_eq!(&bytes[4..], &(-2.5f32).to_le_bytes());
    }

    #[test]
    fn encoding_an_empty_slice_is_empty() {
        let mut buf = Vec::new();
        assert!(encode_le(&[], &mut buf).is_empty());
    }

    #[test]
    fn the_encode_buffer_is_reused_without_accumulating() {
        let mut buf = Vec::new();
        encode_le(&[1.0, 2.0], &mut buf);
        let bytes = encode_le(&[3.0], &mut buf);
        assert_eq!(bytes.len(), 4, "previous contents must be cleared");
    }

    #[test]
    fn binary_capture_roundtrips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("jsctl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.f32");

        let mut sink = Sink::spawn(
            &path,
            Format::Bin,
            vec![Signal::Current, Signal::Voltage],
            1_000_000,
            4,
        )
        .unwrap();
        sink.send(Chunk {
            sample_id: 0,
            decimate: 2,
            rows: 2,
            data: vec![1.0, 2.0, 3.0, 4.0],
        })
        .unwrap();
        let stats = sink.finish().unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(stats.bytes, 16);

        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw.len(), 16);
        assert_eq!(&raw[..4], &1.0f32.to_le_bytes());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn csv_capture_writes_a_header_and_one_line_per_row() {
        let dir = std::env::temp_dir().join(format!("jsctl-test-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.csv");

        let mut sink = Sink::spawn(&path, Format::Csv, vec![Signal::Current], 1_000_000, 4).unwrap();
        sink.send(Chunk {
            sample_id: 100,
            decimate: 2,
            rows: 2,
            data: vec![0.5, 0.25],
        })
        .unwrap();
        sink.finish().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "sample_id,time_s,current_A");
        assert!(lines[1].starts_with("100,"), "got {}", lines[1]);
        assert!(lines[2].starts_with("102,"), "sample id advances by decimate");
        assert_eq!(lines.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn buffers_are_recycled_back_to_the_producer() {
        let dir = std::env::temp_dir().join(format!("jsctl-test-recycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.f32");

        let mut sink = Sink::spawn(&path, Format::Bin, vec![Signal::Current], 1_000_000, 4).unwrap();
        let buf = sink.take_buffer();
        assert!(buf.is_empty(), "nothing to recycle yet");
        sink.send(Chunk {
            sample_id: 0,
            decimate: 2,
            rows: 1,
            data: vec![1.0],
        })
        .unwrap();

        // The writer returns the buffer once it has written the block.
        let mut got = Vec::new();
        for _ in 0..200 {
            got = sink.take_buffer();
            if got.capacity() > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(got.capacity() > 0, "buffer should have come back");
        sink.finish().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creating_a_sink_in_a_missing_directory_fails_clearly() {
        let path = std::env::temp_dir()
            .join("jsctl-no-such-dir-xyz")
            .join("cap.f32");
        let err = match Sink::spawn(&path, Format::Bin, vec![Signal::Current], 1_000_000, 2) {
            Ok(_) => panic!("creating a sink in a missing directory should fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("creating capture file"), "got {err}");
    }
}
