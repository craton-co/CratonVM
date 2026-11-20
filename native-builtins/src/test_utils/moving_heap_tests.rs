use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

fn panics_stale(f: impl FnOnce()) -> bool {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(()) => false,
        Err(e) => {
            let text = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            text.contains("stale ObjectRef")
        }
    }
}

/// Off by default: nothing moves, the collection count stays 0, an
/// address is its identity hash -- exactly what every existing test saw.
#[test]
fn the_default_mode_never_moves() {
    let mut ctx = MockNativeContext::new();
    let a = ctx.create_string("a");
    let _ = ctx.create_string("b");
    assert_eq!(ctx.relocations_for_test(), 0);
    assert_eq!(ctx.gc_collection_count(), 0);
    assert_eq!(ctx.read_string(a).as_deref(), Some("a"));
    assert_eq!(ctx.identity_hash_code(a), a.as_ptr() as i32);
}

/// An allocation moves every object; roots (pins, global roots) and
/// reference fields follow, the old address is retired, the identity
/// hash stays, and the collection count moves.
#[test]
fn an_allocation_moves_every_object_and_forwards_the_roots() {
    let mut ctx = MockNativeContext::new();
    let a = ctx.create_string("a");
    let hash = ctx.identity_hash_code(a);
    let global = ctx.add_global_root(a);
    let pin = ctx.pin_native_root(a);
    ctx.set_moving_heap(true);
    let _ = ctx.alloc_object(ClassId::new(0), 1);
    assert_eq!(ctx.relocations_for_test(), 1);
    assert_eq!(ctx.gc_collection_count(), 1);
    let moved = ctx
        .resolve_global_root(global)
        .expect("the global root is kept");
    assert_ne!(moved, a, "the object moved");
    assert_eq!(ctx.read_native_pin(pin, a), moved, "the pin was forwarded");
    // The String's char[] field was forwarded with it.
    assert_eq!(ctx.read_string(moved).as_deref(), Some("a"));
    assert_eq!(ctx.identity_hash_code(moved), hash);
    assert!(panics_stale(|| {
        let _ = ctx.get_field(a, 0);
    }));
    ctx.unpin_native_roots(pin);
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// Storing a retired reference into a live object is the defect's other
/// half (the store succeeds in a real VM and the field dangles).
#[test]
fn storing_a_stale_reference_panics() {
    let mut ctx = MockNativeContext::new();
    let a = ctx.create_string("a");
    ctx.set_moving_heap(true);
    let holder = ctx.alloc_object(ClassId::new(0), 1);
    assert!(panics_stale(|| ctx.set_field(
        holder,
        0,
        Value::Object(Some(a))
    )));
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    assert!(panics_stale(|| ctx.set_array_element(
        arr,
        0,
        Value::Object(Some(a))
    )));
}

/// A Java call is a GC point: the callee sees its receiver and arguments
/// at their new addresses, and a scripted result is forwarded too.
#[test]
fn a_java_call_moves_the_heap_and_forwards_its_arguments() {
    fn hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        _name: &str,
        _desc: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        // Both current: a stale one panics here.
        let _ = ctx.class_id_of_object(receiver);
        match args.first() {
            Some(Value::Object(Some(o))) => Some(Ok(Some(Value::Object(Some(*o))))),
            _ => None,
        }
    }
    let mut ctx = MockNativeContext::new();
    let receiver = ctx.create_string("r");
    let arg = ctx.create_string("x");
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_moving_heap(true);
    let answer = ctx.invoke_virtual(
        receiver,
        "m",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(arg))],
    );
    assert_eq!(ctx.relocations_for_test(), 1);
    match answer {
        Ok(Some(Value::Object(Some(o)))) => {
            assert_ne!(o, arg, "the argument moved");
            assert_eq!(ctx.read_string(o).as_deref(), Some("x"));
        }
        other => panic!("{other:?}"),
    }
}

/// Round 17 wave 2 (lane funnel): the VM-rule mode moves the heap at a
/// reclaim and at a Java call, never at an allocation, so an unpinned
/// reference held across allocations stays current; switching back to
/// the strict mode restores the allocation GC point.
#[test]
fn the_vm_rule_mode_moves_at_java_calls_and_reclaims_only() {
    let mut ctx = MockNativeContext::new();
    let a = ctx.create_string("a");
    ctx.set_moving_heap_vm_rule(true);
    let holder = ctx.alloc_object(ClassId::new(0), 1);
    let _ = ctx.new_array(ArrayElementType::Int, 4);
    let _ = ctx.create_string("b");
    assert_eq!(
        ctx.relocations_for_test(),
        0,
        "an allocation is no GC point"
    );
    ctx.set_field(holder, 0, Value::Object(Some(a)));
    assert_eq!(ctx.read_string(a).as_deref(), Some("a"));
    let pin = ctx.pin_native_root(holder);
    let _ = ctx.reclaim_before_alloc_retry();
    assert_eq!(ctx.relocations_for_test(), 1, "a reclaim is a GC point");
    let moved = ctx.read_native_pin(pin, holder);
    assert_ne!(moved, holder, "the pinned holder was forwarded");
    assert!(panics_stale(|| {
        let _ = ctx.get_field(holder, 0);
    }));
    ctx.unpin_native_roots(pin);
    let _ = ctx.invoke("x/Y", "m", "()V", &[]);
    assert_eq!(ctx.relocations_for_test(), 2, "a Java call is a GC point");
    ctx.set_moving_heap(true);
    let _ = ctx.alloc_object(ClassId::new(0), 1);
    assert_eq!(ctx.relocations_for_test(), 3, "strict again");
}

/// The VM's `init_string_from_units` allocates without a GC point, so
/// its receiver stays where it is.
#[test]
fn init_string_from_units_is_no_gc_point() {
    let mut ctx = MockNativeContext::new();
    let s = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_moving_heap(true);
    assert!(ctx.init_string_from_units(s, &[0x68, 0x69]));
    assert_eq!(ctx.relocations_for_test(), 0);
    assert_eq!(ctx.read_string(s).as_deref(), Some("hi"));
}
