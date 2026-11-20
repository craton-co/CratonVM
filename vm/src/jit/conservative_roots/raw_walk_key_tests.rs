//! 2026-10-07 (throwfill retirement): the raw walk key's pass over a
//! synthetic stack band -- what it keys, what it checks instead of
//! keying, and that a changed word or a band it can no longer read
//! answers "not the same walk".
use super::*;

/// A band of 64 words with an RBP chain `rbp[0] -> rbp[1] -> rbp[2]`
/// (frames 16 words apart), each frame's return address beside its
/// saved RBP, and one chain entry whose innermost frame is `rbp[0]`.
fn band() -> (Box<[usize; 64]>, JitFrameChainEntry) {
    let mut words = Box::new([0usize; 64]);
    let base = words.as_ptr() as usize;
    let rbp = |i: usize| base + 8 * (8 + 16 * i);
    for i in 0..3 {
        words[8 + 16 * i] = rbp(i + 1); // saved RBP: the parent frame
        words[8 + 16 * i + 1] = 0x7000_0000 + 0x100 * i; // return address
    }
    let entry = JitFrameChainEntry {
        entry_sp: base + 8 * 64,
        precise: Some(PreciseFrameInfo {
            compiled_method: std::ptr::null(),
            frame_base: 0,
            entry_ptr: std::ptr::null(),
            exact_rbp: rbp(0),
            exact_cm_id: 7,
            not_entrant_ret_pc: 0,
        }),
        interp_depth: 3,
        depth_stripe: cratonvm_types::striped_counter::StripeToken::UNSET,
        exec_token: cratonvm_jit::JitExecutionToken::UNSET,
        tally_skipped: false,
    };
    (words, entry)
}

fn record(
    chain: &[JitFrameChainEntry],
    scanner_sp: usize,
    plan: &[(usize, bool)],
) -> Vec<usize> {
    let mut out = Vec::new();
    assert!(raw_walk_pass(
        chain,
        scanner_sp,
        plan,
        &mut |w| {
            out.push(w);
            true
        },
        &mut |_| true,
    ));
    out
}

fn replays(
    chain: &[JitFrameChainEntry],
    scanner_sp: usize,
    plan: &[(usize, bool)],
    words: &[usize],
    not_code: &mut dyn FnMut(usize) -> bool,
) -> bool {
    let mut recorded = words.iter().copied();
    raw_walk_pass(
        chain,
        scanner_sp,
        plan,
        &mut |w| recorded.next() == Some(w),
        not_code,
    ) && recorded.next().is_none()
}

#[test]
fn the_pass_keys_the_reads_the_plan_names_and_replays_equal() {
    let (band, entry) = band();
    let sp = band.as_ptr() as usize;
    let chain = [entry];
    let plan = [(3, true)];
    let words = record(&chain, sp, &plan);
    // Header (2 + 6 words), three parents, two keyed return addresses
    // (the last read's is checked, not keyed), the read count, the
    // chain length.
    assert_eq!(words.len(), 8 + 3 + 2 + 2);
    assert!(replays(&chain, sp, &plan, &words, &mut |_| true));
}

#[test]
fn the_boundary_return_address_is_checked_not_keyed() {
    let (mut band, entry) = band();
    let sp = band.as_ptr() as usize;
    let chain = [entry];
    let plan = [(3, true)];
    let words = record(&chain, sp, &plan);
    // Another path into compiled code: the boundary's return address
    // changes, the walk does not.
    band[8 + 32 + 1] = 0x1234_5678;
    let mut asked = Vec::new();
    assert!(replays(&chain, sp, &plan, &words, &mut |ret| {
        asked.push(ret);
        true
    }));
    assert_eq!(asked, [0x1234_5678]);
    // ... unless it now resolves to a compiled body.
    assert!(!replays(&chain, sp, &plan, &words, &mut |_| false));
}

#[test]
fn a_changed_keyed_word_or_an_unreadable_band_is_another_walk() {
    let (mut band, entry) = band();
    let sp = band.as_ptr() as usize;
    let chain = [entry];
    let plan = [(3, true)];
    let words = record(&chain, sp, &plan);
    let saved = band[8 + 16 + 1];
    band[8 + 16 + 1] = saved + 0x801; // an unrolled copy's call site
    assert!(!replays(&chain, sp, &plan, &words, &mut |_| true));
    band[8 + 16 + 1] = saved;
    assert!(replays(&chain, sp, &plan, &words, &mut |_| true));
    // A scanner above the innermost frame cannot read what the walk read.
    assert!(!replays(&chain, sp + 8 * 40, &plan, &words, &mut |_| true));
    // A plan for another chain shape is refused outright.
    assert!(!replays(
        &chain,
        sp,
        &[(3, true), (0, false)],
        &words,
        &mut |_| true
    ));
}
