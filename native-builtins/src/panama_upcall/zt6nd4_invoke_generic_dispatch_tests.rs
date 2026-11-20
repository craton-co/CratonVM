#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{
    mock_ctx, mock_heap_is_zgc, mock_reclaims, mock_refuse_allocations, mock_reservation,
    MockNativeContext, MockReservation,
};
use crate::try_alloc_concurrent_synthetic;

thread_local! {
    /// What `invokeWithArguments` received: the array's length and its
    /// element 0's slot 0 (the boxed value).
    static SEEN: std::cell::Cell<Option<(usize, Value)>> = const { std::cell::Cell::new(None) };
}

struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
        mock_heap_is_zgc(false);
        mock_reservation(MockReservation::Off);
    }
}

fn record_invoke_with_arguments(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "invokeWithArguments" {
        return None;
    }
    let Some(Value::Object(Some(array))) = args.first().copied() else {
        panic!("invokeWithArguments without an array: {args:?}");
    };
    let len = ctx.array_length(array);
    let boxed = match ctx.get_array_element(array, 0) {
        Value::Object(Some(b)) => ctx.get_field(b, 0),
        other => other,
    };
    SEEN.with(|c| c.set(Some((len, boxed))));
    Some(Ok(Some(Value::Int(42))))
}

/// One upcall `(int) -> int` with argument 5 through the generic
/// invoker; answers the dispatch's success and the reclaims it ran.
fn dispatch_once(zgc: bool) -> (bool, usize) {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(record_invoke_with_arguments);
    mock_heap_is_zgc(zgc);
    mock_reservation(MockReservation::Serve);
    let target = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2)
        .expect("target");
    let shape = UpcallShape {
        params: vec![UpcallParam {
            kind: UpcallKind::Int,
            target_size: 0,
            target_align: 1,            }],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, invoke_generic);
    let data = registry()
        .lock()
        .get(&code)
        .map(|e| e.data as *const UpcallStubData)
        .expect("registered");
    SEEN.with(|c| c.set(None));
    // The generic invoker's first mint (the `Object[]`) is refused once.
    mock_refuse_allocations(1, true);
    let arg: i32 = 5;
    let args = [(&arg as *const i32).cast::<c_void>()];
    // SAFETY: `data` is registered; `args` holds one `int` pointer.
    let outcome = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
    let reclaims = mock_reclaims();
    remove_upcall_stub(code);
    (outcome.is_ok(), reclaims)
}

#[test]
fn zt6nd4_the_generic_invokers_refused_array_collects_on_zgc() {
    let _reset = KnobReset;
    let (ok, reclaims) = dispatch_once(true);
    assert!(ok, "the upcall returned");
    assert_eq!(reclaims, 1, "the refused Object[] ran the ladder once");
    assert_eq!(
        SEEN.with(std::cell::Cell::get),
        Some((1, Value::Int(5))),
        "invokeWithArguments got the boxed argument"
    );
}

/// The gate's control: off ZGC the one-attempt doors run (the mock's
/// one-attempt `new_ref_array` does not consult the refusal), so no
/// reclaim, and the call still reaches `invokeWithArguments`.
#[test]
fn zt6nd4_off_zgc_the_generic_invoker_does_not_collect() {
    let _reset = KnobReset;
    let (ok, reclaims) = dispatch_once(false);
    assert!(ok);
    assert_eq!(reclaims, 0);
    assert_eq!(SEEN.with(std::cell::Cell::get), Some((1, Value::Int(5))));
}
