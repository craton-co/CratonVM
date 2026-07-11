// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration test for `CRATONVM_DBG_STALE_OBJREF` (see
//! `gc/src/stale_objref_debug.rs` and
//! docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md).
//!
//! Deliberately a SEPARATE test binary (one file under `gc/tests/`, one test
//! function) rather than a `#[test]` inside `gen_heap.rs`'s own module: the
//! flag is read through a process-wide `OnceLock` that latches permanently
//! on first read (matching every other `CRATONVM_DBG_*` flag in this
//! codebase — see `vm/src/runtime/env_cache.rs`'s module doc comment).
//! `cargo test` runs every `#[test]` within one file's binary in the SAME
//! process, so if this lived alongside `gen_heap.rs`'s existing (much
//! larger) test module, an earlier test could touch `get_header`/
//! `collect_garbage_inner` first and permanently latch the flag to `false`
//! before `set_var` below ever runs. A dedicated single-test file gets its
//! own fresh process, guaranteeing `set_var` runs before the first read.

use std::collections::HashMap;

use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, Value};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &HashMap<usize, usize>) {}
}

/// Test-only `StopTheWorldToken`. This test runs the heap single-threaded,
/// so the STW invariant is trivially satisfied.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: this test runs the heap single-threaded.
    unsafe { StopTheWorldToken::new() }
}

#[test]
fn stale_native_objref_is_caught_after_evacuation() {
    // Must run before ANY other code in this process reads the flag (see
    // the module doc comment above for why this has to be its own binary).
    std::env::set_var("CRATONVM_DBG_STALE_OBJREF", "1");

    let heap = GenerationalHeap::with_sizes(4 * 1024, 8 * 1024);
    let monitors = NoMonitors;

    // Allocate an object and simulate exactly the bug this whole
    // investigation is about: a native local (`stale`) captures the raw
    // `ObjectRef`, then something (here, an explicit minor GC standing in
    // for any GC-triggering `ctx.*` call) moves it, and the native code
    // goes on to reuse the ORIGINAL local instead of re-reading through
    // `pin_native_root`/`read_native_pin`.
    let stale = heap.alloc_object(ClassId::new(1), 2);
    heap.set_field(stale, 0, Value::Int(42));
    heap.set_field(stale, 1, Value::Long(100));

    let mut roots = vec![stale];
    let result = heap.collect_garbage(&stw(), &mut roots, &monitors);
    assert_eq!(result.stats.objects_copied, 1, "the object must survive the GC");
    let fresh = roots[0];
    assert_ne!(
        fresh.as_ptr(),
        stale.as_ptr(),
        "a real minor GC must relocate the survivor"
    );

    // Legitimate access through the freshly-updated root must still work
    // normally -- the assertion must not false-positive on ordinary,
    // correctly-remapped references.
    assert_eq!(heap.get_field(fresh, 0).as_int(), Some(42));
    assert_eq!(heap.get_field(fresh, 1).as_long(), Some(100));
    assert_eq!(heap.get_header(fresh).gc_age, 1);

    // The OLD `stale` local -- exactly the kind of raw ObjectRef a native
    // function would still be holding if it forgot to pin it -- must be
    // caught deterministically now, instead of silently reading back
    // whatever the evacuated memory looks like.
    let stale_ptr = stale.as_ptr() as usize;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        heap.get_header(stale)
    }));
    let err = result.expect_err(
        "get_header on a stale, evacuated-but-still-quarantined ObjectRef must panic",
    );
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(
        msg.contains("CRATONVM_DBG_STALE_OBJREF"),
        "panic message should name the flag/mechanism, got: {msg:?} (stale addr 0x{stale_ptr:x})"
    );

    // A second full minor GC lets the stale object's quarantine window
    // elapse (one full extra cycle, per the `quarantine` field's contract)
    // and reclaims its memory for real -- confirming the quarantine dance
    // does not wedge or leak, and that the GC keeps working normally on
    // later cycles with the flag still enabled.
    let another = heap.alloc_object(ClassId::new(2), 0);
    let mut roots2 = vec![fresh, another];
    let result2 = heap.collect_garbage(&stw(), &mut roots2, &monitors);
    assert_eq!(
        result2.stats.objects_copied, 2,
        "both survivors must be copied by the second minor GC"
    );
}
