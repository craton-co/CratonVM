use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn call_native(
    registry: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = registry
        .find(class, method, desc)
        .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
    cb(ctx, args)
}

fn make_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_deprecated_lang_natives(&mut r);
    r
}

/// Serialize tests that mutate global ALLOW_THREAD_STOP / ALLOW_THREAD_SUSPEND.
static GLOBAL_FLAG_LOCK: Mutex<()> = Mutex::new(());

// ----- T8.1.1 Thread.stop -----

/// `Thread.stop()V` must stay UNREGISTERED, and this is the assertion that
/// keeps it that way. Re-adding it would silently take the method back off
/// the real bytecode of every supported image — and, if the `lib.rs` copy
/// ever came back with it, would restore the "interrupt and return
/// normally" body this retirement removed.
#[test]
fn t8_1_1_thread_stop_is_retired_not_registered() {
    let reg = make_registry();
    assert!(
        reg.find("java/lang/Thread", "stop", "()V").is_none(),
        "java/lang/Thread.stop()V is served by real bytecode on every \
         supported image; it must not be registered here"
    );
}

#[test]
fn test_thread_stop0_throws_when_disallowed() {
    let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
    ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();
    let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ThreadDeath", 0).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Thread",
        "stop0",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(thr)), Value::Object(Some(exc))],
    );
    assert!(res.is_err());
}

#[test]
fn test_thread_stop0_stores_throwable_when_allowed() {
    let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
    ALLOW_THREAD_STOP.store(true, Ordering::SeqCst);
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    // gc-common w29-a: rows are keyed by weak lock keys, per (VM,
    // address); a private VM identity, forgotten at the end.
    ctx.set_vm_identity(0x29a0_D501);
    let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();
    let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Throwable", 0).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Thread",
        "stop0",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(thr)), Value::Object(Some(exc))],
    );
    // Clean up the sticky flag before any assertion can fail.
    ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
    assert!(res.is_ok());
    let taken = take_stop_throwable(&mut ctx, thr);
    crate::forget_vm_lock_keys(0x29a0_D501);
    assert_eq!(taken, Some(exc), "Expected stored throwable");
}

/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 36): two Threads with one identity hash keep their own stop
/// requests, and a dead Thread's request -- and its throwable's root --
/// goes with its lock key. Fake Threads 4 GiB apart share the mock's hash
/// (the address truncated to `i32`); this path never dereferences them.
#[test]
fn w29a_colliding_threads_keep_their_own_stop_requests() {
    const VM: usize = 0x29a0_D502;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(VM);
        }
    }
    let _forget = Forget;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only, never dereferenced.
    let (thread_a, thread_b) = unsafe {
        (
            ObjectRef::from_raw(0x1_29a0_d508usize as *mut u8),
            ObjectRef::from_raw(0x2_29a0_d508usize as *mut u8),
        )
    };
    assert_eq!(
        ctx.identity_hash_code(thread_a),
        ctx.identity_hash_code(thread_b),
        "premise: one identity hash"
    );
    let (exc_a, exc_b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let before = ctx.global_root_count();
    store_stop_throwable(&mut ctx, thread_a, exc_a).expect("store A");
    store_stop_throwable(&mut ctx, thread_b, exc_b).expect("store B");
    assert_eq!(
        ctx.global_root_count(),
        before + 2,
        "B's store released A's request"
    );
    assert_eq!(take_stop_throwable(&mut ctx, thread_b), Some(exc_b));
    assert_eq!(ctx.global_root_count(), before + 1);

    // Thread A dies: its request goes with its key; the root is released
    // by the VM's next store.
    let a_addr = thread_a.as_ptr() as usize;
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|addr: usize| addr != a_addr),
        1
    );
    assert_eq!(take_stop_throwable(&mut ctx, thread_a), None);
    store_stop_throwable(&mut ctx, thread_b, exc_b).expect("store B again");
    assert_eq!(take_stop_throwable(&mut ctx, thread_b), Some(exc_b));
    assert_eq!(
        ctx.global_root_count(),
        before,
        "the dead Thread's throwable must stop being rooted"
    );
}

// ----- T8.1.2 Thread.suspend / resume -----

#[test]
fn test_thread_suspend_throws_when_disallowed() {
    let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
    ALLOW_THREAD_SUSPEND.store(false, Ordering::SeqCst);
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Thread",
        "suspend0",
        "()V",
        &[Value::Object(Some(thr))],
    );
    assert!(res.is_err());
    let msg = format!("{}", res.unwrap_err());
    assert!(
        msg.contains("not supported"),
        "Expected unsupported, got: {msg}"
    );
}

#[test]
fn test_thread_resume_throws_when_disallowed() {
    let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
    ALLOW_THREAD_SUSPEND.store(false, Ordering::SeqCst);
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Thread",
        "resume0",
        "()V",
        &[Value::Object(Some(thr))],
    );
    assert!(res.is_err());
}

// ----- T8.1.3 Thread.destroy -----

/// `Thread.destroy()V` must stay UNREGISTERED. No supported image declares
/// it (MEASURED over nine), and an unregistered call to a method no image
/// declares already raises `NoSuchMethodError` — which is all the retired
/// native did.
#[test]
fn t8_1_3_thread_destroy_is_retired_not_registered() {
    let reg = make_registry();
    assert!(
        reg.find("java/lang/Thread", "destroy", "()V").is_none(),
        "no supported JDK image declares java/lang/Thread.destroy()V"
    );
}

// ----- T8.1.4 Thread.countStackFrames -----

#[test]
fn test_thread_count_stack_frames_throws() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Thread",
        "countStackFrames",
        "()I",
        &[Value::Object(Some(thr))],
    );
    assert!(res.is_err());
    let msg = format!("{}", res.unwrap_err());
    assert!(
        msg.contains("unsupported"),
        "Expected unsupported, got: {msg}"
    );
}

// ----- T8.1.6 runFinalization -----

/// Both `runFinalization` natives reach the VM's hook (which drains the
/// VM's own finalizer queue — `vm_exec.rs`,
/// `run_pending_finalizers_for_runtime`), once per call, and return void.
/// gc-common w4-d: they used to drain a process-global tracker nothing fed.
#[test]
fn test_run_finalization_reaches_the_vm_hook() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    assert_eq!(ctx.run_finalization_calls.get(), 0);

    for class in ["java/lang/Runtime", "java/lang/System"] {
        let res = call_native(&reg, &mut ctx, class, "runFinalization", "()V", &[]);
        assert!(matches!(res, Ok(None)), "{class}.runFinalization: {res:?}");
    }
    assert_eq!(ctx.run_finalization_calls.get(), 2);
}

#[test]
fn test_system_run_finalization_registered() {
    let reg = make_registry();
    assert!(
        reg.find("java/lang/System", "runFinalization", "()V")
            .is_some(),
        "System.runFinalization should be registered"
    );
}

// ----- T8.1.7 runFinalizersOnExit -----

/// `System.runFinalizersOnExit(Z)V` must stay UNREGISTERED: no supported
/// image declares it, and the flag it set had no reader.
#[test]
fn t8_1_7_run_finalizers_on_exit_is_retired_not_registered() {
    let reg = make_registry();
    assert!(
        reg.find("java/lang/System", "runFinalizersOnExit", "(Z)V")
            .is_none(),
        "no supported JDK image declares java/lang/System.runFinalizersOnExit(Z)V"
    );
}

// ----- T8.1.8 SecurityManager — already in security_manager.rs -----

#[test]
fn test_security_manager_already_registered() {
    let mut reg = NativeMethodRegistry::new();
    crate::security_manager::register_security_manager_natives(&mut reg);
    assert!(
        reg.find(
            "java/lang/SecurityManager",
            "checkPermission",
            "(Ljava/security/Permission;)V"
        )
        .is_some(),
        "SecurityManager.checkPermission must be registered by security_manager.rs"
    );
}

// ----- T8.1.9 ClassLoader.defineClass 3-arg -----

#[test]
fn test_classloader_define_class_3arg_delegates() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let loader = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ClassLoader", 0).unwrap();

    // Pre-arm invoke_virtual to return a class-like object
    let fake_class = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 0).unwrap();
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(fake_class)))));
    }

    let byte_arr = try_alloc_concurrent_synthetic(&mut ctx, "[B", 0).unwrap();
    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/ClassLoader",
        "defineClass",
        "([BII)Ljava/lang/Class;",
        &[
            Value::Object(Some(loader)),
            Value::Object(Some(byte_arr)),
            Value::Int(0),
            Value::Int(10),
        ],
    );
    assert!(res.is_ok());
    match res.unwrap() {
        Some(Value::Object(Some(obj))) => assert_eq!(obj, fake_class),
        other => panic!("Expected Object(Some(fake_class)), got: {:?}", other),
    }
}

// ----- T8.1.10 Compiler -----

#[test]
fn test_compiler_compile_class_returns_false() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Compiler",
        "compileClass",
        "(Ljava/lang/Class;)Z",
        &[Value::Object(None)],
    );
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), Some(Value::Int(0)));
}

#[test]
fn test_compiler_compile_classes_returns_false() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Compiler",
        "compileClasses",
        "(Ljava/lang/String;)Z",
        &[Value::Object(None)],
    );
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), Some(Value::Int(0)));
}

#[test]
fn test_compiler_command_returns_null() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Compiler",
        "command",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(None)],
    );
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), Some(Value::Object(None)));
}

#[test]
fn test_compiler_enable_disable_noop() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let res = call_native(&reg, &mut ctx, "java/lang/Compiler", "enable", "()V", &[]);
    assert!(res.is_ok());
    assert!(res.unwrap().is_none());

    let res = call_native(&reg, &mut ctx, "java/lang/Compiler", "disable", "()V", &[]);
    assert!(res.is_ok());
    assert!(res.unwrap().is_none());
}

// ----- Registration count -----

#[test]
fn test_registration_count() {
    let reg = make_registry();
    // 1 (stop0) + 2 (suspend0/resume0) + 1 (countStackFrames)
    // + 2 (runFinalization) + 1 (defineClass 3-arg) + 5 (Compiler) = 12.
    // `stop`, `destroy` and `runFinalizersOnExit` were retired 2026-08-21;
    // the three assertions above pin their ABSENCE, so this bound only has
    // to stop the registrar losing anything else.
    assert!(
        reg.len() >= 12,
        "Expected at least 12 registered natives, got {}",
        reg.len()
    );
}
