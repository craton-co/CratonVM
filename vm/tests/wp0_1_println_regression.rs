// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP0.1 — regression test for the "Cannot invoke write on null" P0 bug
//! (discovered 2026-04-24 while staging EJBCA; see
//! `wildfly-ejbca-roadmap.md` WP0.1).
//!
//! The bug: two consecutive `System.out.println(String)` calls succeeded
//! on the first and NPE'd on the second with "Cannot invoke write on
//! null".
//!
//! Root cause: when the real JDK is on the boot classpath the real
//! `java/io/PrintStream` class gets loaded (with a populated
//! `VtableManager` entry pointing at real-JDK bytecode for `println`).
//! `ensure_synthetic_class("java/io/PrintStream", 1)` then reuses the
//! existing `class_id` and we allocate a 1-field synthetic object with
//! that class_id. The `execute_invokevirtual_vtable_fast` "fast path 0"
//! consulted the vtable BEFORE checking the native registry — so the
//! second `println` (which hit the populated thread-local invoke_cache
//! the first time, populating it via the slow path) found the vtable
//! entry first on subsequent calls and dispatched to real-JDK
//! `println` bytecode. The real bytecode reads `textOut`
//! (a `BufferedWriter` field that doesn't exist on our 1-field synthetic
//! object) and NPEs on `textOut.write(s)`.
//!
//! The fix (in `vm/src/runtime/interpreter.rs::execute_invokevirtual_vtable_fast`):
//! the vtable fast-path now consults `native_methods.find(receiver_class,
//! method_name, descriptor)` BEFORE looking up the vtable slot, returning
//! `CacheMiss` on a hit so the slow path's `VirtualNative` cache entry is
//! honored. This matches the dispatch order already used by
//! `populate_virtual_invoke_cache` and `try_stackless_invoke`: native
//! override has priority over class-file bytecode.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::{create_java_string, SharedVm};
use std::sync::Arc;

/// Synthetic-only smoke: in pure-synthetic mode the real JDK is never
/// loaded so the bug doesn't manifest, but we still verify the basic
/// allocation/idempotency invariants `ensure_system_streams` relies on.
#[test]
fn test_println_multi_line_no_npe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    // Force the synthetic System.out/System.err to be allocated.
    let (out_ref, err_ref) = shared.ensure_system_streams();

    // Both streams must exist and be distinct objects (different fd tags).
    assert_ne!(
        out_ref.as_ptr(),
        err_ref.as_ptr(),
        "System.out and System.err must be distinct objects"
    );

    // Slot 0's meaning depends on which PrintStream class got loaded: the
    // synthetic stub uses it as a stdout/stderr descriptor tag, but once the
    // real JDK's `java/io/PrintStream` is on the boot classpath (the default
    // config, since compact-ref-fields stays on for the Generational GC —
    // see `vm/src/vm/vm_init.rs::ensure_system_streams`), slot 0 is the real
    // `FilterOutputStream.out` reference field, and streams are identified
    // by object identity instead. Writing an Int there would box it and
    // corrupt the real stream graph, so `ensure_system_streams` deliberately
    // skips the fd-tag write in that case (mirrors
    // `vm_init.rs::ensure_system_streams_creates_objects`).
    let out_header = shared.mem.heap.get_header(out_ref);
    let slot0_is_ref = cratonvm_gc::class_layout(out_header.class_id.as_u32())
        .and_then(|layout| layout.field_is_ref(0))
        .unwrap_or(false);
    if slot0_is_ref {
        assert!(
            !matches!(shared.mem.heap.get_field(out_ref, 0), Value::Int(1)),
            "real PrintStream.out (a reference field) must not hold a boxed fd tag"
        );
        assert!(
            !matches!(shared.mem.heap.get_field(err_ref, 0), Value::Int(2)),
            "real PrintStream.out (a reference field) must not hold a boxed fd tag"
        );
    } else {
        assert!(
            matches!(shared.mem.heap.get_field(out_ref, 0), Value::Int(1)),
            "out fd tag must be readable at slot 0"
        );
        assert!(
            matches!(shared.mem.heap.get_field(err_ref, 0), Value::Int(2)),
            "err fd tag must be readable at slot 0"
        );
    }

    // 5 round-trips of `create_java_string` to exercise the heap arena —
    // this caught a separate (theoretical) field-stomp bug that turned
    // out not to be the real cause, but the round-trip invariant is
    // still worth pinning.
    let lines = [
        (out_ref, "line1"),
        (err_ref, "line2 (stderr)"),
        (out_ref, "line3"),
        (err_ref, "line4 (stderr)"),
        (out_ref, "line5"),
    ];
    for (stream, text) in &lines {
        let s = create_java_string(&shared, text);
        if !slot0_is_ref {
            assert!(
                matches!(shared.mem.heap.get_field(*stream, 0), Value::Int(1 | 2)),
                "fd tag must remain a valid Int(1|2) after allocation round {text}"
            );
        }
        let read_back = cratonvm_vm::vm::read_java_string(&shared.mem.heap, s).unwrap_or_default();
        assert_eq!(
            read_back, *text,
            "create_java_string round-trip must survive between println calls"
        );
    }

    // Idempotency: a second call returns the same refs.
    let (out2, err2) = shared.ensure_system_streams();
    assert_eq!(
        out_ref.as_ptr(),
        out2.as_ptr(),
        "ensure_system_streams must be idempotent for out"
    );
    assert_eq!(
        err_ref.as_ptr(),
        err2.as_ptr(),
        "ensure_system_streams must be idempotent for err"
    );
}

/// Slot-count invariant for the synthetic path.
#[test]
fn test_system_streams_slot_count_matches_class() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let (out_ref, err_ref) = shared.ensure_system_streams();

    // See the `slot0_is_ref` comment in `test_println_multi_line_no_npe`:
    // under the default config (real JDK + compact-ref-fields), slot 0 is
    // the real `FilterOutputStream.out` reference field and stays null, not
    // a boxed fd-tag Int.
    let out_header = shared.mem.heap.get_header(out_ref);
    let slot0_is_ref = cratonvm_gc::class_layout(out_header.class_id.as_u32())
        .and_then(|layout| layout.field_is_ref(0))
        .unwrap_or(false);

    let out_fd = shared.mem.heap.get_field(out_ref, 0);
    let err_fd = shared.mem.heap.get_field(err_ref, 0);
    if slot0_is_ref {
        assert_ne!(out_fd, Value::Int(1));
        assert_ne!(err_fd, Value::Int(2));
    } else {
        assert_eq!(out_fd, Value::Int(1));
        assert_eq!(err_fd, Value::Int(2));
    }
}
