use super::{
    class_init_unsettled, load_absent_native_static_owner, native_static_owner_init_unsettled,
    resolve_static_owner_fully,
};
use crate::classloading::ClassState;
use crate::vm::SharedVm;
use std::sync::Arc;

fn vm() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(crate::config::VmConfig::default()))
}

#[test]
fn an_owner_no_loader_can_define_stays_absent_and_is_not_fabricated() {
    let shared = vm();
    for name in [
        "cratonvm/test/NativeOnlyOwnerNoBytes",
        "java/lang/CratonvmNativeOnlyOwnerNoBytes",
    ] {
        load_absent_native_static_owner(&shared, name);
        assert!(
            shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(name)
                .is_none(),
            "{name}: a native call must not define or fabricate its owner"
        );
    }
}

#[test]
fn a_loaded_compatibility_stub_owner_is_left_as_it_is() {
    const NAME: &str = "cratonvm/test/NativeOwnerStub";
    let shared = vm();
    let stub = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(NAME, 0)
        .expect("Compatible mode fabricates");
    load_absent_native_static_owner(&shared, NAME);
    let cm = shared.classes.class_manager.read();
    assert_eq!(cm.get_loaded_class_id(NAME), Some(stub));
    assert!(cm
        .class_store
        .get(stub)
        .is_some_and(|c| c.origin.is_compatibility_stub()));
}

#[test]
fn strict_mode_does_not_resolve_native_only_vm_service_owners() {
    for name in [
        "java/lang/reflect/Proxy$Dispatch",
        "java/lang/reflect/Proxy$Instance",
    ] {
        assert!(
            !resolve_static_owner_fully(true, true, name),
            "{name} has no JDK class file; its reviewed bridge is the owner"
        );
        assert!(
            !resolve_static_owner_fully(true, false, name),
            "Compatible mode preserves the historical native route"
        );
    }
    assert!(
        resolve_static_owner_fully(true, true, "java/lang/System"),
        "strict mode must still resolve an ordinary native JDK owner"
    );
    assert!(
        resolve_static_owner_fully(false, true, "example/BytecodeOwner"),
        "bytecode owners always resolve"
    );
}

#[test]
fn initialization_state_gates_cache_fills() {
    const NAME: &str = "cratonvm/test/NativeOwnerInitializing";
    let shared = vm();
    let owner = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(NAME, 0)
        .expect("Compatible mode fabricates");
    let set_state = |state: ClassState, initializer: Option<u64>| {
        let mut cm = shared.classes.class_manager.write();
        let class = cm.class_store.get_mut(owner).expect("fabricated");
        class.state = state;
        class.initializing_thread = initializer;
    };
    let unsettled = || {
        let cm = shared.classes.class_manager.read();
        cm.class_store.get(owner).is_some_and(class_init_unsettled)
    };
    set_state(ClassState::Initializing, Some(7));
    assert!(unsettled(), "an initializer in flight is unsettled");
    set_state(ClassState::InitializationError, None);
    assert!(unsettled(), "a failed initializer is unsettled");
    assert!(
        native_static_owner_init_unsettled(&shared, NAME, "m", "()V"),
        "the native route initializes its owner in every mode, so a failed \
         initializer refuses the fill"
    );
    set_state(ClassState::Initializing, Some(7));
    assert!(
        native_static_owner_init_unsettled(&shared, NAME, "m", "()V"),
        "an initializer in flight refuses the fill"
    );
    set_state(ClassState::Initialized, None);
    assert!(
        !native_static_owner_init_unsettled(&shared, NAME, "m", "()V"),
        "an initialized owner may be cached"
    );
}
