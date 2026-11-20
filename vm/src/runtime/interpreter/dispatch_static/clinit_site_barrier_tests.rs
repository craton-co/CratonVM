use super::{clinit_site_lookup, clinit_site_remember};
use crate::classloading::resolution::RedefineGate;
use crate::classloading::{ClassId, ClassState};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_jit_api::CachedBytecodeMethod;
use std::sync::Arc;

fn cached_for(declaring: ClassId, class_name: &str) -> Arc<CachedBytecodeMethod> {
    // Through the constructor, not a struct literal: a new memo cell on
    // `CachedBytecodeMethod` must not have to touch this test.
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from(class_name),
            method_name: Arc::from("helper"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0xb1u8, 0, 0][..]),
            exception_table: Arc::from(Vec::new()),
            max_stack: 0,
            max_locals: 0,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ))
}

/// A `<clinit>`-barrier site is served only while its holder is being
/// initialized by THIS thread; once that stops (another initializer, a
/// failed or finished `<clinit>`) the entry is dropped for good.
#[test]
fn a_barrier_site_is_served_only_while_this_thread_initializes_the_holder() {
    const NAME: &str = "cratonvm/test/ClinitBarrierHolder";
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let thread = JvmThread::new(ThreadId(41), "clinit-barrier-test");
    let holder = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(NAME, 0)
        .expect("Compatible mode fabricates");
    let set_state = |state: ClassState, initializer: Option<u64>| {
        let mut cm = shared.classes.class_manager.write();
        let class = cm.class_store.get_mut(holder).expect("fabricated");
        class.state = state;
        class.initializing_thread = initializer;
    };
    let remember = || {
        clinit_site_remember(
            &shared,
            holder,
            7,
            cached_for(holder, NAME),
            RedefineGate::never_stale(),
        )
    };

    set_state(ClassState::Initializing, Some(41));
    remember();
    assert!(clinit_site_lookup(&shared, &thread, holder, 7).is_some());
    assert!(
        clinit_site_lookup(&shared, &thread, holder, 8).is_none(),
        "another constant-pool index is another site"
    );

    set_state(ClassState::Initializing, Some(42));
    assert!(
        clinit_site_lookup(&shared, &thread, holder, 7).is_none(),
        "another thread's initialization is not ours to skip"
    );
    set_state(ClassState::Initializing, Some(41));
    assert!(
        clinit_site_lookup(&shared, &thread, holder, 7).is_none(),
        "a dropped entry stays dropped"
    );

    remember();
    set_state(ClassState::InitializationError, None);
    assert!(
        clinit_site_lookup(&shared, &thread, holder, 7).is_none(),
        "a failed <clinit> must reach the slow path's NoClassDefFoundError"
    );
}
