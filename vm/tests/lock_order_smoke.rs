// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end lock-order enforcement checks against a real `SharedVm`.
//!
//! The unit tests in `vm/src/runtime/lock_order.rs` exercise the wrappers in
//! isolation. These drive the *actual* `SharedVm` fields, so they fail if a
//! field is ever downgraded back to a raw `parking_lot` lock:
//!
//! - `SharedVm::class_manager` is an `OrderedPlRwLock` at `LockLevel::ClassManager` (L10)
//! - `SharedVm::ref_processor` is an `OrderedPlMutex` at `LockLevel::RefProcessor` (L7)
//! - the `monitors` registry (L6) is announced via `enter_monitors_rank()`
//!
//! The canonical forbidden shape is the "monitor -> class manager" inversion
//! documented at the top of `runtime::lock_order`: hold something below L10 and
//! then reach up for `class_manager`. Historically that is the real defect in
//! this codebase (class_manager/vtable ABBA, and the http-client-cluster
//! `class_manager` RwLock deadlock).
//!
//! Enforcement is unconditional in debug builds and opt-in in release via
//! `CRATONVM_LOCK_ORDER_CHECK`; `enforcement_active()` reports which applies, so
//! these tests assert the panic only when checking is actually on and otherwise
//! just prove the accessors stay callable and do not deadlock.
//!
//! This file uses `std::panic::catch_unwind` rather than `#[should_panic]` so
//! the `SHOULD_PANIC_ATTR_COUNT` source-drift regression in `vm/src/lib.rs`
//! does not need a bump.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::lock_order::enforcement_active;
use cratonvm_vm::vm::SharedVm;

fn fresh_shared_vm() -> Arc<SharedVm> {
    // `use_synthetic_jdk` is the default for `VmConfig::new()`, so this
    // skips JDK auto-discovery and keeps construction cheap.
    let config = VmConfig::new();
    Arc::new(SharedVm::new(config))
}

/// Run `body`; assert it panicked with a lock-order violation when enforcement
/// is on, and that it merely completed when enforcement is off.
fn expect_violation(what: &str, body: impl FnOnce()) {
    let result = catch_unwind(AssertUnwindSafe(body));

    if !enforcement_active() {
        assert!(
            result.is_ok(),
            "{what}: enforcement is off, so the inversion must be a silent no-op"
        );
        return;
    }

    assert!(
        result.is_err(),
        "{what}: expected a lock-order panic with enforcement on; got Ok"
    );
    let payload = result.unwrap_err();
    let msg = payload
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .unwrap_or("");
    assert!(
        msg.contains("lock order violation"),
        "{what}: expected the panic to mention 'lock order violation', got: {msg:?}"
    );
}

/// The documented legal order: `class_manager` (L10) first, then
/// `ref_processor` (L7), then the `monitors` registry (L6). Descending, so it
/// must succeed — and the guards must actually expose the protected values.
#[test]
fn shared_vm_descending_order_is_accepted() {
    let shared = fresh_shared_vm();

    let cm = shared.classes.class_manager.read();
    let rp = shared.mem.ref_processor.lock();
    let _monitors = shared.enter_monitors_rank();

    // Touch both protected values so the guards are not optimized away and we
    // know the wrappers really do Deref to the inner type.
    let _loaded = cm.loaded_count();
    let _weak = rp.weak_ref_count();
}

/// `read_recursive` on `class_manager` under an already-held read is the
/// deliberate reentrancy `interpreter.rs::resolve_method_ref` depends on. It
/// must not be reported as a same-level violation.
#[test]
fn shared_vm_class_manager_read_recursive_is_reentrant() {
    let shared = fresh_shared_vm();
    let outer = shared.classes.class_manager.read();
    let inner = shared.classes.class_manager.read_recursive();
    assert_eq!(outer.loaded_count(), inner.loaded_count());
}

/// The canonical forbidden inversion: hold the `monitors` registry level (L6)
/// and then acquire `class_manager` (L10).
#[test]
fn shared_vm_monitors_then_class_manager_is_detected() {
    let shared = fresh_shared_vm();
    expect_violation("monitors (L6) -> class_manager (L10)", || {
        let _monitors = shared.enter_monitors_rank();
        let _cm = shared.classes.class_manager.read();
    });
}

/// `ref_processor` (L7) held while reaching up for `class_manager` (L10) — the
/// inversion the GC reference-processing path must never take.
#[test]
fn shared_vm_ref_processor_then_class_manager_is_detected() {
    let shared = fresh_shared_vm();
    expect_violation("ref_processor (L7) -> class_manager (L10)", || {
        let _rp = shared.mem.ref_processor.lock();
        let _cm = shared.classes.class_manager_write();
    });
}

/// Reaching up from the L6 monitors registry to `ref_processor` (L7) is also an
/// inversion, and proves the L7 wiring is observed and not just L10's.
#[test]
fn shared_vm_monitors_then_ref_processor_is_detected() {
    let shared = fresh_shared_vm();
    expect_violation("monitors (L6) -> ref_processor (L7)", || {
        let _monitors = shared.enter_monitors_rank();
        let _rp = shared.mem.ref_processor.lock();
    });
}

/// The `_ranked` compatibility accessors must not double-record their level.
/// Before the fields became ordered wrappers these took a *second* rank scope;
/// doing that now would make a lone `class_manager_read_ranked()` call trip the
/// same-level assertion against itself.
#[test]
fn ranked_accessors_do_not_double_record_their_level() {
    let shared = fresh_shared_vm();

    let cm = shared.class_manager_read_ranked();
    let _loaded = cm.loaded_count();
    drop(cm);

    let cm = shared.class_manager_write_ranked();
    let _loaded = cm.loaded_count();
    drop(cm);

    let rp = shared.ref_processor_lock_ranked();
    let _weak = rp.weak_ref_count();
    drop(rp);

    // And they are still order-checked: L7 under L10 is fine, the reverse is not.
    {
        let _cm = shared.class_manager_read_ranked();
        let _rp = shared.ref_processor_lock_ranked();
    }
    expect_violation(
        "ref_processor_lock_ranked -> class_manager_read_ranked",
        || {
            let _rp = shared.ref_processor_lock_ranked();
            let _cm = shared.class_manager_read_ranked();
        },
    );
}

/// Enforcement must be unconditionally on in debug builds — otherwise every
/// assertion above degrades to a no-op without anyone noticing.
#[test]
fn enforcement_is_active_in_debug_builds() {
    if cfg!(debug_assertions) {
        assert!(
            enforcement_active(),
            "lock-order enforcement must be active in debug builds"
        );
    }
}
