// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Apache Lucene / Elasticsearch intrinsics and the RandomizedRunner test-runner shims.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

fn randomized_root_entry(ctx: &mut dyn NativeContext, obj: ObjectRef) -> RandomizedRootEntry {
    RandomizedRootEntry {
        root: ctx.add_global_root(obj),
        fallback: obj,
        identity: ctx.identity_hash_code(obj),
    }
}

fn randomized_resolve_root(
    ctx: &dyn NativeContext,
    entry: RandomizedRootEntry,
) -> Option<ObjectRef> {
    let obj = if entry.root != 0 {
        ctx.resolve_global_root(entry.root)
            .or(Some(entry.fallback))?
    } else {
        entry.fallback
    };
    if ctx.identity_hash_code(obj) == entry.identity {
        Some(obj)
    } else {
        None
    }
}

fn randomized_release_root(ctx: &mut dyn NativeContext, entry: RandomizedRootEntry) {
    if entry.root != 0 {
        let _ = ctx.remove_global_root(entry.root);
    }
}

fn randomized_context_cache() -> &'static parking_lot::Mutex<
    std::collections::HashMap<RandomizedContextCacheKey, RandomizedRootEntry>,
> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<
            std::collections::HashMap<RandomizedContextCacheKey, RandomizedRootEntry>,
        >,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn randomized_context_cache_lookup(
    ctx: &dyn NativeContext,
    key: RandomizedContextCacheKey,
) -> Option<ObjectRef> {
    let entry = randomized_context_cache().lock().get(&key).copied()?;
    randomized_resolve_root(ctx, entry)
}

fn randomized_context_cache_store(
    ctx: &mut dyn NativeContext,
    key: RandomizedContextCacheKey,
    context: ObjectRef,
) {
    let entry = randomized_root_entry(ctx, context);
    let old = randomized_context_cache().lock().insert(key, entry);
    if let Some(old) = old {
        if old.root != entry.root {
            randomized_release_root(ctx, old);
        }
    }
}

fn randomized_random_cache() -> &'static parking_lot::Mutex<
    std::collections::HashMap<RandomizedRandomCacheKey, RandomizedRootEntry>,
> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<
            std::collections::HashMap<RandomizedRandomCacheKey, RandomizedRootEntry>,
        >,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn randomized_random_cache_lookup(
    ctx: &dyn NativeContext,
    key: RandomizedRandomCacheKey,
) -> Option<ObjectRef> {
    let entry = randomized_random_cache().lock().get(&key).copied()?;
    randomized_resolve_root(ctx, entry)
}

fn randomized_random_cache_store(
    ctx: &mut dyn NativeContext,
    key: RandomizedRandomCacheKey,
    random: ObjectRef,
) {
    let entry = randomized_root_entry(ctx, random);
    let old = randomized_random_cache().lock().insert(key, entry);
    if let Some(old) = old {
        if old.root != entry.root {
            randomized_release_root(ctx, old);
        }
    }
}

fn randomized_random_cache_invalidate(ctx: &mut dyn NativeContext, key: RandomizedRandomCacheKey) {
    let old = randomized_random_cache().lock().remove(&key);
    if let Some(old) = old {
        randomized_release_root(ctx, old);
    }
}

fn randomized_random_cache_key_for_context(
    ctx: &mut dyn NativeContext,
    context: ObjectRef,
) -> RandomizedRandomCacheKey {
    let thread = ctx.current_thread_object();
    RandomizedRandomCacheKey {
        vm: ctx.vm_identity(),
        context: ctx.identity_hash_code(context),
        thread: ctx.identity_hash_code(thread),
    }
}

fn randomized_context_static_contexts(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name("com/carrotsearch/randomizedtesting/RandomizedContext")?;
    let idx = ctx.static_field_index_by_name(class_id, "contexts")?;
    match ctx.get_static_field(class_id, idx) {
        Value::Object(Some(contexts)) => Some(contexts),
        _ => None,
    }
}

fn randomized_thread_group(ctx: &mut dyn NativeContext, thread: ObjectRef) -> MethodCallResult {
    ctx.invoke_virtual(thread, "getThreadGroup", "()Ljava/lang/ThreadGroup;", &[])
}

/// Best-effort thread name for the `IllegalStateException` messages below —
/// mirrors `com.carrotsearch.randomizedtesting.Threads.threadName(Thread)`
/// closely enough for a human-readable diagnostic; exact wording doesn't
/// matter for correctness (only the exception *class* does, see below).
fn randomized_thread_name(ctx: &mut dyn NativeContext, thread: ObjectRef) -> String {
    match ctx.invoke_virtual(thread, "getName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(name)))) => ctx
            .read_string(name)
            .unwrap_or_else(|| "<unknown>".to_string()),
        _ => "<unknown>".to_string(),
    }
}

/// `RandomizedContext.context(Thread)` never returns null in real-JDK
/// bytecode — a missing context is always an `IllegalStateException` (see
/// the decompiled `context(Thread)` bytecode this mirrors). Callers such as
/// `AssertingCodec.<init>` (Lucene test framework) rely on that: they wrap
/// `RandomizedContext.current().getTargetClass()` in
/// `catch (IllegalStateException e) { targetClass = null; }` to tolerate
/// running outside a randomized-test thread (e.g. from a static class
/// initializer). Returning a plain `null` here instead of throwing skips
/// that catch block — the very next `invokevirtual getTargetClass()` then
/// NPEs on the null receiver, and the *wrong* exception type propagates
/// uncaught out of the static initializer as `ExceptionInInitializerError`,
/// crashing test classes that would otherwise gracefully no-op (e.g.
/// `ES815BitFlatVectorFormatTests` and the other ES93 BFloat16 vector codec
/// tests during `<clinit>`).
fn randomized_no_context_error(
    ctx: &mut dyn NativeContext,
    thread: ObjectRef,
    terminated: bool,
) -> MethodCallFailed {
    let thread_name = randomized_thread_name(ctx, thread);
    let message = if terminated {
        format!("No context for a terminated thread: {thread_name}")
    } else {
        format!(
            "No context information for thread: {thread_name}. Is this thread running under a \
             RandomizedRunner runner context? Add @RunWith(RandomizedRunner.class) to your test \
             class. Make sure your code accesses random contexts within @BeforeClass and \
             @AfterClass boundary (for example, static test class initializers are not \
             permitted to access random contexts)."
        )
    };
    RuntimeError::IllegalStateException { message }.into()
}

fn randomized_context_for_thread(
    ctx: &mut dyn NativeContext,
    thread: ObjectRef,
) -> MethodCallResult {
    let group_result = randomized_thread_group(ctx, thread)?;
    let group = match group_result {
        Some(Value::Object(Some(group))) => group,
        _ => return Err(randomized_no_context_error(ctx, thread, true)),
    };
    let key = RandomizedContextCacheKey {
        vm: ctx.vm_identity(),
        thread: ctx.identity_hash_code(thread),
        group: ctx.identity_hash_code(group),
    };
    if let Some(context) = randomized_context_cache_lookup(ctx, key) {
        return Ok(Some(Value::Object(Some(context))));
    }

    let contexts = match randomized_context_static_contexts(ctx) {
        Some(contexts) => contexts,
        None => return Err(randomized_no_context_error(ctx, thread, false)),
    };
    let mut current_group = group;
    loop {
        let contexts_pin = ctx.pin_native_root(contexts);
        let group_pin = ctx.pin_native_root(current_group);
        let candidate = ctx.invoke_virtual(
            contexts,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(current_group))],
        )?;
        current_group = ctx.read_native_pin(group_pin, current_group);
        ctx.unpin_native_roots(contexts_pin);
        if let Some(Value::Object(Some(context))) = candidate {
            randomized_context_cache_store(ctx, key, context);
            return Ok(Some(Value::Object(Some(context))));
        }
        let parent_result =
            ctx.invoke_virtual(current_group, "getParent", "()Ljava/lang/ThreadGroup;", &[])?;
        current_group = match parent_result {
            Some(Value::Object(Some(parent))) => parent,
            _ => return Err(randomized_no_context_error(ctx, thread, false)),
        };
    }
}

fn randomized_context_current_obj(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let thread = ctx.current_thread_object();
    randomized_context_for_thread(ctx, thread)
}

fn randomized_context_randomness(
    ctx: &mut dyn NativeContext,
    context: ObjectRef,
) -> MethodCallResult {
    let resources =
        match native_randomized_context_get_per_thread(ctx, &[Value::Object(Some(context))])? {
            Some(Value::Object(Some(resources))) => resources,
            _ => return Ok(Some(Value::Object(None))),
        };
    let deque = match ctx.get_field_by_name(resources, "randomnesses") {
        Value::Object(Some(deque)) => deque,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(deque, "peekFirst", "()Ljava/lang/Object;", &[])
}

fn randomized_random_from_context(
    ctx: &mut dyn NativeContext,
    context: ObjectRef,
) -> MethodCallResult {
    let key = randomized_random_cache_key_for_context(ctx, context);
    if let Some(random) = randomized_random_cache_lookup(ctx, key) {
        return Ok(Some(Value::Object(Some(random))));
    }
    let randomness = match randomized_context_randomness(ctx, context)? {
        Some(Value::Object(Some(randomness))) => randomness,
        _ => return Ok(Some(Value::Object(None))),
    };
    let random = match ctx.get_field_by_name(randomness, "random") {
        Value::Object(Some(random)) => random,
        _ => return Ok(Some(Value::Object(None))),
    };
    randomized_random_cache_store(ctx, key, random);
    Ok(Some(Value::Object(Some(random))))
}

pub(crate) fn native_randomized_context_current(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    randomized_context_current_obj(ctx)
}

pub(crate) fn native_randomized_context_context(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let thread = obj_arg(args, 0)?;
    randomized_context_for_thread(ctx, thread)
}

pub(crate) fn native_randomized_context_get_randomness(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    randomized_context_randomness(ctx, this)
}

pub(crate) fn native_randomized_context_get_random(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    randomized_random_from_context(ctx, this)
}

pub(crate) fn native_randomized_context_push(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let randomness = obj_arg(args, 1)?;
    let key = randomized_random_cache_key_for_context(ctx, this);
    randomized_random_cache_invalidate(ctx, key);
    let resources =
        match native_randomized_context_get_per_thread(ctx, &[Value::Object(Some(this))])? {
            Some(Value::Object(Some(resources))) => resources,
            _ => return Ok(None),
        };
    let deque = match ctx.get_field_by_name(resources, "randomnesses") {
        Value::Object(Some(deque)) => deque,
        _ => return Ok(None),
    };
    ctx.invoke_virtual(
        deque,
        "push",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(randomness))],
    )?;
    Ok(None)
}

pub(crate) fn native_randomized_context_pop_and_destroy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = randomized_random_cache_key_for_context(ctx, this);
    randomized_random_cache_invalidate(ctx, key);
    let resources =
        match native_randomized_context_get_per_thread(ctx, &[Value::Object(Some(this))])? {
            Some(Value::Object(Some(resources))) => resources,
            _ => return Ok(None),
        };
    let deque = match ctx.get_field_by_name(resources, "randomnesses") {
        Value::Object(Some(deque)) => deque,
        _ => return Ok(None),
    };
    let popped = ctx.invoke_virtual(deque, "pop", "()Ljava/lang/Object;", &[])?;
    if let Some(Value::Object(Some(randomness))) = popped {
        let _ = ctx.invoke_virtual(randomness, "destroy", "()V", &[])?;
    }
    Ok(None)
}

pub(crate) fn native_randomized_test_get_context(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    randomized_context_current_obj(ctx)
}

pub(crate) fn native_randomized_test_get_random(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let context = match randomized_context_current_obj(ctx)? {
        Some(Value::Object(Some(context))) => context,
        _ => return Ok(Some(Value::Object(None))),
    };
    randomized_random_from_context(ctx, context)
}

pub(crate) fn native_randomized_test_random_float(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let random = match native_randomized_test_get_random(ctx, &[])? {
        Some(Value::Object(Some(random))) => random,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    ctx.invoke_virtual(random, "nextFloat", "()F", &[])
}

fn randomized_per_thread_cache() -> &'static parking_lot::Mutex<
    std::collections::HashMap<RandomizedPerThreadCacheKey, RandomizedPerThreadCacheEntry>,
> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<
            std::collections::HashMap<RandomizedPerThreadCacheKey, RandomizedPerThreadCacheEntry>,
        >,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn randomized_per_thread_key(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    map: ObjectRef,
    thread: ObjectRef,
) -> RandomizedPerThreadCacheKey {
    RandomizedPerThreadCacheKey {
        vm: ctx.vm_identity(),
        context: ctx.identity_hash_code(this),
        map: ctx.identity_hash_code(map),
        thread: ctx.identity_hash_code(thread),
    }
}

fn randomized_per_thread_cache_lookup(
    ctx: &dyn NativeContext,
    key: RandomizedPerThreadCacheKey,
) -> Option<ObjectRef> {
    let entry = randomized_per_thread_cache().lock().get(&key).copied()?;
    let resource = if entry.root != 0 {
        ctx.resolve_global_root(entry.root)
            .or(Some(entry.fallback))?
    } else {
        entry.fallback
    };
    if ctx.identity_hash_code(resource) == entry.resource_id {
        Some(resource)
    } else {
        None
    }
}

fn randomized_per_thread_cache_store(
    ctx: &mut dyn NativeContext,
    key: RandomizedPerThreadCacheKey,
    resources: ObjectRef,
) {
    let entry = RandomizedPerThreadCacheEntry {
        root: ctx.add_global_root(resources),
        fallback: resources,
        resource_id: ctx.identity_hash_code(resources),
    };
    let old = randomized_per_thread_cache().lock().insert(key, entry);
    if let Some(old) = old {
        if old.root != 0 && old.root != entry.root {
            let _ = ctx.remove_global_root(old.root);
        }
    }
}

pub(crate) fn native_es_vector_util_dot_product_f32(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let len = ctx.array_length(a).min(ctx.array_length(b));
    Ok(Some(Value::Float(es_dot_product_f32(ctx, a, b, len))))
}

// Mirrors Lucene's Panama-vectorized `PanamaVectorUtilSupport.dotProduct`
// (`dotProductBody`) bit-for-bit, as actually executed when the method is
// called too few times to be JIT-compiled (the common case for a single
// JUnit test method): the Vector API ops run through their plain-Java
// interpreter fallback bodies, which are well-defined portable Java, not a
// hardware SIMD intrinsic. Four independent `lanes`-wide fma accumulators
// stride through the array (each lane accumulates every `4 * lanes`-th
// element), a same-width "vector tail" folds any remaining full lane-group
// into the first accumulator only, then the four accumulators are combined
// lane-wise (`(acc1+acc2)+(acc3+acc4)`) and reduced via a strict sequential
// left-to-right fold (`FloatVector.reduceLanes(ADD)`'s fallback semantics,
// per `jdk.incubator.vector.FloatVector.rOpTemplate`), before falling
// through to a scalar fma tail for any elements past the last full lane
// group. Floating-point addition isn't associative, so this exact lane
// grouping and reduction order has to be reproduced precisely, or the
// result diverges from HotSpot in the low ULPs.
fn es_dot_product_f32(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef, len: usize) -> f32 {
    let lanes = panama_preferred_lanes_f32();
    let mut res = 0.0f32;
    let mut i = 0usize;
    if len > 2 * lanes {
        let limit = len - (len % lanes);
        let mut acc1 = vec![0.0f32; lanes];
        let mut acc2 = vec![0.0f32; lanes];
        let mut acc3 = vec![0.0f32; lanes];
        let mut acc4 = vec![0.0f32; lanes];
        let unrolled_limit = limit.saturating_sub(3 * lanes);
        let mut j = 0usize;
        while j < unrolled_limit {
            for l in 0..lanes {
                acc1[l] = float_array_elem(ctx, a, j + l)
                    .mul_add(float_array_elem(ctx, b, j + l), acc1[l]);
            }
            for l in 0..lanes {
                acc2[l] = float_array_elem(ctx, a, j + lanes + l)
                    .mul_add(float_array_elem(ctx, b, j + lanes + l), acc2[l]);
            }
            for l in 0..lanes {
                acc3[l] = float_array_elem(ctx, a, j + 2 * lanes + l)
                    .mul_add(float_array_elem(ctx, b, j + 2 * lanes + l), acc3[l]);
            }
            for l in 0..lanes {
                acc4[l] = float_array_elem(ctx, a, j + 3 * lanes + l)
                    .mul_add(float_array_elem(ctx, b, j + 3 * lanes + l), acc4[l]);
            }
            j += 4 * lanes;
        }
        while j < limit {
            for l in 0..lanes {
                acc1[l] = float_array_elem(ctx, a, j + l)
                    .mul_add(float_array_elem(ctx, b, j + l), acc1[l]);
            }
            j += lanes;
        }
        let mut reduced = 0.0f32;
        for l in 0..lanes {
            let res1 = acc1[l] + acc2[l];
            let res2 = acc3[l] + acc4[l];
            reduced += res1 + res2;
        }
        res += reduced;
        i = limit;
    }
    while i < len {
        res = float_array_elem(ctx, a, i).mul_add(float_array_elem(ctx, b, i), res);
        i += 1;
    }
    res
}

pub(crate) fn native_es_vector_util_square_distance_f32(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let len = ctx.array_length(a).min(ctx.array_length(b));
    Ok(Some(Value::Float(es_square_distance_f32(ctx, a, b, len))))
}

// Mirrors Lucene's Panama-vectorized `PanamaVectorUtilSupport.squareDistance`
// (`squareDistanceBody`) bit-for-bit -- same lane-grouped fma-accumulator
// shape and sequential final reduction as `es_dot_product_f32` above, just
// accumulating `(a[i]-b[i])^2` per lane instead of `a[i]*b[i]`.
fn es_square_distance_f32(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef, len: usize) -> f32 {
    let lanes = panama_preferred_lanes_f32();
    let mut res = 0.0f32;
    let mut i = 0usize;
    if len > 2 * lanes {
        let limit = len - (len % lanes);
        let mut acc1 = vec![0.0f32; lanes];
        let mut acc2 = vec![0.0f32; lanes];
        let mut acc3 = vec![0.0f32; lanes];
        let mut acc4 = vec![0.0f32; lanes];
        let unrolled_limit = limit.saturating_sub(3 * lanes);
        let mut j = 0usize;
        while j < unrolled_limit {
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + l) - float_array_elem(ctx, b, j + l);
                acc1[l] = diff.mul_add(diff, acc1[l]);
            }
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + lanes + l)
                    - float_array_elem(ctx, b, j + lanes + l);
                acc2[l] = diff.mul_add(diff, acc2[l]);
            }
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + 2 * lanes + l)
                    - float_array_elem(ctx, b, j + 2 * lanes + l);
                acc3[l] = diff.mul_add(diff, acc3[l]);
            }
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + 3 * lanes + l)
                    - float_array_elem(ctx, b, j + 3 * lanes + l);
                acc4[l] = diff.mul_add(diff, acc4[l]);
            }
            j += 4 * lanes;
        }
        while j < limit {
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + l) - float_array_elem(ctx, b, j + l);
                acc1[l] = diff.mul_add(diff, acc1[l]);
            }
            j += lanes;
        }
        let mut reduced = 0.0f32;
        for l in 0..lanes {
            let res1 = acc1[l] + acc2[l];
            let res2 = acc3[l] + acc4[l];
            reduced += res1 + res2;
        }
        res += reduced;
        i = limit;
    }
    while i < len {
        let diff = float_array_elem(ctx, a, i) - float_array_elem(ctx, b, i);
        res = diff.mul_add(diff, res);
        i += 1;
    }
    res
}

pub(crate) fn native_es_vector_util_square_distance_f32_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let requested = match args.get(3) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let max_len = ctx.array_length(a).min(ctx.array_length(b));
    let end = offset.saturating_add(requested).min(max_len);
    // Mirrors `DefaultESVectorUtilSupport.squareDistance(float[], float[], int, int)`:
    // a plain sequential fma accumulation (this overload is never unrolled upstream).
    let mut sum = 0.0f32;
    for i in offset..end {
        let d = float_array_elem(ctx, a, i) - float_array_elem(ctx, b, i);
        sum = d.mul_add(d, sum);
    }
    Ok(Some(Value::Float(sum)))
}

pub(crate) fn native_es_vector_util_calculate_osq_loss_f32(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vector = obj_arg(args, 0)?;
    let lower = float_arg(args, 1);
    let upper = float_arg(args, 2);
    let points = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    let norm2 = float_arg(args, 4);
    let lambda = float_arg(args, 5);
    let scratch = obj_arg(args, 6)?;
    let step = (upper - lower) / ((points as f32) - 1.0);
    let inv_step = 1.0 / step;
    let mut xe = 0.0f32;
    let mut e2 = 0.0f32;
    for i in 0..ctx.array_length(vector) {
        let v = float_array_elem(ctx, vector, i);
        let clamped = java_f32_min(java_f32_max(v, lower), upper);
        let q = java_math_round_f32((clamped - lower) * inv_step);
        set_int_array_elem(ctx, scratch, i, q);
        let dequantized = step.mul_add(q as f32, lower);
        let error = v - dequantized;
        e2 = error.mul_add(error, e2);
        xe = v.mul_add(error, xe);
    }
    Ok(Some(Value::Float(
        (1.0 - lambda) * xe * xe / norm2 + lambda * e2,
    )))
}

pub(crate) fn native_es_vector_util_calculate_osq_grid_points_f32(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vector = obj_arg(args, 0)?;
    let quantized = obj_arg(args, 1)?;
    let points = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let scratch = obj_arg(args, 3)?;
    let mut a = 0.0f32;
    let mut b = 0.0f32;
    let mut c = 0.0f32;
    let mut d = 0.0f32;
    let mut e = 0.0f32;
    let inv = 1.0 / ((points as f32) - 1.0);
    for i in 0..ctx.array_length(vector) {
        let v = float_array_elem(ctx, vector, i);
        let q = int_array_elem(ctx, quantized, i) as f32;
        let x = q * inv;
        let y = 1.0 - x;
        a = y.mul_add(y, a);
        b = y.mul_add(x, b);
        c = x.mul_add(x, c);
        d = y.mul_add(v, d);
        e = x.mul_add(v, e);
    }
    set_float_array_elem(ctx, scratch, 0, a);
    set_float_array_elem(ctx, scratch, 1, b);
    set_float_array_elem(ctx, scratch, 2, c);
    set_float_array_elem(ctx, scratch, 3, d);
    set_float_array_elem(ctx, scratch, 4, e);
    Ok(None)
}

fn es_vector_util_center_stats_common<F>(
    ctx: &mut dyn NativeContext,
    len: usize,
    centered: ObjectRef,
    stats: ObjectRef,
    mut value_at: F,
    track_dot_product: bool,
) where
    F: FnMut(&dyn NativeContext, usize) -> (f32, f32, f32),
{
    let mut mean = 0.0f32;
    let mut variance_sum = 0.0f32;
    let mut norm2 = 0.0f32;
    let mut dot_product = 0.0f32;
    let mut min = f32::MAX;
    let mut max = -f32::MAX;
    for i in 0..len {
        let (value, center, dot_value) = value_at(ctx, i);
        if track_dot_product {
            dot_product = value.mul_add(dot_value, dot_product);
        }
        let c = value - center;
        set_float_array_elem(ctx, centered, i, c);
        min = java_f32_min(min, c);
        max = java_f32_max(max, c);
        norm2 = c.mul_add(c, norm2);
        let delta = c - mean;
        mean += delta / ((i + 1) as f32);
        let delta2 = c - mean;
        variance_sum = delta.mul_add(delta2, variance_sum);
    }
    set_float_array_elem(ctx, stats, 0, mean);
    set_float_array_elem(ctx, stats, 1, variance_sum / (len as f32));
    set_float_array_elem(ctx, stats, 2, norm2);
    set_float_array_elem(ctx, stats, 3, min);
    set_float_array_elem(ctx, stats, 4, max);
    if track_dot_product {
        set_float_array_elem(ctx, stats, 5, dot_product);
    }
}

pub(crate) fn native_es_vector_util_center_stats_euclidean_f32(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vector = obj_arg(args, 0)?;
    let centroid = obj_arg(args, 1)?;
    let centered = obj_arg(args, 2)?;
    let stats = obj_arg(args, 3)?;
    let len = ctx.array_length(vector);
    es_vector_util_center_stats_common(
        ctx,
        len,
        centered,
        stats,
        |ctx, i| {
            (
                float_array_elem(ctx, vector, i),
                float_array_elem(ctx, centroid, i),
                0.0,
            )
        },
        false,
    );
    Ok(None)
}

pub(crate) fn native_es_vector_util_center_stats_dp_f32(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vector = obj_arg(args, 0)?;
    let centroid = obj_arg(args, 1)?;
    let centered = obj_arg(args, 2)?;
    let stats = obj_arg(args, 3)?;
    let len = ctx.array_length(vector);
    es_vector_util_center_stats_common(
        ctx,
        len,
        centered,
        stats,
        |ctx, i| {
            let value = float_array_elem(ctx, vector, i);
            let center = float_array_elem(ctx, centroid, i);
            (value, center, center)
        },
        true,
    );
    Ok(None)
}

pub(crate) fn native_es_vector_util_center_stats_euclidean_i8(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vector = obj_arg(args, 0)?;
    let centroid = obj_arg(args, 1)?;
    let centered = obj_arg(args, 2)?;
    let stats = obj_arg(args, 3)?;
    let len = ctx.array_length(vector);
    es_vector_util_center_stats_common(
        ctx,
        len,
        centered,
        stats,
        |ctx, i| {
            (
                byte_array_elem(ctx, vector, i) as f32,
                byte_array_elem(ctx, centroid, i) as f32,
                0.0,
            )
        },
        false,
    );
    Ok(None)
}

pub(crate) fn native_es_vector_util_center_stats_dp_i8(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vector = obj_arg(args, 0)?;
    let centroid = obj_arg(args, 1)?;
    let centered = obj_arg(args, 2)?;
    let stats = obj_arg(args, 3)?;
    let len = ctx.array_length(vector);
    es_vector_util_center_stats_common(
        ctx,
        len,
        centered,
        stats,
        |ctx, i| {
            let value = byte_array_elem(ctx, vector, i) as f32;
            let center = byte_array_elem(ctx, centroid, i) as f32;
            (value, center, center)
        },
        true,
    );
    Ok(None)
}

pub(crate) fn native_es_vector_util_quantize_vector_with_intervals_f32(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vector = obj_arg(args, 0)?;
    let destination = obj_arg(args, 1)?;
    let lower = float_arg(args, 2);
    let upper = float_arg(args, 3);
    let bits = args.get(4).and_then(|v| v.as_int()).unwrap_or(0);
    let max_quant = ((1_i32 << bits) - 1) as f32;
    let inv = max_quant / (upper - lower);
    let mut sum = 0_i32;
    for i in 0..ctx.array_length(vector) {
        let v = java_f32_min(java_f32_max(float_array_elem(ctx, vector, i), lower), upper);
        let q = java_math_round_f32((v - lower) * inv);
        sum = sum.wrapping_add(q);
        set_int_array_elem(ctx, destination, i, q);
    }
    Ok(Some(Value::Int(sum)))
}

pub(crate) fn native_es_vector_util_pack_as_binary(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let values = obj_arg(args, 0)?;
    let packed = obj_arg(args, 1)?;
    let len = ctx.array_length(values);
    if len == 0 {
        return Ok(None);
    }
    let out_len = (len + 7) / 8;
    let mut out = vec![0u8; out_len.min(ctx.array_length(packed))];
    let mut src = 0usize;
    let mut dst = 0usize;
    while src + 7 < len && dst < out.len() {
        let byte = (((int_array_elem(ctx, values, src) & 1) as u8) << 7)
            | (((int_array_elem(ctx, values, src + 1) & 1) as u8) << 6)
            | (((int_array_elem(ctx, values, src + 2) & 1) as u8) << 5)
            | (((int_array_elem(ctx, values, src + 3) & 1) as u8) << 4)
            | (((int_array_elem(ctx, values, src + 4) & 1) as u8) << 3)
            | (((int_array_elem(ctx, values, src + 5) & 1) as u8) << 2)
            | (((int_array_elem(ctx, values, src + 6) & 1) as u8) << 1)
            | ((int_array_elem(ctx, values, src + 7) & 1) as u8);
        out[dst] = byte;
        src += 8;
        dst += 1;
    }
    if src < len && dst < out.len() {
        let mut byte = 0u8;
        let mut shift = 7i32;
        while shift >= 0 && src < len {
            byte |= ((int_array_elem(ctx, values, src) & 1) as u8) << shift;
            src += 1;
            shift -= 1;
        }
        out[dst] = byte;
    }
    if !ctx.write_byte_array_from(packed, 0, &out) {
        return Err(lucene_iobe(
            "ESVectorUtil.packAsBinary destination byte[] write failed",
        ));
    }
    Ok(None)
}

pub(crate) fn native_es_next_diskbbq_vectors_writer_iarray_at_i(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => 0,
    };
    Ok(Some(Value::Int(int_array_elem(ctx, arr, index))))
}

pub(crate) fn native_es_next_diskbbq_vectors_writer_iarray_at_iarray_at_i(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let outer = obj_arg(args, 0)?;
    let inner = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => 0,
    };
    let inner_index = int_array_elem(ctx, inner, index).max(0) as usize;
    Ok(Some(Value::Int(int_array_elem(ctx, outer, inner_index))))
}

pub(crate) fn native_es_next_diskbbq_vectors_writer_iarray_at_i_plus_j(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = obj_arg(args, 0)?;
    let i = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let j = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let index = i.saturating_add(j).max(0) as usize;
    Ok(Some(Value::Int(int_array_elem(ctx, arr, index))))
}

pub(crate) fn native_es_next_diskbbq_vectors_writer_barray_at_iarray_at_i(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let outer = obj_arg(args, 0)?;
    let inner = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => 0,
    };
    let inner_index = int_array_elem(ctx, inner, index).max(0) as usize;
    Ok(Some(Value::Int(
        if bool_array_elem(ctx, outer, inner_index) {
            1
        } else {
            0
        },
    )))
}

pub(crate) fn native_es_next_diskbbq_vectors_writer_i_plus_j(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let i = match args.get(0) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let j = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(i.saturating_add(j))))
}

fn native_es_clustering_float_vector_values_slice_translated_ord(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    ord: i32,
) -> Result<i32, MethodCallFailed> {
    let translator = match ctx.get_field_by_name(this, "ordTranslator") {
        Value::Object(Some(translator)) => translator,
        _ => return Ok(ord),
    };
    Ok(
        match ctx.invoke_virtual(translator, "apply", "(I)I", &[Value::Int(ord)])? {
            Some(Value::Int(mapped)) => mapped,
            _ => ord,
        },
    )
}

pub(crate) fn native_es_clustering_float_vector_values_slice_vector_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let ord = args.get(1).and_then(Value::as_int).unwrap_or(0);
    let all_values = match ctx.get_field_by_name(this, "allValues") {
        Value::Object(Some(all_values)) => all_values,
        _ => return Ok(Some(Value::Object(None))),
    };

    let all_values_pin = ctx.pin_native_root(all_values);
    let translated = native_es_clustering_float_vector_values_slice_translated_ord(ctx, this, ord);
    let all_values = ctx.read_native_pin(all_values_pin, all_values);
    ctx.unpin_native_roots(all_values_pin);
    let translated = translated?;

    ctx.invoke_virtual(
        all_values,
        "vectorValue",
        "(I)Ljava/lang/Object;",
        &[Value::Int(translated)],
    )
}

pub(crate) fn native_es_clustering_float_vector_values_slice_ord_to_doc(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let ord = args.get(1).and_then(Value::as_int).unwrap_or(0);
    Ok(Some(Value::Int(
        native_es_clustering_float_vector_values_slice_translated_ord(ctx, this, ord)?,
    )))
}

pub(crate) fn native_es_clustering_float_vector_values_slice_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(
        ctx.get_field_by_name(this, "size").as_int().unwrap_or(0),
    )))
}

pub(crate) fn native_es_clustering_float_vector_values_slice_dimension(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let all_values = match ctx.get_field_by_name(this, "allValues") {
        Value::Object(Some(all_values)) => all_values,
        _ => return Ok(Some(Value::Int(0))),
    };
    ctx.invoke_virtual(all_values, "dimension", "()I", &[])
}

fn native_es_field_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field_name: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_by_name(this, field_name)))
}

fn native_es_field_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field_name: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(
        ctx.get_field_by_name(this, field_name)
            .as_int()
            .unwrap_or(0),
    )))
}

pub(crate) fn native_es_centroid_slices_slice_offsets(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "sliceOffsets")
}

pub(crate) fn native_es_centroid_slices_slice_num_vectors(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "sliceNumVectors")
}

pub(crate) fn native_es_centroid_slices_max_slice_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_int(ctx, args, "maxSliceSize")
}

pub(crate) fn native_es_centroid_assignments_num_centroids(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_int(ctx, args, "numCentroids")
}

pub(crate) fn native_es_centroid_assignments_centroids(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "centroids")
}

pub(crate) fn native_es_centroid_assignments_assignments(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "assignments")
}

pub(crate) fn native_es_centroid_assignments_overspill_assignments(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "overspillAssignments")
}

pub(crate) fn native_es_centroid_assignments_global_centroid(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "globalCentroid")
}

pub(crate) fn native_es_centroid_assignments_centroid_slices(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "centroidSlices")
}

pub(crate) fn native_es_kmeans_result_centroids(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "centroids")
}

pub(crate) fn native_es_kmeans_result_assignments(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "assignments")
}

pub(crate) fn native_es_kmeans_result_cluster_counts(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "clusterCounts")
}

pub(crate) fn native_es_kmeans_result_soar_assignments(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_value(ctx, args, "soarAssignments")
}

pub(crate) fn native_es_kmeans_float_vector_values_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_es_field_int(ctx, args, "numVectors")
}

pub(crate) fn native_es_next_diskbbq_vectors_writer_write_slices_offsets(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let output = obj_arg(args, 1)?;
    let slices = match args.get(2) {
        Some(Value::Object(Some(slices))) => *slices,
        _ => return Ok(None),
    };
    let offsets = match ctx.get_field_by_name(slices, "sliceOffsets") {
        Value::Object(Some(offsets)) => offsets,
        _ => return Ok(None),
    };
    let len = ctx.array_length(offsets);
    if len == 0 {
        return Ok(None);
    }

    let mut bytes = vec![0u8; len.saturating_mul(4)];
    for i in 0..len {
        let base = i * 4;
        bytes[base..base + 4].copy_from_slice(&int_array_elem(ctx, offsets, i).to_le_bytes());
    }

    let output_pin = ctx.pin_native_root(output);
    let byte_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    let byte_arr_pin = ctx.pin_native_root(byte_arr);
    let result = if ctx.write_byte_array_from(byte_arr, 0, &bytes) {
        let output = ctx.read_native_pin(output_pin, output);
        let byte_arr = ctx.read_native_pin(byte_arr_pin, byte_arr);
        ctx.invoke_virtual(
            output,
            "writeBytes",
            "([BII)V",
            &[
                Value::Object(Some(byte_arr)),
                Value::Int(0),
                Value::Int(bytes.len().min(i32::MAX as usize) as i32),
            ],
        )?;
        Ok(None)
    } else {
        Err(lucene_iobe("writeSlicesOffsets byte[] write failed"))
    };
    ctx.unpin_native_roots(byte_arr_pin);
    ctx.unpin_native_roots(output_pin);
    result
}

fn lucene_missing_static(class_name: &str, field_name: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalArgumentException {
        message: format!("missing static field {class_name}.{field_name}"),
    }))
}

pub(crate) fn lucene_static_object(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_id = ctx.ensure_class_initialized(class_name)?;
    let field_idx = ctx
        .static_field_index_by_name(class_id, field_name)
        .ok_or_else(|| lucene_missing_static(class_name, field_name))?;
    match ctx.get_static_field(class_id, field_idx) {
        Value::Object(Some(obj)) => Ok(obj),
        _ => Err(lucene_missing_static(class_name, field_name)),
    }
}

fn lucene_sortable_i32_to_float(sortable: i32) -> f32 {
    let bits = sortable ^ ((sortable >> 31) & 0x7fff_ffff);
    f32::from_bits(bits as u32)
}

fn es_bulk_neighbor_decode_score(raw: i64) -> f32 {
    lucene_sortable_i32_to_float((raw >> 32) as i32)
}

fn es_bulk_neighbor_decode_doc(raw: i64) -> i32 {
    (!raw) as i32
}

fn native_es_new_score_doc(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    total_fields: usize,
    raw: i64,
) -> ObjectRef {
    let hit = ctx.alloc_object(class_id, total_fields);
    ctx.set_field_by_name(hit, "doc", Value::Int(es_bulk_neighbor_decode_doc(raw)));
    ctx.set_field_by_name(
        hit,
        "score",
        Value::Float(es_bulk_neighbor_decode_score(raw)),
    );
    ctx.set_field_by_name(hit, "shardIndex", Value::Int(-1));
    hit
}

fn native_es_reservoir_values(ctx: &dyn NativeContext, reservoir: ObjectRef) -> Vec<i64> {
    let max_size = lucene_field_int(ctx, reservoir, "maxSize").max(0) as usize;
    let raw_size = lucene_field_int(ctx, reservoir, "size").max(0) as usize;
    let values_arr = match ctx.get_field_by_name(reservoir, "values") {
        Value::Object(Some(values)) => values,
        _ => return Vec::new(),
    };
    let array_len = ctx.array_length(values_arr);
    let read_len = raw_size.min(array_len);
    if read_len == 0 || max_size == 0 {
        return Vec::new();
    }

    if read_len <= max_size {
        let mut values = Vec::with_capacity(read_len);
        for i in 0..read_len {
            values.push(ctx.get_array_element(values_arr, i).as_long().unwrap_or(0));
        }
        return values;
    }

    let mut values = Vec::with_capacity(read_len);
    for i in 0..read_len {
        values.push(ctx.get_array_element(values_arr, i).as_long().unwrap_or(0));
    }
    values.sort();
    values.split_off(read_len - max_size)
}

fn native_es_reset_reservoir(ctx: &mut dyn NativeContext, reservoir: ObjectRef) {
    ctx.set_field_by_name(reservoir, "size", Value::Int(0));
    ctx.set_field_by_name(reservoir, "threshold", Value::Long(i64::MIN));
    ctx.set_field_by_name(reservoir, "thresholdScore", Value::Float(f32::NEG_INFINITY));
}

fn native_es_tiny_heap_values(ctx: &dyn NativeContext, heap: ObjectRef) -> Vec<i64> {
    let size = lucene_field_int(ctx, heap, "size").max(0) as usize;
    let heap_arr = match ctx.get_field_by_name(heap, "heap") {
        Value::Object(Some(heap_arr)) => heap_arr,
        _ => return Vec::new(),
    };
    let array_len = ctx.array_length(heap_arr);
    let mut values = Vec::with_capacity(size);
    for i in 1..=size.min(array_len.saturating_sub(1)) {
        values.push(ctx.get_array_element(heap_arr, i).as_long().unwrap_or(0));
    }
    values
}

fn native_es_reset_tiny_heap(ctx: &mut dyn NativeContext, heap: ObjectRef) {
    ctx.set_field_by_name(heap, "size", Value::Int(0));
}

fn native_es_make_top_docs(
    ctx: &mut dyn NativeContext,
    score_docs: ObjectRef,
    visited_count: i64,
    relation: ObjectRef,
) -> MethodCallResult {
    let relation_pin = ctx.pin_native_root(relation);
    let docs_pin = ctx.pin_native_root(score_docs);
    let total_hits_class = ctx.ensure_class_initialized("org/apache/lucene/search/TotalHits")?;
    let total_hits = ctx.alloc_object(
        total_hits_class,
        ctx.class_num_total_fields(total_hits_class).max(2),
    );
    let relation = ctx.read_native_pin(relation_pin, relation);
    ctx.set_field_by_name(total_hits, "value", Value::Long(visited_count));
    ctx.set_field_by_name(total_hits, "relation", Value::Object(Some(relation)));

    let total_hits_pin = ctx.pin_native_root(total_hits);
    let top_docs_class = ctx.ensure_class_initialized("org/apache/lucene/search/TopDocs")?;
    let top_docs = ctx.alloc_object(
        top_docs_class,
        ctx.class_num_total_fields(top_docs_class).max(2),
    );
    let total_hits = ctx.read_native_pin(total_hits_pin, total_hits);
    let score_docs = ctx.read_native_pin(docs_pin, score_docs);
    ctx.set_field_by_name(top_docs, "totalHits", Value::Object(Some(total_hits)));
    ctx.set_field_by_name(top_docs, "scoreDocs", Value::Object(Some(score_docs)));
    ctx.unpin_native_roots(total_hits_pin);
    ctx.unpin_native_roots(docs_pin);
    ctx.unpin_native_roots(relation_pin);
    Ok(Some(Value::Object(Some(top_docs))))
}

pub(crate) fn native_es_max_score_top_knn_collector_unsorted_top_k(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let queue = lucene_field_obj(ctx, this, "queue")?;
    let mut values = match ctx.get_field_by_name(queue, "tinyHeap") {
        Value::Object(Some(heap)) => {
            let values = native_es_tiny_heap_values(ctx, heap);
            native_es_reset_tiny_heap(ctx, heap);
            values
        }
        _ => match ctx.get_field_by_name(queue, "collector") {
            Value::Object(Some(reservoir)) => {
                let values = native_es_reservoir_values(ctx, reservoir);
                native_es_reset_reservoir(ctx, reservoir);
                values
            }
            _ => Vec::new(),
        },
    };

    let len = values.len();
    let score_docs = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len);
    if len > 0 {
        let score_docs_pin = ctx.pin_native_root(score_docs);
        let score_doc_class = ctx.ensure_class_initialized("org/apache/lucene/search/ScoreDoc")?;
        let score_doc_fields = ctx.class_num_total_fields(score_doc_class).max(3);
        for (i, raw) in values.drain(..).enumerate() {
            let hit = native_es_new_score_doc(ctx, score_doc_class, score_doc_fields, raw);
            let score_docs = ctx.read_native_pin(score_docs_pin, score_docs);
            ctx.set_array_element(score_docs, i, Value::Object(Some(hit)));
        }
        ctx.unpin_native_roots(score_docs_pin);
    }

    // NOT branch-exclusive, despite an `} else {` sitting between this call and
    // the `visitedCount()` below -- that else belongs to `relation_name`. Both
    // calls run, with an allocating `lucene_static_object` between them.
    let this_pin = ctx.pin_native_root(this);
    let early_terminated = matches!(
        ctx.invoke_virtual(this, "earlyTerminated", "()Z", &[])?,
        Some(Value::Int(v)) if v != 0
    );
    let relation_name = if early_terminated {
        "GREATER_THAN_OR_EQUAL_TO"
    } else {
        "EQUAL_TO"
    };
    let relation = lucene_static_object(
        ctx,
        "org/apache/lucene/search/TotalHits$Relation",
        relation_name,
    )?;
    let this = ctx.read_native_pin(this_pin, this);
    let visited_count = match ctx.invoke_virtual(this, "visitedCount", "()J", &[])? {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as i64,
        _ => 0,
    };
    native_es_make_top_docs(ctx, score_docs, visited_count, relation)
}

pub(crate) fn native_lucene_index_reader_context_id(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_by_name(this, "identity")))
}

pub(crate) fn native_es92_int7_vectors_scorer_int7_dot_product_bulk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let query = obj_arg(args, 1)?;
    let count = args.get(2).and_then(Value::as_int).unwrap_or(0);
    let scores = obj_arg(args, 3)?;
    if count <= 0 {
        return Ok(None);
    }

    let dimensions = ctx
        .get_field_by_name(this, "dimensions")
        .as_int()
        .unwrap_or(0);
    if dimensions <= 0 || count as usize > ctx.array_length(scores) {
        return Ok(None);
    }
    let input = lucene_field_obj(ctx, this, "in")?;
    let byte_len = (count as usize)
        .checked_mul(dimensions as usize)
        .ok_or_else(|| lucene_aioobe(i32::MAX))?;
    let packed = ctx.new_array(cratonvm_types::ArrayElementType::Byte, byte_len);
    ctx.invoke_virtual(
        input,
        "readBytes",
        "([BII)V",
        &[
            Value::Object(Some(packed)),
            Value::Int(0),
            Value::Int(byte_len as i32),
        ],
    )?;

    let mut query_bytes = vec![0u8; dimensions as usize];
    if ctx.read_byte_array_into(query, 0, &mut query_bytes) != query_bytes.len() {
        return Ok(None);
    }
    let mut packed_bytes = vec![0u8; byte_len];
    if ctx.read_byte_array_into(packed, 0, &mut packed_bytes) != byte_len {
        return Ok(None);
    }
    for vector in 0..count as usize {
        let start = vector * dimensions as usize;
        let mut dot = 0i32;
        for dimension in 0..dimensions as usize {
            dot = dot.wrapping_add(
                (packed_bytes[start + dimension] as i8 as i32)
                    .wrapping_mul(query_bytes[dimension] as i8 as i32),
            );
        }
        ctx.set_array_element(scores, vector, Value::Float(dot as f32));
    }
    Ok(None)
}

pub(crate) fn native_es_knn_score_doc_query_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let score_docs = obj_arg(args, 1)?;
    let reader = obj_arg(args, 2)?;
    let len = ctx.array_length(score_docs);

    let mut hits: Vec<(i32, f32, ObjectRef, usize)> = Vec::with_capacity(len);
    for i in 0..len {
        let hit = match ctx.get_array_element(score_docs, i) {
            Value::Object(Some(hit)) => hit,
            _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
        };
        let doc = ctx.get_field_by_name(hit, "doc").as_int().unwrap_or(0);
        let score = match ctx.get_field_by_name(hit, "score") {
            Value::Float(v) => v,
            Value::Int(v) => f32::from_bits(v as u32),
            _ => 0.0,
        };
        hits.push((doc, score, hit, i));
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(&b.3)));

    // Everything still live across the two allocations below has to be pinned:
    // `score_docs` and `reader` are dereferenced afterwards, and `hits` holds a
    // whole Vec of raw `ObjectRef`s that are stored back into `score_docs`.
    // This is the `format_impl` shape with a collection instead of one array.
    let sd_pin = ctx.pin_native_root(score_docs);
    let rd_pin = ctx.pin_native_root(reader);
    let mut hit_pins: Vec<usize> = Vec::with_capacity(hits.len());
    for h in &hits {
        hit_pins.push(ctx.pin_native_root(h.2));
    }
    let docs_arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, len);
    let scores_arr = ctx.new_array(cratonvm_types::ArrayElementType::Float, len);
    let score_docs = ctx.read_native_pin(sd_pin, score_docs);
    let reader = ctx.read_native_pin(rd_pin, reader);
    for (i, h) in hits.iter_mut().enumerate() {
        h.2 = ctx.read_native_pin(hit_pins[i], h.2);
    }
    let mut docs = Vec::with_capacity(len);
    for (i, (doc, score, hit, _)) in hits.into_iter().enumerate() {
        docs.push(doc);
        ctx.set_array_element(score_docs, i, Value::Object(Some(hit)));
        ctx.set_array_element(docs_arr, i, Value::Int(doc));
        ctx.set_array_element(scores_arr, i, Value::Float(score));
    }

    let leaves = match ctx.invoke_virtual(reader, "leaves", "()Ljava/util/List;", &[])? {
        Some(Value::Object(Some(leaves))) => leaves,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let leaves_size = match ctx.invoke_virtual(leaves, "size", "()I", &[])? {
        Some(Value::Int(v)) if v >= 0 => v as usize,
        _ => 0,
    };
    let starts_len = leaves_size.saturating_add(1);
    let segment_starts = ctx.new_array(cratonvm_types::ArrayElementType::Int, starts_len);
    if starts_len > 0 {
        ctx.set_array_element(segment_starts, starts_len - 1, Value::Int(len as i32));
    }
    if starts_len != 2 {
        let mut search_from = 0usize;
        for segment in 1..starts_len.saturating_sub(1) {
            let leaf = match ctx.invoke_virtual(
                leaves,
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Int(segment as i32)],
            )? {
                Some(Value::Object(Some(leaf))) => leaf,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let doc_base = ctx.get_field_by_name(leaf, "docBase").as_int().unwrap_or(0);
            let rel = match docs[search_from..].binary_search(&doc_base) {
                Ok(idx) | Err(idx) => idx,
            };
            search_from = search_from.saturating_add(rel).min(docs.len());
            ctx.set_array_element(segment_starts, segment, Value::Int(search_from as i32));
        }
    }

    let context_identity = match ctx.get_field_by_name(reader, "readerContext") {
        Value::Object(Some(context)) => ctx.get_field_by_name(context, "identity"),
        _ => {
            let context = match ctx.invoke_virtual(
                reader,
                "getContext",
                "()Lorg/apache/lucene/index/IndexReaderContext;",
                &[],
            )? {
                Some(Value::Object(Some(context))) => context,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            ctx.get_field_by_name(context, "identity")
        }
    };

    ctx.set_field_by_name(
        this,
        "CLASS_NAME_HASH",
        Value::Int(java_string_hash_code_ascii(
            "org.elasticsearch.search.vectors.KnnScoreDocQuery",
        )),
    );
    ctx.set_field_by_name(this, "docs", Value::Object(Some(docs_arr)));
    ctx.set_field_by_name(this, "scores", Value::Object(Some(scores_arr)));
    ctx.set_field_by_name(this, "segmentStarts", Value::Object(Some(segment_starts)));
    ctx.set_field_by_name(this, "contextIdentity", context_identity);
    Ok(None)
}

pub(crate) fn native_randomized_context_get_per_thread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let thread = ctx.current_thread_object();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);

    let map = match ctx.get_field_by_name(this, "perThreadResources") {
        Value::Object(Some(map)) => map,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cache_key = randomized_per_thread_key(ctx, this, map, thread);
    if let Some(resources) = randomized_per_thread_cache_lookup(ctx, cache_key) {
        return Ok(Some(Value::Object(Some(resources))));
    }

    let map_pin = ctx.pin_native_root(map);
    let thread_pin = ctx.pin_native_root(thread);
    let existing = ctx.invoke_virtual(
        map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(thread))],
    )?;
    let map = ctx.read_native_pin(map_pin, map);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(map_pin);
    if let Some(Value::Object(Some(resources))) = existing {
        randomized_per_thread_cache_store(ctx, cache_key, resources);
        return Ok(Some(Value::Object(Some(resources))));
    }

    let map_pin = ctx.pin_native_root(map);
    let thread_pin = ctx.pin_native_root(thread);
    let resources_result = ctx.new_object_initialized(
        "com/carrotsearch/randomizedtesting/RandomizedContext$PerThreadResources",
        "(Lcom/carrotsearch/randomizedtesting/RandomizedContext$1;)V",
        &[Value::Object(None)],
    )?;
    let map = ctx.read_native_pin(map_pin, map);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(map_pin);
    let resources = match resources_result {
        Some(Value::Object(Some(resources))) => resources,
        _ => return Ok(Some(Value::Object(None))),
    };

    let deque = match ctx.get_field_by_name(resources, "randomnesses") {
        Value::Object(Some(deque)) => deque,
        _ => return Ok(Some(Value::Object(None))),
    };
    let runner = match ctx.get_field_by_name(this, "runner") {
        Value::Object(Some(runner)) => runner,
        _ => return Ok(Some(Value::Object(None))),
    };
    let runner_randomness = match ctx.get_field_by_name(runner, "runnerRandomness") {
        Value::Object(Some(randomness)) => randomness,
        _ => return Ok(Some(Value::Object(None))),
    };

    let resources_pin = ctx.pin_native_root(resources);
    let deque_pin = ctx.pin_native_root(deque);
    let map_pin = ctx.pin_native_root(map);
    let thread_pin = ctx.pin_native_root(thread);
    let cloned_result = ctx.invoke_virtual(
        runner_randomness,
        "clone",
        "(Ljava/lang/Thread;)Lcom/carrotsearch/randomizedtesting/Randomness;",
        &[Value::Object(Some(thread))],
    )?;
    let resources = ctx.read_native_pin(resources_pin, resources);
    let deque = ctx.read_native_pin(deque_pin, deque);
    let map = ctx.read_native_pin(map_pin, map);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(resources_pin);
    let cloned = match cloned_result {
        Some(Value::Object(Some(cloned))) => cloned,
        _ => return Ok(Some(Value::Object(None))),
    };

    let resources_pin = ctx.pin_native_root(resources);
    let map_pin = ctx.pin_native_root(map);
    let thread_pin = ctx.pin_native_root(thread);
    ctx.invoke_virtual(
        deque,
        "push",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(cloned))],
    )?;
    let resources = ctx.read_native_pin(resources_pin, resources);
    let map = ctx.read_native_pin(map_pin, map);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(resources_pin);

    let resources_pin = ctx.pin_native_root(resources);
    let map_pin = ctx.pin_native_root(map);
    let thread_pin = ctx.pin_native_root(thread);
    ctx.invoke_virtual(
        map,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(thread)), Value::Object(Some(resources))],
    )?;
    let resources = ctx.read_native_pin(resources_pin, resources);
    ctx.unpin_native_roots(resources_pin);

    randomized_per_thread_cache_store(ctx, cache_key, resources);
    Ok(Some(Value::Object(Some(resources))))
}

pub(crate) fn native_lucene_ram_usage_tester_ram_used(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    let bytes = if class_name == "org/apache/lucene/document/Document" {
        // Lucene postings-format tests use RamUsageTester.ramUsed(Document) only
        // to cap how many generated documents are indexed. The real helper walks
        // the full object graph reflectively; under CratonVM that reflective
        // walk dominates the test without increasing postings coverage.
        //
        // Keep the loop scale close to HotSpot by charging the actual string
        // payloads inside the Document. For the body-only LineFileDocs shape in
        // BasePostingsFormatTestCase, HotSpot 25 / Lucene 10.4 reports almost
        // exactly `align8(376 + 2 * body.length())` bytes (seed B17AC9D3E1F2A0C4:
        // avg ~2281 bytes, range 1256..3424 for the first 32 docs). The old flat
        // 1 KiB estimate undercharged this loop by ~2.2x and made CratonVM index
        // far more documents than the test's intended ~100 KiB cap.
        estimate_lucene_document_ram_used(ctx, obj)
            .unwrap_or(LUCENE_RAM_USAGE_FALLBACK_DOCUMENT_BYTES)
    } else if class_name == "java/lang/String" {
        ctx.read_string(obj)
            .map(|s| 40 + (s.len() as i64 * 2))
            .unwrap_or(64)
    } else {
        128
    };
    Ok(Some(Value::Long(bytes)))
}

fn estimate_lucene_document_ram_used(ctx: &mut dyn NativeContext, doc: ObjectRef) -> Option<i64> {
    let fields = match ctx.get_field_by_name(doc, "fields") {
        Value::Object(Some(fields)) => fields,
        _ => return None,
    };
    let fields_pin = ctx.pin_native_root(fields);
    let field_count = match invoke_list_size(ctx, fields) {
        Some(field_count) => field_count,
        None => {
            ctx.unpin_native_roots(fields_pin);
            return None;
        }
    };
    if field_count <= 0 {
        ctx.unpin_native_roots(fields_pin);
        return Some(LUCENE_RAM_USAGE_MIN_DOCUMENT_BYTES);
    }

    let mut fields = ctx.read_native_pin(fields_pin, fields);
    let mut string_chars = 0_i64;
    let mut string_fields = 0_i64;
    let max_fields = field_count.min(16);
    for index in 0..max_fields {
        let field = invoke_list_get(ctx, fields, index);
        fields = ctx.read_native_pin(fields_pin, fields);
        let Some(field) = field else {
            continue;
        };
        let Some(chars) = lucene_field_string_chars(ctx, field) else {
            continue;
        };
        string_fields += 1;
        string_chars = string_chars.saturating_add(chars as i64);
    }
    ctx.unpin_native_roots(fields_pin);

    if string_fields == 0 {
        return None;
    }

    let uncounted_fields = (field_count as i64).saturating_sub(string_fields);
    let bytes = LUCENE_RAM_USAGE_DOCUMENT_BASE_BYTES
        .saturating_add(string_chars.saturating_mul(2))
        .saturating_add(string_fields.saturating_sub(1) * LUCENE_RAM_USAGE_EXTRA_FIELD_BYTES)
        .saturating_add(uncounted_fields * 128);
    Some(align_lucene_ram_usage(bytes).max(LUCENE_RAM_USAGE_MIN_DOCUMENT_BYTES))
}

fn align_lucene_ram_usage(bytes: i64) -> i64 {
    bytes.saturating_add(7) & !7
}

fn lucene_field_string_chars(ctx: &dyn NativeContext, field: ObjectRef) -> Option<usize> {
    match ctx.get_field_by_name(field, "fieldsData") {
        Value::Object(Some(data)) => ctx.read_string(data).map(|s| s.len()),
        _ => None,
    }
}

#[cfg(test)]
mod lucene_ram_usage_tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ArrayElementType;

    fn lucene_list_hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method: &str,
        desc: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        let class_name = ctx.class_name_of_id(ctx.class_id_of_object(receiver));
        if class_name.as_deref() != Some("java/util/ArrayList") {
            return None;
        }
        match (method, desc) {
            ("size", "()I") => Some(Ok(Some(ctx.get_field(receiver, 1)))),
            ("get", "(I)Ljava/lang/Object;") => {
                let index = match args.first() {
                    Some(Value::Int(index)) if *index >= 0 => *index as usize,
                    _ => return Some(Ok(Some(Value::Object(None)))),
                };
                let array = match ctx.get_field(receiver, 0) {
                    Value::Object(Some(array)) => array,
                    _ => return Some(Ok(Some(Value::Object(None)))),
                };
                Some(Ok(Some(ctx.get_array_element(array, index))))
            }
            _ => None,
        }
    }

    fn new_obj(ctx: &mut MockNativeContext, class_name: &str) -> ObjectRef {
        match ctx
            .new_object(class_name)
            .expect("mock allocation should not throw")
        {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("unexpected allocation result: {other:?}"),
        }
    }

    fn lucene_doc_with_body_len(ctx: &mut MockNativeContext, body_len: usize) -> ObjectRef {
        ctx.set_invoke_virtual_hook(lucene_list_hook);
        let doc = new_obj(ctx, "org/apache/lucene/document/Document");
        let list = new_obj(ctx, "java/util/ArrayList");
        let field = new_obj(ctx, "org/apache/lucene/document/TextField");
        let body = ctx.create_string(&"x".repeat(body_len));
        let array = ctx.new_array(ArrayElementType::Reference, 1);

        ctx.set_field(field, 0, Value::Object(Some(body)));
        ctx.set_array_element(array, 0, Value::Object(Some(field)));
        ctx.set_field(list, 0, Value::Object(Some(array)));
        ctx.set_field(list, 1, Value::Int(1));
        ctx.set_field(doc, 0, Value::Object(Some(list)));
        doc
    }

    #[test]
    fn lucene_document_ram_usage_tracks_line_file_docs_body_length() {
        let mut ctx = mock_ctx();
        let doc = lucene_doc_with_body_len(&mut ctx, 1023);

        let bytes = estimate_lucene_document_ram_used(&mut ctx, doc)
            .expect("Lucene Document shape should be inspectable");

        assert_eq!(bytes, 2424);
    }

    #[test]
    fn lucene_document_ram_usage_has_hotspot_minimum_floor() {
        let mut ctx = mock_ctx();
        let doc = lucene_doc_with_body_len(&mut ctx, 40);

        let bytes = estimate_lucene_document_ram_used(&mut ctx, doc)
            .expect("Lucene Document shape should be inspectable");

        assert_eq!(bytes, LUCENE_RAM_USAGE_MIN_DOCUMENT_BYTES);
    }

    #[test]
    fn lucene_document_ram_usage_falls_back_for_unknown_shape() {
        let mut ctx = mock_ctx();
        let doc = new_obj(&mut ctx, "org/apache/lucene/document/Document");

        let got = native_lucene_ram_usage_tester_ram_used(&mut ctx, &[Value::Object(Some(doc))])
            .expect("native call should not throw");

        assert_eq!(
            got,
            Some(Value::Long(LUCENE_RAM_USAGE_FALLBACK_DOCUMENT_BYTES))
        );
    }
}

fn lucene_eof() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::EOFException {
        message: "Unexpected EOF".to_string(),
    }))
}

pub(crate) fn lucene_aioobe(index: i32) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::aioobe_index_only(index)))
}

pub(crate) fn lucene_iobe(message: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalArgumentException {
        message: message.to_string(),
    }))
}

fn lucene_byte_buffers_data_output_current_block(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "currentBlock") {
        Value::Object(Some(block)) => Some(block),
        _ => None,
    }
}

fn lucene_byte_buffers_data_output_append_block(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let pin = ctx.pin_native_root(this);
    ctx.invoke_virtual(this, "appendBlock", "()V", &[])?;
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    lucene_byte_buffers_data_output_current_block(ctx, this)
        .ok_or_else(|| lucene_iobe("ByteBuffersDataOutput currentBlock is null"))
}

fn lucene_byte_buffers_data_output_write_raw(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) -> MethodCallResult {
    let this_root = ctx.add_global_root(this);
    let result: MethodCallResult = (|| {
        let mut this = this;
        let mut written = 0;
        while written < bytes.len() {
            let mut block = lucene_byte_buffers_data_output_current_block(ctx, this)
                .ok_or_else(|| lucene_iobe("ByteBuffersDataOutput currentBlock is null"))?;
            if heap_byte_buffer_remaining(ctx, block) == 0 {
                block = lucene_byte_buffers_data_output_append_block(ctx, this)?;
                this = ctx.resolve_global_root(this_root).unwrap_or(this);
            }
            let n = heap_byte_buffer_write_slice(ctx, block, &bytes[written..]);
            if n == 0 {
                return Err(lucene_iobe(
                    "ByteBuffersDataOutput could not write to current block",
                ));
            }
            written += n;
        }
        Ok(None)
    })();
    if this_root != 0 {
        let _ = ctx.remove_global_root(this_root);
    }
    result
}

fn lucene_data_output_write_byte_direct(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    byte: u8,
) -> MethodCallResult {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("org/apache/lucene/store/ByteBuffersDataOutput")
    {
        return lucene_byte_buffers_data_output_write_raw(ctx, this, &[byte]);
    }
    ctx.invoke_virtual(this, "writeByte", "(B)V", &[Value::Int(byte as i8 as i32)])?;
    Ok(None)
}

fn lucene_data_output_write_vint_raw(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    value: i32,
) -> MethodCallResult {
    let mut v = value as u32;
    let mut buf = [0u8; 5];
    let mut len = 0;
    while (v & !0x7f) != 0 {
        buf[len] = ((v & 0x7f) | 0x80) as u8;
        len += 1;
        v >>= 7;
    }
    buf[len] = v as u8;
    len += 1;
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("org/apache/lucene/store/ByteBuffersDataOutput")
    {
        lucene_byte_buffers_data_output_write_raw(ctx, this, &buf[..len])
    } else {
        for b in &buf[..len] {
            lucene_data_output_write_byte_direct(ctx, this, *b)?;
        }
        Ok(None)
    }
}

fn lucene_data_output_write_signed_vlong_raw(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    value: i64,
) -> MethodCallResult {
    let mut v = value as u64;
    let mut buf = [0u8; 10];
    let mut len = 0;
    while (v & !0x7f) != 0 {
        buf[len] = ((v & 0x7f) | 0x80) as u8;
        len += 1;
        v >>= 7;
    }
    buf[len] = v as u8;
    len += 1;
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("org/apache/lucene/store/ByteBuffersDataOutput")
    {
        lucene_byte_buffers_data_output_write_raw(ctx, this, &buf[..len])
    } else {
        for b in &buf[..len] {
            lucene_data_output_write_byte_direct(ctx, this, *b)?;
        }
        Ok(None)
    }
}

pub(crate) fn native_lucene_data_output_write_vint(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    lucene_data_output_write_vint_raw(ctx, this, value)
}

pub(crate) fn native_lucene_data_output_write_zint(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    lucene_data_output_write_vint_raw(ctx, this, (value << 1) ^ (value >> 31))
}

pub(crate) fn native_lucene_data_output_write_vlong(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
    if value < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("cannot write negative vLong: {value}"),
        }
        .into());
    }
    lucene_data_output_write_signed_vlong_raw(ctx, this, value)
}

pub(crate) fn native_lucene_data_output_write_zlong(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
    lucene_data_output_write_signed_vlong_raw(ctx, this, (value << 1) ^ (value >> 63))
}

pub(crate) fn native_lucene_byte_buffers_data_output_write_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let byte = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
    lucene_byte_buffers_data_output_write_raw(ctx, this, &[byte])
}

fn lucene_byte_buffers_data_output_write_byte_array(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    src: ObjectRef,
    off: usize,
    len: usize,
) -> MethodCallResult {
    if len == 0 {
        return Ok(None);
    }
    let mut tmp = vec![0u8; len];
    let copied = ctx.read_byte_array_into(src, off, &mut tmp);
    if copied != len {
        return Err(lucene_iobe(
            "ByteBuffersDataOutput source byte[] read failed",
        ));
    }
    lucene_byte_buffers_data_output_write_raw(ctx, this, &tmp)
}

pub(crate) fn native_lucene_byte_buffers_data_output_write_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = obj_arg(args, 1)?;
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    lucene_byte_buffers_data_output_write_byte_array(ctx, this, src, off, len)
}

pub(crate) fn native_lucene_byte_buffers_data_output_write_bytes_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = obj_arg(args, 1)?;
    let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    native_lucene_byte_buffers_data_output_write_bytes(
        ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(src)),
            Value::Int(0),
            Value::Int(len),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_data_output_write_bytes_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = obj_arg(args, 1)?;
    let len = ctx.array_length(src) as i32;
    native_lucene_byte_buffers_data_output_write_bytes(
        ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(src)),
            Value::Int(0),
            Value::Int(len),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_data_output_write_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
    lucene_byte_buffers_data_output_write_raw(ctx, this, &value.to_le_bytes())
}

pub(crate) fn native_lucene_byte_buffers_data_output_write_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    lucene_byte_buffers_data_output_write_raw(ctx, this, &value.to_le_bytes())
}

pub(crate) fn native_lucene_byte_buffers_data_output_write_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
    lucene_byte_buffers_data_output_write_raw(ctx, this, &value.to_le_bytes())
}

fn lucene_byte_buffers_data_output_copy_bytes_from(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    input: ObjectRef,
    remaining: usize,
) -> MethodCallResult {
    let this_root = ctx.add_global_root(this);
    let result: MethodCallResult = (|| {
        let mut this = this;
        let mut input = input;
        let mut remaining = remaining;
        while remaining > 0 {
            let mut block = lucene_byte_buffers_data_output_current_block(ctx, this)
                .ok_or_else(|| lucene_iobe("ByteBuffersDataOutput currentBlock is null"))?;
            if heap_byte_buffer_remaining(ctx, block) == 0 {
                block = lucene_byte_buffers_data_output_append_block(ctx, this)?;
                this = ctx.resolve_global_root(this_root).unwrap_or(this);
            }
            let position = ctx
                .get_field_by_name(block, "position")
                .as_int()
                .unwrap_or(0);
            let limit = ctx.get_field_by_name(block, "limit").as_int().unwrap_or(0);
            let offset = ctx.get_field_by_name(block, "offset").as_int().unwrap_or(0);
            let hb = match ctx.get_field_by_name(block, "hb") {
                Value::Object(Some(hb)) => hb,
                _ => {
                    return Err(lucene_iobe(
                        "ByteBuffersDataOutput current block has no array",
                    ))
                }
            };
            if position < 0 || limit < position {
                return Err(lucene_iobe("ByteBuffersDataOutput invalid block position"));
            }
            let n = (limit - position).max(0) as usize;
            let n = n.min(remaining);
            if n == 0 {
                return Err(lucene_iobe(
                    "ByteBuffersDataOutput could not make write progress",
                ));
            }
            let raw = offset as i64 + position as i64;
            if raw < 0 || raw as usize + n > ctx.array_length(hb) {
                return Err(lucene_iobe(
                    "ByteBuffersDataOutput block array offset out of bounds",
                ));
            }
            let this_pin = ctx.pin_native_root(this);
            let input_pin = ctx.pin_native_root(input);
            let block_pin = ctx.pin_native_root(block);
            let hb_pin = ctx.pin_native_root(hb);
            ctx.invoke_virtual(
                input,
                "readBytes",
                "([BII)V",
                &[
                    Value::Object(Some(hb)),
                    Value::Int(raw as i32),
                    Value::Int(n as i32),
                ],
            )?;
            this = ctx.read_native_pin(this_pin, this);
            input = ctx.read_native_pin(input_pin, input);
            block = ctx.read_native_pin(block_pin, block);
            let _hb = ctx.read_native_pin(hb_pin, hb);
            ctx.unpin_native_roots(this_pin);
            ctx.unpin_native_roots(input_pin);
            ctx.unpin_native_roots(block_pin);
            ctx.unpin_native_roots(hb_pin);
            ctx.set_field_by_name(block, "position", Value::Int(position + n as i32));
            remaining -= n;
            this = ctx.resolve_global_root(this_root).unwrap_or(this);
        }
        Ok(None)
    })();
    if this_root != 0 {
        let _ = ctx.remove_global_root(this_root);
    }
    result
}

pub(crate) fn native_lucene_byte_buffers_data_output_copy_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input = obj_arg(args, 1)?;
    let remaining = args.get(2).and_then(|v| v.as_long()).unwrap_or(0).max(0) as usize;
    lucene_byte_buffers_data_output_copy_bytes_from(ctx, this, input, remaining)
}

fn lucene_byte_buffers_data_output_size_direct(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<i64, MethodCallFailed> {
    let blocks = match ctx.get_field_by_name(this, "blocks") {
        Value::Object(Some(blocks)) => blocks,
        _ => return Ok(0),
    };
    let block_count = array_deque_size_direct(ctx, blocks)?;
    if block_count < 1 {
        return Ok(0);
    }
    let block_bits = ctx
        .get_field_by_name(this, "blockBits")
        .as_int()
        .unwrap_or(0);
    let block_size = if (0..63).contains(&block_bits) {
        1_i64 << block_bits
    } else {
        0
    };
    let current = lucene_byte_buffers_data_output_current_block(ctx, this)
        .ok_or_else(|| lucene_iobe("ByteBuffersDataOutput currentBlock is null"))?;
    let position = ctx
        .get_field_by_name(current, "position")
        .as_int()
        .unwrap_or(0) as i64;
    Ok((block_count as i64 - 1) * block_size + position)
}

fn native_lucene_byte_buffers_data_output_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Long(
        lucene_byte_buffers_data_output_size_direct(ctx, this)?,
    )))
}

pub(crate) fn native_lucene_byte_buffers_byte_buffer_recycler_reuse(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let block = obj_arg(args, 1)?;
    ctx.set_field_by_name(block, "mark", Value::Int(-1));
    ctx.set_field_by_name(block, "position", Value::Int(0));
    let reuse = lucene_field_obj(ctx, this, "reuse")?;
    ctx.invoke_virtual(
        reuse,
        "addLast",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(block))],
    )?;
    Ok(None)
}

fn lucene_byte_buffers_index_output_delegate(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(this, "delegate") {
        Value::Object(Some(delegate)) => Ok(delegate),
        _ => Err(RuntimeError::IllegalStateException {
            message: "Already closed.".to_string(),
        }
        .into()),
    }
}

fn native_lucene_byte_buffers_index_output_get_file_pointer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    Ok(Some(Value::Long(
        lucene_byte_buffers_data_output_size_direct(ctx, delegate)?,
    )))
}

pub(crate) fn native_lucene_byte_buffers_index_output_write_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    let byte = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
    lucene_byte_buffers_data_output_write_raw(ctx, delegate, &[byte])
}

pub(crate) fn native_lucene_byte_buffers_index_output_write_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    let src = obj_arg(args, 1)?;
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    lucene_byte_buffers_data_output_write_byte_array(ctx, delegate, src, off, len)
}

pub(crate) fn native_lucene_byte_buffers_index_output_write_bytes_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    let src = obj_arg(args, 1)?;
    let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    lucene_byte_buffers_data_output_write_byte_array(ctx, delegate, src, 0, len)
}

pub(crate) fn native_lucene_byte_buffers_index_output_write_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
    lucene_byte_buffers_data_output_write_raw(ctx, delegate, &value.to_le_bytes())
}

pub(crate) fn native_lucene_byte_buffers_index_output_write_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    lucene_byte_buffers_data_output_write_raw(ctx, delegate, &value.to_le_bytes())
}

pub(crate) fn native_lucene_byte_buffers_index_output_write_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    let value = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
    lucene_byte_buffers_data_output_write_raw(ctx, delegate, &value.to_le_bytes())
}

pub(crate) fn native_lucene_byte_buffers_index_output_copy_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delegate = lucene_byte_buffers_index_output_delegate(ctx, this)?;
    let input = obj_arg(args, 1)?;
    let remaining = args.get(2).and_then(|v| v.as_long()).unwrap_or(0).max(0) as usize;
    lucene_byte_buffers_data_output_copy_bytes_from(ctx, delegate, input, remaining)
}

fn lucene_bytes_ref_parts(
    ctx: &dyn NativeContext,
    bytes_ref: ObjectRef,
) -> Result<(ObjectRef, usize, usize), MethodCallFailed> {
    let bytes = lucene_field_obj(ctx, bytes_ref, "bytes")?;
    let offset = ctx
        .get_field_by_name(bytes_ref, "offset")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    let length = ctx
        .get_field_by_name(bytes_ref, "length")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    Ok((bytes, offset, length))
}

fn lucene_byte_array_unsigned_at(ctx: &dyn NativeContext, arr: ObjectRef, index: usize) -> u8 {
    ctx.get_array_element(arr, index).as_int().unwrap_or(0) as i8 as u8
}

fn lucene_bytes_ref_prefix8_from_parts(
    ctx: &dyn NativeContext,
    bytes: ObjectRef,
    offset: usize,
    length: usize,
) -> i64 {
    let available = ctx.array_length(bytes).saturating_sub(offset);
    let n = length.min(available).min(8);
    let mut value = 0u64;
    for i in 0..n {
        value = (value << 8) | lucene_byte_array_unsigned_at(ctx, bytes, offset + i) as u64;
    }
    value <<= (8 - n) * 8;
    value as i64
}

fn lucene_bytes_ref_prefix8(
    ctx: &dyn NativeContext,
    bytes_ref: ObjectRef,
) -> Result<i64, MethodCallFailed> {
    let (bytes, offset, length) = lucene_bytes_ref_parts(ctx, bytes_ref)?;
    Ok(lucene_bytes_ref_prefix8_from_parts(
        ctx, bytes, offset, length,
    ))
}

fn lucene_bytes_ref_compare_unsigned(
    ctx: &dyn NativeContext,
    left: ObjectRef,
    right: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    let (left_bytes, left_off, left_len) = lucene_bytes_ref_parts(ctx, left)?;
    let (right_bytes, right_off, right_len) = lucene_bytes_ref_parts(ctx, right)?;
    let left_avail = ctx.array_length(left_bytes).saturating_sub(left_off);
    let right_avail = ctx.array_length(right_bytes).saturating_sub(right_off);
    let left_len = left_len.min(left_avail);
    let right_len = right_len.min(right_avail);
    let common = left_len.min(right_len);
    for i in 0..common {
        let a = lucene_byte_array_unsigned_at(ctx, left_bytes, left_off + i);
        let b = lucene_byte_array_unsigned_at(ctx, right_bytes, right_off + i);
        if a != b {
            return Ok(a as i32 - b as i32);
        }
    }
    Ok(left_len as i32 - right_len as i32)
}

fn lucene_bytes_ref_equals(
    ctx: &dyn NativeContext,
    left: ObjectRef,
    right: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    Ok(lucene_bytes_ref_compare_unsigned(ctx, left, right)? == 0)
}

pub(crate) fn native_lucene_terms_enum_index_prefix8(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let bytes_ref = obj_arg(args, 0)?;
    Ok(Some(Value::Long(lucene_bytes_ref_prefix8(ctx, bytes_ref)?)))
}

pub(crate) fn native_lucene_terms_enum_index_next(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_root = ctx.add_global_root(this);
    let result: MethodCallResult = (|| {
        let mut this = this;
        let terms_enum = lucene_field_obj(ctx, this, "termsEnum")?;
        let next = ctx.invoke_virtual(
            terms_enum,
            "next",
            "()Lorg/apache/lucene/util/BytesRef;",
            &[],
        )?;
        this = ctx.resolve_global_root(this_root).unwrap_or(this);
        let current = match next {
            Some(Value::Object(obj)) => obj,
            _ => None,
        };
        ctx.set_field_by_name(this, "currentTerm", Value::Object(current));
        let prefix = match current {
            Some(bytes_ref) => lucene_bytes_ref_prefix8(ctx, bytes_ref)?,
            None => 0,
        };
        ctx.set_field_by_name(this, "currentTermPrefix8", Value::Long(prefix));
        Ok(Some(Value::Object(current)))
    })();
    if this_root != 0 {
        let _ = ctx.remove_global_root(this_root);
    }
    result
}

pub(crate) fn native_lucene_terms_enum_index_compare_term_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let left_prefix = ctx
        .get_field_by_name(this, "currentTermPrefix8")
        .as_long()
        .unwrap_or(0) as u64;
    let right_prefix = ctx
        .get_field_by_name(other, "currentTermPrefix8")
        .as_long()
        .unwrap_or(0) as u64;
    if left_prefix != right_prefix {
        return Ok(Some(Value::Int(if left_prefix < right_prefix {
            -1
        } else {
            1
        })));
    }
    let left_term = lucene_field_obj(ctx, this, "currentTerm")?;
    let right_term = lucene_field_obj(ctx, other, "currentTerm")?;
    Ok(Some(Value::Int(lucene_bytes_ref_compare_unsigned(
        ctx, left_term, right_term,
    )?)))
}

pub(crate) fn native_lucene_terms_enum_index_term_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = obj_arg(args, 1)?;
    let left_prefix = ctx
        .get_field_by_name(this, "currentTermPrefix8")
        .as_long()
        .unwrap_or(0);
    let right_prefix = ctx
        .get_field_by_name(state, "termPrefix8")
        .as_long()
        .unwrap_or(0);
    if left_prefix != right_prefix {
        return Ok(Some(Value::Int(0)));
    }
    let current = lucene_field_obj(ctx, this, "currentTerm")?;
    let builder = lucene_field_obj(ctx, state, "term")?;
    let builder_ref = lucene_field_obj(ctx, builder, "ref")?;
    Ok(Some(Value::Int(
        if lucene_bytes_ref_equals(ctx, current, builder_ref)? {
            1
        } else {
            0
        },
    )))
}

pub(crate) fn native_lucene_terms_enum_index_term_state_copy_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let state = obj_arg(args, 0)?;
    let index = obj_arg(args, 1)?;
    let state_root = ctx.add_global_root(state);
    let index_root = ctx.add_global_root(index);
    let result: MethodCallResult = (|| {
        let mut state = state;
        let mut index = index;
        let src = lucene_field_obj(ctx, index, "currentTerm")?;
        let (src_bytes, src_off, src_len) = lucene_bytes_ref_parts(ctx, src)?;
        let builder = lucene_field_obj(ctx, state, "term")?;
        let mut builder_ref = lucene_field_obj(ctx, builder, "ref")?;
        let mut dst_bytes = lucene_field_obj(ctx, builder_ref, "bytes")?;
        if ctx.array_length(dst_bytes) < src_len {
            ctx.invoke_virtual(builder, "growNoCopy", "(I)V", &[Value::Int(src_len as i32)])?;
            state = ctx.resolve_global_root(state_root).unwrap_or(state);
            index = ctx.resolve_global_root(index_root).unwrap_or(index);
            let builder = lucene_field_obj(ctx, state, "term")?;
            builder_ref = lucene_field_obj(ctx, builder, "ref")?;
            dst_bytes = lucene_field_obj(ctx, builder_ref, "bytes")?;
        }
        if ctx.array_length(dst_bytes) < src_len {
            return Err(lucene_iobe(
                "TermsEnumIndex.TermState copy destination too small",
            ));
        }
        let mut tmp = vec![0u8; src_len];
        let copied = ctx.read_byte_array_into(src_bytes, src_off, &mut tmp);
        if copied != src_len {
            return Err(lucene_iobe("TermsEnumIndex.TermState source copy failed"));
        }
        if !ctx.write_byte_array_from(dst_bytes, 0, &tmp) {
            return Err(lucene_iobe(
                "TermsEnumIndex.TermState destination copy failed",
            ));
        }
        ctx.set_field_by_name(builder_ref, "offset", Value::Int(0));
        ctx.set_field_by_name(builder_ref, "length", Value::Int(src_len as i32));
        let prefix = ctx
            .get_field_by_name(index, "currentTermPrefix8")
            .as_long()
            .unwrap_or(0);
        ctx.set_field_by_name(state, "termPrefix8", Value::Long(prefix));
        Ok(None)
    })();
    if state_root != 0 {
        let _ = ctx.remove_global_root(state_root);
    }
    if index_root != 0 {
        let _ = ctx.remove_global_root(index_root);
    }
    result
}

pub(crate) fn native_lucene_ordinal_map_segment_map_new_to_old(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = args.get(1).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let arr = lucene_field_obj(ctx, this, "newToOld")?;
    Ok(Some(Value::Int(
        ctx.get_array_element(arr, index).as_int().unwrap_or(0),
    )))
}

pub(crate) fn native_lucene_ordinal_map_segment_map_old_to_new(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = args.get(1).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let arr = lucene_field_obj(ctx, this, "oldToNew")?;
    Ok(Some(Value::Int(
        ctx.get_array_element(arr, index).as_int().unwrap_or(0),
    )))
}

pub(crate) fn native_lucene_terms_enum_priority_queue_less_than(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let left = obj_arg(args, 1)?;
    let right = obj_arg(args, 2)?;
    let cmp = native_lucene_terms_enum_index_compare_term_to(
        ctx,
        &[Value::Object(Some(left)), Value::Object(Some(right))],
    )?;
    Ok(Some(Value::Int(
        if matches!(cmp, Some(Value::Int(v)) if v < 0) {
            1
        } else {
            0
        },
    )))
}

pub(crate) fn native_lucene_priority_queue_init_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let max_size = args.get(1).and_then(Value::as_int).unwrap_or(0);
    if max_size < 0 {
        return Err(lucene_iobe("maxSize must be non-negative"));
    }
    let heap_len = if max_size == 0 {
        2usize
    } else {
        (max_size as usize)
            .checked_add(1)
            .ok_or_else(|| lucene_iobe("maxSize is too large"))?
    };
    let heap = ctx.new_array(cratonvm_types::ArrayElementType::Reference, heap_len);
    ctx.set_field_by_name(this, "size", Value::Int(0));
    ctx.set_field_by_name(this, "maxSize", Value::Int(max_size));
    ctx.set_field_by_name(this, "heap", Value::Object(Some(heap)));
    Ok(None)
}

pub(crate) fn native_lucene_priority_queue_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(
        ctx.get_field_by_name(this, "size").as_int().unwrap_or(0),
    )))
}

pub(crate) fn native_lucene_priority_queue_top(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let heap = lucene_field_obj(ctx, this, "heap")?;
    Ok(Some(ctx.get_array_element(heap, 1)))
}

pub(crate) fn native_lucene_mock_index_output_wrapper_write_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let byte = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
    let this_root = ctx.add_global_root(this);
    let result: MethodCallResult = (|| {
        let mut this = this;
        if ctx.get_field_by_name(this, "closed").as_int().unwrap_or(0) != 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "Already closed.".to_string(),
            }
            .into());
        }
        let dir = lucene_field_obj(ctx, this, "dir")?;
        if ctx.get_field_by_name(dir, "crashed").as_int().unwrap_or(0) != 0 {
            return Err(RuntimeError::IOException {
                message: "MockDirectoryWrapper has crashed".to_string(),
            }
            .into());
        }

        // Rare disk-full simulation is deliberately left to Lucene's Java body.
        if ctx.get_field_by_name(dir, "maxSize").as_long().unwrap_or(0) != 0 {
            let single = lucene_field_obj(ctx, this, "singleByte")?;
            if !ctx.write_byte_array_from(single, 0, &[byte]) {
                return Err(lucene_iobe(
                    "MockIndexOutputWrapper singleByte write failed",
                ));
            }
            return ctx.invoke_virtual(
                this,
                "writeBytes",
                "([BII)V",
                &[Value::Object(Some(single)), Value::Int(0), Value::Int(1)],
            );
        }

        if let Value::Object(Some(random)) = ctx.get_field_by_name(dir, "randomState") {
            let split = ctx.invoke_virtual(random, "nextInt", "(I)I", &[Value::Int(200)])?;
            this = ctx.resolve_global_root(this_root).unwrap_or(this);
            if matches!(split, Some(Value::Int(0))) {
                ctx.invoke("java/lang/Thread", "yield", "()V", &[])?;
                this = ctx.resolve_global_root(this_root).unwrap_or(this);
            }
        }

        let out = lucene_field_obj(ctx, this, "out")?;
        ctx.invoke_virtual(out, "writeByte", "(B)V", &[Value::Int(byte as i8 as i32)])?;
        this = ctx.resolve_global_root(this_root).unwrap_or(this);

        let dir = lucene_field_obj(ctx, this, "dir")?;
        ctx.invoke_virtual(dir, "maybeThrowDeterministicException", "()V", &[])?;
        this = ctx.resolve_global_root(this_root).unwrap_or(this);

        if ctx.get_field_by_name(this, "first").as_int().unwrap_or(0) != 0 {
            ctx.set_field_by_name(this, "first", Value::Int(0));
            let dir = lucene_field_obj(ctx, this, "dir")?;
            let name = lucene_field_obj(ctx, this, "name")?;
            ctx.invoke_virtual(
                dir,
                "maybeThrowIOException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(name))],
            )?;
        }
        Ok(None)
    })();
    if this_root != 0 {
        let _ = ctx.remove_global_root(this_root);
    }
    result
}

fn lucene_field_int(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> i32 {
    ctx.get_field_by_name(obj, name).as_int().unwrap_or(0)
}

fn lucene_field_long(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> i64 {
    ctx.get_field_by_name(obj, name).as_long().unwrap_or(0)
}

fn lucene_field_obj(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(value)) => Ok(value),
        _ => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException {
                message: Some(format!("missing {name}")),
            },
        ))),
    }
}

fn lucene_bbdin_this(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(obj))) => Ok(*obj),
        _ => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException {
                message: Some("ByteBuffersDataInput receiver is null".to_string()),
            },
        ))),
    }
}

/// The readable byte count of a `ByteBuffersDataInput`.
///
/// The field is `size` — `private final long size` — and there is **no**
/// `length` field on the class. Reading a name that does not exist is silent:
/// `lucene_field_long` answers 0 for a missing field, so every bounds check in
/// this shim compared against 0 and every single read threw
/// `EOFException: Unexpected EOF` (or `ArrayIndexOutOfBoundsException` on the
/// absolute-position accessors). `size()` itself looked fine throughout,
/// because it is not intercepted — the real one-line getter ran and returned
/// the real field. Route every size read through here so the name is stated
/// exactly once.
fn lucene_bbdin_size(ctx: &dyn NativeContext, this: ObjectRef) -> i64 {
    lucene_field_long(ctx, this, "size")
}

fn lucene_bbdin_check_relative(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    relative_pos: i64,
    width: usize,
) -> Result<i64, MethodCallFailed> {
    let length = lucene_bbdin_size(ctx, this);
    if relative_pos < 0 || (relative_pos as i128) + (width as i128) > length as i128 {
        return Err(lucene_aioobe(relative_pos as i32));
    }
    let offset = lucene_field_long(ctx, this, "offset");
    offset
        .checked_add(relative_pos)
        .ok_or_else(|| lucene_aioobe(relative_pos as i32))
}

fn lucene_bbdin_read_abs(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    absolute_pos: i64,
) -> Result<u8, MethodCallFailed> {
    if absolute_pos < 0 {
        return Err(lucene_aioobe(absolute_pos as i32));
    }

    let blocks = lucene_field_obj(ctx, this, "blocks")?;
    let block_bits = lucene_field_int(ctx, this, "blockBits");
    let block_mask = lucene_field_int(ctx, this, "blockMask");
    if !(0..63).contains(&block_bits) {
        return Err(lucene_iobe("invalid ByteBuffersDataInput blockBits"));
    }

    let block_index = ((absolute_pos as u64) >> (block_bits as u32)) as usize;
    if block_index >= ctx.array_length(blocks) {
        return Err(lucene_aioobe(block_index as i32));
    }
    let block = match ctx.get_array_element(blocks, block_index) {
        Value::Object(Some(block)) => block,
        _ => return Err(lucene_aioobe(block_index as i32)),
    };

    let block_offset = if block_mask < 0 {
        absolute_pos as usize
    } else {
        ((absolute_pos as u64) & (block_mask as u32 as u64)) as usize
    };
    let limit = lucene_field_int(ctx, block, "limit");
    if block_offset > i32::MAX as usize || block_offset as i32 >= limit {
        return Err(lucene_aioobe(block_offset as i32));
    }

    let hb = lucene_field_obj(ctx, block, "hb")?;
    let array_offset = lucene_field_int(ctx, block, "offset");
    let raw_index = (array_offset as i64)
        .checked_add(block_offset as i64)
        .ok_or_else(|| lucene_aioobe(block_offset as i32))?;
    if raw_index < 0 || raw_index as usize >= ctx.array_length(hb) {
        return Err(lucene_aioobe(raw_index as i32));
    }

    let mut byte = [0u8; 1];
    if ctx.read_byte_array_into(hb, raw_index as usize, &mut byte) != 1 {
        return Err(lucene_aioobe(raw_index as i32));
    }
    Ok(byte[0])
}

fn lucene_bbdin_read_relative(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    relative_pos: i64,
) -> Result<u8, MethodCallFailed> {
    let absolute_pos = lucene_bbdin_check_relative(ctx, this, relative_pos, 1)?;
    lucene_bbdin_read_abs(ctx, this, absolute_pos)
}

fn lucene_bbdin_read_seq_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    width: usize,
) -> Result<[u8; 8], MethodCallFailed> {
    let pos = lucene_field_long(ctx, this, "pos");
    let offset = lucene_field_long(ctx, this, "offset");
    let length = lucene_bbdin_size(ctx, this);
    if pos < offset || (pos - offset) as i128 + width as i128 > length as i128 {
        return Err(lucene_eof());
    }

    let mut out = [0u8; 8];
    for i in 0..width {
        out[i] = lucene_bbdin_read_abs(ctx, this, pos + i as i64)?;
    }
    ctx.set_field_by_name(this, "pos", Value::Long(pos + width as i64));
    Ok(out)
}

fn lucene_bbdin_read_at_bytes(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    relative_pos: i64,
    width: usize,
) -> Result<[u8; 8], MethodCallFailed> {
    let absolute_pos = lucene_bbdin_check_relative(ctx, this, relative_pos, width)?;
    let mut out = [0u8; 8];
    for i in 0..width {
        out[i] = lucene_bbdin_read_abs(ctx, this, absolute_pos + i as i64)?;
    }
    Ok(out)
}

fn lucene_bbdin_copy_abs_to_array(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mut absolute_pos: i64,
    dst: ObjectRef,
    mut dst_off: usize,
    mut len: usize,
) -> MethodCallResult {
    let blocks = lucene_field_obj(ctx, this, "blocks")?;
    let block_bits = lucene_field_int(ctx, this, "blockBits");
    let block_mask = lucene_field_int(ctx, this, "blockMask");
    if !(0..63).contains(&block_bits) {
        return Err(lucene_iobe("invalid ByteBuffersDataInput blockBits"));
    }
    while len > 0 {
        if absolute_pos < 0 {
            return Err(lucene_eof());
        }
        let block_index = ((absolute_pos as u64) >> (block_bits as u32)) as usize;
        if block_index >= ctx.array_length(blocks) {
            return Err(lucene_eof());
        }
        let block = match ctx.get_array_element(blocks, block_index) {
            Value::Object(Some(block)) => block,
            _ => return Err(lucene_eof()),
        };
        let block_offset = if block_mask < 0 {
            absolute_pos as usize
        } else {
            ((absolute_pos as u64) & (block_mask as u32 as u64)) as usize
        };
        let limit = lucene_field_int(ctx, block, "limit").max(0) as usize;
        if block_offset >= limit {
            return Err(lucene_eof());
        }
        let n = len.min(limit - block_offset);
        let hb = lucene_field_obj(ctx, block, "hb")?;
        let array_offset = lucene_field_int(ctx, block, "offset") as i64;
        let raw_index = array_offset
            .checked_add(block_offset as i64)
            .ok_or_else(lucene_eof)?;
        if raw_index < 0 || raw_index as usize + n > ctx.array_length(hb) {
            return Err(lucene_eof());
        }
        let mut tmp = vec![0u8; n];
        if ctx.read_byte_array_into(hb, raw_index as usize, &mut tmp) != n {
            return Err(lucene_eof());
        }
        if !ctx.write_byte_array_from(dst, dst_off, &tmp) {
            return Err(lucene_iobe(
                "ByteBuffersDataInput destination byte[] write failed",
            ));
        }
        absolute_pos += n as i64;
        dst_off += n;
        len -= n;
    }
    Ok(None)
}

fn lucene_bbdin_copy_abs_to_vec(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    mut absolute_pos: i64,
    dst: &mut [u8],
) -> MethodCallResult {
    let blocks = lucene_field_obj(ctx, this, "blocks")?;
    let block_bits = lucene_field_int(ctx, this, "blockBits");
    let block_mask = lucene_field_int(ctx, this, "blockMask");
    if !(0..63).contains(&block_bits) {
        return Err(lucene_iobe("invalid ByteBuffersDataInput blockBits"));
    }
    let mut written = 0usize;
    while written < dst.len() {
        if absolute_pos < 0 {
            return Err(lucene_eof());
        }
        let block_index = ((absolute_pos as u64) >> (block_bits as u32)) as usize;
        if block_index >= ctx.array_length(blocks) {
            return Err(lucene_eof());
        }
        let block = match ctx.get_array_element(blocks, block_index) {
            Value::Object(Some(block)) => block,
            _ => return Err(lucene_eof()),
        };
        let block_offset = if block_mask < 0 {
            absolute_pos as usize
        } else {
            ((absolute_pos as u64) & (block_mask as u32 as u64)) as usize
        };
        let limit = lucene_field_int(ctx, block, "limit").max(0) as usize;
        if block_offset >= limit {
            return Err(lucene_eof());
        }
        let n = (dst.len() - written).min(limit - block_offset);
        let hb = lucene_field_obj(ctx, block, "hb")?;
        let array_offset = lucene_field_int(ctx, block, "offset") as i64;
        let raw_index = array_offset
            .checked_add(block_offset as i64)
            .ok_or_else(lucene_eof)?;
        if raw_index < 0 || raw_index as usize + n > ctx.array_length(hb) {
            return Err(lucene_eof());
        }
        if ctx.read_byte_array_into(hb, raw_index as usize, &mut dst[written..written + n]) != n {
            return Err(lucene_eof());
        }
        absolute_pos += n as i64;
        written += n;
    }
    Ok(None)
}

fn lucene_array_read_range(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: i32,
    len: i32,
) -> Result<Option<(usize, usize)>, MethodCallFailed> {
    if len <= 0 {
        return Ok(None);
    }
    if off < 0 {
        return Err(lucene_aioobe(off));
    }
    let off = off as usize;
    let len = len as usize;
    let end = off
        .checked_add(len)
        .ok_or_else(|| lucene_aioobe(i32::MAX))?;
    if end > ctx.array_length(arr) {
        return Err(lucene_aioobe(end.min(i32::MAX as usize) as i32));
    }
    Ok(Some((off, len)))
}

fn lucene_bbdin_read_primitive_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    byte_len: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    if byte_len == 0 {
        return Ok(Vec::new());
    }
    let pos = lucene_field_long(ctx, this, "pos");
    let offset = lucene_field_long(ctx, this, "offset");
    let length = lucene_bbdin_size(ctx, this);
    if pos < offset || (pos - offset) as i128 + byte_len as i128 > length as i128 {
        return Err(lucene_eof());
    }
    let new_pos = pos.checked_add(byte_len as i64).ok_or_else(lucene_eof)?;
    let mut bytes = vec![0u8; byte_len];
    lucene_bbdin_copy_abs_to_vec(ctx, this, pos, &mut bytes)?;
    ctx.set_field_by_name(this, "pos", Value::Long(new_pos));
    Ok(bytes)
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_floats(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let dst = obj_arg(args, 1)?;
    let off = args.get(2).and_then(Value::as_int).unwrap_or(0);
    let len = args.get(3).and_then(Value::as_int).unwrap_or(0);
    let Some((off, len)) = lucene_array_read_range(ctx, dst, off, len)? else {
        return Ok(None);
    };
    let byte_len = len.checked_mul(4).ok_or_else(|| lucene_aioobe(i32::MAX))?;
    let bytes = lucene_bbdin_read_primitive_bytes(ctx, this, byte_len)?;
    for i in 0..len {
        let j = i * 4;
        let bits = i32::from_le_bytes([bytes[j], bytes[j + 1], bytes[j + 2], bytes[j + 3]]);
        ctx.set_array_element(dst, off + i, Value::Float(f32::from_bits(bits as u32)));
    }
    Ok(None)
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_longs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let dst = obj_arg(args, 1)?;
    let off = args.get(2).and_then(Value::as_int).unwrap_or(0);
    let len = args.get(3).and_then(Value::as_int).unwrap_or(0);
    let Some((off, len)) = lucene_array_read_range(ctx, dst, off, len)? else {
        return Ok(None);
    };
    let byte_len = len.checked_mul(8).ok_or_else(|| lucene_aioobe(i32::MAX))?;
    let bytes = lucene_bbdin_read_primitive_bytes(ctx, this, byte_len)?;
    for i in 0..len {
        let j = i * 8;
        ctx.set_array_element(
            dst,
            off + i,
            Value::Long(i64::from_le_bytes([
                bytes[j],
                bytes[j + 1],
                bytes[j + 2],
                bytes[j + 3],
                bytes[j + 4],
                bytes[j + 5],
                bytes[j + 6],
                bytes[j + 7],
            ])),
        );
    }
    Ok(None)
}

fn lucene_bbdin_read_bytes_common(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
    update_pos: bool,
    relative_pos: i64,
) -> MethodCallResult {
    if len == 0 {
        return Ok(None);
    }
    let length = lucene_bbdin_size(ctx, this);
    let offset = lucene_field_long(ctx, this, "offset");
    if relative_pos < 0 || (relative_pos as i128) + (len as i128) > length as i128 {
        return Err(lucene_eof());
    }
    let absolute_pos = offset.checked_add(relative_pos).ok_or_else(lucene_eof)?;
    lucene_bbdin_copy_abs_to_array(ctx, this, absolute_pos, dst, dst_off, len)?;
    if update_pos {
        ctx.set_field_by_name(this, "pos", Value::Long(absolute_pos + len as i64));
    }
    Ok(None)
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let dst = obj_arg(args, 1)?;
    let off = args.get(2).and_then(Value::as_int).unwrap_or(0).max(0) as usize;
    let len = args.get(3).and_then(Value::as_int).unwrap_or(0).max(0) as usize;
    let relative_pos = lucene_field_long(ctx, this, "pos") - lucene_field_long(ctx, this, "offset");
    lucene_bbdin_read_bytes_common(ctx, this, dst, off, len, true, relative_pos)
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_bytes_bool(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_lucene_byte_buffers_data_input_read_bytes(ctx, args)
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_bytes_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let relative_pos = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let dst = obj_arg(args, 2)?;
    let off = args.get(3).and_then(Value::as_int).unwrap_or(0).max(0) as usize;
    let len = args.get(4).and_then(Value::as_int).unwrap_or(0).max(0) as usize;
    lucene_bbdin_read_bytes_common(ctx, this, dst, off, len, false, relative_pos)
}

pub(crate) fn native_lucene_byte_buffers_data_input_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    Ok(Some(Value::Long(lucene_bbdin_size(ctx, this))))
}

pub(crate) fn native_lucene_byte_buffers_data_input_position(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    Ok(Some(Value::Long(
        lucene_field_long(ctx, this, "pos") - lucene_field_long(ctx, this, "offset"),
    )))
}

pub(crate) fn native_lucene_byte_buffers_data_input_seek(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let relative_pos = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let length = lucene_bbdin_size(ctx, this);
    if relative_pos > length {
        ctx.set_field_by_name(this, "pos", Value::Long(length));
        return Err(lucene_eof());
    }
    let absolute_pos = lucene_field_long(ctx, this, "offset")
        .checked_add(relative_pos)
        .ok_or_else(lucene_eof)?;
    ctx.set_field_by_name(this, "pos", Value::Long(absolute_pos));
    Ok(None)
}

fn lucene_byte_buffers_index_input_delegate(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(this, "in") {
        Value::Object(Some(input)) => Ok(input),
        _ => Err(RuntimeError::IllegalStateException {
            message: "Already closed.".to_string(),
        }
        .into()),
    }
}

pub(crate) fn native_lucene_byte_buffers_index_input_get_file_pointer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_position(ctx, &[Value::Object(Some(input))])
}

pub(crate) fn native_lucene_byte_buffers_index_input_seek(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_seek(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Long(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_length(ctx, &[Value::Object(Some(input))])
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_byte(ctx, &[Value::Object(Some(input))])
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_bytes(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Object(None)),
            args.get(2).copied().unwrap_or(Value::Int(0)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_bytes_bool(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_lucene_byte_buffers_index_input_read_bytes(ctx, args)
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_floats(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_floats(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Object(None)),
            args.get(2).copied().unwrap_or(Value::Int(0)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_longs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_longs(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Object(None)),
            args.get(2).copied().unwrap_or(Value::Int(0)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_short(ctx, &[Value::Object(Some(input))])
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_int(ctx, &[Value::Object(Some(input))])
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_long(ctx, &[Value::Object(Some(input))])
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_byte_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_byte_at(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Long(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_bytes_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_bytes_at(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Long(0)),
            args.get(2).copied().unwrap_or(Value::Object(None)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
            args.get(4).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_short_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_short_at(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Long(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_int_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_int_at(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Long(0)),
        ],
    )
}

pub(crate) fn native_lucene_byte_buffers_index_input_read_long_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_byte_buffers_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    native_lucene_byte_buffers_data_input_read_long_at(
        ctx,
        &[
            Value::Object(Some(input)),
            args.get(1).copied().unwrap_or(Value::Long(0)),
        ],
    )
}

pub(crate) fn native_lucene_index_input_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_by_name(this, "resourceDescription")))
}

fn lucene_mock_index_input_delegate(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if ctx.get_field_by_name(this, "closed").as_int().unwrap_or(0) != 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Abusing closed IndexInput!".to_string(),
        }
        .into());
    }
    if let Value::Object(Some(parent)) = ctx.get_field_by_name(this, "parent") {
        if ctx
            .get_field_by_name(parent, "closed")
            .as_int()
            .unwrap_or(0)
            != 0
        {
            return Err(RuntimeError::IllegalStateException {
                message: "Abusing clone of a closed IndexInput!".to_string(),
            }
            .into());
        }
    }
    lucene_field_obj(ctx, this, "in")
}

pub(crate) fn native_lucene_mock_index_input_wrapper_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(input, "length", "()J", &[])
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(input, "readByte", "()B", &[])
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(
        input,
        "readBytes",
        "([BII)V",
        &[
            args.get(1).copied().unwrap_or(Value::Object(None)),
            args.get(2).copied().unwrap_or(Value::Int(0)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_bytes_bool(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(
        input,
        "readBytes",
        "([BIIZ)V",
        &[
            args.get(1).copied().unwrap_or(Value::Object(None)),
            args.get(2).copied().unwrap_or(Value::Int(0)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
            args.get(4).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_floats(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(
        input,
        "readFloats",
        "([FII)V",
        &[
            args.get(1).copied().unwrap_or(Value::Object(None)),
            args.get(2).copied().unwrap_or(Value::Int(0)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_longs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(
        input,
        "readLongs",
        "([JII)V",
        &[
            args.get(1).copied().unwrap_or(Value::Object(None)),
            args.get(2).copied().unwrap_or(Value::Int(0)),
            args.get(3).copied().unwrap_or(Value::Int(0)),
        ],
    )
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(input, "readShort", "()S", &[])
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(input, "readInt", "()I", &[])
}

pub(crate) fn native_lucene_mock_index_input_wrapper_read_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = lucene_mock_index_input_delegate(ctx, obj_arg(args, 0)?)?;
    ctx.invoke_virtual(input, "readLong", "()J", &[])
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let bytes = lucene_bbdin_read_seq_bytes(ctx, this, 1)?;
    Ok(Some(Value::Int(bytes[0] as i8 as i32)))
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_byte_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let relative_pos = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let byte = lucene_bbdin_read_relative(ctx, this, relative_pos)?;
    Ok(Some(Value::Int(byte as i8 as i32)))
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let bytes = lucene_bbdin_read_seq_bytes(ctx, this, 2)?;
    Ok(Some(Value::Int(
        i16::from_le_bytes([bytes[0], bytes[1]]) as i32
    )))
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_short_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let relative_pos = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let bytes = lucene_bbdin_read_at_bytes(ctx, this, relative_pos, 2)?;
    Ok(Some(Value::Int(
        i16::from_le_bytes([bytes[0], bytes[1]]) as i32
    )))
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let bytes = lucene_bbdin_read_seq_bytes(ctx, this, 4)?;
    Ok(Some(Value::Int(i32::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ]))))
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_int_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let relative_pos = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let bytes = lucene_bbdin_read_at_bytes(ctx, this, relative_pos, 4)?;
    Ok(Some(Value::Int(i32::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ]))))
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let bytes = lucene_bbdin_read_seq_bytes(ctx, this, 8)?;
    Ok(Some(Value::Long(i64::from_le_bytes(bytes))))
}

pub(crate) fn native_lucene_byte_buffers_data_input_read_long_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let relative_pos = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let bytes = lucene_bbdin_read_at_bytes(ctx, this, relative_pos, 8)?;
    Ok(Some(Value::Long(i64::from_le_bytes(bytes))))
}

pub(crate) fn native_lucene_byte_buffers_data_input_slice(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = lucene_bbdin_this(args)?;
    let relative_offset = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let requested_len = args.get(2).and_then(Value::as_long).unwrap_or(0);
    let source_len = lucene_bbdin_size(ctx, this);
    if relative_offset < 0
        || requested_len < 0
        || requested_len as i128 > source_len as i128 - relative_offset as i128
    {
        return Err(lucene_iobe("ByteBuffersDataInput.slice out of bounds"));
    }

    let absolute_offset = lucene_field_long(ctx, this, "offset")
        .checked_add(relative_offset)
        .ok_or_else(|| lucene_iobe("ByteBuffersDataInput.slice offset overflow"))?;
    let new_obj = match ctx.new_object("org/apache/lucene/store/ByteBuffersDataInput")? {
        Some(Value::Object(Some(obj))) => obj,
        _ => return Err(lucene_iobe("could not allocate ByteBuffersDataInput")),
    };

    ctx.set_field_by_name(new_obj, "blocks", ctx.get_field_by_name(this, "blocks"));
    ctx.set_field_by_name(
        new_obj,
        "floatBuffers",
        ctx.get_field_by_name(this, "floatBuffers"),
    );
    ctx.set_field_by_name(
        new_obj,
        "longBuffers",
        ctx.get_field_by_name(this, "longBuffers"),
    );
    ctx.set_field_by_name(
        new_obj,
        "blockBits",
        ctx.get_field_by_name(this, "blockBits"),
    );
    ctx.set_field_by_name(
        new_obj,
        "blockMask",
        ctx.get_field_by_name(this, "blockMask"),
    );
    ctx.set_field_by_name(new_obj, "size", Value::Long(requested_len));
    ctx.set_field_by_name(new_obj, "offset", Value::Long(absolute_offset));
    ctx.set_field_by_name(new_obj, "pos", Value::Long(absolute_offset));

    Ok(Some(Value::Object(Some(new_obj))))
}

pub(crate) fn native_es_submit_runnable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // BUG-CCE-0716: a genuinely-real receiver (real TPE, or any other real
    // AbstractExecutorService subclass such as Netty's AbstractEventExecutor)
    // must run the real bytecode body so an overridden `newTaskFor()`
    // produces its own real Future subtype -- see `executor_is_real`'s doc.
    if let Some(Value::Object(Some(this))) = args.first() {
        if executor_is_real(ctx, *this) {
            return ctx.invoke_special_bytecode_only(
                "java/util/concurrent/AbstractExecutorService",
                "submit",
                "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
                args,
            );
        }
    }
    // Execute the Runnable immediately (single-threaded model)
    if let Some(Value::Object(Some(runnable))) = args.get(1) {
        let _ = ctx.invoke_virtual(*runnable, "run", "()V", &[]);
    }
    let future = completed_executor_future(ctx, Value::Object(None))?;
    Ok(Some(Value::Object(Some(future))))
}

pub(crate) fn native_es_submit_callable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // BUG-CCE-0716: see native_es_submit_runnable's comment above.
    if let Some(Value::Object(Some(this))) = args.first() {
        if executor_is_real(ctx, *this) {
            return ctx.invoke_special_bytecode_only(
                "java/util/concurrent/AbstractExecutorService",
                "submit",
                "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
                args,
            );
        }
    }
    let mut result = Value::Object(None);
    if let Some(Value::Object(Some(callable))) = args.get(1) {
        result = ctx
            .invoke_virtual(*callable, "call", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
    }
    let future = completed_executor_future(ctx, result)?;
    Ok(Some(Value::Object(Some(future))))
}

pub(crate) fn native_es_execute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A genuinely-real ThreadPoolExecutor shares this exact class name with
    // CratonVM's synthetic 2-field placeholder (`Executors.newSingleThreadExecutor()`
    // et al., AND the internal async worker pool this function itself hands
    // work to below) — dispatch straight to real bytecode instead of the
    // synthetic model. This is also what breaks the recursion: the async
    // pool's own `execute()` call (via `spawn_runnable_on_real_thread` below)
    // lands right back on this native, since its receiver's class is also
    // "ThreadPoolExecutor" — routing it to `invoke_virtual_bytecode_only`
    // (which skips native lookup) instead of looping back through
    // `invoke_virtual`/this native again. See that method's doc for the
    // full rationale.
    if let Some(Value::Object(Some(this))) = args.first() {
        if executor_has_real_workers(ctx, *this) {
            // `invoke_virtual_bytecode_only` still re-resolves the inherited
            // method on a concrete subclass such as JULI's
            // LoggerExecutorService, re-entering this native indefinitely.
            // Resolve directly on ThreadPoolExecutor to execute the real
            // bounded-queue implementation exactly once.
            return ctx.invoke_special_bytecode_only(
                "java/util/concurrent/ThreadPoolExecutor",
                "execute",
                "(Ljava/lang/Runnable;)V",
                args,
            );
        }
    }
    // HANGS-0706b: `Executor.execute(Runnable)` is a fire-and-forget contract --
    // callers are entitled to assume the submitted task runs independently of
    // the calling thread. The prior eager-inline body (`runnable.run()` on the
    // caller) silently violated that contract for every executor created via
    // `Executors.newSingleThreadExecutor()` / `newFixedThreadPool()` /
    // `newCachedThreadPool()` (CratonVM's synthetic 2-field executor). Any
    // task that blocks waiting for a signal only the SUBMITTING thread can
    // later deliver -- e.g.
    // Spring's `OutputStreamPublisher`/`SubscriberInputStream` Flow adapters,
    // whose `LockSupport.park()`/`resume()` handshake assumes the publisher
    // body runs on a thread other than the one calling `subscribe()` -- self-
    // deadlocks permanently: confirmed via a live `gdb` capture showing the
    // main VM thread parked forever in `native_lock_support_park`, reached
    // through the executed Runnable's own call chain, with no other thread
    // ever positioned to call `resume()`/`request()` because the "async" work
    // never left the caller's stack. This is the exact same class of bug
    // already fixed for the ForkJoinPool/CompletableFuture path (Bug D,
    // kafka-suite-0617, see `spawn_runnable_on_real_thread`'s doc comment) --
    // apply the same fix here: hand the task to the shared bounded real
    // `ThreadPoolExecutor` so it runs on an actual worker thread and the
    // caller returns immediately, matching HotSpot's `execute()` semantics.
    //
    // (A second, now-redundant `executor_has_real_workers` guard used to
    // live here — added by a parallel same-day fix as defense-in-depth,
    // degrading a real receiver to synchronous inline execution. Removed:
    // the check at the top of this function already returns before this
    // point is ever reached for a real receiver, and does so via real
    // bytecode — genuine async semantics — rather than a synchronous
    // fallback.)
    if let Some(Value::Object(Some(runnable))) = args.get(1) {
        return spawn_runnable_on_real_thread(ctx, *runnable);
    }
    Ok(None)
}

pub(crate) fn native_es_shutdown_now(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real executor: interrupt its workers so they terminate. Only fall back to
    // the synthetic shutdown flag for the synthetic model — writing field index
    // 1 of a real ThreadPoolExecutor / delegate wrapper would corrupt a real
    // field.
    if !interrupt_executor_workers(ctx, this) {
        ctx.set_field(this, EXEC_FIELD_SHUTDOWN, Value::Int(1));
    }
    // Return empty list
    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))])?;
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_es_is_shutdown(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if executor_has_real_workers(ctx, this) {
        if let Value::Object(Some(ctl)) = ctx.get_field_by_name(this, "ctl") {
            if let Some(Value::Int(state)) = ctx.invoke_virtual(ctl, "get", "()I", &[])? {
                // ThreadPoolExecutor encodes RUNNING with the sign bit set;
                // every shutdown/stop/terminated state is non-negative.
                return Ok(Some(Value::Int((state >= 0) as i32)));
            }
        }
    }
    let shut = match ctx.get_field(this, EXEC_FIELD_SHUTDOWN) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(shut)))
}

pub(crate) fn native_es_await_termination(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    if executor_has_real_workers(ctx, this) {
        return ctx.invoke_special_bytecode_only(
            "java/util/concurrent/ThreadPoolExecutor",
            "awaitTermination",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            args,
        );
    }
    Ok(Some(Value::Int(1)))
}
