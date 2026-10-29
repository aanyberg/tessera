//! Raw bytes into `tessera_vt::Parser`: no input panics, and every action
//! keeps the parser's documented shape.
//!
//! The input is parsed twice, whole and split at a point taken from the
//! first byte, so the parser's cross-call state (partial UTF-8, a sequence
//! straddling two calls, a DCS chunk in flight) is exercised too. The two
//! action streams are *not* compared: `vte` 0.15 drops a byte when a UTF-8
//! character completes across two `advance` calls, which `Terminal::advance`
//! works around (the `chunking` target checks that level). Moving the
//! workaround into the parser is a follow-up; this target is where its
//! chunk-invariance check goes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tessera_vt::{Action, MAX_DCS_CHUNK, Parser};

/// What one action stream must satisfy, checked as the actions arrive.
#[derive(Default)]
struct Checker {
    in_dcs: bool,
    dcs_ignored: bool,
}

impl Checker {
    fn check(&mut self, action: &Action) {
        match action {
            Action::Csi {
                params, subparams, ..
            } => {
                // Bit `i` marks `params[i]`; no bit at or above `params.len()`,
                // `ignore` or not. `vte` keeps at most 32 entries, so a
                // shift by the length overflows only when every bit is in
                // range anyway.
                let len = u32::try_from(params.len()).expect("vte caps the parameters");
                assert!(len <= 32, "{len} parameters: more than vte keeps");
                let beyond = subparams.checked_shr(len).unwrap_or(0);
                assert_eq!(
                    beyond, 0,
                    "subparams {subparams:#b} has bits past {len} params: {action:?}"
                );
                // The first entry starts a group; it is never a sub-parameter.
                assert_eq!(
                    subparams & 1,
                    0,
                    "first parameter marked as sub: {action:?}"
                );
            }
            Action::Print(c) => {
                assert!(
                    !('\u{80}'..='\u{9f}').contains(c),
                    "C1 control {c:?} printed instead of executed"
                );
            }
            Action::DcsStart { ignore, .. } => {
                assert!(!self.in_dcs, "nested DCS");
                self.in_dcs = true;
                self.dcs_ignored = *ignore;
            }
            Action::DcsData(data) => {
                assert!(self.in_dcs, "DCS data outside a DCS");
                assert!(!self.dcs_ignored, "DCS data for an ignored header");
                assert!(
                    (1..=MAX_DCS_CHUNK).contains(&data.len()),
                    "DCS chunk of {} bytes",
                    data.len()
                );
            }
            Action::DcsEnd => {
                assert!(self.in_dcs, "DCS end outside a DCS");
                self.in_dcs = false;
            }
            _ => {}
        }
    }
}

fn run(chunks: &[&[u8]]) {
    let mut parser = Parser::new();
    let mut checker = Checker::default();
    for chunk in chunks {
        parser.advance(chunk, |action| checker.check(&action));
        assert!(parser.pending_dcs_len() <= MAX_DCS_CHUNK);
    }
}

fuzz_target!(|data: &[u8]| {
    run(&[data]);
    if let Some((&first, _)) = data.split_first() {
        let cut = usize::from(first) % (data.len() + 1);
        let (head, tail) = data.split_at(cut);
        run(&[head, tail]);
    }
});
