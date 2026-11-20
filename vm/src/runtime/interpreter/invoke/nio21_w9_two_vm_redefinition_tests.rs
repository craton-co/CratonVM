use crate::classloading::resolution::{CachedBytecodeMethod, CachedInvokeTarget, RedefineGate};
use crate::threading::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_native_api::NativeClassAccess as _;
use cratonvm_types::ClassId;
use std::sync::Arc;

fn jdk_only_vm() -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    SharedVm::new(config)
}

fn compatible_vm() -> SharedVm {
    SharedVm::new(crate::config::VmConfig::default())
}

/// A thread of `vm`, its invoke cache bound to `vm`'s redefinition log as
/// every registered thread's is; `None` under
/// `CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS=0` (nothing binds).
fn bound_thread(vm: &SharedVm, id: u64) -> Option<JvmThread> {
    let mut thread = JvmThread::new(ThreadId(id), "nio21-w9");
    thread
        .invoke_cache
        .bind_redefinition_log(vm.classes.redefinition_log.clone());
    thread.invoke_cache.has_redefinition_log().then_some(thread)
}

fn target<J>(declaring: u32) -> CachedInvokeTarget<J> {
    CachedInvokeTarget::Bytecode {
        cached: Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(declaring),
                class_name: Arc::from("p/Helper"),
                method_name: Arc::from("pick"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: Arc::from(&[0x04u8, 0xACu8][..]), // iconst_1; ireturn
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 0,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        )),
        gate: RedefineGate::never_stale(),
    }
}

#[test]
fn nio21_w9_another_vms_redefinition_keeps_this_vms_planted_invoke_entry() {
    let a = jdk_only_vm();
    let b = compatible_vm();
    assert!(crate::vm::dispatch_policy(&a).is_jdk_only());
    assert!(!crate::vm::dispatch_policy(&b).is_jdk_only());
    let Some(mut thread_b) = bound_thread(&b, 9_101) else {
        return;
    };
    let c = ClassId::new(29_101);
    thread_b.invoke_cache.put(c, 5, false, 0, target(29_102));
    assert!(
        thread_b.invoke_cache.get(c, 5, false, 0).is_some(),
        "planted"
    );

    // VM A redefines a class with the same id, three times.
    for _ in 0..3 {
        a.classes.class_manager.read().bump_redefinition_count_of(c);
    }
    let after_a = cratonvm_classloading::class_redefinition_count();
    assert!(
        thread_b.invoke_cache.get(c, 5, false, 0).is_some(),
        "VM A's redefinition retired VM B's entry"
    );
    assert!(
        thread_b.invoke_cache.redefinitions_seen() >= after_a,
        "B did not catch its stamp up past A's moves"
    );

    // VM B's own redefinition of that class retires it.
    b.classes.class_manager.read().bump_redefinition_count_of(c);
    assert!(
        thread_b.invoke_cache.get(c, 5, false, 0).is_none(),
        "VM B's own redefinition kept a stale entry"
    );
}

#[test]
fn nio21_w9_another_vms_redefinition_does_not_mark_this_vms_walked_frame() {
    let a = jdk_only_vm();
    let b = compatible_vm();
    let Some(mut thread_b) = bound_thread(&b, 9_102) else {
        return;
    };
    let c = ClassId::new(29_111);
    let walked_at = cratonvm_classloading::class_redefinition_count();
    a.classes.class_manager.read().bump_redefinition_count_of(c);
    let now = cratonvm_classloading::class_redefinition_count();

    // The process ring names A's class under the id (when it can answer:
    // a parallel run may lap it).
    let mut ring_named = false;
    if cratonvm_classloading::for_each_redefined_class_between(walked_at, now, |x| {
        ring_named |= x == c
    }) {
        assert!(ring_named, "the ring lost A's record");
    }

    // VM B's door (what `lang_stackwalker` / `reflect_invoke` ask) does not.
    let ctx = crate::vm::NativeContextImpl {
        shared: &b,
        thread: &mut thread_b,
    };
    let mut named = false;
    let answer = ctx.redefined_classes_between(walked_at, now, &mut |x| named |= x == c);
    assert_eq!(answer, Some(true), "B's log answers the span");
    assert!(!named, "VM A's redefinition reached VM B's walked frame");

    // VM B's own redefinition of the class is named.
    let before = cratonvm_classloading::class_redefinition_count();
    b.classes.class_manager.read().bump_redefinition_count_of(c);
    let after = cratonvm_classloading::class_redefinition_count();
    let mut named = false;
    let answer = ctx.redefined_classes_between(before, after, &mut |x| named |= x == c);
    assert_eq!(answer, Some(true));
    assert!(named, "VM B's own redefinition was not named");
}
