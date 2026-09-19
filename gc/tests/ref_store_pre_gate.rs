// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The compiled-reference-store SATB pre-gate must track its sources, not sit
//! armed.
//!
//! **Its own test binary, deliberately.** The gate and the marker count behind
//! it are process-global, and the `gc` unit-test binary runs ~1800 tests in
//! parallel with dozens of heaps in mark phases at any instant — a census
//! taken there read 29 markers armed. The assertion that matters here is what
//! the gate reads when NOTHING is marking, and that state simply does not
//! occur in that binary. A test that cannot reach the state it is about is a
//! vacuous pass, so this one gets a process to itself.

use cratonvm_gc::{
    jit_ref_store_armed_markers, jit_ref_store_gate_addrs, publish_jit_ref_store_plan_masked,
    ConcurrentGcPhase, ConcurrentGcState, GenerationalHeap,
};

/// Read the published `pre_active` byte the way compiled code does.
fn read_pre_gate() -> u8 {
    let (pre, _post, _floor) = jit_ref_store_gate_addrs();
    assert_ne!(pre, 0, "the pre gate must be published");
    // SAFETY: `pre` is the address of a `'static` atomic byte inside the gc
    // crate's published gate block, which lives for the whole process.
    unsafe { std::ptr::read_volatile(pre as *const u8) }
}

/// A published plan arms the SATB pre-gate only while a marker is in a phase
/// whose barrier must run.
///
/// The pair, because only the pair separates a working gate from a constant.
/// `publish_jit_ref_store_plan*` used to store a hard `1` here, on the
/// reasoning that a plan should start armed and be relaxed by a later
/// publisher call. There is no later call: `pre_active` is recomputed only
/// when a marker arms or disarms, so a run whose collector never enters a
/// concurrent mark phase left the gate armed for the life of the process.
/// Every compiled reference store then took the full `jit_putfield_object`
/// path — a run-time census of 16,384,000 stores in `RefStoreLoopProbe` found
/// `inline=0`, every one of them bailing at this gate, on Generational and ZGC
/// alike — while the compile-time census reported the fast path as engaged at
/// both sites. That gap between "emitted" and "executed" is the whole reason
/// the barrier plan measured no throughput change when it landed.
#[test]
fn a_published_plan_arms_the_satb_gate_only_while_a_marker_is() {
    let _heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    assert_eq!(
        jit_ref_store_armed_markers(),
        0,
        "nothing may be marking at the start of this test, or the reading below \
         is not the one under test"
    );
    assert_eq!(
        read_pre_gate(),
        0,
        "with nothing marking, a freshly published plan must leave the SATB \
         gate PERMISSIVE — armed here is a gate that no later call ever lowers"
    );

    // The real caller, not the counter directly: `set_phase` is what a
    // collector entering concurrent mark goes through, and it arms BEFORE the
    // phase becomes observable and disarms AFTER it stops being.
    let state = ConcurrentGcState::new();
    state.set_phase(ConcurrentGcPhase::ConcurrentMark);
    assert_eq!(
        read_pre_gate(),
        1,
        "a collector in concurrent mark must arm the gate — the store it would \
         otherwise let through is one whose overwritten reference belongs in \
         the snapshot"
    );

    // Re-publishing mid-mark must not lower it. Publication reads the
    // disjunction of its sources; it does not get to overrule them in either
    // direction.
    publish_jit_ref_store_plan_masked(cratonvm_types::GC_FLAG_OLD_GEN);
    assert_eq!(
        read_pre_gate(),
        1,
        "publishing a plan while a marker is armed must keep the gate armed"
    );

    state.set_phase(ConcurrentGcPhase::Idle);
    assert_eq!(
        read_pre_gate(),
        0,
        "the gate must come back down when the last marker leaves"
    );
    assert_eq!(
        jit_ref_store_armed_markers(),
        0,
        "and the marker count with it — an unmatched arm would pin the gate for \
         the life of the process, which is the failure this test exists for"
    );
}
