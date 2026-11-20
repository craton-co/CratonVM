// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration test for `CRATONVM_DBG_STALE_OBJREF` (see
//! `gc/src/stale_objref_debug.rs` and
//! wildfly-parallel-boot-stale-objectref-residual.md).
//!
//! Still a SEPARATE test binary (one file under `gc/tests/`, one test
//! function) rather than a `#[test]` inside `gen_heap.rs`'s own module: the
//! flag changes how *every* minor GC in the process recycles its from-space
//! arena, so it has no business being live while unrelated heap tests run.
//!
//! It no longer has to be, for correctness. `CRATONVM_DBG_STALE_OBJREF` is a
//! declared flag served from the process-wide `cratonvm_types::flags`
//! snapshot, which latches on the first read of ANY flag — so the `set_var`
//! this test used to do worked only because it was the sole occupant of its
//! binary, and would have silently become a no-op the day a second test was
//! added to the file. The test installs a `flags::override_process` guard
//! instead, which wins whether or not the snapshot has already latched.

use std::collections::HashMap;

use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, Value};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Test-only `StopTheWorldToken`. This test runs the heap single-threaded,
/// so the STW invariant is trivially satisfied.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: this test runs the heap single-threaded.
    unsafe { StopTheWorldToken::new() }
}

