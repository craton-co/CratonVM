use super::*;
#[test]
fn an_oom_after_a_native_side_effect_is_never_replayed() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(crate::ThreadId(0), "direct-native-oom");
    let mut ctx = crate::vm::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let calls = std::cell::Cell::new(0);
    let pins = ctx.thread.native_pin_roots.len();
    let answer = jit_native_alloc_once(&mut ctx, |ctx| {
        calls.set(calls.get() + 1); // The native side effect has happened.
        let object = ctx
            .shared
            .mem
            .heap
            .try_alloc_object_full(ClassId::new(0), 0)
            .unwrap();
        ctx.thread.native_pin_roots.push(object);
        crate::runtime::native_oom::raise(40, "object")
    });
    assert_eq!(calls.get(), 1);
    assert_eq!(ctx.thread.native_pin_roots.len(), pins);
    assert!(matches!(
        answer,
        Some(Err(crate::error::MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(crate::error::RuntimeError::OutOfMemoryError { .. })
        )))
    ));
    assert!(!crate::runtime::native_oom::unwind_ok());
}
