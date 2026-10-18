# joulescope-rs

A Rust host driver and command line tool for the
[Joulescope JS220](https://www.joulescope.com/) precision energy analyzer.

The official stack is a C driver (`joulescope_driver`) with Python bindings and
a Qt UI. This is an independent implementation of the same USB protocol in
Rust, built around a deeper bulk transfer queue and an allocation-free receive
path.

```
crates/
  js220-proto   wire protocol codec — no I/O, no threads, no device handles
  js220         USB transport and device session
  jsctl         command line tool
  jsgui         live GUI: plots, markers, measurements, recording
```

![jsgui](docs/jsgui.png)

## Status

Working and verified on hardware. Streams at the full 1 Msps on current and
voltage simultaneously — **7.75 MiB/s sustained, 100% of the requested rate** —
and captures to disk at every supported rate from 1 kHz to 1 Msps.

## Build

Needs a Rust toolchain and a linker. Nothing else; there are no C dependencies,
so no libusb, no CMake, no vendored submodules.

```bash
cargo build --release
```

The binary lands at `target/release/jsctl`.

## Use

```bash
# What is attached?
jsctl list

# Versions, settings, and the transfer queue in effect
jsctl info

# Live statistics, computed on the instrument
jsctl stats --stats-rate 2

# Stream current, voltage and power to a file for 10 seconds
jsctl capture -o run.f32 --rate 1000000 --signals i,v,p --duration 10

# Same, as CSV
jsctl capture -o run.csv --format csv --rate 1000 --duration 10

# Capture logic lines alongside the signals
jsctl bench --duration 5 --gpi 0,1,7 --gpio-vref 3v3

# How fast does this host actually sustain?
jsctl bench --duration 10
```

### Capture formats

`--format bin` (the default) writes little-endian `f32`, columns interleaved in
the order given to `--signals`, with no header. A three-signal capture at
1 Msps is 12 MB/s. Read it back with anything that can read raw floats:

```python
import numpy as np
rows = np.fromfile("run.f32", dtype="<f4").reshape(-1, 3)
current, voltage, power = rows[:, 0], rows[:, 1], rows[:, 2]
```

`--format csv` writes a header plus `sample_id`, `time_s` and one column per
signal. It is far slower and much larger; use it for small captures and quick
inspection.

## The GUI

```bash
jsgui           # pick a device and press Start
jsgui --start   # begin streaming immediately
```

![jsgui](docs/jsgui.png)

**Resolution follows the view.** Recent samples are retained raw and aggregated
to whatever range is on screen, so zooming in reveals the real waveform down to
individual samples. Aggregating once at capture time would be cheaper, but the
detail would be gone for good and a 1 Msps signal would read as a solid block
at every zoom. The badge on each plot says what you are looking at: `raw`, or
how many samples each column covers, or `history` once you pan beyond the raw
window into the coarse buffer.

Each column is stroked from its minimum to its maximum, so a single-sample
spike stays visible however far out you are, with the mean drawn over the top.

**Two acquisition paths.** The rate list runs 1 kHz to 1 MHz for calibrated
amperes and volts, and ends with **2 MHz raw ADC**. The instrument caps
calibrated output at 1 Msps because its analog ports decimate the 2 MHz
counter by two; the raw ADC ports do not, so they reach the full rate. Raw
samples are uncalibrated counts with no gain, offset or range correction
applied, so the plots relabel to ADC 0 and ADC 1, power disappears — counts
cannot be multiplied into watts — and no integral is offered. It is for seeing
fast edges, not for measuring. `jsctl --source raw` does the same from the
command line.

**Scaling.** Both axes pan and zoom at any time, with the wheel zooming about
the pointer; shift restricts it to time and ctrl to the value axis. Each plot
has its own *Follow* (track the newest data) and *Auto Y* (scale to what is on
screen), kept independent so a Y zoom does not stop the trace tracking live
data and following live data does not stamp out a Y zoom. *Fit* turns both back
on. A view the data has scrolled past snaps back to the live edge rather than
stranding you on an empty plot.

**Markers.** `A|B` places a pair around the pointer, `A` places one there,
`✖` clears them; right-clicking any plot offers the same at the point you
clicked. They are dragged directly in any plot and appear at the same instant
in all of them; dragging *between* a pair moves both rigidly, which is how a
measurement window is positioned once its width is right.
Each is labelled with the value underneath it, and the span's statistics are
drawn beside it inside the plot — avg, std, rms, min, max, p2p, and the time
integral where one is meaningful, so current integrates to charge and power to
energy but voltage gets none.

**Peaks and thresholds.** *Peaks* marks the tallest local maxima on every
plot, with a count you choose; each is labelled with its value and time.
*Thresh*, per plot, takes a level in that plot's own unit — amperes for
current, watts for power — and shades every region on the chosen side of it,
reporting the extreme reached and how long the excursion lasted. Both run over
the aggregated buckets rather than the raw samples, so they find what is
actually on screen and zooming in refines the answer instead of changing it;
because a bucket is compared on its extreme rather than its mean, a 2 µs
excursion inside a 2.5 ms bucket still counts.

**Export** writes the visible span, or just the span between the markers, to
`.csv` or a real `.xlsx` workbook — the extension picks the format, and xlsx
carries typed numbers with a frozen header row rather than being a CSV in
disguise. The span is re-aggregated at the row count you set rather than
reusing the on-screen buckets, so the file does not inherit the width of the
window it happened to be exported from. Every row carries the mean, minimum
and maximum of its time slice, because exporting the mean alone would discard
exactly the spike the envelope exists to show. Slices no signal reached are
left out rather than written as `NaN`.

**Logic lines.** *Logic* picks which of the instrument's general purpose
inputs to capture — IN 0 through IN 3 and IN 7, the trigger input — and they
appear as compact rows beneath the waveforms, sharing one time axis and the
same markers, so an edge can be read against the current step it caused. Each
row reports the edges and duty cycle over the marker span, and the markers
show the level underneath them. They run at the full 2 MHz whatever rate the
analog path is using, because the point of a logic line is where its edges
fall; the ports are enabled as part of the stream configuration, so the
selection applies when streaming starts. Pass `--gpi 0,1,7` to select them
from the command line instead.

*Compare against* decides what the inputs measure themselves against.
**External Vref** is the instrument's own default and senses against the
reference you drive on the Vref pin; with nothing connected there the inputs
read a fixed level and no edge ever appears. **3.3 V** has the instrument
supply the reference itself.

**Marker measurements.** With a pair down, a panel under the plots reports
Δt and 1/Δt and, per signal, avg, std, rms, min, max, p2p and the time
integral where one is meaningful — so charge and energy are read off the same
span, and voltage gets a dash rather than a number nobody asked for. Logic
lines report their edges and duty cycle over the same span. The per-plot
overlay steps aside while the panel is up rather than repeating it over the
trace it describes.

**Stop does not disturb the target.** Stopping closes the instrument's data
ports but keeps the device open, so the current path stays shut and whatever
is being measured keeps running. Starting again takes about 0.35 s. `⏏`
releases the instrument deliberately, and that *does* cut the target: the
JS220 reverts to its own defaults on disconnect, and those open the path.

**Recording** writes the same interleaved little-endian `f32` format `jsctl`
produces, so the two are interchangeable.

The acquisition thread owns the device and never blocks on rendering: a host
that stops draining the bulk IN queue makes the *device* drop samples silently.

## Design

### Where the throughput comes from

The JS220 streams up to 1 Msps on current and voltage simultaneously, which is
roughly 8 MB/s of bulk IN traffic that must never stall. Three things matter:

**A deep, wide transfer queue.** The default is 8 bulk IN transfers of 128 KiB
each, one megabyte in flight. The device always has somewhere to put data, and
the host wakes roughly 60 times a second per queued transfer rather than
thousands. Both dimensions are tunable with `--transfer-size` and
`--queue-depth`, and `jsctl bench` will tell you what a given pairing achieves
on your machine.

**An allocation-free steady state.** A completed transfer is parsed in place —
frames are borrowed out of the USB buffer, never copied into intermediate
messages — and the buffer goes straight back into the queue. Decoded samples
land in reused per-signal buffers. After the first few seconds, streaming
performs no heap allocation at all.

**Decimation on the instrument.** Sample rates below 1 Msps are produced by the
JS220's own decimation filter, so the host never receives samples it would only
filter away. Running at 10 ksps moves a hundredth of the bytes rather than
moving them all and discarding 99%.

`jsctl capture` additionally runs formatting and disk I/O on a separate thread,
fed by a bounded queue with buffer recycling. If the writer falls behind, blocks
are dropped and **counted**, rather than back-pressuring the acquisition thread
into stalling the USB queue — a stalled queue makes the *device* drop data
silently, which is strictly worse than a host-side drop you can see in the
summary.

### Sample alignment

Current and voltage arrive on separate ports, in separate frames, with
independent sample ids and no guarantee of arriving together. The driver places
both on one timeline keyed by sample id and hands out only the span where every
requested signal is present.

Gaps are filled with `NaN` rather than closed up. This is deliberate: if a frame
is lost, row *N* must still be the same instant on every signal, or every
sample after the loss is silently misaligned against the others. A `NaN` is
visible; a quiet one-sample shift is not.

Power is computed on the host as `current × voltage` from the aligned pair, so
requesting power implicitly enables both. The instrument can emit power
directly at lower rates, but computing it keeps one code path for all rates and
costs a vectorized multiply.

### 128-bit statistics

The on-instrument statistics record accumulates sums, sums of squares and
running integrals in fixed point up to 128 bits wide. Rust has `i128` natively,
so variance and charge/energy integration are ordinary arithmetic; the
reference C driver carries a hand-written 128-bit module for the same job.
Variance is computed entirely in integers before a single conversion to `f64`,
so no precision is lost to intermediate rounding.

### Threading

`Js220` is deliberately single-threaded: one thread owns the device and drives
`poll()`. There are no locks on the receive path. Moving decoded samples
elsewhere is the caller's decision — `jsctl capture` sends them to a writer
thread, `jsctl stats` just prints them.

## Verification

**Unit tests** (183, `cargo test`, no hardware): frame packing and parsing, the
value union both ways, pub/sub encoding, port 0 decoding, statistics
fixed-point conversion against hand-computed values, sample id unwrapping
across the 32-bit wrap, signal alignment (gaps, overlaps, duplicates, late
starts, oversized-gap re-anchoring), box decimation, the time base, and capture
round-tripping through real files.

**On hardware** (JS220 005636, fw 1.3.0, FPGA 1.3.3):

| | Result |
| --- | --- |
| Sustained bulk IN | **7.75 MiB/s** |
| Rate achieved, 1 Msps | **99.99–100.00%** of requested, measured from real samples |
| Rates 1 k / 10 k / 100 k / 200 k / 500 k / 1 M | 100% on most runs; an occasional run lands lower (one 200 kHz run measured 84%) |
| Raw ADC, 2 Msps | **1,999,875 samples/s — 99.99%**, at the same 7.75 MiB/s, since i16 at twice the rate is the same bytes as f32 |
| Statistics | 2 Hz blocks arriving on time, charge and energy accumulating monotonically |
| Frame integrity | 0 undecodable, 0 sequence breaks |
| Gap fill | ~126 NaN samples per run, all at stream startup |
| Capture, 1 Msps binary | 2,999,808 rows in 3.001 s — exactly 1 Msps, 34.33 MiB |
| Capture, 1 ksps CSV | 1,999 rows in 2.001 s, sample ids advancing by 2000 |
| GPI, all five lines | **2.00 Msps each**, 0 filled, 0 discontinuities, at 8.72 MiB/s |
| GUI, three logic lines | 1.01 M samples/s at 8.53 MiB/s |
| Target power switch | **10 of 10 toggles**, device's own `s/i/range/mode` reading 0 and 4 exactly as asked |
| Stream restart, device held open | **0.35 s**, target current continuous across it (was 1.29 s) |
| GUI, all five logic lines | 1.08 M samples/s at 9.73 MiB/s, 2 min 47 s unbroken, no frame errors, ~1.15 cores |

GPI **levels** are not verified. Nothing is connected to the input pins here,
so every line reads low under both Vref settings and no edge has ever been
observed. The instrument exposes no output that could drive one, so this
cannot be settled without physically connecting a signal. The decoding itself
follows the instrument's own driver, which packs these ports eight samples to
a byte, least significant bit first.

### Two defects that hardware found, and what they teach

**A completed bulk OUT transfer is not proof the device received anything.** It
means only that the host controller accepted it. Immediately after CONNECT the
JS220 is not yet consuming OUT frames, so early configuration publishes were
discarded silently — the device would acknowledge every enable with success and
stream nothing. `open` now round-trips a port 0 echo until it comes back, which
is positive evidence that frames sent afterwards are acted on. This was the
difference between failing five runs in six and passing every one.

The same lesson had to be learned twice. `stream_start` publishes a burst of
configuration and did not use that barrier, so the tail of the burst was lost:
asking for five GPI lines reliably produced three or four, varying run to run,
always dropping the ones published last. One echo round-trip after the burst
fixed it — five of five, three runs in a row. The general form is that any
sequence of publishes needs a barrier before the device is expected to act on
the last of them.

**Connecting does not leave the instrument in a usable state.** A session that
configures only the topics it needs leaves everything else as the previous
session left it, and the device eventually stops streaming in a way no amount
of re-enabling recovers. `open` restores the complete parameter set in
[`defaults.rs`](crates/js220/src/defaults.rs), transcribed from the reference
driver's own debug log. Adding this recovered a device that had been failing
every run for twenty minutes.

A third issue is worth knowing independently: **every frame the device sends is
exactly 512 bytes, which is also the endpoint's maximum packet size**, so a
bulk IN transfer is never terminated early by a short packet — it completes
only when full. A large transfer therefore holds low-rate control traffic
hostage until enough replies accumulate to fill it.

### Known gaps

- **Power statistics were once wrong and are not any more, for reasons that
  were never established.** An earlier build reported a power minimum that
  never changed and a standard deviation and maximum of exactly zero. They are
  now fully populated and check out: over a 500 k sample block the reported
  power mean matched current mean x voltage mean to four figures, with the
  minimum and maximum tracking too, across every run tried. Removing the
  output barrier added for the GPI lines did not bring the old behaviour back,
  so that was not the cause. Recorded here rather than deleted, because a
  defect that disappears without a diagnosis can return the same way.
- Stream startup is not perfectly uniform: most runs reach 100% of the
  requested rate, but an occasional run comes in lower. Capture also discards
  the first fraction of a second while the stream settles, so a short capture
  yields slightly fewer rows than `duration × rate`.
- Statistics require that no sample stream is enabled. Enabling one alongside
  them stops the instrument emitting statistics after the first record, so
  `jsctl stats` runs with every data port off. The cause is not understood;
  the official tool does the same.
- **The first metadata request after a streaming session goes unanswered.**
  The device accepts the connect and passes the output barrier, then simply
  sends no metadata; asking a second time always works. This is not specific
  to the GPI lines — a plain 1 Msps run and a raw 2 Msps run both do it.
  `jsctl topics` now asks up to three times and says so when it has to.
- **`jsctl reset` cannot work on Windows.** WinUSB offers no port reset, so
  the command reports that and tells you to unplug the instrument instead,
  which has the same effect.
- GPI levels are unverified. The lines stream at exactly 2 Msps with no gaps,
  but nothing is wired to the inputs here, so every line reads low and no edge
  has been observed. The instrument has no output that could drive one.
- The current-range stream is decoded but not plotted. It is the same packed
  4-bit format as the GPI lines and would show which shunt is engaged at any
  instant, which is worth having when a range change shows up as a step in the
  current trace.

### A warning for anyone measuring this

A high row rate does not mean it is working. The aligner fills gaps with NaN,
so a dead stream can still produce rows at the requested rate — this reported a
confident "100.08%" from 2,265,732 synthesized samples against 2,520 real ones.
`jsctl bench` now reports the rate from real samples only. Trust that and the
bulk IN throughput, not the row count.

## Scope

Implemented: enumeration, open/close, the connect handshake, pub/sub
configuration, analog streaming with alignment, on-instrument statistics, time
synchronization and the sample-id-to-UTC map, and capture to disk.

Not implemented: firmware update (port 3 memory operations), GPI and UART
streaming, trigger and soft-fuse configuration, JS110 and JS320 support, and
host-side downsampling below 1 ksps. The protocol crate covers the port 3 and
GPI wire formats, so these are additions rather than redesigns.

## Platform notes

Built on [`nusb`](https://crates.io/crates/nusb), which is pure Rust and needs
no libusb.

- **Windows** — the JS220 must be bound to WinUSB, which the official Joulescope
  installer does. WinUSB permits a single handle per interface, so **close the
  Joulescope UI before using `jsctl`**; otherwise opening fails with access
  denied.
- **Linux** — needs a udev rule granting access to `16d0:10ba`.
- **macOS** — no additional setup.

## Relationship to the official driver

Independent implementation. No code is derived from
[`jetperch/joulescope_driver`](https://github.com/jetperch/joulescope_driver);
the wire formats it documents — frame layout, port assignments, message
structures, fixed-point scaling — were used as the interface specification they
are. See [NOTICE.md](NOTICE.md).

This project is not affiliated with or endorsed by Jetperch LLC. Joulescope is
a trademark of Jetperch LLC.

## License

MIT or Apache-2.0, at your option.
