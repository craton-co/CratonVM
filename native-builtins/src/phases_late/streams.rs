// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.stream` / `java.util.function` natives: Stream, Collectors, Spliterator, Gatherer, summary statistics.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// ============================================================================
// Phase 56: Stream enhancements, Collectors expansion, java.util.function,
//           Summary Statistics, Stream factories
// ============================================================================

/// REACHABILITY (traced wave 4): the only non-test call site is
/// `native-builtins/src/lib.rs`, inside `register_synthetic_overrides`, which
/// is `#[cfg(feature = "synthetic-jdk")]`. Everything registered from here is
/// therefore SYNTHETIC-ONLY and does nothing in the default real-JDK build.
/// Several entries below are correct only under that condition — see the
/// static-interface-method note on `Gatherer.defaultInitializer`.
pub(crate) fn register_phase56_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_phase56_stream_extras(registry);
    register_phase56_collectors_extras(registry);
    register_phase56_summary_stats(registry);
    register_phase56_function_extras(registry);
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Stream extra operations: peek, iterate, generate, takeWhile, dropWhile,
// concat, ofNullable, flatMapToInt/Long/Double, boxed, parallel/sequential
// ---------------------------------------------------------------------------
pub(crate) fn register_phase56_stream_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let stream = "java/util/stream/Stream";

    // --- Stream.peek(Consumer) → Stream ---
    // Applies the consumer to each element, returns new stream with same elements
    r.register(
        stream,
        "peek",
        "(Ljava/util/function/Consumer;)Ljava/util/stream/Stream;",
        p56_stream_peek,
    );

    // --- Stream.takeWhile(Predicate) → Stream (Java 9+) ---
    r.register(
        stream,
        "takeWhile",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        p56_stream_take_while,
    );

    // --- Stream.dropWhile(Predicate) → Stream (Java 9+) ---
    r.register(
        stream,
        "dropWhile",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        p56_stream_drop_while,
    );

    // --- Stream.parallel() → Stream (returns self, no actual parallelism) ---
    r.register(
        stream,
        "parallel",
        "()Ljava/util/stream/Stream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    // BaseStream return type variant (used by JDK bytecode)
    r.register(
        stream,
        "parallel",
        "()Ljava/util/stream/BaseStream;",
        |_ctx, args| Ok(Some(args[0])),
    );

    // --- Stream.sequential() → Stream ---
    r.register(
        stream,
        "sequential",
        "()Ljava/util/stream/Stream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        stream,
        "sequential",
        "()Ljava/util/stream/BaseStream;",
        |_ctx, args| Ok(Some(args[0])),
    );

    // --- Stream.isParallel() → boolean ---
    // KEEP: constant false is the truth for this stream model, not a stub.
    // `parallel()` above returns the receiver unchanged and no synthetic
    // stream operation ever splits, so every synthetic stream really is
    // sequential. Reporting true would be the lie.
    //
    // Wave 4 re-derived this rather than inheriting it, because the obvious
    // "implement it" — have `parallel()` record a requested-mode bit that
    // `isParallel()` reads back — does not survive contact with the model: the
    // synthetic stream carries its elements in field 0 and every intermediate
    // op (`map`/`filter`/`peek`/...) builds a BRAND NEW synthetic stream via
    // `p56_build_stream`, so a mode bit set on the receiver of `.parallel()`
    // would be dropped by the next stage and `isParallel()` would then answer
    // true or false depending on pipeline position. Constant false is the only
    // answer that is true of every synthetic stream at every stage.
    // (`native-builtins/src/streams.rs:195` registers the same triple with the
    // same constant; this one wins on ordering. No behavioural conflict.)
    r.register(stream, "isParallel", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    // --- Stream.iterator() → Iterator (inherited from BaseStream) ---
    // Spring Boot's `IterableConfigurationPropertySource.iterator()` default
    // method calls `this.stream().iterator()`. Our synthetic Stream
    // (field 0 = Object[]) has no iterator native, so the invokeinterface
    // dispatches against bare `java/util/stream/Stream` (the receiver's
    // class_id_of) and NSME's because `BaseStream.iterator()` is abstract.
    // Reuse the ServiceLoader$Itr layout (field 0 = array, field 1 = idx)
    // whose hasNext/next natives are already registered in `servlet.rs`.
    r.register(stream, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr_val = ctx.get_field(this, 0);
        let arr = if let Value::Object(Some(a)) = arr_val {
            a
        } else {
            ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0)
        };
        let itr = alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(itr))))
    });
    // BaseStream.iterator() variant (some bytecode resolves against BaseStream)
    r.register(
        "java/util/stream/BaseStream",
        "iterator",
        "()Ljava/util/Iterator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr_val = ctx.get_field(this, 0);
            let arr = if let Value::Object(Some(a)) = arr_val {
                a
            } else {
                ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0)
            };
            let itr = alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2);
            ctx.set_field(itr, 0, Value::Object(Some(arr)));
            ctx.set_field(itr, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(itr))))
        },
    );

    // --- Stream.spliterator() → Spliterator (inherited from BaseStream) ---
    // Some Spring code paths call spliterator() directly. We don't have a
    // full Spliterator implementation, but returning an iterator-like
    // backing object lets downstream `forEachRemaining` paths drive elements.
    // Skipped for now: only register iterator() which is the demonstrated
    // call site.

    // --- Stream.unordered() → Stream ---
    r.register(
        stream,
        "unordered",
        "()Ljava/util/stream/Stream;",
        |_ctx, args| Ok(Some(args[0])),
    );

    // --- Stream.forEachOrdered(Consumer) --- same as forEach for us
    r.register(
        stream,
        "forEachOrdered",
        "(Ljava/util/function/Consumer;)V",
        p56_stream_for_each_ordered,
    );

    // --- Stream.concat(Stream, Stream) → Stream ---
    r.register(
        stream,
        "concat",
        "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;",
        p56_stream_concat,
    );

    // --- Stream.ofNullable(T) → Stream (Java 9+) ---
    r.register(
        stream,
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        p56_stream_of_nullable,
    );

    // --- Stream.iterate(seed, UnaryOperator) → Stream (simplified: generates N elements) ---
    r.register(
        stream,
        "iterate",
        "(Ljava/lang/Object;Ljava/util/function/UnaryOperator;)Ljava/util/stream/Stream;",
        p56_stream_iterate,
    );

    // --- Stream.iterate(seed, Predicate, UnaryOperator) → Stream (Java 9+ with hasNext) ---
    r.register(stream, "iterate", "(Ljava/lang/Object;Ljava/util/function/Predicate;Ljava/util/function/UnaryOperator;)Ljava/util/stream/Stream;", p56_stream_iterate_predicate);

    // --- Stream.generate(Supplier) → Stream (simplified: generates N elements) ---
    r.register(
        stream,
        "generate",
        "(Ljava/util/function/Supplier;)Ljava/util/stream/Stream;",
        p56_stream_generate,
    );

    // --- Stream.flatMapToInt(Function) → IntStream ---
    r.register(
        stream,
        "flatMapToInt",
        "(Ljava/util/function/Function;)Ljava/util/stream/IntStream;",
        p56_stream_flat_map_to_int,
    );

    // --- Stream.flatMapToLong(Function) → LongStream ---
    r.register(
        stream,
        "flatMapToLong",
        "(Ljava/util/function/Function;)Ljava/util/stream/LongStream;",
        p56_stream_flat_map_to_long,
    );

    // --- Stream.flatMapToDouble(Function) → DoubleStream ---
    r.register(
        stream,
        "flatMapToDouble",
        "(Ljava/util/function/Function;)Ljava/util/stream/DoubleStream;",
        p56_stream_flat_map_to_double,
    );

    // --- Stream.mapToInt(ToIntFunction) → IntStream ---
    r.register(
        stream,
        "mapToInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/IntStream;",
        p56_stream_map_to_int,
    );

    // --- Stream.mapToLong(ToLongFunction) → LongStream ---
    r.register(
        stream,
        "mapToLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/LongStream;",
        p56_stream_map_to_long,
    );

    // --- Stream.mapToDouble(ToDoubleFunction) → DoubleStream ---
    r.register(
        stream,
        "mapToDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/DoubleStream;",
        p56_stream_map_to_double,
    );

    // --- IntStream extras ---
    let is = "java/util/stream/IntStream";
    r.register(
        is,
        "peek",
        "(Ljava/util/function/IntConsumer;)Ljava/util/stream/IntStream;",
        p56_int_stream_peek,
    );
    r.register(
        is,
        "takeWhile",
        "(Ljava/util/function/IntPredicate;)Ljava/util/stream/IntStream;",
        p56_int_stream_take_while,
    );
    r.register(
        is,
        "dropWhile",
        "(Ljava/util/function/IntPredicate;)Ljava/util/stream/IntStream;",
        p56_int_stream_drop_while,
    );
    r.register(
        is,
        "sorted",
        "()Ljava/util/stream/IntStream;",
        p56_int_stream_sorted,
    );
    r.register(
        is,
        "boxed",
        "()Ljava/util/stream/Stream;",
        p56_int_stream_boxed,
    );
    r.register(
        is,
        "asLongStream",
        "()Ljava/util/stream/LongStream;",
        p56_int_stream_as_long,
    );
    r.register(
        is,
        "asDoubleStream",
        "()Ljava/util/stream/DoubleStream;",
        p56_int_stream_as_double,
    );
    r.register(
        is,
        "parallel",
        "()Ljava/util/stream/IntStream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        is,
        "sequential",
        "()Ljava/util/stream/IntStream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        is,
        "concat",
        "(Ljava/util/stream/IntStream;Ljava/util/stream/IntStream;)Ljava/util/stream/IntStream;",
        p56_int_stream_concat,
    );
    r.register(
        is,
        "forEachOrdered",
        "(Ljava/util/function/IntConsumer;)V",
        p56_int_stream_for_each_ordered,
    );
    r.register(
        is,
        "summaryStatistics",
        "()Ljava/util/IntSummaryStatistics;",
        p56_int_stream_summary_stats,
    );

    // --- LongStream extras ---
    let ls = "java/util/stream/LongStream";
    r.register(
        ls,
        "peek",
        "(Ljava/util/function/LongConsumer;)Ljava/util/stream/LongStream;",
        p56_long_stream_peek,
    );
    r.register(
        ls,
        "takeWhile",
        "(Ljava/util/function/LongPredicate;)Ljava/util/stream/LongStream;",
        p56_long_stream_take_while,
    );
    r.register(
        ls,
        "dropWhile",
        "(Ljava/util/function/LongPredicate;)Ljava/util/stream/LongStream;",
        p56_long_stream_drop_while,
    );
    r.register(
        ls,
        "boxed",
        "()Ljava/util/stream/Stream;",
        p56_long_stream_boxed,
    );
    r.register(
        ls,
        "mapToObj",
        "(Ljava/util/function/LongFunction;)Ljava/util/stream/Stream;",
        p56_long_stream_map_to_obj,
    );
    r.register(
        ls,
        "asDoubleStream",
        "()Ljava/util/stream/DoubleStream;",
        p56_long_stream_as_double,
    );
    r.register(
        ls,
        "parallel",
        "()Ljava/util/stream/LongStream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        ls,
        "sequential",
        "()Ljava/util/stream/LongStream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        ls,
        "concat",
        "(Ljava/util/stream/LongStream;Ljava/util/stream/LongStream;)Ljava/util/stream/LongStream;",
        p56_long_stream_concat,
    );
    r.register(
        ls,
        "summaryStatistics",
        "()Ljava/util/LongSummaryStatistics;",
        p56_long_stream_summary_stats,
    );

    // --- DoubleStream extras ---
    let ds = "java/util/stream/DoubleStream";
    r.register(
        ds,
        "peek",
        "(Ljava/util/function/DoubleConsumer;)Ljava/util/stream/DoubleStream;",
        p56_double_stream_peek,
    );
    r.register(
        ds,
        "takeWhile",
        "(Ljava/util/function/DoublePredicate;)Ljava/util/stream/DoubleStream;",
        p56_double_stream_take_while,
    );
    r.register(
        ds,
        "dropWhile",
        "(Ljava/util/function/DoublePredicate;)Ljava/util/stream/DoubleStream;",
        p56_double_stream_drop_while,
    );
    r.register(
        ds,
        "boxed",
        "()Ljava/util/stream/Stream;",
        p56_double_stream_boxed,
    );
    r.register(
        ds,
        "mapToObj",
        "(Ljava/util/function/DoubleFunction;)Ljava/util/stream/Stream;",
        p56_double_stream_map_to_obj,
    );
    r.register(
        ds,
        "parallel",
        "()Ljava/util/stream/DoubleStream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        ds,
        "sequential",
        "()Ljava/util/stream/DoubleStream;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(ds, "concat", "(Ljava/util/stream/DoubleStream;Ljava/util/stream/DoubleStream;)Ljava/util/stream/DoubleStream;", p56_double_stream_concat);
    r.register(
        ds,
        "summaryStatistics",
        "()Ljava/util/DoubleSummaryStatistics;",
        p56_double_stream_summary_stats,
    );
    r.set_category(__prev_cat);
}

// --- Helper: read stream elements from 1-field synthetic (field 0 = Object[]) ---
pub(crate) fn p56_read_stream_elems(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let arr_val = ctx.get_field(this, 0);
    if let Value::Object(Some(arr)) = arr_val {
        let len = ctx.array_length(arr);
        (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
    } else {
        Vec::new()
    }
}

// --- Helper: build a new Stream from a Vec<Value> ---
pub(crate) fn p56_build_stream(
    ctx: &mut dyn NativeContext,
    elems: Vec<Value>,
    class: &str,
) -> ObjectRef {
    use cratonvm_types::ArrayElementType;
    let len = elems.len();
    // Pin across the array/stream allocs below — a moving young GC there
    // would relocate the object elements and the fresh array (native
    // stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(ArrayElementType::Reference, len);
    let arr_pin = ctx.pin_native_root(arr);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let v = read_pinned_object_value(ctx, *p, *v);
        ctx.set_array_element(arr, i, v);
    }
    let stream = alloc_concurrent_synthetic(ctx, class, 1);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.unpin_native_roots(first_pin.unwrap_or(arr_pin));
    stream
}

// --- Stream.peek ---
pub(crate) fn p56_stream_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the consumer callbacks below — a moving young GC there would
    // relocate `consumer` and the object elements (native stale-local family).
    let consumer_pin = ctx.pin_native_root(consumer);
    let pins = pin_object_values(ctx, &elems);
    // Apply consumer to each element for side effect
    for (v, p) in elems.iter().zip(&pins) {
        let c = ctx.read_native_pin(consumer_pin, consumer);
        let v = read_pinned_object_value(ctx, *p, *v);
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(Ljava/lang/Object;)V", &[v]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    let elems = read_pinned_object_values(ctx, &pins, &elems);
    ctx.unpin_native_roots(consumer_pin);
    // Return new stream with same elements
    let result = p56_build_stream(ctx, elems, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(result))))
}

// --- Stream.takeWhile ---
pub(crate) fn p56_stream_take_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` and the object elements (native stale-local family).
    let predicate_pin = ctx.pin_native_root(predicate);
    let pins = pin_object_values(ctx, &elems);
    let mut taken = 0usize;
    for (v, p) in elems.iter().zip(&pins) {
        let pred = ctx.read_native_pin(predicate_pin, predicate);
        let v = read_pinned_object_value(ctx, *p, *v);
        let test = match ctx.invoke_virtual(pred, "test", "(Ljava/lang/Object;)Z", &[v]) {
            Ok(t) => t,
            Err(e) => {
                ctx.unpin_native_roots(predicate_pin);
                return Err(e);
            }
        };
        if test.unwrap_or(Value::Int(0)) == Value::Int(0) {
            break;
        }
        taken += 1;
    }
    let result = read_pinned_object_values(ctx, &pins[..taken], &elems[..taken]);
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.dropWhile ---
pub(crate) fn p56_stream_drop_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` and the object elements (native stale-local family).
    let predicate_pin = ctx.pin_native_root(predicate);
    let pins = pin_object_values(ctx, &elems);
    let mut start = elems.len();
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let pred = ctx.read_native_pin(predicate_pin, predicate);
        let v = read_pinned_object_value(ctx, *p, *v);
        let test = match ctx.invoke_virtual(pred, "test", "(Ljava/lang/Object;)Z", &[v]) {
            Ok(t) => t,
            Err(e) => {
                ctx.unpin_native_roots(predicate_pin);
                return Err(e);
            }
        };
        if test.unwrap_or(Value::Int(0)) == Value::Int(0) {
            start = i;
            break;
        }
    }
    let result = read_pinned_object_values(ctx, &pins[start..], &elems[start..]);
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.forEachOrdered ---
pub(crate) fn p56_stream_for_each_ordered(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the consumer callbacks below — a moving young GC there would
    // relocate `consumer` and the object elements (native stale-local family).
    let consumer_pin = ctx.pin_native_root(consumer);
    let pins = pin_object_values(ctx, &elems);
    for (v, p) in elems.iter().zip(&pins) {
        let c = ctx.read_native_pin(consumer_pin, consumer);
        let v = read_pinned_object_value(ctx, *p, *v);
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(Ljava/lang/Object;)V", &[v]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(consumer_pin);
    Ok(None)
}

// --- Stream.concat ---
pub(crate) fn p56_stream_concat(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let mut elems = p56_read_stream_elems(ctx, a);
    elems.extend(p56_read_stream_elems(ctx, b));
    let s = p56_build_stream(ctx, elems, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.ofNullable ---
pub(crate) fn p56_stream_of_nullable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let elems = if args[0] == Value::Object(None) {
        Vec::new()
    } else {
        vec![args[0]]
    };
    let s = p56_build_stream(ctx, elems, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.iterate(seed, op) — generate 256 elements (lazy in real JVM, eager here) ---
pub(crate) fn p56_stream_iterate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let seed = args[0];
    let op = obj_arg(args, 1)?;
    // Pin across the operator callbacks below — a moving young GC there would
    // relocate `op` and the loop-carried elements (native stale-local family);
    // each freshly produced element is pinned as it materialises.
    let op_pin = ctx.pin_native_root(op);
    let mut elems = Vec::with_capacity(256);
    let mut pins = Vec::with_capacity(256);
    let mut current = seed;
    let mut current_pin = pinned_object_value(ctx, current);
    for _ in 0..256 {
        elems.push(current);
        pins.push(current_pin);
        let op_cur = ctx.read_native_pin(op_pin, op);
        let arg = read_pinned_object_value(ctx, current_pin, current);
        let next = match ctx.invoke_virtual(
            op_cur,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[arg],
        ) {
            Ok(v) => v,
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        };
        current = next.unwrap_or(Value::Object(None));
        current_pin = pinned_object_value(ctx, current);
    }
    let elems = read_pinned_object_values(ctx, &pins, &elems);
    ctx.unpin_native_roots(op_pin);
    let s = p56_build_stream(ctx, elems, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.iterate(seed, hasNext, op) --- Java 9+
pub(crate) fn p56_stream_iterate_predicate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let seed = args[0];
    let has_next = obj_arg(args, 1)?;
    let op = obj_arg(args, 2)?;
    // Pin across the predicate/operator callbacks below — a moving young GC
    // there would relocate them and the loop-carried elements (native
    // stale-local family); each freshly produced element is pinned as it
    // materialises.
    let has_next_pin = ctx.pin_native_root(has_next);
    let op_pin = ctx.pin_native_root(op);
    let mut elems = Vec::new();
    let mut pins = Vec::new();
    let mut current = seed;
    let mut current_pin = pinned_object_value(ctx, current);
    for _ in 0..10000 {
        let hn = ctx.read_native_pin(has_next_pin, has_next);
        let arg = read_pinned_object_value(ctx, current_pin, current);
        let test = match ctx.invoke_virtual(hn, "test", "(Ljava/lang/Object;)Z", &[arg]) {
            Ok(t) => t,
            Err(e) => {
                ctx.unpin_native_roots(has_next_pin);
                return Err(e);
            }
        };
        if test.unwrap_or(Value::Int(0)) == Value::Int(0) {
            break;
        }
        elems.push(current);
        pins.push(current_pin);
        let op_cur = ctx.read_native_pin(op_pin, op);
        let arg = read_pinned_object_value(ctx, current_pin, current);
        let next = match ctx.invoke_virtual(
            op_cur,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[arg],
        ) {
            Ok(v) => v,
            Err(e) => {
                ctx.unpin_native_roots(has_next_pin);
                return Err(e);
            }
        };
        current = next.unwrap_or(Value::Object(None));
        current_pin = pinned_object_value(ctx, current);
    }
    let elems = read_pinned_object_values(ctx, &pins, &elems);
    ctx.unpin_native_roots(has_next_pin);
    let s = p56_build_stream(ctx, elems, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.generate(Supplier) — generate 256 elements ---
pub(crate) fn p56_stream_generate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let supplier = obj_arg(args, 0)?;
    // Pin across the supplier callbacks below — a moving young GC there would
    // relocate `supplier` and the already-produced elements (native
    // stale-local family); each fresh element is pinned as it materialises.
    let supplier_pin = ctx.pin_native_root(supplier);
    let mut elems = Vec::with_capacity(256);
    let mut pins = Vec::with_capacity(256);
    for _ in 0..256 {
        let s = ctx.read_native_pin(supplier_pin, supplier);
        let v = match ctx.invoke_virtual(s, "get", "()Ljava/lang/Object;", &[]) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(supplier_pin);
                return Err(e);
            }
        };
        pins.push(pinned_object_value(ctx, v));
        elems.push(v);
    }
    let elems = read_pinned_object_values(ctx, &pins, &elems);
    ctx.unpin_native_roots(supplier_pin);
    let s = p56_build_stream(ctx, elems, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.flatMapToInt ---
pub(crate) fn p56_stream_flat_map_to_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the object elements (native stale-local family).
    let func_pin = ctx.pin_native_root(func);
    let pins = pin_object_values(ctx, &elems);
    let mut ints = Vec::new();
    for (v, p) in elems.iter().zip(&pins) {
        let f = ctx.read_native_pin(func_pin, func);
        let v = read_pinned_object_value(ctx, *p, *v);
        let int_stream_val =
            match ctx.invoke_virtual(f, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v]) {
                Ok(r) => r,
                Err(e) => {
                    ctx.unpin_native_roots(func_pin);
                    return Err(e);
                }
            };
        if let Some(Value::Object(Some(is))) = int_stream_val {
            let inner = p56_read_stream_elems(ctx, is);
            ints.extend(inner);
        }
    }
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, ints, "java/util/stream/IntStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.flatMapToLong ---
pub(crate) fn p56_stream_flat_map_to_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the object elements (native stale-local family).
    let func_pin = ctx.pin_native_root(func);
    let pins = pin_object_values(ctx, &elems);
    let mut longs = Vec::new();
    for (v, p) in elems.iter().zip(&pins) {
        let f = ctx.read_native_pin(func_pin, func);
        let v = read_pinned_object_value(ctx, *p, *v);
        let long_stream_val =
            match ctx.invoke_virtual(f, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v]) {
                Ok(r) => r,
                Err(e) => {
                    ctx.unpin_native_roots(func_pin);
                    return Err(e);
                }
            };
        if let Some(Value::Object(Some(ls))) = long_stream_val {
            let inner = p56_read_stream_elems(ctx, ls);
            longs.extend(inner);
        }
    }
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, longs, "java/util/stream/LongStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.flatMapToDouble ---
pub(crate) fn p56_stream_flat_map_to_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the object elements (native stale-local family).
    let func_pin = ctx.pin_native_root(func);
    let pins = pin_object_values(ctx, &elems);
    let mut doubles = Vec::new();
    for (v, p) in elems.iter().zip(&pins) {
        let f = ctx.read_native_pin(func_pin, func);
        let v = read_pinned_object_value(ctx, *p, *v);
        let dbl_stream_val =
            match ctx.invoke_virtual(f, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v]) {
                Ok(r) => r,
                Err(e) => {
                    ctx.unpin_native_roots(func_pin);
                    return Err(e);
                }
            };
        if let Some(Value::Object(Some(ds))) = dbl_stream_val {
            let inner = p56_read_stream_elems(ctx, ds);
            doubles.extend(inner);
        }
    }
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, doubles, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.mapToInt ---
pub(crate) fn p56_stream_map_to_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the object elements (native stale-local family).
    let func_pin = ctx.pin_native_root(func);
    let pins = pin_object_values(ctx, &elems);
    let mut ints = Vec::new();
    for (v, p) in elems.iter().zip(&pins) {
        let f = ctx.read_native_pin(func_pin, func);
        let v = read_pinned_object_value(ctx, *p, *v);
        let r = match ctx.invoke_virtual(f, "applyAsInt", "(Ljava/lang/Object;)I", &[v]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(func_pin);
                return Err(e);
            }
        };
        ints.push(r.unwrap_or(Value::Int(0)));
    }
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, ints, "java/util/stream/IntStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.mapToLong ---
pub(crate) fn p56_stream_map_to_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the object elements (native stale-local family).
    let func_pin = ctx.pin_native_root(func);
    let pins = pin_object_values(ctx, &elems);
    let mut longs = Vec::new();
    for (v, p) in elems.iter().zip(&pins) {
        let f = ctx.read_native_pin(func_pin, func);
        let v = read_pinned_object_value(ctx, *p, *v);
        let r = match ctx.invoke_virtual(f, "applyAsLong", "(Ljava/lang/Object;)J", &[v]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(func_pin);
                return Err(e);
            }
        };
        longs.push(r.unwrap_or(Value::Long(0)));
    }
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, longs, "java/util/stream/LongStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- Stream.mapToDouble ---
pub(crate) fn p56_stream_map_to_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the object elements (native stale-local family).
    let func_pin = ctx.pin_native_root(func);
    let pins = pin_object_values(ctx, &elems);
    let mut doubles = Vec::new();
    for (v, p) in elems.iter().zip(&pins) {
        let f = ctx.read_native_pin(func_pin, func);
        let v = read_pinned_object_value(ctx, *p, *v);
        let r = match ctx.invoke_virtual(f, "applyAsDouble", "(Ljava/lang/Object;)D", &[v]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(func_pin);
                return Err(e);
            }
        };
        doubles.push(r.unwrap_or(Value::Double(0.0)));
    }
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, doubles, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.peek ---
pub(crate) fn p56_int_stream_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the consumer callbacks below — a moving young GC there would
    // relocate `consumer` (native stale-local family; elements are primitive).
    let consumer_pin = ctx.pin_native_root(consumer);
    for v in &elems {
        let c = ctx.read_native_pin(consumer_pin, consumer);
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(I)V", &[*v]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(consumer_pin);
    let result = p56_build_stream(ctx, elems, "java/util/stream/IntStream");
    Ok(Some(Value::Object(Some(result))))
}

// --- IntStream.takeWhile ---
pub(crate) fn p56_int_stream_take_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` (native stale-local family; elements are primitive).
    let predicate_pin = ctx.pin_native_root(predicate);
    let mut result = Vec::new();
    for v in elems {
        let pred = ctx.read_native_pin(predicate_pin, predicate);
        let test = match ctx.invoke_virtual(pred, "test", "(I)Z", &[v]) {
            Ok(t) => t,
            Err(e) => {
                ctx.unpin_native_roots(predicate_pin);
                return Err(e);
            }
        };
        if test.unwrap_or(Value::Int(0)) == Value::Int(0) {
            break;
        }
        result.push(v);
    }
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/IntStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.dropWhile ---
pub(crate) fn p56_int_stream_drop_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` (native stale-local family; elements are primitive).
    let predicate_pin = ctx.pin_native_root(predicate);
    let mut dropping = true;
    let mut result = Vec::new();
    for v in elems {
        if dropping {
            let pred = ctx.read_native_pin(predicate_pin, predicate);
            let test = match ctx.invoke_virtual(pred, "test", "(I)Z", &[v]) {
                Ok(t) => t,
                Err(e) => {
                    ctx.unpin_native_roots(predicate_pin);
                    return Err(e);
                }
            };
            if test.unwrap_or(Value::Int(0)) != Value::Int(0) {
                continue;
            }
            dropping = false;
        }
        result.push(v);
    }
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/IntStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.sorted → IntStream (natural ascending order) ---
// Without this native, the synthetic IntStream's `sorted()` dispatched to the
// abstract `IntStream.sorted()` interface method → "has no Code attribute" AME.
// Groovy's shaded ANTLR4 lexer (`LexerActionExecutor.execute`) calls it, so the
// Groovy LEXER died → every Groovy script failed to compile
// (SpringRepositoriesExtensionTests).
pub(crate) fn p56_int_stream_sorted(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut elems = p56_read_stream_elems(ctx, this);
    elems.sort_by_key(|v| v.as_int().unwrap_or(0));
    let s = p56_build_stream(ctx, elems, "java/util/stream/IntStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.boxed → Stream ---
pub(crate) fn p56_int_stream_boxed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Each Int is already a Value::Int, box by wrapping in Integer wrapper.
    // Pin each wrapper across the subsequent allocs — a moving young GC there
    // would relocate the earlier wrappers (native stale-local family).
    let mut boxed = Vec::with_capacity(elems.len());
    let mut pins = Vec::with_capacity(elems.len());
    let mut first_pin = None;
    for v in elems {
        let wrapper = alloc_concurrent_synthetic(ctx, "java/lang/Integer", 1);
        let h = ctx.pin_native_root(wrapper);
        if first_pin.is_none() {
            first_pin = Some(h);
        }
        ctx.set_field(wrapper, 0, v);
        boxed.push(Value::Object(Some(wrapper)));
        pins.push(Some((h, wrapper)));
    }
    let boxed = read_pinned_object_values(ctx, &pins, &boxed);
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    let s = p56_build_stream(ctx, boxed, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.asLongStream → LongStream ---
pub(crate) fn p56_int_stream_as_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let longs: Vec<Value> = elems
        .into_iter()
        .map(|v| match v {
            Value::Int(i) => Value::Long(i as i64),
            other => other,
        })
        .collect();
    let s = p56_build_stream(ctx, longs, "java/util/stream/LongStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.asDoubleStream → DoubleStream ---
pub(crate) fn p56_int_stream_as_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let doubles: Vec<Value> = elems
        .into_iter()
        .map(|v| match v {
            Value::Int(i) => Value::Double(i as f64),
            other => other,
        })
        .collect();
    let s = p56_build_stream(ctx, doubles, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.concat ---
pub(crate) fn p56_int_stream_concat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let mut elems = p56_read_stream_elems(ctx, a);
    elems.extend(p56_read_stream_elems(ctx, b));
    let s = p56_build_stream(ctx, elems, "java/util/stream/IntStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- IntStream.forEachOrdered ---
pub(crate) fn p56_int_stream_for_each_ordered(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the consumer callbacks below — a moving young GC there would
    // relocate `consumer` (native stale-local family; elements are primitive).
    let consumer_pin = ctx.pin_native_root(consumer);
    for v in elems {
        let c = ctx.read_native_pin(consumer_pin, consumer);
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(I)V", &[v]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(consumer_pin);
    Ok(None)
}

// --- LongStream.peek ---
pub(crate) fn p56_long_stream_peek(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the consumer callbacks below — a moving young GC there would
    // relocate `consumer` (native stale-local family; elements are primitive).
    let consumer_pin = ctx.pin_native_root(consumer);
    for v in &elems {
        let c = ctx.read_native_pin(consumer_pin, consumer);
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(J)V", &[*v]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(consumer_pin);
    let result = p56_build_stream(ctx, elems, "java/util/stream/LongStream");
    Ok(Some(Value::Object(Some(result))))
}

// --- LongStream.takeWhile ---
pub(crate) fn p56_long_stream_take_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` (native stale-local family; elements are primitive).
    let predicate_pin = ctx.pin_native_root(predicate);
    let mut result = Vec::new();
    for v in elems {
        let pred = ctx.read_native_pin(predicate_pin, predicate);
        let test = match ctx.invoke_virtual(pred, "test", "(J)Z", &[v]) {
            Ok(t) => t,
            Err(e) => {
                ctx.unpin_native_roots(predicate_pin);
                return Err(e);
            }
        };
        if test.unwrap_or(Value::Int(0)) == Value::Int(0) {
            break;
        }
        result.push(v);
    }
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/LongStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- LongStream.dropWhile ---
pub(crate) fn p56_long_stream_drop_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` (native stale-local family; elements are primitive).
    let predicate_pin = ctx.pin_native_root(predicate);
    let mut dropping = true;
    let mut result = Vec::new();
    for v in elems {
        if dropping {
            let pred = ctx.read_native_pin(predicate_pin, predicate);
            let test = match ctx.invoke_virtual(pred, "test", "(J)Z", &[v]) {
                Ok(t) => t,
                Err(e) => {
                    ctx.unpin_native_roots(predicate_pin);
                    return Err(e);
                }
            };
            if test.unwrap_or(Value::Int(0)) != Value::Int(0) {
                continue;
            }
            dropping = false;
        }
        result.push(v);
    }
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/LongStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- LongStream.boxed → Stream ---
pub(crate) fn p56_long_stream_boxed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin each wrapper across the subsequent allocs — a moving young GC there
    // would relocate the earlier wrappers (native stale-local family).
    let mut boxed = Vec::with_capacity(elems.len());
    let mut pins = Vec::with_capacity(elems.len());
    let mut first_pin = None;
    for v in elems {
        let wrapper = alloc_concurrent_synthetic(ctx, "java/lang/Long", 1);
        let h = ctx.pin_native_root(wrapper);
        if first_pin.is_none() {
            first_pin = Some(h);
        }
        ctx.set_field(wrapper, 0, v);
        boxed.push(Value::Object(Some(wrapper)));
        pins.push(Some((h, wrapper)));
    }
    let boxed = read_pinned_object_values(ctx, &pins, &boxed);
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    let s = p56_build_stream(ctx, boxed, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- LongStream.mapToObj(LongFunction) -> Stream ---
// CratonVM's `LongStream` (and `DoubleStream`) is a synthetic object whose class
// is the interface itself; `boxed`/`map`/etc. are registered natives, but
// `mapToObj` was missing, so the call landed on the bodiless interface method
// (`AbstractMethodError: … has no Code attribute`). `IntStream.mapToObj` works
// because IntStream is not synthesised. Mirror `boxed`, applying the function.
pub(crate) fn p56_long_stream_map_to_obj(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the already-mapped results (native stale-local
    // family); each fresh result is pinned as it materialises.
    let func_pin = ctx.pin_native_root(func);
    let mut out = Vec::with_capacity(elems.len());
    let mut pins = Vec::with_capacity(elems.len());
    for v in elems {
        let f = ctx.read_native_pin(func_pin, func);
        let mapped = match ctx.invoke_virtual(f, "apply", "(J)Ljava/lang/Object;", &[v]) {
            Ok(m) => m.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(func_pin);
                return Err(e);
            }
        };
        pins.push(pinned_object_value(ctx, mapped));
        out.push(mapped);
    }
    let out = read_pinned_object_values(ctx, &pins, &out);
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, out, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- DoubleStream.mapToObj(DoubleFunction) -> Stream ---
pub(crate) fn p56_double_stream_map_to_obj(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the mapper callbacks below — a moving young GC there would
    // relocate `func` and the already-mapped results (native stale-local
    // family); each fresh result is pinned as it materialises.
    let func_pin = ctx.pin_native_root(func);
    let mut out = Vec::with_capacity(elems.len());
    let mut pins = Vec::with_capacity(elems.len());
    for v in elems {
        let f = ctx.read_native_pin(func_pin, func);
        let mapped = match ctx.invoke_virtual(f, "apply", "(D)Ljava/lang/Object;", &[v]) {
            Ok(m) => m.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(func_pin);
                return Err(e);
            }
        };
        pins.push(pinned_object_value(ctx, mapped));
        out.push(mapped);
    }
    let out = read_pinned_object_values(ctx, &pins, &out);
    ctx.unpin_native_roots(func_pin);
    let s = p56_build_stream(ctx, out, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- LongStream.asDoubleStream ---
pub(crate) fn p56_long_stream_as_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let doubles: Vec<Value> = elems
        .into_iter()
        .map(|v| match v {
            Value::Long(l) => Value::Double(l as f64),
            other => other,
        })
        .collect();
    let s = p56_build_stream(ctx, doubles, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- LongStream.concat ---
pub(crate) fn p56_long_stream_concat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let mut elems = p56_read_stream_elems(ctx, a);
    elems.extend(p56_read_stream_elems(ctx, b));
    let s = p56_build_stream(ctx, elems, "java/util/stream/LongStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- DoubleStream.peek ---
pub(crate) fn p56_double_stream_peek(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the consumer callbacks below — a moving young GC there would
    // relocate `consumer` (native stale-local family; elements are primitive).
    let consumer_pin = ctx.pin_native_root(consumer);
    for v in &elems {
        let c = ctx.read_native_pin(consumer_pin, consumer);
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(D)V", &[*v]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(consumer_pin);
    let result = p56_build_stream(ctx, elems, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(result))))
}

// --- DoubleStream.takeWhile ---
pub(crate) fn p56_double_stream_take_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` (native stale-local family; elements are primitive).
    let predicate_pin = ctx.pin_native_root(predicate);
    let mut result = Vec::new();
    for v in elems {
        let pred = ctx.read_native_pin(predicate_pin, predicate);
        let test = match ctx.invoke_virtual(pred, "test", "(D)Z", &[v]) {
            Ok(t) => t,
            Err(e) => {
                ctx.unpin_native_roots(predicate_pin);
                return Err(e);
            }
        };
        if test.unwrap_or(Value::Int(0)) == Value::Int(0) {
            break;
        }
        result.push(v);
    }
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- DoubleStream.dropWhile ---
pub(crate) fn p56_double_stream_drop_while(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let predicate = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin across the predicate callbacks below — a moving young GC there would
    // relocate `predicate` (native stale-local family; elements are primitive).
    let predicate_pin = ctx.pin_native_root(predicate);
    let mut dropping = true;
    let mut result = Vec::new();
    for v in elems {
        if dropping {
            let pred = ctx.read_native_pin(predicate_pin, predicate);
            let test = match ctx.invoke_virtual(pred, "test", "(D)Z", &[v]) {
                Ok(t) => t,
                Err(e) => {
                    ctx.unpin_native_roots(predicate_pin);
                    return Err(e);
                }
            };
            if test.unwrap_or(Value::Int(0)) != Value::Int(0) {
                continue;
            }
            dropping = false;
        }
        result.push(v);
    }
    ctx.unpin_native_roots(predicate_pin);
    let s = p56_build_stream(ctx, result, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(s))))
}

// --- DoubleStream.boxed → Stream ---
pub(crate) fn p56_double_stream_boxed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    // Pin each wrapper across the subsequent allocs — a moving young GC there
    // would relocate the earlier wrappers (native stale-local family).
    let mut boxed = Vec::with_capacity(elems.len());
    let mut pins = Vec::with_capacity(elems.len());
    let mut first_pin = None;
    for v in elems {
        let wrapper = alloc_concurrent_synthetic(ctx, "java/lang/Double", 1);
        let h = ctx.pin_native_root(wrapper);
        if first_pin.is_none() {
            first_pin = Some(h);
        }
        ctx.set_field(wrapper, 0, v);
        boxed.push(Value::Object(Some(wrapper)));
        pins.push(Some((h, wrapper)));
    }
    let boxed = read_pinned_object_values(ctx, &pins, &boxed);
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    let s = p56_build_stream(ctx, boxed, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(s))))
}

// --- DoubleStream.concat ---
pub(crate) fn p56_double_stream_concat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let mut elems = p56_read_stream_elems(ctx, a);
    elems.extend(p56_read_stream_elems(ctx, b));
    let s = p56_build_stream(ctx, elems, "java/util/stream/DoubleStream");
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// Summary Statistics: IntSummaryStatistics, LongSummaryStatistics, DoubleSummaryStatistics
// 4-field synthetic: (count=0 Long, sum=1 Long/Double, min=2, max=3)
// ---------------------------------------------------------------------------
pub(crate) const STATS_FIELD_COUNT: usize = 0;

pub(crate) const STATS_FIELD_SUM: usize = 1;

pub(crate) const STATS_FIELD_MIN: usize = 2;

pub(crate) const STATS_FIELD_MAX: usize = 3;

pub(crate) fn p56_int_stream_summary_stats(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let count = elems.len() as i64;
    let mut sum: i64 = 0;
    let mut min = i32::MAX;
    let mut max = i32::MIN;
    for v in &elems {
        if let Value::Int(i) = v {
            sum += *i as i64;
            if *i < min {
                min = *i;
            }
            if *i > max {
                max = *i;
            }
        }
    }
    if count == 0 {
        min = i32::MAX;
        max = i32::MIN;
    }
    let stats = alloc_concurrent_synthetic(ctx, "java/util/IntSummaryStatistics", 4);
    ctx.set_field(stats, STATS_FIELD_COUNT, Value::Long(count));
    ctx.set_field(stats, STATS_FIELD_SUM, Value::Long(sum));
    ctx.set_field(stats, STATS_FIELD_MIN, Value::Int(min));
    ctx.set_field(stats, STATS_FIELD_MAX, Value::Int(max));
    Ok(Some(Value::Object(Some(stats))))
}

pub(crate) fn p56_long_stream_summary_stats(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let count = elems.len() as i64;
    let mut sum: i64 = 0;
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for v in &elems {
        if let Value::Long(l) = v {
            sum = sum.wrapping_add(*l);
            if *l < min {
                min = *l;
            }
            if *l > max {
                max = *l;
            }
        }
    }
    if count == 0 {
        min = i64::MAX;
        max = i64::MIN;
    }
    let stats = alloc_concurrent_synthetic(ctx, "java/util/LongSummaryStatistics", 4);
    ctx.set_field(stats, STATS_FIELD_COUNT, Value::Long(count));
    ctx.set_field(stats, STATS_FIELD_SUM, Value::Long(sum));
    ctx.set_field(stats, STATS_FIELD_MIN, Value::Long(min));
    ctx.set_field(stats, STATS_FIELD_MAX, Value::Long(max));
    Ok(Some(Value::Object(Some(stats))))
}

pub(crate) fn p56_double_stream_summary_stats(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let count = elems.len() as i64;
    let mut sum: f64 = 0.0;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for v in &elems {
        if let Value::Double(d) = v {
            sum += *d;
            if *d < min {
                min = *d;
            }
            if *d > max {
                max = *d;
            }
        }
    }
    if count == 0 {
        min = f64::INFINITY;
        max = f64::NEG_INFINITY;
    }
    let stats = alloc_concurrent_synthetic(ctx, "java/util/DoubleSummaryStatistics", 4);
    ctx.set_field(stats, STATS_FIELD_COUNT, Value::Long(count));
    ctx.set_field(stats, STATS_FIELD_SUM, Value::Double(sum));
    ctx.set_field(stats, STATS_FIELD_MIN, Value::Double(min));
    ctx.set_field(stats, STATS_FIELD_MAX, Value::Double(max));
    Ok(Some(Value::Object(Some(stats))))
}

pub(crate) fn register_phase56_summary_stats(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- IntSummaryStatistics ---
    let iss = "java/util/IntSummaryStatistics";
    // The no-arg ctor must seed the identity values, not leave the slots at
    // their allocation default. The JDK starts min at Integer.MAX_VALUE and
    // max at Integer.MIN_VALUE so the first accept() wins both comparisons.
    // With an all-default object `accept()`'s `min.min(val)` folded against a
    // stale 0 and `getMin()` on an empty statistics answered 0 instead of
    // Integer.MAX_VALUE — e.g. `IntStream.of(5, 7)` reported min 0.
    r.register(iss, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(0));
        ctx.set_field(this, STATS_FIELD_SUM, Value::Long(0));
        ctx.set_field(this, STATS_FIELD_MIN, Value::Int(i32::MAX));
        ctx.set_field(this, STATS_FIELD_MAX, Value::Int(i32::MIN));
        Ok(None)
    });
    r.register(iss, "getCount", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_COUNT)))
    });
    r.register(iss, "getSum", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_SUM)))
    });
    r.register(iss, "getMin", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_MIN)))
    });
    r.register(iss, "getMax", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_MAX)))
    });
    r.register(iss, "getAverage", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Long(l) => l,
            _ => 0,
        };
        let avg = if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        };
        Ok(Some(Value::Double(avg)))
    });
    r.register(iss, "accept", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Long(l) => l,
            _ => 0,
        };
        let min = match ctx.get_field(this, STATS_FIELD_MIN) {
            Value::Int(i) => i,
            _ => i32::MAX,
        };
        let max = match ctx.get_field(this, STATS_FIELD_MAX) {
            Value::Int(i) => i,
            _ => i32::MIN,
        };
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(count + 1));
        ctx.set_field(this, STATS_FIELD_SUM, Value::Long(sum + val as i64));
        ctx.set_field(this, STATS_FIELD_MIN, Value::Int(min.min(val)));
        ctx.set_field(this, STATS_FIELD_MAX, Value::Int(max.max(val)));
        Ok(None)
    });
    r.register(iss, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Long(l) => l,
            _ => 0,
        };
        let min = match ctx.get_field(this, STATS_FIELD_MIN) {
            Value::Int(i) => i,
            _ => 0,
        };
        let max = match ctx.get_field(this, STATS_FIELD_MAX) {
            Value::Int(i) => i,
            _ => 0,
        };
        let s = format!(
            "IntSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}",
            count,
            sum,
            min,
            if count == 0 {
                0.0
            } else {
                sum as f64 / count as f64
            },
            max
        );
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });

    // --- LongSummaryStatistics ---
    let lss = "java/util/LongSummaryStatistics";
    // Identity seeding — see the IntSummaryStatistics ctor above.
    r.register(lss, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(0));
        ctx.set_field(this, STATS_FIELD_SUM, Value::Long(0));
        ctx.set_field(this, STATS_FIELD_MIN, Value::Long(i64::MAX));
        ctx.set_field(this, STATS_FIELD_MAX, Value::Long(i64::MIN));
        Ok(None)
    });
    r.register(lss, "getCount", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_COUNT)))
    });
    r.register(lss, "getSum", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_SUM)))
    });
    r.register(lss, "getMin", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_MIN)))
    });
    r.register(lss, "getMax", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_MAX)))
    });
    r.register(lss, "getAverage", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Long(l) => l,
            _ => 0,
        };
        let avg = if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        };
        Ok(Some(Value::Double(avg)))
    });
    r.register(lss, "accept", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args[1] {
            Value::Long(l) => l,
            _ => 0,
        };
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Long(l) => l,
            _ => 0,
        };
        let min = match ctx.get_field(this, STATS_FIELD_MIN) {
            Value::Long(l) => l,
            _ => i64::MAX,
        };
        let max = match ctx.get_field(this, STATS_FIELD_MAX) {
            Value::Long(l) => l,
            _ => i64::MIN,
        };
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(count + 1));
        ctx.set_field(this, STATS_FIELD_SUM, Value::Long(sum.wrapping_add(val)));
        ctx.set_field(this, STATS_FIELD_MIN, Value::Long(min.min(val)));
        ctx.set_field(this, STATS_FIELD_MAX, Value::Long(max.max(val)));
        Ok(None)
    });
    r.register(lss, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Long(l) => l,
            _ => 0,
        };
        let min = match ctx.get_field(this, STATS_FIELD_MIN) {
            Value::Long(l) => l,
            _ => 0,
        };
        let max = match ctx.get_field(this, STATS_FIELD_MAX) {
            Value::Long(l) => l,
            _ => 0,
        };
        let s = format!(
            "LongSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}",
            count,
            sum,
            min,
            if count == 0 {
                0.0
            } else {
                sum as f64 / count as f64
            },
            max
        );
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });

    // --- DoubleSummaryStatistics ---
    let dss = "java/util/DoubleSummaryStatistics";
    // Identity seeding — see the IntSummaryStatistics ctor above. The JDK
    // seeds min/max with +/-Infinity for the double flavour.
    r.register(dss, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(0));
        ctx.set_field(this, STATS_FIELD_SUM, Value::Double(0.0));
        ctx.set_field(this, STATS_FIELD_MIN, Value::Double(f64::INFINITY));
        ctx.set_field(this, STATS_FIELD_MAX, Value::Double(f64::NEG_INFINITY));
        Ok(None)
    });
    r.register(dss, "getCount", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_COUNT)))
    });
    r.register(dss, "getSum", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_SUM)))
    });
    r.register(dss, "getMin", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_MIN)))
    });
    r.register(dss, "getMax", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, STATS_FIELD_MAX)))
    });
    r.register(dss, "getAverage", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Double(d) => d,
            _ => 0.0,
        };
        let avg = if count == 0 { 0.0 } else { sum / count as f64 };
        Ok(Some(Value::Double(avg)))
    });
    r.register(dss, "accept", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args[1] {
            Value::Double(d) => d,
            _ => 0.0,
        };
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Double(d) => d,
            _ => 0.0,
        };
        let min = match ctx.get_field(this, STATS_FIELD_MIN) {
            Value::Double(d) => d,
            _ => f64::INFINITY,
        };
        let max = match ctx.get_field(this, STATS_FIELD_MAX) {
            Value::Double(d) => d,
            _ => f64::NEG_INFINITY,
        };
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(count + 1));
        ctx.set_field(this, STATS_FIELD_SUM, Value::Double(sum + val));
        ctx.set_field(this, STATS_FIELD_MIN, Value::Double(min.min(val)));
        ctx.set_field(this, STATS_FIELD_MAX, Value::Double(max.max(val)));
        Ok(None)
    });
    r.register(dss, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, STATS_FIELD_COUNT) {
            Value::Long(l) => l,
            _ => 0,
        };
        let sum = match ctx.get_field(this, STATS_FIELD_SUM) {
            Value::Double(d) => d,
            _ => 0.0,
        };
        let min = match ctx.get_field(this, STATS_FIELD_MIN) {
            Value::Double(d) => d,
            _ => 0.0,
        };
        let max = match ctx.get_field(this, STATS_FIELD_MAX) {
            Value::Double(d) => d,
            _ => 0.0,
        };
        let s = format!(
            "DoubleSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}",
            count,
            sum,
            min,
            if count == 0 { 0.0 } else { sum / count as f64 },
            max
        );
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Collectors expansion: maxBy, minBy, mapping, filtering, flatMapping,
// summarizingInt/Long/Double, toUnmodifiableList/Set/Map, collectingAndThen
// Collector tags. There is exactly ONE decoder for the tag we write here:
// native-collections' `native_stream_collect`, the only registered
// `Stream.collect(Collector)` in the VM. So these constants MUST live in
// native-collections' `COLLECTOR_TAG_*` numbering — they are aliases of it, not
// a parallel namespace.
//
// They used to be a private 9..15 numbering, and for the factories
// native-collections does NOT itself register (maxBy, minBy, filtering,
// summarizing{Int,Long,Double}) nothing overwrote them, so the tag we wrote was
// decoded in the wrong namespace: 9 → GROUPING_BY_DOWNSTREAM, 10 →
// GROUPING_BY_SUPPLIER, 12 → TO_MAP_MERGE, 13 → COLLECTING_AND_THEN, 14 →
// TO_COLLECTION, 15 → MAPPING. `stream.collect(Collectors.minBy(cmp))` handed
// the program a Map instead of an Optional — a silent wrong answer.
//
// `mapping` and `collectingAndThen` are also registered by native-collections,
// whose registration wins (registry is last-wins and
// `register_collections_natives` runs after `register_builtins`), so their old
// 11/18 values never reached the decoder. They are aliased here anyway so a
// future ordering change cannot resurrect the same bug.
//
// averaging* (19..21) and summing* (22..24) were the second half of the same
// bug, reached by a different route: those numbers were in NO decoder table at
// all, so `is_known_collector_tag` rejected them, `collect` treated the
// collector as an untagged JDK one, and `collect(averagingInt(f))` handed back
// the raw accumulation ArrayList where a `Double` is required. They are now
// tags 31..36 in native-collections and aliased below; 17..24 are unallocated.
// ---------------------------------------------------------------------------
pub(crate) const P56_COLLECTOR_MAX_BY: i32 = cratonvm_native_collections::COLLECTOR_TAG_MAX_BY;

pub(crate) const P56_COLLECTOR_MIN_BY: i32 = cratonvm_native_collections::COLLECTOR_TAG_MIN_BY;

pub(crate) const P56_COLLECTOR_MAPPING: i32 = cratonvm_native_collections::COLLECTOR_TAG_MAPPING;

pub(crate) const P56_COLLECTOR_FILTERING: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_FILTERING;

pub(crate) const P56_COLLECTOR_SUMMARIZING_INT: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_SUMMARIZING_INT;

pub(crate) const P56_COLLECTOR_SUMMARIZING_LONG: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_SUMMARIZING_LONG;

pub(crate) const P56_COLLECTOR_SUMMARIZING_DOUBLE: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_SUMMARIZING_DOUBLE;

pub(crate) const P56_COLLECTOR_TO_UNMODIFIABLE_LIST: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_TO_LIST;

pub(crate) const P56_COLLECTOR_TO_UNMODIFIABLE_SET: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_TO_SET;

pub(crate) const P56_COLLECTOR_COLLECTING_AND_THEN: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_COLLECTING_AND_THEN;

pub(crate) const P56_COLLECTOR_AVERAGING_INT: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_AVERAGING_INT;

pub(crate) const P56_COLLECTOR_AVERAGING_LONG: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_AVERAGING_LONG;

pub(crate) const P56_COLLECTOR_AVERAGING_DOUBLE: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_AVERAGING_DOUBLE;

pub(crate) const P56_COLLECTOR_SUMMING_INT: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_SUMMING_INT;

pub(crate) const P56_COLLECTOR_SUMMING_LONG: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_SUMMING_LONG;

pub(crate) const P56_COLLECTOR_SUMMING_DOUBLE: i32 =
    cratonvm_native_collections::COLLECTOR_TAG_SUMMING_DOUBLE;

/// `Collectors.teeing` — registered in Phase 64 below, same shared numbering.
pub(crate) const P64_COLLECTOR_TEEING: i32 = cratonvm_native_collections::COLLECTOR_TAG_TEEING;

pub(crate) fn register_phase56_collectors_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let col = "java/util/stream/Collectors";

    // --- maxBy(Comparator) → Collector ---
    r.register(
        col,
        "maxBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let comparator = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let comparator_pin = pinned_object_value(ctx, comparator);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            let comparator = read_pinned_object_value(ctx, comparator_pin, comparator);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_MAX_BY));
            ctx.set_field(c, 1, comparator);
            if let Some((h, _)) = comparator_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- minBy(Comparator) → Collector ---
    r.register(
        col,
        "minBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let comparator = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let comparator_pin = pinned_object_value(ctx, comparator);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            let comparator = read_pinned_object_value(ctx, comparator_pin, comparator);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_MIN_BY));
            ctx.set_field(c, 1, comparator);
            if let Some((h, _)) = comparator_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- mapping(Function, Collector) → Collector ---
    r.register(
        col,
        "mapping",
        "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            let downstream = args[1];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let downstream_pin = pinned_object_value(ctx, downstream);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_MAPPING));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func)); // Function
            ctx.set_field(
                c,
                2,
                read_pinned_object_value(ctx, downstream_pin, downstream),
            ); // downstream Collector
            if let Some((h, _)) = func_pin.or(downstream_pin) {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- filtering(Predicate, Collector) → Collector ---
    r.register(
        col,
        "filtering",
        "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let pred = args[0];
            let downstream = args[1];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let pred_pin = pinned_object_value(ctx, pred);
            let downstream_pin = pinned_object_value(ctx, downstream);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_FILTERING));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, pred_pin, pred)); // Predicate
            ctx.set_field(
                c,
                2,
                read_pinned_object_value(ctx, downstream_pin, downstream),
            ); // downstream Collector
            if let Some((h, _)) = pred_pin.or(downstream_pin) {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summarizingInt(ToIntFunction) → Collector ---
    r.register(
        col,
        "summarizingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_SUMMARIZING_INT));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summarizingLong(ToLongFunction) → Collector ---
    r.register(
        col,
        "summarizingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_SUMMARIZING_LONG));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summarizingDouble(ToDoubleFunction) → Collector ---
    r.register(
        col,
        "summarizingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_SUMMARIZING_DOUBLE));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- toUnmodifiableList() → Collector ---
    r.register(
        col,
        "toUnmodifiableList",
        "()Ljava/util/stream/Collector;",
        |ctx, _args| {
            let c = cratonvm_native_collections::make_to_list_collector(ctx);
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- toUnmodifiableSet() → Collector ---
    r.register(
        col,
        "toUnmodifiableSet",
        "()Ljava/util/stream/Collector;",
        |ctx, _args| {
            let c = cratonvm_native_collections::make_to_set_collector(ctx);
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- collectingAndThen(Collector, Function) → Collector ---
    r.register(
        col,
        "collectingAndThen",
        "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let downstream = args[0];
            let finisher = args[1];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let downstream_pin = pinned_object_value(ctx, downstream);
            let finisher_pin = pinned_object_value(ctx, finisher);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_COLLECTING_AND_THEN));
            ctx.set_field(
                c,
                1,
                read_pinned_object_value(ctx, downstream_pin, downstream),
            ); // downstream Collector
            ctx.set_field(c, 2, read_pinned_object_value(ctx, finisher_pin, finisher)); // finisher Function
            if let Some((h, _)) = downstream_pin.or(finisher_pin) {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- averagingInt(ToIntFunction) → Collector (returns a boxed Double) ---
    r.register(
        col,
        "averagingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_AVERAGING_INT));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "averagingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_AVERAGING_LONG));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "averagingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_AVERAGING_DOUBLE));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summingInt/Long/Double ---
    r.register(
        col,
        "summingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_SUMMING_INT));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "summingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_SUMMING_LONG));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "summingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args[0];
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let func_pin = pinned_object_value(ctx, func);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 3);
            ctx.set_field(c, 0, Value::Int(P56_COLLECTOR_SUMMING_DOUBLE));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, func_pin, func));
            if let Some((h, _)) = func_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// java.util.function — BiFunction, BiConsumer, BiPredicate, UnaryOperator,
// BinaryOperator interface dispatch registrations
// ---------------------------------------------------------------------------
pub(crate) fn register_phase56_function_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // BiFunction<T,U,R>.apply(T,U) → R — dispatched via invoke_virtual
    let bf = "java/util/function/BiFunction";
    r.register(
        bf,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[args[1], args[2]],
            )
        },
    );

    // BiConsumer<T,U>.accept(T,U) → void
    let bc = "java/util/function/BiConsumer";
    r.register(
        bc,
        "accept",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "accept",
                "(Ljava/lang/Object;Ljava/lang/Object;)V",
                &[args[1], args[2]],
            )
        },
    );

    // BiPredicate<T,U>.test(T,U) → boolean
    let bp = "java/util/function/BiPredicate";
    r.register(
        bp,
        "test",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "test",
                "(Ljava/lang/Object;Ljava/lang/Object;)Z",
                &[args[1], args[2]],
            )
        },
    );

    // UnaryOperator<T> extends Function<T,T> — apply is inherited from Function
    let uo = "java/util/function/UnaryOperator";
    r.register(
        uo,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[args[1]],
            )
        },
    );
    // UnaryOperator.identity()
    r.register(
        uo,
        "identity",
        "()Ljava/util/function/UnaryOperator;",
        |ctx, _args| {
            // Create a lambda proxy that returns its argument
            let proxy =
                alloc_concurrent_synthetic(ctx, "java/util/function/UnaryOperator$Identity", 0);
            Ok(Some(Value::Object(Some(proxy))))
        },
    );

    // BinaryOperator<T> extends BiFunction<T,T,T> — apply is inherited
    let bo = "java/util/function/BinaryOperator";
    r.register(
        bo,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[args[1], args[2]],
            )
        },
    );

    // BinaryOperator.maxBy(Comparator) → BinaryOperator
    r.register(
        bo,
        "maxBy",
        "(Ljava/util/Comparator;)Ljava/util/function/BinaryOperator;",
        |ctx, args| {
            let comparator = args[0];
            // Pin across the proxy alloc below — a moving young GC there would
            // relocate it (native stale-local family).
            let comparator_pin = pinned_object_value(ctx, comparator);
            // Store comparator in a 1-field synthetic
            let proxy =
                alloc_concurrent_synthetic(ctx, "java/util/function/BinaryOperator$MaxBy", 1);
            ctx.set_field(
                proxy,
                0,
                read_pinned_object_value(ctx, comparator_pin, comparator),
            );
            if let Some((h, _)) = comparator_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(proxy))))
        },
    );

    // BinaryOperator.minBy(Comparator) → BinaryOperator
    r.register(
        bo,
        "minBy",
        "(Ljava/util/Comparator;)Ljava/util/function/BinaryOperator;",
        |ctx, args| {
            let comparator = args[0];
            // Pin across the proxy alloc below — a moving young GC there would
            // relocate it (native stale-local family).
            let comparator_pin = pinned_object_value(ctx, comparator);
            let proxy =
                alloc_concurrent_synthetic(ctx, "java/util/function/BinaryOperator$MinBy", 1);
            ctx.set_field(
                proxy,
                0,
                read_pinned_object_value(ctx, comparator_pin, comparator),
            );
            if let Some((h, _)) = comparator_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(proxy))))
        },
    );

    // ToIntFunction, ToLongFunction, ToDoubleFunction interface dispatch
    r.register(
        "java/util/function/ToIntFunction",
        "applyAsInt",
        "(Ljava/lang/Object;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(Ljava/lang/Object;)I", &[args[1]])
        },
    );
    r.register(
        "java/util/function/ToLongFunction",
        "applyAsLong",
        "(Ljava/lang/Object;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(Ljava/lang/Object;)J", &[args[1]])
        },
    );
    r.register(
        "java/util/function/ToDoubleFunction",
        "applyAsDouble",
        "(Ljava/lang/Object;)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(Ljava/lang/Object;)D", &[args[1]])
        },
    );

    // IntFunction, LongFunction, DoubleFunction
    r.register(
        "java/util/function/IntFunction",
        "apply",
        "(I)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "apply", "(I)Ljava/lang/Object;", &[args[1]])
        },
    );
    r.register(
        "java/util/function/LongFunction",
        "apply",
        "(J)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "apply", "(J)Ljava/lang/Object;", &[args[1]])
        },
    );
    r.register(
        "java/util/function/DoubleFunction",
        "apply",
        "(D)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "apply", "(D)Ljava/lang/Object;", &[args[1]])
        },
    );

    // IntToLongFunction, IntToDoubleFunction, LongToIntFunction, etc.
    r.register(
        "java/util/function/IntToLongFunction",
        "applyAsLong",
        "(I)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(I)J", &[args[1]])
        },
    );
    r.register(
        "java/util/function/IntToDoubleFunction",
        "applyAsDouble",
        "(I)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(I)D", &[args[1]])
        },
    );
    r.register(
        "java/util/function/LongToIntFunction",
        "applyAsInt",
        "(J)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(J)I", &[args[1]])
        },
    );
    r.register(
        "java/util/function/LongToDoubleFunction",
        "applyAsDouble",
        "(J)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(J)D", &[args[1]])
        },
    );
    r.register(
        "java/util/function/DoubleToIntFunction",
        "applyAsInt",
        "(D)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(D)I", &[args[1]])
        },
    );
    r.register(
        "java/util/function/DoubleToLongFunction",
        "applyAsLong",
        "(D)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(D)J", &[args[1]])
        },
    );

    // IntBinaryOperator, LongBinaryOperator, DoubleBinaryOperator
    r.register(
        "java/util/function/IntBinaryOperator",
        "applyAsInt",
        "(II)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(II)I", &[args[1], args[2]])
        },
    );
    r.register(
        "java/util/function/LongBinaryOperator",
        "applyAsLong",
        "(JJ)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(JJ)J", &[args[1], args[2]])
        },
    );
    r.register(
        "java/util/function/DoubleBinaryOperator",
        "applyAsDouble",
        "(DD)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(DD)D", &[args[1], args[2]])
        },
    );

    // ObjIntConsumer, ObjLongConsumer, ObjDoubleConsumer
    r.register(
        "java/util/function/ObjIntConsumer",
        "accept",
        "(Ljava/lang/Object;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "accept",
                "(Ljava/lang/Object;I)V",
                &[args[1], args[2]],
            )
        },
    );
    r.register(
        "java/util/function/ObjLongConsumer",
        "accept",
        "(Ljava/lang/Object;J)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "accept",
                "(Ljava/lang/Object;J)V",
                &[args[1], args[2]],
            )
        },
    );
    r.register(
        "java/util/function/ObjDoubleConsumer",
        "accept",
        "(Ljava/lang/Object;D)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "accept",
                "(Ljava/lang/Object;D)V",
                &[args[1], args[2]],
            )
        },
    );

    // Predicate.and/or/negate — composite predicates via 2-field synthetic
    let pred = "java/util/function/Predicate";
    r.register(
        pred,
        "and",
        "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = args[1];
            // Pin across the composite alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let other_pin = pinned_object_value(ctx, other);
            let composite =
                alloc_concurrent_synthetic(ctx, "java/util/function/Predicate$$Lambda$And", 2);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(composite, 0, Value::Object(Some(this)));
            ctx.set_field(
                composite,
                1,
                read_pinned_object_value(ctx, other_pin, other),
            );
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(composite))))
        },
    );
    r.register(
        pred,
        "or",
        "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = args[1];
            // Pin across the composite alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let other_pin = pinned_object_value(ctx, other);
            let composite =
                alloc_concurrent_synthetic(ctx, "java/util/function/Predicate$$Lambda$Or", 2);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(composite, 0, Value::Object(Some(this)));
            ctx.set_field(
                composite,
                1,
                read_pinned_object_value(ctx, other_pin, other),
            );
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(composite))))
        },
    );
    r.register(
        pred,
        "negate",
        "()Ljava/util/function/Predicate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Pin across the composite alloc below — a moving young GC there
            // would relocate `this` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let composite =
                alloc_concurrent_synthetic(ctx, "java/util/function/Predicate$$Lambda$Negate", 1);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(composite, 0, Value::Object(Some(this)));
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(composite))))
        },
    );
    // Predicate.not(Predicate) — static
    r.register(
        pred,
        "not",
        "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;",
        |ctx, args| {
            let target = args[0];
            // Pin across the composite alloc below — a moving young GC there
            // would relocate it (native stale-local family).
            let target_pin = pinned_object_value(ctx, target);
            let composite =
                alloc_concurrent_synthetic(ctx, "java/util/function/Predicate$$Lambda$Negate", 1);
            ctx.set_field(
                composite,
                0,
                read_pinned_object_value(ctx, target_pin, target),
            );
            if let Some((h, _)) = target_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(composite))))
        },
    );

    // --- M3 fix: register test() on synthetic Predicate composition classes ---

    // Predicate$And.test(x) = first.test(x) && second.test(x)
    r.register(
        "java/util/function/Predicate$$Lambda$And",
        "test",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut input = args[1];
            let first = ctx.get_field(this, 0);
            let mut second = ctx.get_field(this, 1);
            if let Value::Object(Some(first_ref)) = first {
                // Pin across the first test() below — a moving young GC there
                // would relocate `second`/`input` (native stale-local family).
                let second_pin = pinned_object_value(ctx, second);
                let input_pin = pinned_object_value(ctx, input);
                let r1 = match ctx.invoke_virtual(
                    first_ref,
                    "test",
                    "(Ljava/lang/Object;)Z",
                    &[input],
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        if let Some((h, _)) = second_pin.or(input_pin) {
                            ctx.unpin_native_roots(h);
                        }
                        return Err(e);
                    }
                };
                second = read_pinned_object_value(ctx, second_pin, second);
                input = read_pinned_object_value(ctx, input_pin, input);
                if let Some((h, _)) = second_pin.or(input_pin) {
                    ctx.unpin_native_roots(h);
                }
                if r1 == Some(Value::Int(0)) {
                    return Ok(Some(Value::Int(0)));
                }
            }
            if let Value::Object(Some(second_ref)) = second {
                return ctx.invoke_virtual(second_ref, "test", "(Ljava/lang/Object;)Z", &[input]);
            }
            Ok(Some(Value::Int(0)))
        },
    );

    // Predicate$Or.test(x) = first.test(x) || second.test(x)
    r.register(
        "java/util/function/Predicate$$Lambda$Or",
        "test",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut input = args[1];
            let first = ctx.get_field(this, 0);
            let mut second = ctx.get_field(this, 1);
            if let Value::Object(Some(first_ref)) = first {
                // Pin across the first test() below — a moving young GC there
                // would relocate `second`/`input` (native stale-local family).
                let second_pin = pinned_object_value(ctx, second);
                let input_pin = pinned_object_value(ctx, input);
                let r1 = match ctx.invoke_virtual(
                    first_ref,
                    "test",
                    "(Ljava/lang/Object;)Z",
                    &[input],
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        if let Some((h, _)) = second_pin.or(input_pin) {
                            ctx.unpin_native_roots(h);
                        }
                        return Err(e);
                    }
                };
                second = read_pinned_object_value(ctx, second_pin, second);
                input = read_pinned_object_value(ctx, input_pin, input);
                if let Some((h, _)) = second_pin.or(input_pin) {
                    ctx.unpin_native_roots(h);
                }
                if r1 == Some(Value::Int(1)) {
                    return Ok(Some(Value::Int(1)));
                }
            }
            if let Value::Object(Some(second_ref)) = second {
                return ctx.invoke_virtual(second_ref, "test", "(Ljava/lang/Object;)Z", &[input]);
            }
            Ok(Some(Value::Int(0)))
        },
    );

    // Predicate$Negate.test(x) = !inner.test(x)
    r.register(
        "java/util/function/Predicate$$Lambda$Negate",
        "test",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let input = args[1];
            let inner = ctx.get_field(this, 0);
            if let Value::Object(Some(inner_ref)) = inner {
                let r1 =
                    ctx.invoke_virtual(inner_ref, "test", "(Ljava/lang/Object;)Z", &[input])?;
                let val = match r1 {
                    Some(Value::Int(v)) => v,
                    _ => 0,
                };
                return Ok(Some(Value::Int(if val == 0 { 1 } else { 0 })));
            }
            Ok(Some(Value::Int(1)))
        },
    );

    // Function.compose/andThen
    let func = "java/util/function/Function";
    r.register(
        func,
        "compose",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let before = args[1];
            // Pin across the composite alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let before_pin = pinned_object_value(ctx, before);
            let composite =
                alloc_concurrent_synthetic(ctx, "java/util/function/Function$Compose", 2);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(composite, 0, Value::Object(Some(this)));
            ctx.set_field(
                composite,
                1,
                read_pinned_object_value(ctx, before_pin, before),
            );
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(composite))))
        },
    );
    r.register(
        func,
        "andThen",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let after = args[1];
            // Pin across the composite alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let after_pin = pinned_object_value(ctx, after);
            let composite =
                alloc_concurrent_synthetic(ctx, "java/util/function/Function$AndThen", 2);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(composite, 0, Value::Object(Some(this)));
            ctx.set_field(
                composite,
                1,
                read_pinned_object_value(ctx, after_pin, after),
            );
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(composite))))
        },
    );
    // Function.identity()
    r.register(
        func,
        "identity",
        "()Ljava/util/function/Function;",
        |ctx, _args| {
            let proxy = alloc_concurrent_synthetic(ctx, "java/util/function/Function$Identity", 0);
            Ok(Some(Value::Object(Some(proxy))))
        },
    );

    // Consumer.andThen
    let cons = "java/util/function/Consumer";
    r.register(
        cons,
        "andThen",
        "(Ljava/util/function/Consumer;)Ljava/util/function/Consumer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let after = args[1];
            // Pin across the composite alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let after_pin = pinned_object_value(ctx, after);
            let composite =
                alloc_concurrent_synthetic(ctx, "java/util/function/Consumer$AndThen", 2);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(composite, 0, Value::Object(Some(this)));
            ctx.set_field(
                composite,
                1,
                read_pinned_object_value(ctx, after_pin, after),
            );
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(composite))))
        },
    );

    // --- M3 fix: register apply/accept on synthetic composition classes ---

    // Function$AndThen.apply(x) = after.apply(first.apply(x))
    r.register(
        "java/util/function/Function$AndThen",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let input = args[1];
            let first = ctx.get_field(this, 0); // field 0 = first function
            let after = ctx.get_field(this, 1); // field 1 = after function
            let first_ref = match first {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(input)),
            };
            // Pin across the first apply() below — a moving young GC there
            // would relocate `after` (native stale-local family).
            let after_pin = pinned_object_value(ctx, after);
            // Use invoke_virtual to support lambda proxy dispatch
            let mid = match ctx.invoke_virtual(
                first_ref,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[input],
            ) {
                Ok(m) => m,
                Err(e) => {
                    if let Some((h, _)) = after_pin {
                        ctx.unpin_native_roots(h);
                    }
                    return Err(e);
                }
            };
            let mid_val = mid.unwrap_or(Value::Object(None));
            let after = read_pinned_object_value(ctx, after_pin, after);
            if let Some((h, _)) = after_pin {
                ctx.unpin_native_roots(h);
            }
            let after_ref = match after {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(mid_val)),
            };
            ctx.invoke_virtual(
                after_ref,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[mid_val],
            )
        },
    );

    // Function$Compose.apply(x) = first.apply(before.apply(x))
    r.register(
        "java/util/function/Function$Compose",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let input = args[1];
            let first = ctx.get_field(this, 0); // field 0 = outer function
            let before = ctx.get_field(this, 1); // field 1 = before function
            let before_ref = match before {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(input)),
            };
            // Pin across the before apply() below — a moving young GC there
            // would relocate `first` (native stale-local family).
            let first_pin = pinned_object_value(ctx, first);
            // Use invoke_virtual to support lambda proxy dispatch
            let mid = match ctx.invoke_virtual(
                before_ref,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[input],
            ) {
                Ok(m) => m,
                Err(e) => {
                    if let Some((h, _)) = first_pin {
                        ctx.unpin_native_roots(h);
                    }
                    return Err(e);
                }
            };
            let mid_val = mid.unwrap_or(Value::Object(None));
            let first = read_pinned_object_value(ctx, first_pin, first);
            if let Some((h, _)) = first_pin {
                ctx.unpin_native_roots(h);
            }
            let first_ref = match first {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(mid_val)),
            };
            ctx.invoke_virtual(
                first_ref,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[mid_val],
            )
        },
    );

    // Function$Identity.apply(x) = x
    r.register(
        "java/util/function/Function$Identity",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, args| Ok(Some(args[1])),
    );

    // Consumer$AndThen.accept(x) = first.accept(x); after.accept(x);
    r.register(
        "java/util/function/Consumer$AndThen",
        "accept",
        "(Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut input = args[1];
            let first = ctx.get_field(this, 0);
            let mut after = ctx.get_field(this, 1);
            // Use invoke_virtual to support lambda proxy dispatch
            if let Value::Object(Some(first_ref)) = first {
                // Pin across the first accept() below — a moving young GC there
                // would relocate `after`/`input` (native stale-local family).
                let after_pin = pinned_object_value(ctx, after);
                let input_pin = pinned_object_value(ctx, input);
                if let Err(e) =
                    ctx.invoke_virtual(first_ref, "accept", "(Ljava/lang/Object;)V", &[input])
                {
                    if let Some((h, _)) = after_pin.or(input_pin) {
                        ctx.unpin_native_roots(h);
                    }
                    return Err(e);
                }
                after = read_pinned_object_value(ctx, after_pin, after);
                input = read_pinned_object_value(ctx, input_pin, input);
                if let Some((h, _)) = after_pin.or(input_pin) {
                    ctx.unpin_native_roots(h);
                }
            }
            if let Value::Object(Some(after_ref)) = after {
                ctx.invoke_virtual(after_ref, "accept", "(Ljava/lang/Object;)V", &[input])?;
            }
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// Spliterator completion + StreamSupport
// Spliterator = 2-field synthetic (array=0, cursor=1)
// =============================================================================

/// `java.util.Spliterator.SORTED`.
pub(crate) const P59_SPLITERATOR_SORTED: i32 = 0x04;

/// Characteristics bitset of a Spliterator receiver, obtained by dispatching
/// `characteristics()I` on it so an overriding implementation (real JDK
/// bytecode or a later native) answers for itself. Falls back to 0 ("nothing
/// guaranteed") when the call is unavailable — the conservative answer, since
/// every `hasCharacteristics` query then reports false only for bits nobody
/// claimed.
pub(crate) fn p59_spliterator_characteristics(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    match ctx.invoke_virtual(this, "characteristics", "()I", &[]) {
        Ok(Some(v)) => v.as_int().unwrap_or(0),
        _ => 0,
    }
}

pub(crate) fn register_p59_spliterator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let spl = "java/util/Spliterator";

    // Additional Spliterator methods
    r.register(spl, "getExactSizeIfKnown", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let cursor = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let len = ctx.array_length(arr);
            Ok(Some(Value::Long((len - cursor) as i64)))
        } else {
            Ok(Some(Value::Long(-1)))
        }
    });
    // getComparator(): the JDK contract is three-way — return the Comparator if
    // the source is SORTED by one, null if SORTED in natural order, and throw
    // IllegalStateException otherwise. Answering null unconditionally told
    // every caller "sorted, natural order", so callers that trust it (e.g. a
    // merge that assumes pre-sorted input) skipped their own sort and produced
    // silently unordered results. Our synthetic spliterators are never SORTED,
    // so this now throws — the spec'd answer for an unsorted source.
    r.register(
        spl,
        "getComparator",
        "()Ljava/util/Comparator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ch = p59_spliterator_characteristics(ctx, this);
            if (ch & P59_SPLITERATOR_SORTED) != 0 {
                // SORTED but no comparator recorded => natural ordering.
                return Ok(Some(Value::Object(None)));
            }
            Err(RuntimeError::IllegalStateException {
                message: "Spliterator source is not SORTED".into(),
            }
            .into())
        },
    );
    // hasCharacteristics(c) is defined as `(characteristics() & c) == c`.
    // The constant `false` contradicted characteristics() (which reports
    // ORDERED|SIZED), so callers took the "unordered / unsized" branch and
    // e.g. discarded encounter order or refused a sized short-circuit.
    r.register(spl, "hasCharacteristics", "(I)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let wanted = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let ch = p59_spliterator_characteristics(ctx, this);
        Ok(Some(Value::Int(if (ch & wanted) == wanted {
            1
        } else {
            0
        })))
    });

    // Spliterator constants — KEEP: these are `public static final int` fields
    // of java.util.Spliterator surfaced as descriptor-"I" natives. The values
    // are the JLS-visible constants themselves, so a constant body is the
    // correct (and only possible) implementation.
    r.register(spl, "ORDERED", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x10)))
    });
    r.register(spl, "DISTINCT", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x01)))
    });
    r.register(spl, "SORTED", "I", |_ctx, _args| Ok(Some(Value::Int(0x04))));
    r.register(spl, "SIZED", "I", |_ctx, _args| Ok(Some(Value::Int(0x40))));
    r.register(spl, "NONNULL", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x100)))
    });
    r.register(spl, "IMMUTABLE", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x400)))
    });
    r.register(spl, "CONCURRENT", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x1000)))
    });
    r.register(spl, "SUBSIZED", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x4000)))
    });

    // StreamSupport — factory methods
    let ss = "java/util/stream/StreamSupport";
    r.register(
        ss,
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        p59_stream_from_spliterator,
    );
    r.register(
        ss,
        "intStream",
        "(Ljava/util/Spliterator$OfInt;Z)Ljava/util/stream/IntStream;",
        p59_int_stream_from_spliterator,
    );
    r.register(
        ss,
        "longStream",
        "(Ljava/util/Spliterator$OfLong;Z)Ljava/util/stream/LongStream;",
        p59_long_stream_from_spliterator,
    );
    r.register(
        ss,
        "doubleStream",
        "(Ljava/util/Spliterator$OfDouble;Z)Ljava/util/stream/DoubleStream;",
        p59_double_stream_from_spliterator,
    );

    // Collection.spliterator() — returns a Spliterator backed by the collection
    r.register(
        "java/util/Collection",
        "spliterator",
        "()Ljava/util/Spliterator;",
        p59_collection_spliterator,
    );
    r.register(
        "java/util/List",
        "spliterator",
        "()Ljava/util/Spliterator;",
        p59_collection_spliterator,
    );
    r.register(
        "java/util/ArrayList",
        "spliterator",
        "()Ljava/util/Spliterator;",
        p59_collection_spliterator,
    );

    // S111r12: `HashSet.spliterator()` — JDK bytecode constructs a
    // `HashMap.KeySpliterator(this.map, ...)` and later does
    // `getfield m.table` on the wrapped map. With our synthetic HashMap
    // layout (slot 2 = capacity Int(16)), that read returns `Int(16)` and
    // `arraylength` on it surfaces as
    //   `internal error: expected object reference, got int(16)`.
    // Same family of failure as S111r7 (`System.getenv()` HashMap layout)
    // and S111r11 (`Properties.size`). Register `HashSet.spliterator()`
    // as a native that walks the synthetic backing map and returns a
    // synthetic Spliterator (data_array, cursor) — the existing
    // `p59_stream_from_spliterator` and `Spliterator.*` natives already
    // know how to consume that layout.
    r.register(
        "java/util/HashSet",
        "spliterator",
        "()Ljava/util/Spliterator;",
        p59_hashset_spliterator,
    );
    // Synthetic-stream `spliterator()` — see `p_int_stream_spliterator`. Routed
    // here for synthetic stream objects (stamped with the bare interface class)
    // via the no-Code receiver-walk rescue; real `*Pipeline` streams keep their
    // own bytecode. Fixes real JDK stream code (e.g. `IntStream.concat` →
    // `a.spliterator()`) that the synthetic streams otherwise can't satisfy.
    r.register(
        "java/util/stream/IntStream",
        "spliterator",
        "()Ljava/util/Spliterator$OfInt;",
        p_int_stream_spliterator,
    );
    r.register(
        "java/util/stream/LongStream",
        "spliterator",
        "()Ljava/util/Spliterator$OfLong;",
        p_long_stream_spliterator,
    );
    r.register(
        "java/util/stream/DoubleStream",
        "spliterator",
        "()Ljava/util/Spliterator$OfDouble;",
        p_double_stream_spliterator,
    );
    register_synthetic_stream_spliterators(r);
    r.set_category(__prev_cat);
}

/// Register the synthetic-stream `spliterator()` natives. Called from BOTH the
/// synthetic-JDK path (`register_p59_spliterator`) and the real-JDK path
/// (`register_essential_natives`) — synthetic stream objects (stamped with the
/// bare `java/util/stream/*Stream` interface, slot 0 = element array) are
/// produced in real-JDK mode too (e.g. `OptionalInt.stream()`), and real JDK
/// stream code (`IntStream.concat` → `a.spliterator()`; JUnit's
/// `getLegacyReportingIndexes`) then calls `spliterator()` on them. Without a
/// native the call falls to the abstract interface method → AbstractMethodError.
pub(crate) fn register_synthetic_stream_spliterators(r: &mut NativeMethodRegistry) {
    let __prev = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "java/util/stream/IntStream",
        "spliterator",
        "()Ljava/util/Spliterator$OfInt;",
        p_int_stream_spliterator,
    );
    r.register(
        "java/util/stream/LongStream",
        "spliterator",
        "()Ljava/util/Spliterator$OfLong;",
        p_long_stream_spliterator,
    );
    r.register(
        "java/util/stream/DoubleStream",
        "spliterator",
        "()Ljava/util/Spliterator$OfDouble;",
        p_double_stream_spliterator,
    );
    r.register(
        "java/util/stream/Stream",
        "spliterator",
        "()Ljava/util/Spliterator;",
        p_obj_stream_spliterator,
    );
    r.register(
        "java/util/stream/IntStream",
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfInt;",
        p_int_stream_iterator,
    );
    r.register(
        "java/util/stream/LongStream",
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfLong;",
        p_long_stream_iterator,
    );
    r.register(
        "java/util/stream/DoubleStream",
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfDouble;",
        p_double_stream_iterator,
    );
    r.set_category(__prev);
}

pub(crate) fn p59_stream_from_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Convert Spliterator to Stream by collecting remaining elements
    let spl = obj_arg(args, 0)?;
    let arr = match ctx.get_field(spl, 0) {
        Value::Object(Some(a)) => a,
        _ => {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            // Pin across the stream alloc below — a moving young GC there
            // would relocate the fresh array (native stale-local family).
            let empty_pin = ctx.pin_native_root(empty);
            let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
            let empty = ctx.read_native_pin(empty_pin, empty);
            ctx.set_field(stream, 0, Value::Object(Some(empty)));
            ctx.unpin_native_roots(empty_pin);
            return Ok(Some(Value::Object(Some(stream))));
        }
    };
    // Pin across the stream alloc below — a moving young GC there would
    // relocate the element array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

/// `{Int,Long,Double}Stream.spliterator()` / `Stream.spliterator()` on a
/// SYNTHETIC stream object (one stamped with the bare `java/util/stream/*Stream`
/// interface class, slot 0 = element array). Real JDK stream code calls
/// `spliterator()` on these — e.g. `IntStream.concat(a,b)` does `a.spliterator()`,
/// and JUnit's `JupiterTestDescriptor.getLegacyReportingIndexes` (run for EVERY
/// dynamic/parameterized test via `TestIdentifier.from`) builds exactly such a
/// concat. Since the receiver is the bare interface there is no concrete
/// `spliterator()` override, so dispatch fell to the abstract interface method
/// → `AbstractMethodError: IntStream.spliterator()...OfInt has no Code attribute`
/// → swallowed by the launcher → the dynamic test never registered (whole
/// parameterized classes reported EMPTY/found=0). Build a real primitive array
/// from the synthetic elements and delegate to the real
/// `java.util.Spliterators.spliterator(...)`, returning a genuine
/// `Spliterator.OfInt/OfLong/OfDouble`/`Spliterator` the JDK machinery consumes.
pub(crate) fn p_int_stream_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let n = elems.len();
    // Pin any boxed elements across the array alloc below — a moving young GC
    // there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, n);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let iv = match read_pinned_object_value(ctx, *p, *v) {
            Value::Int(x) => Value::Int(x),
            Value::Object(Some(o)) => ctx.get_field(o, 0),
            _ => Value::Int(0),
        };
        ctx.set_array_element(arr, i, iv);
    }
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    ctx.invoke(
        "java/util/Spliterators",
        "spliterator",
        "([IIII)Ljava/util/Spliterator$OfInt;",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(n as i32),
            Value::Int(0),
        ],
    )
}

pub(crate) fn p_long_stream_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let n = elems.len();
    // Pin any boxed elements across the array alloc below — a moving young GC
    // there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, n);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let lv = match read_pinned_object_value(ctx, *p, *v) {
            Value::Long(x) => Value::Long(x),
            Value::Object(Some(o)) => ctx.get_field(o, 0),
            _ => Value::Long(0),
        };
        ctx.set_array_element(arr, i, lv);
    }
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    ctx.invoke(
        "java/util/Spliterators",
        "spliterator",
        "([JIII)Ljava/util/Spliterator$OfLong;",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(n as i32),
            Value::Int(0),
        ],
    )
}

pub(crate) fn p_double_stream_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let n = elems.len();
    // Pin any boxed elements across the array alloc below — a moving young GC
    // there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Double, n);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let dv = match read_pinned_object_value(ctx, *p, *v) {
            Value::Double(x) => Value::Double(x),
            Value::Object(Some(o)) => ctx.get_field(o, 0),
            _ => Value::Double(0.0),
        };
        ctx.set_array_element(arr, i, dv);
    }
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    ctx.invoke(
        "java/util/Spliterators",
        "spliterator",
        "([DIII)Ljava/util/Spliterator$OfDouble;",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(n as i32),
            Value::Int(0),
        ],
    )
}

pub(crate) fn p_obj_stream_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let n = elems.len();
    // Pin the object elements across the array alloc below — a moving young
    // GC there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, n);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let v = read_pinned_object_value(ctx, *p, *v);
        ctx.set_array_element(arr, i, v);
    }
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    ctx.invoke(
        "java/util/Spliterators",
        "spliterator",
        "([Ljava/lang/Object;III)Ljava/util/Spliterator;",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(n as i32),
            Value::Int(0),
        ],
    )
}

/// `{Int,Long,Double}Stream.iterator()` on a SYNTHETIC stream object (slot 0 =
/// element array). Same "no Code attribute" family as `spliterator()` above —
/// the receiver is stamped with the bare `java/util/stream/*Stream` interface,
/// so `iterator()` (declared on `BaseStream`, no override) dispatches to the
/// abstract interface method → AbstractMethodError. Build a real primitive
/// array and delegate to `java.util.Arrays.stream(...)`, which returns a
/// genuine, bytecode-backed JDK stream implementation; calling `iterator()` on
/// THAT object is a normal virtual/interface dispatch that resolves to the
/// real JDK's own Code-attributed method (not this native), so there is no
/// re-entrancy risk.
pub(crate) fn p_int_stream_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let n = elems.len();
    // Pin any boxed elements across the array alloc below — a moving young GC
    // there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, n);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let iv = match read_pinned_object_value(ctx, *p, *v) {
            Value::Int(x) => Value::Int(x),
            Value::Object(Some(o)) => ctx.get_field(o, 0),
            _ => Value::Int(0),
        };
        ctx.set_array_element(arr, i, iv);
    }
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    let real_stream = ctx.invoke(
        "java/util/Arrays",
        "stream",
        "([I)Ljava/util/stream/IntStream;",
        &[Value::Object(Some(arr))],
    )?;
    let Some(real_stream) = real_stream else {
        return Ok(None);
    };
    ctx.invoke(
        "java/util/stream/IntStream",
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfInt;",
        &[real_stream],
    )
}

pub(crate) fn p_long_stream_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let n = elems.len();
    // Pin any boxed elements across the array alloc below — a moving young GC
    // there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, n);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let lv = match read_pinned_object_value(ctx, *p, *v) {
            Value::Long(x) => Value::Long(x),
            Value::Object(Some(o)) => ctx.get_field(o, 0),
            _ => Value::Long(0),
        };
        ctx.set_array_element(arr, i, lv);
    }
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    let real_stream = ctx.invoke(
        "java/util/Arrays",
        "stream",
        "([J)Ljava/util/stream/LongStream;",
        &[Value::Object(Some(arr))],
    )?;
    let Some(real_stream) = real_stream else {
        return Ok(None);
    };
    ctx.invoke(
        "java/util/stream/LongStream",
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfLong;",
        &[real_stream],
    )
}

pub(crate) fn p_double_stream_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    let n = elems.len();
    // Pin any boxed elements across the array alloc below — a moving young GC
    // there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Double, n);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let dv = match read_pinned_object_value(ctx, *p, *v) {
            Value::Double(x) => Value::Double(x),
            Value::Object(Some(o)) => ctx.get_field(o, 0),
            _ => Value::Double(0.0),
        };
        ctx.set_array_element(arr, i, dv);
    }
    if let Some(h) = first_pin {
        ctx.unpin_native_roots(h);
    }
    let real_stream = ctx.invoke(
        "java/util/Arrays",
        "stream",
        "([D)Ljava/util/stream/DoubleStream;",
        &[Value::Object(Some(arr))],
    )?;
    let Some(real_stream) = real_stream else {
        return Ok(None);
    };
    ctx.invoke(
        "java/util/stream/DoubleStream",
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfDouble;",
        &[real_stream],
    )
}

pub(crate) fn p59_int_stream_from_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let spl = obj_arg(args, 0)?;
    let arr = match ctx.get_field(spl, 0) {
        Value::Object(Some(a)) => a,
        _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
    };
    // Pin across the stream alloc below — a moving young GC there would
    // relocate the element array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 1);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

pub(crate) fn p59_long_stream_from_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let spl = obj_arg(args, 0)?;
    let arr = match ctx.get_field(spl, 0) {
        Value::Object(Some(a)) => a,
        _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
    };
    // Pin across the stream alloc below — a moving young GC there would
    // relocate the element array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/LongStream", 1);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

pub(crate) fn p59_double_stream_from_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let spl = obj_arg(args, 0)?;
    let arr = match ctx.get_field(spl, 0) {
        Value::Object(Some(a)) => a,
        _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
    };
    // Pin across the stream alloc below — a moving young GC there would
    // relocate the element array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/DoubleStream", 1);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

pub(crate) fn p59_collection_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Create a Spliterator from the collection's backing array (ArrayList field 0)
    let this = obj_arg(args, 0)?;
    let data = match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => arr,
        _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
    };
    // Pin across the Spliterator alloc below — a moving young GC there would
    // relocate the backing array (native stale-local family).
    let data_pin = ctx.pin_native_root(data);
    // 3-field layout (elements=0, pos=1, fence=2) — see the `tryAdvance`/
    // `estimateSize`/`characteristics`/`forEachRemaining` natives below,
    // which all read field 2 as the exclusive upper bound. This previously
    // allocated only 2 fields and never wrote `fence`, so every consumer
    // read an uninitialized slot 2 — a genuine hang/wrong-size-stream
    // hazard traced back to this via a native-call-hang watchdog dump
    // during `TestContextAotGeneratorIntegrationTests` (AccessControl
    // .lowest -> Arrays.stream -> Arrays.asList(arr).stream() ->
    // Collection.stream() default method -> this native).
    let len = ctx.array_length(data) as i32;
    let spl = alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3);
    let data = ctx.read_native_pin(data_pin, data);
    ctx.set_field(spl, 0, Value::Object(Some(data)));
    ctx.set_field(spl, 1, Value::Int(0)); // cursor at start
    ctx.set_field(spl, 2, Value::Int(len)); // fence = backing array length
    ctx.unpin_native_roots(data_pin);
    Ok(Some(Value::Object(Some(spl))))
}

/// `HashSet.spliterator()` — bypass the JDK bytecode (which would build a
/// `HashMap.KeySpliterator(this.map, ...)` and then read `m.table` from
/// the synthetic backing HashMap, hitting the `int(16)` layout-mismatch).
/// Snapshot the keys via the synthetic HashMap layout and hand back a
/// synthetic `(data, cursor)` Spliterator that the rest of the
/// `Spliterator.*` and `StreamSupport.stream(...)` natives understand.
pub(crate) fn p59_hashset_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Walk the synthetic HashSet to collect keys. Two layouts in use:
    //   * Wrapped-HashMap layout: slot 0 = HashMap (`HS_FIELD_MAP`), the
    //     HashMap holds buckets at slot 0 and each Node has key at slot 0,
    //     next at slot 3.
    //   * Legacy 2-field layout: slot 0 = Object[] of keys directly (used by
    //     some older synthetic builders before the HashMap wrap).
    let mut keys: Vec<Value> = Vec::new();
    let slot0 = ctx.get_field(this, 0);
    if let Value::Object(Some(inner)) = slot0 {
        // Disambiguate by looking at slot 0 of the inner object. If it's an
        // array we treat the inner as a HashMap (buckets array). Otherwise
        // we treat the inner array as the legacy data array.
        let inner_slot0 = ctx.get_field(inner, 0);
        if let Value::Object(Some(buckets)) = inner_slot0 {
            // Wrapped-HashMap path.
            let n = ctx.array_length(buckets);
            for i in 0..n {
                let mut node = ctx.get_array_element(buckets, i);
                while let Value::Object(Some(node_ref)) = node {
                    keys.push(ctx.get_field(node_ref, 0));
                    node = ctx.get_field(node_ref, 3);
                }
            }
        } else {
            // Legacy direct-array path: `inner` itself is the Object[] data.
            let n = ctx.array_length(inner);
            for i in 0..n {
                keys.push(ctx.get_array_element(inner, i));
            }
        }
    }
    // Pin the collected keys across the array/Spliterator allocs below — a
    // moving young GC there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &keys);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, keys.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, (k, p)) in keys.iter().zip(&pins).enumerate() {
        let k = read_pinned_object_value(ctx, *p, *k);
        ctx.set_array_element(arr, i, k);
    }
    // 3-field layout (elements=0, pos=1, fence=2) — see the companion fix
    // in `p59_collection_spliterator` above for why fence must be set.
    let fence = keys.len() as i32;
    let spl = alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(fence));
    ctx.unpin_native_roots(first_pin.unwrap_or(arr_pin));
    Ok(Some(Value::Object(Some(spl))))
}

// =============================================================================
// Stream modern methods — Java 16+
// Stream.toList(), Stream.mapMulti(), Stream.toArray(IntFunction)
// =============================================================================

pub(crate) fn register_p64_stream_modern(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let s = "java/util/stream/Stream";
    // Stream.toList() — Java 16: returns unmodifiable list
    r.register(s, "toList", "()Ljava/util/List;", native_p64_stream_to_list);

    // Stream.mapMulti — simplified as flatMap-like
    r.register(
        s,
        "mapMulti",
        "(Ljava/util/function/BiConsumer;)Ljava/util/stream/Stream;",
        |_ctx, args| {
            // Simplified: return self (real impl would expand via consumer)
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );

    // IntStream.toList doesn't exist, but boxed() -> toList works
    // Register Stream.of(T...) convenience
    r.register(
        s,
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let val = args.first().copied().unwrap_or(Value::Object(None));
            let count = if matches!(val, Value::Object(None)) {
                0
            } else {
                1
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count);
            if count > 0 {
                ctx.set_array_element(arr, 0, val);
            }
            let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );

    // Stream.concat
    r.register(
        s,
        "concat",
        "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;",
        |ctx, args| {
            // Get elements from both streams
            let (a_arr, a_len) =
                p64_stream_elements(ctx, args.first().copied().unwrap_or(Value::Object(None)));
            let (b_arr, b_len) =
                p64_stream_elements(ctx, args.get(1).copied().unwrap_or(Value::Object(None)));
            let total = a_len + b_len;
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, total);
            for i in 0..a_len {
                if let Some(a) = a_arr {
                    ctx.set_array_element(new_arr, i, ctx.get_array_element(a, i));
                }
            }
            for i in 0..b_len {
                if let Some(b) = b_arr {
                    ctx.set_array_element(new_arr, a_len + i, ctx.get_array_element(b, i));
                }
            }
            let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
            ctx.set_field(stream, 0, Value::Object(Some(new_arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );

    // dropWhile / takeWhile already registered in Phase 56 — do not override
    r.set_category(__prev_cat);
}

pub(crate) fn p64_stream_elements(
    ctx: &dyn NativeContext,
    stream: Value,
) -> (Option<ObjectRef>, usize) {
    match stream {
        Value::Object(Some(s)) => match ctx.get_field(s, 0) {
            Value::Object(Some(arr)) => {
                let len = ctx.array_length(arr);
                (Some(arr), len)
            }
            _ => (None, 0),
        },
        _ => (None, 0),
    }
}

pub(crate) fn native_p64_stream_to_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => {
            // Empty list
            let al = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            ctx.set_field(al, 0, Value::Object(None));
            ctx.set_field(al, 1, Value::Int(0));
            return Ok(Some(Value::Object(Some(al))));
        }
    };
    let len = ctx.array_length(arr);
    let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len);
    for i in 0..len {
        ctx.set_array_element(new_arr, i, ctx.get_array_element(arr, i));
    }
    let al = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
    ctx.set_field(al, 0, Value::Object(Some(new_arr)));
    ctx.set_field(al, 1, Value::Int(len as i32));
    Ok(Some(Value::Object(Some(al))))
}

// =============================================================================
// Collectors.teeing — Java 12
// =============================================================================

pub(crate) fn register_p64_collectors_teeing(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Collectors.teeing(Collector, Collector, BiFunction) -> Collector
    // ARG1 = downstream1, ARG2 = downstream2, ARG3 = the merge BiFunction.
    //
    // This used to write tag 1 (toList) with null args — the comment claimed
    // "Tag 9 … ARG1/ARG2 downstreams" but the body kept none of the three
    // arguments, so `collect(teeing(a, b, merge))` silently returned a List of
    // the stream elements instead of `merge.apply(a-result, b-result)`.
    // Five fields, not three: the merger lives in ARG3 and reading field 3 off a
    // 3-field object is out of bounds.
    r.register("java/util/stream/Collectors", "teeing",
        "(Ljava/util/stream/Collector;Ljava/util/stream/Collector;Ljava/util/function/BiFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let down1 = args.first().copied().unwrap_or(Value::Object(None));
            let down2 = args.get(1).copied().unwrap_or(Value::Object(None));
            let merger = args.get(2).copied().unwrap_or(Value::Object(None));
            // Pin across the Collector alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let d1_pin = pinned_object_value(ctx, down1);
            let d2_pin = pinned_object_value(ctx, down2);
            let merger_pin = pinned_object_value(ctx, merger);
            let c = alloc_concurrent_synthetic(ctx, "java/util/stream/Collector", 5);
            ctx.set_field(c, 0, Value::Int(P64_COLLECTOR_TEEING));
            ctx.set_field(c, 1, read_pinned_object_value(ctx, d1_pin, down1));
            ctx.set_field(c, 2, read_pinned_object_value(ctx, d2_pin, down2));
            ctx.set_field(c, 3, read_pinned_object_value(ctx, merger_pin, merger));
            ctx.set_field(c, 4, Value::Object(None));
            if let Some((h, _)) = d1_pin.or(d2_pin).or(merger_pin) {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(c))))
        });

    // flatMapping and filtering already registered in Phase 56 — do not override
    r.set_category(__prev_cat);
}

// =============================================================================
// java.util.stream.Gatherer — Java 22 (preview → final Java 24)
// Stub for the Gatherer API
// =============================================================================

pub(crate) fn register_p67_gatherer(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let g = "java/util/stream/Gatherer";
    // Gatherer.of(integrator) → Gatherer
    r.register(
        g,
        "of",
        "(Ljava/util/stream/Gatherer$Integrator;)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            // 3-field: initializer=0, integrator=1, finisher=2
            let obj = alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 3);
            ctx.set_field(obj, 0, Value::Object(None));
            ctx.set_field(obj, 1, args.first().copied().unwrap_or(Value::Object(None)));
            ctx.set_field(obj, 2, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(g, "ofSequential", "(Ljava/util/function/Supplier;Ljava/util/stream/Gatherer$Integrator;)Ljava/util/stream/Gatherer;", |ctx, args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 3);
        ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
        ctx.set_field(obj, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
        ctx.set_field(obj, 2, Value::Object(None));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(g, "ofSequential", "(Ljava/util/function/Supplier;Ljava/util/stream/Gatherer$Integrator;Ljava/util/function/BiConsumer;)Ljava/util/stream/Gatherer;", |ctx, args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 3);
        ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
        ctx.set_field(obj, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
        ctx.set_field(obj, 2, args.get(2).copied().unwrap_or(Value::Object(None)));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        g,
        "integrator",
        "()Ljava/util/stream/Gatherer$Integrator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(
        g,
        "initializer",
        "()Ljava/util/function/Supplier;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        g,
        "finisher",
        "()Ljava/util/function/BiConsumer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    // Gatherer.defaultInitializer / defaultFinisher
    //
    // KEEP — but the wave-3 one-liner ("null IS the Gatherer sentinel") is only
    // half true, and the accurate version is what makes this safe. In the REAL
    // JDK these are not null: `Gatherer.defaultInitializer()` is
    // `Gatherers.Value.DEFAULT.initializer()`, and its javadoc pins it to
    // "always returns the same instance" because the value is used as an
    // IDENTITY sentinel — "Gatherers whose initializer is `defaultInitializer()`
    // are considered to be stateless, and invoking their initializer is
    // optional" (`java.base/java/util/stream/Gatherer.java`, @implSpec).
    //
    // That identity test is the only spec-defined observation, and this model
    // passes it: `Gatherer.of(..)` above stores null in slots 0 and 2, and
    // `initializer()`/`finisher()` hand those same slots straight back, so
    // `g.initializer() == Gatherer.defaultInitializer()` compares null with
    // null and answers `true` exactly where the real JDK would. The gather
    // engine (`pd_gather_fold` / `pd_gather_scan` / `pd_gather_custom` in
    // lib.rs) reads the same sentinel by branching on `Value::Object(Some(_))`.
    // Manufacturing a synthetic Supplier / BiConsumer here would flip that
    // identity test to `false` for every default gatherer AND hand the engine a
    // value it then has to call.
    //
    // One reachability caveat worth leaving in writing: these are STATIC
    // interface methods, and static interface methods DO keep the native check
    // (only interface *instance* methods are dropped — see the carve-out in
    // `try_stackless_invoke` step 6). So unlike the instance methods around
    // them they would intercept in real-JDK mode and hand real `Gatherers` a
    // null where it requires `Gatherers.Value.DEFAULT`. They are safe only
    // because this whole registrar is synthetic-only; do not move
    // `register_phase56_stream_extras` onto the real-JDK path with these in it.
    r.register(
        g,
        "defaultInitializer",
        "()Ljava/util/function/Supplier;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        g,
        "defaultFinisher",
        "()Ljava/util/function/BiConsumer;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // Gatherers utility class (Java 22)
    let gs = "java/util/stream/Gatherers";
    // Gatherers.fold(initial, folder)
    r.register(
        gs,
        "fold",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiFunction;)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 3);
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
            ctx.set_field(obj, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(obj, 2, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // Gatherers.scan(initial, scanner)
    r.register(
        gs,
        "scan",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiFunction;)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 3);
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
            ctx.set_field(obj, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(obj, 2, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // Gatherers.windowFixed(size) → Gatherer
    r.register(
        gs,
        "windowFixed",
        "(I)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 3);
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Int(1)));
            ctx.set_field(obj, 1, Value::Object(None));
            ctx.set_field(obj, 2, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // Gatherers.windowSliding(size) → Gatherer
    r.register(
        gs,
        "windowSliding",
        "(I)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 3);
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Int(1)));
            ctx.set_field(obj, 1, Value::Object(None));
            ctx.set_field(obj, 2, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // Stream.gather(Gatherer) — add to Stream
    r.register(
        "java/util/stream/Stream",
        "gather",
        "(Ljava/util/stream/Gatherer;)Ljava/util/stream/Stream;",
        |_ctx, args| {
            // Simplified: return a new empty stream (real impl would transform elements)
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.util.Spliterator and StreamSupport
// =============================================================================

pub(crate) fn register_p69_spliterator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Spliterator = 3-field (elements=0 Object[], pos=1 Int, fence=2 Int)
    let sp = "java/util/Spliterator";
    r.register(
        sp,
        "tryAdvance",
        "(Ljava/util/function/Consumer;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let fence = match ctx.get_field(this, 2) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            if pos >= fence {
                return Ok(Some(Value::Int(0)));
            }
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = ctx.get_array_element(arr, pos);
            ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
            // Invoke consumer.accept(elem)
            if let Some(Value::Object(Some(consumer))) = args.get(1) {
                // `invoke_virtual` takes the receiver SEPARATELY from `args`
                // (see every other callback in this file, e.g. `&[v]` at ~489 /
                // ~584). Passing the consumer again as args[0] made
                // `Consumer.accept` receive the CONSUMER instead of the element.
                let _ = ctx.invoke_virtual(*consumer, "accept", "(Ljava/lang/Object;)V", &[elem]);
            }
            Ok(Some(Value::Int(1)))
        },
    );
    // `trySplit()` returning null is the spec'd answer for a spliterator that
    // "cannot be split", so it is legal rather than an error path, and every
    // caller has to handle it (`Spliterators`, `AbstractTask`).
    //
    // But wave 4 asked the sharper question — COULD it split? — and the answer
    // is yes: this receiver is exactly the JDK's `ArraySpliterator` shape
    // (field 0 = Object[], 1 = cursor, 2 = fence), whose real `trySplit` is the
    // four-line `mid = (lo + fence) >>> 1; return lo >= mid ? null : new
    // ArraySpliterator(array, lo, index = mid, chars)`. So this null is a
    // genuine under-implementation, NOT a truthful "cannot split".
    //
    // It is deliberately not fixed here, because fixing it here would change
    // nothing: this registration is SHADOWED. `native-collections/src/lib.rs`
    // (`register_iterator_protocol_natives`) registers the identical triple
    // `java/util/Spliterator.trySplit()Ljava/util/Spliterator;` ->
    // `native_return_null_obj`, and `vm/src/vm/vm_init.rs` calls
    // `register_collections_natives` AFTER `register_builtins` — so
    // last-registration-wins hands the call to native-collections, not to this
    // line. The fix belongs there (and `vm/src/vm.rs::spliterator_basics_p69`
    // asserts the null, so it has to move with it). Reported as an escalation.
    // Whoever implements it: keep native-collections'
    // `heap_kind_of(a) == ObjectKind::Array` guard on field 0 — dispatch can
    // route a real-JDK Spliterator subclass into these natives, and its field 0
    // is not an array.
    r.register(
        sp,
        "trySplit",
        "()Ljava/util/Spliterator;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(sp, "estimateSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, 1) {
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let fence = match ctx.get_field(this, 2) {
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Long((fence - pos).max(0))))
    });
    // KEEP: this describes the ONE spliterator shape these natives implement —
    // a cursor (pos=1, fence=2) over an Object[] (elements=0). That is ORDERED
    // (array index order) and SIZED (`estimateSize()` is exact). It is not
    // DISTINCT/SORTED/NONNULL/IMMUTABLE/CONCURRENT, and not SUBSIZED because
    // `trySplit()` never produces children. `hasCharacteristics(c)` is derived
    // from this value rather than hardcoded.
    r.register(sp, "characteristics", "()I", |_ctx, _args| {
        // ORDERED | SIZED
        Ok(Some(Value::Int(0x10 | 0x40)))
    });
    r.register(sp, "getExactSizeIfKnown", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, 1) {
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let fence = match ctx.get_field(this, 2) {
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Long((fence - pos).max(0))))
    });
    r.register(
        sp,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(None),
            };
            let mut pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let fence = match ctx.get_field(this, 2) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let consumer = match args.get(1) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(None),
            };
            // Pin across the consumer callbacks below — a moving young GC
            // there would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let arr_pin = ctx.pin_native_root(arr);
            let consumer_pin = ctx.pin_native_root(consumer);
            while pos < fence {
                let arr = ctx.read_native_pin(arr_pin, arr);
                let consumer = ctx.read_native_pin(consumer_pin, consumer);
                let elem = ctx.get_array_element(arr, pos);
                // Receiver is passed separately — see the same fix in
                // `tryAdvance` above.
                let _ = ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem]);
                pos += 1;
            }
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, 1, Value::Int(fence as i32));
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        },
    );

    // Spliterator constants — KEEP: `public static final int` fields of
    // java.util.Spliterator surfaced as descriptor-"I" natives; the constant
    // IS the value. (Duplicate of the p59 block, same values; the later
    // registration simply overwrites the identical earlier one.)
    r.register(sp, "ORDERED", "I", |_ctx, _args| Ok(Some(Value::Int(0x10))));
    r.register(sp, "DISTINCT", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x01)))
    });
    r.register(sp, "SORTED", "I", |_ctx, _args| Ok(Some(Value::Int(0x04))));
    r.register(sp, "SIZED", "I", |_ctx, _args| Ok(Some(Value::Int(0x40))));
    r.register(sp, "NONNULL", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x100)))
    });
    r.register(sp, "IMMUTABLE", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x400)))
    });
    r.register(sp, "CONCURRENT", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x1000)))
    });
    r.register(sp, "SUBSIZED", "I", |_ctx, _args| {
        Ok(Some(Value::Int(0x4000)))
    });

    // Spliterators utility class
    let sps = "java/util/Spliterators";
    r.register(
        sps,
        "spliterator",
        "([Ljava/lang/Object;I)Ljava/util/Spliterator;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let obj = alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3);
            ctx.set_field(obj, 0, Value::Object(Some(arr)));
            ctx.set_field(obj, 1, Value::Int(0));
            ctx.set_field(obj, 2, Value::Int(len as i32));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        sps,
        "emptySpliterator",
        "()Ljava/util/Spliterator;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let obj = alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3);
            ctx.set_field(obj, 0, Value::Object(Some(arr)));
            ctx.set_field(obj, 1, Value::Int(0));
            ctx.set_field(obj, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        sps,
        "spliteratorUnknownSize",
        "(Ljava/util/Iterator;I)Ljava/util/Spliterator;",
        |ctx, args| {
            // The previous implementation always returned an empty
            // spliterator, ignoring the supplied Iterator. That broke the
            // entire `Iterable.spliterator()` default-method chain — the
            // JDK's default `Iterable.spliterator()` calls
            // `Spliterators.spliteratorUnknownSize(iterator(), 0)`, so any
            // caller that drove `ServiceLoader` (or any other Iterable) via
            // `spliterator()` saw zero elements regardless of what
            // `iterator()` returned. Elasticsearch's CliToolProvider lookup
            // is the canonical example: `ServiceLoader.load(...).spliterator()
            // .stream().filter(name=="server")` returned `[]` despite our
            // `ServiceLoader.iterator()` producing 13 providers.
            //
            // Fix: drain the iterator eagerly into an Object[] and stuff it
            // into the synthetic 3-field spliterator that `Spliterator.*`
            // natives already understand. Bounded by a generous safety cap
            // to avoid runaway iteration on a pathological infinite source.
            let iter = match args.first() {
                Some(Value::Object(Some(it))) => *it,
                _ => {
                    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    let obj = alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3);
                    ctx.set_field(obj, 0, Value::Object(Some(empty)));
                    ctx.set_field(obj, 1, Value::Int(0));
                    ctx.set_field(obj, 2, Value::Int(0));
                    return Ok(Some(Value::Object(Some(obj))));
                }
            };
            let mut collected: Vec<Value> = Vec::new();
            const SAFETY_CAP: usize = 1_000_000;
            loop {
                let has_next = ctx.invoke_virtual(iter, "hasNext", "()Z", &[]);
                let proceed = matches!(has_next, Ok(Some(Value::Int(1))));
                if !proceed {
                    break;
                }
                let next = ctx.invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[]);
                let val = match next {
                    Ok(Some(v)) => v,
                    _ => break,
                };
                collected.push(val);
                if collected.len() >= SAFETY_CAP {
                    break;
                }
            }
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, collected.len());
            for (i, v) in collected.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            let obj = alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3);
            ctx.set_field(obj, 0, Value::Object(Some(arr)));
            ctx.set_field(obj, 1, Value::Int(0));
            ctx.set_field(obj, 2, Value::Int(collected.len() as i32));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // StreamSupport
    let ss = "java/util/stream/StreamSupport";
    r.register(
        ss,
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        |ctx, args| {
            // Convert spliterator to stream (1-field synthetic with backing array).
            //
            // Two cases:
            //  * Synthetic spliterator (built by our `Collection.spliterator`,
            //    `Spliterators.spliterator`, etc.): field 0 is already the
            //    backing Object[] — just snapshot it.
            //  * Real-JDK Spliterator subclass (e.g. log4j's
            //    `ServiceLoaderUtil$ServiceLoaderSpliterator`, whose field 0
            //    is an `Iterator`, not an array). We can't read its private
            //    layout; drain it via `forEachRemaining(Consumer)` into a
            //    collector consumer whose `accept` natively appends to a
            //    growing Object[].
            let spliterator = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => {
                    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
                    ctx.set_field(stream, 0, Value::Object(Some(empty)));
                    return Ok(Some(Value::Object(Some(stream))));
                }
            };
            let field0 = ctx.get_field(spliterator, 0);
            if crate::nbflags().dbg_streamsupp {
                let cid = ctx.class_id_of_object(spliterator);
                let cn = ctx.class_name_of_id(cid).unwrap_or_default();
                let f0_kind = match field0 {
                    Value::Object(Some(a)) => format!("Object(kind={:?})", ctx.heap_kind_of(a)),
                    Value::Object(None) => "Object(null)".to_string(),
                    Value::Int(i) => format!("Int({})", i),
                    Value::Long(l) => format!("Long({})", l),
                    _ => "other".to_string(),
                };
                eprintln!(
                    "[STREAMSUPP-DBG] spliterator class={} field0={}",
                    cn, f0_kind
                );
            }
            let arr = match field0 {
                Value::Object(Some(a))
                    if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array =>
                {
                    a
                }
                _ => {
                    // Real Spliterator subclass — drain via a collecting
                    // consumer.
                    drain_spliterator(ctx, spliterator)?
                }
            };
            let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );
    r.register(
        ss,
        "intStream",
        "(Ljava/util/Spliterator$OfInt;Z)Ljava/util/stream/IntStream;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 1);
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );
    r.register(
        ss,
        "longStream",
        "(Ljava/util/Spliterator$OfLong;Z)Ljava/util/stream/LongStream;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/LongStream", 1);
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );
    r.register(
        ss,
        "doubleStream",
        "(Ljava/util/Spliterator$OfDouble;Z)Ljava/util/stream/DoubleStream;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/DoubleStream", 1);
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );

    // Collector consumer used by `drain_spliterator`. Layout:
    //   field 0: Object[] storage (capacity == array_length)
    //   field 1: Int — current logical length
    // `accept(Object)V` appends, growing the storage on demand.
    r.register(
        "cratonvm/internal/StreamCollector",
        "accept",
        "(Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            let mut len = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let storage = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16),
            };
            let cap = ctx.array_length(storage);
            let storage = if len >= cap {
                let new_cap = (cap * 2).max(16);
                let bigger = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                for i in 0..len {
                    let v = ctx.get_array_element(storage, i);
                    ctx.set_array_element(bigger, i, v);
                }
                ctx.set_field(this, 0, Value::Object(Some(bigger)));
                bigger
            } else {
                storage
            };
            ctx.set_array_element(storage, len, elem);
            len += 1;
            ctx.set_field(this, 1, Value::Int(len as i32));
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

/// Drain a (possibly real-JDK) Spliterator into a freshly-allocated
/// Object[] by repeatedly invoking `tryAdvance(Consumer)` with a synthetic
/// collector consumer. Used when `StreamSupport.stream` is handed a
/// Spliterator whose field-0 layout we don't control (e.g. log4j's
/// `ServiceLoaderUtil$ServiceLoaderSpliterator`).
pub(crate) fn drain_spliterator(
    ctx: &mut dyn NativeContext,
    spliterator: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // Allocate the collector consumer.
    let collector = alloc_concurrent_synthetic(ctx, "cratonvm/internal/StreamCollector", 2);
    let initial = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
    ctx.set_field(collector, 0, Value::Object(Some(initial)));
    ctx.set_field(collector, 1, Value::Int(0));

    // Drive the spliterator. Try forEachRemaining first (one virtual call),
    // fall back to tryAdvance loop if forEachRemaining isn't usable.
    // NOTE: `invoke_virtual` prepends the receiver itself — `args` must NOT
    // include it. The previous version double-passed the receiver, producing
    // a malformed 3-arg call for a 2-arg method, which silently dropped the
    // collector and left the output array empty (the symptom that surfaced
    // as the FORE-DBG trace and as `Stream.forEach` returning zero elements
    // during WildFly's log4j init).
    let _ = ctx.invoke_virtual(
        spliterator,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
        &[Value::Object(Some(collector))],
    );

    // Snapshot to an exactly-sized array.
    let len = match ctx.get_field(collector, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let storage = match ctx.get_field(collector, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0)),
    };
    let out = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len);
    for i in 0..len {
        let v = ctx.get_array_element(storage, i);
        ctx.set_array_element(out, i, v);
    }
    Ok(out)
}
