// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.ref.*` native method implementations.
//!
//! T16.8: Reference types (Reference / WeakReference / SoftReference /
//! PhantomReference) and ReferenceQueue. These natives maintain the
//! synthetic field layout used by the GC's reference-processing pipeline
//! (see `gc/src/reference.rs`).
//!
//! Extracted from `lib.rs` (Session 88 / T16.8). The block previously lived
//! between lib.rs lines ~9037 .. 9382 as `register_reference_natives` plus
//! its helpers (`ref_init_impl`, `discover_ref_from_args`, etc.).
//!
//! Layout conventions (mirrored in lib.rs for crate-wide visibility):
//! * Reference  : field 0 = referent, field 1 = queue
//! * RQ (queue) : field 0 = head-of-linked-list, field 1 = size
//!
//! Integration with the GC reference processor:
//! * Each `<init>` calls `ctx.discover_reference(ref_type, ref_obj, referent, queue)`
//!   so the GC knows to process the reference during its weak/soft/phantom
//!   sweep (see `RefProcessor` in gc/src/reference.rs).
//! * `PhantomReference.get()` always returns null per the JDK contract
//!   (JDK 1.2+). The other ref subclasses return the referent field.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

use crate::{REF_FIELD_NEXT, REF_FIELD_QUEUE, REF_FIELD_REFERENT, RQ_FIELD_HEAD, RQ_FIELD_SIZE};

/// Queue-linkage slot for a Reference: the real-JDK `next` field declared on
/// `java/lang/ref/Reference` itself (referent, queue, next, discovered) when
/// the object has one; the legacy referent-slot fallback otherwise (old
/// synthetic 2-field shape). Reusing the REFERENT slot as the next pointer
/// made `get()` on an enqueued-but-unpolled WeakReference return the NEXT
/// queue element instead of null. Both the enqueue and poll sides must
/// agree, so they share this.
///
/// MUST resolve BY NAME against `Reference`'s own declaring class rather
/// than assume a fixed index (the old `object_num_fields > REF_FIELD_NEXT`
/// heuristic): a `java.util.WeakHashMap$Entry` (itself a `WeakReference`
/// subclass) also declares its own field named `next` — its hash-BUCKET
/// chain pointer, a completely different linked list. Blindly using
/// `REF_FIELD_NEXT`'s index for such a subclass can splice the
/// ReferenceQueue link over the bucket-chain link, corrupting the bucket a
/// later `WeakHashMap.get()` walks forever. See the matching fix in
/// `vm/src/runtime/interpreter.rs`'s `gc_reference_next_slot` (the GC's own
/// auto-enqueue path must agree with this one) and
/// fixed-suite-bugs/springboot/thymeleaf-groovy-layoutdialect-metaclass-introspection-hang-FIXED.md.
fn ref_next_slot(ctx: &mut dyn NativeContext, ref_obj: cratonvm_types::ObjectRef) -> usize {
    if ctx.object_num_fields(ref_obj) <= REF_FIELD_NEXT {
        return REF_FIELD_REFERENT; // legacy synthetic 2-field shape
    }
    ctx.resolve_field_index("java/lang/ref/Reference", "next")
        .unwrap_or(REF_FIELD_NEXT)
}

/// Run `body` holding the `ReferenceQueue`'s own Java monitor, and hand it the
/// post-acquire receiver.
///
/// **Why this exists (H2 `TestMultiThread`, 2026-08-16).** The real JDK guards
/// `head`, `queueLength` and `Reference.next` with `ReferenceQueue.lock` — a
/// `ReentrantLock` taken by `enqueue0`, `poll` and `remove` alike. The natives
/// below REPLACE that bytecode, and until now supplied no exclusion of their
/// own: `native_rq_poll` reads `head`, reads `head.next`, then writes both back,
/// with nothing stopping a second thread from doing the same read in between.
/// Two threads then pop the SAME reference and both return it, and a third
/// interleaving loses a whole segment of the list.
///
/// Measured on this host, 8 threads × 400 phantom refs through one queue,
/// against HotSpot's 3200-delivered / 0-duplicate baseline: unguarded CratonVM
/// delivered 2882 with 152 duplicates; with the same natives serialized, 3186
/// with 22. H2 sees it as `CloseWatcher.pollUnclosed` returning a watcher it has
/// already removed from its `refs` set — or, when the crossed links strand a
/// reclaimed slot, as `ClassCastException: class java.lang.String cannot be cast
/// to class org.h2.util.CloseWatcher` out of a `ReferenceQueue.poll()`, which is
/// what `new JdbcConnection(...)` → `closeOld()` raises on a concurrent open.
///
/// The QUEUE object's own monitor, deliberately, rather than a new global lock:
/// it is per-queue (two unrelated queues never contend), it needs no new static
/// and no new `Mutex`, and no Java code competes for it — JDK 9+ `ReferenceQueue`
/// synchronizes on a private `ReentrantLock` field, never on `this`. The JDK's
/// lock therefore nests strictly INSIDE this one on the delegating enqueue path
/// (`native_ref_enqueue`'s real-layout arm) and never in the other order, so no
/// cycle is introduced.
///
/// Plain `monitor_enter`, not `monitor_enter_gc_safe`: the contended wait on the
/// ordinary path is not GC-blocked, so `queue` cannot move underneath it — but
/// the pin is taken anyway and the body is handed the re-read receiver, since
/// every caller here goes on to touch fields on it.
fn with_queue_monitor<T>(
    ctx: &mut dyn NativeContext,
    queue: ObjectRef,
    body: impl FnOnce(&mut dyn NativeContext, ObjectRef) -> T,
) -> T {
    let pin = ctx.pin_native_root(queue);
    let entered = ctx.read_native_pin(pin, queue);
    ctx.monitor_enter(entered);
    let out = body(&mut *ctx, entered);
    let entered = ctx.read_native_pin(pin, entered);
    ctx.monitor_exit(entered);
    ctx.unpin_native_roots(pin);
    out
}

/// Register every `java.lang.ref.*` native. Called from
/// `register_essential_natives` in `lib.rs`.
pub(crate) fn register_reference_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // =========================================================================
    // java/lang/ref/Reference (abstract base) and its subclasses
    // =========================================================================
    for class in &[
        "java/lang/ref/Reference",
        "java/lang/ref/WeakReference",
        "java/lang/ref/SoftReference",
        "java/lang/ref/PhantomReference",
    ] {
        registry.register(class, "get", "()Ljava/lang/Object;", native_ref_get);
        registry.register(class, "clear", "()V", native_ref_clear);
        registry.register(class, "enqueue", "()Z", native_ref_enqueue);
        registry.register(class, "isEnqueued", "()Z", native_ref_is_enqueued);
        registry.register(
            class,
            "refersTo",
            "(Ljava/lang/Object;)Z",
            native_ref_refers_to,
        );
    }

    // WeakReference constructors
    registry.register(
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        native_weak_ref_init,
    );
    registry.register(
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        native_weak_ref_init_queue,
    );
    // Brave stores `WeakKey` instances in ConcurrentHashMap but removes them
    // with the live TraceContext referent.  Its asymmetric Java `equals`
    // relies on ConcurrentHashMap's key comparison order.  Keep the bridge
    // explicit for the real-JDK reference layout so this path remains correct
    // even when a shared Object.equals call site was previously cached for a
    // different receiver shape.
    registry.register(
        "brave/internal/collect/WeakConcurrentMap$WeakKey",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_brave_weak_key_equals,
    );

    // Same underlying defect as the Brave bridge above (see its comment): a
    // shared/polymorphic `Object.equals()` call site inside
    // `ConcurrentHashMap`'s internals can resolve to the wrong override for a
    // rarely-exercised receiver class, silently falling back to identity
    // comparison. Mockito's `mockito-core`'s own `WeakConcurrentMap` vendors
    // the exact same WeakKey/LatentKey asymmetric-equals design as Brave's
    // (both libraries independently converged on the same pattern for a
    // GC-aware identity map), and hits the identical dispatch failure: a
    // freshly created mock's entry is unfindable via
    // `WeakConcurrentMap.get()` after `put()` genuinely stored it, because
    // `LatentKey.equals(WeakKey)` (or the reverse) never actually reaches
    // Mockito's own bytecode. Confirmed via `Mockito.verify(mock)` throwing
    // `NotAMockException` for a mock created and used successfully moments
    // earlier (`fixed-suite-bugs/springboot/servletcontextlistener-forkedclasspath-mockito-notamock-FIXED.md`
    // — note that doc's own conclusion: these bridges are a correct fix for a
    // real dispatch defect, but the `NotAMockException` they were first written
    // for had a second, independent cause in `Method.invoke`).
    // Bridge both directions explicitly, mirroring Mockito's own semantics.
    registry.register(
        "org/mockito/internal/util/concurrent/WeakConcurrentMap$WeakKey",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_mockito_weak_key_equals,
    );
    registry.register(
        "org/mockito/internal/util/concurrent/WeakConcurrentMap$LatentKey",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_mockito_latent_key_equals,
    );

    // SoftReference constructors
    registry.register(
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        native_soft_ref_init,
    );
    registry.register(
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        native_soft_ref_init_queue,
    );

    // PhantomReference constructor (always requires queue)
    registry.register(
        "java/lang/ref/PhantomReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        native_phantom_ref_init,
    );
    // PhantomReference.get() always returns null per JDK spec (override base registration)
    registry.register(
        "java/lang/ref/PhantomReference",
        "get",
        "()Ljava/lang/Object;",
        native_phantom_ref_get,
    );

    // Round-5 fix (HIGH): SoftReference.get() must touch the GC's
    // soft-ref LRU index so subsequently-cleared soft refs reflect
    // recency-of-use. The base `native_ref_get` registered for all
    // four classes above does not call into the LRU; override the
    // SoftReference slot with a specialized native that performs the
    // LRU touch after reading the referent.
    registry.register(
        "java/lang/ref/SoftReference",
        "get",
        "()Ljava/lang/Object;",
        native_soft_ref_get,
    );

    // Reference base constructors (for subclass dispatch)
    registry.register(
        "java/lang/ref/Reference",
        "<init>",
        "(Ljava/lang/Object;)V",
        native_ref_init,
    );
    registry.register(
        "java/lang/ref/Reference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        native_ref_init_queue,
    );

    // =========================================================================
    // java/lang/ref/ReferenceQueue
    // =========================================================================
    registry.register(
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        native_rq_init,
    );
    registry.register(
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        native_rq_poll,
    );
    registry.register(
        "java/lang/ref/ReferenceQueue",
        "remove",
        "()Ljava/lang/ref/Reference;",
        native_rq_remove_blocking,
    );
    registry.register(
        "java/lang/ref/ReferenceQueue",
        "remove",
        "(J)Ljava/lang/ref/Reference;",
        native_rq_remove_timeout,
    );
    registry.set_category(__prev_cat);
}

fn native_brave_weak_key_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first() else {
        return Ok(Some(Value::Int(0)));
    };
    let Some(Value::Object(Some(other))) = args.get(1) else {
        return Ok(Some(Value::Int(0)));
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }
    let referent = ctx.get_field(*this, REF_FIELD_REFERENT);
    Ok(Some(Value::Int(
        matches!(referent, Value::Object(Some(value)) if value == *other) as i32,
    )))
}

/// `WeakConcurrentMap$WeakKey.equals(Object)` — mirrors Mockito's own
/// bytecode: `other instanceof LatentKey ? ((LatentKey) other).key ==
/// this.get() : ((WeakKey) other).get() == this.get()`. See the registration
/// site's comment for why the real bytecode is unreliable here.
fn native_mockito_weak_key_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first() else {
        return Ok(Some(Value::Int(0)));
    };
    let Some(Value::Object(Some(other))) = args.get(1) else {
        return Ok(Some(Value::Int(0)));
    };
    let this_referent = ctx.get_field_by_name(*this, "referent");
    let is_latent = ctx.class_name_arc_of_id(ctx.class_id_of_object(*other)).as_deref()
        == Some("org/mockito/internal/util/concurrent/WeakConcurrentMap$LatentKey");
    let other_key = if is_latent {
        ctx.get_field_by_name(*other, "key")
    } else {
        ctx.get_field_by_name(*other, "referent")
    };
    Ok(Some(Value::Int(
        (matches!((this_referent, other_key),
            (Value::Object(a), Value::Object(b)) if a == b))
            as i32,
    )))
}

/// `WeakConcurrentMap$LatentKey.equals(Object)` — mirrors Mockito's own
/// bytecode: `other instanceof LatentKey ? other.key == this.key :
/// ((WeakKey) other).get() == this.key`.
fn native_mockito_latent_key_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first() else {
        return Ok(Some(Value::Int(0)));
    };
    let Some(Value::Object(Some(other))) = args.get(1) else {
        return Ok(Some(Value::Int(0)));
    };
    let this_key = ctx.get_field_by_name(*this, "key");
    let other_key = if ctx.class_name_arc_of_id(ctx.class_id_of_object(*other))
        .as_deref()
        == Some("org/mockito/internal/util/concurrent/WeakConcurrentMap$LatentKey")
    {
        ctx.get_field_by_name(*other, "key")
    } else {
        ctx.get_field_by_name(*other, "referent")
    };
    Ok(Some(Value::Int(
        (matches!((this_key, other_key),
            (Value::Object(a), Value::Object(b)) if a == b))
            as i32,
    )))
}

// ---------------------------------------------------------------------------
// Reference constructors
// ---------------------------------------------------------------------------

/// Shared init helper: writes the referent into field 0 and the queue (or
/// null when absent) into field 1.
fn ref_init_impl(ctx: &mut dyn NativeContext, args: &[Value], has_queue: bool) {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return,
    };
    let referent = args.get(1).cloned().unwrap_or(Value::Object(None));
    let class_id = ctx.class_id_of_object(this);
    if ctx
        .resolve_field_index_by_class_id(class_id, "referent")
        .is_some()
    {
        // Real JDK Reference declares queue/next/discovered/referent in a
        // different order than the synthetic two-slot runtime object.
        ctx.set_field_by_name(this, "referent", referent);
        let queue = if has_queue {
            args.get(2).cloned().unwrap_or(Value::Object(None))
        } else {
            Value::Object(None)
        };
        ctx.set_field_by_name(this, "queue", queue);
        return;
    }
    ctx.set_field(this, REF_FIELD_REFERENT, referent);
    if has_queue {
        let queue = args.get(2).cloned().unwrap_or(Value::Object(None));
        ctx.set_field(this, REF_FIELD_QUEUE, queue);
    } else {
        ctx.set_field(this, REF_FIELD_QUEUE, Value::Object(None));
    }
}

fn native_ref_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, false);
    Ok(None)
}

fn native_ref_init_queue(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, true);
    Ok(None)
}

/// Notify the GC's reference processor about a freshly-constructed
/// weak/soft/phantom reference so it will be discovered on the next mark
/// pass. `ref_type`: 0 = Weak, 1 = Soft, 2 = Phantom.
fn discover_ref_from_args(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ref_type: u8,
    has_queue: bool,
) {
    let reference_obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return,
    };
    let referent = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return,
    };
    let queue = if has_queue {
        match args.get(2) {
            Some(Value::Object(Some(q))) => Some(*q),
            _ => None,
        }
    } else {
        None
    };
    ctx.discover_reference(ref_type, reference_obj, referent, queue);
}

fn native_weak_ref_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, false);
    discover_ref_from_args(ctx, args, 0, false);
    Ok(None)
}

fn native_weak_ref_init_queue(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, true);
    discover_ref_from_args(ctx, args, 0, true);
    Ok(None)
}

/// Round-5 fixed `SoftReference.get()` to touch the LRU timestamp on every
/// *read* — but a freshly-constructed SoftReference whose first read is
/// still ahead of it starts with `last_access_time_ms == 0`
/// (`RefProcessor::discover_reference`), which looks INFINITELY idle to
/// `process_soft_refs`'s `current_time_ms - last_access_time_ms > threshold`
/// check. A cache that populates a SoftReference once and only reads it
/// through a wrapper that doesn't itself call `.get()` on the first
/// population (Groovy's `org.codehaus.groovy.util.LazyReference.getLocked`
/// stores `new ManagedReference(bundle, res)` and returns `res` directly,
/// without ever calling the `ManagedReference`'s own `.get()`) can therefore
/// have its cached value cleared on the very first GC after construction —
/// SECONDS before anything ever read it — instead of only under genuine LRU
/// staleness. Root cause of `groovy.lang.MetaClassImpl.addFields` NPEing on
/// `CachedClass.getFields()` returning null the *second* time a given
/// interface's fields are requested (`SpringApplicationNoWebTests`,
/// `GroovySystem.<clinit>` bootstrap). Touch it once at construction so a
/// brand-new SoftReference starts its LRU clock at "now", matching a real
/// JVM's effective behavior (a soft ref's clock is seeded relative to the
/// GC epoch at creation, not zero).
fn native_soft_ref_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, false);
    discover_ref_from_args(ctx, args, 1, false);
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.touch_soft_reference(*this);
    }
    Ok(None)
}

fn native_soft_ref_init_queue(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, true);
    discover_ref_from_args(ctx, args, 1, true);
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.touch_soft_reference(*this);
    }
    Ok(None)
}

/// `jdk.internal.ref.Cleaner` — the real JDK's own pre-`java.lang.ref.Cleaner`
/// reclamation hook, and the one `java.nio.DirectByteBuffer` still uses.
const JDK_INTERNAL_CLEANER: &str = "jdk/internal/ref/Cleaner";

/// Is this reference object a `jdk.internal.ref.Cleaner`?
///
/// Its constructor is `super(referent, dummyQueue)`, so it arrives at
/// [`native_phantom_ref_init`] as an ordinary `PhantomReference`.
fn is_jdk_internal_cleaner(ctx: &mut dyn NativeContext, reference_obj: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(reference_obj);
    ctx.class_name_of_id(class_id)
        .is_some_and(|name| name == JDK_INTERNAL_CLEANER)
}

fn native_phantom_ref_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, true);
    // A `jdk.internal.ref.Cleaner` is discovered as a CLEANER, not as a plain
    // phantom.
    //
    // In the real JDK the difference is made by `ReferenceHandler`, which
    // special-cases `instanceof Cleaner` and calls `clean()` directly instead of
    // enqueueing: a `Cleaner`'s queue is a private `dummyQueue` that nothing
    // ever polls, so a Cleaner routed down the ordinary phantom path is
    // cleared, parked on a queue with no consumer, and never runs its thunk.
    //
    // `java.nio.DirectByteBuffer`'s entire reclamation path is one of these —
    // `Cleaner.create(this, new Deallocator(base, size, cap))` — so without this
    // NOTHING ever returns off-heap memory or refunds `Bits.reserveMemory`.
    // Measured: `probes/DirectReclaimProbe.java` died with
    // `OutOfMemoryError: Direct buffer memory` after exactly 16384 dropped
    // 64 KiB buffers at `-Xmx 1g` — i.e. zero reclaimed — where HotSpot churns
    // 2.5 GiB through the same loop.
    //
    // Wire encoding 4 = "phantom that RUNS instead of enqueueing"; it stays a
    // PHANTOM rather than becoming `ReferenceType::Cleaner` because only phantom
    // entries get their referent nulled before marking, and a `Cleaner` is
    // reachable forever from its class's static list — so without that nulling
    // the referent stays strongly reachable and can never die.
    let ref_type = match args.first() {
        Some(Value::Object(Some(obj))) if is_jdk_internal_cleaner(ctx, *obj) => 4,
        _ => 2,
    };
    discover_ref_from_args(ctx, args, ref_type, true);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Reference methods
// ---------------------------------------------------------------------------

fn native_phantom_ref_get(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // PhantomReference.get() always returns null per JDK spec (JDK 1.2+)
    Ok(Some(Value::Object(None)))
}

fn native_ref_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let referent = ctx.get_field(this, REF_FIELD_REFERENT);
    // INT-8: G1ReferenceGet-equivalent keep-alive. The G1 marker hides
    // referent slots during a concurrent cycle, so a referent handed to the
    // mutator here could otherwise be stored behind an already-scanned
    // object as its only strong path and then be cleared+freed at remark
    // while strongly reachable. Logging it via the SATB pre-barrier keeps
    // it live for the remainder of the cycle; no-op outside a cycle.
    if let Value::Object(Some(r)) = referent {
        ctx.gc_reference_keep_alive(r);
    }
    Ok(Some(referent))
}

/// Round-5 fix (HIGH — broken SoftRef LRU): `SoftReference.get()` must
/// notify the GC's reference processor so the soft-ref LRU timestamp
/// for this reference is refreshed. The base `native_ref_get` reads the
/// referent and returns it but never calls into the LRU, leaving every
/// SoftReference with `last_access_time_ms == 0` — making them all
/// appear infinitely stale and clearing the entire soft-ref population
/// on the first low-memory cycle (defeating soft-ref-backed caches like
/// `WeakHashMap`/`Caffeine` variants and the JDK's own
/// `sun.nio.ch.Util.BufferCache`).
///
/// The `touch_soft_reference` hook on `NativeContext` defaults to a
/// no-op for test mocks; the VM's `NativeContextImpl` overrides it to
/// call `ReferenceProcessor::touch_soft_reference` with the current
/// wall-clock time.
fn native_soft_ref_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let referent = ctx.get_field(this, REF_FIELD_REFERENT);
    // Only refresh the LRU when the referent is still live — touching
    // a cleared soft ref would needlessly churn the index for an
    // entry that's about to be removed.
    if let Value::Object(Some(r)) = referent {
        ctx.touch_soft_reference(this);
        // INT-8 keep-alive — see `native_ref_get`.
        ctx.gc_reference_keep_alive(r);
    }
    Ok(Some(referent))
}

fn native_ref_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, REF_FIELD_REFERENT, Value::Object(None));
    Ok(None)
}

fn native_ref_enqueue(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        // Real JDK Reference uses declared queue/next/discovered/referent
        // fields and ReferenceQueue.enqueue() owns its locking, queueLength,
        // and self-link rules. Do not splice it with the two-slot synthetic
        // model: that can place a bare Object in poll0()'s Reference.next.
        let real_layout = ctx
            .resolve_field_index_by_class_id(ctx.class_id_of_object(this), "referent")
            .is_some();
        if real_layout {
            let queue = ctx.get_field_by_name(this, "queue");
            let Value::Object(Some(queue)) = queue else {
                return Ok(Some(Value::Int(0)));
            };
            // Under the queue's monitor like every other list mutation here:
            // the JDK's `enqueue` takes its own `ReentrantLock`, which excludes
            // this path against ITSELF but not against `native_rq_poll`, whose
            // pop of the same `head` is pure native. See [`with_queue_monitor`].
            let result = with_queue_monitor(ctx, queue, |ctx, queue| {
                let queue_pin = ctx.pin_native_root(queue);
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field_by_name(this, "referent", Value::Object(None));
                let queue = ctx.read_native_pin(queue_pin, queue);
                let this = ctx.read_native_pin(this_pin, this);
                let out = ctx.invoke_special(
                    "java/lang/ref/ReferenceQueue",
                    "enqueue",
                    "(Ljava/lang/ref/Reference;)Z",
                    &[Value::Object(Some(queue)), Value::Object(Some(this))],
                );
                ctx.unpin_native_roots(queue_pin);
                out
            });
            return result;
        }
        let queue = ctx.get_field(this, REF_FIELD_QUEUE);
        match queue {
            Value::Object(Some(q)) => {
                // Same exclusion as the real-layout arm above and as
                // `native_rq_poll` — this arm publishes a new `head` and links
                // the old one through `next`, which is exactly the read-modify-
                // write a concurrent poll must not interleave with.
                let result = with_queue_monitor(ctx, q, |ctx, q| {
                    let queue_pin = ctx.pin_native_root(q);
                    // `ref_next_slot` can resolve/load metadata. Root both
                    // participants and resolve it before loading old_head, so
                    // no unrooted queue-link value crosses that GC-capable call.
                    let this = ctx.read_native_pin(this_pin, this);
                    let next_slot = ref_next_slot(ctx, this);
                    let this = ctx.read_native_pin(this_pin, this);
                    let q = ctx.read_native_pin(queue_pin, q);
                    // JDK 9+ semantics: enqueue() clears the referent before
                    // adding to the queue.
                    ctx.set_field(this, REF_FIELD_REFERENT, Value::Object(None));
                    let old_head = ctx.get_field(q, RQ_FIELD_HEAD);
                    let q = ctx.read_native_pin(queue_pin, q);
                    let this = ctx.read_native_pin(this_pin, this);
                    ctx.set_field(q, RQ_FIELD_HEAD, Value::Object(Some(this)));
                    let this = ctx.read_native_pin(this_pin, this);
                    ctx.set_field(this, next_slot, old_head);
                    let q = ctx.read_native_pin(queue_pin, q);
                    // See `native_rq_poll`: preserve the slot's stored width.
                    let old_size = ctx.get_field(q, RQ_FIELD_SIZE);
                    let q = ctx.read_native_pin(queue_pin, q);
                    let new_size = match old_size {
                        Value::Long(v) => Value::Long(v + 1),
                        Value::Int(v) => Value::Int(v + 1),
                        _ => Value::Int(1),
                    };
                    ctx.set_field(q, RQ_FIELD_SIZE, new_size);
                    let this = ctx.read_native_pin(this_pin, this);
                    // Mark as enqueued — sentinel Int(1) distinguishes from
                    // never having had a queue.
                    ctx.set_field(this, REF_FIELD_QUEUE, Value::Int(1));
                    ctx.unpin_native_roots(queue_pin);
                    Ok(Some(Value::Int(1)))
                });
                result
            }
            _ => Ok(Some(Value::Int(0))), // no queue attached
        }
    })();
    // PGJDBC-PHANTOM-GHOST (2026-08-07): a successful explicit enqueue must
    // retire this reference's entry in the GC's own registry, or the GC's
    // weak/phantom processing can rediscover and re-deliver it a second time
    // once its (possibly still-shared) referent later dies for real. See
    // `ReferenceProcessor::mark_manually_enqueued`'s doc for the full
    // mechanism. Read the pin ONE more time first: `invoke_special` above can
    // run arbitrary bytecode (a GC-capable call), so `this` may have moved.
    if matches!(&result, Ok(Some(Value::Int(1)))) {
        let this = ctx.read_native_pin(this_pin, this);
        ctx.mark_reference_manually_enqueued(this);
    }
    ctx.unpin_native_roots(this_pin);
    result
}

fn native_ref_is_enqueued(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // After enqueue, the queue field is set to sentinel Int(1).
    // Object(None) = never had queue, Object(Some(_)) = has queue but not yet enqueued.
    let queue = ctx.get_field(this, REF_FIELD_QUEUE);
    Ok(Some(Value::Int(match queue {
        Value::Int(1) => 1, // enqueued sentinel
        _ => 0,
    })))
}

fn native_ref_refers_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let referent = ctx.get_field(this, REF_FIELD_REFERENT);
    let other = args.get(1).cloned().unwrap_or(Value::Object(None));
    let same = match (&referent, &other) {
        (Value::Object(a), Value::Object(b)) => a == b,
        _ => false,
    };
    if !same && crate::dbg_refers_to() {
        if let (Value::Object(Some(a)), Value::Object(Some(b))) = (&referent, &other) {
            eprintln!(
                "[refersto] FALSE(shadow) this={:#x} referent={:#x} other={:#x}",
                this.as_ptr() as usize,
                a.as_ptr() as usize,
                b.as_ptr() as usize,
            );
        }
    }
    Ok(Some(Value::Int(if same { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// ReferenceQueue
// ---------------------------------------------------------------------------

fn native_rq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, RQ_FIELD_HEAD, Value::Object(None));
    ctx.set_field(this, RQ_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_rq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `ReferenceQueue.remove(timeout)` can resume after a moving collection;
    // furthermore resolving Reference.next can allocate/load metadata. Root
    // both the queue and the dequeued reference, then reload each before every
    // field access in the linked-list update.
    //
    // The whole pop — read head, read head.next, publish next as the new head —
    // runs under the queue's monitor. See [`with_queue_monitor`]: without it two
    // concurrent pollers hand the same Reference to both callers.
    let this_pin = ctx.pin_native_root(this);
    let result = with_queue_monitor(ctx, this, |ctx, this| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        if ctx.object_num_fields(this) < 2 {
            return Ok(Some(Value::Object(None)));
        }
        let head = ctx.get_field(this, RQ_FIELD_HEAD);
        match head {
            Value::Object(Some(ref_obj)) => {
                let ref_pin = ctx.pin_native_root(ref_obj);
                let result = {
                    // Pop from linked list — linked through the `next` slot
                    // (or the legacy referent-slot fallback; see ref_next_slot).
                    let ref_obj = ctx.read_native_pin(ref_pin, ref_obj);
                    let next_slot = ref_next_slot(ctx, ref_obj);
                    let ref_obj = ctx.read_native_pin(ref_pin, ref_obj);
                    let next = ctx.get_field(ref_obj, next_slot);
                    // PGJDBC-PHANTOM-DOUBLE-POLL (2026-08-07): the real JDK
                    // marks the LAST element of the queue by SELF-LINKING it —
                    // `ReferenceQueue.enqueue0` does
                    // `r.next = (head == null) ? r : head;`, and `poll0` undoes
                    // it with `head = (r.next == r) ? null : r.next;`.
                    // `native_ref_enqueue`'s real-JDK-layout path delegates
                    // straight to that real bytecode, so a Reference enqueued
                    // while the queue was empty legitimately arrives here with
                    // `next == ref_obj`. Taking `next` literally (as this
                    // native override previously did unconditionally)
                    // re-published `ref_obj` ITSELF as the new head right
                    // after popping it: the NEXT `poll()` call finds the
                    // "same" head again and delivers the already-fully-
                    // processed Reference a SECOND time (a ghost redelivery).
                    //
                    // Independently converged on from two witnesses: H2's
                    // embedded `TestPgServer.testDateTime` (a bare
                    // `clear()`+`enqueue()` cycle with no null check on the
                    // duplicate poll), and pgjdbc's real-Postgres
                    // `SimpleQuery.unprepare()`/`setCleanupRef()`, where
                    // `QueryExecutorImpl.processDeadParsedQueries`'s
                    // `parsedQueryMap.remove(polled)` returned null for the
                    // ghost and fed `sendCloseStatement` a null
                    // `statementName`, raising `NullPointerException: ...
                    // because "statementName" is null` from inside the
                    // driver — the dominant failure signature in a real-
                    // Postgres full Hibernate-suite run.
                    //
                    // The GC auto-enqueue path uses the synthetic convention
                    // (`next` = old head, or null when the queue was empty)
                    // and never self-links, so it is unaffected either way.
                    // Normalize the sentinel to "queue now empty" here,
                    // matching real JDK `poll0`.
                    let ref_obj = ctx.read_native_pin(ref_pin, ref_obj);
                    let next = match next {
                        Value::Object(Some(n)) if n == ref_obj => Value::Object(None),
                        other => other,
                    };
                    let this = ctx.read_native_pin(this_pin, this);
                    ctx.set_field(this, RQ_FIELD_HEAD, next);
                    // Detach the popped reference from the list and clear its
                    // enqueued state (JDK: poll sets queue = null, so
                    // isEnqueued() reads false afterwards).
                    let ref_obj = ctx.read_native_pin(ref_pin, ref_obj);
                    ctx.set_field(ref_obj, next_slot, Value::Object(None));
                    let ref_obj = ctx.read_native_pin(ref_pin, ref_obj);
                    ctx.set_field(ref_obj, REF_FIELD_QUEUE, Value::Object(None));
                    let this = ctx.read_native_pin(this_pin, this);
                    // Slot 1 is `size` in the synthetic two-slot shape but
                    // `queueLength` — a `long` — on a real JDK ReferenceQueue,
                    // whose own `enqueue0` bytecode reads it back. Preserve the
                    // stored width so a native poll can never leave an `int`
                    // sitting in a `long` field.
                    let old_size = ctx.get_field(this, RQ_FIELD_SIZE);
                    let this = ctx.read_native_pin(this_pin, this);
                    let new_size = match old_size {
                        Value::Long(v) => Value::Long((v - 1).max(0)),
                        Value::Int(v) => Value::Int((v - 1).max(0)),
                        _ => Value::Int(0),
                    };
                    ctx.set_field(this, RQ_FIELD_SIZE, new_size);
                    let ref_obj = ctx.read_native_pin(ref_pin, ref_obj);
                    Ok(Some(Value::Object(Some(ref_obj))))
                };
                ctx.unpin_native_roots(ref_pin);
                result
            }
            _ => Ok(Some(Value::Object(None))),
        }
    });
    ctx.unpin_native_roots(this_pin);
    result
}

fn native_rq_remove_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Blocking remove: spin-wait with yield until an element is available.
    // Safety cap at 60 seconds to prevent true deadlock.
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(60);
    // Keep the receiver in the native root snapshot for the whole parked
    // lifetime. `largs` is only a local copy: a moving GC can update it at a
    // region boundary only after finding a rooted source reference. Reload the
    // pin before every poll so the synthetic and real-JDK queue paths both see
    // the post-GC queue object.
    let queue_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let mut largs: Vec<Value> = args.to_vec();
        // Exponential-backoff park (200µs → 10ms): the queue is usually empty
        // and long-lived cleaner/reference-handler threads must not yield-spin.
        let mut backoff_us: u64 = 200;
        loop {
            // `end_blocking_region_refs` is the authoritative post-GC
            // rewrite for locals captured before parking. Reading only the
            // pin here can preserve an address from the deposit snapshot
            // across a blocked young collection.
            let this = match largs.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => ctx.read_native_pin(queue_pin, this),
            };
            largs[0] = Value::Object(Some(this));
            let result = native_rq_poll(ctx, &largs)?;
            if let Some(Value::Object(Some(_))) = result {
                return Ok(result);
            }
            if start.elapsed() >= timeout {
                return Ok(Some(Value::Object(None)));
            }
            ctx.begin_blocking_region();
            std::thread::sleep(std::time::Duration::from_micros(backoff_us));
            ctx.end_blocking_region_refs(&mut largs);
            backoff_us = (backoff_us * 2).min(10_000);
        }
    })();
    ctx.unpin_native_roots(queue_pin);
    result
}

fn native_rq_remove_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Blocking remove with timeout in milliseconds.
    let timeout_ms = match args.get(1) {
        Some(Value::Long(v)) => *v as u64,
        Some(Value::Int(v)) => *v as u64,
        _ => 0,
    };
    if timeout_ms == 0 {
        return native_rq_poll(ctx, args);
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(timeout_ms);
    // See `native_rq_remove_blocking`: the pin, rather than `largs`, is the
    // stable root deposited while this native thread is outside the safepoint
    // barrier. The Common Cleaner uses this overload for its entire lifetime.
    let queue_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let mut largs: Vec<Value> = args.to_vec();
        let mut backoff_us: u64 = 200;
        loop {
            // See the blocking overload: use the wake-up rewrite, not the
            // pre-park pin snapshot, as the next poll receiver.
            let this = match largs.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => ctx.read_native_pin(queue_pin, this),
            };
            largs[0] = Value::Object(Some(this));
            let result = native_rq_poll(ctx, &largs)?;
            if let Some(Value::Object(Some(_))) = result {
                return Ok(result);
            }
            if start.elapsed() >= timeout {
                return Ok(Some(Value::Object(None)));
            }
            ctx.begin_blocking_region();
            std::thread::sleep(std::time::Duration::from_micros(backoff_us));
            ctx.end_blocking_region_refs(&mut largs);
            backoff_us = (backoff_us * 2).min(10_000);
        }
    })();
    ctx.unpin_native_roots(queue_pin);
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;

    #[test]
    fn register_reference_natives_registers_all_expected_entries() {
        let mut r = NativeMethodRegistry::new();
        register_reference_natives(&mut r);

        // A handful of representative entries across the four classes.
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
}
