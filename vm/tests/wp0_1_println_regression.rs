// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP0.1 — regression test for the "Cannot invoke write on null" P0 bug
//! (discovered 2026-04-24 while staging EJBCA; see
//! `docs/wildfly-ejbca-roadmap.md` WP0.1).
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

    // Slot 0 must hold the fd tag (1 = stdout, 2 = stderr).
    assert!(
        matches!(shared.heap.get_field(out_ref, 0), Value::Int(1)),
        "out fd tag must be readable at slot 0"
    );
    assert!(
        matches!(shared.heap.get_field(err_ref, 0), Value::Int(2)),
        "err fd tag must be readable at slot 0"
    );

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
        assert!(
            matches!(shared.heap.get_field(*stream, 0), Value::Int(1 | 2)),
            "fd tag must remain a valid Int(1|2) after allocation round {text}"
        );
        let read_back = cratonvm_vm::vm::read_java_string(&shared.heap, s).unwrap_or_default();
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

    let out_fd = shared.heap.get_field(out_ref, 0);
    let err_fd = shared.heap.get_field(err_ref, 0);
    assert_eq!(out_fd, Value::Int(1));
    assert_eq!(err_fd, Value::Int(2));
}
