//! Interpreter round i1 wave 26, lane L2
//! (`interpreter-L2-single-pass-frames-describe-no-lock-their-code-took-FIXED-20260928`): the
//! per-pc analysis of the monitors a single-pass body's code takes, on
//! javac 25's own bytecode (`javap -c`, shown beside each array). The
//! real-compile half — the snapshots `build_frame_state_at` publishes from
//! it — is `x64/tests.rs` `w26_single_pass_frames_name_*`.

use super::taken_monitor_slots_per_pc;
use rustc_hash::FxHashMap;
use std::collections::HashSet;

fn slots(
    code: &[u8],
    ranges: &[(usize, usize, usize)],
    elided: &[usize],
) -> FxHashMap<usize, Vec<usize>> {
    let elided: HashSet<usize> = elided.iter().copied().collect();
    taken_monitor_slots_per_pc(code, code.len(), ranges, &[], &elided)
}

/// [`slots`] with the request's per-entry catch-all bits.
fn slots_typed(
    code: &[u8],
    ranges: &[(usize, usize, usize)],
    catch_all: &[bool],
) -> FxHashMap<usize, Vec<usize>> {
    taken_monitor_slots_per_pc(code, code.len(), ranges, catch_all, &HashSet::new())
}

/// `static void plain(Object lock, Runnable r) { synchronized (lock) { r.run(); } }`
#[rustfmt::skip]
const PLAIN: [u8; 21] = [
    0x2a,                         //  0: aload_0
    0x59,                         //  1: dup
    0x4d,                         //  2: astore_2
    0xc2,                         //  3: monitorenter
    0x2b,                         //  4: aload_1
    0xb9, 0x00, 0x07, 0x01, 0x00, //  5: invokeinterface Runnable.run
    0x2c,                         // 10: aload_2
    0xc3,                         // 11: monitorexit
    0xa7, 0x00, 0x08,             // 12: goto 20
    0x4e,                         // 15: astore_3
    0x2c,                         // 16: aload_2
    0xc3,                         // 17: monitorexit
    0x2d,                         // 18: aload_3
    0xbf,                         // 19: athrow
    0xb1,                         // 20: return
];
const PLAIN_TABLE: [(usize, usize, usize); 2] = [(4, 12, 15), (15, 18, 15)];

#[test]
fn a_javac_block_names_its_temp_inside_and_nothing_outside() {
    let at = slots(&PLAIN, &PLAIN_TABLE, &[]);
    for pc in [4, 5, 10, 11, 15, 16, 17] {
        assert_eq!(at.get(&pc), Some(&vec![2]), "pc {pc} is inside the block");
    }
    for pc in [0, 1, 2, 3, 12, 18, 19, 20] {
        assert!(!at.contains_key(&pc), "pc {pc} holds no lock");
    }
}

/// `try { synchronized (lock) { r.run(); } } catch (RuntimeException e) {}`:
/// the `catch` covers the block, and without catch types its handler is
/// reached from both sides of the enter. Inside the block the answer is
/// unchanged; the handler and what follows it get none (which is also the
/// right description there: nothing is held).
#[test]
fn a_try_around_the_block_keeps_the_inside_and_answers_nothing_after_its_handler() {
    let mut code = PLAIN[..20].to_vec();
    code.extend_from_slice(&[
        0xa7, 0x00, 0x04, // 20: goto 24
        0x4d, // 23: astore_2
        0xb1, // 24: return
    ]);
    let at = slots(&code, &[(4, 12, 15), (15, 18, 15), (0, 20, 23)], &[]);
    assert_eq!(at.get(&5), Some(&vec![2]), "the protected invoke");
    assert_eq!(at.get(&17), Some(&vec![2]), "the catch-all's exit");
    for pc in [0, 3, 20, 23, 24] {
        assert!(!at.contains_key(&pc), "pc {pc}");
    }
}

/// `synchronized (a) { synchronized (b) { r.run(); } }`: outermost first.
#[test]
fn nested_blocks_are_listed_outermost_first() {
    #[rustfmt::skip]
    let code: [u8; 42] = [
        0x2a, 0x59, 0x4e, 0xc2,             //  0: aload_0 dup astore_3 monitorenter
        0x2b, 0x59, 0x3a, 0x04, 0xc2,       //  4: aload_1 dup astore 4 monitorenter
        0x2c,                               //  9: aload_2
        0xb9, 0x00, 0x07, 0x01, 0x00,       // 10: invokeinterface
        0x19, 0x04, 0xc3,                   // 15: aload 4; 17: monitorexit
        0xa7, 0x00, 0x0b,                   // 18: goto 29
        0x3a, 0x05, 0x19, 0x04, 0xc3,       // 21: astore 5; aload 4; monitorexit
        0x19, 0x05, 0xbf,                   // 26: aload 5; athrow
        0x2d, 0xc3,                         // 29: aload_3; monitorexit
        0xa7, 0x00, 0x0a,                   // 31: goto 41
        0x3a, 0x06, 0x2d, 0xc3,             // 34: astore 6; aload_3; monitorexit
        0x19, 0x06, 0xbf,                   // 38: aload 6; athrow
        0xb1,                               // 41: return
    ];
    let table = [(9, 18, 21), (21, 26, 21), (4, 31, 34), (34, 38, 34)];
    let at = slots(&code, &table, &[]);
    assert_eq!(at.get(&4), Some(&vec![3]));
    assert_eq!(at.get(&10), Some(&vec![3, 4]), "the invoke holds both");
    assert_eq!(at.get(&21), Some(&vec![3, 4]), "the inner catch-all");
    assert_eq!(at.get(&29), Some(&vec![3]), "after the inner exit");
    assert!(!at.contains_key(&41));
}

/// A store to the temp, an exit that is not `aload t; monitorexit` of the
/// innermost entry, an enter with no `dup; astore` in front: each is
/// answered with nothing from the point where the analysis cannot follow.
#[test]
fn what_the_analysis_cannot_prove_it_does_not_name() {
    // aload_0 dup astore_1 monitorenter; aconst_null astore_1;
    // aload_0 monitorexit; return
    let overwritten = [0x2a, 0x59, 0x4c, 0xc2, 0x01, 0x4c, 0x2a, 0xc3, 0xb1];
    let at = slots(&overwritten, &[], &[]);
    assert_eq!(at.get(&4), Some(&vec![1]));
    assert_eq!(at.get(&5), Some(&vec![1]), "before the store");
    for pc in [6, 7, 8] {
        assert!(
            !at.contains_key(&pc),
            "pc {pc}: slot 1 no longer holds the lock"
        );
    }

    // aload_0 monitorenter aload_0 monitorexit return: no temp.
    assert!(slots(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1], &[], &[]).is_empty());

    // Two enters, then the OUTER one's exit first.
    #[rustfmt::skip]
    let out_of_order = [
        0x2a, 0x59, 0x4d, 0xc2, //  0: aload_0 dup astore_2 monitorenter
        0x2b, 0x59, 0x4e, 0xc2, //  4: aload_1 dup astore_3 monitorenter
        0x2c, 0xc3,             //  8: aload_2 monitorexit (the outer one)
        0x2d, 0xc3,             // 10: aload_3 monitorexit
        0xb1,                   // 12: return
    ];
    let at = slots(&out_of_order, &[], &[]);
    assert_eq!(at.get(&8), Some(&vec![2, 3]));
    assert!(!at.contains_key(&10) && !at.contains_key(&11));
}

/// An elided (scalar) monitor is the scalar lane's to describe; a pc with
/// one open is not answered at all. `jsr` methods are not analysed.
#[test]
fn elided_monitors_and_subroutines_are_left_alone() {
    assert!(slots(&PLAIN, &PLAIN_TABLE, &[3, 11, 17]).is_empty());
    let mut with_jsr = PLAIN.to_vec();
    with_jsr[4] = 0xa8; // jsr at pc 4
    assert!(slots(&with_jsr, &PLAIN_TABLE, &[]).is_empty());
}

// ---------------------------------------------------------------------
// Interpreter round i1 wave 27, lane L2
// (`interpreter-L2-single-pass-monitor-analysis-answers-nothing-in-a-loop-around-a-try-synchronized-FIXED-20260928`):
// a catch-all ends a pc's handler search, so a `try` around a
// `synchronized` block inside a loop no longer merges both sides of the
// enter at its handler.
// ---------------------------------------------------------------------

/// javac 25 for
/// `static void loop(Object lock, Runnable r, int n) { for (int i = 0; i < n; i++) { try { synchronized (lock) { r.run(); } } catch (RuntimeException e) {} } }`
/// (`javap -c`), bytes copied from the class file.
#[rustfmt::skip]
const LOOP: [u8; 44] = [
    0x03, 0x3e,                   //  0: iconst_0; istore_3
    0x1d, 0x1c,                   //  2: iload_3; iload_2   <- loop header
    0xa2, 0x00, 0x27,             //  4: if_icmpge 43
    0x2a, 0x59, 0x3a, 0x04,       //  7: aload_0; dup; astore 4
    0xc2,                         // 11: monitorenter
    0x2b,                         // 12: aload_1
    0xb9, 0x00, 0x07, 0x01, 0x00, // 13: invokeinterface Runnable.run
    0x19, 0x04, 0xc3,             // 18: aload 4; 20: monitorexit
    0xa7, 0x00, 0x0b,             // 21: goto 32
    0x3a, 0x05, 0x19, 0x04, 0xc3, // 24: astore 5; aload 4; 28: monitorexit
    0x19, 0x05, 0xbf,             // 29: aload 5; 31: athrow
    0xa7, 0x00, 0x05,             // 32: goto 37
    0x3a, 0x04,                   // 35: astore 4
    0x84, 0x03, 0x01,             // 37: iinc 3, 1
    0xa7, 0xff, 0xda,             // 40: goto 2
    0xb1,                         // 43: return
];
/// `[12,21)→24 any`, `[24,29)→24 any`, `[7,32)→35 RuntimeException`.
const LOOP_TABLE: [(usize, usize, usize); 3] = [(12, 21, 24), (24, 29, 24), (7, 32, 35)];
const LOOP_CATCH_ALL: [bool; 3] = [true, true, false];

#[test]
fn a_try_synchronized_in_a_loop_is_described_with_catch_all_bits() {
    let at = slots_typed(&LOOP, &LOOP_TABLE, &LOOP_CATCH_ALL);
    for pc in [12, 13, 18, 20, 24, 26, 28] {
        assert_eq!(
            at.get(&pc),
            Some(&vec![4]),
            "pc {pc} is inside the block or its catch-all"
        );
    }
    for pc in [0, 2, 4, 7, 8, 9, 11, 21, 29, 31, 32, 35, 37, 40, 43] {
        assert!(!at.contains_key(&pc), "pc {pc} holds no lock");
    }
}

/// The same bytecode without the bits is the wave-26 answer: the outer
/// handler merges `[]` and `[4]`, and the loop carries that to every pc,
/// the protected invoke included. The negative control for the test
/// above.
#[test]
fn without_catch_all_bits_the_loop_answers_nothing() {
    assert!(slots(&LOOP, &LOOP_TABLE, &[]).is_empty());
    // A length that does not match the table is read as "no bits".
    assert!(slots_typed(&LOOP, &LOOP_TABLE, &[true]).is_empty());
}

/// Table order, not range order: a catch-all listed AFTER an enclosing
/// typed entry does not stop that typed entry's edge (the JVM tries the
/// typed entry first), so the merge at its handler is back.
#[test]
fn a_catch_all_stops_only_the_entries_after_it() {
    let reordered = [LOOP_TABLE[2], LOOP_TABLE[0], LOOP_TABLE[1]];
    let bits = [LOOP_CATCH_ALL[2], LOOP_CATCH_ALL[0], LOOP_CATCH_ALL[1]];
    assert!(slots_typed(&LOOP, &reordered, &bits).is_empty());
}
