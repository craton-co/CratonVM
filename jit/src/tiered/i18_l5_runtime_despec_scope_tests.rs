use super::{AtomicU32, AtomicU64, ClassId, TieredCompilationManager};
use crate::compile_gate::RuntimeDespecRegistry;
use std::sync::Arc;

/// Another VM's class with the same `ClassId` keeps its verdict when this
/// VM unloads or redefines its own class under that id.
#[test]
fn a_class_unload_forgets_only_in_the_managers_own_vm() {
    let flush = Arc::new(AtomicU64::new(1));
    let redefine = Arc::new(AtomicU32::new(0));
    let own = RuntimeDespecRegistry::new_registered_with_vm_epochs(
        Arc::clone(&flush),
        Arc::clone(&redefine),
    );
    let other = RuntimeDespecRegistry::new_registered();
    let mgr = TieredCompilationManager::with_env_policy_and_vm_epochs(
        flush,
        redefine,
        Arc::clone(&own),
    );
    let (id, name) = (ClassId::new(0x1805), "i18l5/SameId");
    own.mark(id, name, "m", "()V");
    other.mark(id, name, "m", "()V");

    mgr.invalidate_class(id, name);
    assert!(
        !own.is_not_compilable(id, name, "m", "()V"),
        "own VM forgets"
    );
    assert!(
        other.is_not_compilable(id, name, "m", "()V"),
        "another VM's class with this id is a different class"
    );

    own.mark(id, name, "m", "()V");
    mgr.on_class_redefined(id, name);
    assert!(!own.is_not_compilable(id, name, "m", "()V"));
    assert!(other.is_not_compilable(id, name, "m", "()V"));
}
