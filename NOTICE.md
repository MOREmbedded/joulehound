# Notices and attribution

## Relationship to joulescope_driver

This project is an independent, clean implementation of the Joulescope JS220
USB protocol in Rust. It is not a port, and it does not link against or vendor
any part of the official driver.

The protocol itself — frame header bit layout, port assignments, message
structures, fixed-point scaling factors, topic names, USB identifiers — was
taken from the public documentation and source of
[`jetperch/joulescope_driver`](https://github.com/jetperch/joulescope_driver),
which is licensed Apache-2.0 and is the normative description of how a JS220
communicates. Those are interface facts: a host that encoded them differently
would simply not work.

No source code was copied. Structure, naming, error handling, buffering
strategy, threading model and the test suite are original. Where the two
implementations differ in approach, this is noted in the code — for example,
the 128-bit statistics arithmetic uses Rust's native `i128` rather than the
hand-rolled 128-bit module the C driver requires.

Jetperch LLC holds copyright in `joulescope_driver`. Joulescope is a trademark
of Jetperch LLC. This project is not affiliated with, sponsored by, or endorsed
by Jetperch LLC.

## Dependencies

Direct dependencies and their licenses:

| Crate | License |
| --- | --- |
| [`nusb`](https://crates.io/crates/nusb) | MIT OR Apache-2.0 |
| [`clap`](https://crates.io/crates/clap) | MIT OR Apache-2.0 |
| [`anyhow`](https://crates.io/crates/anyhow) | MIT OR Apache-2.0 |
| [`ctrlc`](https://crates.io/crates/ctrlc) | MIT OR Apache-2.0 |
| [`log`](https://crates.io/crates/log) | MIT OR Apache-2.0 |
| [`env_logger`](https://crates.io/crates/env_logger) | MIT OR Apache-2.0 |

Run `cargo tree` for the full transitive set.

## This project

Copyright the joulescope-rs contributors.

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work shall be dual licensed as above, without any
additional terms or conditions.
