// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration test for the `CRATONVM_DBG_STALE_OBJREF_CYCLES` quarantine
//! ring (see `gc/src/stale_objref_debug.rs::quarantine_cycles` and the
//! `quarantine` field in `gc/src/gen_heap.rs`).
//!
//! A stale raw `ObjectRef` read exactly one minor GC after its object moved
//! is caught by the original single-cycle quarantine, but the WildFly
//! `parallel-extension-add` CCE family reads its stale references two or
//! more cycles late — by which point the single-cycle quarantine has already
//! recycled the arena and the read silently resolves to reused memory. The
//! ring keeps each evacuated from-space intact for N cycles so those late
//! reads still hit the loud forwarded-header panic.
//!
//! Still its own test binary — both flags change how every minor GC in the
//! process recycles its arenas — but no longer for correctness: the flags are
//! installed through `flags::override_process`, which wins over the
//! process-wide snapshot whether or not something has already latched it. See
//! the sibling `stale_objref_debug_assertion.rs` module comment.

use std::collections::HashMap;

use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, Value};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: this test runs the heap single-threaded.
    unsafe { StopTheWorldToken::new() }
}

#[test]
fn stale_read_three_cycles_late_is_still_caught() {
    // Held for the whole test: `gc_flags()` reads the snapshot live, so the
    // 4-cycle quarantine ring is in force exactly while this guard is.
    let _quarantine_ring = cratonvm_types::flags::override_process(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[
            ("CRATONVM_DBG_STALE_OBJREF", Some("1")),
            ("CRATONVM_DBG_STALE_OBJREF_CYCLES", Some("4")),
        ]),
    );

    let heap = GenerationalHeap::with_sizes(4 * 1024, 8 * 1024);
    let monitors = NoMonitors;

    let stale = heap.alloc_object(ClassId::new(1), 2);
    heap.set_field(stale, 0, Value::Int(7));

    // Cycle 1: the object survives and moves; `stale` now dangles.
    let mut roots = vec![stale];
    let result = heap.collect_garbage(&stw(), &mut roots, &monitors);
    assert_eq!(result.stats.objects_copied, 1);
    let fresh = roots[0];
    assert_ne!(fresh.as_ptr(), stale.as_ptr());

    // Cycles 2 and 3: two MORE minor GCs, tracking the live object through
    // each cycle's updated root (it may be copied again or promoted to old
    // gen — either keeps it alive; only `stale`'s original arena matters
    // here). With the original single-cycle quarantine the arena holding
    // `stale`'s forwarding marker would have been recycled during cycle 2,
    // and the read below would silently resolve to reused memory. The
    // 4-cycle ring must keep it intact.
    let mut live = fresh;
    for _ in 0..2 {
        let mut rs = vec![live];
        let _ = heap.collect_garbage(&stw(), &mut rs, &monitors);
        live = rs[0];
    }
    assert_eq!(
        heap.get_field(live, 0).as_int(),
        Some(7),
        "the live object must remain readable through the updated root"
    );

    // The stale read, three cycles late, must still be caught loudly.
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = heap.get_header(stale);
    }));
    assert!(
        caught.is_err(),
        "a stale ObjectRef read 3 minor GCs after its object moved must \
         still panic while the quarantine ring (CYCLES=4) holds its arena"
    );
}
