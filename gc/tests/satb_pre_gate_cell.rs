// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! t1 lane R: the code-adjacent copy of the SATB pre-gate byte
//! (`install_jit_satb_pre_gate_cell`) is named by the gate address handed to
//! compilers and tracks the same disjunction as the `static` byte.
//!
//! Its own test binary, deliberately: the copy is installed once per process
//! (first install wins), and the `gc` unit-test binary must keep seeing the
//! static (its tests compare the plan against it), and the marker count is
//! process-global (see `ref_store_pre_gate.rs`).

use std::sync::atomic::{AtomicU8, Ordering};

use cratonvm_gc::{
    install_jit_satb_pre_gate_cell, jit_ref_store_armed_markers, jit_satb_pre_gate_addr,
    GenerationalHeap, SatbQueue, JIT_REF_STORE_GATES,
};

#[test]
fn the_installed_cell_is_the_published_pre_gate_and_follows_the_markers() {
    let static_addr = &JIT_REF_STORE_GATES.pre_active as *const AtomicU8 as usize;
    assert_eq!(jit_satb_pre_gate_addr(), static_addr, "nothing installed yet");
    assert_eq!(jit_ref_store_armed_markers(), 0);

    let cell: &'static AtomicU8 = Box::leak(Box::new(AtomicU8::new(0)));
    assert!(install_jit_satb_pre_gate_cell(cell));
    let cell_addr = cell as *const AtomicU8 as usize;
    assert_eq!(jit_satb_pre_gate_addr(), cell_addr);
    // Re-installing the same cell is a no-op success; another cell loses.
    assert!(install_jit_satb_pre_gate_cell(cell));
    let other: &'static AtomicU8 = Box::leak(Box::new(AtomicU8::new(0)));
    assert!(!install_jit_satb_pre_gate_cell(other));
    assert_eq!(jit_satb_pre_gate_addr(), cell_addr);

    // The install refresh left the copy at the true disjunction (nothing
    // marking: permissive), not at the armed value it was seeded with.
    assert_eq!(cell.load(Ordering::SeqCst), 0);

    // A generational heap's plan names the copy.
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    let (pre, _post, _floor, _mask) = heap.jit_ref_store_plan();
    if pre != 0 {
        assert_eq!(pre, cell_addr, "the plan must name the code-adjacent copy");
    }

    let q = SatbQueue::new();
    q.activate();
    assert_eq!(cell.load(Ordering::SeqCst), 1, "an active queue arms the copy");
    assert_eq!(JIT_REF_STORE_GATES.pre_active.load(Ordering::SeqCst), 1);
    q.deactivate_and_discard();
    assert_eq!(cell.load(Ordering::SeqCst), 0, "and the copy comes back down");
    assert_eq!(JIT_REF_STORE_GATES.pre_active.load(Ordering::SeqCst), 0);
}
