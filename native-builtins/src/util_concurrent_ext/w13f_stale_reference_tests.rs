use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;
use std::cell::{Cell, RefCell};

const CSLM: &str = "java/util/concurrent/ConcurrentSkipListMap";
const CF: &str = "java/util/concurrent/CompletableFuture";

thread_local! {
    /// (old, new) addresses the hook moves at the first `compareTo`.
    static W13F_MOVES: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    /// `compareTo` calls seen so far.
    static W13F_CALLS: Cell<usize> = const { Cell::new(0) };
    /// The key argument of the last `compareTo`.
    static W13F_SEEN_KEY: Cell<usize> = const { Cell::new(0) };
    /// (moved stage, apply result) for the `thenApply` test.
    static W13F_CF: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

fn w13f_apply_moves(ctx: &mut MockNativeContext) {
    let moves = W13F_MOVES.with(|m| std::mem::take(&mut *m.borrow_mut()));
    for (old, new) in moves {
        ctx.remap_native_pin_addr_for_test(old, new);
    }
}

fn w13f_record_key(args: &[Value]) {
    if let Some(Value::Object(Some(key))) = args.first() {
        W13F_SEEN_KEY.with(|c| c.set(key.as_ptr() as usize));
    }
}

/// First probe moves everything and answers "equal".
fn w13f_move_then_match(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "compareTo" {
        return None;
    }
    w13f_apply_moves(ctx);
    w13f_record_key(args);
    Some(Ok(Some(Value::Int(0))))
}

/// First probe moves everything and answers "less"; the second answers
/// "equal".
fn w13f_move_then_match_second(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "compareTo" {
        return None;
    }
    let call = W13F_CALLS.with(|c| {
        let n = c.get();
        c.set(n + 1);
        n
    });
    if call == 0 {
        w13f_apply_moves(ctx);
        return Some(Ok(Some(Value::Int(-1))));
    }
    w13f_record_key(args);
    Some(Ok(Some(Value::Int(0))))
}

fn w13f_array(ctx: &mut MockNativeContext, elems: &[Value]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Reference, 4);
    for i in 0..4 {
        let v = elems.get(i).copied().unwrap_or(Value::Object(None));
        ctx.set_array_element(arr, i, v);
    }
    arr
}

fn w13f_map(
    ctx: &mut MockNativeContext,
    keys: ObjectRef,
    vals: ObjectRef,
    size: i32,
) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CSLM).unwrap();
    let map = ctx.alloc_object(cid, 3);
    ctx.set_field(map, 0, Value::Object(Some(keys)));
    ctx.set_field(map, 1, Value::Object(Some(vals)));
    ctx.set_field(map, 2, Value::Int(size));
    map
}

fn w13f_cslm(method: &str, descriptor: &str) -> cratonvm_native_api::NativeCallback {
    let mut registry = NativeMethodRegistry::new();
    register_t31_concurrent_extras(&mut registry);
    registry
        .find(CSLM, method, descriptor)
        .expect("ConcurrentSkipListMap native should be registered")
}

/// `remove(key)`: the map, both arrays and the key move while the first
/// `compareTo` runs. The shift and the size store must land on the moved
/// copies; the vacated ones stay untouched.
#[test]
fn w13f_cslm_remove_shifts_the_arrays_moved_by_compare_to() {
    let remove = w13f_cslm("remove", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF13_0201);
    let (k0, k1) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let (v0, v1) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let entries_k = [Value::Object(Some(k0)), Value::Object(Some(k1))];
    let entries_v = [Value::Object(Some(v0)), Value::Object(Some(v1))];
    let keys = w13f_array(&mut ctx, &entries_k);
    let vals = w13f_array(&mut ctx, &entries_v);
    let moved_keys = w13f_array(&mut ctx, &entries_k);
    let moved_vals = w13f_array(&mut ctx, &entries_v);
    let this = w13f_map(&mut ctx, keys, vals, 2);
    let moved_this = w13f_map(&mut ctx, moved_keys, moved_vals, 2);
    let key = ctx.fresh_object_ref();
    let moved_key = ctx.fresh_object_ref();
    W13F_MOVES.with(|m| {
        *m.borrow_mut() = vec![
            (this.as_ptr() as usize, moved_this.as_ptr() as usize),
            (keys.as_ptr() as usize, moved_keys.as_ptr() as usize),
            (vals.as_ptr() as usize, moved_vals.as_ptr() as usize),
            (key.as_ptr() as usize, moved_key.as_ptr() as usize),
        ]
    });
    ctx.set_invoke_virtual_hook(w13f_move_then_match);

    let old = remove(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(key))],
    )
    .unwrap();

    assert_eq!(old, Some(Value::Object(Some(v0))));
    assert_eq!(
        ctx.get_field(moved_this, 2),
        Value::Int(1),
        "size on the moved map"
    );
    assert_eq!(
        ctx.get_field(this, 2),
        Value::Int(2),
        "the vacated map must not be written"
    );
    assert_eq!(
        ctx.get_array_element(moved_keys, 0),
        Value::Object(Some(k1))
    );
    assert_eq!(ctx.get_array_element(moved_keys, 1), Value::Object(None));
    assert_eq!(
        ctx.get_array_element(moved_vals, 0),
        Value::Object(Some(v1))
    );
    assert_eq!(
        ctx.get_array_element(keys, 0),
        Value::Object(Some(k0)),
        "the vacated key array must not be shifted"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `get(key)`: everything moves during the first probe; the second probe
/// must pass the MOVED key and the hit must be read from the MOVED values.
#[test]
fn w13f_cslm_get_probes_with_the_key_moved_by_compare_to() {
    let get = w13f_cslm("get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF13_0202);
    let (k0, k1) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let v1 = ctx.fresh_object_ref();
    let entries_k = [Value::Object(Some(k0)), Value::Object(Some(k1))];
    let keys = w13f_array(&mut ctx, &entries_k);
    let moved_keys = w13f_array(&mut ctx, &entries_k);
    // The vacated value array no longer holds the hit; the moved one does.
    let vals = w13f_array(&mut ctx, &[]);
    let moved_vals = w13f_array(&mut ctx, &[Value::Object(None), Value::Object(Some(v1))]);
    let this = w13f_map(&mut ctx, keys, vals, 2);
    let key = ctx.fresh_object_ref();
    let moved_key = ctx.fresh_object_ref();
    W13F_MOVES.with(|m| {
        *m.borrow_mut() = vec![
            (keys.as_ptr() as usize, moved_keys.as_ptr() as usize),
            (vals.as_ptr() as usize, moved_vals.as_ptr() as usize),
            (key.as_ptr() as usize, moved_key.as_ptr() as usize),
        ]
    });
    W13F_CALLS.with(|c| c.set(0));
    W13F_SEEN_KEY.with(|c| c.set(0));
    ctx.set_invoke_virtual_hook(w13f_move_then_match_second);

    let hit = get(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(key))],
    )
    .unwrap();

    assert_eq!(
        hit,
        Some(Value::Object(Some(v1))),
        "read from the moved values"
    );
    assert_eq!(
        W13F_SEEN_KEY.with(Cell::get),
        moved_key.as_ptr() as usize,
        "the second probe must pass the moved key"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// While the Function's `apply` runs, move the new stage wherever the
/// native rooted it, and answer a fresh result.
fn w13f_move_stage_on_apply(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "apply" {
        return None;
    }
    let cf_cid = ctx.ensure_class_initialized(CF).unwrap();
    let mut moved = 0usize;
    for i in 0..ctx.native_pin_count_for_test() {
        let pinned = ctx.read_native_pin(i, receiver);
        if ctx.class_id_of_object(pinned) == cf_cid {
            let copy = ctx.alloc_object(cf_cid, 4);
            ctx.remap_native_pin_addr_for_test(
                pinned.as_ptr() as usize,
                copy.as_ptr() as usize,
            );
            moved = copy.as_ptr() as usize;
            break;
        }
    }
    let result = ctx.fresh_object_ref();
    W13F_CF.with(|c| c.set((moved, result.as_ptr() as usize)));
    Some(Ok(Some(Value::Object(Some(result)))))
}

/// `thenApply(fn)` on a completed synthetic future: the new stage is
/// moved while `apply` runs, so the result and the done flag must be
/// written to — and the native must return — the MOVED stage.
#[test]
fn w13f_then_apply_completes_the_stage_moved_by_apply() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF13_0203);
    // Slot 1 an Int: the synthetic (non-real-JDK) future layout.
    let this = ctx.fresh_object_ref();
    ctx.set_field(this, FUT_FIELD_DONE, Value::Int(1));
    let func = ctx.fresh_object_ref();
    W13F_CF.with(|c| c.set((0, 0)));
    ctx.set_invoke_virtual_hook(w13f_move_stage_on_apply);

    let out = native_cf_then_apply(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(func))],
    )
    .unwrap();

    let (moved, result) = W13F_CF.with(Cell::get);
    assert_ne!(
        moved, 0,
        "the new stage must have been rooted across `apply`"
    );
    let Some(Value::Object(Some(stage))) = out else {
        panic!("thenApply must return a stage, got {out:?}");
    };
    assert_eq!(
        stage.as_ptr() as usize,
        moved,
        "the MOVED stage is returned"
    );
    match ctx.get_field(stage, FUT_FIELD_RESULT) {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr() as usize, result),
        other => panic!("the result must land on the moved stage, got {other:?}"),
    }
    assert_eq!(ctx.get_field(stage, FUT_FIELD_DONE), Value::Int(1));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
