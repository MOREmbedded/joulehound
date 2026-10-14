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
```

## Status

The protocol layer and driver logic are complete and unit tested (167 tests, no
hardware required). Streaming has been **proven against a physical JS220** at
the full rate, but **starting a stream is currently unreliable** — see
[Verification](#verification).

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

Being precise about this, because "it compiles" is not "it works".

**Confirmed by unit tests** (167 tests, `cargo test`): frame packing and
parsing, the value union in both directions, pub/sub encoding, port 0 message
decoding, statistics fixed-point conversion against hand-computed values,
sample id unwrapping across the 32-bit wrap, signal alignment (gaps, overlaps,
duplicates, late starts, oversized-gap re-anchoring), the time base, and
capture round-tripping through real files.

**Confirmed against hardware** (JS220, fw 1.3.0, FPGA 1.3.3):

- Enumeration, open, and the connect handshake.
- `jsctl topics`, which reads the device's own metadata tree.
- `jsctl stats` — on-instrument statistics, returning physically sensible
  values (nA-scale current, µV-scale voltage noise on an idle port).
- **Full-rate streaming.** Repeated benchmark runs reached **7.75 MiB/s,
  1,000,009 rows/s — 100.00% of the requested 1 Msps** with both signals
  balanced, zero undecodable frames, zero frame-sequence breaks, and ~130 NaN
  samples filled per three-second run (all at stream startup).

**Known defect: stream startup is unreliable.** When a stream starts it runs
perfectly, but it only starts on roughly one run in six. `stream_start` detects
the failure and returns an error rather than handing back a silently empty
capture, so it fails loudly.

This has been isolated to a defect *here*, not to the device or its state. Run
back to back on the same device, the official `pyjoulescope_driver` streams
1,000,338 samples/s on both signals every single time, while this driver fails
every time, in either order. Device state is not the variable.

What is established:

- The instrument acknowledges every configuration publish with success
  (`s/i/ctrl# = 0`) and then sends no samples, so acknowledgements cannot be
  used as proof that an enable took effect.
- Time synchronization is healthy throughout and the device's own sample
  counter is advancing at ~2 MHz, so the sensor is producing samples
  internally — they are simply not being sent.
- The sensor stays powered and responsive (FPGA version reads back, no fuse is
  engaged).
- `defaults.rs` holds the official driver's complete open sequence, captured
  from its own debug log: metadata request, ping barrier, a 57-topic parameter
  restore, values request, ping barrier, then the enables. Replicating it made
  matters strictly worse — never starting, versus one run in six — so it is
  retained as verified reference data rather than live behaviour. Something
  outside the published topic sequence is the difference.

A genuine transport defect was found and fixed along the way, and it is worth
knowing about independently: **every frame the device sends is exactly 512
bytes, which is also the endpoint's maximum packet size.** A bulk IN transfer
is therefore never terminated early by a short packet — it completes only once
it is full. A 128 KiB transfer holds a trickle of configuration replies hostage
until 256 of them accumulate, which is why pings and acknowledgements appeared
to time out and then arrive immediately afterwards. Anything that reads
low-rate control traffic from this device needs small transfers.

Two warnings for anyone continuing this:

- **The device's state drifts across sessions, which silently invalidates A/B
  testing.** Two consecutive six-run trials of one binary gave 1/6 then 0/6.
  Compare against the official driver in the same sitting, or the measurement
  is meaningless.
- A run that reports a high row rate is not necessarily working. The aligner
  fills gaps with NaN, so a dead stream can still produce rows. Check the
  reported bulk IN throughput, which is measured at the USB layer: 7.75 MiB/s
  is real, anything near zero is not.

The next step is a USB-level capture of both drivers' open sequences, diffed at
the packet layer. The pub/sub sequences are already known to match, so the
difference lies below them.

**Implemented to spec but not exercised by any firmware:** the `u4` and `u1`
run-length decoders for the current-range and GPI streams. Current firmware
ships those uncompressed — the reference driver notes the same — so they are
covered by unit tests against the documented format only.

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
