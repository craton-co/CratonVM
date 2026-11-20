#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::FieldMetadata;
use cratonvm_types::ClassId;

const SEGMENT: &str = "jdk/internal/foreign/NativeMemorySegmentImpl";
const SESSION: &str = "jdk/internal/foreign/MemorySessionImpl";

fn field(name: &str, desc: &str, slot: usize, owner: ClassId, is_static: bool) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: owner,
        is_static,
    }
}

/// A `--real-jdk` mock with the two JDK classes and `GLOBAL_SESSION` set.
fn jdk_ctx() -> (MockNativeContext, ClassId, ClassId) {
    let mut ctx = mock_ctx();
    ctx.set_jdk_only(true);
    let seg = ctx.ensure_class_initialized(SEGMENT).expect("segment class");
    let ses = ctx.ensure_class_initialized(SESSION).expect("session class");
    ctx.set_declared_fields(
        seg,
        vec![
            field("min", "J", 0, seg, false),
            field("length", "J", 1, seg, false),
            field("readOnly", "Z", 2, seg, false),
            field("scope", "Ljdk/internal/foreign/MemorySessionImpl;", 3, seg, false),
        ],
    );
    ctx.set_declared_fields(
        ses,
        vec![field("GLOBAL_SESSION", "Ljdk/internal/foreign/MemorySessionImpl;", 0, ses, true)],
    );
    let global = ctx.alloc_object(ses, 1);
    ctx.set_static_field(ses, 0, Value::Object(Some(global)));
    (ctx, seg, ses)
}

fn address(size: i64, align: i64) -> UpcallParam {
    UpcallParam {
        kind: UpcallKind::Address,
        target_size: size,
        target_align: align,
    }
}

fn int() -> UpcallParam {
    UpcallParam {
        kind: UpcallKind::Int,
        target_size: 0,
        target_align: 1,
    }
}

/// `(min, length, readOnly, scope)` of a segment value.
fn fields(ctx: &MockNativeContext, value: Value) -> (Value, Value, Value, Value) {
    let Value::Object(Some(seg)) = value else {
        panic!("not a segment: {value:?}");
    };
    (ctx.get_field(seg, 0), ctx.get_field(seg, 1), ctx.get_field(seg, 2), ctx.get_field(seg, 3))
}

/// The batch on a moving heap (every allocation relocates every object):
/// each address argument is a segment of its own address and target size
/// in the CURRENT global session, the primitive argument is untouched,
/// and no pin is left behind.
#[test]
fn batched_segments_are_the_jdk_segments_after_moves() {
    let (mut ctx, seg_class, ses) = jdk_ctx();
    ctx.set_moving_heap(true);
    let params = [address(4, 1), int(), address(0, 1)];
    let mut values = [Value::Long(0x1000), Value::Int(-7), Value::Long(0x2008)];
    let cached = Default::default();
    let done = mint_address_args_batched(&mut ctx, &cached, &params, &mut values);
    assert!(matches!(done, Some(Ok(()))), "taken: {done:?}");
    assert_eq!(batched_first_class(&params, cached.get()), Some(seg_class));
    assert_eq!(batched_first_class(&[int(), address(4, 1)], cached.get()), None);
    assert_eq!(batched_first_class(&params, None), None);
    let Value::Object(Some(global)) = ctx.get_static_field(ses, 0) else {
        panic!("no session");
    };
    let scope = Value::Object(Some(global));
    assert_eq!(fields(&ctx, values[0]), (Value::Long(0x1000), Value::Long(4), Value::Int(0), scope));
    assert_eq!(values[1], Value::Int(-7));
    assert_eq!(fields(&ctx, values[2]), (Value::Long(0x2008), Value::Long(0), Value::Int(0), scope));
    let Value::Object(Some(first)) = values[0] else { unreachable!() };
    assert_eq!(ctx.class_id_of_object(first), seg_class);
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// The same arguments through the per-argument loop's mint
/// (`new_arg_segment`) answer the same fields: the batch changes the
/// cost, not the segment.
#[test]
fn batched_and_per_argument_mints_agree() {
    let (mut ctx, _, _) = jdk_ctx();
    let params = [address(4, 4), address(16, 8)];
    let mut values = [Value::Long(0x40), Value::Long(0x80)];
    assert!(matches!(
        mint_address_args_batched(&mut ctx, &Default::default(), &params, &mut values),
        Some(Ok(()))
    ));
    for (param, (value, raw)) in params.iter().zip(values.iter().zip([0x40i64, 0x80])) {
        let single = new_arg_segment(&mut ctx, raw, param.target_size).expect("minted");
        assert_eq!(fields(&ctx, *value), fields(&ctx, Value::Object(Some(single))));
    }
}

/// A misaligned address fails at its own position with HotSpot's
/// `Utils.longToAddress` message, after the earlier argument was minted,
/// and releases every pin.
#[test]
fn a_misaligned_address_fails_in_place_and_unpins() {
    let (mut ctx, _, _) = jdk_ctx();
    let params = [address(4, 4), address(4, 4)];
    let mut values = [Value::Long(0x40), Value::Long(0x42)];
    let err = match mint_address_args_batched(&mut ctx, &Default::default(), &params, &mut values) {
        Some(Err(err)) => err,
        other => panic!("expected a failure, got {other:?}"),
    };
    assert!(
        format!("{err:?}").contains("Invalid alignment constraint for address: 0x42"),
        "{err:?}"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}


/// The stub's mint is kept once resolved, and a refusal is not kept: a
/// stub that first ran before the mode answered resolves on a later call.
#[test]
fn the_stub_keeps_a_resolved_mint_only() {
    let (ctx, _, _) = jdk_ctx();
    let cached = std::sync::OnceLock::new();
    ctx.set_jdk_only(false);
    assert!(stub_segment_mint(&ctx, &cached).is_none());
    assert!(cached.get().is_none(), "a refusal is not kept");
    ctx.set_jdk_only(true);
    assert!(stub_segment_mint(&ctx, &cached).is_some());
    assert!(cached.get().is_some(), "kept once resolved");
}
