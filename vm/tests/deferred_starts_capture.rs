//! Phase 5 fast boot, WP3: a capture-mode process queues its thread starts
//! on the VM's `DeferredStarts`, and `run_deferred_starts` runs them once the
//! process has left `Capture`.
//!
//! Its own test binary, with one test: the image mode is process-wide and
//! one-way (`Off` -> `Capture` -> `Restored`), so no other test may share the
//! process.

use cratonvm_types::image_layout::{self, ImageMode};
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::deferred_starts::run_deferred_starts;
use cratonvm_vm::SharedVm;
use std::sync::{Arc, Mutex};

#[test]
fn a_capture_run_queues_starts_and_the_restore_runs_them_in_order() {
    image_layout::set_mode(ImageMode::Capture);
    // Built while capturing: the heap's own threads (ZUncommitter,
    // ZPrefaulter on a ZGC heap) are queued, not started.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let queued_by_boot = shared.deferred_starts.pending_names();
    for name in &queued_by_boot {
        assert!(
            name == "ZUncommitter" || name == "ZPrefaulter",
            "SharedVm::new deferred an unexpected start {name:?}"
        );
    }

    let order = Arc::new(Mutex::new(Vec::new()));
    let o = Arc::clone(&order);
    assert!(!shared.deferred_starts.start_or_defer("first", move || {
        o.lock().unwrap().push("first");
    }));
    let o = Arc::clone(&order);
    assert!(!shared
        .deferred_starts
        .start_or_defer_with_vm(&shared, "second", move |vm| {
            assert!(
                vm.deferred_starts.pending_len() == 0,
                "drained before running"
            );
            o.lock().unwrap().push("second");
        }));
    let slot = shared
        .deferred_starts
        .start_or_defer_into(&shared, "slot", |_| 42u32);
    assert!(slot.lock().is_none(), "nothing runs while capturing");
    assert!(order.lock().unwrap().is_empty());
    assert_eq!(
        shared.deferred_starts.pending_len(),
        queued_by_boot.len() + 3
    );
    // Still capturing: the queue must survive to P1.
    assert_eq!(run_deferred_starts(&shared), 0);
    assert_eq!(
        shared.deferred_starts.pending_len(),
        queued_by_boot.len() + 3
    );

    image_layout::set_mode(ImageMode::Restored);
    assert_eq!(run_deferred_starts(&shared), queued_by_boot.len() + 3);
    assert_eq!(*order.lock().unwrap(), vec!["first", "second"]);
    assert_eq!(*slot.lock(), Some(42));
    assert_eq!(shared.deferred_starts.pending_len(), 0);
    // After the restore every start runs at once.
    let o = Arc::clone(&order);
    assert!(shared.deferred_starts.start_or_defer("late", move || {
        o.lock().unwrap().push("late");
    }));
    assert_eq!(order.lock().unwrap().last(), Some(&"late"));
    assert_eq!(run_deferred_starts(&shared), 0);
}
