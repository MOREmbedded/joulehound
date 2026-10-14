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

The protocol layer and the driver logic are complete and unit tested (164
tests, no hardware required). **The end-to-end path has not yet been exercised
against a physical JS220** — see [Verification](#verification) for exactly what
is and is not confirmed.

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

**Confirmed by unit tests** (164 tests, `cargo test`):

- Frame header packing, parsing, iteration, and recovery from a corrupt frame
- The value union, in both directions, for every type tag
- Pub/sub message encoding including topic truncation and the fixed-width field
- Port 0 connect, echo, time sync and time map decoding
- Statistics fixed-point conversion, checked against hand-computed Q31/Q27
  values and a two-level signal whose mean and standard deviation are known
- Sample id unwrapping across the 32-bit wrap, including repeated wraps
- Signal alignment: gaps, overlaps, duplicate frames, late-starting signals
- The time base, including the 2018 epoch and sample-id round trips
- CSV and binary capture round-tripping through real files

**Confirmed against hardware:** device enumeration only (`jsctl list` correctly
identifies an attached JS220 by serial, mode and product id).

**Not yet confirmed against hardware:** opening, the connect handshake,
streaming, statistics, and the throughput claims above. The architecture is
sound and the protocol is implemented to spec, but no samples have come off a
real device through this code yet. Treat the performance section as design
rationale, not as measurements. `jsctl bench` exists precisely so the claims can
be checked rather than taken on faith.

**Implemented to spec but not exercised by any firmware:** the `u4` and `u1`
run-length decoders for the current-range and GPI streams. Current JS220
firmware ships those streams uncompressed — the reference driver notes the same
— so they are covered by unit tests against the documented format only. The bit
order within a `u1` literal token follows the little-endian convention used
everywhere else in the protocol but has not been confirmed on a device.

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
