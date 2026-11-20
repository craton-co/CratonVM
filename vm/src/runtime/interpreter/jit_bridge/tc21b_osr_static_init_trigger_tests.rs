use super::static_init_pending_any_initialized;
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

/// JIT r21 tc21b review of TYPECHECK21-8: the OSR memo's trigger follows
/// the class's own `init_state` -- not initialized and in progress are
/// "still pending", `INITIALIZED` fires -- and a list holding one pending
/// and one unknown id fires on the initialized one alone.
#[test]
fn tc21b_the_osr_static_init_trigger_follows_init_state() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let id = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/tc21b/OsrInit", 0) else {
            return; // --real-jdk policy in the environment
        };
        let Some(class) = cm.get_class(id) else {
            return;
        };
        class
            .init_state
            .store(cratonvm_classloading::CLASS_INIT_UNINITIALIZED, Ordering::Release);
        id
    };
    let set_state = |state: u8| {
        let cm = shared.classes.class_manager.read();
        if let Some(class) = cm.get_class(id) {
            class.init_state.store(state, Ordering::Release);
        }
    };
    let pending = [0x00ff_fff0, id.as_u32()];
    assert!(
        !static_init_pending_any_initialized(&shared, &pending),
        "uninitialized: still pending"
    );
    set_state(cratonvm_classloading::CLASS_INIT_IN_PROGRESS);
    assert!(
        !static_init_pending_any_initialized(&shared, &pending),
        "<clinit> running: still pending"
    );
    set_state(cratonvm_classloading::CLASS_INIT_INITIALIZED);
    assert!(
        static_init_pending_any_initialized(&shared, &pending),
        "initialized: the memo entry is forgotten"
    );
    assert!(
        !static_init_pending_any_initialized(&shared, &[0x00ff_fff0]),
        "an unknown id never fires"
    );
}
