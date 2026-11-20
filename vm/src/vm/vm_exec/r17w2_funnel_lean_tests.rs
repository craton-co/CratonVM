use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::Arc;

fn echo_first(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(args.first().copied())
}

/// The lean route answers what the prevalidated funnel answers, roots an
/// object result in the handoff slot the same way, and leaves the pin
/// stack as it found it.
#[test]
fn the_lean_funnel_answers_as_the_prevalidated_one() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(crate::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "r17w2-funnel-lean");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let args = [Value::Object(Some(obj)), Value::Int(3)];
    let pins = thread.native_pin_roots.len();
    let full = safe_native_call_prevalidated_objects(&shared, &mut thread, echo_first, &args);
    assert!(matches!(full, Ok(Some(Value::Object(Some(o)))) if o == obj));
    thread.native_pending_return = None;
    let lean = safe_native_call_jit_lean(&shared, &mut thread, echo_first, &args);
    assert!(matches!(lean, Ok(Some(Value::Object(Some(o)))) if o == obj));
    assert_eq!(thread.native_pending_return, Some(obj));
    assert_eq!(thread.native_pin_roots.len(), pins);
}
