//! gc-common w21-b
//! (`common-w20d-p57-read-path-java-fallback-leaves-callers-refs-stale-FIXED-20260923`):
//! a foreign Path (no String in its slot) is read
//! through its `toString()`, which is Java. The mock's `invoke_virtual`
//! hook plays a moving collection there by re-pointing the native pins
//! named in `W21B_MOVES`, and the readers must hand back -- and read
//! through -- the moved addresses.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::RefCell;

thread_local! {
    /// (old, new) address pairs the hook moves on the next `toString`.
    static W21B_MOVES: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    /// `toString` calls seen.
    static W21B_CALLS: RefCell<usize> = const { RefCell::new(0) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn w21b_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "toString" {
        return None;
    }
    W21B_CALLS.with(|c| *c.borrow_mut() += 1);
    for (old, new) in W21B_MOVES.with(|m| std::mem::take(&mut *m.borrow_mut())) {
        ctx.remap_native_pin_addr_for_test(old, new);
    }
    let s = ctx.create_string("/tmp/w21b-foreign");
    Some(Ok(Some(Value::Object(Some(s)))))
}

fn reset() {
    W21B_MOVES.with(|m| m.borrow_mut().clear());
    W21B_CALLS.with(|c| *c.borrow_mut() = 0);
}

fn calls() -> usize {
    W21B_CALLS.with(|c| *c.borrow())
}

#[test]
fn w21b_read_path_hands_back_the_foreign_path_to_string_moved() {
    reset();
    let mut ctx = MockNativeContext::new();
    // Slot 0 holds no String, so the read falls back to `toString()`.
    let old = ctx.fresh_object_ref();
    let new = ctx.fresh_object_ref();
    W21B_MOVES.with(|m| m.borrow_mut().push((addr(old), addr(new))));
    ctx.set_invoke_virtual_hook(w21b_hook);

    let pins = ctx.native_pin_count_for_test();
    let mut path = old;
    let text = p57_read_path(&mut ctx, &mut path);
    assert_eq!(text, "/tmp/w21b-foreign");
    assert_eq!(calls(), 1, "the foreign Path was asked its toString()");
    assert_eq!(
        addr(path),
        addr(new),
        "the Path is handed back at its moved address"
    );
    assert_eq!(ctx.native_pin_count_for_test(), pins, "the pin is released");
}

#[test]
fn w21b_read_path_of_a_slot_backed_path_runs_no_java_and_takes_no_pin() {
    reset();
    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(w21b_hook);
    let mut path = p57_alloc_path(&mut ctx, "/tmp/w21b-own").expect("alloc Path");
    let before = path;
    let pins = ctx.native_pin_count_for_test();
    assert_eq!(p57_read_path(&mut ctx, &mut path), "/tmp/w21b-own");
    assert_eq!(calls(), 0, "a slot-backed Path is read from its slot");
    assert_eq!(addr(path), addr(before));
    assert_eq!(ctx.native_pin_count_for_test(), pins);
}

#[test]
fn w21b_read_path_pair_reads_the_second_path_where_the_first_read_moved_it() {
    reset();
    let mut ctx = MockNativeContext::new();
    let foreign = ctx.fresh_object_ref();
    // `b` before and after the "collection": different texts, so a read
    // through the stale address is visible in the answer.
    let b_old = p57_alloc_path(&mut ctx, "/tmp/w21b-stale").expect("alloc Path");
    let b_new = p57_alloc_path(&mut ctx, "/tmp/w21b-moved").expect("alloc Path");
    W21B_MOVES.with(|m| m.borrow_mut().push((addr(b_old), addr(b_new))));
    ctx.set_invoke_virtual_hook(w21b_hook);

    let pins = ctx.native_pin_count_for_test();
    let (mut a, mut b) = (foreign, b_old);
    let (ta, tb) = p57_read_path_pair(&mut ctx, &mut a, &mut b);
    assert_eq!(ta, "/tmp/w21b-foreign");
    assert_eq!(
        tb, "/tmp/w21b-moved",
        "the second Path is read at its current address"
    );
    assert_eq!(addr(b), addr(b_new), "and handed back there");
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins,
        "both pins are released"
    );
}

#[test]
fn w21b_read_path_arg_hands_back_every_object_argument_moved() {
    reset();
    let mut ctx = MockNativeContext::new();
    let path_old = ctx.fresh_object_ref();
    let path_new = ctx.fresh_object_ref();
    let opt_old = ctx.fresh_object_ref();
    let opt_new = ctx.fresh_object_ref();
    W21B_MOVES.with(|m| {
        let mut m = m.borrow_mut();
        m.push((addr(path_old), addr(path_new)));
        m.push((addr(opt_old), addr(opt_new)));
    });
    ctx.set_invoke_virtual_hook(w21b_hook);

    let pins = ctx.native_pin_count_for_test();
    let args = [
        Value::Object(Some(path_old)),
        Value::Int(7),
        Value::Object(Some(opt_old)),
        Value::Object(None),
    ];
    let (text, current) = p57_read_path_arg(&mut ctx, &args, 0).expect("non-null Path");
    assert_eq!(text, "/tmp/w21b-foreign");
    assert!(
        matches!(current, std::borrow::Cow::Owned(_)),
        "a read that ran Java answers a refreshed copy"
    );
    assert!(matches!(current[0], Value::Object(Some(o)) if addr(o) == addr(path_new)));
    assert!(
        matches!(current[1], Value::Int(7)),
        "primitives are carried over"
    );
    assert!(
        matches!(current[2], Value::Object(Some(o)) if addr(o) == addr(opt_new)),
        "a later argument is handed back at its moved address"
    );
    assert!(matches!(current[3], Value::Object(None)));
    assert_eq!(ctx.native_pin_count_for_test(), pins, "the scope is closed");
}

#[test]
fn w21b_read_path_arg_borrows_args_for_a_slot_backed_path() {
    reset();
    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(w21b_hook);
    let path = p57_alloc_path(&mut ctx, "/tmp/w21b-own").expect("alloc Path");
    let args = [Value::Object(Some(path)), Value::Object(None)];
    let (text, current) = p57_read_path_arg(&mut ctx, &args, 0).expect("non-null Path");
    assert_eq!(text, "/tmp/w21b-own");
    assert!(matches!(current, std::borrow::Cow::Borrowed(_)));
    assert_eq!(calls(), 0);
}

#[test]
fn loaded_platform_path_is_read_without_a_native_layout_allocation() {
    reset();
    let mut ctx = MockNativeContext::new();
    let cid = ctx.declare_loaded_class(cratonvm_native_api::path_layout::impl_class());
    ctx.set_declared_fields(
        cid,
        vec![cratonvm_native_api::FieldMetadata {
            name: "path".to_string(),
            descriptor: if cfg!(windows) {
                "Ljava/lang/String;"
            } else {
                "[B"
            }
            .to_string(),
            access_flags: 0,
            slot_index: 32,
            declaring_class_id: cid,
            is_static: false,
        }],
    );
    let mut path = ctx.alloc_object(cid, 33);
    let raw = if cfg!(windows) {
        "C:\\ffm\\first"
    } else {
        "/tmp/ffm-first"
    };
    let text = if cfg!(windows) {
        // This is mock test input, not a VM-published interned string.
        ctx.create_string_uninterned(raw)
    } else {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, raw.len());
        ctx.write_byte_array_from(arr, 0, raw.as_bytes());
        arr
    };
    ctx.set_field(path, 32, Value::Object(Some(text)));
    ctx.set_invoke_virtual_hook(w21b_hook);
    let pins = ctx.native_pin_count_for_test();
    let expected = if cfg!(windows) { "C:/ffm/first" } else { raw };
    assert_eq!(p57_read_path(&mut ctx, &mut path), expected);
    let args = [Value::Object(Some(path)), Value::Object(None)];
    let (got, current) = p57_read_path_arg(&mut ctx, &args, 0).unwrap();
    assert_eq!(got, expected);
    assert!(matches!(current, std::borrow::Cow::Borrowed(_)));
    assert_eq!(calls(), 0, "loaded carrier fields require no Java fallback");
    assert_eq!(ctx.native_pin_count_for_test(), pins);
}

#[test]
fn w21b_read_path_arg_refuses_a_null_path_like_obj_arg() {
    reset();
    let mut ctx = MockNativeContext::new();
    let args = [Value::Object(None)];
    assert!(p57_read_path_arg(&mut ctx, &args, 0).is_err());
}
