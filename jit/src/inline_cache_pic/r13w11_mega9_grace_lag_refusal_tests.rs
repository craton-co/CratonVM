//! Round 13 wave 11, lane mega9: `JitPICSlot::install` reports a receiver
//! it could place nowhere while a way it could have taken waits for its
//! grace -- the count proposal M13-4 is gated on. The test holds a JIT
//! execution token across the retirement, the state of a thread spinning
//! in compiled code, so no other test's drain graces it in between.
use super::*;

/// Raw, even entry words (bit 0 is the context tag): published, never called.
fn word(class_id: u32) -> u64 {
    0x5000 + (u64::from(class_id) << 4)
}

/// Two classes whose hashed set is `probe`'s, and two whose set is not.
fn classes(probe: u32) -> ([u32; 2], [u32; 2]) {
    let set = JitPICSlot::mega_base_index(probe);
    let mut same: Vec<u32> = Vec::new();
    let mut other: Vec<u32> = Vec::new();
    for c in (probe + 1)..(probe + 100_000) {
        if JitPICSlot::mega_base_index(c) == set {
            if same.len() < 2 {
                same.push(c);
            }
        } else if other.len() < 2 {
            other.push(c);
        }
        if same.len() == 2 && other.len() == 2 {
            break;
        }
    }
    assert!(same.len() == 2 && other.len() == 2, "the hash spreads");
    ([same[0], same[1]], [other[0], other[1]])
}

#[test]
fn a_receiver_refused_only_for_a_lagging_grace_is_reported() {
    // This one thread plays both the installer and the thread lagging in
    // compiled code. Round 14 wave 7's catch-up self-stamp
    // (`CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP`) would let the installer vouch
    // for itself and lift the refusal this test models, so pin it off.
    crate::THREAD_QUIESCENCE.with(|h| h.ic_catch_up_self_stamp_flag.set(1));
    let execution = crate::jit_execution_enter();
    let probe = 9u32;
    let ([a, b], [c, d]) = classes(probe);
    // Hashed-first slot: A and B fill the probe's hashed set and inline
    // ways 0 and 1; C and D take ways 2 and 3.
    let full = JitPICSlot::new();
    for class in [a, b, c, d] {
        assert!(!full.install(class, "C", word(class), false, false));
    }
    // Every way the probe could take holds a live receiver: refused, but
    // not for the grace.
    assert!(!full.install(probe, "C", word(probe), false, false));
    full.clear_entries();

    let lagging = JitPICSlot::new();
    for class in [a, b, c, d] {
        lagging.install(class, "C", word(class), false, false);
    }
    // A's callee recompiled: its inline way and its hashed way retire and
    // wait for the grace the held token withholds.
    assert!(lagging.retire_entry(word(a) as usize));
    assert!(
        lagging.install(probe, "C", word(probe), false, false),
        "only retired, ungraced ways were left for the probe"
    );
    assert!(lagging.lookup_megamorphic(probe).is_none());
    // A itself may take its ways back at once (same-key refill): not a
    // grace refusal.
    assert!(!lagging.install(a, "C", word(a) + 0x100_0000, false, false));
    lagging.clear_entries();
    crate::jit_execution_leave(execution);
}
