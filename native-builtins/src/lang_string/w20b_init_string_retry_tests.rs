#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, mock_reclaims, mock_refuse_allocations, MockNativeContext};
use cratonvm_types::error::{RuntimeError, VmError};
use cratonvm_types::ObjectRef;

/// Clears the thread-local knob even when an assertion panics.
struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
    }
}

fn is_oom(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { .. }
        ))
    )
}

/// Whether the mock's `init_string_from_units` consumes a pending refusal.
fn mock_init_honours_refusals(ctx: &mut MockNativeContext) -> bool {
    let probe = ctx.fresh_object_ref();
    mock_refuse_allocations(1, false);
    let refused = !ctx.init_string_from_units(probe, &[0x41]);
    mock_refuse_allocations(0, false);
    refused
}

fn fresh_string(ctx: &mut MockNativeContext) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("java/lang/String").unwrap();
    ctx.alloc_object(cid, 4)
}

fn char_array(ctx: &mut MockNativeContext, units: &[u16]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, units.len());
    for (i, &u) in units.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(u as i32));
    }
    arr
}

/// The units the mock's in-place fill left in slot 0 (`None`: no value).
fn filled_units(ctx: &MockNativeContext, s: ObjectRef) -> Option<Vec<u16>> {
    let Value::Object(Some(arr)) = ctx.get_field(s, 0) else {
        return None;
    };
    Some(
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as u16,
                _ => 0,
            })
            .collect(),
    )
}

#[test]
fn w20b_char_array_ctor_fills_this_without_collecting() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let this = fresh_string(&mut ctx);
    let chars = char_array(&mut ctx, &[0x61, 0xD800, 0x62]);
    mock_refuse_allocations(0, false);
    let pins = ctx.native_pin_count_for_test();
    native_string_init_from_char_array(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(chars))],
    )
    .expect("an unrefused fill succeeds");
    assert_eq!(mock_reclaims(), 0, "nothing was refused, nothing collects");
    assert_eq!(filled_units(&ctx, this), Some(vec![0x61, 0xD800, 0x62]));
    assert_eq!(ctx.native_pin_count_for_test(), pins, "the pin is released");
}

#[test]
fn w20b_char_array_ctor_reclaims_once_and_fills_this() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    if !mock_init_honours_refusals(&mut ctx) {
        eprintln!("skipped: the mock's init_string_from_units ignores refusals");
        return;
    }
    let this = fresh_string(&mut ctx);
    let chars = char_array(&mut ctx, &[0x68, 0x69]);
    let pins = ctx.native_pin_count_for_test();
    mock_refuse_allocations(1, true);
    native_string_init_from_char_array(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(chars))],
    )
    .expect("the retry after the reclaim is served");
    assert_eq!(mock_reclaims(), 1);
    mock_refuse_allocations(0, false);
    assert_eq!(
        filled_units(&ctx, this),
        Some(vec![0x68, 0x69]),
        "the refused fill must not leave a null `value` behind"
    );
    assert_eq!(ctx.native_pin_count_for_test(), pins);
}

#[test]
fn w20b_char_array_range_ctor_raises_oome_when_the_reclaim_cannot_help() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    if !mock_init_honours_refusals(&mut ctx) {
        eprintln!("skipped: the mock's init_string_from_units ignores refusals");
        return;
    }
    let this = fresh_string(&mut ctx);
    let chars = char_array(&mut ctx, &[0x61, 0x62, 0x63]);
    let pins = ctx.native_pin_count_for_test();
    mock_refuse_allocations(1, false);
    let err = native_string_init_from_char_array_range(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(chars)),
            Value::Int(1),
            Value::Int(2),
        ],
    )
    .unwrap_err();
    assert!(is_oom(&err), "expected OutOfMemoryError, got {err:?}");
    assert_eq!(mock_reclaims(), 1);
    assert_eq!(ctx.native_pin_count_for_test(), pins);
}

/// `String(AbstractStringBuilder, Void)` (the real-JDK `toString()`
/// path) writes the builder's units through the shared in-place writer.
#[test]
fn w20b_abstract_string_builder_ctor_writes_the_builder_units_in_place() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    mock_refuse_allocations(0, false);
    let sb_cid = ctx
        .ensure_class_initialized("java/lang/StringBuilder")
        .unwrap();
    let sb = ctx.alloc_object(sb_cid, 4);
    let text = ctx.create_string("h\u{e9}!");
    native_sb_init_string(
        &mut ctx,
        &[Value::Object(Some(sb)), Value::Object(Some(text))],
    )
    .expect("the builder is initialised");
    let this = fresh_string(&mut ctx);
    let pins = ctx.native_pin_count_for_test();
    native_string_init_abstract_string_builder(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(sb)),
            Value::Object(None),
        ],
    )
    .expect("the constructor succeeds");
    assert_eq!(filled_units(&ctx, this), Some(vec![0x68, 0xE9, 0x21]));
    assert_eq!(mock_reclaims(), 0);
    assert_eq!(ctx.native_pin_count_for_test(), pins, "the pin is released");
}

#[test]
fn w20b_fill_retries_once_then_raises_oome() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    if !mock_init_honours_refusals(&mut ctx) {
        eprintln!("skipped: the mock's init_string_from_units ignores refusals");
        return;
    }
    let this = fresh_string(&mut ctx);
    mock_refuse_allocations(2, true);
    let err = init_string_from_units_or_oom(&mut ctx, this, &[0x41], "probe").unwrap_err();
    assert!(is_oom(&err), "expected OutOfMemoryError, got {err:?}");
    assert_eq!(mock_reclaims(), 1, "one reclaim, no spinning");
}
