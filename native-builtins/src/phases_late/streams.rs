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
        let itr = try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2)?;
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
            let itr = try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2)?;
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
    // `forEachOrdered` and `summaryStatistics` moved to
    // `register_phase56_primitive_stream_terminals` (called at the end of this
    // function) so the real-JDK build can reach them without also inheriting
    // the intermediate ops here — see the note on that registrar.

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
    register_phase56_primitive_stream_terminals(r);
    r.set_category(__prev_cat);
}

/// The primitive-stream TERMINALS this crate owns: `summaryStatistics` and
/// `forEachOrdered` on `IntStream` / `LongStream` / `DoubleStream`.
///
/// Split out of `register_phase56_stream_extras` on 2026-08-11 so the real-JDK
/// build has something safe to call. The rest of this file is synthetic-JDK
/// only: its single caller is `lib.rs::register_synthetic_overrides`, which
/// `vm/src/native/builtins.rs` replaces with a no-op shim whenever the
/// `synthetic-jdk` feature is off — and it is off by default. That is why
/// `IntStream.rangeClosed(1,5).summaryStatistics()` killed the run under
/// `--real-jdk`: `native_int_stream_range_closed` (native-collections, live in
/// both modes) hands back a synthetic object whose class IS the interface
/// `java/util/stream/IntStream`, nothing live registers `summaryStatistics` on
/// that interface, and the interface's own declaration has no Code attribute.
/// native-collections already carries the same finding for `LongStream.mapToObj`
/// in its own registrar comment.
///
/// TERMINALS ONLY, and deliberately. The intermediate ops in
/// `register_phase56_stream_extras` build their result with `p56_build_stream`,
/// which allocates a REFERENCE array; native-collections' `make_int_stream`
/// allocates a primitive `Int`/`Long`/`Double` array and comments that a
/// reference array coerces `Value::Int` to null. Registering those into the
/// real-JDK path would hand the live readers a stream of nulls. The two
/// terminals here return a statistics object and void respectively, so they
/// never mint a stream.
///
/// ORDERING: safe to call from the real-JDK essentials path, which runs BEFORE
/// `register_collections_natives` and would therefore be overwritten by it.
/// Every triple below is one native-collections does NOT register (verified
/// against its `register_{int,long,double}_stream_natives`), so nothing here is
/// a re-registration. Adding a triple that native-collections also registers
/// makes this registrar silently inert — check before extending it.
pub(crate) fn register_phase56_primitive_stream_terminals(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);

    r.register(
        "java/util/stream/IntStream",
        "forEachOrdered",
        "(Ljava/util/function/IntConsumer;)V",
        p56_int_stream_for_each_ordered,
    );
    r.register(
        "java/util/stream/IntStream",
        "summaryStatistics",
        "()Ljava/util/IntSummaryStatistics;",
        p56_int_stream_summary_stats,
    );

    r.register(
        "java/util/stream/LongStream",
        "forEachOrdered",
        "(Ljava/util/function/LongConsumer;)V",
        p56_long_stream_for_each_ordered,
    );
    r.register(
        "java/util/stream/LongStream",
        "summaryStatistics",
        "()Ljava/util/LongSummaryStatistics;",
        p56_long_stream_summary_stats,
    );

    r.register(
        "java/util/stream/DoubleStream",
        "forEachOrdered",
        "(Ljava/util/function/DoubleConsumer;)V",
        p56_double_stream_for_each_ordered,
    );
    r.register(
        "java/util/stream/DoubleStream",
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
) -> Result<ObjectRef, MethodCallFailed> {
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
    let stream = try_alloc_concurrent_synthetic(ctx, class, 1)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.unpin_native_roots(first_pin.unwrap_or(arr_pin));
    Ok(stream)
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
    Ok(Some(Value::Object(Some(result?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(result?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
        let wrapper = try_alloc_concurrent_synthetic(ctx, "java/lang/Integer", 1)?;
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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

// --- LongStream.forEachOrdered ---
// Added 2026-08-11 with the W7-2 sweep. `forEachOrdered` was registered for
// IntStream and for the reference Stream but for neither of the other two
// primitive streams — the same "covered for whichever members a probe reached"
// shape as `summaryStatistics` itself. Ordered and unordered traversal are the
// same traversal for a sequential synthetic stream, so this is `forEach`.
pub(crate) fn p56_long_stream_for_each_ordered(
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
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(J)V", &[v]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(consumer_pin);
    Ok(None)
}

// --- DoubleStream.forEachOrdered --- see the LongStream note above.
pub(crate) fn p56_double_stream_for_each_ordered(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);
    let consumer_pin = ctx.pin_native_root(consumer);
    for v in elems {
        let c = ctx.read_native_pin(consumer_pin, consumer);
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(D)V", &[v]) {
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
    Ok(Some(Value::Object(Some(result?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
        let wrapper = try_alloc_concurrent_synthetic(ctx, "java/lang/Long", 1)?;
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(result?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
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
        let wrapper = try_alloc_concurrent_synthetic(ctx, "java/lang/Double", 1)?;
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
    Ok(Some(Value::Object(Some(s?))))
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
    Ok(Some(Value::Object(Some(s?))))
}

// ---------------------------------------------------------------------------
// Summary Statistics: IntSummaryStatistics, LongSummaryStatistics, DoubleSummaryStatistics
// 4-field synthetic: (count=0 Long, sum=1 Long/Double, min=2, max=3)
//
// That 4-slot shape is the SYNTHETIC one, shared with native-collections'
// `summarizing*`/`averaging*` collectors, and it is only the whole truth for two
// of the three classes. `javap -p` against the JDK 25 image says:
//
//   java.util.IntSummaryStatistics     count, sum, min, max                    (4)
//   java.util.LongSummaryStatistics    count, sum, min, max                    (4)
//   java.util.DoubleSummaryStatistics  count, sum, sumCompensation, simpleSum,
//                                      min, max                                (6)
//
// so on a REAL `DoubleSummaryStatistics` receiver slots 2 and 3 are
// `sumCompensation` and `simpleSum`, not min and max. `try_alloc_concurrent_
// synthetic` clamps the slot count UP to the resolved class's real field count
// and keeps the REAL class id, so which of the two shapes a terminal is holding
// is decided at run time by whether the class file loaded — see
// `p56_double_stats_store` below, which is the only place that decides it.
// ---------------------------------------------------------------------------
pub(crate) const STATS_FIELD_COUNT: usize = 0;

pub(crate) const STATS_FIELD_SUM: usize = 1;

pub(crate) const STATS_FIELD_MIN: usize = 2;

pub(crate) const STATS_FIELD_MAX: usize = 3;

/// Real `java.util.DoubleSummaryStatistics` field order (javap -p, JDK 25).
/// Only reachable when the real class file loaded; see `p56_double_stats_store`.
const REAL_DSS_FIELD_COUNT: usize = 0;
const REAL_DSS_FIELD_SUM: usize = 1;
const REAL_DSS_FIELD_SUM_COMPENSATION: usize = 2;
const REAL_DSS_FIELD_SIMPLE_SUM: usize = 3;
const REAL_DSS_FIELD_MIN: usize = 4;
const REAL_DSS_FIELD_MAX: usize = 5;

/// `java.lang.Math.min(double,double)`, which is NOT Rust's `f64::min`.
///
/// Java propagates NaN and orders `-0.0` below `+0.0`; Rust's `f64::min` is IEEE
/// `minNum`, which RETURNS THE NON-NaN OPERAND. Measured on HotSpot 25,
/// `DoubleStream.of(1.0, Double.NaN, 3.0).summaryStatistics()` prints
/// `count=3, sum=NaN, min=NaN, average=NaN, max=NaN` — every field NaN, because
/// `accept` folds with `Math.min`/`Math.max`. Folding with `f64::min` instead
/// answers `min=1.0, max=3.0` beside a NaN sum: three fields that cannot all
/// have come from the same data, which is the shape of a wrong answer that
/// reads as a right one.
fn p56_java_math_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        // `Math.min(-0.0, 0.0)` is `-0.0`; `==` cannot tell them apart.
        return if a.is_sign_negative() { a } else { b };
    }
    if a <= b {
        a
    } else {
        b
    }
}

/// `java.lang.Math.max(double,double)` — the mirror of [`p56_java_math_min`].
fn p56_java_math_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { b } else { a };
    }
    if a >= b {
        a
    } else {
        b
    }
}

/// Render a double the way `String.format("%f", d)` does, which is what every
/// `*SummaryStatistics.toString()` in the JDK uses for its `average` (and, for
/// the double flavour, for `sum`/`min`/`max` too).
///
/// Rust's `{}` prints `3` where Java prints `3.000000`, and `inf` where Java
/// prints `Infinity` — both of which an empty `DoubleSummaryStatistics` hits on
/// its very first line (`min=Infinity, max=-Infinity`, measured on HotSpot 25).
///
/// KNOWN GAP, stated rather than hidden: the JDK's `%f` is locale-sensitive
/// (the same call prints `3,000000` under a comma-decimal default locale) and
/// rounds HALF_UP where Rust's `{:.6}` rounds half-to-even. This renders the
/// C/en form unconditionally.
fn p56_format_java_f(d: f64) -> String {
    if d.is_nan() {
        "NaN".to_string()
    } else if d.is_infinite() {
        if d.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        }
    } else {
        format!("{:.6}", d)
    }
}

/// Read a `*SummaryStatistics` slot known to hold a `long`, defaulting to 0.
fn p56_stats_long(ctx: &dyn NativeContext, stats: ObjectRef, slot: usize) -> i64 {
    match ctx.get_field(stats, slot) {
        Value::Long(l) => l,
        _ => 0,
    }
}

/// Read a `*SummaryStatistics` slot known to hold an `int`. The default is the
/// caller's identity value (`Integer.MAX_VALUE` for min, `MIN_VALUE` for max),
/// never 0 — a 0 default is what made `IntStream.of(5, 7)` report `min=0` before
/// the identity-seeding ctor below existed.
fn p56_stats_int(ctx: &dyn NativeContext, stats: ObjectRef, slot: usize, identity: i32) -> i32 {
    match ctx.get_field(stats, slot) {
        Value::Int(i) => i,
        _ => identity,
    }
}

/// Read a `*SummaryStatistics` slot known to hold a `double`, defaulting to the
/// caller's identity value — see [`p56_stats_int`].
fn p56_stats_double(ctx: &dyn NativeContext, stats: ObjectRef, slot: usize, identity: f64) -> f64 {
    match ctx.get_field(stats, slot) {
        Value::Double(d) => d,
        _ => identity,
    }
}

/// The two argument checks every `*SummaryStatistics(count, min, max, sum)`
/// constructor performs, with the JDK's exact messages (measured on HotSpot 25:
/// `java.lang.IllegalArgumentException: Negative count value` and
/// `... : Minimum greater than maximum`).
///
/// The min/max check is conditional on `count > 0` — the JDK skips the whole
/// body for an empty statistics and leaves the identity field defaults, which is
/// why `new IntSummaryStatistics(0L, 9, 1, 0L)` constructs cleanly.
fn p56_stats_ctor_guard(count: i64, min_gt_max: bool) -> Result<(), MethodCallFailed> {
    if count < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Negative count value".to_string(),
        }
        .into());
    }
    if count > 0 && min_gt_max {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Minimum greater than maximum".to_string(),
        }
        .into());
    }
    Ok(())
}

pub(crate) fn p56_int_stream_summary_stats(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elems = p56_read_stream_elems(ctx, this);
    // COUNT WHAT WE FOLD. The previous spelling took `count` from `elems.len()`
    // but summed only the `Value::Int` elements, so any element of another
    // shape produced a statistics object whose count and sum disagreed —
    // `count=5, sum=0` reads as a real answer, not as the layout error it is.
    let mut count: i64 = 0;
    let mut sum: i64 = 0;
    let mut min = i32::MAX;
    let mut max = i32::MIN;
    for v in &elems {
        if let Value::Int(i) = v {
            count += 1;
            // The JDK's `sum` is a plain `long +=`, so it WRAPS rather than
            // saturating. Match it.
            sum = sum.wrapping_add(*i as i64);
            if *i < min {
                min = *i;
            }
            if *i > max {
                max = *i;
            }
        }
    }
    // Identity values for the empty stream: HotSpot 25 prints
    // `min=2147483647, max=-2147483648` for `IntStream.of().summaryStatistics()`
    // (measured), which is what the JDK's field initialisers leave behind.
    if count == 0 {
        min = i32::MAX;
        max = i32::MIN;
    }
    let stats = try_alloc_concurrent_synthetic(ctx, "java/util/IntSummaryStatistics", 4)?;
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
    // Count what we fold — see `p56_int_stream_summary_stats`.
    let mut count: i64 = 0;
    let mut sum: i64 = 0;
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for v in &elems {
        if let Value::Long(l) = v {
            count += 1;
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
    let stats = try_alloc_concurrent_synthetic(ctx, "java/util/LongSummaryStatistics", 4)?;
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
    // Count what we fold — see `p56_int_stream_summary_stats`.
    let mut count: i64 = 0;
    // COMPENSATED summation, byte-for-byte `DoubleSummaryStatistics.
    // sumWithCompensation` plus the `simpleSum` shadow its `getSum()` falls back
    // to. A naive `sum += d` is measurably not the same number:
    // `DoubleStream.of(1e16, 1.0, -1e16).summaryStatistics().getSum()` is `0.0`
    // on HotSpot 25 (measured) and `2.0` naively.
    let mut sum: f64 = 0.0;
    let mut compensation: f64 = 0.0;
    let mut simple_sum: f64 = 0.0;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for v in &elems {
        if let Value::Double(d) = v {
            count += 1;
            simple_sum += *d;
            let tmp = *d - compensation;
            let velvel = sum + tmp;
            compensation = (velvel - sum) - tmp;
            sum = velvel;
            // Java's Math.min/max, not Rust's — NaN must propagate into all
            // three of min/max/sum together. See `p56_java_math_min`.
            min = p56_java_math_min(min, *d);
            max = p56_java_math_max(max, *d);
        }
    }
    if count == 0 {
        min = f64::INFINITY;
        max = f64::NEG_INFINITY;
    }
    // `getSum()`'s own reconciliation: the compensated total, except that a
    // spurious NaN produced by accumulating same-signed infinities is answered
    // with the correctly-signed infinity `simpleSum` kept.
    let total = {
        let tmp = sum - compensation;
        if tmp.is_nan() && simple_sum.is_infinite() {
            simple_sum
        } else {
            tmp
        }
    };
    let stats = try_alloc_concurrent_synthetic(ctx, "java/util/DoubleSummaryStatistics", 4)?;
    p56_double_stats_store(ctx, stats, count, total, simple_sum, min, max);
    Ok(Some(Value::Object(Some(stats))))
}

/// Write a `DoubleSummaryStatistics` result into whichever of the two layouts
/// the receiver actually has.
///
/// This is the one place that decides it. The synthetic class this file's own
/// accessors serve declares four fields (count, sum, min, max); the REAL
/// `java.util.DoubleSummaryStatistics` declares six (count, sum,
/// sumCompensation, simpleSum, min, max — `javap -p`, JDK 25), and its `getMin`
/// / `getMax` read slots 4 and 5. Writing the 4-slot shape onto a real receiver
/// puts min into `sumCompensation` and max into `simpleSum`, after which the
/// real `getMin()`/`getMax()` answer 0.0 and the real `getSum()` answers
/// `sum - min` — three wrong numbers and no error anywhere.
///
/// `try_alloc_concurrent_synthetic` resolves the class by name and clamps the
/// slot count UP to the real one, so the discriminator is the allocated
/// object's own field count, not a compile-time mode flag.
fn p56_double_stats_store(
    ctx: &mut dyn NativeContext,
    stats: ObjectRef,
    count: i64,
    sum: f64,
    simple_sum: f64,
    min: f64,
    max: f64,
) {
    let class_id = ctx.class_id_of_object(stats);
    let fields = ctx.class_num_total_fields(class_id);
    if fields > REAL_DSS_FIELD_MAX {
        ctx.set_field(stats, REAL_DSS_FIELD_COUNT, Value::Long(count));
        // `sum` is already the reconciled total, so the compensation term the
        // real `getSum()` subtracts must be zero, not left at whatever the
        // allocation defaulted to.
        ctx.set_field(stats, REAL_DSS_FIELD_SUM, Value::Double(sum));
        ctx.set_field(stats, REAL_DSS_FIELD_SUM_COMPENSATION, Value::Double(0.0));
        ctx.set_field(stats, REAL_DSS_FIELD_SIMPLE_SUM, Value::Double(simple_sum));
        ctx.set_field(stats, REAL_DSS_FIELD_MIN, Value::Double(min));
        ctx.set_field(stats, REAL_DSS_FIELD_MAX, Value::Double(max));
    } else {
        ctx.set_field(stats, STATS_FIELD_COUNT, Value::Long(count));
        ctx.set_field(stats, STATS_FIELD_SUM, Value::Double(sum));
        ctx.set_field(stats, STATS_FIELD_MIN, Value::Double(min));
        ctx.set_field(stats, STATS_FIELD_MAX, Value::Double(max));
    }
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
        // The JDK's format string is
        // `"%s{count=%d, sum=%d, min=%d, average=%f, max=%d}"` — `average` is
        // `%f`, i.e. SIX decimal places. Rust's `{}` printed `average=3` where
        // HotSpot 25 prints `average=3.000000` (measured). See
        // `p56_format_java_f`.
        let avg = if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        };
        let s = format!(
            "IntSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}",
            count,
            sum,
            min,
            p56_format_java_f(avg),
            max
        );
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });
    // `combine(IntSummaryStatistics)` — declared by the class (javap, JDK 25)
    // and registered nowhere until 2026-08-11. It is not an exotic corner: it is
    // the third argument of `IntPipeline.summaryStatistics()`'s own
    // `collect(IntSummaryStatistics::new, ::accept, ::combine)`, and every
    // `Collectors.summarizingInt` merge goes through it.
    r.register(
        iss,
        "combine",
        "(Ljava/util/IntSummaryStatistics;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                // The JDK dereferences `other` unconditionally, so a null
                // argument is an NPE there, not a silent no-op.
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let (c, s, mn, mx) = (
                p56_stats_long(ctx, this, STATS_FIELD_COUNT),
                p56_stats_long(ctx, this, STATS_FIELD_SUM),
                p56_stats_int(ctx, this, STATS_FIELD_MIN, i32::MAX),
                p56_stats_int(ctx, this, STATS_FIELD_MAX, i32::MIN),
            );
            let (oc, os, omn, omx) = (
                p56_stats_long(ctx, other, STATS_FIELD_COUNT),
                p56_stats_long(ctx, other, STATS_FIELD_SUM),
                p56_stats_int(ctx, other, STATS_FIELD_MIN, i32::MAX),
                p56_stats_int(ctx, other, STATS_FIELD_MAX, i32::MIN),
            );
            ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(c.wrapping_add(oc)));
            ctx.set_field(this, STATS_FIELD_SUM, Value::Long(s.wrapping_add(os)));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Int(mn.min(omn)));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Int(mx.max(omx)));
            Ok(None)
        },
    );
    // `IntSummaryStatistics(long count, int min, int max, long sum)` — note the
    // argument order, which is NOT the field order, and the two documented
    // IllegalArgumentExceptions. With `count == 0` the JDK ignores min/max
    // entirely and leaves the identity defaults: `new IntSummaryStatistics(0L,
    // 9, 1, 0L)` prints `min=2147483647, max=-2147483648` on HotSpot 25
    // (measured) rather than throwing "Minimum greater than maximum".
    r.register(iss, "<init>", "(JIIJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match args.get(1) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        let min = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let max = match args.get(3) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let sum = match args.get(4) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        p56_stats_ctor_guard(count, min > max)?;
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(count.max(0)));
        if count > 0 {
            ctx.set_field(this, STATS_FIELD_SUM, Value::Long(sum));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Int(min));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Int(max));
        } else {
            ctx.set_field(this, STATS_FIELD_SUM, Value::Long(0));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Int(i32::MAX));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Int(i32::MIN));
        }
        Ok(None)
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
        // `%f` average — see the IntSummaryStatistics toString above.
        let avg = if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        };
        let s = format!(
            "LongSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}",
            count,
            sum,
            min,
            p56_format_java_f(avg),
            max
        );
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });
    // `LongSummaryStatistics` implements BOTH `LongConsumer` and `IntConsumer`
    // (javap, JDK 25), so it declares `accept(int)` beside `accept(long)`. Only
    // the long overload was registered, so `IntStream.forEach(stats::accept)`
    // and any `IntConsumer`-typed use of a LongSummaryStatistics reached a
    // bodiless declaration. The JDK's implementation is `accept((long) value)`.
    r.register(lss, "accept", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Int(i)) => *i as i64,
            _ => 0,
        };
        let count = p56_stats_long(ctx, this, STATS_FIELD_COUNT);
        let sum = p56_stats_long(ctx, this, STATS_FIELD_SUM);
        let min = p56_stats_long(ctx, this, STATS_FIELD_MIN);
        let max = p56_stats_long(ctx, this, STATS_FIELD_MAX);
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(count + 1));
        ctx.set_field(this, STATS_FIELD_SUM, Value::Long(sum.wrapping_add(val)));
        ctx.set_field(this, STATS_FIELD_MIN, Value::Long(min.min(val)));
        ctx.set_field(this, STATS_FIELD_MAX, Value::Long(max.max(val)));
        Ok(None)
    });
    // `combine` / the 4-arg ctor — see the IntSummaryStatistics notes above.
    r.register(
        lss,
        "combine",
        "(Ljava/util/LongSummaryStatistics;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let c = p56_stats_long(ctx, this, STATS_FIELD_COUNT);
            let s = p56_stats_long(ctx, this, STATS_FIELD_SUM);
            let mn = p56_stats_long(ctx, this, STATS_FIELD_MIN);
            let mx = p56_stats_long(ctx, this, STATS_FIELD_MAX);
            let oc = p56_stats_long(ctx, other, STATS_FIELD_COUNT);
            let os = p56_stats_long(ctx, other, STATS_FIELD_SUM);
            let omn = p56_stats_long(ctx, other, STATS_FIELD_MIN);
            let omx = p56_stats_long(ctx, other, STATS_FIELD_MAX);
            ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(c.wrapping_add(oc)));
            ctx.set_field(this, STATS_FIELD_SUM, Value::Long(s.wrapping_add(os)));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Long(mn.min(omn)));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Long(mx.max(omx)));
            Ok(None)
        },
    );
    r.register(lss, "<init>", "(JJJJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match args.get(1) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        let min = match args.get(2) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        let max = match args.get(3) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        let sum = match args.get(4) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        p56_stats_ctor_guard(count, min > max)?;
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(count.max(0)));
        if count > 0 {
            ctx.set_field(this, STATS_FIELD_SUM, Value::Long(sum));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Long(min));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Long(max));
        } else {
            ctx.set_field(this, STATS_FIELD_SUM, Value::Long(0));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Long(i64::MAX));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Long(i64::MIN));
        }
        Ok(None)
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
        // Java's Math.min/max, not Rust's `f64::min`/`max`: NaN must poison
        // min/max the way it poisons the sum, and `-0.0` must sort below `+0.0`.
        // See `p56_java_math_min` for the measured HotSpot behaviour.
        ctx.set_field(
            this,
            STATS_FIELD_MIN,
            Value::Double(p56_java_math_min(min, val)),
        );
        ctx.set_field(
            this,
            STATS_FIELD_MAX,
            Value::Double(p56_java_math_max(max, val)),
        );
        Ok(None)
    });
    r.register(dss, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = p56_stats_long(ctx, this, STATS_FIELD_COUNT);
        let sum = p56_stats_double(ctx, this, STATS_FIELD_SUM, 0.0);
        let min = p56_stats_double(ctx, this, STATS_FIELD_MIN, f64::INFINITY);
        let max = p56_stats_double(ctx, this, STATS_FIELD_MAX, f64::NEG_INFINITY);
        // The double flavour's JDK format string is
        // `"%s{count=%d, sum=%f, min=%f, average=%f, max=%f}"` — FOUR `%f`
        // fields, not one. Rust's `{}` printed `min=inf` for an empty
        // statistics where HotSpot 25 prints `min=Infinity` (measured), and
        // `sum=6.5` where it prints `sum=6.500000`.
        let avg = if count == 0 { 0.0 } else { sum / count as f64 };
        let s = format!(
            "DoubleSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}",
            count,
            p56_format_java_f(sum),
            p56_format_java_f(min),
            p56_format_java_f(avg),
            p56_format_java_f(max)
        );
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });
    // `combine` / the 4-arg ctor — see the IntSummaryStatistics notes above.
    // The synthetic 4-slot shape has no `sumCompensation`, so `combine` adds the
    // reconciled sums directly rather than replaying `sumWithCompensation`
    // twice; that costs the compensation term, not a field.
    r.register(
        dss,
        "combine",
        "(Ljava/util/DoubleSummaryStatistics;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let c = p56_stats_long(ctx, this, STATS_FIELD_COUNT);
            let s = p56_stats_double(ctx, this, STATS_FIELD_SUM, 0.0);
            let mn = p56_stats_double(ctx, this, STATS_FIELD_MIN, f64::INFINITY);
            let mx = p56_stats_double(ctx, this, STATS_FIELD_MAX, f64::NEG_INFINITY);
            let oc = p56_stats_long(ctx, other, STATS_FIELD_COUNT);
            let os = p56_stats_double(ctx, other, STATS_FIELD_SUM, 0.0);
            let omn = p56_stats_double(ctx, other, STATS_FIELD_MIN, f64::INFINITY);
            let omx = p56_stats_double(ctx, other, STATS_FIELD_MAX, f64::NEG_INFINITY);
            ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(c.wrapping_add(oc)));
            ctx.set_field(this, STATS_FIELD_SUM, Value::Double(s + os));
            ctx.set_field(
                this,
                STATS_FIELD_MIN,
                Value::Double(p56_java_math_min(mn, omn)),
            );
            ctx.set_field(
                this,
                STATS_FIELD_MAX,
                Value::Double(p56_java_math_max(mx, omx)),
            );
            Ok(None)
        },
    );
    // `DoubleSummaryStatistics(long count, double min, double max, double sum)`
    // carries a THIRD check the int/long flavours do not: if any of min, max or
    // sum is NaN then all three must be. Measured on HotSpot 25,
    // `new DoubleSummaryStatistics(2L, Double.NaN, 3.0, 4.0)` throws
    // `IllegalArgumentException: Some, not all, of the minimum, maximum, or sum
    // is NaN`.
    r.register(dss, "<init>", "(JDDD)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match args.get(1) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        let min = match args.get(2) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        };
        let max = match args.get(3) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        };
        let sum = match args.get(4) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        };
        p56_stats_ctor_guard(count, min > max)?;
        if count > 0 {
            let any_nan = min.is_nan() || max.is_nan() || sum.is_nan();
            let all_nan = min.is_nan() && max.is_nan() && sum.is_nan();
            if any_nan && !all_nan {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Some, not all, of the minimum, maximum, or sum is NaN".to_string(),
                }
                .into());
            }
        }
        ctx.set_field(this, STATS_FIELD_COUNT, Value::Long(count.max(0)));
        if count > 0 {
            ctx.set_field(this, STATS_FIELD_SUM, Value::Double(sum));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Double(min));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Double(max));
        } else {
            ctx.set_field(this, STATS_FIELD_SUM, Value::Double(0.0));
            ctx.set_field(this, STATS_FIELD_MIN, Value::Double(f64::INFINITY));
            ctx.set_field(this, STATS_FIELD_MAX, Value::Double(f64::NEG_INFINITY));
        }
        Ok(None)
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Collectors expansion: maxBy, minBy, mapping, filtering,
// summarizingInt/Long/Double, averaging*/summing*, toUnmodifiableList/Set,
// collectingAndThen.
//
// Every factory here mints its Collector through native-collections'
// `make_*_collector` helpers, because native-collections owns the ONLY
// registered `Stream.collect(Collector)` (`native_stream_collect`) and with it
// the only collector tag namespace anything decodes. This file used to stamp
// its own `P56_COLLECTOR_*` tags (maxBy=9, minBy=10, filtering=12,
// summarizing*=13/14/15, …) into 3-field objects that nothing ever read: those
// numbers aliased live tags on the other side, so `collect(minBy(cmp))` was
// decoded as `groupingBy(classifier, supplier, downstream)` and answered a Map
// instead of an Optional. Registration is last-wins and native-collections
// registers after this crate, so both crates must produce identical objects —
// route new factories through the shared helpers, never through a local tag.
// ---------------------------------------------------------------------------
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
            let comparator = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_max_by_collector(ctx, comparator)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- minBy(Comparator) → Collector ---
    r.register(
        col,
        "minBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let comparator = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_min_by_collector(ctx, comparator)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- mapping(Function, Collector) → Collector ---
    r.register(
        col,
        "mapping",
        "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let downstream = args.get(1).copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_mapping_collector(ctx, func, downstream)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- filtering(Predicate, Collector) → Collector ---
    r.register(
        col,
        "filtering",
        "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let pred = args.first().copied().unwrap_or(Value::Object(None));
            let downstream = args.get(1).copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_filtering_collector(ctx, pred, downstream)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summarizingInt(ToIntFunction) → Collector ---
    r.register(
        col,
        "summarizingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_summarizing_int_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summarizingLong(ToLongFunction) → Collector ---
    r.register(
        col,
        "summarizingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_summarizing_long_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summarizingDouble(ToDoubleFunction) → Collector ---
    r.register(
        col,
        "summarizingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_summarizing_double_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- toUnmodifiableList() → Collector ---
    r.register(
        col,
        "toUnmodifiableList",
        "()Ljava/util/stream/Collector;",
        |ctx, _args| {
            let c = cratonvm_native_collections::make_to_list_collector(ctx)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- toUnmodifiableSet() → Collector ---
    r.register(
        col,
        "toUnmodifiableSet",
        "()Ljava/util/stream/Collector;",
        |ctx, _args| {
            let c = cratonvm_native_collections::make_to_set_collector(ctx)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- collectingAndThen(Collector, Function) → Collector ---
    r.register(
        col,
        "collectingAndThen",
        "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let downstream = args.first().copied().unwrap_or(Value::Object(None));
            let finisher = args.get(1).copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_collecting_and_then_collector(
                ctx, downstream, finisher,
            )?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- averagingInt/Long/Double(To*Function) → Collector (Double average) ---
    r.register(
        col,
        "averagingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_averaging_int_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "averagingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_averaging_long_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "averagingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_averaging_double_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );

    // --- summingInt/Long/Double ---
    r.register(
        col,
        "summingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_summing_int_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "summingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_summing_long_collector(ctx, func)?;
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(
        col,
        "summingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let func = args.first().copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_summing_double_collector(ctx, func)?;
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
    // UnaryOperator.identity() — `SyntheticStub`, not the enclosing registrar's
    // `Bridge`. Same reasoning as L7 item 4 applied to `Function.identity` a few
    // hundred lines below, and this is the copy that lane missed: a strict
    // census A/B on 2026-08-06 found this registration and
    // `Function$Identity.apply` still ALIVE under `--jdk-only`, because L7
    // retagged the `native-builtins/src/lib.rs` cluster and these two sit in a
    // different file under a different scope.
    //
    // `java.util.function.UnaryOperator.identity()` is a one-line
    // `invokedynamic` returning `t -> t`, so a working real-bytecode fallback
    // plainly exists — which is exactly what `Bridge` asserts there is not. And
    // the stand-in it mints, `UnaryOperator$Identity`, has no class file
    // anywhere: contract §5 forbids fabricating it under `JdkOnly`, so the
    // `Bridge` tag was keeping alive a bridge to a receiver the policy says may
    // not exist. Dropped here, strict mode runs `java.base`'s own lambda.
    r.register_with_kind(
        uo,
        "identity",
        "()Ljava/util/function/UnaryOperator;",
        |ctx, _args| {
            // Create a lambda proxy that returns its argument
            let proxy = try_alloc_concurrent_synthetic(
                ctx,
                "java/util/function/UnaryOperator$Identity",
                0,
            )?;
            Ok(Some(Value::Object(Some(proxy))))
        },
        cratonvm_native_api::NativeKind::SyntheticStub,
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

    // TOMBSTONE — `BinaryOperator.maxBy(Comparator)` and `.minBy(Comparator)`
    // were registered here as `SyntheticStub`, minting a
    // `java/util/function/BinaryOperator$MaxBy` / `$MinBy` no image declares.
    // DELETED 2026-08-20 (H3-1). Two of the seven `java.util.function`
    // default/static-method stubs `G89-1` N1 nominated.
    //
    // Why deletion is safe rather than merely tidy, and what it does NOT buy:
    //
    //   * `javap -p java.util.function.BinaryOperator` on the JDK 25 image:
    //     both are `public static`, NOT `ACC_NATIVE`, with a real body
    //     (`(a, b) -> comparator.compare(a, b) >= 0 ? a : b`). So real
    //     bytecode serves the call the moment nothing shadows it.
    //   * The carriers they minted had NO `apply` registered anywhere in the
    //     workspace (the site's own note said so), so the object handed back
    //     could not be invoked in ANY mode.
    //   * `SyntheticStub` is not `allowed_in(JdkOnly)`, so `--jdk-only`
    //     already dropped both. **This deletion moves strict mode by exactly
    //     zero**; it changes `Compatible` / `--real-jdk` only.
    //
    // Do NOT re-add a mint here. If a future reader needs these, the answer is
    // real bytecode, and the failing-vector evidence is
    // `regression-suite/src/RJdkFunctionCombinators.java` (`notFabricated`).

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

    // TOMBSTONE — `Predicate.and`, `.or`, `.negate` and the static `.not` were
    // registered here as `SyntheticStub`, minting
    // `java/util/function/Predicate$$Lambda$And` / `$Or` / `$Negate`, none of
    // which any image declares. DELETED 2026-08-20 (H3-1). Four of the seven
    // `java.util.function` stubs `G89-1` N1 nominated.
    //
    // Why deletion, and what it does NOT buy:
    //
    //   * `javap -p java.util.function.Predicate` on the JDK 25 image: all four
    //     are `default`/`static` with real bodies and NOT `ACC_NATIVE`
    //     (`and` is `(t) -> test(t) && other.test(t)`). Real bytecode serves
    //     the call the moment nothing shadows it.
    //   * MEASURED, not reasoned: `RJdkFunctionCombinators` PASSES under
    //     `--jdk-only` — where these four are dropped at registration — and
    //     FAILS in `Compatible` on the same binary, where they mint
    //     (`G62-1` §1). Deleting the registration puts `Compatible` in the
    //     state strict mode is already measured green in.
    //   * `SyntheticStub` is not `allowed_in(JdkOnly)`, so **this moves strict
    //     mode by exactly zero.** It is a `Compatible`-mode correctness fix and
    //     a seven-row cut in wave 2's backlog, nothing more.
    //
    // The `test` natives on the three minted carriers are left registered a few
    // lines below: nothing mints those classes any more, so they are dead
    // rather than wrong, and removing them too would make the ratchet delta
    // something other than the exact -7 the re-freeze is derived from. See
    // H3-1's NOMINATIONS.
    //
    // NOT REMOVED HERE, AND IT MUST BE: `force_native_over_real_jdk_bytecode`
    // (`vm/src/runtime/interpreter/native_override.rs`) still names these four
    // triples. That arm is now inert — both consulting sites re-check the
    // registry (`dispatch_virtual.rs`) or only SEAL a method out of the JIT
    // (`jit_bridge.rs`), so a stale entry costs a branch and a missed tier-up,
    // never an `UnsatisfiedLinkError` — but it is a lie about the tree. H3-1
    // "OUT-OF-FILE EDITS REQUIRED" carries the exact deletion.

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

    // Function.compose/andThen/identity — `SyntheticStub`, not the enclosing
    // registrar's `Bridge` (JDK-only wave 2, lane L7 item 4, 2026-08-05).
    //
    // All three mint a `Function$Compose` / `Function$AndThen` /
    // `Function$Identity` stand-in, and no JDK declares any of those names:
    // the real `Function.compose`/`andThen` are default methods that return a
    // lambda, and `identity()` returns `t -> t`. So there IS a working
    // real-bytecode fallback, which is exactly what `Bridge` asserts there is
    // not — and under `--jdk-only` the `Bridge` tag meant strict mode invoked
    // them and then fabricated the stand-in behind a recorded violation.
    // Tagged `SyntheticStub`, strict mode drops the registration (recording a
    // `SyntheticNativeRegistered` violation naming this site) and the real
    // default methods run. `Compatible` / `--real-jdk` keep SyntheticStub
    // registrations, so both are byte-for-byte unchanged.
    //
    // `Function.identity` is registered a second time, later and with the same
    // treatment, by `register_function_identity_natives` in
    // `native-builtins/src/lib.rs`; registration is last-write-wins, so that
    // one is the copy a `Compatible` run actually dispatches. Both are tagged
    // the same way on purpose — a strict run must not depend on which of two
    // registrars ran last.
    // NOT REGISTERED AGAINST A REAL JDK IMAGE (H19, 2026-08-21).
    //
    // The `SyntheticStub` tag made `--jdk-only` drop `compose`/`andThen` and
    // run the real default methods; it left `Compatible` / `--real-jdk`
    // dispatching a stand-in the comment above already says is unnecessary
    // ("So there IS a working real-bytecode fallback"). Registering these two
    // only when the registry is NOT being populated for a real image finishes
    // that 2026-08-05 change in the mode it did not reach.
    //
    // MEASURED 2026-08-21 (`C:/craton/cratonvm-r5.exe`, oracle HotSpot
    // 25.0.3+9), one probe, three arms — TWO divergences per method, not one:
    //
    //   f.andThen(g).getClass()  HotSpot Function$$Lambda/0x… | --jdk-only
    //     Function$$Lambda/0x… | Compatible java.util.function.Function$AndThen
    //   f.compose(g).getClass()  HotSpot Function$$Lambda/0x… | --jdk-only
    //     Function$$Lambda/0x… | Compatible java.util.function.Function$Compose
    //   f.andThen(null)          HotSpot NullPointerException | --jdk-only
    //     NullPointerException | Compatible RETURNS, does not throw
    //   f.compose(null)          same shape
    //
    // The computed values were right in all three arms (`andThen` 30,
    // `compose` 21), which is why only a CLASS-NAME screen or a null-contract
    // check can see this: the stand-in does the arithmetic correctly and lies
    // about its identity and its argument checking.
    //
    // The null half is the part no record predicted. `Objects.requireNonNull`
    // lives in the default method's bytecode, so a native that replaces the
    // method silently drops it — and `Predicate.and`/`or`, `Consumer.andThen`
    // and `BinaryOperator.maxBy` all DO throw here, because H3-1 deleted their
    // stand-ins on 2026-08-20. That contrast is the measurement: seven rows
    // went, these two stayed, and the difference is visible from Java.
    //
    // WHY A GUARD AND NOT A DELETION (H15-3 §2.4 proposed deletion): four
    // tests in `vm/src/vm/tests.rs` — `m3_function_and_then_creates_composite`,
    // `m3_function_and_then_apply_chains_correctly`,
    // `m3_function_compose_creates_composite`,
    // `m3_function_compose_apply_chains_correctly` — reach these exact triples
    // through `call_native`, which PANICS on an absent registration, and two of
    // them `assert_eq!` the composite's class name against the stand-in. That
    // file is not this lane's to edit and
    // `cargo test -p cratonvm-vm --lib --features synthetic-jdk` blocks CI.
    // Its registry is `NativeMethodRegistry::new()` and never calls
    // `set_drop_real_layout_synthetic`, so the guard leaves it alone — as it
    // leaves the synthetic-JDK image, which has no `Function` bytecode to fall
    // back to. (H15-3 §4 N4 named two of the four; the other two are
    // `compose`'s and fail the same way.)
    //
    // `identity()` is NOT guarded here on purpose: `register_function_identity_
    // natives` in `native-builtins/src/lib.rs` registers it again, later, and
    // registration is last-write-wins, so guarding only this copy would change
    // nothing. It also does not need to be — MEASURED, `Function.identity()`,
    // `UnaryOperator.identity()` and `BinaryOperator.maxBy`/`minBy` already
    // return real lambdas in Compatible mode DESPITE being registered, and why
    // the static factories do not fire while these two default methods do is
    // unexplained. See the record's NOMINATIONS.
    //
    // docs/known-issues/jdk-only/H19-1-three-stand-ins-retired-against-a-real-image-20260821.md §2
    let __func_prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let func = "java/util/function/Function";
    if !r.drops_real_layout_synthetic() {
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
                let composite = crate::util_concurrent_ext::try_alloc_concurrent_synthetic(
                    ctx,
                    "java/util/function/Function$Compose",
                    2,
                )?;
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
                let composite = crate::util_concurrent_ext::try_alloc_concurrent_synthetic(
                    ctx,
                    "java/util/function/Function$AndThen",
                    2,
                )?;
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
    }
    // Function.identity()
    r.register(
        func,
        "identity",
        "()Ljava/util/function/Function;",
        |ctx, _args| {
            let proxy = crate::util_concurrent_ext::try_alloc_concurrent_synthetic(
                ctx,
                "java/util/function/Function$Identity",
                0,
            )?;
            Ok(Some(Value::Object(Some(proxy))))
        },
    );
    r.set_category(__func_prev_cat);

    // TOMBSTONE — `Consumer.andThen(Consumer)` was registered here as
    // `SyntheticStub`, minting a `java/util/function/Consumer$AndThen` no image
    // declares. DELETED 2026-08-20 (H3-1). The seventh and last of the
    // `java.util.function` stubs `G89-1` N1 nominated.
    //
    //   * `javap -p java.util.function.Consumer` on the JDK 25 image: `andThen`
    //     is `default`, NOT `ACC_NATIVE`, and returns
    //     `(T t) -> { accept(t); after.accept(t); }`.
    //   * Under `--jdk-only` this mint was already refused outright (the name
    //     carries no `$$Lambda` infix, so `fabricated_origin_for_name` gives it
    //     `ClassOrigin::CompatibilityStub` and §5 shuts the door), and the
    //     registration itself was already dropped — `SyntheticStub` is not
    //     `allowed_in(JdkOnly)`. **Strict mode moves by exactly zero.**
    //   * `Compatible` is where this changes anything: `RJdkFunctionCombinators`
    //     fails there and passes strict on the same binary (`G62-1` §1).
    //
    // `Consumer$AndThen.accept` is left registered below for the same reason as
    // the `Predicate` carriers: dead, not wrong, and keeping the ratchet delta
    // at exactly -7.

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

    // Function$Identity.apply(x) = x — `SyntheticStub`, the other half of the
    // copy L7 item 4 missed. `lib.rs`'s registration of this same triple is
    // already `SyntheticStub`; this one was `Bridge`, and under `--jdk-only`
    // the stub is refused at the door so THIS row survived to own the slot.
    // With both stated, no `Function$Identity` is minted under `--jdk-only` and
    // the method is unreachable, which is the point: the class has no bytes
    // anywhere and §5 forbids fabricating it.
    r.register_with_kind(
        "java/util/function/Function$Identity",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, args| Ok(Some(args[1])),
        cratonvm_native_api::NativeKind::SyntheticStub,
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
    // `getComparator` is NOT registered here any more. The identical three-way
    // rule -- the Comparator when SORTED by one, `null` when SORTED naturally,
    // `IllegalStateException` otherwise -- now lives in `native-collections`'
    // `register_iterator_protocol_natives`, which runs in EVERY mode. This pass
    // runs only under `--features synthetic-jdk`, so the rule was absent from
    // the shipping build and `treeSet.spliterator().getComparator()` fell
    // through to the interface default, which throws unconditionally
    // (probes/UtilTailShadowSweep 143).
    //
    // The two bodies AGREE -- this one asks `this.characteristics()` virtually,
    // which lands on the shipping body -- which is why this is the one row of
    // the six `registrar_drift::no_new_mode_drift` reported that was redundant
    // rather than wrong.
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
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
            let empty = ctx.read_native_pin(empty_pin, empty);
            ctx.set_field(stream, 0, Value::Object(Some(empty)));
            ctx.unpin_native_roots(empty_pin);
            return Ok(Some(Value::Object(Some(stream))));
        }
    };
    // Pin across the stream alloc below — a moving young GC there would
    // relocate the element array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
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
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 1)?;
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
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/LongStream", 1)?;
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
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/DoubleStream", 1)?;
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
    let spl = try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3)?;
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
    let spl = try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3)?;
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
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
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
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
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
            let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
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
    let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
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
    // Collectors.teeing(Collector, Collector, BiFunction) -> Collector.
    // Minted through native-collections like every other Collectors factory —
    // see the phase-56 block comment on why a locally-stamped tag is a bug.
    // This used to discard all three arguments and hand back a bare toList
    // collector, so `collect(teeing(a, b, merger))` answered a List of the
    // elements instead of `merger.apply(collect(a), collect(b))`.
    r.register("java/util/stream/Collectors", "teeing",
        "(Ljava/util/stream/Collector;Ljava/util/stream/Collector;Ljava/util/function/BiFunction;)Ljava/util/stream/Collector;",
        |ctx, args| {
            let downstream1 = args.first().copied().unwrap_or(Value::Object(None));
            let downstream2 = args.get(1).copied().unwrap_or(Value::Object(None));
            let merger = args.get(2).copied().unwrap_or(Value::Object(None));
            let c = cratonvm_native_collections::make_teeing_collector(
                ctx,
                downstream1,
                downstream2,
                merger,
            )?;
            Ok(Some(Value::Object(Some(c))))
        });

    // flatMapping and filtering already registered in Phase 56 — do not override
    r.set_category(__prev_cat);
}

// =============================================================================
// java.util.stream.Gatherer — Java 22 (preview → final Java 24)
//
// ONE slot map for this class, and it is NOT this file's.
//
// `java/util/stream/Gatherer` has NO `synthetic_stub_fields` arm, so
// `class_num_total_fields` answers 0 and `try_alloc_concurrent_synthetic`'s
// closing `let n = num_fields.max(real)` leaves the caller's request as the
// LITERAL object width — there is no clamp to hide a disagreement behind. This
// registrar used to model the class at 3 slots (initializer 0, integrator 1,
// finisher 2) while `lib.rs::register_pd_stream_gatherers` models it at 5
// (initializer 0, integrator 1, combiner 2, finisher 3, VM-internal KIND 4).
// Not two widths — two INCOMPATIBLE maps: `finisher` was slot 2 here and slot 3
// there, and slot 2 there is the `combiner`.
//
// The 5-slot map is the right one on both authorities available without a run:
//
//   * the ORACLE. `Gatherer` itself is an interface; the carrier the real JDK
//     returns from every one of its static factories is the record
//     `java.util.stream.Gatherers.GathererImpl`, whose components are
//     `initializer, integrator, combiner, finisher` in that order (JDK 25
//     source, `java.base/java/util/stream/Gatherers.java:502-506`). Slots 0..3
//     of the 5-slot map ARE that record, in order; slot 4 is a VM-internal kind
//     tag anchored past it. The 3-slot map dropped `combiner` — which is a real
//     interface method, `Gatherer.combiner()` — and so mis-seated `finisher`.
//   * the CONSUMERS. `register_phase_d_natives` (`lib.rs:24181`) runs AFTER
//     `register_phase67_natives` (`lib.rs:24124`) inside
//     `register_synthetic_overrides`, and `register()` is
//     last-registration-wins, so every reader that actually executes is
//     lib.rs's: `pd_stream_gather` opens with `ctx.get_field(gatherer, 4)`,
//     `finisher()` reads slot 3, `combiner()` reads slot 2.
//
// So every factory here was minting objects for readers that disagreed with it.
// Six of the seven were already shadowed by a 5-slot twin in `lib.rs`; the
// seventh, `ofSequential(Supplier,Integrator)`, was registered ONLY here, and
// its 3-slot product reached `get_field(gatherer, 4)`. `Heap::get_field`
// (`gc/src/heap.rs:652`) opens with `assert!(index < num_slots)`, so
// `stream.gather(Gatherer.ofSequential(sup, integ))` aborted the VM with
// "field index 4 out of bounds (num_slots=3)". That overload is now registered
// in the 5-slot shape at `lib.rs:42681`.
//
// The producers and accessors are therefore DELETED here rather than widened to
// 5: widening would leave two copies of one slot map to drift apart again,
// which is the defect itself. Deleted (each already re-registered later, and
// already winning, in `lib.rs::register_pd_stream_gatherers`):
//
//   Gatherer.of(Integrator)                                lib.rs:42693
//   Gatherer.ofSequential(Supplier,Integrator)             lib.rs:42681
//   Gatherer.ofSequential(Supplier,Integrator,BiConsumer)  lib.rs:42636
//   Gatherer.initializer() / integrator() / finisher()     lib.rs:42599..42634
//   Gatherers.fold / scan / windowFixed / windowSliding    lib.rs:42741..42811
//   Stream.gather(Gatherer)                                lib.rs:42591
//
// `lib.rs` additionally serves `Gatherer.combiner()`, `Gatherer$Downstream.push`
// and `Gatherers.mapConcurrent`, which this file never had. What is left below
// is the ONE pair `lib.rs` does not register.
//
// See docs/known-issues/jdk-only/E35-R11-SYNTHETIC-WIDTH-SWEEP-20260813.md §3.1
// and E41's record for the full derivation.
// =============================================================================

pub(crate) fn register_p67_gatherer(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let g = "java/util/stream/Gatherer";
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
    // That identity test is the only spec-defined observation, and the model
    // passes it: `Gatherer.of(..)` (now `lib.rs:42693`, the 5-slot map) stores
    // null in the initializer slot 0 and the finisher slot 3, and
    // `initializer()`/`finisher()` (`lib.rs:42599`/`:42626`) hand those same
    // slots straight back, so `g.initializer() == Gatherer.defaultInitializer()`
    // compares null with null and answers `true` exactly where the real JDK
    // would. The gather engine (`pd_gather_fold` / `pd_gather_scan` /
    // `pd_gather_custom` in lib.rs) reads the same sentinel by branching on
    // `Value::Object(Some(_))`.
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
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3)?;
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
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3)?;
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
                    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3)?;
                    ctx.set_field(obj, 0, Value::Object(Some(empty)));
                    ctx.set_field(obj, 1, Value::Int(0));
                    ctx.set_field(obj, 2, Value::Int(0));
                    return Ok(Some(Value::Object(Some(obj))));
                }
            };
            let mut collected: Vec<Value> = Vec::new();
            const SAFETY_CAP: usize = 1_000_000;
            // `iter` is held across every call in this loop; pin once and
            // re-derive per use.
            let iter_pin = ctx.pin_native_root(iter);
            let mut iter = iter;
            loop {
                iter = ctx.read_native_pin(iter_pin, iter);
                let has_next = ctx.invoke_virtual(iter, "hasNext", "()Z", &[]);
                let proceed = matches!(has_next, Ok(Some(Value::Int(1))));
                if !proceed {
                    break;
                }
                iter = ctx.read_native_pin(iter_pin, iter);
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
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3)?;
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
                    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
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
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
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
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 1)?;
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
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/LongStream", 1)?;
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
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/DoubleStream", 1)?;
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
    // Ask the policy BEFORE minting, because `alloc_concurrent_synthetic` is
    // the infallible funnel and would fabricate under `--jdk-only` anyway.
    // While any one site did that, every other site's refusal was
    // order-dependent rather than a policy — `probes/StrictIterPrimitivesProbe`
    // caught the same shape in the iterator family, where
    // `Arrays.asList(a).iterator()` minted `HashMap$KeyItr` and the next
    // `try_alloc_synthetic` for that name then found it and succeeded.
    if ctx
        .try_ensure_synthetic_class("cratonvm/internal/StreamCollector", 2)
        .is_err()
    {
        return cratonvm_native_collections::drain_spliterator_via_real_iterator(
            ctx,
            spliterator,
            1_000_000,
        );
    }
    // Allocate the collector consumer.
    let collector = try_alloc_concurrent_synthetic(ctx, "cratonvm/internal/StreamCollector", 2)?;
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
