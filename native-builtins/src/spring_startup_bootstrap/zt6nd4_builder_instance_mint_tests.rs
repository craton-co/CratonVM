#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, mock_reclaims, mock_refuse_allocations};

struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
    }
}

fn heap_refusal() -> MethodCallFailed {
    crate::util_concurrent_ext::native_heap_oom("example.Builder$$Lookup")
}

/// The mock is not a ZGC VM: the gate is closed (Generational / G1 keep
/// the one-attempt door).
#[test]
fn zt6nd4_the_builder_gate_is_closed_off_zgc() {
    let ctx = mock_ctx();
    assert!(!ctx.heap_is_zgc());
    assert!(!builder_instance_collects(&ctx));
}

/// A refused first attempt collects, and the retry after the reclaim
/// serves the instance.
#[test]
fn zt6nd4_a_refused_builder_mint_collects_then_retries() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let obj = ctx.fresh_object_ref();
    mock_refuse_allocations(0, true);
    let mut calls = 0usize;
    let got = builder_instance_mint(&mut ctx, |_| true, |_ctx| {
        calls += 1;
        if calls == 1 {
            Err(heap_refusal())
        } else {
            Ok(Some(Value::Object(Some(obj))))
        }
    });
    assert_eq!(got, Some(obj));
    assert_eq!(calls, 2, "the first attempt and one retry");
    assert_eq!(mock_reclaims(), 1, "one reclaim ran before the retry");
}

/// The kill switch's arm (`collects == false`): the one-attempt door,
/// no reclaim.
#[test]
fn zt6nd4_without_the_gate_a_refused_builder_mint_does_not_collect() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    mock_refuse_allocations(0, true);
    let mut calls = 0usize;
    let got = builder_instance_mint(&mut ctx, |_| false, |_ctx| {
        calls += 1;
        Err(heap_refusal())
    });
    assert_eq!(got, None);
    assert_eq!(calls, 1);
    assert_eq!(mock_reclaims(), 0);
}

/// A failure that is not the heap's (an unresolvable class) does not run
/// the ladder.
#[test]
fn zt6nd4_a_non_heap_failure_does_not_collect() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    mock_refuse_allocations(0, true);
    let got = builder_instance_mint(&mut ctx, |_| true, |_ctx| {
        Err(MethodCallFailed::from(
            cratonvm_types::error::LinkageError::NoClassDefFoundError {
                class_name: "example/Missing".to_string(),
            },
        ))
    });
    assert_eq!(got, None);
    assert_eq!(mock_reclaims(), 0);
}

/// zt t7/x4b: the gate (three reads: `heap_is_zgc`, the doors' switch,
/// the environment) is asked only after a HEAP refusal -- not for a served
/// mint, not for a resolution failure. Control: a heap refusal asks once.
#[test]
fn zt7x4b_the_gate_is_read_only_after_a_heap_refusal() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let obj = ctx.fresh_object_ref();
    let mut asked = 0usize;
    let served = builder_instance_mint(
        &mut ctx,
        |_| {
            asked += 1;
            true
        },
        |_ctx| Ok(Some(Value::Object(Some(obj)))),
    );
    assert_eq!(served, Some(obj));
    let missing = builder_instance_mint(
        &mut ctx,
        |_| {
            asked += 1;
            true
        },
        |_ctx| {
            Err(MethodCallFailed::from(
                cratonvm_types::error::LinkageError::NoClassDefFoundError {
                    class_name: "example/Missing".to_string(),
                },
            ))
        },
    );
    assert_eq!(missing, None);
    assert_eq!(asked, 0, "no refusal by the heap: the gate is not read");
    let refused = builder_instance_mint(
        &mut ctx,
        |_| {
            asked += 1;
            false
        },
        |_ctx| Err(heap_refusal()),
    );
    assert_eq!(refused, None);
    assert_eq!(asked, 1, "control: a heap refusal asks the gate once");
    assert_eq!(mock_reclaims(), 0, "the gate said no");
}
