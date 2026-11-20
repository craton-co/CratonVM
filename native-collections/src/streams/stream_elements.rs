#[allow(unused_imports)]
use super::*;

/// Extract elements from a Stream. Returns `Result` so a throwing intermediate
/// lambda (peek/map/filter) deferred under `CRATONVM_LAZY_STREAMS` propagates at
/// the terminal, matching HotSpot. (Gate-off there is never a chain, so this never
/// invokes a lambda and never errors — `?` at call sites is then a no-op.)
pub(crate) fn stream_elements(
    ctx: &mut dyn NativeContext,
    stream: &mut ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    // GC-SAFETY: `materialize_lazy_stream`/`stream_apply_chain_full` (and the
    // `toArray()` fallback below) allocate / drive an allocating spliterator
    // drain that can relocate `stream`. Every subsequent use of `stream` here
    // (class lookup, field read, toArray dispatch) must see the forwarded
    // reference, or we read a zeroed header and mis-dispatch
    // (`Object.toArray()` / empty stream). Pin it.
    //
    // `&mut ObjectRef`, NOT a by-value copy, and that is the whole contract:
    // the relocation this pin absorbs is just as visible to the CALLER, which
    // in ~85 of the sites below goes on using its own receiver afterwards.
    // This used to be `mut stream: ObjectRef` while `stream_elements_mut`'s
    // doc comment asserted "`stream_elements` is now `&mut`" — the refresh
    // reached the copy and died at the closing brace. See
    // `docs/feature-designs/real-jdk-mode.md`.
    let stream_pin = ctx.pin_native_root(*stream);
    // For our synthetic Stream object, field 0 holds an Object[] of elements.
    // But some streams are real JDK ReferencePipeline instances (returned by
    // e.g. Spring's MergedAnnotations.stream()) — materialize those via toArray().
    let class_id = ctx.class_id_of_object(*stream);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    // W7-65: `AbstractPipeline.linkedOrConsumed`. This is the single funnel
    // every operation on a synthetic stream reaches, so it is the single place
    // the flag is read and written. Reference streams only (see
    // `stream_link_or_consume` for why the primitive ones are excluded), and
    // ahead of any work: the JDK also throws before it evaluates anything.
    //
    // Nothing has allocated between `stream_pin` and here — `class_id_of_object`
    // and `class_name_of_id` are pure lookups — so `stream` is still current.
    let linked = if class_name == "java/util/stream/Stream" {
        stream_link_or_consume(ctx, *stream)
    } else {
        // Round 18 wave 11 (lane streams5): a PRIMITIVE carrier is only
        // MARKED here -- this funnel is infallible for it -- and its fallible
        // registration wrappers refuse the reuse
        // ([`stream_primitive_link_once_enabled`]).
        if is_synthetic_stream(&class_name) && stream_primitive_link_once_enabled() {
            stream_mark_linked(ctx, *stream);
        }
        Ok(())
    };
    // Single-exit so the `stream` pin is always released AND the caller's
    // reference is always refreshed (see GC-SAFETY above). The
    // `materialize_lazy_stream(..)?` below used to `?` straight out of the
    // function, past both the write-back and the `unpin_native_roots` this
    // comment promises; it is a `match` now.
    let result: Result<Vec<Value>, MethodCallFailed> = if let Err(e) = linked {
        Err(e)
    } else if is_synthetic_stream(&class_name) {
        // keycloak-16 Part B: a lazy stream defers its peek/map/filter/limit/skip
        // ops onto slot 3 while sharing the upstream SOURCE (slot 0, which may
        // itself still be an undrained lazy spliterator -- slot 2). Apply the
        // chain here so every eager terminal/op sees the fully-transformed
        // elements. Gate-off (default) never has a chain, so this is inert.
        //
        // FIX (stream-eager-drain-20260715): when a chain IS present, route
        // through `stream_apply_chain_full` (-> `stream_pull` ->
        // `stream_pull_internal`) WITHOUT first calling `materialize_lazy_stream`
        // here. `stream_pull_internal` itself now checks for a still-live lazy
        // spliterator and, if found, drives it INLINE through the chain one
        // element at a time. Doing the plain `materialize_lazy_stream` drain
        // unconditionally BEFORE this check (the old code) ran the raw
        // dumb-collector drain fully disconnected from the chain -- the bug.
        if lazy_streams_enabled() && stream_has_chain(ctx, *stream) {
            stream_apply_chain_full(ctx, *stream)
        } else {
            // No chain: either an ordinary eager stream, or a lazy stream with
            // no intermediate ops at all (e.g. `StreamSupport.stream(sp,
            // false).collect(...)` directly) -- draining it raw here is
            // correct/unavoidable in that case (there is no downstream
            // processing to interleave with `tryAdvance` either way).
            // THROWS5-3: a lazy walk's frames not built yet are built first
            // (a no-op for every other stream).
            let filled = materialize_lazy_stream(ctx, stream).and_then(|()| {
                *stream = ctx.read_native_pin(stream_pin, *stream);
                lazy_walk_fill(ctx, *stream, 0, usize::MAX)
            });
            match filled {
                Err(e) => Err(e),
                Ok(()) => {
                    *stream = ctx.read_native_pin(stream_pin, *stream);
                    if let Value::Object(Some(arr)) = ctx.get_field(*stream, STREAM_FIELD_ELEMENTS)
                    {
                        let len = ctx.array_length(arr);
                        Ok((0..len).map(|i| ctx.get_array_element(arr, i)).collect())
                    } else {
                        Ok(Vec::new())
                    }
                }
            }
        }
    } else {
        // Real JDK object pipeline (e.g. Spring's MergedAnnotations.stream()):
        // materialize via the object toArray. (Real PRIMITIVE pipelines don't
        // reach here — `Arrays.stream(int[])` / `IntStream.of(...)` are
        // intercepted to produce synthetic primitive streams.)
        match ctx.invoke_virtual(*stream, "toArray", "()[Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(arr)))) => {
                let len = ctx.array_length(arr);
                Ok((0..len).map(|i| ctx.get_array_element(arr, i)).collect())
            }
            Ok(_) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    };
    // The write-back is what makes the `&mut` mean anything: on every arm --
    // including the two that ran real bytecode -- the caller's own reference
    // becomes the post-GC address before this returns.
    *stream = ctx.read_native_pin(stream_pin, *stream);
    ctx.unpin_native_roots(stream_pin);
    result
}

/// True for CratonVM's synthetic Stream / IntStream / LongStream / DoubleStream
/// objects (all share the `STREAM_FIELD_ELEMENTS` backing-array layout). The
/// primitive-stream interface names must be included — `make_int_stream` etc.
/// allocate objects whose class is the interface name `java/util/stream/IntStream`,
/// and `int_stream_elements` delegates here; previously only the bare
/// `java/util/stream/Stream` matched, so EVERY primitive-stream terminal that
/// read elements this way (e.g. `IntStream.average()`) saw an empty stream and
/// returned the empty/zero result. Real JDK primitive streams are concrete
/// `*Pipeline` classes, not these interface names, so they still fall through to
/// the `toArray()` materialisation path.
pub(crate) fn is_synthetic_stream(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/util/stream/Stream"
            | "java/util/stream/IntStream"
            | "java/util/stream/LongStream"
            | "java/util/stream/DoubleStream"
    )
}

/// Materialise a possibly-REAL primitive stream's elements. CratonVM's synthetic
/// primitive streams keep elements in `STREAM_FIELD_ELEMENTS`, but factories like
/// `IntStream.of(int...)` delegate to `Arrays.stream` and return a real
/// `IntPipeline$Head`, whose elements are only reachable via the *primitive*
/// `toArray()` (`()[I` / `()[J` / `()[D`, NOT `()[Ljava/lang/Object;`). `toarray_desc`
/// selects the right one. Used by `flatMap`, whose mapper commonly returns such
/// real primitive streams.
///
/// Fallible for the reason on [`collection_elements_generic`]: the receiver
/// here is frequently a REAL JDK pipeline, so `toArray` is real bytecode that
/// can throw — and the `let _ =` on the drain plus the catch-all
/// `_ => Vec::new()` turned both a thrown exception and a strict refusal into
/// "that sub-stream was empty", which `flatMap` then concatenated into its
/// result as if it were.
///
/// `&mut` for the same reason [`stream_elements`] is: `materialize_lazy_stream`
/// states a refresh contract in its parameter mode, and a by-value wrapper
/// satisfies it with a copy the caller never sees.
pub(crate) fn prim_stream_values(
    ctx: &mut dyn NativeContext,
    stream: &mut ObjectRef,
    toarray_desc: &str,
) -> Result<Vec<Value>, MethodCallFailed> {
    materialize_lazy_stream(ctx, stream)?;
    let cn = ctx
        .class_name_of_id(ctx.class_id_of_object(*stream))
        .unwrap_or_default();
    if is_synthetic_stream(&cn) {
        if let Value::Object(Some(arr)) = ctx.get_field(*stream, STREAM_FIELD_ELEMENTS) {
            let len = ctx.array_length(arr);
            return Ok((0..len).map(|i| ctx.get_array_element(arr, i)).collect());
        }
        return Ok(Vec::new());
    }
    // A REAL pipeline: `toArray` is bytecode, so it can collect and relocate
    // the receiver. Refresh the caller's reference across it.
    let stream_pin = ctx.pin_native_root(*stream);
    let out = match ctx.invoke_virtual(*stream, "toArray", toarray_desc, &[]) {
        Ok(Some(Value::Object(Some(arr)))) => {
            let len = ctx.array_length(arr);
            Ok((0..len).map(|i| ctx.get_array_element(arr, i)).collect())
        }
        Err(e) => Err(e),
        _ => Ok(Vec::new()),
    };
    *stream = ctx.read_native_pin(stream_pin, *stream);
    ctx.unpin_native_roots(stream_pin);
    out
}

// JDK-ONLY-CLASSIFY: unknown — needs census. Same abstract-interface hazard as
// `register_interface_natives`: 30 of 37 registrations here target abstract
// methods of `java.util.stream.Stream` and friends. `java.util.stream` has no
// ACC_NATIVE method anywhere, so nothing in this group is a bridge; the open
// question is whether each entry should become an `Intrinsic` (the pipeline
// fusion these natives perform is a real speedup) or be deleted so the JDK's
// own `ReferencePipeline` runs. That question is answered by measurement, not
// by `javap`. Do not retag without a benchmark and a differential run.
pub(crate) fn register_stream_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/stream/Stream";
    // FIX (stream-eager-drain-20260715): `accept(Object)V` native for the
    // `cratonvm/internal/StreamChainCollector` consumer used by
    // `drain_spliterator_inline` to drive a lazy spliterator's elements
    // straight through their downstream op-chain, one `tryAdvance` at a
    // time. See stream-eager-drain-inline-fix-20260715-FIXED.md.
    r.register(
        STREAM_CHAIN_COLLECTOR_CLASS,
        "accept",
        "(Ljava/lang/Object;)V",
        native_stream_chain_collector_accept, cratonvm_native_api::NativeKind::Bridge,
    );

    // Source methods
    r.register(
        c,
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        native_stream_of_one, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "of",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
        native_stream_of_array, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "empty",
        "()Ljava/util/stream/Stream;",
        native_stream_empty, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "concat",
        "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;",
        |ctx, args| stream_concat_op(ctx, args, native_stream_concat), cratonvm_native_api::NativeKind::Bridge,
    );

    // Intermediate operations
    r.register(
        c,
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        |ctx, args| stream_stage_op(ctx, args, native_stream_filter, StageClass::RefFilter), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "map",
        "(Ljava/util/function/Function;)Ljava/util/stream/Stream;",
        |ctx, args| stream_stage_op(ctx, args, native_stream_map, StageClass::RefMap), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "flatMap",
        "(Ljava/util/function/Function;)Ljava/util/stream/Stream;",
        |ctx, args| stream_stage_op(ctx, args, native_stream_flat_map, StageClass::RefFlatMap), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "sorted", "()Ljava/util/stream/Stream;", |ctx, args| {
        stream_stage_op(ctx, args, native_stream_sorted, StageClass::RefSorted)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "sorted",
        "(Ljava/util/Comparator;)Ljava/util/stream/Stream;",
        |ctx, args| stream_stage_op(ctx, args, native_stream_sorted_cmp, StageClass::RefSorted), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "distinct", "()Ljava/util/stream/Stream;", |ctx, args| {
        stream_stage_op(ctx, args, native_stream_distinct, StageClass::RefDistinct)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "limit", "(J)Ljava/util/stream/Stream;", |ctx, args| {
        stream_slice_op(ctx, args, native_stream_limit, StageClass::RefSlice, false)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "skip", "(J)Ljava/util/stream/Stream;", |ctx, args| {
        stream_slice_op(ctx, args, native_stream_skip, StageClass::RefSlice, true)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "peek",
        "(Ljava/util/function/Consumer;)Ljava/util/stream/Stream;",
        |ctx, args| stream_stage_op(ctx, args, native_stream_peek, StageClass::RefPeek), cratonvm_native_api::NativeKind::Bridge,
    );

    // Terminal operations
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_stream_for_each, cratonvm_native_api::NativeKind::Bridge,
    );
    // W7-9 §8.1 precondition 1. `forEachOrdered(Consumer)` is ABSTRACT on the
    // real `java.util.stream.Stream`, so — per the interface rule in
    // docs/architecture/natives-over-real-jdk-classes.md §1 and W7-9 §2 — this
    // registration is reachable only for a receiver whose runtime class IS the
    // interface (a CratonVM mint); a real `ReferencePipeline` has its own
    // `Code` and is not shadowed.
    //
    // Until 2026-08-12 the ONLY registration of this triple was in
    // `native-builtins/src/phases_late/streams.rs::register_phase56_stream_extras`,
    // which `register_synthetic_overrides` alone reaches — a no-op shim in both
    // shipping builds. That dead registrar is why
    // `vm/src/runtime/interpreter.rs`'s `!has_code` arm carries a hardcoded
    // `method_name == "forEachOrdered"` re-dispatch to `forEach`, which tests
    // the NAME and descriptor and not the `class_id`.
    //
    // Registering it here is deliberately behaviour-NEUTRAL today: that
    // interpreter special case runs ~20 lines ABOVE `resolve_native_for_dispatch`
    // and still wins, so this callback is not yet reached. It exists so the
    // special case can be deleted — W7-9 §8.1's order is "land the registration,
    // prove it live, then delete", and deleting first converts a wrong answer
    // into an `AbstractMethodError`.
    //
    // `forEach` is the correct body for the same reason the deleted comment
    // gave: every synthetic stream here is sequential, and `stream_elements`
    // yields the encounter-order snapshot. The three PRIMITIVE widths take a
    // different descriptor and are registered by
    // `phases_late::register_phase56_primitive_stream_terminals`, wired into the
    // essentials path — see `native-builtins/tests/essential_wiring_ratchet.rs`,
    // whose comment naming this row as "the next row to add" is now stale.
    r.register(
        c,
        "forEachOrdered",
        "(Ljava/util/function/Consumer;)V",
        native_stream_for_each, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "count", "()J", native_stream_count, cratonvm_native_api::NativeKind::Bridge);
    // keycloak-16 Part B: chain-aware `iterator()`. The phases_late / streams.rs
    // `iterator()` natives read the backing array (slot 0) DIRECTLY, which under a
    // deferred lazy chain returns the RAW source (un-filtered/un-mapped) — e.g.
    // `filter(nonNull).iterator()` leaks nulls (broke Spring Boot's
    // `IndexedElementsBinder` → `ConfigurationPropertyName.chop()` NPE during
    // `spring.config.name` binding). native-collections registers after
    // native-builtins (last-writer-wins), so this override wins; it materialises
    // via the chain-aware `stream_elements` before iterating.
    r.register(
        c,
        "iterator",
        "()Ljava/util/Iterator;",
        native_stream_iterator, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/stream/BaseStream",
        "iterator",
        "()Ljava/util/Iterator;",
        native_stream_iterator, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_stream_to_array, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_stream_to_array_gen, cratonvm_native_api::NativeKind::Bridge,
    );
    // NOTE: we deliberately do NOT override `toArray(IntFunction)` on the
    // concrete `java/util/stream/ReferencePipeline` class.
    //
    // A *real* JDK `ReferencePipeline`'s no-arg `toArray()` (real bytecode,
    // `ReferencePipeline.java:658`) is `return toArray(Object[]::new)` — it
    // delegates to the generator overload. `stream_elements()` materialises a
    // real (non-synthetic) pipeline by calling that no-arg `toArray()` via
    // `invoke_virtual`. If we shadow the generator overload with
    // `native_stream_to_array_gen` (which itself calls `stream_elements`), the
    // two bounce forever:
    //   stream_elements → toArray() [real bytecode] → toArray(IntFunction)
    //     [our native] → stream_elements → toArray() → …  ⇒ StackOverflowError.
    // This was the open `MergedAnnotations.stream()` / bug-06 fam6 "~2 GB OOM"
    // and it blocked the entire JUnit-Platform Spring suite (the launcher's
    // TestPlan build calls `Stream.collect`/`toArray` on real pipelines).
    //
    // Synthetic CratonVM streams have class name `java/util/stream/Stream`, so
    // their `toArray(IntFunction)` is still served by the interface-level
    // registration above (the Kafka `Utils.enumOptions` typed-array case).
    // Real pipelines run the real JDK `toArray(IntFunction)` bytecode, which is
    // exactly what `stream_elements`' real-pipeline branch already relies on.
    // close() — BaseStream.close is abstract; synthetic Stream objects
    // (class `java/util/stream/Stream`) dispatch directly to the abstract
    // interface declaration and throw AbstractMethodError. Register a
    // no-op so try-with-resources and explicit s.close() succeed.
    // Without this, jboss-modules / WildFly / Keycloak boot hits
    // "BaseStream.close()V has no Code attribute" inside lambda/stream-based
    // utility methods during MBean / module wiring.
    // keycloak #16 Part A: real close-handler semantics for synthetic streams.
    // onClose(Runnable) records the handler (field 1); close() runs them once.
    // Registered here so this last-registration WINS over the streams.rs
    // `return_this`/no-op stubs (registration order: register_builtins then
    // register_collections_natives, per vm/src/vm/vm_init.rs).
    for sc in &[
        c,
        "java/util/stream/BaseStream",
        "java/util/stream/IntStream",
        "java/util/stream/LongStream",
        "java/util/stream/DoubleStream",
    ] {
        r.register(sc, "close", "()V", native_stream_close, cratonvm_native_api::NativeKind::Bridge);
    }
    // Override every (class, return-descriptor) combo streams.rs registered for
    // onClose (each call site records its own covariant return type, plus the
    // erased BaseStream return) so our handler-recording native always wins.
    for (cls, ret) in &[
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
    ] {
        r.register(
            cls,
            "onClose",
            &format!("(Ljava/lang/Runnable;){}", ret),
            native_stream_on_close, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    r.register(
        c,
        "findFirst",
        "()Ljava/util/Optional;",
        native_stream_find_first, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "findAny",
        "()Ljava/util/Optional;",
        native_stream_find_first, cratonvm_native_api::NativeKind::Bridge,
    ); // same as findFirst for sequential
    r.register(
        c,
        "anyMatch",
        "(Ljava/util/function/Predicate;)Z",
        native_stream_any_match, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "allMatch",
        "(Ljava/util/function/Predicate;)Z",
        native_stream_all_match, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "noneMatch",
        "(Ljava/util/function/Predicate;)Z",
        native_stream_none_match, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "reduce",
        "(Ljava/lang/Object;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;",
        native_stream_reduce_identity, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Stream.reduce(U, BiFunction<U, ? super T, U>, BinaryOperator<U>)` is
    // distinct from the same-typed identity overload above.  Spring HATEOAS
    // uses it to fold stream elements into a HAL Forms builder, so omitting
    // this descriptor resolves the abstract Stream declaration and produces
    // `AbstractMethodError: ... has no Code attribute`.
    r.register(
        c,
        "reduce",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;",
        native_stream_reduce_general, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "reduce",
        "(Ljava/util/function/BinaryOperator;)Ljava/util/Optional;",
        native_stream_reduce_optional, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "min",
        "(Ljava/util/Comparator;)Ljava/util/Optional;",
        native_stream_min, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "max",
        "(Ljava/util/Comparator;)Ljava/util/Optional;",
        native_stream_max, cratonvm_native_api::NativeKind::Bridge,
    );

    // collect — register on both the Stream interface and JDK's
    // ReferencePipeline concrete class. JDK code obtained via
    // `MergedAnnotations.stream()` returns a real `ReferencePipeline`, so
    // `invokeinterface Stream.collect` dispatches to `ReferencePipeline.collect`
    // (a JDK method) rather than our `Stream.collect` native. The JDK
    // implementation would then call `Collector.supplier/accumulator/finisher`
    // on our synthetic tagged Collector, which has no such methods. Intercept
    // `ReferencePipeline.collect` so our tagged-Collector dispatch always runs.
    r.register(
        c,
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        native_stream_collect, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/stream/ReferencePipeline",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        native_stream_collect, cratonvm_native_api::NativeKind::Bridge,
    );

    // 3-arg collect — `<R> R collect(Supplier<R>, BiConsumer<R,? super T>, BiConsumer<R,R>)`.
    // In real JDK 25 this is an ABSTRACT method on `Stream` (the body lives in
    // `ReferencePipeline`), so when our synthetic Stream (class =
    // `java/util/stream/Stream` interface) is the receiver, invokeinterface
    // resolves to the abstract declaration and throws
    // "AbstractMethodError: Stream.collect(...) has no Code attribute".
    // H2's `FilePathDisk.newDirectoryStream` hits this on every directory
    // listing because `Files.list(Path)` -> `StreamSupport.stream(spliterator, false)`
    // is intercepted by our native and returns a synthetic Stream.
    //
    // Semantics: `R c = supplier.get(); for elem in stream: accumulator.accept(c, elem);
    // return c;`. The combiner is parallel-only and we are sequential-only,
    // so it is ignored (matches JDK behaviour for non-parallel streams).
    r.register(
        c,
        "collect",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;",
        native_stream_collect_3arg, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/stream/ReferencePipeline",
        "collect",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;",
        native_stream_collect_3arg, cratonvm_native_api::NativeKind::Bridge,
    );

    // mapToInt
    r.register(
        c,
        "mapToInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/IntStream;",
        |ctx, args| stream_stage_op(ctx, args, native_stream_map_to_int, StageClass::RefMapToInt), cratonvm_native_api::NativeKind::Bridge,
    );

    // mapToLong
    r.register(
        c,
        "mapToLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/LongStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                |ctx, args| {
                    synthetic_stream_map_to_primitive(
                        ctx,
                        args,
                        "applyAsLong",
                        "(Ljava/lang/Object;)J",
                        Value::Long(0),
                        "java/util/stream/LongStream",
                    )
                },
                StageClass::RefMapToLong,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // flatMapToInt — Stream<T>.flatMapToInt(T -> IntStream) -> IntStream.
    // Lucene's UnicodeUtil clinit uses this; without a native the
    // abstract method on `Stream` is called and the clinit aborts with
    // AbstractMethodError.
    r.register(
        c,
        "flatMapToInt",
        "(Ljava/util/function/Function;)Ljava/util/stream/IntStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                |ctx, args| {
                    synthetic_stream_flat_map_to_primitive(
                        ctx,
                        args,
                        "()[I",
                        "java/util/stream/IntStream",
                    )
                },
                StageClass::RefFlatMapToInt,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // flatMapToLong — same shape returning a LongStream.
    r.register(
        c,
        "flatMapToLong",
        "(Ljava/util/function/Function;)Ljava/util/stream/LongStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                |ctx, args| {
                    synthetic_stream_flat_map_to_primitive(
                        ctx,
                        args,
                        "()[J",
                        "java/util/stream/LongStream",
                    )
                },
                StageClass::RefFlatMapToLong,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // flatMapToDouble — same shape returning a DoubleStream.
    r.register(
        c,
        "flatMapToDouble",
        "(Ljava/util/function/Function;)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                |ctx, args| {
                    synthetic_stream_flat_map_to_primitive(
                        ctx,
                        args,
                        "()[D",
                        "java/util/stream/DoubleStream",
                    )
                },
                StageClass::RefFlatMapToDouble,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // mapToDouble
    r.register(
        c,
        "mapToDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                |ctx, args| {
                    synthetic_stream_map_to_primitive(
                        ctx,
                        args,
                        "applyAsDouble",
                        "(Ljava/lang/Object;)D",
                        Value::Double(0.0),
                        "java/util/stream/DoubleStream",
                    )
                },
                StageClass::RefMapToDouble,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // toList (Java 16+ convenience). The JDK's unmodifiable `ListN` under
    // `CRATONVM_COMPAT_JDK_IMMUTABLES`, read once here, per VM (interpreter
    // round i1 wave 49, lane L6).
    // The HotSpot-alignment row `jdk-immutables` since interpreter round i6
    // wave 118, lane L5 (the variable is its legacy key).
    let to_list: cratonvm_native_api::NativeCallback =
        if r.aligned(cratonvm_types::HotSpotAlignment::CompatJdkImmutables) {
            native_stream_to_list_jdk
        } else {
            native_stream_to_list
        };
    r.register(c, "toList", "()Ljava/util/List;", to_list, cratonvm_native_api::NativeKind::Bridge);

    // spliterator() — declared abstract on `java.util.stream.BaseStream` and
    // inherited by Stream/IntStream/LongStream/DoubleStream. Real-JDK bytecode
    // (e.g. Netty's iterator-from-stream paths) calls `stream.spliterator()`;
    // with no Java-side pipeline our synthetic Stream has nothing to dispatch
    // to, surfacing as `NoSuchMethodError Stream.spliterator()`. Register a
    // native on every Stream sub-interface (plus BaseStream) that materialises
    // the stream's backing Object[] into a synthetic 3-field Spliterator
    // (field 0 = array, field 1 = pos, field 2 = fence) — the exact layout the
    // existing Spliterator natives (estimateSize/tryAdvance/forEachRemaining/
    // characteristics) already understand.
    for sc in &[
        "java/util/stream/Stream",
        "java/util/stream/BaseStream",
        "java/util/stream/IntStream",
        "java/util/stream/LongStream",
        "java/util/stream/DoubleStream",
        "java/util/stream/ReferencePipeline",
    ] {
        r.register(
            sc,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_stream_spliterator, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    }

/// An empty synthetic 3-field `java/util/Spliterator` (backing array, cursor,
/// fence): the null-receiver answer of the `spliterator()` natives.
///
/// gc-common w20-a: the three copies this replaces allocated the array, then
/// the shell, and stored the array through its pre-allocation address. The
/// array is rooted across the shell's allocation here.
pub(crate) fn empty_synthetic_spliterator(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let spl = alloc_empty_array_holder(ctx, "java/util/Spliterator", 3)?;
    ctx.set_field(spl, 2, Value::Int(0));
    Ok(Some(Value::Object(Some(spl))))
}

/// A synthetic `class` object of `n_fields` slots whose slot 0 is a fresh
/// empty `Object[]` and slot 1 is `Int(0)`: the shape of every empty
/// iterator / enumeration / spliterator this crate mints. The caller writes
/// any slot past 1.
///
/// gc-common w20-a: the twelve inline copies this replaces allocated the
/// array, then the holder, and stored the array through its pre-allocation
/// address (an argument use, which the stale-handle scanner does not see).
/// The array is rooted across the holder's allocation here.
pub(crate) fn alloc_empty_array_holder(
    ctx: &mut dyn NativeContext,
    class: &str,
    n_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let arr: ObjectRef = alloc_ref_array(ctx, 0);
    let arr_pin = ctx.pin_native_root(arr);
    let holder = match try_alloc_synthetic(ctx, class, n_fields) {
        Ok(h) => h,
        Err(e) => {
            ctx.unpin_native_roots(arr_pin);
            return Err(e);
        }
    };
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    ctx.set_field(holder, 0, Value::Object(Some(arr)));
    ctx.set_field(holder, 1, Value::Int(0));
    Ok(holder)
}

/// `Stream.spliterator()` / `BaseStream.spliterator()` — build a synthetic
/// Spliterator over the stream's backing elements.
///
/// The returned object is our standard 3-field synthetic Spliterator
/// (field 0 = backing Object[], field 1 = cursor/pos, field 2 = fence),
/// which `native_spliterator_{estimate_size,characteristics,try_advance,
/// for_each_remaining}` all consume directly. Works for both synthetic
/// Streams (field-0 array read) and real-JDK ReferencePipeline instances
/// (materialised via `toArray()` inside `stream_elements_mut`).
pub(crate) fn native_stream_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // Null receiver — return an empty spliterator.
            return empty_synthetic_spliterator(ctx);
        }
    };
    // `--real-jdk`: a REAL pipeline (`ReferencePipeline$Head`, `LongPipeline$3`,
    // ...) has a real, LAZY `spliterator()` -- `AbstractPipeline.spliterator()`,
    // a `WrappingSpliterator` that pulls on demand. Materialising the stream here
    // (and then building the fabricated `java/util/Spliterator` below, which a
    // strict run refuses anyway) turns every infinite stream that reaches
    // `spliterator()` into an `OutOfMemoryError`: `Stream.concat(Stream.of(x),
    // random.longs(a, b).mapToObj(..))` -- `Stream.concat` calls
    // `b.spliterator()` -- failed `InstantFormatterTests` at class load with
    // `alloc_array length 268435456`.
    let jdk_fallback = ctx.is_jdk_only() && spliterator_jdk_fallback_enabled();
    // Round 17 wave 1 (lane streams): under `--compatible` a REAL pipeline
    // stage takes the same road (`real_pipeline_spliterator_delegates`), so
    // its spliterator is the JDK's own `WrappingSpliterator` / source
    // spliterator, byte for byte, instead of a snapshot whose facts are
    // guessed from the stage's depth.
    let compat_real = !ctx.is_jdk_only() && real_pipeline_spliterator_delegates(&*ctx, this);
    if ctx.is_jdk_only() || compat_real {
        let class_name = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_default();
        if !is_synthetic_stream(&class_name) {
            // Round 18 wave 11 (lane backlogfix, R16B-194 audit): the JDK's
            // `spliterator()` is Java and can collect; a failure that falls
            // through (`_ => {}` below) keeps using `this`, so it is pinned
            // and re-read across the call
            // (`scripts/stale-handle-across-alloc-audit.py` baseline 0).
            let this_pin = ctx.pin_native_root(this);
            let real = ctx.invoke_special(
                "java/util/stream/AbstractPipeline",
                "spliterator",
                "()Ljava/util/Spliterator;",
                &[Value::Object(Some(this))],
            );
            this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            match real {
                Ok(Some(spl @ Value::Object(Some(_)))) => return Ok(Some(spl)),
                // Wave 9 (lane spliter, backlog collections w8b-1): the JDK's
                // own `spliterator()` threw -- `IllegalStateException` for a
                // linked or consumed pipeline, most often. That is the answer;
                // materialising the pipeline behind its back (the old
                // fall-through) ran a stage HotSpot refused to run.
                Err(e) if jdk_fallback || compat_real => return Err(e),
                _ => {}
            }
        }
    }
    // Round 17 wave 1 (lane streams): a HEAD stage still holding its
    // undrained source spliterator (`StreamSupport.stream(spl, ..)`, so every
    // `Collection.stream()` default) answers that spliterator itself, as
    // `AbstractPipeline.spliterator()` does for `this == sourceStage`, after
    // the same linked-or-consumed check. It used to drain it into a
    // splitting snapshot with the list default's facts.
    // Only a carrier wide enough to remember that it was linked (the
    // full layout `StreamSupport.stream` builds since this round): a short one
    // could not refuse a second use, which HotSpot does.
    // Round 17 wave 2 (lane streams2): not a WRAPPED stage that holds the
    // source without an op chain (`stream_lazy_unordered_stage`): that is an
    // intermediate stage, whose spliterator is a `WrappingSpliterator`.
    if lazy_head_spliterator_enabled()
        && stream_is_reference_carrier(&*ctx, this)
        && ctx.object_num_fields(this) > STREAM_FIELD_LINKED
        && !stream_has_chain(&*ctx, this)
        && matches!(
            ctx.get_field(this, STREAM_FIELD_ELEMENTS),
            Value::Object(None)
        )
        && !matches!(stream_spl_facts(&*ctx, this), Some((c, _)) if c & SPL_PRIV_WRAPPED != 0)
    {
        if let Some(spl) = stream_lazy_spliterator(&*ctx, this) {
            stream_link_or_consume(ctx, this)?;
            return Ok(Some(Value::Object(Some(spl))));
        }
    }
    // Round 17 wave 2 (lane streams2): an unsized WRAPPED stage over a
    // still-undrained source reports the SOURCE's estimate
    // ([`stream_lazy_stage_estimate_enabled`]); ask it before the drain below.
    // A failing call records nothing (the snapshot's count, as before).
    let mut lazy_source_estimate: Option<usize> = None;
    if stream_lazy_stage_estimate_enabled() && real_pipeline_shape(&*ctx, this).is_none() {
        let unsized_wrapped = matches!(
            stream_spl_facts(&*ctx, this),
            Some((c, None)) if c & SPL_PRIV_WRAPPED != 0 && c & SPL_SIZED == 0
        );
        // A linked stage refuses below before anything is asked, as HotSpot does.
        if unsized_wrapped && !stream_is_linked(&*ctx, this) {
            if let Some(spl) = stream_lazy_spliterator(&*ctx, this) {
                let this_pin = ctx.pin_native_root(this);
                let r = ctx.invoke_virtual(spl, "estimateSize", "()J", &[]);
                this = ctx.read_native_pin(this_pin, this);
                ctx.unpin_native_roots(this_pin);
                if let Ok(Some(Value::Long(n))) = r {
                    lazy_source_estimate = Some(n.max(0) as usize);
                }
            }
        }
    }
    let elements = stream_elements(ctx, &mut this)?;
    // Wave 9 (lane spliter): what this stage's spliterator reports
    // ([`stream_spl_facts`]), read from the refreshed `this` before anything
    // below allocates. Off under the kill switch, which restores the
    // three-field list-default snapshot.
    // Wave 15 (lane compat2): a REAL pipeline stage has no carrier slots;
    // its shape is read by field name instead ([`real_pipeline_shape`]).
    let real = real_pipeline_shape(&*ctx, this);
    let facts = if spliterator_facts_enabled() || jdk_fallback {
        match real {
            Some(shape) => real_pipeline_spl_facts(shape),
            None => stream_spl_facts(&*ctx, this),
        }
    } else {
        None
    };
    // Wave 14 (lane compat2, CC13-3): a parallel pipeline's
    // `WrappingSpliterator` splits its source; read with the facts.
    let parallel = match real {
        Some(shape) => shape.parallel,
        None => stream_is_parallel(&*ctx, this),
    };
    let comparator = match facts {
        Some(_) if real.is_none() && ctx.object_num_fields(this) > STREAM_FIELD_SPL_CMP => {
            match ctx.get_field(this, STREAM_FIELD_SPL_CMP) {
                v @ Value::Object(Some(_)) => v,
                _ => Value::Object(None),
            }
        }
        _ => Value::Object(None),
    };
    let n = elements.len();
    // GC-SAFETY: `elements` are bare refs; `alloc_ref_array` / `alloc_synthetic`
    // below can trigger a moving GC that relocates them and `arr`. Pin every
    // element (and then `arr`) and read each back before storing, or the
    // spliterator ends up backed by stale/zeroed slots.
    let (base, handles) = pin_value_slice(ctx, &elements);
    let cmp_pin = pin_value(ctx, comparator);
    let arr = alloc_ref_array(ctx, n);
    let arr_pin = ctx.pin_native_root(arr);
    let elements = read_value_slice(ctx, &handles, &elements);
    let arr = ctx.read_native_pin(arr_pin, arr);
    for (i, v) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    // Element pins (if any), then the comparator's, precede `arr_pin`; release
    // the whole batch from the earliest handle (`usize::MAX` = not pinned).
    let unpin_from = base.min(cmp_pin).min(arr_pin);
    // `--real-jdk` (wave 9, backlog collections w8b-1): a synthetic stream
    // reaches here (the `StackWalker.walk` stream is one), and so does a real
    // pipeline whose `spliterator()` answered null. Build through the JDK's
    // own `Spliterators.spliterator(Object[], int)` -- a real
    // `Spliterators$ArraySpliterator`, which adds SIZED | SUBSIZED itself --
    // instead of minting an instance of the `Spliterator` INTERFACE, which
    // only the interface-triple shims can serve (an `AbstractMethodError`
    // whenever `CRATONVM_SPLITERATOR_IFACE_SHIM_STUB` drops them).
    // `CRATONVM_SPLITERATOR_JDK_FALLBACK=0` restores the interface instance.
    if jdk_fallback {
        let chars = facts
            .map(|(c, _)| c & !SPL_PRIV_MASK)
            .unwrap_or(SPL_LIST_DEFAULT)
            & !(SPL_SIZED | SPL_SUBSIZED);
        let r = ctx.invoke(
            "java/util/Spliterators",
            "spliterator",
            "([Ljava/lang/Object;I)Ljava/util/Spliterator;",
            &[Value::Object(Some(arr)), Value::Int(chars)],
        );
        ctx.unpin_native_roots(unpin_from);
        return r;
    }
    if let Some((raw, head)) = facts {
        // A stage with an intermediate op is a sequential
        // `WrappingSpliterator`: no split, and its SOURCE's estimate when it
        // is not SIZED. A source stage carries its own split bits. A parallel
        // pipeline's `WrappingSpliterator` splits its source instead.
        let comparator = read_pinned_elem(ctx, cmp_pin, comparator);
        let wrapped = raw & SPL_PRIV_WRAPPED != 0;
        let mut chars = raw & !SPL_PRIV_WRAPPED;
        let mut fixed_est = None;
        if wrapped {
            if !parallel {
                chars |= SPL_PRIV_NO_SPLIT;
            }
            if chars & SPL_SIZED == 0 {
                fixed_est = head.or(lazy_source_estimate);
            }
        }
        // Round 17 wave 2 (lane streams2): a described stage whose public
        // characteristics are 0 keeps a non-zero slot, which
        // `spl_raw_bits` would otherwise read as the list default.
        if chars & !SPL_PRIV_MASK == 0 && stream_zero_facts_enabled() {
            chars |= SPL_PRIV_DESCRIBED;
        }
        let r = alloc_facts_spliterator(ctx, arr, chars, comparator, fixed_est);
        ctx.unpin_native_roots(unpin_from);
        return Ok(Some(Value::Object(Some(r?))));
    }
    let spl = match try_alloc_synthetic(ctx, "java/util/Spliterator", 3) {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(unpin_from);
            return Err(e);
        }
    };
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(n as i32));
    ctx.unpin_native_roots(unpin_from);
    Ok(Some(Value::Object(Some(spl))))
}

/// `CRATONVM_SPLITERATOR_JDK_FALLBACK` (default on; `=0` is the kill switch):
/// under `--real-jdk`, the snapshot fallbacks of `Stream.spliterator()` and
/// `CopyOnWriteArraySet.spliterator()` build a real
/// `Spliterators$ArraySpliterator` through `Spliterators.spliterator(Object[],
/// int)` instead of an instance of the `java/util/Spliterator` interface, and
/// a real pipeline's own `spliterator()` failure is raised rather than
/// materialised around (JIT round 16 wave 9, lane spliter; backlog
/// collections w8b-1). Read once per call.
pub(crate) fn spliterator_jdk_fallback_enabled() -> bool {
    true
}

/// `CRATONVM_COMPAT_REAL_PIPELINE_SPLITERATOR` -- default ON, `=0` restores the
/// round-16 snapshot. JIT round 17 wave 1, lane streams
/// (`r17-compatible-stream-residuals-FIXED-20261005.md`).
///
/// Under `--compatible` the stream natives also receive REAL pipeline stages
/// (`IntStream.of(int...)` and a collection's `Collection.stream()` default
/// build `IntPipeline$Head` / `ReferencePipeline$Head`, and `boxed()` /
/// `map()` over them may run the JDK's own body). Their `spliterator()`
/// answered a materialised snapshot whose characteristics, estimate and split
/// policy were guessed from `depth` alone ([`real_pipeline_spl_facts`]: the
/// list default for every wrapped stage, a splitting 3-field snapshot for
/// every head), so a real head over a `HashSet` reported `0x4050` where
/// HotSpot reports `0x41`, and every real stage over an infinite source was
/// drained into an array. `--real-jdk` already runs
/// `AbstractPipeline.spliterator()` itself (wave 9); with the switch on,
/// `--compatible` does the same for an `AbstractPipeline` receiver, and the
/// primitive `spliterator()` natives in `native-builtins` run
/// `IntPipeline` / `LongPipeline` / `DoublePipeline.spliterator()`. Read
/// live, per call.
pub(crate) fn compat_real_pipeline_spliterator_enabled() -> bool {
    true
}

/// `CRATONVM_STREAM_LAZY_HEAD_SPLITERATOR` -- default ON, `=0` restores the
/// drained snapshot. JIT round 17 wave 1, lane streams. JDK 25
/// `AbstractPipeline.spliterator()` on the source stage itself answers the
/// source spliterator (after `linkedOrConsumed`), so
/// `list.stream().spliterator()` is the list's own `ArrayListSpliterator`,
/// with its characteristics, estimate and split policy, and it is lazy.
/// [`native_stream_spliterator`] drained the lazy carrier's spliterator
/// into a 3-field snapshot reporting the list default (an infinite source
/// never returned). Read live, per call.
pub(crate) fn lazy_head_spliterator_enabled() -> bool {
    true
}

/// Whether a `spliterator()` native should hand `stream` to the JDK's own
/// pipeline body: `stream` is a REAL `java.util.stream.AbstractPipeline`
/// stage ([`real_pipeline_shape`]) and
/// [`compat_real_pipeline_spliterator_enabled`] is on. Pure reads; `false`
/// for every synthetic carrier.
pub fn real_pipeline_spliterator_delegates(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    compat_real_pipeline_spliterator_enabled() && real_pipeline_shape(ctx, stream).is_some()
}

// -- Source methods --

/// `ORDERED | SIZED | SUBSIZED | IMMUTABLE`: `Stream.of(t)`'s
/// `Streams.StreamBuilderImpl` and `Stream.of(t...)`'s `Arrays.spliterator`.
pub(crate) const SPL_IMMUTABLE_ARRAY: i32 = SPL_ORDERED | SPL_SIZED | SPL_SUBSIZED | SPL_IMMUTABLE;
/// `SIZED | SUBSIZED`: `Spliterators.emptySpliterator()`, behind every
/// `*Stream.empty()` (and an empty primitive `range`).
pub(crate) const SPL_EMPTY: i32 = SPL_SIZED | SPL_SUBSIZED;
/// `Streams.RangeIntSpliterator` / `RangeLongSpliterator`.
pub(crate) const SPL_RANGE: i32 = SPL_ORDERED
    | SPL_SIZED
    | SPL_SUBSIZED
    | SPL_IMMUTABLE
    | SPL_NONNULL
    | SPL_DISTINCT
    | SPL_SORTED;

/// `r` (a stream a SOURCE native built) with its source facts recorded. No
/// allocation between the build and the store.
pub(crate) fn with_source_spl_facts(
    ctx: &mut dyn NativeContext,
    r: MethodCallResult,
    chars: i32,
) -> MethodCallResult {
    if let Ok(Some(Value::Object(Some(s)))) = &r {
        stream_set_source_spl_facts(ctx, *s, chars, Value::Object(None));
    }
    r
}

/// `r` (a stream an intermediate-op native built) with the facts
/// [`stream_derive_spl_facts`] computed from its source before the build. No
/// allocation between the build and the store.
pub(crate) fn with_derived_spl_facts(
    ctx: &mut dyn NativeContext,
    r: MethodCallResult,
    facts: Option<i64>,
) -> MethodCallResult {
    if let (Ok(Some(Value::Object(Some(s)))), Some(packed)) = (&r, facts) {
        stream_set_spl_facts(ctx, *s, packed);
    }
    r
}

pub(crate) fn native_stream_of_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.first().copied().unwrap_or(Value::Object(None));
    let r = make_stream(ctx, &[elem]);
    with_source_spl_facts(ctx, r, SPL_IMMUTABLE_ARRAY)
}

pub(crate) fn native_stream_of_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return make_stream(ctx, &[]),
    };
    let len = ctx.array_length(arr);
    let elems: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
    let r = make_stream(ctx, &elems);
    with_source_spl_facts(ctx, r, SPL_IMMUTABLE_ARRAY)
}

pub(crate) fn native_stream_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let r = make_stream(ctx, &[]);
    with_source_spl_facts(ctx, r, SPL_EMPTY)
}

/// Bounded variant of [`stream_elements`], used ONLY by [`native_stream_concat`].
///
/// FMT-STREAMS-CCE: `Stream.concat(a, b)` used to call the plain
/// `stream_elements` on both operands, which for a REAL (non-synthetic)
/// pipeline operand materialises via the real bytecode `toArray()` — entirely
/// unbounded. `random.longs(min, max)` (JDK's 2-arg `Random.longs` overload,
/// internally `longs(Long.MAX_VALUE, min, max)`) produces an
/// effectively-infinite `RandomLongsSpliterator`; piping it through
/// `.mapToObj(...)` and into `Stream.concat(Stream.of(x), ...)` made
/// `concat` try to drain that infinite pipeline to completion via `toArray()`
/// BEFORE the caller's `.limit(10)` (applied to the CONCAT RESULT, not to
/// either operand) ever got a chance to short-circuit anything. The JDK's own
/// `SpinedBuffer` grew without bound and eventually attempted a
/// 268435456-element (2^28) array allocation, throwing `OutOfMemoryError:
/// Java heap space` (Spring's
/// `format.datetime.standard.InstantFormatterTests.RandomInstantProvider`,
/// which does exactly this pattern; HotSpot never reaches `toArray()` at all
/// here because its whole pipeline is lazy end-to-end).
///
/// We can't make CratonVM's synthetic concat stream fully lazy — `concat`
/// still eagerly builds a materialised result — but we CAN cap the damage.
/// For a real pipeline this obtains the REAL spliterator by calling
/// `AbstractPipeline.spliterator()` via `invoke_special` (invokespecial
/// semantics: exact resolved method on the given class, no virtual retarget),
/// which bypasses our own `native_stream_spliterator` override (registered on
/// the concrete `java/util/stream/ReferencePipeline` class name, so a normal
/// virtual dispatch would hit it and recurse back into `stream_elements`/
/// `toArray()` — the very thing we're trying to avoid). The genuine spliterator
/// is then drained through the existing bounded `tryAdvance` loop
/// (`drain_spliterator_to_array`, capped at 1,000,000 elements) instead of the
/// unbounded native `toArray()`. This is byte-identical to the previous
/// behavior for every FINITE real pipeline (same elements, same encounter
/// order) and turns an unbounded-allocation OOM into a bounded, harmless
/// materialization for the rare unbounded case — matching the bounded-cap
/// precedent already used for real spliterators elsewhere (e.g.
/// `StreamSupport.stream(realSpliterator, false)`).
pub(crate) fn stream_elements_concat_bounded(
    ctx: &mut dyn NativeContext,
    stream: &mut ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    // Synthetic streams (including lazy-spliterator-backed ones, which are
    // already bounded via `drain_spliterator_to_array`'s 1M cap inside
    // `materialize_lazy_stream`) are unaffected by this bug — delegate
    // straight to the ordinary path.
    let class_id = ctx.class_id_of_object(*stream);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    if is_synthetic_stream(&class_name) {
        return stream_elements(ctx, stream);
    }
    let stream_pin = ctx.pin_native_root(*stream);
    let result: Result<Vec<Value>, MethodCallFailed> = match ctx.invoke_special(
        "java/util/stream/AbstractPipeline",
        "spliterator",
        "()Ljava/util/Spliterator;",
        &[Value::Object(Some(*stream))],
    ) {
        Ok(Some(Value::Object(Some(spl)))) => {
            // Smaller cap than the shared 1,000,000 default: this path exists
            // only to survive an unbounded/infinite operand without OOMing,
            // and no legitimate finite `concat` operand needs anywhere near
            // this many elements. Keeps the bounded fallback fast.
            const CONCAT_SAFETY_CAP: usize = 20_000;
            // Not `?`: a failed drain must still release `stream_pin` below.
            drain_spliterator_to_array_capped(ctx, spl, CONCAT_SAFETY_CAP).map(|arr| {
                let len = ctx.array_length(arr);
                (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
            })
        }
        // Not an AbstractPipeline subclass (or spliterator() unavailable via
        // invokespecial for some other reason) — stay defensive and fall back
        // to the original unbounded toArray() path so we never regress a
        // real, finite, non-pipeline Stream implementation.
        _ => {
            *stream = ctx.read_native_pin(stream_pin, *stream);
            match ctx.invoke_virtual(*stream, "toArray", "()[Ljava/lang/Object;", &[]) {
                Ok(Some(Value::Object(Some(arr)))) => {
                    let len = ctx.array_length(arr);
                    Ok((0..len).map(|i| ctx.get_array_element(arr, i)).collect())
                }
                Ok(_) => Ok(Vec::new()),
                Err(e) => Err(e),
            }
        }
    };
    // Write the refresh back: the `spliterator` drain above runs real bytecode
    // and `native_stream_concat` reuses its own receiver for operand B.
    *stream = ctx.read_native_pin(stream_pin, *stream);
    ctx.unpin_native_roots(stream_pin);
    result
}

pub(crate) fn native_stream_concat(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a_ref = match args.first() {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    let b_ref = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    // cceres3 (SE_005 store-canary capture: native_stream_concat ->
    // make_stream -> set_array_element stored an already-evacuated element):
    // BOTH drains below run arbitrary Java (a real pipeline's spliterator),
    // and `make_stream`/`alloc_ref_array` allocate — every raw local here
    // spans GC-capable calls. Pin the two stream heads for the whole native,
    // pin A's drained elements across B's drain and re-read them through the
    // pins before combining, and redo the close-handler merge with the same
    // discipline (its reads spanned B's drain and its stores spanned a fresh
    // allocation).
    let first_pin = match (a_ref, b_ref) {
        (Some(a), _) => {
            let p = ctx.pin_native_root(a);
            if let Some(b) = b_ref {
                ctx.pin_native_root(b);
            }
            Some((p, p, p + if b_ref.is_some() { 1 } else { 0 }))
        }
        (None, Some(b)) => {
            let p = ctx.pin_native_root(b);
            Some((p, usize::MAX, p))
        }
        (None, None) => None,
    };
    let (unpin_base, a_pin, b_pin) = match first_pin {
        Some((base, ap, bp)) => (Some(base), ap, bp),
        None => (None, usize::MAX, usize::MAX),
    };
    macro_rules! bail {
        ($e:expr) => {{
            if let Some(base) = unpin_base {
                ctx.unpin_native_roots(base);
            }
            return Err($e);
        }};
    }
    // (Sequential `&mut` borrows — `stream_elements` is now `&mut`, so avoid the
    // closure form which would capture `ctx` mutably twice.)
    //
    // FMT-STREAMS-CCE: use the bounded variant here (see doc comment on
    // `stream_elements_concat_bounded`) — `concat` is the one native that
    // must tolerate an unbounded/infinite real-pipeline operand.
    let a = match a_ref {
        Some(r) => {
            let mut r = ctx.read_native_pin(a_pin, r);
            match stream_elements_concat_bounded(ctx, &mut r) {
                Ok(v) => v,
                Err(e) => bail!(e),
            }
        }
        None => Vec::new(),
    };
    // Pin A's elements across B's drain, then re-read them through the pins.
    let (a_elem_base, a_handles) = pin_value_slice(ctx, &a);
    let b = match b_ref {
        Some(r) => {
            let mut r = ctx.read_native_pin(b_pin, r);
            match stream_elements_concat_bounded(ctx, &mut r) {
                Ok(v) => v,
                Err(e) => {
                    if a_elem_base != usize::MAX {
                        ctx.unpin_native_roots(a_elem_base);
                    }
                    bail!(e)
                }
            }
        }
        None => Vec::new(),
    };
    let mut combined: Vec<Value> = a
        .iter()
        .enumerate()
        .map(|(i, v)| read_pinned_elem(ctx, a_handles[i], *v))
        .collect();
    combined.extend(b);
    if a_elem_base != usize::MAX {
        ctx.unpin_native_roots(a_elem_base);
    }
    // `make_stream` pins `combined` itself at entry; nothing GC-capable runs
    // between the unpin above and that entry.
    let r = match make_stream(ctx, &combined) {
        Ok(r) => r,
        Err(e) => bail!(e),
    };
    let r = if let Some(Value::Object(Some(dst))) = &r {
        // JDK contract: closing the concatenated stream closes BOTH inputs.
        // Only merge handlers from synthetic inputs (a foreign Stream impl keeps
        // unrelated data in field 1; its own close() owns its handlers).
        let mut dst = *dst;
        let dst_pin = ctx.pin_native_root(dst);
        let mut handlers: Vec<Value> = Vec::new();
        for (src, pin) in [(a_ref, a_pin), (b_ref, b_pin)] {
            let Some(src) = src else { continue };
            let src = ctx.read_native_pin(pin, src);
            let cn = ctx
                .class_name_of_id(ctx.class_id_of_object(src))
                .unwrap_or_default();
            if !is_synthetic_stream(&cn) {
                continue;
            }
            // W1 fix: skip 1-field sources with no close-handler slot.
            if ctx.object_num_fields(src) <= STREAM_FIELD_CLOSE_HANDLERS {
                continue;
            }
            // Round 18 wave 11: the operand pipeline's handlers live on its
            // close root ([`stream_close_root`]).
            let src = stream_close_root(&*ctx, src);
            if let Value::Object(Some(arr)) = ctx.get_field(src, STREAM_FIELD_CLOSE_HANDLERS) {
                let n = ctx.array_length(arr);
                for i in 0..n {
                    handlers.push(ctx.get_array_element(arr, i));
                }
            }
        }
        if !handlers.is_empty() {
            let (h_base, h_handles) = pin_value_slice(ctx, &handlers);
            let arr = alloc_ref_array(ctx, handlers.len());
            let arr_pin = ctx.pin_native_root(arr);
            for (i, h) in handlers.iter().enumerate() {
                let arr = ctx.read_native_pin(arr_pin, arr);
                let h = read_pinned_elem(ctx, h_handles[i], *h);
                ctx.set_array_element(arr, i, h);
            }
            dst = ctx.read_native_pin(dst_pin, dst);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(dst, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(Some(arr)));
            let _ = h_base;
        }
        let dst = ctx.read_native_pin(dst_pin, dst);
        Some(Value::Object(Some(dst)))
    } else {
        r
    };
    if let Some(base) = unpin_base {
        ctx.unpin_native_roots(base);
    }
    Ok(r)
}

pub(crate) fn native_al_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = ksv_route(ctx, args, native_ksv_stream) {
        return r;
    }
    if let Some(r) = vc_route(ctx, args, native_al_stream) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // GC-safety: `this` is a plain Rust copy of the interpreter's args slice,
    // invisible to the collector, and may already be stale by the time this
    // native runs. `resync_values_view`'s first read (values_view_source ->
    // al_state -> heap_kind_of) dereferences `this` immediately -- pin and
    // refresh before handing it off rather than trusting the incoming copy.
    // `resync_values_view` itself now returns the (possibly further
    // refreshed) list -- it allocates internally for an actual values()-view
    // resync and can move `this` again via its own allocations. Confirmed
    // live via CRATONVM_DBG_STALE_OBJREF under concurrent stream map/flatMap
    // stress at -Xmx32m; see
    // stream-arraylist-gc-pressure-heap-corruption-FIXED.md.
    let this_pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(this_pin, this);
    let this = resync_values_view(ctx, this)?;
    // `al_state` returns an empty ArrayList (Some, size 0) — not None — for a
    // non-ArrayList collection like RegularEnumSet, so the old `Some` branch
    // produced an empty stream for `enumSet.stream()`. Use the generic helper,
    // whose ArrayList heuristics keep the fast path and whose iterator fallback
    // walks EnumSet/TreeSet/foreign collections. (That iterator fallback only
    // became real on 2026-08-01 — see `al_or_collection_elements`; until then
    // `stream()` over an unmodelled foreign collection was silently empty.)
    // As for HashSet.stream(), never carry an unrooted Rust snapshot across
    // stream/array allocation: a moving GC may forward every list element and
    // turn the stale refs into unrelated Objects. This is particularly visible
    // while SmallRye builds nested config mappings from ArrayList-backed source
    // lists. Count before allocation, then re-snapshot from the pinned list.
    let this = ctx.read_native_pin(this_pin, this);
    let len = al_or_collection_elements(ctx, this)?.len();
    // Wave 14 (lane compat2, SPL9-2): a hash map's `values()` view streams
    // over its own spliterator (`0x40` for a `HashMap`). A plain `int`, read
    // before the allocations below.
    let this = ctx.read_native_pin(this_pin, this);
    let source_chars = al_values_view_stream_source_chars(&*ctx, this);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS)?;
    let stream_pin = ctx.pin_native_root(stream);
    let arr = alloc_ref_array(ctx, len);
    let arr_pin = ctx.pin_native_root(arr);
    let this = ctx.read_native_pin(this_pin, this);
    let elements = al_or_collection_elements(ctx, this)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    for (i, element) in elements.iter().enumerate().take(len) {
        ctx.set_array_element(arr, i, *element);
    }
    let stream = ctx.read_native_pin(stream_pin, stream);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    if let Some(chars) = source_chars {
        stream_set_source_spl_facts(ctx, stream, chars, Value::Object(None));
    }
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(stream_pin);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

/// The source facts [`native_al_stream`] records for a map's `values()` (or
/// list-shaped entry) view: the cell its own `spliterator()` answers
/// ([`view_spliterator_characteristics`]). `None` for an ordinary list (it
/// keeps the list default, which is right), for a `TreeMap` view (a tree
/// spliterator, not modelled by this snapshot) and under
/// [`stream_source_facts_enabled`]'s kill switch. Pure reads.
pub(crate) fn al_values_view_stream_source_chars(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if !stream_source_facts_enabled() {
        return None;
    }
    let src = values_view_source(ctx, this)?;
    if is_tree_map_receiver(ctx, src) {
        return None;
    }
    let kind = if al_view_holds_entries(ctx, this) {
        SplViewKind::Entries
    } else {
        SplViewKind::Values
    };
    Some(view_spliterator_characteristics(ctx, src, kind))
}

/// `spliterator()` for a [`MAP_VIEW_CARRIERS`] (a map's `values()` view,
/// ArrayList-layout under its own class — e.g. `TreeMap$Values`,
/// `HashMap$Values`).
///
/// The `Set`-shaped twin of this gap (`keySet()`/`entrySet()` views) was
/// already covered by [`native_hs_spliterator`], registered alongside
/// `native_hs_stream` in `register_set_view_carrier_natives`. This one was
/// missing its `Collection`-shaped equivalent: `register_map_view_carrier_natives`
/// registered `stream` (`native_al_stream`, above) but never `spliterator`, so
/// `StreamSupport.stream(view.spliterator(), false)` — as opposed to
/// `view.stream()` — fell through to the real inherited `AbstractCollection
/// .spliterator()` bytecode, which this ArrayList-layout carrier's fields
/// don't back correctly and which silently produced an EMPTY spliterator
/// rather than throwing. `view.stream()` was unaffected (it has its own
/// native, above) — only the explicit `.spliterator()` call broke. First
/// surfaced as `Hibernate boot metadata: `Database.getNamespaces()`
/// (`TreeMap$Values`) streamed via `StreamSupport.stream(iterable
/// .spliterator(), false).flatMap(...)` (e.g. `CommentsTest`,
/// `SchemaUpdateTest`) silently finding zero tables/namespaces.
///
/// Mirrors [`native_hs_spliterator`]'s synthetic `java/util/Spliterator`
/// (fields: backing array, cursor, limit) built from a snapshot — reuses
/// `al_or_collection_elements`, the same element-reading helper
/// `native_al_stream` already relies on, so any collection `stream()` gets
/// right, `spliterator()` now gets right too.
pub(crate) fn native_al_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return empty_synthetic_spliterator(ctx);
        }
    };
    let this_pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(this_pin, this);
    let this = resync_values_view(ctx, this)?;
    let this = ctx.read_native_pin(this_pin, this);
    let len = al_or_collection_elements(ctx, this)?.len();
    let arr = alloc_ref_array(ctx, len);
    let arr_pin = ctx.pin_native_root(arr);
    let this = ctx.read_native_pin(this_pin, this);
    let elements = al_or_collection_elements(ctx, this)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    for (i, element) in elements.iter().enumerate().take(len) {
        ctx.set_array_element(arr, i, *element);
    }
    // A VIEW answers its source family's cell; an ordinary list keeps the
    // three-field shape and [`SPL_LIST_DEFAULT`]. Every ArrayList-shaped map
    // view used to take the list default, which is why `HashMap.values()`
    // claimed ORDERED and SUBSIZED it does not have and `TreeMap.entrySet()`
    // claimed neither DISTINCT nor SORTED that it does -- four of the twelve
    // cells `apps/probes/UtilCoverageSweep` found wrong.
    let this = ctx.read_native_pin(this_pin, this);
    // Wave 9 (lane spliter): a `TreeMap` view's spliterator is a tree
    // spliterator on HotSpot -- `TreeMap.values()` declines to split two
    // entries (`R16CollectionsSpliter`) -- and `entrySet()`'s comparator is
    // `Map.Entry.comparingByKey(..)`, never null. A sub-map's (`headMap`,
    // `descendingMap`) views inherit `AbstractCollection`'s
    // `Spliterators.spliterator(this, ..)` instead: SIZED | SUBSIZED (plus
    // DISTINCT for the entry set), no comparator. No allocation between these
    // reads and `alloc_facts_spliterator`'s own pins.
    if spliterator_facts_enabled() {
        if let Some(src) = values_view_source(&*ctx, this) {
            if is_tree_map_receiver(&*ctx, src) {
                let entries = al_view_holds_entries(&*ctx, this);
                let (chars, comparator) = if tm_view_spec(&*ctx, src).is_some() {
                    let set_bit = if entries { SPL_DISTINCT } else { 0 };
                    (SPL_SIZED | SPL_SUBSIZED | set_bit, Value::Object(None))
                } else if entries {
                    (
                        SPL_TREE_SET | SPL_PRIV_TREE_SPLIT | SPL_PRIV_ENTRY_CMP,
                        tm_get_slot(&*ctx, src, TM_FIELD_COMPARATOR),
                    )
                } else {
                    (SPL_TREE_VALUES | SPL_PRIV_TREE_SPLIT, Value::Object(None))
                };
                let arr = ctx.read_native_pin(arr_pin, arr);
                let r = alloc_facts_spliterator(ctx, arr, chars, comparator, None);
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(r?))));
            }
        }
    }
    let view_chars = values_view_source(&*ctx, this).map(|src| {
        let kind = if al_view_holds_entries(&*ctx, this) {
            SplViewKind::Entries
        } else {
            SplViewKind::Values
        };
        view_spliterator_characteristics(&*ctx, src, kind)
    });
    // Round 17 wave 1 (lane streams): under `--real-jdk` this body is reached
    // only if a carrier's own `Collection.spliterator()` default loses to the
    // registration (`Hashtable$ValueCollection` declares none). Answer with a
    // real `Spliterators$ArraySpliterator`, as `native_hs_spliterator` does,
    // never an instance of the `Spliterator` INTERFACE, which
    // `CRATONVM_SPLITERATOR_IFACE_SHIM_STUB` would leave unserved there.
    // `CRATONVM_SPLITERATOR_JDK_FALLBACK=0` restores the interface instance.
    if ctx.is_jdk_only() && spliterator_jdk_fallback_enabled() {
        let chars = view_chars.unwrap_or(0) & !(SPL_PRIV_MASK | SPL_SIZED | SPL_SUBSIZED);
        let arr = ctx.read_native_pin(arr_pin, arr);
        let r = ctx.invoke(
            "java/util/Spliterators",
            "spliterator",
            "([Ljava/lang/Object;I)Ljava/util/Spliterator;",
            &[Value::Object(Some(arr)), Value::Int(chars)],
        );
        ctx.unpin_native_roots(this_pin);
        return r;
    }
    let n_fields = if view_chars.is_some() { 4 } else { 3 };
    let spl = try_alloc_synthetic(ctx, "java/util/Spliterator", n_fields)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(len as i32));
    if let Some(chars) = view_chars {
        ctx.set_field(spl, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(spl))))
}

pub(crate) fn native_hs_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    resync_view_set(ctx, &mut this)?;
    // Round 17 wave 3 (lane streams3): a real `CopyOnWriteArraySet` runs its
    // own `stream()` resolution, the `Collection.stream()` default
    // ([`cow_set_stream_jdk_body_enabled`]). `None` = not a real one.
    if cow_set_stream_jdk_body_enabled() {
        let routed = [Value::Object(Some(this))];
        if let Some(r) = cow_set_route(ctx, &routed, "stream", "()Ljava/util/stream/Stream;") {
            return r;
        }
    }
    // A real `CopyOnWriteArraySet` has no backing map to walk. Under the kill
    // switch above `stream()` is the one method of its surface
    // [`cow_set_route`] does not delegate — see [`cow_set_elements`] — so build
    // the carrier from its elements.
    if let Some(elements) = cow_set_elements(ctx, this) {
        return make_stream(ctx, &elements);
    }
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return make_stream(ctx, &[]),
    };
    // `make_stream` allocates both the stream object and its backing array.
    // Collecting object references before those allocations leaves a moving GC
    // free to forward the backing map and every key, so the stale snapshot can
    // later surface as unrelated `java.lang.Object` values. SmallRye's
    // PropertyMappingInterceptor hits this through `LinkedHashSet.stream()`
    // while enumerating config property names. Count first, allocate while the
    // backing map is pinned, then collect the live keys and store them without
    // another allocation.
    let backing_pin = ctx.pin_native_root(backing);
    let len = hs_view_elements(ctx, this, backing)?.len();
    // Wave 14 (lane compat2, SPL9-2): `set.stream()` is
    // `StreamSupport.stream(spliterator(), false)`, so its spliterator is the
    // set's own (`0x41` for a `HashSet`), and a later stage derives from that,
    // not from the list default. A plain `int`, read before the allocations.
    let source_chars = hs_stream_source_chars(&*ctx, this, backing);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS)?;
    let stream_pin = ctx.pin_native_root(stream);
    let arr = alloc_ref_array(ctx, len);
    let arr_pin = ctx.pin_native_root(arr);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let keys = hs_view_elements(ctx, this, backing)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    for (i, key) in keys.iter().enumerate().take(len) {
        ctx.set_array_element(arr, i, *key);
    }
    let stream = ctx.read_native_pin(stream_pin, stream);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    if let Some(chars) = source_chars {
        stream_set_source_spl_facts(ctx, stream, chars, Value::Object(None));
    }
    ctx.unpin_native_roots(backing_pin);
    ctx.unpin_native_roots(stream_pin);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

/// The source facts [`native_hs_stream`] records for `this` (backing map
/// `backing`): [`hs_spliterator_characteristics`], or `None` under
/// [`stream_source_facts_enabled`]'s kill switch. A view over a `TreeMap`
/// records nothing either: its HotSpot spliterator is a tree spliterator
/// with a comparator, which a hash-set snapshot does not model (the stream
/// keeps the list default it always had). Pure reads.
pub(crate) fn hs_stream_source_chars(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    backing: ObjectRef,
) -> Option<i32> {
    if !stream_source_facts_enabled() {
        return None;
    }
    if view_backing_source(ctx, backing).is_some_and(|src| is_tree_map_receiver(ctx, src)) {
        return None;
    }
    Some(hs_spliterator_characteristics(ctx, this, backing))
}

// TreeSet.stream() — snapshot of sorted elements (also serves TreeMap.keySet()
// since native_tm_key_set returns a synthetic TreeSet). Real JDK's
// TreeSet.spliterator() goes through TreeMap.keySpliteratorFor(), which requires
// the JDK's `root`/`size` red-black tree fields populated by put(); our
// synthetic layout does not provide those, so without this override the stream
// is empty (observed: `m.keySet().stream().count()` returned 0 for a 3-entry
// TreeMap, which broke Keycloak FeatureOptions.<clinit>).
pub(crate) fn native_ts_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ts_refresh_real_mirrors(ctx, args);
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let this = resync_ts_view(ctx, this)?;
    let this_pin = ctx.pin_native_root(this);
    let (_, size, _) = ts_state(ctx, this);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS)?;
    let stream_pin = ctx.pin_native_root(stream);
    let arr = alloc_ref_array(ctx, size as usize);
    let arr_pin = ctx.pin_native_root(arr);
    let this = ctx.read_native_pin(this_pin, this);
    let (data_opt, live_size, _) = ts_state(ctx, this);
    let arr = ctx.read_native_pin(arr_pin, arr);
    if let Some(data) = data_opt {
        for i in 0..(live_size as usize).min(size as usize) {
            ctx.set_array_element(arr, i, ctx.get_array_element(data, i));
        }
    }
    let stream = ctx.read_native_pin(stream_pin, stream);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    // Wave 9 (lane spliter): `treeSet.stream().spliterator()` is the set's own
    // spliterator on HotSpot (0x55, its comparator, a tree split). No
    // allocation between these reads and the stores.
    let this = ctx.read_native_pin(this_pin, this);
    let (chars, comparator) = ts_spliterator_facts(&*ctx, this);
    stream_set_source_spl_facts(ctx, stream, chars, comparator);
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(stream_pin);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

/// The characteristics HotSpot's `TreeMap.keySpliteratorFor(m)` gives the
/// spliterator of a `TreeSet` / `TreeMap$KeySet` carrier this crate minted.
///
/// ```text
///   receiver                                   HotSpot class               chars  estimateSize
///   plain TreeSet, tm.keySet(), navigableKeySet TreeMap$KeySpliterator      85     exact
///   tm.descendingKeySet()                       TreeMap$DescendingKeySpl.   81     exact
///   a bounded view (headSet, headMap.keySet ..) NavigableSubMap$SubMapKeyIt 21     Long.MAX
///   a bounded view of a descending map          ..$DescendingSubMapKeyIt    17     Long.MAX
/// ```
///
/// SORTED | DISTINCT are what the list default does not carry, and without
/// SORTED `Spliterator.getComparator()`'s own default body throws — which is
/// also HotSpot's answer for the two descending rows.
///
/// # Why the view's SOURCE decides, not merely its presence (SBR-W3 strictgate)
///
/// `b762ca749` (2026-09-18) made every carrier with a [`ts_view_source`] marker
/// answer the unsized 21 shape, to fix the range views. But `tm.keySet()` is a
/// marked view too — its source is the full `TreeMap`, stashed so the view
/// writes through — and it had answered HotSpot's 85 until then. Measured in
/// `--compatible` (`apps/probes/UtilCoverageSweep` rows 10, 73, 74, and
/// `SgProbe` on the int1 control): `keySet()`, `navigableKeySet()` and
/// `descendingKeySet()` answered 21 / `Long.MAX_VALUE` where HotSpot gives
/// 85 / 81 and the exact size. The strict corpus has diverged on those rows
/// since.
///
/// So a marked view is classified by what stands behind it: a range spec on
/// the set itself (the `TreeSet` range wrappers register one), or — for a
/// keySet over a map — the map's own spec: none (the full map), unbounded
/// and descending (`descendingMap()`), or bounded. A set-shaped source with no
/// spec (`TreeSet.descendingSet()`) keeps the pre-existing unsized answer; it
/// is not a row any probe measures, and guessing it would be a change without
/// a measurement.
pub(crate) fn ts_spliterator_characteristics(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    const UNSIZED_ASC: i32 = SPL_DISTINCT | SPL_SORTED | SPL_ORDERED;
    const UNSIZED_DESC: i32 = SPL_DISTINCT | SPL_ORDERED;
    const SIZED_DESC: i32 = SPL_SIZED | SPL_DISTINCT | SPL_ORDERED;
    let bounded_chars = |spec: &TmViewSpec| -> i32 {
        match (spec.lo_bounded || spec.hi_bounded, spec.descending) {
            (true, false) => UNSIZED_ASC,
            (true, true) => UNSIZED_DESC,
            (false, false) => SPL_TREE_SET,
            (false, true) => SIZED_DESC,
        }
    };
    let source = match ts_view_source(ctx, this) {
        Some(s) => s,
        None => return SPL_TREE_SET,
    };
    if let Some(spec) = tm_view_spec(ctx, this) {
        return bounded_chars(&spec);
    }
    if is_tree_set_shaped(ctx, source) {
        return UNSIZED_ASC;
    }
    match tm_view_spec(ctx, source) {
        Some(spec) => bounded_chars(&spec),
        None => SPL_TREE_SET,
    }
}

/// `TreeSet.spliterator()` / `TreeMap$KeySet.spliterator()` — the same sorted
/// snapshot [`native_ts_stream`] takes, in the three-field synthetic
/// `java/util/Spliterator` shape [`native_hs_spliterator`] produces.
///
/// The comment on `native_ts_stream` above explains why `stream()` needs an
/// override; `spliterator()` needs it for the same reason and did not have one.
/// Both classes DECLARE `spliterator()`, so the receiver-has-own-bytecode rule
/// ran the JDK body — `TreeMap.keySpliteratorFor(m)` — against fields our
/// layout keeps in the `ts_array_table` side-table instead, and every caller
/// got `NullPointerException: Cannot invoke
/// "java.util.TreeMap$NavigableSubMap.keySpliterator()" because "sm" is null`.
/// `force_native_over_real_jdk_bytecode` already listed `spliterator` for
/// `java/util/TreeMap$KeySet`; as ever, a gate entry is not a registration.
pub(crate) fn native_ts_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let spl = alloc_empty_array_holder(ctx, "java/util/Spliterator", 4)?;
            ctx.set_field(spl, 2, Value::Int(0));
            // SORTED | DISTINCT, which the list default does not carry -- and without SORTED, `getComparator()` throws.
            ctx.set_field(spl, SPL_FIELD_CHARACTERISTICS, Value::Int(SPL_TREE_SET));
            return Ok(Some(Value::Object(Some(spl))));
        }
    };
    let this = resync_ts_view(ctx, this)?;
    let this_pin = ctx.pin_native_root(this);
    let (_, size, _) = ts_state(ctx, this);
    // Allocate first, then RE-read the element table through the pin: the
    // allocation can move it (same count -> allocate -> re-read discipline as
    // `native_hs_to_array`).
    let arr = alloc_ref_array(ctx, size as usize);
    let arr_pin = ctx.pin_native_root(arr);
    let this = ctx.read_native_pin(this_pin, this);
    let (data_opt, live_size, _) = ts_state(ctx, this);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let n = (live_size as usize).min(size as usize);
    if let Some(data) = data_opt {
        for i in 0..n {
            ctx.set_array_element(arr, i, ctx.get_array_element(data, i));
        }
    }
    // Wave 9 (lane spliter): HotSpot's spliterator class for this set, with
    // its comparator and split policy ([`ts_spliterator_facts`]). Read before
    // the allocation below: `this` is only good until then.
    if spliterator_facts_enabled() {
        let (chars, comparator) = ts_spliterator_facts(&*ctx, this);
        let r = alloc_facts_spliterator(ctx, arr, chars, comparator, None);
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(Some(r?))));
    }
    let spl = match try_alloc_synthetic(ctx, "java/util/Spliterator", 4) {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    // The cursor end is the array's own length, so a shrunk-since-allocation
    // table cannot leave trailing nulls inside the reported range.
    ctx.set_field(spl, 2, Value::Int(n as i32));
    let this = ctx.read_native_pin(this_pin, this);
    let chars = ts_spliterator_characteristics(&*ctx, this);
    ctx.set_field(spl, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(spl))))
}

/// Depth bound for [`nav_shape`]'s walk up a chain of navigable views.
pub(crate) const NAV_SHAPE_MAX_DEPTH: usize = 8;

/// The iteration direction and boundedness of a native `TreeMap` / `TreeSet`
/// or any view of one, relative to the ROOT collection: `(descending,
/// bounded)`. A view records its IMMEDIATE source and its own direction
/// (`TmViewSpec`), so this folds the chain: each descending step flips the
/// direction, any bound makes the result bounded. A `TreeSet` with a source
/// marker and no spec is a `keySet()` when the source is a map and a
/// `descendingSet()` when it is a set.
pub(crate) fn nav_shape(ctx: &dyn NativeContext, obj: ObjectRef, depth: usize) -> (bool, bool) {
    if depth > NAV_SHAPE_MAX_DEPTH {
        return (false, false);
    }
    if let Some(spec) = tm_view_spec(ctx, obj) {
        let (d, b) = nav_shape(ctx, spec.source, depth + 1);
        return (d ^ spec.descending, b || spec.lo_bounded || spec.hi_bounded);
    }
    if is_tree_map_receiver(ctx, obj) {
        return (false, false);
    }
    match ts_view_source(ctx, obj) {
        Some(src) if is_tree_map_receiver(ctx, src) => nav_shape(ctx, src, depth + 1),
        Some(src) => {
            let (d, b) = nav_shape(ctx, src, depth + 1);
            (!d, b)
        }
        None => (false, false),
    }
}

/// What HotSpot's `spliterator()` reports for the native `TreeSet` (or
/// `TreeMap` key-set view) `this`: the raw characteristics (with the private
/// split bits) and the comparator `getComparator()` answers. MEASURED,
/// `R16SpliterW9` / `R16CollectionsSpliter`, HotSpot 25:
///
/// ```text
///   TreeSet, keySet()                      KeySpliterator             0x55  set comparator  tree split
///   descendingSet(), descendingKeySet()    DescendingKeySpliterator   0x51  throws ISE      tree split
///   headSet/tailSet/subSet, headMap..keys  SubMapKeyIterator          0x15  view comparator never splits
///   ..the same over a descending view      DescendingSubMapKeyIterator 0x11 throws ISE      never splits
/// ```
///
/// The pre-wave-9 rule was "a source marker means a range view", which also
/// caught every `keySet()` and `descendingSet()` (`TreeMap.keySet`: estimate
/// `Long.MAX_VALUE`, 0x15, where HotSpot answers 4, 0x55), and every SORTED
/// answer was a natural-order `null`. Residual: `descendingSet()` of a
/// `descendingSet()` is an unbounded ASCENDING sub-map on HotSpot (0x15) and
/// the full-tree cell here.
pub(crate) fn ts_spliterator_facts(ctx: &dyn NativeContext, this: ObjectRef) -> (i32, Value) {
    let (_, size, comparator) = ts_state(ctx, this);
    let shape = nav_shape(ctx, this, 0);
    let (chars, comparator) = ts_spliterator_shape_facts(shape, comparator);
    // Round 16 wave 13 (lane compatcoll): a whole two-element tree splits
    // when its root is not the element the spliterator walks first -- the
    // first for `KeySpliterator`, the last for `DescendingKeySpliterator`.
    let pair_split = spliterator_tree_pair_root_enabled()
        && size == 2
        && chars & SPL_PRIV_TREE_SPLIT != 0
        && ts_pair_root_is_max(ctx, this) != shape.0;
    if pair_split {
        (chars | SPL_PRIV_PAIR_SPLIT, comparator)
    } else {
        (chars, comparator)
    }
}

/// [`ts_spliterator_facts`]' table: the raw characteristics and comparator
/// for a view of `(descending, bounded)` shape over a set whose comparator is
/// `comparator`.
pub(crate) fn ts_spliterator_shape_facts(shape: (bool, bool), comparator: Value) -> (i32, Value) {
    match shape {
        (false, false) => (SPL_TREE_SET | SPL_PRIV_TREE_SPLIT, comparator),
        (true, false) => (
            SPL_SIZED | SPL_DISTINCT | SPL_ORDERED | SPL_PRIV_TREE_SPLIT,
            Value::Object(None),
        ),
        (false, true) => (
            SPL_DISTINCT | SPL_SORTED | SPL_ORDERED | SPL_PRIV_NO_SPLIT,
            comparator,
        ),
        (true, true) => (
            SPL_DISTINCT | SPL_ORDERED | SPL_PRIV_NO_SPLIT,
            Value::Object(None),
        ),
    }
}

// -- Intermediate operations --

/// Diagnostic-only tripwire for the pin/read-through-handle path shared by
/// `native_stream_filter`/`native_stream_map`/`native_stream_flat_map`. Snapshots
/// each input element's class id right after `pin_value_slice` and compares it
/// against the class id read back via `read_pinned_elem` immediately before the
/// element is dispatched to Java bytecode. A mismatch would mean a stale or
/// incorrectly-forwarded native root slipped through a moving-GC window — the
/// exact failure shape suspected (never confirmed) behind the intermittent
/// `ConfigurationTest::testDatabaseProperties` `ClassCastException`, see
/// keycloak-quarkus-runtime-config-resolution-mismatches.md
/// ("Residual" section). That race stopped reproducing before this canary could
/// be validated against it; kept as a near-zero-overhead tripwire (one class-id
/// comparison per element; no allocation/formatting unless it actually fires,
/// and it reports only once per process) in case it ever resurfaces.
pub(crate) static STREAM_PIN_CANARY_FIRED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[inline]
pub(crate) fn stream_pin_canary_snapshot(ctx: &dyn NativeContext, vals: &[Value]) -> Vec<Option<ClassId>> {
    vals.iter()
        .map(|v| match v {
            Value::Object(Some(o)) => Some(ctx.class_id_of_object(*o)),
            _ => None,
        })
        .collect()
}

#[inline]
pub(crate) fn stream_pin_canary_check(
    ctx: &dyn NativeContext,
    site: &'static str,
    index: usize,
    before: Option<ClassId>,
    after: Value,
) {
    if let (Some(before), Value::Object(Some(o))) = (before, after) {
        let after_cid = ctx.class_id_of_object(o);
        if after_cid != before
            && !STREAM_PIN_CANARY_FIRED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!(
                "[CRATONVM_STREAM_PIN_CANARY] {site}: element {index} class id changed \
                 {before:?} -> {after_cid:?} across pin/read (stale-ref race?)"
            );
        }
    }
}

pub(crate) fn native_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // Use `stream_elements_mut` (rather than the immutable `stream_elements`)
    // so we fall through to `Stream.toArray()` for real-JDK Stream
    // implementations whose field-0 isn't our synthetic backing array. ES's
    // `CliToolProvider.load` runs `ServiceLoader.load(...).spliterator()
    // .stream().filter(name=="server")`; the resulting stream is a real JDK
    // `ReferencePipeline` (not our `java/util/stream/Stream` synthetic) and
    // the immutable helper would return Vec::new() — producing
    // `AssertionError: available names are []` even though
    // `ServiceLoader.iterator()` correctly produced 13 providers.
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let this_pin = ctx.pin_native_root(this);
    let pred_pin = ctx.pin_native_root(predicate);
    match stream_try_defer(ctx, this, LAZY_OP_FILTER, Some(predicate), 0) {
        Ok(Some(s)) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(s));
        }
        Ok(None) => {}
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let mut this = ctx.read_native_pin(this_pin, this);
    let predicate = ctx.read_native_pin(pred_pin, predicate);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    // GC-SAFETY: `test` allocates → moving young GC relocates the predicate and
    // the elements; pin both, re-read each before dispatch, track kept by INDEX,
    // and rebuild the kept list by re-reading from the pins at the end.
    let (_, ehandles) = pin_value_slice(ctx, &elements);
    let elem_cids = stream_pin_canary_snapshot(ctx, &elements);
    let mut kept_idx: Vec<usize> = Vec::new();
    let mut err = None;
    for i in 0..elements.len() {
        let p = ctx.read_native_pin(pred_pin, predicate);
        let e = read_pinned_elem(ctx, ehandles[i], elements[i]);
        stream_pin_canary_check(ctx, "native_stream_filter", i, elem_cids[i], e);
        match ctx.invoke_virtual(p, "test", "(Ljava/lang/Object;)Z", &[e]) {
            Ok(r) => {
                if matches!(r, Some(Value::Int(v)) if v != 0) {
                    kept_idx.push(i);
                }
            }
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let kept: Vec<Value> = kept_idx
        .iter()
        .map(|&i| {
            let e = read_pinned_elem(ctx, ehandles[i], elements[i]);
            stream_pin_canary_check(ctx, "native_stream_filter:rebuild", i, elem_cids[i], e);
            e
        })
        .collect();
    // cceres3: re-read `this` through its pin before the derived-stream build
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(e) = err {
        return Err(e);
    }
    make_derived_stream(ctx, this, &kept, SplOp::Filter)
}

pub(crate) fn native_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // Mirror the `filter` fix: use the `_mut` variant so real-JDK
    // `ReferencePipeline` instances drain via `Stream.toArray()` rather than
    // silently producing an empty stream.
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let this_pin = ctx.pin_native_root(this);
    let fn_pin = ctx.pin_native_root(function);
    match stream_try_defer(ctx, this, LAZY_OP_MAP, Some(function), 0) {
        Ok(Some(s)) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(s));
        }
        Ok(None) => {}
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let mut this = ctx.read_native_pin(this_pin, this);
    let function = ctx.read_native_pin(fn_pin, function);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    // GC-SAFETY: `apply` allocates → moving young GC relocates the function, the
    // input elements, AND the freshly-mapped result objects that accumulate in
    // `mapped`. Pin the function + inputs (re-read each before dispatch) and pin
    // every result as it is produced, then rebuild `mapped` from those pins.
    let (_, ehandles) = pin_value_slice(ctx, &elements);
    let elem_cids = stream_pin_canary_snapshot(ctx, &elements);
    let mut mapped = Vec::with_capacity(elements.len());
    let mut result_handles: Vec<usize> = Vec::with_capacity(elements.len());
    let mut err = None;
    for i in 0..elements.len() {
        let f = ctx.read_native_pin(fn_pin, function);
        let e = read_pinned_elem(ctx, ehandles[i], elements[i]);
        stream_pin_canary_check(ctx, "native_stream_map", i, elem_cids[i], e);
        match ctx.invoke_virtual(f, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[e]) {
            Ok(r) => {
                let v = r.unwrap_or(Value::Object(None));
                match v {
                    Value::Object(Some(o)) => result_handles.push(ctx.pin_native_root(o)),
                    _ => result_handles.push(usize::MAX),
                }
                mapped.push(v);
            }
            Err(ex) if is_placeholder_object_class_cast(ctx, e, &ex) => {
                continue;
            }
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let mapped = read_value_slice(ctx, &result_handles, &mapped);
    // cceres3: re-read `this` through its pin before the derived-stream build
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(e) = err {
        return Err(e);
    }
    make_derived_stream(ctx, this, &mapped, SplOp::Map)
}

pub(crate) fn native_stream_flat_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let this_pin = ctx.pin_native_root(this);
    let fn_pin = ctx.pin_native_root(function);
    match stream_try_defer(ctx, this, LAZY_OP_FLAT_MAP, Some(function), 0) {
        Ok(Some(s)) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(s));
        }
        Ok(None) => {}
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let mut this = ctx.read_native_pin(this_pin, this);
    let function = ctx.read_native_pin(fn_pin, function);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    let elem_cids = stream_pin_canary_snapshot(ctx, &elements);
    let mut flat = Vec::new();
    let mut flat_handles = Vec::new();
    for i in 0..elements.len() {
        let function = ctx.read_native_pin(fn_pin, function);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        stream_pin_canary_check(ctx, "native_stream_flat_map", i, elem_cids[i], elem);
        let result = match ctx.invoke_virtual(
            function,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[elem],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        if let Some(Value::Object(Some(mut inner_stream))) = result {
            let inner_pin = ctx.pin_native_root(inner_stream);
            let inner = match stream_elements(ctx, &mut inner_stream) {
                Ok(v) => v,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            for v in inner {
                if let Value::Object(Some(o)) = v {
                    flat_handles.push(ctx.pin_native_root(o));
                } else {
                    flat_handles.push(usize::MAX);
                }
                flat.push(v);
            }
            // JDK contract: each mapped stream is closed after its contents are
            // placed into the result (this is what runs keycloak's flatMap-inner
            // onClose handlers). Real ClosingStream.close() → delegate.close();
            // a synthetic inner runs its own handlers.
            let inner_stream = ctx.read_native_pin(inner_pin, inner_stream);
            if let Err(e) = ctx.invoke_virtual(inner_stream, "close", "()V", &[]) {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    }
    let flat = read_value_slice(ctx, &flat_handles, &flat);
    // cceres3: re-read `this` through its pin before the derived-stream build
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    make_derived_stream(ctx, this, &flat, SplOp::FlatMap)
}

/// Extract a numeric sort key from a Value, unboxing wrapper objects.
pub(crate) fn numeric_sort_key(ctx: &dyn NativeContext, val: &Value) -> Option<f64> {
    match val {
        Value::Int(v) => Some(*v as f64),
        Value::Long(v) => Some(*v as f64),
        Value::Float(v) => Some(*v as f64),
        Value::Double(v) => Some(*v),
        Value::Object(Some(obj)) => {
            if let Some(prim) = unbox_wrapper(ctx, *obj) {
                match prim {
                    Value::Int(v) => Some(v as f64),
                    Value::Long(v) => Some(v as f64),
                    Value::Float(v) => Some(v as f64),
                    Value::Double(v) => Some(v),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn native_stream_sorted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — `this`
    // is used again for the derived-stream build after the allocating key loop.
    let this_pin = ctx.pin_native_root(this);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };

    // GC-SAFETY: key extraction (`obj_to_display_units` → `toString`) allocates
    // and can trigger a moving young GC that relocates the elements out from under
    // this bare Rust `Vec`. Pin the elements, precompute each key by re-reading
    // from its pin handle, sort an index permutation (keys are plain, no further
    // dispatch), then rebuild the result by re-reading from the pins.
    let (_, ehandles) = pin_value_slice(ctx, &elements);
    let all_numeric = (0..elements.len()).all(|i| {
        let e = read_pinned_elem(ctx, ehandles[i], elements[i]);
        numeric_sort_key(ctx, &e).is_some()
    });
    let mut idx: Vec<usize> = (0..elements.len()).collect();
    if all_numeric {
        let keys: Vec<f64> = (0..elements.len())
            .map(|i| {
                let e = read_pinned_elem(ctx, ehandles[i], elements[i]);
                numeric_sort_key(ctx, &e).unwrap_or(0.0)
            })
            .collect();
        // `Double.compare` order, NOT `partial_cmp`. `f64::partial_cmp` returns
        // `None` for a NaN operand, and `unwrap_or(Equal)` turns that into "NaN
        // equals everything" — which is both the wrong answer (Java sorts every
        // NaN to the END) and a NON-TRANSITIVE comparator, the shape TimSort
        // rejects with "Comparison method violates its general contract!". It is
        // also wrong without any NaN at all: `partial_cmp(-0.0, 0.0)` is `Equal`
        // where `Double.compare` is `-1`.
        //
        // Measured: `Stream.of(...).sorted().limit(3)` over a list containing
        // two NaNs returned `NaN NaN -inf` against HotSpot's `-inf -1.0 -0.0`.
        // This is the one live site of that idiom — the sibling `pq_compare` and
        // `p65_compare_values` primitive arms are unreachable, see the note on
        // each.
        idx.sort_by(|&a, &b| cratonvm_types::jfp::double_ordering(keys[a], keys[b]));
    } else {
        // Fallback: sort by string representation.
        // `collect::<Result<..>>` rather than a `?` inside the closure: the
        // closure has to keep returning a `Result` for the sort key, and a
        // refused element `toString()` must abort the sort instead of ordering
        // the stream by a fabricated `ClassName@hash`.
        // Units, not text -- and this one is an ORDERING, not a display. Java
        // compares Strings by UTF-16 code unit (`String.compareTo`), and
        // `Vec<u16>` compares lexicographically by unit, so the units key is
        // the closer match as well as the lossless one: two elements whose
        // renderings differ only in an unpaired surrogate used to collapse to
        // the same U+FFFD key and sort arbitrarily.
        let keys: Vec<Units> = (0..elements.len())
            .map(|i| {
                let e = read_pinned_elem(ctx, ehandles[i], elements[i]);
                obj_to_display_units(ctx, &e)
            })
            .collect::<Result<Vec<Units>, MethodCallFailed>>()?;
        idx.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
    }
    let sorted: Vec<Value> = idx
        .iter()
        .map(|&i| read_pinned_elem(ctx, ehandles[i], elements[i]))
        .collect();
    // cceres3: re-read `this` through its pin before the derived-stream build
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    make_derived_stream(ctx, this, &sorted, SplOp::SortedNatural)
}

pub(crate) fn native_stream_sorted_cmp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };

    // `stream_elements` can materialize/dispatch and therefore collect.  Both
    // inputs arrived as raw native locals, so root them BEFORE that first
    // potentially-GC-triggering call.  Pinning `comparator` afterwards pins
    // the old evacuated address and cannot repair it (the WildFly
    // JmxMetricCollector sorted-stream path exposed exactly that failure).
    let this_pin = ctx.pin_native_root(this);
    let cmp_pin = ctx.pin_native_root(comparator);
    let result = (|| -> MethodCallResult {
        let mut this = ctx.read_native_pin(this_pin, this);
        let elems = stream_elements(ctx, &mut this)?;

        // GC-SAFETY: the Comparator dispatch (and any key-extractor `apply`) allocates
        // and re-enters Java → a moving young GC relocates the comparator and the
        // materialized elements out from under these bare Rust locals. Pin both, sort
        // an index permutation, and re-read the comparator + the two compared elements
        // from their pin handles on every comparison; build the result by re-reading
        // in sorted order. Stable merge sort — O(n log n); the comparison is fallible
        // (a comparator that throws short-circuits) and a non-Int return is "equal".
        let (_, elem_handles) = pin_value_slice(ctx, &elems);
        let mut idx: Vec<Value> = (0..elems.len() as i32).map(Value::Int).collect();
        let sort_res = merge_sort_fallible(ctx, &mut idx, |c, a, b| {
            let ia = if let Value::Int(v) = a {
                *v as usize
            } else {
                0
            };
            let ib = if let Value::Int(v) = b {
                *v as usize
            } else {
                0
            };
            let ea = read_pinned_elem(c, elem_handles[ia], elems[ia]);
            let eb = read_pinned_elem(c, elem_handles[ib], elems[ib]);
            let cmp = c.read_native_pin(cmp_pin, comparator);
            match comparator_compare(c, cmp, ea, eb)? {
                Some(Value::Int(v)) => Ok(v),
                _ => Ok(0),
            }
        });
        let sorted: Vec<Value> = idx
            .iter()
            .map(|v| {
                let i = if let Value::Int(x) = v {
                    *x as usize
                } else {
                    0
                };
                read_pinned_elem(ctx, elem_handles[i], elems[i])
            })
            .collect();
        sort_res?;
        let this = ctx.read_native_pin(this_pin, this);
        make_derived_stream(ctx, this, &sorted, SplOp::SortedCmp)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_stream_distinct(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — `this`
    // is used again for the derived-stream build after the `equals` loops.
    let this_pin = ctx.pin_native_root(this);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    let mut unique: Vec<Value> = Vec::new();
    let mut unique_handles: Vec<usize> = Vec::new();
    // Survivor indices into `unique`, keyed by the survivor's Java `hashCode()`.
    //
    // `Stream.distinct()` is specified by the `HashSet` it builds: a later
    // element is a duplicate only when an earlier one has the SAME hash and
    // `equals` it. This used to ask `equals` of every survivor so far, which is
    // `n(n-1)/2` Java calls for `n` distinct elements. MEASURED 2026-09-12:
    // `LocaleServiceProvider.isSupportedLocale` runs a `distinct()` over the
    // 1152 CLDR language tags, so the first `Calendar.getInstance` per locale
    // took 1.5 s against HotSpot's 11 ms — long enough that Tomcat's
    // `TestAccessLogValve` timed out waiting for a `%{begin:...SSS}t` line.
    // Asking only same-hash candidates is also the JDK's answer when `equals`
    // and `hashCode` disagree (two `equals` keys with different hashes both
    // survive), which the all-pairs scan got wrong in the other direction.
    let mut buckets: std::collections::HashMap<i32, Vec<usize>> = std::collections::HashMap::new();
    for i in 0..elements.len() {
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let hash = element_hash_code(ctx, &elem)?;
        // `values_equal` alone only recognises identity, String, enum and
        // unboxed primitives, so value classes/records with a real `equals`
        // override (`ResourcePatternHint`, `UUID`, user records) were never
        // deduped (Spring AOT `ResourceHintsAttributes` emitted 8 globs where
        // HotSpot collapses to 5). `list_element_matches` takes that cheap
        // structural check first and then falls back to Java `equals`, with the
        // NEW element as receiver — `HashMap.putVal`'s `key.equals(k)`.
        let mut dup = false;
        if let Some(candidates) = buckets.get(&hash) {
            for &u_idx in candidates {
                let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
                let u = read_pinned_elem(ctx, unique_handles[u_idx], unique[u_idx]);
                if list_element_matches(ctx, &u, &elem)? {
                    dup = true;
                    break;
                }
            }
        }
        if !dup {
            let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
            if let Value::Object(Some(o)) = elem {
                unique_handles.push(ctx.pin_native_root(o));
            } else {
                unique_handles.push(usize::MAX);
            }
            buckets.entry(hash).or_default().push(unique.len());
            unique.push(elem);
        }
    }
    let unique = read_value_slice(ctx, &unique_handles, &unique);
    // cceres3: re-read `this` through its pin before the derived-stream build
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    make_derived_stream(ctx, this, &unique, SplOp::Distinct)
}

pub(crate) fn native_stream_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_count_refused(args)?;
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let n = match args.get(1) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    if let Some(s) = stream_try_defer(ctx, this, LAZY_OP_LIMIT, None, n as i64)? {
        return Ok(Some(s));
    }
    // THROWS5-3: a lazy walk stream's slice builds no frame.
    if let Some(r) = lazy_walk_slice(ctx, this, false, n) {
        return r;
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — `this`
    // is used again for the derived-stream build after materialization.
    let this_pin = ctx.pin_native_root(this);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let limited: Vec<Value> = elements.into_iter().take(n).collect();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    make_derived_stream(ctx, this, &limited, SplOp::Preserve)
}

pub(crate) fn native_stream_skip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_count_refused(args)?;
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let n = match args.get(1) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    if let Some(s) = stream_try_defer(ctx, this, LAZY_OP_SKIP, None, n as i64)? {
        return Ok(Some(s));
    }
    // THROWS5-3: a lazy walk stream's slice builds no frame.
    if let Some(r) = lazy_walk_slice(ctx, this, true, n) {
        return r;
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — `this`
    // is used again for the derived-stream build after materialization.
    let this_pin = ctx.pin_native_root(this);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let skipped: Vec<Value> = elements.into_iter().skip(n).collect();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    make_derived_stream(ctx, this, &skipped, SplOp::Preserve)
}

pub(crate) fn native_stream_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let this_pin = ctx.pin_native_root(this);
    let con_pin = ctx.pin_native_root(consumer);
    match stream_try_defer(ctx, this, LAZY_OP_PEEK, Some(consumer), 0) {
        Ok(Some(s)) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(s));
        }
        Ok(None) => {}
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let mut this = ctx.read_native_pin(this_pin, this);
    let consumer = ctx.read_native_pin(con_pin, consumer);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    for i in 0..elements.len() {
        let consumer = ctx.read_native_pin(con_pin, consumer);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        if let Err(e) = ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let elements = read_value_slice(ctx, &elem_handles, &elements);
    // cceres3: re-read `this` through its pin before the derived-stream build
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    make_derived_stream(ctx, this, &elements, SplOp::Preserve)
}

// -- Terminal operations --

pub(crate) fn native_stream_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // The terminal consumer remains live across stream materialisation, which
    // can invoke arbitrary Java code and move it. Root it before choosing or
    // executing either stream path; taking this pin after `stream_elements`
    // is too late and was the dominant synthetic-lambda PIN-STALE firing.
    let con_pin = ctx.pin_native_root(consumer);
    // Lazy path: a stream straight from `StreamSupport.stream(realSpliterator,
    // false)` (no intervening op materialised it) drives its source spliterator
    // one element at a time, so a side-effecting consumer observes per-element
    // processing exactly like the JDK (the fix for DetachedPreviousRowStateTest).
    // FIX (stream-eager-drain-20260715): only take this direct/bespoke lazy
    // loop when `this` has NO deferred op-chain (a straight
    // `StreamSupport.stream(sp, false).forEach(consumer)` with no
    // intervening map/filter/etc) -- feeding raw elements straight to
    // `consumer` is correct there. A `this` that DOES carry a chain
    // (`.map(...).forEach(...)`) must NOT take this branch: it would hand
    // the consumer raw pre-map elements, skipping the chain entirely. Fall
    // through to `stream_elements` below instead, which (as of this fix)
    // drives a still-lazy source inline through the chain via
    // `stream_pull_internal`.
    if !stream_has_chain(ctx, this) {
        if let Some(spl) = stream_lazy_spliterator(ctx, this) {
            // GC-SAFETY: the `tryAdvance` loop below re-enters Java once per
            // element and can trigger a moving GC that relocates `this`; the
            // trailing `set_field(this, ...)` after the loop was reading the
            // stale address captured at function entry. Pin `this` alongside
            // `spl`/`consumer` and re-read it before that final use. Confirmed
            // live via CRATONVM_DBG_STALE_OBJREF during WildFly parallel-boot
            // ServiceLoader stream draining -- see
            // wildfly-parallel-boot-stale-objectref-residual.md.
            let this_pin = ctx.pin_native_root(this);
            let spl_pin = ctx.pin_native_root(spl);
            const SAFETY_CAP: usize = 1_000_000;
            let mut n = 0usize;
            let result = loop {
                let s = ctx.read_native_pin(spl_pin, spl);
                let c = ctx.read_native_pin(con_pin, consumer);
                match ctx.invoke_virtual(
                    s,
                    "tryAdvance",
                    "(Ljava/util/function/Consumer;)Z",
                    &[Value::Object(Some(c))],
                ) {
                    Ok(Some(Value::Int(v))) if v != 0 => {
                        n += 1;
                        if n >= SAFETY_CAP {
                            break Ok(None);
                        }
                    }
                    Ok(_) => break Ok(None),
                    Err(e) => break Err(e),
                }
            };
            let mut this = ctx.read_native_pin(this_pin, this);
            // `con_pin` is the first pin owned by this native; one truncate clears
            // it and the later `this`/`spl` pins without violating stack order.
            ctx.unpin_native_roots(con_pin);
            // Mark consumed so a (illegal) second terminal sees an empty stream.
            ctx.set_field(this, STREAM_FIELD_LAZY_SPLITERATOR, Value::Object(None));
            return result;
        }
    }
    let elements = stream_elements(ctx, &mut this)?;
    // GC-SAFETY: `accept` re-enters Java and can trigger a moving young GC that
    // relocates the consumer and the materialized elements; pin them and re-read
    // each from its handle before the (allocating) dispatch.
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    // GC-SAFETY (register-invisible-root closeable gap — see
    // `NativeContext::refresh_root_snapshot`'s doc comment): the pins above
    // are invisible to a peer-initiated STW collection until this thread's
    // deposited root snapshot is refreshed, which does not happen on any
    // fixed cadence once `accept` re-enters Java and (likely, given `accept`
    // is about to run once per element here) tiers up into JIT — JIT-compiled
    // code has no periodic cooperative safepoint poll of its own. `accept`
    // can run arbitrarily long per element (e.g. a full network round trip),
    // so refresh once up front to publish this batch immediately, then again
    // at the top of every iteration to keep the deposit from going stale
    // across a long-running consumer.
    ctx.refresh_root_snapshot();
    let mut result = Ok(None);
    for (i, &elem) in elements.iter().enumerate() {
        // Periodic, not per-element — see `stream_refresh_interval`.
        if stream_should_refresh(i) {
            ctx.refresh_root_snapshot();
        }
        let c = ctx.read_native_pin(con_pin, consumer);
        let e = read_pinned_elem(ctx, elem_handles[i], elem);
        if let Err(err) = ctx.invoke_virtual(c, "accept", "(Ljava/lang/Object;)V", &[e]) {
            result = Err(err);
            break;
        }
    }
    ctx.unpin_native_roots(con_pin);
    result
}

pub(crate) fn native_stream_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    // THROWS5-3: a lazy walk stream with no deferred op counts its frames
    // without building them (linked or consumed as `stream_elements` would).
    if stream_is_reference_carrier(&*ctx, this)
        && !stream_has_chain(&*ctx, this)
        && stream_lazy_spliterator(&*ctx, this).is_none()
    {
        if let Some(len) = lazy_walk_len(&*ctx, this) {
            stream_link_or_consume(ctx, this)?;
            // Cast: an array length.
            return Ok(Some(Value::Long(len as i64)));
        }
    }
    let elements = stream_elements(ctx, &mut this)?;
    Ok(Some(Value::Long(elements.len() as i64)))
}

/// Chain-aware `Stream.iterator()` / `BaseStream.iterator()`: materialise via
/// `stream_elements` (which applies any deferred lazy op-chain) and hand back a
/// `ServiceLoader$Itr` (slot 0 = array, slot 1 = cursor; its hasNext/next natives
/// are registered in streams.rs). Replaces the field-0-raw iterator that bypassed
/// the chain. Non-synthetic / real pipelines flow through `stream_elements`'s
/// `toArray` path, so this is correct for those too.
///
/// W7-2 §3: this registration also serves the THREE PRIMITIVE streams, because
/// `iterator()Ljava/util/Iterator;` is declared on `BaseStream` — the supertype
/// of `IntStream`/`LongStream`/`DoubleStream` — and that is the descriptor a
/// call site records whenever the static type is `BaseStream` or `Stream`. A
/// synthetic `IntStream` keeps its elements in a PRIMITIVE `int[]`, so
/// `stream_elements` hands back bare `Value::Int`s; storing those into the
/// `Object[]` below put an untyped word where the JDK's `Arrays$ArrayItr.next()`
/// — declared `()Ljava/lang/Object;` — hands a reference to the caller. It is
/// silent at the store and only misbehaves one frame away: measured, `o != null`
/// was TRUE and `o.getClass()` raised NPE on the very same word, and
/// `o instanceof Integer` was false for every element. That is the
/// primitive-in-a-reference-store species (W7-84), not a wrong answer.
/// [`box_primitive_stream_elements`] fixes it for every element and allocates
/// nothing on a reference stream, which is the common case this function exists
/// for (Hibernate's `JoinedList.iterator()`).
///
/// NOTE the registration order: `streams.rs`'s `native_stream_empty_iterator`
/// does its own boxing, but native-collections registers AFTER native-builtins
/// (last-writer-wins), so for `Stream`/`BaseStream` THIS function is the one
/// that runs. The two must both box or the surface is only half covered.
pub(crate) fn native_stream_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = alloc_ref_array(ctx, 0);
            // Same landing as every other snapshot iterator: under --real-jdk
            // the fabricated shape is refused, and a real Arrays$ArrayItr over
            // the same Object[] answers instead.
            return make_iterator_from_array(ctx, arr, 0);
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    // Box BEFORE pinning: the returned wrappers are already re-read past any
    // collection `valueOf` caused, and nothing allocates between here and the
    // `pin_value_slice` on the next line.
    let elements = box_primitive_stream_elements(ctx, &elements)?;
    let (elem_base, elem_handles) = pin_value_slice(ctx, &elements);
    let arr = alloc_ref_array(ctx, elements.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elements.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    // `make_iterator_from_array` owns the refusal: under --real-jdk the
    // fabricated `java/util/ServiceLoader$Itr` — a name no JDK declares — is
    // refused, and a real `java.util.Arrays$ArrayItr` over this very array
    // stands in. Refusing it with nowhere to land took out ServiceLoader, and
    // with it the CLDR locale provider: `ServiceConfigurationError: Locale
    // provider adapter "CLDR" cannot be instantiated`.
    let itr = make_iterator_from_array(ctx, arr, elements.len());
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        arr_pin
    } else {
        elem_base
    });
    itr
}

pub(crate) fn native_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = alloc_ref_array(ctx, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let (elem_base, elem_handles) = pin_value_slice(ctx, &elements);
    let arr = alloc_ref_array(ctx, elements.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elements.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        arr_pin
    } else {
        elem_base
    });
    Ok(Some(Value::Object(Some(arr))))
}

/// `Stream.toArray(IntFunction<A[]> generator)` — invoke the generator
/// with the element count to produce a correctly-typed array, then copy
/// elements into it.
///
/// Real-JDK code uses this for any `Stream.toArray(String[]::new)` chain,
/// e.g. `Utils.enumOptions(SecurityProtocol.class)` in Kafka. Without
/// this native override, dispatch falls into the real JDK
/// `ReferencePipeline.toArray` which expects a real Spliterator-backed
/// pipeline, not our synthetic field-0-array stream — silently
/// returning a 0-length array. That made
/// `ConfigDef.ValidString.in(...)` build an empty allow-list, and
/// `ReplicationConfigs.<clinit>` threw
/// `ConfigException: Invalid value PLAINTEXT for configuration
/// security.inter.broker.protocol: String must be one of:`.
pub(crate) fn native_stream_to_array_gen(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = alloc_ref_array(ctx, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let generator = args.get(1).copied().unwrap_or(Value::Object(None));
    let gen_pin = match generator {
        Value::Object(Some(g)) => Some(ctx.pin_native_root(g)),
        _ => None,
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — a `?`
    // here leaked `gen_pin` on the error path (pin-stack imbalance).
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            if let Some(p) = gen_pin {
                ctx.unpin_native_roots(p);            }
            return Err(e);
        }
    };
    let (elem_base, elem_handles) = pin_value_slice(ctx, &elements);
    let len = elements.len();
    // Try to use the generator's apply(int) to allocate a typed array.
    let arr = match generator {
        Value::Object(Some(g)) => {
            let g = gen_pin.map_or(g, |pin| ctx.read_native_pin(pin, g));
            let r = ctx.invoke_virtual(
                g,
                "apply",
                "(I)Ljava/lang/Object;",
                &[Value::Int(len as i32)],
            );
            match r {
                Ok(Some(Value::Object(Some(a)))) => a,
                _ => alloc_ref_array(ctx, len),
            }
        }
        _ => alloc_ref_array(ctx, len),
    };
    let arr_pin = ctx.pin_native_root(arr);
    // If generator returned a too-small array (or fallback Object[]),
    // ensure it has enough slots.
    let arr = ctx.read_native_pin(arr_pin, arr);
    let cap = ctx.array_length(arr);
    let (arr, arr_pin) = if cap < len {
        ctx.unpin_native_roots(arr_pin);
        let arr = alloc_ref_array(ctx, len);
        let pin = ctx.pin_native_root(arr);
        (arr, pin)
    } else {
        (arr, arr_pin)
    };
    for (i, val) in elements.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    let root_base = gen_pin.unwrap_or(if elem_base == usize::MAX {
        arr_pin
    } else {
        elem_base
    });
    ctx.unpin_native_roots(root_base);
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_stream_find_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    if lazy_streams_enabled() && stream_is_synthetic(ctx, this) && stream_has_chain(ctx, this) {
        let mut found: Option<Value> = None;
        stream_pull_terminal(ctx, this, |_c, v| {
            found = Some(v);
            Ok(PullStep::Stop)
        })?;
        // GC-SAFETY: `alloc_synthetic` below allocates the Optional wrapper and
        // can trigger a moving GC; `found` is a bare `Value` captured from the
        // pull closure with no pin. Pin it first and refresh after, matching
        // the eager branch a few lines below.
        let found_pin = match found {
            Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(o)),
            _ => None,
        };
        let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
        if let Some(fv) = found {
            let fv = match (fv, found_pin) {
                (Value::Object(Some(o)), Some(pin)) => {
                    Value::Object(Some(ctx.read_native_pin(pin, o)))
                }
                _ => fv,
            };
            ctx.set_field(opt, OPT_FIELD_VALUE, fv);
        }
        if let Some(pin) = found_pin {
            ctx.unpin_native_roots(pin);
        }
        return Ok(Some(Value::Object(Some(opt))));
    }
    // THROWS5-3: a lazy walk stream builds its first frame only.
    let mut lazy_first: Option<Value> = None;
    let lazy = lazy_walk_each(ctx, this, &mut |_c, v| {
        lazy_first = Some(v);
        Ok(PullStep::Stop)
    });
    let elements = match lazy {
        Some(done) => {
            done?;
            lazy_first.into_iter().collect()
        }
        None => stream_elements(ctx, &mut this)?,
    };
    let first_pin = elements.first().and_then(|v| match v {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
        _ => None,
    });
    let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
    if let Some(first) = elements.first() {
        let first = match (*first, first_pin) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            _ => *first,
        };
        ctx.set_field(opt, OPT_FIELD_VALUE, first);
    }
    if let Some(pin) = first_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Object(Some(opt))))
}

/// THROWS5-3: `anyMatch` / `allMatch` / `noneMatch` on a lazy walk stream
/// with no deferred op ([`lazy_walk_each`]): test the frames in order, each
/// built when reached, until the predicate answers `stop_when`. `Some(Ok(true))`
/// when it did; `None` when `this` is not such a stream.
pub(crate) fn lazy_walk_match(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    predicate: ObjectRef,
    stop_when: bool,
) -> Option<Result<bool, MethodCallFailed>> {
    lazy_walk_len(&*ctx, this)?;
    let pred_pin = ctx.pin_native_root(predicate);
    let mut stopped = false;
    let done = lazy_walk_each(ctx, this, &mut |c, v| {
        let predicate = c.read_native_pin(pred_pin, predicate);
        let r = c.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[v])?;
        if matches!(r, Some(Value::Int(x)) if x != 0) == stop_when {
            stopped = true;
            Ok(PullStep::Stop)
        } else {
            Ok(PullStep::Continue)
        }
    });
    ctx.unpin_native_roots(pred_pin);
    done.map(|r| r.map(|()| stopped))
}

pub(crate) fn native_stream_any_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    if lazy_streams_enabled() && stream_is_synthetic(ctx, this) && stream_has_chain(ctx, this) {
        let mut matched = false;
        let pred_pin = ctx.pin_native_root(predicate);
        let pull_result = stream_pull_terminal(ctx, this, |c, v| {
            let predicate = c.read_native_pin(pred_pin, predicate);
            let r = c.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[v])?;
            if matches!(r, Some(Value::Int(x)) if x != 0) {
                matched = true;
                Ok(PullStep::Stop)
            } else {
                Ok(PullStep::Continue)
            }
        });
        ctx.unpin_native_roots(pred_pin);
        pull_result?;
        return Ok(Some(Value::Int(if matched { 1 } else { 0 })));
    }
    // THROWS5-3: a lazy walk stream builds frames up to the deciding one.
    if let Some(done) = lazy_walk_match(ctx, this, predicate, true) {
        let stopped = done?;
        return Ok(Some(Value::Int(i32::from(stopped))));
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let pred_pin = ctx.pin_native_root(predicate);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(pred_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    for i in 0..elements.len() {
        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let result = match ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(pred_pin);
                return Err(e);
            }
        };
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            ctx.unpin_native_roots(pred_pin);
            return Ok(Some(Value::Int(1)));
        }
    }
    ctx.unpin_native_roots(pred_pin);
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_stream_all_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    if lazy_streams_enabled() && stream_is_synthetic(ctx, this) && stream_has_chain(ctx, this) {
        let mut all = true;
        let pred_pin = ctx.pin_native_root(predicate);
        let pull_result = stream_pull_terminal(ctx, this, |c, v| {
            let predicate = c.read_native_pin(pred_pin, predicate);
            let r = c.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[v])?;
            if !matches!(r, Some(Value::Int(x)) if x != 0) {
                all = false;
                Ok(PullStep::Stop)
            } else {
                Ok(PullStep::Continue)
            }
        });
        ctx.unpin_native_roots(pred_pin);
        pull_result?;
        return Ok(Some(Value::Int(if all { 1 } else { 0 })));
    }
    // THROWS5-3: a lazy walk stream builds frames up to the deciding one.
    if let Some(done) = lazy_walk_match(ctx, this, predicate, false) {
        let stopped = done?;
        return Ok(Some(Value::Int(i32::from(!stopped))));
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let pred_pin = ctx.pin_native_root(predicate);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(pred_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    for i in 0..elements.len() {
        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let result = match ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(pred_pin);
                return Err(e);
            }
        };
        if !matches!(result, Some(Value::Int(v)) if v != 0) {
            ctx.unpin_native_roots(pred_pin);
            return Ok(Some(Value::Int(0)));
        }
    }
    ctx.unpin_native_roots(pred_pin);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_stream_none_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    if lazy_streams_enabled() && stream_is_synthetic(ctx, this) && stream_has_chain(ctx, this) {
        let mut none = true;
        let pred_pin = ctx.pin_native_root(predicate);
        let pull_result = stream_pull_terminal(ctx, this, |c, v| {
            let predicate = c.read_native_pin(pred_pin, predicate);
            let r = c.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[v])?;
            if matches!(r, Some(Value::Int(x)) if x != 0) {
                none = false;
                Ok(PullStep::Stop)
            } else {
                Ok(PullStep::Continue)
            }
        });
        ctx.unpin_native_roots(pred_pin);
        pull_result?;
        return Ok(Some(Value::Int(if none { 1 } else { 0 })));
    }
    // THROWS5-3: a lazy walk stream builds frames up to the deciding one.
    if let Some(done) = lazy_walk_match(ctx, this, predicate, true) {
        let stopped = done?;
        return Ok(Some(Value::Int(i32::from(!stopped))));
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let pred_pin = ctx.pin_native_root(predicate);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(pred_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    for i in 0..elements.len() {
        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let result = match ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(pred_pin);
                return Err(e);
            }
        };
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            ctx.unpin_native_roots(pred_pin);
            return Ok(Some(Value::Int(0)));
        }
    }
    ctx.unpin_native_roots(pred_pin);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_stream_reduce_identity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(args.get(1).copied()),
    };
    let identity = args.get(1).copied().unwrap_or(Value::Object(None));
    let operator = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(identity)),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(operator);
    let mut acc = identity;
    let mut acc_handle = match acc {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    };
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    for i in 0..elements.len() {
        let operator = ctx.read_native_pin(op_pin, operator);
        let acc_arg = read_pinned_elem(ctx, acc_handle, acc);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let result = match ctx.invoke_virtual(
            operator,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[acc_arg, elem],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        };
        // Keep the operator result as-is. `coerce_return` inside invoke_virtual
        // already boxes primitives to match the SAM's `Ljava/lang/Object;`
        // return type, so `checkcast` in callers like JoinedList.<init> sees
        // a proper object reference. Unboxing here (normalize_for_compare)
        // was a T16.9 leftover that violated the `T reduce(T, BinaryOperator<T>)`
        // contract and broke any caller that stores the result as a reference.
        acc = result.unwrap_or(Value::Object(None));
        acc_handle = match acc {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
    }
    let acc = read_pinned_elem(ctx, acc_handle, acc);
    ctx.unpin_native_roots(op_pin);
    Ok(Some(acc))
}

/// Sequential implementation of `Stream.reduce(U, BiFunction, BinaryOperator)`.
///
/// CratonVM executes object streams sequentially, so the combiner is required
/// by the Java API but is not invoked.  Every object that remains live across a
/// user-supplied `BiFunction.apply` is pinned: the callback can allocate and
/// therefore move the young generation.
pub(crate) fn native_stream_reduce_general(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(args.get(1).copied()),
    };
    let mut acc = args.get(1).copied().unwrap_or(Value::Object(None));
    let accumulator = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(acc)),
    };

    // Keep one batch of pins and release it as a unit.  Each accumulator result
    // gets a new pin before the next callback, so it cannot go stale even when
    // the callback triggers a moving collection.
    let pin_base = ctx.pin_native_root(this);
    let mut acc_pin = pin_value(ctx, acc);
    let accumulator_pin = ctx.pin_native_root(accumulator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(pin_base);
            return Err(e);
        }
    };
    let (_, elem_pins) = pin_value_slice(ctx, &elements);

    for (index, element) in elements.iter().enumerate() {
        let accumulator = ctx.read_native_pin(accumulator_pin, accumulator);
        let acc_arg = read_pinned_elem(ctx, acc_pin, acc);
        let element = read_pinned_elem(ctx, elem_pins[index], *element);
        let result = match ctx.invoke_virtual(
            accumulator,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[acc_arg, element],
        ) {
            Ok(result) => result,
            Err(e) => {
                ctx.unpin_native_roots(pin_base);
                return Err(e);
            }
        };
        acc = result.unwrap_or(Value::Object(None));
        acc_pin = pin_value(ctx, acc);
    }

    let acc = read_pinned_elem(ctx, acc_pin, acc);
    ctx.unpin_native_roots(pin_base);
    Ok(Some(acc))
}

pub(crate) fn native_stream_reduce_optional(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(operator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
    if elements.is_empty() {
        ctx.unpin_native_roots(op_pin);
        return Ok(Some(Value::Object(Some(opt))));
    }
    let opt_pin = ctx.pin_native_root(opt);
    let mut acc = elements[0];
    let mut acc_handle = elem_handles[0];
    for i in 1..elements.len() {
        let operator = ctx.read_native_pin(op_pin, operator);
        let acc_arg = read_pinned_elem(ctx, acc_handle, acc);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let result = match ctx.invoke_virtual(
            operator,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[acc_arg, elem],
        ) {
            Ok(r) => r,
            Err(e) => {
                // cceres3: op_pin is the first pin this function created
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        };
        acc = result.unwrap_or(Value::Object(None));
        acc_handle = match acc {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
    }
    let opt = ctx.read_native_pin(opt_pin, opt);
    let acc = read_pinned_elem(ctx, acc_handle, acc);
    ctx.set_field(opt, OPT_FIELD_VALUE, acc);
    ctx.unpin_native_roots(op_pin);
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_stream_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let cmp_pin = ctx.pin_native_root(comparator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(cmp_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
    if elements.is_empty() {
        ctx.unpin_native_roots(cmp_pin);
        return Ok(Some(Value::Object(Some(opt))));
    }
    let opt_pin = ctx.pin_native_root(opt);
    let mut best = elements[0];
    let mut best_handle = elem_handles[0];
    for i in 1..elements.len() {
        let comparator = ctx.read_native_pin(cmp_pin, comparator);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let best_arg = read_pinned_elem(ctx, best_handle, best);
        let cmp = match comparator_compare(ctx, comparator, elem, best_arg) {
            Ok(r) => r,
            Err(e) => {
                // cceres3: cmp_pin is the first pin this function created
                ctx.unpin_native_roots(cmp_pin);
                return Err(e);
            }
        };
        if matches!(cmp, Some(Value::Int(v)) if v < 0) {
            best = elem;
            best_handle = elem_handles[i];
        }
    }
    let opt = ctx.read_native_pin(opt_pin, opt);
    let best = read_pinned_elem(ctx, best_handle, best);
    ctx.set_field(opt, OPT_FIELD_VALUE, best);
    ctx.unpin_native_roots(cmp_pin);
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_stream_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let cmp_pin = ctx.pin_native_root(comparator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(cmp_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
    if elements.is_empty() {
        ctx.unpin_native_roots(cmp_pin);
        return Ok(Some(Value::Object(Some(opt))));
    }
    let opt_pin = ctx.pin_native_root(opt);
    let mut best = elements[0];
    let mut best_handle = elem_handles[0];
    for i in 1..elements.len() {
        let comparator = ctx.read_native_pin(cmp_pin, comparator);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let best_arg = read_pinned_elem(ctx, best_handle, best);
        let cmp = match comparator_compare(ctx, comparator, elem, best_arg) {
            Ok(r) => r,
            Err(e) => {
                // cceres3: cmp_pin is the first pin this function created
                ctx.unpin_native_roots(cmp_pin);
                return Err(e);
            }
        };
        if matches!(cmp, Some(Value::Int(v)) if v > 0) {
            best = elem;
            best_handle = elem_handles[i];
        }
    }
    let opt = ctx.read_native_pin(opt_pin, opt);
    let best = read_pinned_elem(ctx, best_handle, best);
    ctx.set_field(opt, OPT_FIELD_VALUE, best);
    ctx.unpin_native_roots(cmp_pin);
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_stream_to_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_list_of(ctx, &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    if let Some(r) = jdk_list_nulls_allowed(ctx, &elements) {
        return r;
    }
    make_list_of(ctx, &elements)
}

/// `CRATONVM_COMPAT_STREAM_TOLIST_UNMODIFIABLE` -- default ON, `=0` restores
/// the mutable `ArrayList`. `Stream.toList()` on a native (synthetic) stream
/// answered a plain, MUTABLE `ArrayList` (`make_list_of`): `add` / `set` /
/// `remove` succeeded, and `List.copyOf` of it copied as for any list. JDK 25's
/// `ReferencePipeline.toList()` answers
/// `ImmutableCollections.listFromTrustedArrayNullsAllowed(toArray())`: an
/// unmodifiable `ListN` that allows nulls (`EMPTY_LIST_NULLS` when empty),
/// which `List.copyOf` copies (and refuses when it holds a null). This asks
/// that very method for it, so the result is HotSpot's object. The same switch
/// covers `CollSer.readResolve` of an `IMM_LIST_NULLS` list
/// (`of_list_allowing_nulls`), which the JDK rebuilds through the same method
/// and the native froze into a wrapper `copyOf` handed back unchanged. Round
/// 15 wave 2 (lane compat), `r15w2-compat-stream-tolist-is-mutable-FIXED-20260930.md`.
pub(crate) fn stream_to_list_unmodifiable_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    true
}

/// `ImmutableCollections.listFromTrustedArrayNullsAllowed(elems)` over a fresh
/// `Object[]`, as [`stream_to_list_unmodifiable_enabled`] describes. `None` --
/// nothing has run, the caller builds its `ArrayList` from `elems` -- only with
/// the switch off. Otherwise always `Some`: the list, an exception (the
/// allocation's, or the method's), or -- when the class or method is missing
/// (the synthetic JDK) or answers no list -- an unmodifiable list, built from
/// the elements READ BACK through their pins, since the class initialisation,
/// the allocation and the call can all move them.
pub(crate) fn jdk_list_nulls_allowed(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> Option<MethodCallResult> {
    const IC: &str = "java/util/ImmutableCollections";
    const NAME: &str = "listFromTrustedArrayNullsAllowed";
    const DESC: &str = "([Ljava/lang/Object;)Ljava/util/List;";
    if !stream_to_list_unmodifiable_enabled() {
        return None;
    }
    // Everything below collects (the class's `<clinit>`, the array, the
    // call): the elements stay pinned across all of it. The array is the
    // list's own storage from here on ("trusted"), so nothing else may hold it.
    let (base, handles) = pin_value_slice(ctx, elems);
    // Synthetic classes optimistically report arbitrary methods as present.
    let usable = ctx
        .ensure_class_initialized(IC)
        .is_ok_and(|id| ctx.class_declares_method(id, NAME, DESC));
    let answer: MethodCallResult = if usable {
        match alloc_ref_array_or_oom(ctx, elems.len()) {
            Ok(arr) => {
                for (i, v) in elems.iter().enumerate() {
                    let v = read_pinned_elem(ctx, handles[i], *v);
                    ctx.set_array_element(arr, i, v);
                }
                ctx.invoke(IC, NAME, DESC, &[Value::Object(Some(arr))])
            }
            Err(e) => Err(e),
        }
    } else {
        Ok(None)
    };
    let now: Option<Vec<Value>> = match answer {
        Ok(Some(Value::Object(Some(_)))) | Err(_) => None,
        Ok(_) => Some(
            elems
                .iter()
                .enumerate()
                .map(|(i, v)| read_pinned_elem(ctx, handles[i], *v))
                .collect(),
        ),
    };
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    match now {
        Some(now) => {
            let list = make_list_of(ctx, &now);
            Some(freeze_result(ctx, UNMOD_LIST_CLASS, list))
        }
        None => Some(answer),
    }
}

/// `Stream.toList()` as the JDK's `ReferencePipeline.toList` answers it:
/// `ImmutableCollections.listFromTrustedArrayNullsAllowed(toArray())`, an
/// unmodifiable `ListN` that admits nulls. [`native_stream_to_list`] handed
/// out a MUTABLE `ArrayList` (`Stream.of("a").toList().add("b")` succeeded
/// under `--compatible`; HotSpot throws `UnsupportedOperationException`).
/// Registered in its place under `CRATONVM_COMPAT_JDK_IMMUTABLES`
/// (interpreter round i1 wave 49, lane L6; stage 4 of
/// `interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020`).
pub(crate) fn native_stream_to_list_jdk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    const CLASS: &str = "java/util/ImmutableCollections";
    const METHOD: &str = "listFromTrustedArrayNullsAllowed";
    const DESCRIPTOR: &str = "([Ljava/lang/Object;)Ljava/util/List;";
    let elements = match args.first() {
        Some(Value::Object(Some(r))) => {
            let mut this = *r;
            stream_elements(ctx, &mut this)?
        }
        _ => Vec::new(),
    };
    let (elem_base, elem_handles) = pin_value_slice(ctx, &elements);
    let class = match ctx.ensure_class_initialized(CLASS) {
        Ok(class) => class,
        Err(error) => {
            if elem_base != usize::MAX {
                ctx.unpin_native_roots(elem_base);
            }
            return Err(error);
        }
    };
    if !ctx.class_declares_method(class, METHOD, DESCRIPTOR) {
        let current: Vec<_> = elements
            .iter()
            .enumerate()
            .map(|(i, value)| read_pinned_elem(ctx, elem_handles[i], *value))
            .collect();
        let list = make_list_of(ctx, &current);
        let result = freeze_result(ctx, UNMOD_LIST_CLASS, list);
        if elem_base != usize::MAX {
            ctx.unpin_native_roots(elem_base);
        }
        return result;
    }
    let arr = alloc_ref_array_or_oom(ctx, elements.len());
    let arr = match arr {
        Ok(arr) => arr,
        Err(e) => {
            if elem_base != usize::MAX {
                ctx.unpin_native_roots(elem_base);            }
            return Err(e);
        }
    };
    for (i, val) in elements.iter().enumerate() {
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    if elem_base != usize::MAX {
        ctx.unpin_native_roots(elem_base);
    }
    ctx.invoke(CLASS, METHOD, DESCRIPTOR, &[Value::Object(Some(arr))])
}

pub(crate) fn native_stream_map_to_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — root
    // `this`/`function` BEFORE `stream_elements`, which can dispatch and collect.
    let this_pin = ctx.pin_native_root(this);
    let fn_pin = ctx.pin_native_root(function);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    let mut ints = Vec::with_capacity(elements.len());
    for i in 0..elements.len() {
        let function = ctx.read_native_pin(fn_pin, function);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let result =
            match ctx.invoke_virtual(function, "applyAsInt", "(Ljava/lang/Object;)I", &[elem]) {
                Ok(r) => r,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
        ints.push(result.unwrap_or(Value::Int(0)));    }
    let r = make_int_stream(ctx, &ints)?;
    if let Some(Value::Object(Some(dst))) = &r {
        let this = ctx.read_native_pin(this_pin, this);
        stream_inherit_close_handlers(ctx, this, *dst);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(r)
}

/// A one-slot synthetic primitive stream of `class` over `vals` (primitive
/// `Value`s, so nothing in `vals` needs a pin): the body the synthetic
/// `mapToLong`/`mapToDouble`/`flatMapTo*` natives shared inline.
///
/// gc-common w20-a: the stream is rooted across its array's allocation; the
/// inline copies stored the array into the stream's pre-allocation address.
pub(crate) fn synthetic_primitive_stream_of(
    ctx: &mut dyn NativeContext,
    class: &str,
    vals: &[Value],
) -> MethodCallResult {
    // JIT round 18 wave 11 (lane streams5): the full primitive layout
    // (`make_{int,long,double}_stream`: primitive array, every slot), so the
    // `Stream.mapToLong` / `mapToDouble` / `flatMapTo*` stage can carry its
    // stage class, parallel bit, close root and facts like every other
    // stage. The one-slot carrier had none of them and answered the shape's
    // `Head` from `getClass()` (`Stream.of(1).mapToLong(f).getClass()` is
    // `ReferencePipeline$5` on HotSpot) and `false` from `isParallel()`.
    // `CRATONVM_STREAM_STAGE_DISPLAY_CLASS=0` keeps the one-slot carrier.
    if stream_stage_display_class_enabled() {
        match class {
            "java/util/stream/IntStream" => return make_int_stream(ctx, vals),
            "java/util/stream/LongStream" => return make_long_stream(ctx, vals),
            "java/util/stream/DoubleStream" => return make_double_stream(ctx, vals),
            _ => {}
        }
    }
    let stream = try_alloc_synthetic(ctx, class, 1)?;
    let stream_pin = ctx.pin_native_root(stream);
    let arr: ObjectRef = alloc_ref_array(ctx, vals.len());
    let stream = ctx.read_native_pin(stream_pin, stream);
    ctx.unpin_native_roots(stream_pin);
    for (i, v) in vals.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

/// The synthetic `Stream.mapToLong` / `mapToDouble`: `method` (`applyAsLong` /
/// `applyAsDouble`, descriptor `desc`) over every element, `default` for an
/// element whose application fails or answers nothing (the historical
/// behaviour), collected into a synthetic `class` stream.
///
/// gc-common w20-a: the mapper and every element are pinned across the walk
/// and read back before each application. The inline closures this replaces
/// dispatched on the mapper and passed each element through the addresses
/// read before `stream_elements` and the earlier applications ran Java.
pub(crate) fn synthetic_stream_map_to_primitive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    desc: &str,
    default: Value,
    class: &str,
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mapper_pin = ctx.pin_native_root(mapper);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(mapper_pin);
            return Err(e);
        }
    };
    let (_, handles) = pin_value_slice(ctx, &elements);
    let mut mapped: Vec<Value> = Vec::with_capacity(elements.len());
    for (e, &h) in elements.iter().zip(handles.iter()) {
        let mapper = ctx.read_native_pin(mapper_pin, mapper);
        let e = read_pinned_elem(ctx, h, *e);
        mapped.push(
            ctx.invoke_virtual(mapper, method, desc, &[e])
                .ok()
                .flatten()
                .unwrap_or(default),
        );
    }
    ctx.unpin_native_roots(mapper_pin);
    synthetic_primitive_stream_of(ctx, class, &mapped)
}

/// The synthetic `Stream.flatMapToInt` / `flatMapToLong` / `flatMapToDouble`:
/// `mapper.apply(e).toArray()` (descriptor `to_array_desc`) for every element,
/// concatenated into a synthetic `class` stream. An element whose `apply`
/// fails or answers no stream contributes nothing (the historical behaviour).
///
/// gc-common w20-a: every element is pinned across the walk and read back
/// before its `apply` (the inline closures passed the elements through the
/// addresses read before the earlier applications ran Java), and the mapper's
/// pin is released on every path.
pub(crate) fn synthetic_stream_flat_map_to_primitive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    to_array_desc: &str,
    class: &str,
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-safety: `apply` is an arbitrary user lambda, dispatched on `mapper`
    // once per element, and `mapper` came out of `args` before
    // `stream_elements` re-entered Java.
    let mapper_pin = ctx.pin_native_root(mapper);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(mapper_pin);
            return Err(e);
        }
    };
    let (_, handles) = pin_value_slice(ctx, &elements);
    let mut flat: Vec<Value> = Vec::new();
    for (e, &h) in elements.iter().zip(handles.iter()) {
        let mapper = ctx.read_native_pin(mapper_pin, mapper);
        let e = read_pinned_elem(ctx, h, *e);
        let sub = ctx
            .invoke_virtual(
                mapper,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[e],
            )
            .ok()
            .flatten();
        if let Some(Value::Object(Some(sub_stream))) = sub {
            let sub_arr: ObjectRef =
                match ctx.invoke_virtual(sub_stream, "toArray", to_array_desc, &[]) {
                    Ok(Some(Value::Object(Some(a)))) => a,
                    _ => continue,
                };
            // Primitive elements: nothing to pin, and no GC point in the copy.
            let len = ctx.array_length(sub_arr);
            for i in 0..len {
                flat.push(ctx.get_array_element(sub_arr, i));
            }
        }
    }
    ctx.unpin_native_roots(mapper_pin);
    synthetic_primitive_stream_of(ctx, class, &flat)
}

// ===========================================================================
// Collectors — tagged synthetic objects
// ===========================================================================

pub(crate) const COLLECTOR_FIELD_TAG: usize = 0;
pub(crate) const COLLECTOR_FIELD_ARG1: usize = 1;
pub(crate) const COLLECTOR_FIELD_ARG2: usize = 2;
pub(crate) const COLLECTOR_FIELD_ARG3: usize = 3;
pub(crate) const COLLECTOR_FIELD_ARG4: usize = 4;
pub(crate) const COLLECTOR_NUM_FIELDS: usize = 5;

// `pub` where `native-builtins` needs the value: its `phases_late::streams`
// registers `Collectors` factories this table is the sole decoder for, and its
// `P56_COLLECTOR_*` constants are aliases of these so the two crates cannot
// drift into two colliding namespaces again.
pub const COLLECTOR_TAG_TO_LIST: i32 = 1;
pub const COLLECTOR_TAG_TO_SET: i32 = 2;
pub(crate) const COLLECTOR_TAG_JOINING: i32 = 3;
pub(crate) const COLLECTOR_TAG_JOINING_DELIM: i32 = 4;
pub(crate) const COLLECTOR_TAG_TO_MAP: i32 = 5;
pub(crate) const COLLECTOR_TAG_COUNTING: i32 = 6;
pub(crate) const COLLECTOR_TAG_GROUPING_BY: i32 = 7;
pub(crate) const COLLECTOR_TAG_PARTITIONING_BY: i32 = 8;
/// groupingBy(classifier, downstream) — arg1=classifier, arg2=downstream Collector
pub(crate) const COLLECTOR_TAG_GROUPING_BY_DOWNSTREAM: i32 = 9;
/// T2.3.18 — `Collectors.groupingBy(Function,Supplier,Collector)`.
/// ARG1=classifier, ARG2=supplier (ignored — we always build a HashMap since
/// every supported Supplier from Collectors.* yields some Map subtype our
/// synthetic HashMap natively satisfies), ARG3=downstream Collector.
pub(crate) const COLLECTOR_TAG_GROUPING_BY_SUPPLIER: i32 = 10;
/// T2.3.19 — `Collectors.partitioningBy(Predicate,Collector)` with
/// downstream collector applied to each partition.
pub(crate) const COLLECTOR_TAG_PARTITIONING_BY_DOWNSTREAM: i32 = 11;
/// `Collectors.toMap(keyFn, valFn, mergeFn)` — ARG1=keyFn, ARG2=valFn,
/// ARG3=BinaryOperator merge function applied on duplicate keys.
pub(crate) const COLLECTOR_TAG_TO_MAP_MERGE: i32 = 12;
/// `Collectors.collectingAndThen(downstream, finisher)` — ARG1=downstream
/// Collector, ARG2=finisher Function applied to the downstream result.
pub const COLLECTOR_TAG_COLLECTING_AND_THEN: i32 = 13;
/// `Collectors.toCollection(Supplier)` — ARG1=Supplier producing the target
/// Collection. Stream elements are added to the supplied Collection via
/// `Collection.add(Object)`. Required by Spring Boot 4.x
/// `AutoConfigurationImportSelector.AutoConfigurationGroup.selectImports` which
/// collects entries into a user-supplied LinkedHashSet.
pub(crate) const COLLECTOR_TAG_TO_COLLECTION: i32 = 14;
/// `Collectors.mapping(Function, Collector)` — ARG1=mapper Function applied to
/// each element, ARG2=downstream Collector that accumulates the mapped values.
/// Registered synthetic so the real `Collectors.mapping` bytecode never runs:
/// that bytecode eagerly calls `downstream.accumulator()`, and our synthetic
/// downstream collectors (whose runtime class is the bare `java/util/stream/
/// Collector` interface) have no concrete `accumulator()` body → the JDK throws
/// `AbstractMethodError: Collector.accumulator() has no Code attribute`. Keeping
/// `mapping` synthetic lets it compose with our tagged downstream collectors via
/// the same recursive sub-stream protocol used by groupingBy(downstream).
pub const COLLECTOR_TAG_MAPPING: i32 = 15;
/// `Collectors.toMap(keyFn, valFn, mergeFn, supplier)`.
/// ARG1=keyFn, ARG2=valFn, ARG3=mergeFn, ARG4=supplier.
pub(crate) const COLLECTOR_TAG_TO_MAP_SUPPLIER: i32 = 16;
// ---------------------------------------------------------------------------
// Tags 17+ arrived when a SECOND collector tag namespace was folded into this
// one. `native-builtins`' `phases_late::streams` used to mint its own
// `P56_COLLECTOR_*` numbers (maxBy=9, minBy=10, filtering=12,
// summarizingInt/Long/Double=13/14/15, …) that nothing ever decoded — this
// file owns the only registered `Stream.collect(Collector)` — while aliasing
// live tags above. `Collectors.minBy(cmp)` was stamped 10, so
// `stream.collect(minBy(cmp))` came back through GROUPING_BY_SUPPLIER and
// answered a Map instead of an Optional; maxBy/filtering/summarizing* were
// wrong the same way. Every producer, in either crate, now mints through the
// `make_*_collector` helpers below, so there is one namespace to keep
// consistent: a new tag must be added here, to `is_known_collector_tag`, and
// to the `native_stream_collect` match.
/// `Collectors.minBy(Comparator)` — ARG1=comparator. Reduces to an `Optional`
/// (empty when the stream is empty), like `Stream.min(Comparator)`.
pub(crate) const COLLECTOR_TAG_MIN_BY: i32 = 17;
/// `Collectors.maxBy(Comparator)` — ARG1=comparator. See [`COLLECTOR_TAG_MIN_BY`].
pub(crate) const COLLECTOR_TAG_MAX_BY: i32 = 18;
/// `Collectors.filtering(Predicate, Collector)` — ARG1=predicate, ARG2=downstream
/// Collector fed only the elements the predicate accepts.
pub(crate) const COLLECTOR_TAG_FILTERING: i32 = 19;
/// `Collectors.summarizingInt(ToIntFunction)` — ARG1=extractor. Yields a
/// `java.util.IntSummaryStatistics` in the layout `native-builtins`'
/// `phases_late::streams` accessors read (see [`STATS_FIELD_COUNT`]).
pub(crate) const COLLECTOR_TAG_SUMMARIZING_INT: i32 = 20;
/// `Collectors.summarizingLong(ToLongFunction)` — ARG1=extractor.
pub(crate) const COLLECTOR_TAG_SUMMARIZING_LONG: i32 = 21;
/// `Collectors.summarizingDouble(ToDoubleFunction)` — ARG1=extractor.
pub(crate) const COLLECTOR_TAG_SUMMARIZING_DOUBLE: i32 = 22;
/// `Collectors.averagingInt(ToIntFunction)` — ARG1=extractor. Yields a boxed
/// `Double` (0.0 for an empty stream), matching the JDK.
pub(crate) const COLLECTOR_TAG_AVERAGING_INT: i32 = 23;
/// `Collectors.averagingLong(ToLongFunction)` — ARG1=extractor.
pub(crate) const COLLECTOR_TAG_AVERAGING_LONG: i32 = 24;
/// `Collectors.averagingDouble(ToDoubleFunction)` — ARG1=extractor.
pub(crate) const COLLECTOR_TAG_AVERAGING_DOUBLE: i32 = 25;
/// `Collectors.summingInt(ToIntFunction)` — ARG1=extractor. Yields a boxed
/// `Integer`; the sum wraps at 32 bits exactly as the JDK's `int` accumulator does.
pub(crate) const COLLECTOR_TAG_SUMMING_INT: i32 = 26;
/// `Collectors.summingLong(ToLongFunction)` — ARG1=extractor. Yields a boxed `Long`.
pub(crate) const COLLECTOR_TAG_SUMMING_LONG: i32 = 27;
/// `Collectors.summingDouble(ToDoubleFunction)` — ARG1=extractor. Yields a boxed `Double`.
pub(crate) const COLLECTOR_TAG_SUMMING_DOUBLE: i32 = 28;
/// `Collectors.teeing(Collector, Collector, BiFunction)` — ARG1/ARG2 are the two
/// downstream Collectors, both fed the SAME elements, ARG3 the BiFunction that
/// merges their two results. Registered synthetic for the reason `mapping` is
/// (see [`COLLECTOR_TAG_MAPPING`]): the real `Collectors.teeing` bytecode calls
/// `downstream.accumulator()` eagerly, which AbstractMethodErrors on our tagged
/// synthetic downstreams. Until this tag existed, `teeing` handed back a bare
/// `toList` collector and dropped both downstreams and the merger on the floor.
pub(crate) const COLLECTOR_TAG_TEEING: i32 = 29;

// Field layout of the synthetic `java.util.{Int,Long,Double}SummaryStatistics`
// the summarizing collectors build. Mirrors — and must stay in step with — the
// `STATS_FIELD_*` layout that `native-builtins`' `phases_late::streams`
// allocates and whose registered getters (`getCount`/`getSum`/`getMin`/
// `getMax`/`accept`) read, and the real JDK's field declaration order.
pub(crate) const STATS_FIELD_COUNT: usize = 0;
pub(crate) const STATS_FIELD_SUM: usize = 1;
pub(crate) const STATS_FIELD_MIN: usize = 2;
pub(crate) const STATS_FIELD_MAX: usize = 3;
pub(crate) const STATS_NUM_FIELDS: usize = 4;

pub(crate) fn register_collectors_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/stream/Collectors";

    r.register(
        c,
        "toList",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_list, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toSet",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toUnmodifiableList",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_unmodifiable_list, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toUnmodifiableSet",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_unmodifiable_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_to_map, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;",
        native_collectors_to_map_merge, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;Ljava/util/function/Supplier;)Ljava/util/stream/Collector;",
        native_collectors_to_map_supplier, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toUnmodifiableMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_to_unmodifiable_map, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toUnmodifiableMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;",
        native_collectors_to_unmodifiable_map_merge, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "collectingAndThen",
        "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_collecting_and_then, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "joining",
        "()Ljava/util/stream/Collector;",
        native_collectors_joining, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "joining",
        "(Ljava/lang/CharSequence;)Ljava/util/stream/Collector;",
        native_collectors_joining_delim, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "joining",
        "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/util/stream/Collector;",
        native_collectors_joining_full, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "counting",
        "()Ljava/util/stream/Collector;",
        native_collectors_counting, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "groupingBy",
        "(Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_grouping_by, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "groupingBy",
        "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_grouping_by_downstream, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "partitioningBy",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Collector;",
        native_collectors_partitioning_by, cratonvm_native_api::NativeKind::Bridge,
    );
    // T2.3.18 — groupingBy(Function, Supplier, Collector)
    r.register(
        c,
        "groupingBy",
        "(Ljava/util/function/Function;Ljava/util/function/Supplier;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_grouping_by_supplier, cratonvm_native_api::NativeKind::Bridge,
    );
    // T2.3.19 — partitioningBy(Predicate, Collector)
    r.register(
        c,
        "partitioningBy",
        "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_partitioning_by_downstream, cratonvm_native_api::NativeKind::Bridge,
    );
    // toCollection(Supplier) — needed by Spring Boot 4.x
    // AutoConfigurationImportSelector which collects into a user-supplied
    // LinkedHashSet.
    r.register(
        c,
        "toCollection",
        "(Ljava/util/function/Supplier;)Ljava/util/stream/Collector;",
        native_collectors_to_collection, cratonvm_native_api::NativeKind::Bridge,
    );
    // mapping(Function, Collector) — adapts a downstream collector by mapping
    // each element first. Must be synthetic: the real bytecode calls
    // `downstream.accumulator()`, which AbstractMethodErrors on our tagged
    // synthetic downstream collectors. Exercised by keycloak MapperTypeSerializer
    // (groupingBy(key, mapping(value, toUnmodifiableList()))).
    r.register(
        c,
        "mapping",
        "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_mapping, cratonvm_native_api::NativeKind::Bridge,
    );
    // minBy/maxBy/filtering/summarizing*/averaging*/summing*. `native-builtins`
    // (phase 56) registers the same factories and mints through the shared
    // `make_*_collector` helpers, so whichever registration wins produces the
    // same object. Before tags 17+ these were produced ONLY there, in a second
    // tag namespace nothing decodes — `collect(minBy(cmp))` answered a Map.
    r.register(
        c,
        "minBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        native_collectors_min_by, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "maxBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        native_collectors_max_by, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "filtering",
        "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_filtering, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "summarizingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        native_collectors_summarizing_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "summarizingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        native_collectors_summarizing_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "summarizingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        native_collectors_summarizing_double, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "averagingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        native_collectors_averaging_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "averagingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        native_collectors_averaging_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "averagingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        native_collectors_averaging_double, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "summingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        native_collectors_summing_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "summingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;",
        native_collectors_summing_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "summingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;",
        native_collectors_summing_double, cratonvm_native_api::NativeKind::Bridge,
    );
    // teeing — also registered by native-builtins (phase 64), which delegates to
    // the same mint helper.
    r.register(
        c,
        "teeing",
        "(Ljava/util/stream/Collector;Ljava/util/stream/Collector;Ljava/util/function/BiFunction;)Ljava/util/stream/Collector;",
        native_collectors_teeing, cratonvm_native_api::NativeKind::Bridge,
    );
    // Our synthetic Collector objects need a `characteristics()` method that
    // returns a non-null Set — JDK stream internals (e.g.
    // ReduceOps$3.getOpFlags) call `collector.characteristics().contains(UNORDERED)`,
    // which NPEs if characteristics returns null. We return an empty HashSet so
    // the stream treats the collector as ordered, matching Collectors.toList /
    // toCollection semantics for the cases we synthesize.
    r.register(
        "java/util/stream/Collector",
        "characteristics",
        "()Ljava/util/Set;",
        native_collector_characteristics, cratonvm_native_api::NativeKind::Bridge,
    );
    // Our synthetic tagged Collectors are driven internally by native_stream_collect
    // (tag dispatch), but REAL bytecode consumers (e.g. Mutiny's Multi.collect()
    // .asList()) follow the JLS Collector contract: supplier().get(),
    // accumulator().accept(c, e), finisher().apply(c), combiner().apply(a, b).
    // Those four methods were abstract-with-no-body on the synthetic Collector, so
    // such consumers hit AbstractMethodError (ReactiveAdapterRegistryTests.toMulti).
    // Hand back synthetic functional objects (class == the function interface, so
    // they pass `(Supplier) ...` / `(BiConsumer) ...` casts and route their SAM
    // here) that build/append the accumulation container for the container
    // collectors. Real lambdas have lambda-proxy classes, not these interface
    // names, so the SAM registrations below never intercept them.
    r.register(
        "java/util/stream/Collector",
        "supplier",
        "()Ljava/util/function/Supplier;",
        native_collector_supplier, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/stream/Collector",
        "accumulator",
        "()Ljava/util/function/BiConsumer;",
        native_collector_accumulator, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/stream/Collector",
        "finisher",
        "()Ljava/util/function/Function;",
        native_collector_finisher, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/stream/Collector",
        "combiner",
        "()Ljava/util/function/BinaryOperator;",
        native_collector_combiner, cratonvm_native_api::NativeKind::Bridge,
    );
    // Some real-JDK interface dispatch paths can arrive with a tagged
    // synthetic Collector whose runtime class has collapsed to Object. Keep
    // the standard Collector contract methods available under Object as a
    // defensive bridge.
    //
    // CAVEAT (collections-interception audit, 2026-08-01) — this comment used
    // to end "…; the callbacks still validate the receiver layout/tag", and
    // that is NOT true of these four: `make_collector_fn` wraps whatever
    // receiver it is handed, unconditionally. `java/lang/Object` is the
    // universal base, so the dispatch walk reaches these for ANY receiver that
    // is sent `supplier()`/`accumulator()`/`finisher()`/`combiner()` and whose
    // own class and every superclass leave it undeclared — including a receiver
    // whose implementation is an interface DEFAULT method, which the walk has
    // not consulted yet. Validation does happen, but one call later and only
    // partially: the SAM natives (`native_collfn_supplier_get` and friends)
    // check `collector_tag_of` and degrade to an empty list/map rather than
    // failing. See `docs/feature-designs/collections-interception.md`, Residual 1.
    // Left as-is deliberately: there is no way for a native to decline a call
    // (`MethodCallResult` has no "not handled" arm), so the only fail-closed
    // options are to throw — which would break any legitimate default-method
    // receiver — or to drop these four registrations, which needs the
    // collapsed-to-Object Collector path re-tested first.
    r.register(
        "java/lang/Object",
        "supplier",
        "()Ljava/util/function/Supplier;",
        native_collector_supplier, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/lang/Object",
        "accumulator",
        "()Ljava/util/function/BiConsumer;",
        native_collector_accumulator, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/lang/Object",
        "finisher",
        "()Ljava/util/function/Function;",
        native_collector_finisher, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/lang/Object",
        "combiner",
        "()Ljava/util/function/BinaryOperator;",
        native_collector_combiner, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/function/Supplier",
        "get",
        "()Ljava/lang/Object;",
        native_collfn_supplier_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/function/BiConsumer",
        "accept",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_collfn_accumulator_accept, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/function/Function",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_collfn_finisher_apply, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/function/BinaryOperator",
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_collfn_combiner_apply, cratonvm_native_api::NativeKind::Bridge,
    );
    }

pub(crate) fn is_known_collector_tag(tag: i32) -> bool {
    matches!(
        tag,
        COLLECTOR_TAG_TO_LIST
            | COLLECTOR_TAG_TO_SET
            | COLLECTOR_TAG_JOINING
            | COLLECTOR_TAG_JOINING_DELIM
            | COLLECTOR_TAG_TO_MAP
            | COLLECTOR_TAG_COUNTING
            | COLLECTOR_TAG_GROUPING_BY
            | COLLECTOR_TAG_PARTITIONING_BY
            | COLLECTOR_TAG_GROUPING_BY_DOWNSTREAM
            | COLLECTOR_TAG_GROUPING_BY_SUPPLIER
            | COLLECTOR_TAG_PARTITIONING_BY_DOWNSTREAM
            | COLLECTOR_TAG_TO_MAP_MERGE
            | COLLECTOR_TAG_COLLECTING_AND_THEN
            | COLLECTOR_TAG_TO_COLLECTION
            | COLLECTOR_TAG_MAPPING
            | COLLECTOR_TAG_TO_MAP_SUPPLIER
            | COLLECTOR_TAG_MIN_BY
            | COLLECTOR_TAG_MAX_BY
            | COLLECTOR_TAG_FILTERING
            | COLLECTOR_TAG_SUMMARIZING_INT
            | COLLECTOR_TAG_SUMMARIZING_LONG
            | COLLECTOR_TAG_SUMMARIZING_DOUBLE
            | COLLECTOR_TAG_AVERAGING_INT
            | COLLECTOR_TAG_AVERAGING_LONG
            | COLLECTOR_TAG_AVERAGING_DOUBLE
            | COLLECTOR_TAG_SUMMING_INT
            | COLLECTOR_TAG_SUMMING_LONG
            | COLLECTOR_TAG_SUMMING_DOUBLE
            | COLLECTOR_TAG_TEEING
    )
}

/// True iff `v` is one of our synthetic tagged `java/util/stream/Collector`
/// objects; if so, returns its tag.
pub(crate) fn collector_tag_of(ctx: &mut dyn NativeContext, v: Value) -> Option<i32> {
    if let Value::Object(Some(c)) = v {
        let is_named_collector = ctx
            .class_name_arc_of_id(ctx.class_id_of_object(c))
            .as_deref()
            == Some("java/util/stream/Collector");
        let has_collector_layout = ctx.object_num_fields(c) >= COLLECTOR_NUM_FIELDS;
        if (is_named_collector || has_collector_layout)
            && matches!(ctx.get_field(c, COLLECTOR_FIELD_TAG), Value::Int(t) if is_known_collector_tag(t))
        {
            if let Value::Int(t) = ctx.get_field(c, COLLECTOR_FIELD_TAG) {
                return Some(t);
            }
        }
    }
    None
}

/// The source `Collector` a synthetic collector-function object carries in
/// field 0, or `Object(None)` when the receiver is not one.
///
/// `make_collector_fn` allocates these with exactly one field, but the SAM
/// natives that read that field are registered on the PUBLIC functional
/// interfaces, so a receiver with no fields at all can reach them — a
/// non-capturing user lambda proxy, for instance. Reading slot 0 of a
/// zero-slot object is an out-of-bounds field read: the heap drops it, but not
/// before `CRATONVM_DBG`-grade machinery captures a full Rust backtrace per
/// call. Ask the layout first.
pub(crate) fn collector_fn_source(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    if ctx.object_num_fields(this) == 0 {
        return Value::Object(None);
    }
    ctx.get_field(this, 0)
}

/// `Collector.supplier()/accumulator()/finisher()/combiner()` — return a synthetic
/// functional object (class == the SAM interface) carrying the source Collector in
/// field 0 so its SAM (registered above) can read the tag.
pub(crate) fn make_collector_fn(ctx: &mut dyn NativeContext, args: &[Value], iface: &str) -> MethodCallResult {
    let coll = args.first().copied().unwrap_or(Value::Object(None));
    let f = try_alloc_synthetic(ctx, iface, 1)?;
    ctx.set_field(f, 0, coll);
    Ok(Some(Value::Object(Some(f))))
}

pub(crate) fn native_collector_supplier(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    make_collector_fn(ctx, args, "java/util/function/Supplier")
}
pub(crate) fn native_collector_accumulator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    make_collector_fn(ctx, args, "java/util/function/BiConsumer")
}
pub(crate) fn native_collector_finisher(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    make_collector_fn(ctx, args, "java/util/function/Function")
}
pub(crate) fn native_collector_combiner(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    make_collector_fn(ctx, args, "java/util/function/BinaryOperator")
}

/// `supplier.get()` - fresh accumulation container for the source collector.
pub(crate) fn native_collfn_supplier_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return make_list_of(ctx, &[]),
    };
    let coll = collector_fn_source(ctx, this);
    match collector_tag_of(ctx, coll) {
        Some(COLLECTOR_TAG_TO_SET) => make_set_of(ctx, &[]),
        Some(COLLECTOR_TAG_TO_MAP) | Some(COLLECTOR_TAG_TO_MAP_MERGE) => make_map_of(ctx, &[]),
        Some(COLLECTOR_TAG_TO_MAP_SUPPLIER) => {
            if let Value::Object(Some(c)) = coll {
                if let Value::Object(Some(user)) = ctx.get_field(c, COLLECTOR_FIELD_ARG4) {
                    if let Ok(Some(Value::Object(Some(map)))) =
                        ctx.invoke_virtual(user, "get", "()Ljava/lang/Object;", &[])
                    {
                        return Ok(Some(Value::Object(Some(map))));
                    }
                }
            }
            make_map_of(ctx, &[])
        }
        Some(COLLECTOR_TAG_TO_COLLECTION) => {
            if let Value::Object(Some(c)) = coll {
                if let Value::Object(Some(user)) = ctx.get_field(c, COLLECTOR_FIELD_ARG1) {
                    if let Ok(r @ Some(_)) =
                        ctx.invoke_virtual(user, "get", "()Ljava/lang/Object;", &[])
                    {
                        return Ok(r);
                    }
                }
            }
            make_list_of(ctx, &[])
        }
        _ => make_list_of(ctx, &[]),
    }
}

/// `accumulator.accept(container, element)` - append to collection or map collector.
pub(crate) fn native_collfn_accumulator_accept(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let container = args.get(1).copied().unwrap_or(Value::Object(None));
    let item = args.get(2).copied().unwrap_or(Value::Object(None));
    let coll = collector_fn_source(ctx, this);
    match collector_tag_of(ctx, coll) {
        Some(COLLECTOR_TAG_TO_MAP)
        | Some(COLLECTOR_TAG_TO_MAP_MERGE)
        | Some(COLLECTOR_TAG_TO_MAP_SUPPLIER) => {
            let map = match container {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            let c = match coll {
                Value::Object(Some(c)) => c,
                _ => return Ok(None),
            };
            let key_fn = match ctx.get_field(c, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(f)) => f,
                _ => return Ok(None),
            };
            let val_fn = match ctx.get_field(c, COLLECTOR_FIELD_ARG2) {
                Value::Object(Some(f)) => f,
                _ => return Ok(None),
            };
            // Decided once: the collector's tag does not change, and reading
            // it again after the Java calls below went through a stale `coll`.
            let plain_to_map = matches!(collector_tag_of(ctx, coll), Some(COLLECTOR_TAG_TO_MAP));
            // gc-common w20-a: every step below is a Java call. The map, the
            // collector, the item, the value function, the key and the value
            // were all held raw across them (the value function got a stale
            // `item`, `get`/`put` a stale map and key, the merge function was
            // read off a stale collector). All are pinned now; `map_pin` is
            // the frame's base.
            let map_pin = ctx.pin_native_root(map);
            let c_pin = ctx.pin_native_root(c);
            let val_fn_pin = ctx.pin_native_root(val_fn);
            let item_pin = pin_value(ctx, item);
            let outcome = (|| -> Result<(), MethodCallFailed> {
                let key = ctx
                    .invoke_virtual(
                        key_fn,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[item],
                    )?
                    .unwrap_or(Value::Object(None));
                let key_pin = pin_value(ctx, key);
                let val_fn = ctx.read_native_pin(val_fn_pin, val_fn);
                let item = read_pinned_elem(ctx, item_pin, item);
                let value = ctx
                    .invoke_virtual(
                        val_fn,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[item],
                    )?
                    .unwrap_or(Value::Object(None));
                let value_pin = pin_value(ctx, value);
                let mut merged: Option<Value> = None;
                if !plain_to_map {
                    let map = ctx.read_native_pin(map_pin, map);
                    let key = read_pinned_elem(ctx, key_pin, key);
                    let old = ctx
                        .invoke_virtual(
                            map,
                            "get",
                            "(Ljava/lang/Object;)Ljava/lang/Object;",
                            &[key],
                        )?
                        .unwrap_or(Value::Object(None));
                    if !matches!(old, Value::Object(None)) {
                        let c = ctx.read_native_pin(c_pin, c);
                        if let Value::Object(Some(mf)) = ctx.get_field(c, COLLECTOR_FIELD_ARG3) {
                            let value = read_pinned_elem(ctx, value_pin, value);
                            merged = Some(
                                ctx.invoke_virtual(
                                    mf,
                                    "apply",
                                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                                    &[old, value],
                                )?
                                .unwrap_or(Value::Object(None)),
                            );
                        }
                    }
                }
                // A merged value is the fresh result of the last Java call; an
                // unmerged one is read back through its pin.
                let value = merged.unwrap_or_else(|| read_pinned_elem(ctx, value_pin, value));
                let map = ctx.read_native_pin(map_pin, map);
                let key = read_pinned_elem(ctx, key_pin, key);
                ctx.invoke_virtual(
                    map,
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key, value],
                )?;
                Ok(())
            })();
            ctx.unpin_native_roots(map_pin);
            outcome?;
        }
        _ => {
            if let Value::Object(Some(c)) = container {
                ctx.invoke_virtual(c, "add", "(Ljava/lang/Object;)Z", &[item])?;
            }
        }
    }
    Ok(None)
}

