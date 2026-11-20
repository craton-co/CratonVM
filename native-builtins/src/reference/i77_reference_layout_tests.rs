use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeCallback, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess,
};

fn field(
    name: &str,
    descriptor: &str,
    slot: usize,
    owner: ClassId,
    is_static: bool,
) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: owner,
        is_static,
    }
}

/// The real JDK 25 shapes: `Reference` (referent, queue, next,
/// discovered) and `ReferenceQueue` (head, queueLength, lock; statics
/// NULL_QUEUE, ENQUEUED), with the two sentinels planted. Returns
/// `(Reference, ReferenceQueue, NULL_QUEUE, ENQUEUED)`.
fn real_classes(ctx: &mut MockNativeContext) -> (ClassId, ClassId, ObjectRef, ObjectRef) {
    let reference = ctx
        .ensure_class_initialized("java/lang/ref/Reference")
        .expect("mock Reference");
    ctx.set_declared_fields(
        reference,
        vec![
            field("referent", "Ljava/lang/Object;", 0, reference, false),
            field(
                "queue",
                "Ljava/lang/ref/ReferenceQueue;",
                1,
                reference,
                false,
            ),
            field("next", "Ljava/lang/ref/Reference;", 2, reference, false),
            field(
                "discovered",
                "Ljava/lang/ref/Reference;",
                3,
                reference,
                false,
            ),
        ],
    );
    let rq = ctx
        .ensure_class_initialized("java/lang/ref/ReferenceQueue")
        .expect("mock ReferenceQueue");
    ctx.set_declared_fields(
        rq,
        vec![
            field("head", "Ljava/lang/ref/Reference;", 0, rq, false),
            field("queueLength", "J", 1, rq, false),
            field("lock", "Ljava/lang/ref/ReferenceQueue$Lock;", 2, rq, false),
            field("NULL_QUEUE", "Ljava/lang/ref/ReferenceQueue;", 0, rq, true),
            field("ENQUEUED", "Ljava/lang/ref/ReferenceQueue;", 1, rq, true),
        ],
    );
    let null_queue = ctx.alloc_object(rq, 3);
    let enqueued = ctx.alloc_object(rq, 3);
    ctx.set_static_field(rq, 0, Value::Object(Some(null_queue)));
    ctx.set_static_field(rq, 1, Value::Object(Some(enqueued)));
    (reference, rq, null_queue, enqueued)
}

#[test]
fn i77_the_layout_resolves_on_reference_itself() {
    let mut ctx = mock_ctx();
    assert_eq!(
        ReferenceLayout::resolve(&ctx),
        None,
        "no classes, no layout"
    );
    let (_reference, rq, _, _) = real_classes(&mut ctx);
    assert_eq!(
        ReferenceLayout::resolve(&ctx),
        Some(ReferenceLayout {
            referent: 0,
            queue: 1,
            queue_class: rq,
            null_queue: 0,
            enqueued: 1,
        })
    );
    // The mock has no per-VM slot: `reference_layout` resolves per call.
    assert_eq!(reference_layout(&ctx), ReferenceLayout::resolve(&ctx));
}

/// The retirement rule on the memoized layout is the by-name rule: a live
/// queue retires, `NULL_QUEUE`, `ENQUEUED` and a null / never-written
/// slot do not.
#[test]
fn i77_the_retirement_rule_matches_the_by_name_rule() {
    let mut ctx = mock_ctx();
    let (reference, rq, null_queue, enqueued) = real_classes(&mut ctx);
    let layout = ReferenceLayout::resolve(&ctx).expect("layout");
    let live_queue = ctx.alloc_object(rq, 3);
    for (queue, want) in [
        (Value::Object(Some(live_queue)), true),
        (Value::Object(Some(null_queue)), false),
        (Value::Object(Some(enqueued)), false),
        (Value::Object(None), false),
    ] {
        let r = ctx.alloc_object(reference, 4);
        ctx.set_field(r, 1, queue);
        assert_eq!(
            cleared_reference_needs_retire(&ctx, r, &layout),
            want,
            "queue {queue:?}"
        );
    }
    let untouched = ctx.alloc_object(reference, 4);
    assert!(!cleared_reference_needs_retire(&ctx, untouched, &layout));
}

/// `clear0` on the layout nulls `Reference.referent` and nothing else,
/// exactly as the by-name native does for a receiver that does not shadow
/// the field.
#[test]
fn i77_clear0_on_layout_clears_the_referent_like_the_by_name_native() {
    let mut ctx = mock_ctx();
    let (reference, rq, _, _) = real_classes(&mut ctx);
    let referent = ctx.alloc_object(ClassId::new(0), 1);
    let queue = ctx.alloc_object(rq, 3);
    let fresh = |ctx: &mut MockNativeContext| {
        let r = ctx.alloc_object(reference, 4);
        ctx.set_field(r, 0, Value::Object(Some(referent)));
        ctx.set_field(r, 1, Value::Object(Some(queue)));
        r
    };
    let memo = fresh(&mut ctx);
    assert!(clear0_on_layout(&mut ctx, memo));
    let by_name = fresh(&mut ctx);
    let _ = crate::native_reference_clear0(&mut ctx, &[Value::Object(Some(by_name))]);
    for r in [memo, by_name] {
        assert_eq!(ctx.get_field(r, 0), Value::Object(None), "referent cleared");
        assert_eq!(
            ctx.get_field(r, 1),
            Value::Object(Some(queue)),
            "queue kept"
        );
    }
}

/// A subclass declaring its own field named `referent` (legal Java:
/// `Reference.referent` is private). The JDK's `clear0` clears
/// `Reference.referent`; the by-name native resolves the name on the
/// receiver's class and clears the subclass's field instead
/// (`i77-L5-reference-clear0-writes-a-subclass-field-named-referent-20261110.md`).
/// The layout path clears `Reference`'s own slot, as HotSpot does.
#[test]
fn i77_a_shadowing_subclass_field_is_left_alone_on_the_layout_path() {
    let mut ctx = mock_ctx();
    let (reference, _rq, _, _) = real_classes(&mut ctx);
    let shadow = ctx
        .ensure_class_initialized("p/Shadow")
        .expect("mock Shadow");
    ctx.set_superclass(shadow, reference);
    ctx.set_declared_fields(
        shadow,
        vec![field("referent", "Ljava/lang/Object;", 4, shadow, false)],
    );
    let referent = ctx.alloc_object(ClassId::new(0), 1);
    let own = ctx.alloc_object(ClassId::new(0), 1);
    let fresh = |ctx: &mut MockNativeContext| {
        let r = ctx.alloc_object(shadow, 5);
        ctx.set_field(r, 0, Value::Object(Some(referent)));
        ctx.set_field(r, 4, Value::Object(Some(own)));
        r
    };
    let memo = fresh(&mut ctx);
    assert!(clear0_on_layout(&mut ctx, memo));
    assert_eq!(
        ctx.get_field(memo, 0),
        Value::Object(None),
        "Reference.referent cleared"
    );
    assert_eq!(
        ctx.get_field(memo, 4),
        Value::Object(Some(own)),
        "Shadow.referent untouched"
    );

    // The by-name native's answer today (the defect), pinned so a fix of
    // it shows up here.
    let by_name = fresh(&mut ctx);
    let _ = crate::native_reference_clear0(&mut ctx, &[Value::Object(Some(by_name))]);
    assert_eq!(ctx.get_field(by_name, 0), Value::Object(Some(referent)));
    assert_eq!(ctx.get_field(by_name, 4), Value::Object(None));
}

/// Wave 79: the constructor bridges (`ref_init_impl`) write
/// `Reference.referent` / `Reference.queue` by the layout's slots, so a
/// subclass declaring its own `referent` and `queue` fields keeps them
/// (HotSpot's `Reference(T, ReferenceQueue)` body is
/// `putfield java/lang/ref/Reference.referent` / `.queue`). A null queue
/// becomes `NULL_QUEUE`, as before.
#[test]
fn i79_ref_init_writes_reference_own_slots_past_a_shadowing_subclass() {
    let mut ctx = mock_ctx();
    let (reference, rq, null_queue, _) = real_classes(&mut ctx);
    let shadow = ctx
        .ensure_class_initialized("p/Shadow")
        .expect("mock Shadow");
    ctx.set_superclass(shadow, reference);
    ctx.set_declared_fields(
        shadow,
        vec![
            field("referent", "Ljava/lang/Object;", 4, shadow, false),
            field("queue", "Ljava/lang/Object;", 5, shadow, false),
        ],
    );
    let referent = ctx.alloc_object(ClassId::new(0), 1);
    let queue = ctx.alloc_object(rq, 3);
    let r = ctx.alloc_object(shadow, 6);
    ref_init_impl(
        &mut ctx,
        &[
            Value::Object(Some(r)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
        true,
    );
    assert_eq!(
        ctx.get_field(r, 0),
        Value::Object(Some(referent)),
        "Reference.referent"
    );
    assert_eq!(
        ctx.get_field(r, 1),
        Value::Object(Some(queue)),
        "Reference.queue"
    );
    assert_eq!(
        ctx.get_field(r, 4),
        Value::Int(0),
        "Shadow.referent untouched"
    );
    assert_eq!(ctx.get_field(r, 5), Value::Int(0), "Shadow.queue untouched");

    let q_less = ctx.alloc_object(shadow, 6);
    ref_init_impl(
        &mut ctx,
        &[Value::Object(Some(q_less)), Value::Object(Some(referent))],
        false,
    );
    assert_eq!(ctx.get_field(q_less, 0), Value::Object(Some(referent)));
    assert_eq!(
        ctx.get_field(q_less, 1),
        Value::Object(Some(null_queue)),
        "NULL_QUEUE"
    );
    assert_eq!(ctx.get_field(q_less, 5), Value::Int(0));
}

/// No real layout (the synthetic two-slot shape) or a receiver too small
/// for it: the layout path writes nothing and hands back to the by-name
/// native.
#[test]
fn i77_the_layout_path_declines_without_a_layout_or_a_fitting_receiver() {
    let mut ctx = mock_ctx();
    let synthetic = ctx.alloc_object(ClassId::new(0), 2);
    let referent = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(synthetic, 0, Value::Object(Some(referent)));
    assert!(!clear0_on_layout(&mut ctx, synthetic));
    assert_eq!(ctx.get_field(synthetic, 0), Value::Object(Some(referent)));

    let (reference, _, _, _) = real_classes(&mut ctx);
    let small = ctx.alloc_object(reference, 1);
    ctx.set_field(small, 0, Value::Object(Some(referent)));
    assert!(!clear0_on_layout(&mut ctx, small));
    assert_eq!(ctx.get_field(small, 0), Value::Object(Some(referent)));
}

/// The switch picks the registered `clear0` body (both values set
/// explicitly); the kind and the row count do not depend on it.
#[test]
fn i77_the_switch_picks_the_clear0_body() {
    let memo: NativeCallback = crate::native_reference_clear0_layout;
    let by_name: NativeCallback = crate::native_reference_clear0;
    for (on, want) in [(true, memo), (false, by_name)] {
        let mut r = NativeMethodRegistry::new();
        r.set_reference_layout_memo(on);
        assert_eq!(
            crate::reference_clear0_callback(&r) as usize,
            want as usize,
            "switch {on}"
        );
    }
}

/// The same switch picks the `clear()` and `enqueue()` bridge bodies of
/// the four reference classes (the rows a subclass's `clear()` /
/// `enqueue()` and `PhantomCleanable.clean` reach).
#[test]
fn i77_the_switch_picks_the_clear_and_enqueue_bodies() {
    let layout_clear: NativeCallback = native_ref_clear_layout;
    let by_name_clear: NativeCallback = native_ref_clear;
    let layout_enqueue: NativeCallback = native_ref_enqueue_layout;
    let by_name_enqueue: NativeCallback = native_ref_enqueue;
    for on in [true, false] {
        let mut r = NativeMethodRegistry::new();
        r.set_reference_layout_memo(on);
        register_reference_natives(&mut r);
        for class in [
            "java/lang/ref/Reference",
            "java/lang/ref/WeakReference",
            "java/lang/ref/SoftReference",
            "java/lang/ref/PhantomReference",
        ] {
            let clear = r.find(class, "clear", "()V").map(|c| c as usize);
            let enqueue = r.find(class, "enqueue", "()Z").map(|c| c as usize);
            let (want_clear, want_enqueue) = if on {
                (layout_clear, layout_enqueue)
            } else {
                (by_name_clear, by_name_enqueue)
            };
            assert_eq!(
                clear,
                Some(want_clear as usize),
                "{class} clear, switch {on}"
            );
            assert_eq!(
                enqueue,
                Some(want_enqueue as usize),
                "{class} enqueue, switch {on}"
            );
        }
    }
}

// ---- t1 wave 7, lane G6: every clear spelling reaches one registry hook ----

fn addr(r: ObjectRef) -> usize {
    r.as_ptr() as usize
}

/// `CRATONVM_REF_NOTES_PER_THREAD`: every application clear of the real
/// layout -- `clear0` on the memoized layout, the by-name `clear0` native,
/// and the `clear()` bridges' tail -- reaches exactly one registry hook: the
/// locked retire for a reference with a live queue, the lock-free note
/// cancel for one with `NULL_QUEUE`, `ENQUEUED` or no queue. Before wave 7
/// the queue-less clears returned with no hook at all, so a still-waiting
/// allocation note of a `ThreadLocal` entry became a row the next pause
/// bound only to drop.
#[test]
fn g6_every_real_layout_clear_retires_or_cancels() {
    let mut ctx = mock_ctx();
    let (reference, rq, null_queue, enqueued) = real_classes(&mut ctx);
    let live = ctx.alloc_object(rq, 3);
    let referent = ctx.alloc_object(ClassId::new(0), 1);
    let fresh = |ctx: &mut MockNativeContext, queue: Option<ObjectRef>| {
        let r = ctx.alloc_object(reference, 4);
        ctx.set_field(r, 0, Value::Object(Some(referent)));
        ctx.set_field(r, 1, Value::Object(queue));
        r
    };
    for (queue, hook) in [
        (Some(live), "retire"),
        (Some(null_queue), "cancel"),
        (Some(enqueued), "cancel"),
        (None, "cancel"),
    ] {
        let a = fresh(&mut ctx, queue);
        ctx.ref_hook_log.clear();
        assert!(clear0_on_layout(&mut ctx, a));
        assert_eq!(ctx.ref_hook_log, vec![(hook, addr(a))], "layout clear0, {queue:?}");
        assert_eq!(ctx.get_field(a, 0), Value::Object(None));

        let b = fresh(&mut ctx, queue);
        ctx.ref_hook_log.clear();
        let _ = crate::native_reference_clear0(&mut ctx, &[Value::Object(Some(b))]);
        assert_eq!(ctx.ref_hook_log, vec![(hook, addr(b))], "by-name clear0, {queue:?}");
        assert_eq!(ctx.get_field(b, 0), Value::Object(None));

        let c = fresh(&mut ctx, queue);
        ctx.ref_hook_log.clear();
        let _ = native_ref_clear_layout(&mut ctx, &[Value::Object(Some(c))]);
        assert_eq!(ctx.ref_hook_log, vec![(hook, addr(c))], "clear() bridge, {queue:?}");
    }
}

/// `enqueue()` that answers `false` has cleared the referent (JDK 25:
/// `clearImpl()` first), so it cancels a waiting note like a queue-less
/// `clear()`; it never takes the locked retire. Both bodies (layout and
/// by name).
#[test]
fn g6_a_false_enqueue_cancels_the_note() {
    let mut ctx = mock_ctx();
    let (reference, _rq, _, _) = real_classes(&mut ctx);
    let referent = ctx.alloc_object(ClassId::new(0), 1);
    for use_layout in [true, false] {
        let r = ctx.alloc_object(reference, 4);
        ctx.set_field(r, 0, Value::Object(Some(referent)));
        ctx.set_field(r, 1, Value::Object(None));
        ctx.ref_hook_log.clear();
        let layout = if use_layout {
            reference_layout(&ctx)
        } else {
            None
        };
        let out = ref_enqueue_impl(&mut ctx, &[Value::Object(Some(r))], layout);
        assert!(matches!(out, Ok(Some(Value::Int(0)))), "layout {use_layout}");
        assert_eq!(ctx.get_field(r, 0), Value::Object(None), "referent cleared");
        assert_eq!(ctx.ref_hook_log, vec![("cancel", addr(r))], "layout {use_layout}");
    }
}

/// The synthetic two-slot shape (no real `Reference` class): a queue
/// object retires, the `Int(1)` enqueued mark, a null queue and a
/// one-slot receiver cancel; a queue-less `enqueue()` cancels too.
#[test]
fn g6_the_synthetic_shape_retires_or_cancels() {
    let mut ctx = mock_ctx();
    let referent = ctx.alloc_object(ClassId::new(0), 1);
    let queue = ctx.alloc_object(ClassId::new(0), 3);
    for (slot, hook) in [
        (Value::Object(Some(queue)), "retire"),
        (Value::Int(1), "cancel"),
        (Value::Object(None), "cancel"),
    ] {
        let r = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(r, 0, Value::Object(Some(referent)));
        ctx.set_field(r, 1, slot);
        ctx.ref_hook_log.clear();
        let _ = native_ref_clear(&mut ctx, &[Value::Object(Some(r))]);
        assert_eq!(ctx.ref_hook_log, vec![(hook, addr(r))], "queue slot {slot:?}");
        assert_eq!(ctx.get_field(r, 0), Value::Object(None));
    }
    let small = ctx.alloc_object(ClassId::new(0), 1);
    ctx.ref_hook_log.clear();
    retire_cleared_reference(&mut ctx, small);
    assert_eq!(ctx.ref_hook_log, vec![("cancel", addr(small))]);

    let e = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(e, 0, Value::Object(Some(referent)));
    ctx.set_field(e, 1, Value::Object(None));
    ctx.ref_hook_log.clear();
    let out = ref_enqueue_impl(&mut ctx, &[Value::Object(Some(e))], None);
    assert!(matches!(out, Ok(Some(Value::Int(0)))));
    assert_eq!(ctx.get_field(e, 0), Value::Object(None));
    assert_eq!(ctx.ref_hook_log, vec![("cancel", addr(e))]);
}
