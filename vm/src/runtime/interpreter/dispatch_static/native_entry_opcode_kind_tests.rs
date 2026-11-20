use super::*;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodId};

fn never_run(
    _ctx: &mut dyn NativeContext,
    _args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(Some(cratonvm_types::Value::Int(-1)))
}

fn native_entry(is_static: bool) -> CachedInvokeTarget {
    CachedInvokeTarget::Native {
        callback: never_run,
        native_id: NativeMethodId::from_u32(0),
        native_kind: NativeKind::Bridge,
        num_params: 0,
        facts: cratonvm_jit_api::DescriptorFacts::of("()I"),
        is_static,
        sync: None,
        gate: RedefineGate::never_stale(),
        owner: ClassId::new(0),
    }
}

fn thread_with_one_int_on_the_stack(caller: ClassId) -> JvmThread {
    let mut thread = JvmThread::new(ThreadId(4_747), "native-entry-kind");
    thread.frames.push(Frame::new(
        caller,
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    thread.frames[0]
        .stack
        .push_int(7)
        .expect("an 8-slot stack takes one int");
    thread
}

#[test]
fn invokestatic_refuses_an_instance_native_entry_before_popping() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let caller = ClassId::new(91_001);
    let mut thread = thread_with_one_int_on_the_stack(caller);
    thread
        .invoke_cache
        .put(caller, 5, false, 0, native_entry(false));
    let outcome = execute_invokestatic_cached(&shared, &mut thread, 0, 5, 0);
    assert!(matches!(outcome, Ok(CachedCallResult::CacheMiss)));
    assert_eq!(thread.frames[0].stack.len(), 1, "nothing was popped");
    assert!(
        thread.invoke_cache.get(caller, 5, false, 0).is_none(),
        "the wrong-kind entry is evicted, as the `Bytecode` arm does"
    );
}

#[test]
fn invokevirtual_refuses_a_static_native_entry_before_popping() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let caller = ClassId::new(91_002);
    let mut thread = thread_with_one_int_on_the_stack(caller);
    thread
        .invoke_cache
        .put(caller, 5, false, 0, native_entry(true));
    let outcome = execute_invokevirtual_cached(&shared, &mut thread, 0, 5, 0, false, false);
    assert!(matches!(outcome, Ok(CachedCallResult::CacheMiss)));
    assert_eq!(thread.frames[0].stack.len(), 1, "nothing was popped");
    assert!(
        thread.invoke_cache.get(caller, 5, false, 0).is_some(),
        "the entry is right for its own `invokestatic` site and stays"
    );
}
