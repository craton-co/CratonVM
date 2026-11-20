//! gci i47w lane oz47: `ClassManagerWriteGuard::run_before_hooks` runs its
//! work with the class-manager write lock already released, once, at the
//! guard's drop (`SharedVm::load_class_concurrent_for` clears its per-name
//! loading flag there).
use crate::vm::SharedVm;
use crate::VmConfig;

#[test]
fn gci_i47w_oz47_before_hooks_work_runs_after_the_write_lock_is_released() {
    let shared = SharedVm::new(VmConfig::default());
    let ran = std::cell::Cell::new(0u32);
    {
        let mut guard = shared.classes.class_manager_write();
        guard.run_before_hooks(|| {
            assert!(
                shared.classes.class_manager.try_read().is_some(),
                "the write lock is released before the work runs"
            );
            ran.set(ran.get() + 1);
        });
        assert_eq!(ran.get(), 0, "nothing runs while the guard is held");
    }
    assert_eq!(ran.get(), 1, "runs exactly once, at the drop");
    // A guard that set nothing releases as before.
    drop(shared.classes.class_manager_write());
    assert!(shared.classes.class_manager.try_write().is_some());
}
