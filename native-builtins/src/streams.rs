// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use crate::try_alloc_concurrent_synthetic;

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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_flow_subscription_overrides(registry);
    register_basestream_mode_overrides(registry);
    register_service_loader_itr_overrides(registry);
    registry.set_category(__prev_cat);
}

/// `java/util/ServiceLoader$Itr.{hasNext,next}` — the array-backed synthetic
/// iterator (field 0 = element array, field 1 = cursor) that
/// [`native_stream_empty_iterator`] above (and the `ServiceLoader.iterator()`
/// natives in `servlet.rs` / `phases_late.rs`) hand back.
///
/// These were previously registered ONLY inside the servlet-support
/// registration, which runs lazily the first time servlet/Jakarta classes are
/// touched. A plain app that never loads a servlet (e.g. the Kafka clients
/// unit suite) therefore got a `NoSuchMethodError
/// java/util/ServiceLoader$Itr.hasNext()Z` the moment any
/// `stream().iterator()` / `ServiceLoader.iterator()` produced one of these
/// synthetic iterators — which fired inside `ConsumerConfig.<clinit>`
/// (serializer/deserializer discovery) and JUnit Jupiter extension discovery,
/// failing the whole `<clinit>` and surfacing downstream as
/// `NoClassDefFoundError: …/ConsumerConfig`. Registering them here in the
/// always-run essential stream path makes the iterator usable everywhere.
fn register_service_loader_itr_overrides(registry: &mut NativeMethodRegistry) {
    let itr = "java/util/ServiceLoader$Itr";
    registry.register(itr, "hasNext", "()Z", native_sl_itr_has_next);
    registry.register(itr, "next", "()Ljava/lang/Object;", native_sl_itr_next);
}

/// `ServiceLoader$Itr.hasNext()` — true while the cursor (field 1) is before the
/// end of the backing array (field 0).
fn native_sl_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match ctx.get_field(this, 1) {
        Value::Int(i) => i,
        _ => return Ok(Some(Value::Int(0))),
    };
    let len = ctx.array_length(arr) as i32;
    Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
}

/// `ServiceLoader$Itr.next()` — return the element at the cursor and advance it.
fn native_sl_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = match ctx.get_field(this, 1) {
        Value::Int(i) => i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(arr) as i32;
    if idx >= len {
        return Ok(Some(Value::Object(None)));
    }
    let elem = ctx.get_array_element(arr, idx as usize);
    // `set_field` may collect. Keep the iterator, its snapshot, and the
    // returned element rooted, then reread their forwarded references.
    let this_pin = ctx.pin_native_root(this);
    let _arr_pin = ctx.pin_native_root(arr);
    let elem_pin = match elem {
        Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
        _ => None,
    };
    let this_cur = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this_cur, 1, Value::Int(idx + 1));
    let elem_cur = match (elem, elem_pin) {
        (Value::Object(Some(object)), Some(pin)) => {
            Value::Object(Some(ctx.read_native_pin(pin, object)))
        }
        _ => elem,
    };
    ctx.unpin_native_roots(this_pin);
    Ok(Some(elem_cur))
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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Each stream type declares the return type of sequential/parallel/unordered
    // as its own interface, *not* BaseStream. The descriptor recorded in the
    // bytecode is what `invokeinterface` looks up, so we register all the
    // declared-on-the-call-site variants.
    let classes: &[(&str, &str)] = &[
        (
            "java/util/stream/BaseStream",
            "Ljava/util/stream/BaseStream;",
        ),
        ("java/util/stream/Stream", "Ljava/util/stream/Stream;"),
        ("java/util/stream/Stream", "Ljava/util/stream/BaseStream;"),
        ("java/util/stream/IntStream", "Ljava/util/stream/IntStream;"),
        (
            "java/util/stream/IntStream",
            "Ljava/util/stream/BaseStream;",
        ),
        (
            "java/util/stream/LongStream",
            "Ljava/util/stream/LongStream;",
        ),
        (
            "java/util/stream/LongStream",
            "Ljava/util/stream/BaseStream;",
        ),
        (
            "java/util/stream/DoubleStream",
            "Ljava/util/stream/DoubleStream;",
        ),
        (
            "java/util/stream/DoubleStream",
            "Ljava/util/stream/BaseStream;",
        ),
    ];

    for (cls, ret) in classes {
        let sig_return_self = format!("(){}", ret);
        registry.register(
            cls,
            "sequential",
            &sig_return_self,
            native_stream_return_this,
        );
        registry.register(cls, "parallel", &sig_return_self, native_stream_return_this);
        registry.register(
            cls,
            "unordered",
            &sig_return_self,
            native_stream_return_this,
        );

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
        registry.register(
            cls,
            "iterator",
            "()Ljava/util/Iterator;",
            native_stream_empty_iterator,
        );
    }
    // `ServiceLoader$Itr.{hasNext,next}` are registered by
    // `register_service_loader_itr_overrides` (called from `register_stream_overrides`),
    // so the synthetic iterator `native_stream_empty_iterator` hands back is usable.
    registry.set_category(__prev_cat);
}

/// Native helper backing `BaseStream.iterator()`.
///
/// Our synthetic stream pipeline (in `cratonvm-native-collections` /
/// `phases_late.rs`) is eager: every stage materialises its elements into a
/// reference array stored at field slot 0 of the synthetic Stream object (the
/// same array `collect()` / `count()` / `toList()` read — see
/// `p64_stream_elements` / `native_p64_stream_to_list`). So when the receiver
/// carries such a backing array we must return a *real* iterator over those
/// elements — returning an empty iterator here silently drops the entire
/// stream contents. That regression manifested as Hibernate's
/// `PersistentClass.getProperties()` (which returns a `JoinedList` whose
/// `iterator()` is `lists.stream().flatMap(List::stream).iterator()`) yielding
/// zero properties even though `collect()` saw them all — every mapped entity
/// lost all of its non-id attributes.
///
/// Only when the receiver has no backing element array (a bare/synthetic
/// `Stream` interface object that lost its concrete pipeline class — the
/// original Round-63 case for Spring's `stream().iterator()` default-method
/// path) do we fall back to a fresh empty `Collections$EmptyIterator`.
///
/// THIS IS NOT THE ONLY IMPLEMENTATION OF THIS DESCRIPTOR, and for the two
/// registrations that matter most it is not the one that runs. This function is
/// registered on all five of `BaseStream`/`Stream`/`IntStream`/`LongStream`/
/// `DoubleStream`, but `native-collections`' `native_stream_iterator`
/// re-registers `BaseStream` and `Stream` and native-collections registers
/// AFTER native-builtins (last-writer-wins, see `vm/src/vm/vm_init.rs`), so
/// those two land there instead. Measured, not inferred: under `--jdk-only`,
/// `((BaseStream<?,?>) IntStream.rangeClosed(1,3)).iterator()` returns a
/// `java.util.Arrays$ArrayItr` — native-collections' landing — and never the
/// `java/util/ServiceLoader$Itr` this function builds. So a boxing fix applied
/// only here (as W7-2 §9.2 first did) leaves the reachable half untouched;
/// `box_primitive_stream_elements` is that function's counterpart and both must
/// stay. See W7-2 §9.5.
fn native_stream_empty_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // When the receiver carries its backing element array at field slot 0
    // (every CratonVM synthetic Stream/IntStream/... — the same array
    // `forEach`/`collect`/`count` read via `stream_elements`), return a real
    // iterator over those elements. Mirrors the proven `ServiceLoader$Itr`
    // (slot 0 = array, slot 1 = cursor) pattern whose hasNext/next natives are
    // registered in `servlet.rs`. Returning an empty iterator here silently
    // drops the whole stream — that regression made Hibernate's
    // `PersistentClass.getProperties()` (a `JoinedList` whose `iterator()` is
    // `lists.stream().flatMap(List::stream).iterator()`) yield zero properties
    // even though `collect()` saw them all, so every mapped entity lost all of
    // its non-id attributes.
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let arr = match ctx.get_field(this, 0) {
            // Backing element array present → iterate it.
            Value::Object(Some(a)) => a,
            // Bare/synthetic Stream interface that lost its concrete pipeline
            // class (the original Round-63 Spring `stream().iterator()` case):
            // no backing array → empty iterator.
            _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
        };
        // W7-2 §3: this same registration serves `IntStream`/`LongStream`/
        // `DoubleStream`, whose backing array is a PRIMITIVE `int[]`/`long[]`/
        // `double[]` (`make_int_stream` allocates one deliberately, because a
        // reference array coerces `Value::Int` to null). `ServiceLoader$Itr.next`
        // is declared `()Ljava/lang/Object;`, so handing those elements straight
        // back puts a bare `Value::Int` where the caller — and the GC — expects
        // a reference: the primitive-in-a-reference-store species of W7-84, not
        // a wrong answer but an untyped word. Box first.
        //
        // The record predicted "answers EMPTY" here; that is STALE — the
        // Hibernate `JoinedList` fix already made this read the backing array.
        // What survived is the boxing half.
        //
        // Scanned rather than copied unconditionally: a reference stream (the
        // overwhelmingly common receiver, and the Hibernate case this function
        // exists for) finds no primitive and keeps its own array with no
        // allocation at all.
        let arr = box_primitive_iterator_source(ctx, arr)?;
        // Allocating the iterator and storing either field can collect. Root
        // the backing array and iterator shell and reread both after every
        // allocation-capable operation.
        let arr_pin = ctx.pin_native_root(arr);
        let itr = try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2)?;
        let itr_pin = ctx.pin_native_root(itr);
        let itr_cur = ctx.read_native_pin(itr_pin, itr);
        let arr_cur = ctx.read_native_pin(arr_pin, arr);
        ctx.set_field(itr_cur, 0, Value::Object(Some(arr_cur)));
        let itr_cur = ctx.read_native_pin(itr_pin, itr);
        ctx.set_field(itr_cur, 1, Value::Int(0));
        let itr_cur = ctx.read_native_pin(itr_pin, itr);
        ctx.unpin_native_roots(arr_pin);
        return Ok(Some(Value::Object(Some(itr_cur))));
    }
    let iter = try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyIterator", 0)?;
    Ok(Some(Value::Object(Some(iter))))
}

/// If `arr` holds any primitive element, return a fresh REFERENCE array of the
/// same elements boxed through `Integer`/`Long`/`Double`.`valueOf`; otherwise
/// return `arr` unchanged.
///
/// Used by [`native_stream_empty_iterator`] to make the
/// `iterator()Ljava/util/Iterator;` bridge safe on the three primitive streams,
/// whose backing store is a primitive array (W7-2 §3). `Iterator.next()` is
/// declared to return a reference; a `Value::Int` there is an untyped word on
/// the operand stack, which is the shape `W7-84-primitive-in-reference-store`
/// is about — and it is silent, because the value only misbehaves at the
/// caller's `checkcast` or `intValue()`, one frame away.
///
/// GC-SAFETY: `Integer.valueOf` and friends run real bytecode and allocate, so
/// `arr` and every wrapper produced so far can move. `arr` is pinned and
/// re-read each iteration, and each wrapper is stored into the output array
/// IMMEDIATELY — nothing is accumulated in a Rust `Vec` across an allocation.
/// The output array is pinned for the same reason.
fn box_primitive_iterator_source(
    ctx: &mut dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
) -> Result<cratonvm_types::ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let len = ctx.array_length(arr);
    let mut needs_boxing = false;
    for i in 0..len {
        if matches!(
            ctx.get_array_element(arr, i),
            Value::Int(_) | Value::Long(_) | Value::Double(_) | Value::Float(_)
        ) {
            needs_boxing = true;
            break;
        }
    }
    if !needs_boxing {
        return Ok(arr);
    }
    let arr_pin = ctx.pin_native_root(arr);
    let out = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len);
    let out_pin = ctx.pin_native_root(out);
    for i in 0..len {
        let src = ctx.read_native_pin(arr_pin, arr);
        let elem = ctx.get_array_element(src, i);
        let boxed = match elem {
            Value::Int(v) => ctx.invoke(
                "java/lang/Integer",
                "valueOf",
                "(I)Ljava/lang/Integer;",
                &[Value::Int(v)],
            ),
            Value::Long(v) => ctx.invoke(
                "java/lang/Long",
                "valueOf",
                "(J)Ljava/lang/Long;",
                &[Value::Long(v)],
            ),
            Value::Double(v) => ctx.invoke(
                "java/lang/Double",
                "valueOf",
                "(D)Ljava/lang/Double;",
                &[Value::Double(v)],
            ),
            Value::Float(v) => ctx.invoke(
                "java/lang/Float",
                "valueOf",
                "(F)Ljava/lang/Float;",
                &[Value::Float(v)],
            ),
            other => Ok(Some(other)),
        };
        let boxed = match boxed {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(arr_pin);
                return Err(e);
            }
        };
        let dst = ctx.read_native_pin(out_pin, out);
        ctx.set_array_element(dst, i, boxed);
    }
    let out = ctx.read_native_pin(out_pin, out);
    ctx.unpin_native_roots(arr_pin);
    Ok(out)
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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let flow_sub = "java/util/concurrent/Flow$Subscription";

    registry.register(flow_sub, "request", "(J)V", native_flow_request);
    // `cancel()` is registered in lib.rs; re-register here so the demand
    // map is cleaned up when the subscription is cancelled.
    registry.register(flow_sub, "cancel", "()V", native_flow_cancel);
    registry.set_category(__prev_cat);
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn register_stream_overrides_installs_flow_subscription_request() {
        let mut r = NativeMethodRegistry::new();
        register_stream_overrides(&mut r);
        assert!(r
            .find("java/util/concurrent/Flow$Subscription", "request", "(J)V")
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
        assert!(r
            .find(
                "java/util/stream/BaseStream",
                "sequential",
                "()Ljava/util/stream/BaseStream;"
            )
            .is_some());
        // Stream-narrowed return type (covariant override seen on call sites)
        assert!(r
            .find(
                "java/util/stream/Stream",
                "sequential",
                "()Ljava/util/stream/BaseStream;"
            )
            .is_some());
        assert!(r
            .find(
                "java/util/stream/Stream",
                "sequential",
                "()Ljava/util/stream/Stream;"
            )
            .is_some());
        assert!(r
            .find("java/util/stream/Stream", "isParallel", "()Z")
            .is_some());
        assert!(r
            .find(
                "java/util/stream/IntStream",
                "parallel",
                "()Ljava/util/stream/IntStream;"
            )
            .is_some());
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
