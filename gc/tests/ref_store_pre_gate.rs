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
    jit_ref_store_armed_markers, publish_jit_ref_store_plan_masked, ConcurrentGcPhase,
    ConcurrentGcState, GenerationalHeap, SatbQueue,
};

/// Read the published `pre_active` byte the way compiled code does.
///
/// gen r4w4/cards4: through the HEAP's plan (`GenerationalHeap::jit_ref_store_plan`),
/// which is what `build_helpers_for` hands a generational VM's compiled code.
/// The generational collector no longer publishes its plan into the process
/// table; the pre byte it names is still the process-wide marker disjunction.
fn read_pre_gate(heap: &GenerationalHeap) -> u8 {
    let (pre, _post, _floor, _mask) = heap.jit_ref_store_plan();
    assert_ne!(pre, 0, "the pre gate must be published");
    // SAFETY: `pre` is the address of a `'static` atomic byte inside the gc
    // crate's process gate block, which lives for the whole process.
    unsafe { std::ptr::read_volatile(pre as *const u8) }
}

/// A published plan arms the SATB pre-gate only while an SATB queue accepts
/// logs (gc-common w5-e: the queue, no longer the marker's phase).
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
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    assert_eq!(
        jit_ref_store_armed_markers(),
        0,
        "nothing may be marking at the start of this test, or the reading below \
         is not the one under test"
    );
    assert_eq!(
        read_pre_gate(&heap),
        0,
        "with nothing marking, a freshly published plan must leave the SATB \
         gate PERMISSIVE — armed here is a gate that no later call ever lowers"
    );

    // gc-common w5-e (`common-g-proposal-one-satb-gate`, finished): the phase
    // no longer arms anything. It is the marker's business; the barriers of
    // both collectors, and the compiled-code gate, follow the SATB QUEUE.
    let state = ConcurrentGcState::new();
    state.set_phase(ConcurrentGcPhase::ConcurrentMark);
    assert_eq!(
        read_pre_gate(&heap),
        0,
        "the phase alone must not arm the gate: the queue is its one \
         Generational/G1 source now"
    );
    assert_eq!(jit_ref_store_armed_markers(), 0);
    state.set_phase(ConcurrentGcPhase::Idle);

    // The real caller, not the counter directly: a collector entering
    // concurrent mark activates its SATB queue, and the queue arms BEFORE it
    // stores ACTIVE and disarms AFTER it stores INACTIVE.
    let q = SatbQueue::new();
    q.activate();
    assert_eq!(
        read_pre_gate(&heap),
        1,
        "an active SATB queue must arm the gate — the store it would \
         otherwise let through is one whose overwritten reference belongs in \
         the snapshot"
    );

    // Re-publishing mid-mark must not lower it. Publication reads the
    // disjunction of its sources; it does not get to overrule them in either
    // direction.
    publish_jit_ref_store_plan_masked(cratonvm_types::GC_FLAG_OLD_GEN);
    assert_eq!(
        read_pre_gate(&heap),
        1,
        "publishing a plan while a queue is active must keep the gate armed"
    );

    q.deactivate_and_discard();
    assert_eq!(
        read_pre_gate(&heap),
        0,
        "the gate must come back down when the last queue closes"
    );
    assert_eq!(
        jit_ref_store_armed_markers(),
        0,
        "and the marker count with it — an unmatched arm would pin the gate for \
         the life of the process, which is the failure this test exists for"
    );
}
