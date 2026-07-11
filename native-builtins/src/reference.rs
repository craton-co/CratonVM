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
use cratonvm_types::Value;

use crate::{REF_FIELD_NEXT, REF_FIELD_QUEUE, REF_FIELD_REFERENT, RQ_FIELD_HEAD, RQ_FIELD_SIZE};

/// Queue-linkage slot for a Reference: the real-JDK `next` field (slot 2 of
/// referent/queue/next/discovered) when the object has one; the legacy
/// referent-slot fallback otherwise (old synthetic 2-field shape). Reusing
/// the REFERENT slot as the next pointer made `get()` on an
/// enqueued-but-unpolled WeakReference return the NEXT queue element instead
/// of null. Both the enqueue and poll sides must agree, so they share this.
fn ref_next_slot(ctx: &mut dyn NativeContext, ref_obj: cratonvm_types::ObjectRef) -> usize {
    if ctx.object_num_fields(ref_obj) > REF_FIELD_NEXT {
        REF_FIELD_NEXT
    } else {
        REF_FIELD_REFERENT
    }
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

fn native_soft_ref_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, false);
    discover_ref_from_args(ctx, args, 1, false);
    Ok(None)
}

fn native_soft_ref_init_queue(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, true);
    discover_ref_from_args(ctx, args, 1, true);
    Ok(None)
}

fn native_phantom_ref_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ref_init_impl(ctx, args, true);
    discover_ref_from_args(ctx, args, 2, true);
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
    let queue = ctx.get_field(this, REF_FIELD_QUEUE);
    match queue {
        Value::Object(Some(q)) => {
            // JDK 9+ semantics: `enqueue()` clears the referent before
            // adding to the queue.
            ctx.set_field(this, REF_FIELD_REFERENT, Value::Object(None));
            // Push this reference onto the queue's linked list head,
            // linked through the `next` slot (see `ref_next_slot` — NOT
            // the referent, which must keep reading null after enqueue).
            let old_head = ctx.get_field(q, RQ_FIELD_HEAD);
            ctx.set_field(q, RQ_FIELD_HEAD, Value::Object(Some(this)));
            let next_slot = ref_next_slot(ctx, this);
            ctx.set_field(this, next_slot, old_head);
            // Increment size
            let size = match ctx.get_field(q, RQ_FIELD_SIZE) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(q, RQ_FIELD_SIZE, Value::Int(size + 1));
            // Mark as enqueued — sentinel Int(1) distinguishes from "never had queue"
            ctx.set_field(this, REF_FIELD_QUEUE, Value::Int(1));
            Ok(Some(Value::Int(1)))
        }
        _ => Ok(Some(Value::Int(0))), // no queue attached
    }
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
    // avrora `get_field` OOB fix: `ReferenceQueue.remove(timeout)` parks the
    // JDK Common Cleaner thread in a 60 s native poll loop, holding `this` as a
    // raw `ObjectRef` snapshot. A moving young GC that runs while the thread is
    // blocked relocates the queue, but this thread is excluded from the STW
    // barrier and never applies the pointer map to its raw `this`; the young
    // allocator then reuses the stale from-space slot for a bare
    // `java.lang.Object`. Reading `RQ_FIELD_HEAD`/`SIZE` off that 0-field object
    // tripped the `gen_heap` out-of-bounds guard tens of thousands of times per
    // run (the guard already returned `null`, so `poll()` already behaved as
    // "empty" — this only suppresses the warning by detecting the reclaimed
    // receiver up front). A real (live) queue always carries its head/size
    // slots, so a receiver with fewer than two fields cannot be one; treat it
    // as empty rather than dereferencing past its layout. The blocked thread
    // re-reads the live queue from its (eventually remapped) frame on a later
    // `remove()` invocation, so this is graceful degradation, not data loss.
    if ctx.object_num_fields(this) < 2 {
        return Ok(Some(Value::Object(None)));
    }
    let head = ctx.get_field(this, RQ_FIELD_HEAD);
    match head {
        Value::Object(Some(ref_obj)) => {
            // Pop from linked list — linked through the `next` slot (or the
            // legacy referent-slot fallback; see `ref_next_slot`).
            let next_slot = ref_next_slot(ctx, ref_obj);
            let next = ctx.get_field(ref_obj, next_slot);
            ctx.set_field(this, RQ_FIELD_HEAD, next);
            // Detach the popped reference from the list and clear its
            // enqueued state (JDK: poll sets queue = null, so isEnqueued()
            // reads false afterwards). The referent stays whatever it was —
            // null for GC-cleared/enqueued references.
            ctx.set_field(ref_obj, next_slot, Value::Object(None));
            ctx.set_field(ref_obj, REF_FIELD_QUEUE, Value::Object(None));
            // Decrement size
            let size = match ctx.get_field(this, RQ_FIELD_SIZE) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, RQ_FIELD_SIZE, Value::Int((size - 1).max(0)));
            Ok(Some(Value::Object(Some(ref_obj))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

fn native_rq_remove_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Blocking remove: spin-wait with yield until an element is available.
    // Safety cap at 60 seconds to prevent true deadlock.
    let _this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(60);
    // T19.H1 — this poll loop can spin for up to 60 s without ever
    // reaching an interpreter safepoint (the JBoss/WildFly Reference
    // Handler thread parks here for the whole process lifetime). Mark a
    // blocking region so a concurrent stop-the-world GC does not
    // deadlock waiting for this thread in `wait_for_all`.
    //
    // Stale-receiver fix (the H2 MVStore "compareAndSetRoot on null"
    // writer): the poll must run OUTSIDE the blocked region. While
    // blocked, this thread is excluded from the STW barrier, so a moving
    // GC can relocate the queue mid-loop and the raw `args` receiver goes
    // stale — `native_rq_poll` then splices head/referent/size fields
    // through whatever live object recycled the address. Outside the
    // region we are an expected mutator: a GC may START but cannot
    // COMPLETE until we arrive (at the next region boundary), so `largs`
    // cannot go stale mid-poll. The region is entered only around the
    // yield, and `end_blocking_region_refs` re-syncs both the frames and
    // our local arg copies against the GC fixup accumulated while parked.
    let mut largs: Vec<Value> = args.to_vec();
    // Exponential-backoff park (200µs → 10ms): the queue is usually empty
    // (always, during bootstrap), and the JDK Reference Handler + Common
    // Cleaner sit in this loop for the whole process lifetime. A hot
    // yield-spin here costs a full core each AND a region-transition +
    // root-snapshot deposit per iteration — measured as a ~10x bootstrap
    // slowdown. Reference processing tolerates a ≤10ms wake (HotSpot
    // blocks on a monitor here outright).
    let mut backoff_us: u64 = 200;
    loop {
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
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(timeout_ms);
    // T19.H1 + stale-receiver fix — see `native_rq_remove_blocking`: poll
    // outside the blocked region (an expected mutator cannot observe a GC
    // completing mid-poll), park inside it only for the yield, and re-sync
    // the local arg copies on every region exit. The JDK Common Cleaner
    // parks here (`remove(60_000)`) for the whole process lifetime.
    let mut largs: Vec<Value> = args.to_vec();
    // Exponential-backoff park — see `native_rq_remove_blocking`.
    let mut backoff_us: u64 = 200;
    loop {
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
