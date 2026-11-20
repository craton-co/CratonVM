use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;
use std::cell::{Cell, RefCell};

thread_local! {
    /// (old, new) address pairs the first hooked call moves.
    static W20B_MOVES: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    static W20B_MOVED: Cell<bool> = const { Cell::new(false) };
    /// The `accept` arguments, in order.
    static W20B_ACCEPTED: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

/// Clears the thread-locals even when an assertion panics.
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        W20B_MOVES.with(|m| m.borrow_mut().clear());
        W20B_MOVED.with(|m| m.set(false));
        W20B_ACCEPTED.with(|a| a.borrow_mut().clear());
    }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// Remap every registered pair once, and clobber each vacated copy with
/// `clobber` (applied to the OLD address).
fn move_once(ctx: &mut MockNativeContext, clobber: fn(&mut MockNativeContext, ObjectRef)) {
    if W20B_MOVED.with(|m| m.replace(true)) {
        return;
    }
    let moves = W20B_MOVES.with(|m| m.borrow().clone());
    for (old, new) in moves {
        ctx.remap_native_pin_addr_for_test(old, new);
        // SAFETY: `old` is the address of a live mock object.
        clobber(ctx, unsafe { ObjectRef::from_raw(old as *mut u8) });
    }
}

fn key(ctx: &MockNativeContext, o: ObjectRef) -> i32 {
    ctx.get_field(o, 0).as_int().unwrap_or(i32::MIN)
}

/// A comparable whose key is field 0; `compareTo` answers the key
/// difference, THEN moves every registered object.
fn compare_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "compareTo" {
        return None;
    }
    let Some(Value::Object(Some(other))) = args.first().copied() else {
        return Some(Ok(Some(Value::Int(0))));
    };
    let answer = key(ctx, receiver) - key(ctx, other);
    move_once(ctx, |ctx, old| {
        if ctx.object_is_array(old) {
            for i in 0..ctx.array_length(old) {
                ctx.set_array_element(old, i, Value::Object(None));
            }
        } else {
            ctx.set_field(old, 0, Value::Int(1000));
        }
    });
    Some(Ok(Some(Value::Int(answer))))
}

fn keyed(ctx: &mut MockNativeContext, k: i32) -> ObjectRef {
    let o = ctx.fresh_object_ref();
    ctx.set_field(o, 0, Value::Int(k));
    o
}

fn ref_array(ctx: &mut MockNativeContext, elems: &[ObjectRef]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Reference, elems.len());
    for (i, &e) in elems.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(e)));
    }
    arr
}

/// `Arrays.parallelSort(Object[])`: the snapshot is of handles, so the
/// comparisons after the first move see the moved elements, and the
/// sorted result is written into the moved array.
#[test]
fn w20b_parallel_sort_compares_and_stores_the_moved_elements() {
    let _reset = Reset;
    let mut ctx = mock_ctx();
    let (o3, o1, o2) = (keyed(&mut ctx, 3), keyed(&mut ctx, 1), keyed(&mut ctx, 2));
    let (n3, n1, n2) = (keyed(&mut ctx, 3), keyed(&mut ctx, 1), keyed(&mut ctx, 2));
    let arr_old = ref_array(&mut ctx, &[o3, o1, o2]);
    let arr_new = ref_array(&mut ctx, &[n3, n1, n2]);
    W20B_MOVES.with(|m| {
        *m.borrow_mut() = vec![
            (addr(o3), addr(n3)),
            (addr(o1), addr(n1)),
            (addr(o2), addr(n2)),
            (addr(arr_old), addr(arr_new)),
        ]
    });
    ctx.set_invoke_virtual_hook(compare_hook);

    parallel_sort_objects_slice(&mut ctx, arr_old, 0, 3).expect("the sort completes");

    let sorted: Vec<Value> = (0..3).map(|i| ctx.get_array_element(arr_new, i)).collect();
    assert_eq!(
        sorted,
        vec![
            Value::Object(Some(n1)),
            Value::Object(Some(n2)),
            Value::Object(Some(n3)),
        ],
        "the moved array holds the moved elements, in key order"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every root is released");
}

fn accept_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "accept" {
        return None;
    }
    let v = args.first().and_then(|v| v.as_int()).unwrap_or(i32::MIN);
    W20B_ACCEPTED.with(|a| a.borrow_mut().push(v));
    move_once(ctx, |ctx, old| {
        if ctx.object_is_array(old) {
            for i in 0..ctx.array_length(old) {
                ctx.set_array_element(old, i, Value::Int(-1));
            }
        }
    });
    Some(Ok(None))
}

fn spliterator(ctx: &mut MockNativeContext, data: ObjectRef) -> ObjectRef {
    let o = ctx.fresh_object_ref();
    ctx.set_field(o, SPL_FIELD_DATA, Value::Object(Some(data)));
    ctx.set_field(o, SPL_FIELD_CURSOR, Value::Int(0));
    o
}

fn int_array(ctx: &mut MockNativeContext, values: &[i32]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Int, values.len());
    for (i, &v) in values.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(v));
    }
    arr
}

/// `Spliterator.OfInt.forEachRemaining`: every element after the first
/// `accept` is read from the moved array, and the cursor lands on the
/// moved receiver.
#[test]
fn w20b_primitive_for_each_remaining_reads_and_stores_through_the_moved_copies() {
    let _reset = Reset;
    let mut ctx = mock_ctx();
    let data_old = int_array(&mut ctx, &[10, 20, 30]);
    let data_new = int_array(&mut ctx, &[10, 20, 30]);
    let this_old = spliterator(&mut ctx, data_old);
    let this_new = spliterator(&mut ctx, data_new);
    let consumer = ctx.fresh_object_ref();
    W20B_MOVES.with(|m| {
        *m.borrow_mut() = vec![
            (addr(this_old), addr(this_new)),
            (addr(data_old), addr(data_new)),
        ]
    });
    ctx.set_invoke_virtual_hook(accept_hook);

    spl_prim_for_each_remaining(
        &mut ctx,
        &[Value::Object(Some(this_old)), Value::Object(Some(consumer))],
        SplPrim::Int,
        "(I)V",
    )
    .expect("forEachRemaining completes");

    assert_eq!(W20B_ACCEPTED.with(|a| a.borrow().clone()), vec![10, 20, 30]);
    assert_eq!(ctx.get_field(this_new, SPL_FIELD_CURSOR), Value::Int(3));
    assert_eq!(ctx.get_field(this_old, SPL_FIELD_CURSOR), Value::Int(0));
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every root is released");
}
