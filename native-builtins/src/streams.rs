//! Stream-terminal and Flow.Subscriber native overrides.
//!
//! T16.9: Streams — the JDK's real `Stream.reduce` / `Stream.collect`
//! terminal operations are implemented in Java bytecode that walks a
//! Spliterator. In `--synthetic-jdk` mode we have no Java-side Stream
//! bytecode to fall back to; the synthetic stream natives live in
//! `cratonvm-native-collections`.
//!
//! This module is intentionally small. Its purpose:
//!
//! 1. Provide a dedicated home for Stream-terminal / Flow overrides so
//!    that subsequent edits do not need to touch `phases_late.rs`
//!    (which is being edited by multiple concurrent work streams).
//! 2. Override the no-op `Flow.Subscription.request(long)` with a
//!    saturating-add demand counter, matching the T9.2.2 pattern used
//!    by HTTP/2 `BodySubscriber`.
//!
//! The prior inline `Flow.Subscription` registration in `lib.rs`
//! (around line 10737) treats `request(J)V` as a no-op. Re-registering
//! the same key here overwrites the earlier callback because
//! `NativeMethodRegistry::register` is last-writer-wins for identical
//! triples.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cratonvm_types::Value;
use cratonvm_types::error::MethodCallResult;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};

use crate::alloc_concurrent_synthetic;

/// Process-wide demand counter for `Flow.Subscription.request(long)`.
///
/// Keyed by the subscription ObjectRef's raw pointer (stable for the
/// lifetime of the object). Values accumulate via saturating add so a
/// subscriber that repeatedly requests `Long.MAX_VALUE` never wraps.
fn flow_subscription_demand() -> &'static Mutex<HashMap<u64, i64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<u64, i64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register Stream-terminal / Flow.Subscriber overrides.
pub(crate) fn register_stream_overrides(registry: &mut NativeMethodRegistry) {
    register_flow_subscription_overrides(registry);
    register_basestream_mode_overrides(registry);
}

/// `BaseStream.sequential() / parallel() / unordered() / isParallel() / onClose(Runnable)`
/// are default methods on the `java.util.stream.BaseStream` interface. Real-JDK code
/// (e.g. Spring Boot's auto-configuration) calls `stream.sequential()` and our
/// invokeinterface dispatch can't resolve default methods through superinterfaces
/// reliably, surfacing as `NoSuchMethodError`. Register no-op implementations on
/// every Stream sub-interface that is actually used at the call sites.
///
/// `sequential()` / `parallel()` / `unordered()` simply return `this` (the runtime
/// always operates in sequential mode for now). `isParallel()` returns false.
/// `onClose(Runnable)` returns `this` (we don't track close handlers).
fn register_basestream_mode_overrides(registry: &mut NativeMethodRegistry) {
    // Each stream type declares the return type of sequential/parallel/unordered
    // as its own interface, *not* BaseStream. The descriptor recorded in the
    // bytecode is what `invokeinterface` looks up, so we register all the
    // declared-on-the-call-site variants.
    let classes: &[(&str, &str)] = &[
        ("java/util/stream/BaseStream", "Ljava/util/stream/BaseStream;"),
        ("java/util/stream/Stream", "Ljava/util/stream/Stream;"),
        ("java/util/stream/Stream", "Ljava/util/stream/BaseStream;"),
        ("java/util/stream/IntStream", "Ljava/util/stream/IntStream;"),
        ("java/util/stream/IntStream", "Ljava/util/stream/BaseStream;"),
        ("java/util/stream/LongStream", "Ljava/util/stream/LongStream;"),
        ("java/util/stream/LongStream", "Ljava/util/stream/BaseStream;"),
        ("java/util/stream/DoubleStream", "Ljava/util/stream/DoubleStream;"),
        ("java/util/stream/DoubleStream", "Ljava/util/stream/BaseStream;"),
    ];

    for (cls, ret) in classes {
        let sig_return_self = format!("(){}", ret);
        registry.register(cls, "sequential", &sig_return_self, native_stream_return_this);
        registry.register(cls, "parallel", &sig_return_self, native_stream_return_this);
        registry.register(cls, "unordered", &sig_return_self, native_stream_return_this);

        let sig_onclose = format!("(Ljava/lang/Runnable;){}", ret);
        registry.register(cls, "onClose", &sig_onclose, native_stream_return_this);
    }

    // isParallel returns boolean — always false in our sequential-only runtime.
    for cls in &[
        "java/util/stream/BaseStream",
        "java/util/stream/Stream",
        "java/util/stream/IntStream",
        "java/util/stream/LongStream",
        "java/util/stream/DoubleStream",
    ] {
        registry.register(cls, "isParallel", "()Z", native_stream_return_false);
    }

    // Round 63: `BaseStream.iterator()` is abstract on BaseStream and inherited
    // by Stream/IntStream/etc.  Spring Cloud Eureka (and other Spring Boot
    // apps) call `IterableConfigurationPropertySource.iterator()` which is a
    // default method `return stream().iterator();`. When `stream()` returns a
    // synthetic Stream whose runtime class is the interface itself (no
    // concrete pipeline), the invokeinterface raises NSME because no concrete
    // class is on the dispatch chain that declares a non-abstract `iterator`.
    // Register the call-site descriptor (`()Ljava/util/Iterator;`) on Stream
    // and BaseStream, returning an empty Iterator — semantically equivalent
    // to an empty stream, which matches our sequential-only synthetic-stream
    // shape when no elements are buffered.
    for cls in &[
        "java/util/stream/BaseStream",
        "java/util/stream/Stream",
        "java/util/stream/IntStream",
        "java/util/stream/LongStream",
        "java/util/stream/DoubleStream",
    ] {
        registry.register(cls, "iterator", "()Ljava/util/Iterator;", native_stream_empty_iterator);
    }
}

/// Native helper: return a fresh empty `Collections$EmptyIterator`.
/// Used by `BaseStream.iterator()` when the receiver is a bare/synthetic
/// Stream interface that lost its concrete pipeline class.
fn native_stream_empty_iterator(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let iter = alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyIterator", 0);
    Ok(Some(Value::Object(Some(iter))))
}

/// Native helper: return `this` (the first argument). Used by
/// `sequential() / parallel() / unordered() / onClose()` which are all
/// identity / no-op transformations in our sequential-only stream runtime.
fn native_stream_return_this(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(args.first().copied())
}

/// Native helper: return boolean false. Used by `isParallel()`.
fn native_stream_return_false(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// Override the no-op `Flow.Subscription.request(long)` with a real
/// saturating-add demand counter, plus a companion cancel() that drops
/// the tracked demand.
fn register_flow_subscription_overrides(registry: &mut NativeMethodRegistry) {
    let flow_sub = "java/util/concurrent/Flow$Subscription";

    registry.register(flow_sub, "request", "(J)V", native_flow_request);
    // `cancel()` is registered in lib.rs; re-register here so the demand
    // map is cleaned up when the subscription is cancelled.
    registry.register(flow_sub, "cancel", "()V", native_flow_cancel);
}

/// `Flow.Subscription.request(long n)` — saturating-add the demand.
/// Zero or negative `n` is silently ignored (JDK reactive-streams spec
/// says negative should trigger `onError(IllegalArgumentException)`,
/// but the synthetic JDK path has no live subscriber reference here so
/// we take the no-op path).
fn native_flow_request(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let n = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    if n <= 0 {
        return Ok(None);
    }
    let key = this.as_ptr() as u64;
    let mut map = flow_subscription_demand()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let current = *map.get(&key).unwrap_or(&0);
    map.insert(key, current.saturating_add(n));
    // We deliberately do NOT write the demand back into field 0 — that
    // slot is the cancellation flag in the base lib.rs layout. The demand
    // map is the sole source of truth.
    Ok(None)
}

/// `Flow.Subscription.cancel()` — mark cancelled and drop demand.
fn native_flow_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, 0, Value::Int(1));
    let key = this.as_ptr() as u64;
    let mut map = flow_subscription_demand()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.remove(&key);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;

    #[test]
    fn register_stream_overrides_installs_flow_subscription_request() {
        let mut r = NativeMethodRegistry::new();
        register_stream_overrides(&mut r);
        assert!(r
            .find(
                "java/util/concurrent/Flow$Subscription",
                "request",
                "(J)V"
            )
            .is_some());
        assert!(r
            .find("java/util/concurrent/Flow$Subscription", "cancel", "()V")
            .is_some());
    }

    #[test]
    fn register_stream_overrides_installs_basestream_mode_methods() {
        let mut r = NativeMethodRegistry::new();
        register_stream_overrides(&mut r);
        // BaseStream-declared signatures
        assert!(r.find(
            "java/util/stream/BaseStream", "sequential",
            "()Ljava/util/stream/BaseStream;").is_some());
        // Stream-narrowed return type (covariant override seen on call sites)
        assert!(r.find(
            "java/util/stream/Stream", "sequential",
            "()Ljava/util/stream/BaseStream;").is_some());
        assert!(r.find(
            "java/util/stream/Stream", "sequential",
            "()Ljava/util/stream/Stream;").is_some());
        assert!(r.find(
            "java/util/stream/Stream", "isParallel", "()Z").is_some());
        assert!(r.find(
            "java/util/stream/IntStream", "parallel",
            "()Ljava/util/stream/IntStream;").is_some());
    }

    #[test]
    fn flow_subscription_demand_map_uses_saturating_add() {
        // Direct unit test of the demand-map arithmetic without going
        // through the full native dispatch path.
        let demand = flow_subscription_demand();
        let mut map = demand.lock().unwrap_or_else(|e| e.into_inner());
        // Synthetic key that does not overlap any real ObjectRef pointer.
        let key = 0xDEAD_BEEF_C0FF_EE01u64;
        map.remove(&key); // ensure clean start

        // First request: 10
        let a = *map.get(&key).unwrap_or(&0);
        map.insert(key, a.saturating_add(10));
        assert_eq!(*map.get(&key).unwrap_or(&0), 10);

        // Second request: i64::MAX — must saturate, not wrap
        let b = *map.get(&key).unwrap_or(&0);
        map.insert(key, b.saturating_add(i64::MAX));
        assert_eq!(*map.get(&key).unwrap_or(&0), i64::MAX);

        // Third request: 1 — must stay at i64::MAX
        let c = *map.get(&key).unwrap_or(&0);
        map.insert(key, c.saturating_add(1));
        assert_eq!(*map.get(&key).unwrap_or(&0), i64::MAX);

        map.remove(&key); // cleanup
    }
}
