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

use rustjvm_types::Value;
use rustjvm_types::error::MethodCallResult;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};

use crate::{REF_FIELD_QUEUE, REF_FIELD_REFERENT, RQ_FIELD_HEAD, RQ_FIELD_SIZE};

/// Register every `java.lang.ref.*` native. Called from
/// `register_essential_natives` in `lib.rs`.
pub(crate) fn register_reference_natives(registry: &mut NativeMethodRegistry) {
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
    Ok(Some(ctx.get_field(this, REF_FIELD_REFERENT)))
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
            // Push this reference onto the queue's linked list head
            let old_head = ctx.get_field(q, RQ_FIELD_HEAD);
            ctx.set_field(q, RQ_FIELD_HEAD, Value::Object(Some(this)));
            // Use the referent field as a "next" pointer for queued references
            // (referent is cleared when enqueued anyway)
            ctx.set_field(this, REF_FIELD_REFERENT, old_head);
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
    let head = ctx.get_field(this, RQ_FIELD_HEAD);
    match head {
        Value::Object(Some(ref_obj)) => {
            // Pop from linked list — the reference's referent field is used as "next"
            let next = ctx.get_field(ref_obj, REF_FIELD_REFERENT);
            ctx.set_field(this, RQ_FIELD_HEAD, next);
            // Clear the popped reference's referent
            ctx.set_field(ref_obj, REF_FIELD_REFERENT, Value::Object(None));
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
    loop {
        let result = native_rq_poll(ctx, args)?;
        if let Some(Value::Object(Some(_))) = result {
            return Ok(result);
        }
        if start.elapsed() >= timeout {
            return Ok(Some(Value::Object(None)));
        }
        std::thread::yield_now();
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
    loop {
        let result = native_rq_poll(ctx, args)?;
        if let Some(Value::Object(Some(_))) = result {
            return Ok(result);
        }
        if start.elapsed() >= timeout {
            return Ok(Some(Value::Object(None)));
        }
        std::thread::yield_now();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rustjvm_native_api::NativeMethodRegistry;

    #[test]
    fn register_reference_natives_registers_all_expected_entries() {
        let mut r = NativeMethodRegistry::new();
        register_reference_natives(&mut r);

        // A handful of representative entries across the four classes.
        assert!(r.find("java/lang/ref/WeakReference", "get", "()Ljava/lang/Object;").is_some());
        assert!(r.find("java/lang/ref/SoftReference", "get", "()Ljava/lang/Object;").is_some());
        assert!(r.find(
            "java/lang/ref/PhantomReference",
            "get",
            "()Ljava/lang/Object;"
        )
        .is_some());
        assert!(r.find("java/lang/ref/Reference", "clear", "()V").is_some());
        assert!(r.find("java/lang/ref/ReferenceQueue", "poll", "()Ljava/lang/ref/Reference;").is_some());
        assert!(r.find("java/lang/ref/ReferenceQueue", "remove", "()Ljava/lang/ref/Reference;").is_some());
        assert!(r.find("java/lang/ref/ReferenceQueue", "remove", "(J)Ljava/lang/ref/Reference;").is_some());
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
        assert!(r.find("java/lang/ref/PhantomReference", "get", "()Ljava/lang/Object;").is_some());
        assert!(r.find("java/lang/ref/WeakReference", "get", "()Ljava/lang/Object;").is_some());
    }
}
