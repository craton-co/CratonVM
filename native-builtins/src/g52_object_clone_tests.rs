//! `native_object_clone`, pinned.
//!
//! `G45-1` §3 ranked this site fourth in the tree by descriptor-coercion
//! events (54 in a 19-vector sweep, 270 corpus-wide) and the standing
//! reading was that clone AMPLIFIES whatever the original store got
//! wrong. G52 measured it and that is not what it does — see the block
//! comment inside the `ObjectKind::Object` arm for the numbers, and
//! `docs/feature-designs/native-dispatch-architecture.md`
//! for the record.
//!
//! What these tests can and cannot reach: `MockNativeContext`'s
//! `get_field`/`set_field` are RAW, because the descriptor resolution
//! lives in `NativeContextImpl` (`vm/src/vm/vm_exec.rs`) and this crate
//! cannot start a VM. So the coercion half is pinned where it lives, in
//! `gc/src/heap.rs` (`the_descriptor_coercion_is_idempotent` is the
//! theorem this loop's double coercion rests on). What is pinned HERE is
//! the half the mock can see and the half a refactor is most likely to
//! break: that the copy is total, that it is field-for-field faithful
//! through a raw accessor pair, and that the array arm does not go
//! through the field path at all — which is where 4,533 of
//! `RJdkSecurity`'s 4,534 clones go.

use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_types::ArrayElementType;
// The heap/array accessors live on the component traits, not on the
// `NativeContext` bundle, so they must be in scope for method resolution
// on the concrete mock. Same import block as the other test modules here.
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn clone_of(ctx: &mut crate::test_utils::MockNativeContext, obj: ObjectRef) -> ObjectRef {
    match native_object_clone(ctx, &[Value::Object(Some(obj))]) {
        Ok(Some(Value::Object(Some(c)))) => c,
        other => panic!("clone must return an object reference, got {other:?}"),
    }
}

/// The copy is TOTAL and FAITHFUL. Every slot, in order, with the value
/// the source held — including the `Value` variant, which is the part
/// `Object.clone()`'s "copy the fields" contract turns on and the part a
/// descriptor-driven rewrite would silently change.
#[test]
fn object_clone_copies_every_slot_verbatim_through_a_raw_accessor_pair() {
    let mut ctx = mock_ctx();
    let src = ctx.alloc_object(ClassId::new(41), 6);
    let pointee = ctx.alloc_object(ClassId::new(42), 0);
    let planted = [
        Value::Int(7),
        Value::Long(-9),
        Value::Object(Some(pointee)),
        Value::Object(None),
        // The measured population: a raw `Int(0)` sitting in a slot the
        // real class declares a reference. Through the production
        // accessors this is the read that fires
        // `primitive-into-reference`; through the mock it must simply
        // survive, which is what proves the loop itself copies rather
        // than rewrites.
        Value::Int(0),
        Value::Float(1.5),
    ];
    for (i, v) in planted.iter().enumerate() {
        ctx.set_field(src, i, *v);
    }

    let dst = clone_of(&mut ctx, src);
    assert_ne!(
        dst.as_ptr(),
        src.as_ptr(),
        "clone must be a distinct object, not the receiver",
    );
    assert_eq!(
        ctx.object_num_fields(dst),
        planted.len(),
        "the clone must be as wide as the receiver; a short clone drops \
         the tail slots silently",
    );
    for (i, v) in planted.iter().enumerate() {
        assert_eq!(
            ctx.get_field(dst, i),
            *v,
            "slot {i} of the clone must equal slot {i} of the receiver",
        );
        assert_eq!(
            ctx.get_field(src, i),
            *v,
            "cloning must not disturb the receiver at slot {i}",
        );
    }
}

/// The clone is INDEPENDENT: writing one must not move the other. A
/// "clone" that handed back an alias would pass the equality test above
/// and fail every real use.
#[test]
fn writing_the_clone_does_not_move_the_receiver() {
    let mut ctx = mock_ctx();
    let src = ctx.alloc_object(ClassId::new(43), 2);
    ctx.set_field(src, 0, Value::Int(1));
    ctx.set_field(src, 1, Value::Int(2));

    let dst = clone_of(&mut ctx, src);
    ctx.set_field(dst, 0, Value::Int(99));

    assert_eq!(ctx.get_field(src, 0), Value::Int(1));
    assert_eq!(ctx.get_field(dst, 0), Value::Int(99));
    assert_eq!(ctx.get_field(dst, 1), Value::Int(2));
}

/// THE RATIO THAT DECIDES WHETHER THIS SITE IS WORTH ANY WORK. MEASURED,
/// `RJdkSecurity` `--real-jdk` under `CRATONVM_DBG_CLONE=1`: 4,534
/// clones, 4,533 of them arrays. The array arm uses
/// `get_array_element`/`set_array_element`, which are NOT
/// descriptor-aware and never reach
/// `gc/src/heap.rs::coerce_field_value_for_slot` — so the coercion
/// question does not touch 99.98% of what `Object.clone()` actually does
/// in that vector. This test pins that the array arm stays on the array
/// accessors: if someone "unifies" the two arms onto `get_field`, the
/// element type is lost and every array clone starts paying (and
/// reporting) a coercion.
#[test]
fn the_array_arm_copies_elements_and_keeps_the_element_type() {
    let mut ctx = mock_ctx();
    let src = ctx.new_array(ArrayElementType::Int, 4);
    for i in 0..4 {
        ctx.set_array_element(src, i, Value::Int(i as i32 * 10));
    }

    let dst = clone_of(&mut ctx, src);
    assert_ne!(dst.as_ptr(), src.as_ptr());
    assert_eq!(ctx.array_length(dst), 4);
    assert_eq!(ctx.heap_element_type_of(dst), ArrayElementType::Int);
    for i in 0..4 {
        assert_eq!(ctx.get_array_element(dst, i), Value::Int(i as i32 * 10));
    }
    assert_eq!(
        ctx.object_num_fields(dst),
        0,
        "an array clone must be an array, not an object with fields",
    );
}

/// A reference array clones through `new_ref_array`, so the clone is a
/// reference array and its elements are references — not the `Int(0)`
/// zero-init that `new_array` would have produced.
#[test]
fn a_reference_array_clones_as_a_reference_array() {
    let mut ctx = mock_ctx();
    let src = ctx.new_ref_array(ClassId::new(44), 3);
    let a = ctx.alloc_object(ClassId::new(45), 0);
    ctx.set_array_element(src, 1, Value::Object(Some(a)));

    let dst = clone_of(&mut ctx, src);
    assert_eq!(
        ctx.heap_element_type_of(dst),
        ArrayElementType::Reference,
        "a reference array must not clone into a primitive array",
    );
    assert_eq!(ctx.get_array_element(dst, 0), Value::Object(None));
    assert_eq!(ctx.get_array_element(dst, 1), Value::Object(Some(a)));
}

/// JLS §12.6 / HotSpot `JVM_Clone`: the clone of an instance is offered
/// to the finalizer registration hook exactly once, and the source is not
/// re-registered (gc-common w5-d, `handoff-w5d-clone-registers-finalizable`).
/// The VM's hook registers only a class with `finalize()`.
#[test]
fn object_clone_offers_the_clone_to_finalizer_registration_once() {
    let mut ctx = mock_ctx();
    let src = ctx.alloc_object(ClassId::new(41), 2);
    let dst = clone_of(&mut ctx, src);
    assert_eq!(*ctx.register_if_finalizable_calls.borrow(), vec![dst]);
}

/// `clone` on `null` is an NPE and not a panic, and not a clone of
/// nothing.
#[test]
fn clone_on_null_is_a_null_pointer_exception() {
    let mut ctx = mock_ctx();
    for arg in [vec![], vec![Value::Object(None)], vec![Value::Int(3)]] {
        assert!(
            native_object_clone(&mut ctx, &arg).is_err(),
            "clone on {arg:?} must raise, not fabricate an object",
        );
    }
}
