# Roadmap

What is not built yet, in roughly the order it is worth building. Paused
2026-10-18; the work below is the agreed next tranche.

## Settings parity with the instrument software

The device panel covers the settings this driver could verify against a real
JS220. What the official application has and this does not:

- **Friendly fuse configuration.** The panel takes the instrument's own `F`
  (4q14) and `K` (8q10) constants. The official UI takes a pair of currents
  and a duration and converts. That conversion is in neither the C driver, the
  datasheet nor the user's guide — it lives in the Python UI. Read it out of
  `pyjoulescope_ui`, port it, and keep the raw constants available underneath,
  since a protective device deserves a way to see exactly what it was given.
- **Fuse response curve.** The official panel plots trip time against current.
  It falls out of the conversion above.
- **Signal frequency and statistics frequency as one coherent group.** Both
  exist; they are split between the toolbar and the panel.
- **Sensor-side LEDs.** `set_leds(sensor: true, …)` is implemented and
  unexercised; the panel only drives the controller's.
- **`c/target`, `s/dbg/mode`.** Deliberately omitted: one selects the firmware
  image to boot, the other a debug IO mode. Neither belongs behind a control
  that is easy to click by accident.

## Filtering

- **Host-side filters.** The instrument offers wideband and sinc1 between
  2 MHz and 1 MHz, and that is all it offers. Anything else — a proper
  decimating low-pass, a moving average, a median for spike rejection — has to
  run on the host, over the raw ring that is already retained.
- **Per-signal filtering.** Current and voltage rarely want the same
  treatment.
- **`s/dwnN/N`, the instrument's own divider.** Tried once and abandoned: it
  needs a filter mode absent from the published metadata, and setting the
  divider alone produced no samples at all. Host decimation was used instead.
  Worth another attempt now that the output barrier makes configuration
  reliable, since instrument-side decimation would cut USB bandwidth
  proportionally.

## Storage

- **A capture format that survives a long session.** The recorder writes
  interleaved `f32` with no header, which is honest and fast and tells a
  reader nothing — not the rate, not which signals, not when. Give it a header
  and an index.
- **Read back what was recorded.** Nothing opens a capture file. The GUI
  should load one and offer the same markers, measurements and export over it.
- **The official `.jls` format.** Reading it would make captures from the
  official application openable here. Writing it would make captures from here
  openable there. The format is documented in `jetperch/jls`.
- **A bounded disk buffer.** The GUI keeps about eleven minutes of coarse
  history in memory and a few seconds raw. Longer sessions need to spill to
  disk, which is what the official application's memory buffer setting does.
- **Export the whole capture**, not only the visible or marked span.

## Menu and layout

- **A real menu bar.** Controls live in one wrapping toolbar; it is already
  crowded and the next feature makes it worse.
- **Detachable panels.** The official application lets widgets be arranged and
  torn off. egui supports this; it is a question of persisting the layout.
- **Remember settings between runs.** Nothing is persisted: rate, visible
  signals, marker positions and every device setting reset on each launch.

## Verification still owed

- **A wire from an output to an input.** It settles the GPI decoding in
  seconds, and is the one claim in the README that rests on inference rather
  than observation.
- **Linux and macOS.** Never built, let alone run.
- **A second instrument.** Device selection is exercised only by unit tests.
