#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, MockAllocOom};
use crate::try_alloc_concurrent_synthetic;
use cratonvm_types::error::VmError;

/// An invoker whose dispatch hits a refused one-attempt door: it pins
/// one more reference first (as the real dispatch does), then raises the
/// heap-exhaustion unwind.
fn raises_heap_exhaustion(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    _args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    let _ = ctx.pin_native_root(target);
    std::panic::panic_any(MockAllocOom)
}

/// Any other panic.
fn panics_otherwise(
    _ctx: &mut dyn NativeContext,
    _target: ObjectRef,
    _args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    std::panic::panic_any(String::from("zs5nd5: not a heap-exhaustion unwind"))
}

fn stub(ctx: &mut dyn NativeContext, invoker: UpcallInvoker) -> (usize, *const UpcallStubData) {
    let target = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 2)
        .expect("target");
    let shape = UpcallShape {
        params: vec![UpcallParam {
            kind: UpcallKind::Int,
            target_size: 0,
            target_align: 1,            }],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&*ctx, target, shape, None, invoker);
    let data = registry()
        .lock()
        .get(&code)
        .map(|e| e.data as *const UpcallStubData)
        .expect("registered");
    (code, data)
}

#[test]
fn a_heap_exhaustion_unwind_in_the_dispatch_is_the_uncaught_oome() {
    let mut ctx = mock_ctx();
    let (code, data) = stub(&mut ctx, raises_heap_exhaustion);
    let arg: i32 = 5;
    let args = [(&arg as *const i32).cast::<c_void>()];
    // Before: the bare dispatch lets the unwind out (on the host it then
    // met libffi's trampoline and aborted the process).
    let raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `data` is registered; `args` holds one `int` pointer.
        unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) }
    }));
    assert!(raw.is_err(), "the bare dispatch unwinds");
    // After: stopped here, the upcall's uncaught OutOfMemoryError.
    // SAFETY: as above.
    let outcome = unsafe { upcall_dispatch_contained(&mut ctx, &*data, args.as_ptr()) };
    assert!(
        matches!(
            outcome,
            Err(UpcallFatal::Uncaught(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::OutOfMemoryError { .. })
            )))
        ),
        "the uncaught OOME"
    );
    remove_upcall_stub(code);
}

/// zs s6/rv4: the REPORT of the contained failure allocates too (the
/// `OutOfMemoryError`'s message `String`, then the exit message), on the
/// same thread, with the same unwind permission, usually on the same
/// exhausted heap. Uncontained (`contain == false`: the s5 sequence) the
/// second refusal unwinds out of the report -- on the host, across
/// libffi's trampoline: the abort again. Contained, each stage falls back
/// to the process stderr and the report returns; any other panic is still
/// resumed.
#[test]
fn zs6rv4_a_heap_exhaustion_inside_the_uncaught_report_is_contained_too() {
    use crate::test_utils::mock_create_string_raises_oom;
    let mut ctx = mock_ctx();
    let oome = || {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Java heap space".to_string(),
        }))
    };
    mock_create_string_raises_oom(true);
    let bare = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report_uncaught(&mut ctx, oome(), false)
    }));
    let contained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report_uncaught(&mut ctx, oome(), true)
    }));
    mock_create_string_raises_oom(false);
    let payload = bare.expect_err("the uncontained report unwinds on its String");
    assert!(
        payload.downcast_ref::<MockAllocOom>().is_some(),
        "positive control: the report allocated and the knob refused it"
    );
    assert!(contained.is_ok(), "the contained report returns");
    let other = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        contained_report_stage(
            &mut ctx,
            true,
            |_| std::panic::panic_any(String::from("zs6rv4: not heap exhaustion")),
            || panic!("the fallback is for heap exhaustion only"),
        )
    }));
    let payload = other.expect_err("resumed");
    assert!(payload
        .downcast_ref::<String>()
        .is_some_and(|s| s.contains("zs6rv4")));
    assert_eq!(
        uncaught_plain_line(&VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Java heap space".to_string(),
        })),
        "java.lang.OutOfMemoryError: Java heap space"
    );
}

/// Positive control of the payload test: any other panic is resumed
/// unchanged (the containment is for the VM's unwind only).
#[test]
fn any_other_panic_is_resumed() {
    let mut ctx = mock_ctx();
    let (code, data) = stub(&mut ctx, panics_otherwise);
    let arg: i32 = 5;
    let args = [(&arg as *const i32).cast::<c_void>()];
    let resumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `data` is registered; `args` holds one `int` pointer.
        unsafe { upcall_dispatch_contained(&mut ctx, &*data, args.as_ptr()) }
    }));
    let payload = resumed.expect_err("resumed");
    assert!(payload
        .downcast_ref::<String>()
        .is_some_and(|s| s.contains("zs5nd5")));
    remove_upcall_stub(code);
}
