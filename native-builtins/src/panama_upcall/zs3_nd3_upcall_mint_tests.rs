#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, mock_reclaims, mock_refuse_allocations};
use cratonvm_types::error::VmError;

struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
    }
}

fn is_oom(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message
        })) if message.starts_with("Java heap space")
    )
}

#[test]
fn a_refused_box_reclaims_and_is_served() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    for (value, desc) in [
        (Value::Int(7), "I"),
        (Value::Long(-3), "J"),
        (Value::Double(2.5), "D"),
        (Value::Int(1), "Z"),
    ] {
        mock_refuse_allocations(1, true);
        let boxed = box_value_collecting(&mut ctx, value, desc).expect("served");
        assert_eq!(mock_reclaims(), 1, "{desc}: one reclaim ran");
        let Value::Object(Some(obj)) = boxed else {
            panic!("{desc}: {boxed:?}");
        };
        assert_eq!(ctx.get_field(obj, 0), value, "{desc}");
    }
    // A reference descriptor: the value itself, no allocation.
    mock_refuse_allocations(usize::MAX, false);
    assert_eq!(
        box_value_collecting(&mut ctx, Value::Object(None), "Ljava/lang/Object;").unwrap(),
        Value::Object(None)
    );
    assert_eq!(mock_reclaims(), 0);
}

#[test]
fn a_refused_args_array_reclaims_and_is_served() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    mock_refuse_allocations(1, true);
    let array = generic_args_array_collecting(&mut ctx, cratonvm_types::ClassId::new(0), 3)
        .expect("served");
    assert_eq!(mock_reclaims(), 1);
    assert_eq!(ctx.array_length(array), 3);
}

#[test]
fn a_refused_group_arena_and_block_reclaim_and_are_served() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    mock_refuse_allocations(1, true);
    let arena = foreign_ffm::p67_new_vm_confined_arena_with(&mut ctx, true).expect("arena");
    assert_eq!(mock_reclaims(), 1, "the arena's mint reclaimed");
    let mut blocks = Vec::new();
    mock_refuse_allocations(1, true);
    let made = upcall_group_block(&mut ctx, arena, 24, 8, &mut blocks).expect("block");
    assert_eq!(mock_reclaims(), 1, "the segment's mint reclaimed");
    let Some(Value::Object(Some(segment))) = made else {
        panic!("{made:?}");
    };
    assert_eq!(blocks.len(), 1);
    assert_ne!(ctx.get_field(segment, 0), Value::Long(0), "a native block");
    assert_eq!(ctx.get_field(segment, 1), Value::Long(24));
    assert_eq!(ctx.get_field(segment, 4), Value::Int(1), "alive");
    free_upcall_blocks(&mut ctx, true, &blocks);
}

#[test]
fn spent_ladders_are_oomes() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let arena = foreign_ffm::p67_new_vm_confined_arena_with(&mut ctx, true).expect("arena");
    mock_refuse_allocations(usize::MAX, false);
    let err = box_value_collecting(&mut ctx, Value::Int(1), "I").unwrap_err();
    assert!(is_oom(&err), "{err:?}");
    let err = generic_args_array_collecting(&mut ctx, cratonvm_types::ClassId::new(0), 2)
        .unwrap_err();
    assert!(is_oom(&err), "{err:?}");
    let mut blocks = Vec::new();
    let err = upcall_group_block(&mut ctx, arena, 8, 8, &mut blocks).unwrap_err();
    assert!(is_oom(&err), "{err:?}");
    assert!(
        blocks.is_empty(),
        "a refused carrier's block is freed, not kept"
    );
    let err = foreign_ffm::p67_new_vm_confined_arena_with(&mut ctx, true).unwrap_err();
    assert!(is_oom(&err), "{err:?}");
}

/// The one-attempt arm never consults the fallible door or the ladder:
/// with refusals pending, `collecting = false` makes the arena without a
/// reclaim (the gate's positive control).
#[test]
fn the_one_attempt_arena_never_reclaims() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    mock_refuse_allocations(1, true);
    let _ = foreign_ffm::p67_new_vm_confined_arena_with(&mut ctx, false).expect("arena");
    assert_eq!(mock_reclaims(), 0);
}

/// The mock is not the VM's ZGC heap: the dispatch takes the one-attempt
/// arms there (the collecting arms are reached only through
/// `upcall_args_collect`).
#[test]
fn the_gate_is_closed_off_zgc() {
    let ctx = mock_ctx();
    assert!(!upcall_args_collect(&ctx));
}
