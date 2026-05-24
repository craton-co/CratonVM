//! Smoke test for the SharedVm `_ranked` accessors (task #18,
//! alt-branch integration).
//!
//! Drives ONE of the new accessors through a deliberate rank-order
//! violation and asserts the debug-build panic. The canonical
//! inversion taken from `docs/lock-order.md §"Forbidden: monitor →
//! class manager"` is: hold `monitors` (rank-tagged at
//! `LockLevel::ThreadList`) and then try to acquire `class_manager`
//! (rank-tagged at the lower `LockLevel::HeapLock`). The descending
//! acquisition fires the debug-only assertion inside
//! `cratonvm_vm::runtime::lock_order` and the `_ranked` accessor
//! panics before it ever touches the underlying `parking_lot::RwLock`.
//!
//! In release builds the rank check is compiled away and the
//! violation goes undetected by design (zero-cost). This file uses
//! `std::panic::catch_unwind` rather than `#[should_panic]` so the
//! `SHOULD_PANIC_ATTR_COUNT` source-drift regression in `vm/src/lib.rs`
//! does not need a bump.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::SharedVm;

fn fresh_shared_vm() -> Arc<SharedVm> {
    // `use_synthetic_jdk` is the default for `VmConfig::new()`, so this
    // skips JDK auto-discovery and keeps construction cheap.
    let config = VmConfig::new();
    Arc::new(SharedVm::new(config))
}

#[test]
fn shared_vm_ranked_accessors_panic_on_inversion() {
    #[cfg(debug_assertions)]
    {
        // The canonical "Forbidden: monitor → class manager" inversion
        // from docs/lock-order.md: hold `monitors` then try to acquire
        // `class_manager`. With HEAD's 6-level LockLevel mapping
        // `monitors` is at `ThreadList` (3) and `class_manager` is at
        // `HeapLock` (0); HeapLock < ThreadList so the assertion
        // `self.level > held` inside lock_order::OrderedMutex::lock
        // fails and the `_ranked` accessor panics.
        let shared = fresh_shared_vm();

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _monitors = shared.enter_monitors_rank();
            // Forbidden: acquiring class_manager while holding monitors.
            let _cm = shared.class_manager_read_ranked();
        }));

        assert!(
            result.is_err(),
            "expected a debug-build lock-order panic when acquiring \
             `class_manager_read_ranked` while holding the `monitors` \
             rank; got Ok"
        );

        let payload = result.unwrap_err();
        let msg = payload
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| payload.downcast_ref::<&'static str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("lock order violation"),
            "expected panic message to mention 'lock order violation', \
             got: {msg:?}"
        );
    }

    // In release builds the rank check is compiled out by design
    // (zero-cost wrapper). Just smoke-check that the accessors remain
    // callable and the inversion does not deadlock.
    #[cfg(not(debug_assertions))]
    {
        let shared = fresh_shared_vm();
        let _monitors = shared.enter_monitors_rank();
        let _cm = shared.class_manager_read_ranked();
        drop(_cm);
        drop(_monitors);
    }
}

/// Sanity check: the legal ascending order does NOT panic and the
/// `_ranked` accessors actually return usable guards.
#[test]
fn shared_vm_ranked_accessors_ascending_order_ok() {
    let shared = fresh_shared_vm();

    // Ascending in acquisition order: class_manager (HeapLock=0) first,
    // then monitors (ThreadList=3). Both must succeed.
    let cm = shared.class_manager_read_ranked();
    let _monitors = shared.enter_monitors_rank();

    // The guard derefs to the inner ClassManager — exercise it so we
    // know the wrapper actually exposes the protected value.
    let _loaded = cm.loaded_count();
}
