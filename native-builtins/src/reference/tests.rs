use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::{FieldMetadata, NativeMethodRegistry};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

// -----------------------------------------------------------------------
// G49-1 — `isEnqueued()` and the sentinel that a reference slot eats.
//
// MEASURED before the fix, `cratonvm --real-jdk` against HotSpot
// 25.0.3+9-LTS: after an `enqueue()` that returned `true`, `isEnqueued()`
// answered `false` for WeakReference, SoftReference and PhantomReference
// alike. The JDK answers `true` for all three.
//
// The cause is the G30-1 coercion, read from the other end: slot 1 of a
// real `java/lang/ref/Reference` is `queue`, descriptor
// `Ljava/lang/ref/ReferenceQueue;`, so the `Int(1)` sentinel this native
// was matching on cannot survive in it — while the JDK's own `enqueue0`,
// which is what actually runs under `--real-jdk`, stores the live
// `ReferenceQueue.ENQUEUED` object there.
// -----------------------------------------------------------------------

/// Register `java/lang/ref/ReferenceQueue` in the mock with a static
/// `ENQUEUED` field, and return `(class_id, sentinel_object)`.
fn with_enqueued_sentinel(ctx: &mut MockNativeContext) -> (ClassId, ObjectRef) {
    let cid = ctx
        .ensure_class_initialized("java/lang/ref/ReferenceQueue")
        .expect("mock could not initialize java/lang/ref/ReferenceQueue");
    ctx.set_declared_fields(
        cid,
        vec![FieldMetadata {
            name: "ENQUEUED".to_string(),
            descriptor: "Ljava/lang/ref/ReferenceQueue;".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: cid,
            is_static: true,
        }],
    );
    let sentinel = ctx.alloc_object(cid, 3);
    ctx.set_static_field(cid, 0, Value::Object(Some(sentinel)));
    (cid, sentinel)
}

fn is_enqueued(ctx: &mut MockNativeContext, this: ObjectRef) -> bool {
    match native_ref_is_enqueued(ctx, &[Value::Object(Some(this))]) {
        Ok(Some(Value::Int(v))) => v != 0,
        other => panic!("isEnqueued did not answer a boolean: {other:?}"),
    }
}

/// **The pin.** A real-JDK-layout `Reference` whose `queue` slot holds
/// `ReferenceQueue.ENQUEUED` — exactly what `enqueue0` leaves behind — is
/// enqueued. Reverting to the `Int(1)`-only test makes this red.
#[test]
fn the_real_jdk_enqueued_sentinel_reads_as_enqueued() {
    let mut ctx = mock_ctx();
    let (_rq_cid, sentinel) = with_enqueued_sentinel(&mut ctx);
    let reference = ctx.alloc_object(ClassId::new(0), 4);
    ctx.set_field(reference, REF_FIELD_QUEUE, Value::Object(Some(sentinel)));
    assert!(
        is_enqueued(&mut ctx, reference),
        "queue == ReferenceQueue.ENQUEUED is the JDK's own definition of enqueued"
    );
    // Guard the fixture: the sentinel really is the static we planted.
    assert_eq!(reference_queue_enqueued_sentinel(&ctx), Some(sentinel));
}

/// The other half, so the test above cannot be satisfied by a function that
/// answers `true` for any reference. A `Reference` still holding its OWN
/// queue has not been enqueued yet — this is the state every
/// `new WeakReference(o, q)` starts in, and answering `true` here would
/// make `isEnqueued()` useless in the opposite direction.
#[test]
fn a_reference_still_holding_its_own_queue_is_not_enqueued() {
    let mut ctx = mock_ctx();
    let (rq_cid, _sentinel) = with_enqueued_sentinel(&mut ctx);
    let queue = ctx.alloc_object(rq_cid, 3);
    let reference = ctx.alloc_object(ClassId::new(0), 4);
    ctx.set_field(reference, REF_FIELD_QUEUE, Value::Object(Some(queue)));
    assert!(
        !is_enqueued(&mut ctx, reference),
        "a live queue reference that is not ENQUEUED means 'has a queue, not yet enqueued'"
    );
}

/// The synthetic two-slot shape must keep working. `vm/src/vm/tests.rs`'s
/// `s28_is_enqueued_lifecycle` drives exactly this path — `ClassId(0)`
/// objects with no `L` descriptor on slot 1, where `native_ref_enqueue`'s
/// non-real-layout arm writes `Int(1)` and nothing coerces it away.
#[test]
fn the_synthetic_int_sentinel_still_reads_as_enqueued() {
    let mut ctx = mock_ctx();
    with_enqueued_sentinel(&mut ctx);
    let reference = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(reference, REF_FIELD_QUEUE, Value::Int(1));
    assert!(is_enqueued(&mut ctx, reference));
}

/// `native_rq_poll` detaches by writing `Object(None)`, and a `Reference`
/// constructed without a queue never had one. Both are "not enqueued", and
/// so is the raw `Int(0)` a never-written slot decodes to (`gen_heap.rs`'s
/// R-niche rule — the shape behind the 670 benign reads in G49-1 §2).
#[test]
fn queueless_detached_and_never_written_all_read_as_not_enqueued() {
    let mut ctx = mock_ctx();
    with_enqueued_sentinel(&mut ctx);
    let detached = ctx.alloc_object(ClassId::new(0), 4);
    ctx.set_field(detached, REF_FIELD_QUEUE, Value::Object(None));
    assert!(!is_enqueued(&mut ctx, detached), "polled -> not enqueued");

    let untouched = ctx.alloc_object(ClassId::new(0), 4);
    assert!(
        !is_enqueued(&mut ctx, untouched),
        "a never-written slot reads Int(0), which is not the Int(1) sentinel"
    );
}

/// The lookup must degrade to `false`, never panic and never throw, when
/// `java/lang/ref/ReferenceQueue` is absent. This is what licenses
/// `reference_queue_enqueued_sentinel` to skip `ensure_class_initialized`
/// and stay free of GC-capable calls: with no class there is no queue
/// object, so there is nothing that could have been enqueued.
#[test]
fn an_unresolvable_sentinel_answers_not_enqueued_rather_than_failing() {
    let mut ctx = mock_ctx();
    assert_eq!(reference_queue_enqueued_sentinel(&ctx), None);
    let queue = ctx.alloc_object(ClassId::new(0), 3);
    let reference = ctx.alloc_object(ClassId::new(0), 4);
    ctx.set_field(reference, REF_FIELD_QUEUE, Value::Object(Some(queue)));
    assert!(!is_enqueued(&mut ctx, reference));
}

/// A null or absent receiver is `false`, not a panic — `isEnqueued` is
/// registered for four classes and the arg shape is not guaranteed.
#[test]
fn a_missing_receiver_answers_not_enqueued() {
    let mut ctx = mock_ctx();
    with_enqueued_sentinel(&mut ctx);
    assert!(matches!(
        native_ref_is_enqueued(&mut ctx, &[]),
        Ok(Some(Value::Int(0)))
    ));
    assert!(matches!(
        native_ref_is_enqueued(&mut ctx, &[Value::Object(None)]),
        Ok(Some(Value::Int(0)))
    ));
}

#[test]
fn register_reference_natives_registers_all_expected_entries() {
    let mut r = NativeMethodRegistry::new();
    register_reference_natives(&mut r);

    // A handful of representative entries across the four classes.
    assert!(r
        .find("java/lang/ref/Reference", "get0", "()Ljava/lang/Object;")
        .is_some());
    assert!(r
        .find("java/lang/ref/WeakReference", "get", "()Ljava/lang/Object;")
        .is_some());
    assert!(r
        .find("java/lang/ref/SoftReference", "get", "()Ljava/lang/Object;")
        .is_some());
    assert!(r
        .find(
            "java/lang/ref/PhantomReference",
            "get",
            "()Ljava/lang/Object;"
        )
        .is_some());
    assert!(r.find("java/lang/ref/Reference", "clear", "()V").is_some());
    assert!(r
        .find(
            "java/lang/ref/ReferenceQueue",
            "poll",
            "()Ljava/lang/ref/Reference;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/ref/ReferenceQueue",
            "remove",
            "()Ljava/lang/ref/Reference;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/ref/ReferenceQueue",
            "remove",
            "(J)Ljava/lang/ref/Reference;"
        )
        .is_some());
}

#[test]
fn phantom_ref_get_is_distinct_callback_returning_null() {
    // PhantomReference.get() is overridden (vs. the base) to return null
    // unconditionally per JDK contract. We verify the override is present
    // by checking both Weak and Phantom registrations succeed — the actual
    // null-return behaviour is covered by s28_phantom_ref_get_always_returns_null
    // in the vm crate.
    let mut r = NativeMethodRegistry::new();
    register_reference_natives(&mut r);
    assert!(r
        .find(
            "java/lang/ref/PhantomReference",
            "get",
            "()Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find("java/lang/ref/WeakReference", "get", "()Ljava/lang/Object;")
        .is_some());
}

// -----------------------------------------------------------------------
// W2-F-1 — `ReferenceQueue.remove()` must never answer null.
//
// The JDK Finalizer thread does `Finalizer f = (Finalizer) queue.remove();
// f.runFinalizer(jla);`. The untimed wait used to give up after 60 s and
// return null, killing that thread with an NPE in every process that lived
// a minute. The mock's blocking region is a no-op, so an untimed wait on an
// empty queue would spin forever here: the tests drive the loop's two
// other exits (a reference, an interrupt) and the timed overload's
// deadline instead.
// -----------------------------------------------------------------------

/// A synthetic two-slot queue (head, size) holding `refs` in order.
fn synthetic_queue(ctx: &mut MockNativeContext, refs: usize) -> (ObjectRef, Vec<ObjectRef>) {
    let queue = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(queue, RQ_FIELD_HEAD, Value::Object(None));
    ctx.set_field(queue, RQ_FIELD_SIZE, Value::Int(0));
    let mut out = Vec::new();
    for _ in 0..refs {
        let reference = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(reference, REF_FIELD_QUEUE, Value::Object(Some(queue)));
        assert!(matches!(
            native_ref_enqueue(ctx, &[Value::Object(Some(reference))]),
            Ok(Some(Value::Int(1)))
        ));
        out.push(reference);
    }
    (queue, out)
}

#[test]
fn untimed_remove_on_an_empty_queue_throws_on_interrupt_instead_of_returning_null() {
    let mut ctx = mock_ctx();
    ctx.set_jdk_only(true);
    let (queue, _) = synthetic_queue(&mut ctx, 0);
    ctx.set_interrupted(true);
    let got = native_rq_remove_blocking(&mut ctx, &[Value::Object(Some(queue))]);
    assert!(
        got.is_err(),
        "an interrupted remove() on an empty queue must throw InterruptedException, got {got:?}"
    );
    assert!(
        !ctx.is_interrupted(false),
        "throwing InterruptedException clears the interrupt status"
    );
}

#[test]
fn remove_returns_an_available_reference_even_when_interrupted() {
    let mut ctx = mock_ctx();
    ctx.set_jdk_only(true);
    let (queue, refs) = synthetic_queue(&mut ctx, 1);
    ctx.set_interrupted(true);
    let got = native_rq_remove_blocking(&mut ctx, &[Value::Object(Some(queue))]);
    assert!(
        matches!(got, Ok(Some(Value::Object(Some(r)))) if r == refs[0]),
        "the JDK polls before it waits: an available reference wins over the interrupt"
    );
    assert!(
        ctx.is_interrupted(false),
        "a remove() that did not wait leaves the interrupt pending"
    );
}

/// JIT round 11 wave 12 (lane rt): `--compatible` answers like HotSpot too.
/// It used to cap an untimed `remove()` at 60 s with a null, which killed
/// the JDK Finalizer thread in every `--compatible` run longer than a minute
/// (`r11w11-orch-compatible-finalizer-thread-npe-FIXED-20260924.md`), and ignored
/// the interrupt flag. With the cap gone, the interrupt is the untimed wait's
/// only exit on an empty queue, so it must be honoured in this mode as well.
/// (The mock's `is_jdk_only` answers false unless `set_jdk_only(true)`.)
#[test]
fn compatible_mode_untimed_remove_throws_on_interrupt_instead_of_returning_null() {
    let mut ctx = mock_ctx();
    ctx.set_jdk_only(false);
    let (queue, _) = synthetic_queue(&mut ctx, 0);
    ctx.set_interrupted(true);
    let got = native_rq_remove_blocking(&mut ctx, &[Value::Object(Some(queue))]);
    assert!(
        got.is_err(),
        "--compatible: an interrupted remove() on an empty queue must throw, got {got:?}"
    );
    assert!(
        !ctx.is_interrupted(false),
        "throwing InterruptedException clears the interrupt status"
    );
}

#[test]
fn compatible_mode_timed_remove_throws_on_interrupt() {
    let mut ctx = mock_ctx();
    ctx.set_jdk_only(false);
    let (queue, _) = synthetic_queue(&mut ctx, 0);
    ctx.set_interrupted(true);
    let got =
        native_rq_remove_timeout(&mut ctx, &[Value::Object(Some(queue)), Value::Long(20)]);
    assert!(got.is_err(), "got {got:?}");
    assert!(!ctx.is_interrupted(false));
}

/// The source-level guard for the cap itself: the untimed wait is not a
/// test that can run to 60 s (the mock's blocking region is a no-op and an
/// empty queue would spin forever), so pin that no mode-dependent deadline
/// is built from `from_secs(60)` in `rq_remove_wait` any more.
#[test]
fn untimed_remove_carries_no_sixty_second_cap_in_any_mode() {
    let src = include_str!("../reference.rs");
    let start = src
        .find("fn rq_remove_wait(")
        .expect("rq_remove_wait exists");
    let end = start
        + src[start..]
            .find("\n}")
            .expect("rq_remove_wait has a closing brace");
    let body = &src[start..end];
    assert!(
        !body.contains("from_secs("),
        "rq_remove_wait must not build a capped deadline for the untimed remove()"
    );
}

#[test]
fn timed_remove_answers_null_only_at_its_deadline() {
    let mut ctx = mock_ctx();
    let (queue, _) = synthetic_queue(&mut ctx, 0);
    let start = std::time::Instant::now();
    let got =
        native_rq_remove_timeout(&mut ctx, &[Value::Object(Some(queue)), Value::Long(30)]);
    assert!(matches!(got, Ok(Some(Value::Object(None)))), "got {got:?}");
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(30),
        "a timed remove() may answer null only once its timeout has elapsed"
    );
}

#[test]
fn timed_remove_with_an_unrepresentable_deadline_does_not_panic() {
    let mut ctx = mock_ctx();
    let (queue, refs) = synthetic_queue(&mut ctx, 1);
    let got = native_rq_remove_timeout(
        &mut ctx,
        &[Value::Object(Some(queue)), Value::Long(i64::MAX)],
    );
    assert!(matches!(got, Ok(Some(Value::Object(Some(r)))) if r == refs[0]));
}

// -----------------------------------------------------------------------
// gc-common w6-d (2026-09-24) — a `--real-jdk` `remove` sleeps on
// `ReferenceQueue.lock` once the VM delivers the collector's wake-ups.
// The mock's `monitor_wait` returns at once and records the call, so these
// pin WHICH monitor is waited on and for how long, not the sleep itself.
// -----------------------------------------------------------------------

/// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
/// the `--compatible` poll's regions publish the queue's `lock` (what
/// JDK 25's `remove(timeout)` waits on), and nothing with the switch off
/// or for a queue with no `lock`.
#[test]
fn i68_compatible_poll_publishes_the_queue_lock() {
    let mut ctx = mock_ctx();
    let (queue, lock) = real_layout_empty_queue(&mut ctx);
    let got =
        native_rq_remove_timeout(&mut ctx, &[Value::Object(Some(queue)), Value::Long(20)]);
    assert!(matches!(got, Ok(Some(Value::Object(None)))), "got {got:?}");
    let published = ctx.region_wait_objects.borrow().clone();
    assert!(!published.is_empty(), "the poll opened publishing regions");
    for (obj, kind) in published {
        assert_eq!(obj, lock, "each nap publishes the queue's lock");
        assert_eq!(kind, rq_remove_block_kind(true));
    }
    assert!(
        ctx.monitor_wait_calls.borrow().is_empty(),
        "--compatible polls"
    );

    let mut off = mock_ctx();
    off.stand_in_wait_objects_answer.set(false);
    let (queue, _) = real_layout_empty_queue(&mut off);
    let _ = native_rq_remove_timeout(&mut off, &[Value::Object(Some(queue)), Value::Long(5)]);
    assert!(
        off.region_wait_objects.borrow().is_empty(),
        "=0 publishes nothing"
    );

    let mut synthetic = mock_ctx();
    let (queue, _) = synthetic_queue(&mut synthetic, 0);
    let _ = native_rq_remove_timeout(
        &mut synthetic,
        &[Value::Object(Some(queue)), Value::Long(5)],
    );
    assert!(
        synthetic.region_wait_objects.borrow().is_empty(),
        "a queue with no lock publishes nothing"
    );
}

/// An empty real-layout queue (`head`, `queueLength`, `lock`) and its lock.
fn real_layout_empty_queue(ctx: &mut MockNativeContext) -> (ObjectRef, ObjectRef) {
    let cid = ctx
        .ensure_class_initialized("java/lang/ref/ReferenceQueue")
        .expect("mock could not initialize java/lang/ref/ReferenceQueue");
    let field = |name: &str, descriptor: &str, slot: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            field("head", "Ljava/lang/ref/Reference;", 0),
            field("queueLength", "J", 1),
            field("lock", "Ljava/lang/ref/ReferenceQueue$Lock;", 2),
        ],
    );
    let queue = ctx.alloc_object(cid, 3);
    ctx.set_field(queue, RQ_FIELD_HEAD, Value::Object(None));
    ctx.set_field(queue, RQ_FIELD_SIZE, Value::Long(0));
    let lock = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(queue, 2, Value::Object(Some(lock)));
    (queue, lock)
}

#[test]
fn w6d_strict_remove_waits_on_the_queue_lock_when_wake_ups_are_delivered() {
    let mut ctx = mock_ctx();
    ctx.set_jdk_only(true);
    ctx.queue_wake_ups_delivered.set(true);
    let (queue, lock) = real_layout_empty_queue(&mut ctx);
    let got =
        native_rq_remove_timeout(&mut ctx, &[Value::Object(Some(queue)), Value::Long(40)]);
    assert!(matches!(got, Ok(Some(Value::Object(None)))), "got {got:?}");
    let waits = ctx.monitor_wait_calls.borrow();
    assert!(
        !waits.is_empty(),
        "an empty queue is waited on through its lock, not polled"
    );
    for &(obj, ms) in waits.iter() {
        assert_eq!(obj, lock, "the wait is on ReferenceQueue.lock");
        assert!(
            matches!(ms, Some(t) if (1..=40).contains(&t)),
            "each slice ({ms:?}) stays within the caller's timeout"
        );
    }
}

/// `--compatible` never asks, and a strict VM that does not (yet) deliver
/// the wake-ups keeps the poll: no monitor is waited on.
#[test]
fn w6d_remove_keeps_the_poll_unless_strict_and_delivered() {
    for (jdk_only, delivered) in [(false, true), (true, false), (false, false)] {
        let mut ctx = mock_ctx();
        ctx.set_jdk_only(jdk_only);
        ctx.queue_wake_ups_delivered.set(delivered);
        let (queue, _lock) = real_layout_empty_queue(&mut ctx);
        let got =
            native_rq_remove_timeout(&mut ctx, &[Value::Object(Some(queue)), Value::Long(15)]);
        assert!(
            matches!(got, Ok(Some(Value::Object(None)))),
            "jdk_only={jdk_only} delivered={delivered}: got {got:?}"
        );
        assert!(
            ctx.monitor_wait_calls.borrow().is_empty(),
            "jdk_only={jdk_only} delivered={delivered}: the poll must be kept"
        );
    }
}

/// The synthetic two-slot queue has no `lock`: even strict and delivered,
/// the loop falls back to the poll rather than waiting on nothing.
#[test]
fn w6d_a_queue_without_a_lock_is_polled() {
    let mut ctx = mock_ctx();
    ctx.set_jdk_only(true);
    ctx.queue_wake_ups_delivered.set(true);
    let (queue, _) = synthetic_queue(&mut ctx, 0);
    let got =
        native_rq_remove_timeout(&mut ctx, &[Value::Object(Some(queue)), Value::Long(15)]);
    assert!(matches!(got, Ok(Some(Value::Object(None)))), "got {got:?}");
    assert!(ctx.monitor_wait_calls.borrow().is_empty());
}
