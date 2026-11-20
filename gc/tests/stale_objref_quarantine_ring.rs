// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

