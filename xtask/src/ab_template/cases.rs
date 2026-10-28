//! Benchmark and differential cases. The crates under test are available as
//! {aliases}.
//!
//! A bench case must do the same work on both sides and return a checksum of
//! its output; `cargo xtask ab bench` reports differing checksums as a
//! problem, which fails `--max-ratio`. Keep setup (corpus generation,
//! allocation) identical on both sides: the probe times the whole call. The
//! case must also be large enough to measure: at least 1000x the run-to-run
//! spread of the empty process (a few tens of millions of instructions is
//! typical); `bench` reports a smaller case as a problem.

use std::hint::black_box;

use crate::{Rng, Side};

/// Runs bench `case` on `side` and returns a checksum of its output, or
/// `None` for an unknown case.
pub fn bench(case: &str, side: Side) -> Option<u64> {
    match (case, side) {
        // ("ascii", Side::Base) => Some(feed_base(ASCII_CORPUS)),
        // ("ascii", Side::Head) => Some(feed_head(ASCII_CORPUS)),
        ("noop", _) => Some(black_box(0)),
        _ => None,
    }
}

/// Runs differential `case` for one seed: drive base and head with the same
/// generated input and return `Err` describing the first disagreement, or
/// `None` for an unknown case.
pub fn diff(case: &str, seed: u64) -> Option<Result<(), String>> {
    match case {
        "noop" => {
            let mut rng = Rng::new(seed);
            let input = rng.next_u64();
            // Compare `format!("{:?}", ...)` of the observable state of both sides.
            let (base, head) = (input, input);
            Some(if base == head {
                Ok(())
            } else {
                Err(format!("input {input}: base {base} != head {head}"))
            })
        }
        _ => None,
    }
}
