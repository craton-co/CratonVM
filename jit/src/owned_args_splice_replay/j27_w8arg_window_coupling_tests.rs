//! j27 wave 8 (lane W8ARG). dev's service-copy window (gkr,
//! `deopt::rerun_window_closed_at` on origin/dev) places a safepoint inside
//! a splice at its enclosing `invoke`'s bci, and since hotfix 8335e345ff
//! calls that `invoke` an exact re-run point when
//! `deopt::owned_args_caller_replay_is_exact(outer, .., bci, &[])` holds.
//! A spliced owned call's hand-off re-runs the OUTER frame from bci 0 while
//! it stands there, so every chain the two-level rule admits must satisfy
//! that predicate at level 0, or the window would clear the outer caller's
//! argument copies that the re-run reads. These pin the coupling.
use super::*;

const MID: &str = "P.mid:(Ljava/lang/Object;)I";
/// `0 aload_0  1 invokestatic #1  4 ireturn`.
const PURE: [u8; 5] = [0x2a, 0xb8, 0x00, 0x01, 0xac];
/// ARGWIN21-4's fresh-array prefix: `0 iconst_1  1 newarray int  3 dup
/// 4 iconst_0  5 iconst_1  6 iastore  7 invokestatic #1  10 ireturn`.
const FRESH: [u8; 11] = [0x04, 0xbc, 0x0a, 0x59, 0x03, 0x04, 0x4f, 0xb8, 0x00, 0x01, 0xac];
/// A constant into a static first: `0 iconst_1  1 putstatic #2
/// 4 aload_0  5 invokestatic #1  8 ireturn`.
const CONST_STATIC: [u8; 9] = [0x04, 0xb3, 0x00, 0x02, 0x2a, 0xb8, 0x00, 0x01, 0xac];

fn outer_call(outer: &[u8]) -> usize {
    outer.iter().position(|&op| op == 0xb8).unwrap()
}

/// `mid` (PURE) spliced at base 100 under `outer`'s call, its own call at 101.
fn admitted(outer: &[u8]) -> Result<u8, ChainRefusal> {
    let mid = SplicedBodyCode {
        method_key: MID.to_string(),
        code: PURE.to_vec(),
        num_params: 1,
        num_locals: 1,
        handlers: Vec::new(),
    };
    let enclosing = [(100usize, PURE.len(), outer_call(outer), MID)];
    spliced_call_replay(101, &enclosing, outer, &[], &[mid], false, false)
}

/// dev's window closure's owned-arguments term at the resolved `invoke`.
fn window_counts_exact(outer: &[u8]) -> bool {
    let bci = u32::try_from(outer_call(outer)).unwrap();
    crate::deopt::owned_args_caller_replay_is_exact(outer, outer.len(), bci, &[])
}

#[test]
fn j27_w8arg_every_admitted_chain_is_an_exact_rerun_point_of_the_window() {
    for outer in [&PURE[..], &FRESH[..]] {
        assert_eq!(admitted(outer), Ok(0xb8), "{outer:02x?}");
        assert!(window_counts_exact(outer), "{outer:02x?}");        }    }