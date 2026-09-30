// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Apache Lucene / Elasticsearch intrinsics and the RandomizedRunner test-runner shims.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

/// Java `fma` for the kernels below, which mirror Lucene's Panama `fma`
/// accumulators bit for bit: `f32::mul_add` / `f64::mul_add` call the C
/// runtime's `fmaf` / `fma` on this build (no `+fma` target feature), which
/// may round twice, while the JDK code these stand in for calls the correctly
/// rounded `Math.fma` per lane (`r12w8-rt3-vector-fma-lanes-trust-the-crt-patch`).
/// `CRATONVM_MATH_FMA_EXACT=0` restores `mul_add`.
trait JavaFma {
    fn java_fma(self, b: Self, c: Self) -> Self;
}

impl JavaFma for f32 {
    #[inline]
    fn java_fma(self, b: f32, c: f32) -> f32 {
        crate::phases_late::java_fma_f32(self, b, c)
    }
}

impl JavaFma for f64 {
    #[inline]
    fn java_fma(self, b: f64, c: f64) -> f64 {
        crate::phases_late::java_fma_f64(self, b, c)
    }
}

/// One cached RandomizedRunner object: a global root in the VM named by the
/// row's key (`Randomized*CacheKey::vm`), and the object's identity hash,
/// re-checked on every resolve.
///
/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 4): replaces `lib.rs::RandomizedRootEntry` /
/// `RandomizedPerThreadCacheEntry`, whose raw `fallback: ObjectRef` was
/// answered when the root was `0` (no global-ref table) or no longer
/// resolved -- an address from before any number of moving collections,
/// whose identity hash was then read through a possibly dead address. An
/// object that cannot be rooted is not cached now, and a handle that does not
/// resolve is a miss. The three tables are dropped per VM at teardown
/// ([`forget_vm_randomized_caches`]).
///
/// gc-common w31-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 32): the keys are tuples of identity hashes, and a hash is 32 bits
/// from a counter that wraps. The per-thread rows need only ONE collision --
/// the thread's -- because the context (and its `perThreadResources` map) is
/// the same object for every thread of a suite, and a dead thread's row stays
/// until the VM goes; a new thread that drew its hash was served the dead
/// thread's `PerThreadResources` and `Random`, where RandomizedRunner's own
/// map is keyed by the `Thread` object. A new context that drew an old one's
/// hash on the same thread was served the old context's `Random` the same
/// way. So a row now also records the ADDRESSES of the objects its key names
/// ([`RandomizedOwners`]) and answers only them. A collection that moves one
/// of them makes the row a miss, which recomputes it through the Java map
/// and re-files it: correct, and at most one recomputation per move.
///
/// gc-common w32-a (`common-w31b-randomized-runner-caches-root-dead-threads-rows-until-vm-teardown`):
/// a row also records the WEAK LOCK KEYS of its owners ([`RandomizedOwnerKeys`],
/// minted by `lib.rs::gc_stable_weak_lock_key` when the row is filed), and the
/// first lock-key sweep that finds an owner dead -- a finished thread, or a
/// finished suite's context -- drops the row ([`forget_randomized_keys`]) and
/// queues its global root, which the VM's next store or invalidation releases
/// ([`randomized_release_deferred`]; a sweep has no `NativeContext`). Before,
/// a row went only with its VM, so a long test JVM kept one rooted
/// `PerThreadResources` / `Random` / context row per thread it ever ran.
/// The rows stay keyed by hashes, so a lookup (the hot path) takes no
/// registry lock; only a store, a miss, mints the keys. No rooted value
/// reaches its owners strongly (RandomizedRunner's own per-thread map is a
/// `WeakHashMap<Thread, _>`), so the owners can die while their row exists.
#[derive(Clone, Copy)]
struct RandomizedRoot {
    root: usize,
    identity: i32,
    owners: RandomizedOwners,
    keys: RandomizedOwnerKeys,
}

/// The current addresses of the objects a cache key names by hash (unused
/// slots 0). Compared on every lookup; see [`RandomizedRoot`].
type RandomizedOwners = [usize; 2];

/// The weak lock keys of the objects in [`RandomizedOwners`] (unused slot
/// `None`). The row goes with the first of them a sweep frees.
type RandomizedOwnerKeys = [Option<usize>; 2];

/// [`RandomizedOwners`] of `a` (and `b`), which must be CURRENT.
fn randomized_owners(a: ObjectRef, b: Option<ObjectRef>) -> RandomizedOwners {
    [a.as_ptr() as usize, b.map_or(0, |b| b.as_ptr() as usize)]
}

/// Root `obj` for a cache row owned by `owner` (and `second`), all CURRENT,
/// or `None` when no weak lock key or global root can be had (then nothing
/// is cached). The keys are minted first, so a failure takes no root.
fn randomized_root_entry(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    owner: ObjectRef,
    second: Option<ObjectRef>,
) -> Option<RandomizedRoot> {
    let first_key = gc_stable_weak_lock_key(ctx, owner).ok()?;
    let second_key = match second {
        Some(second) => Some(gc_stable_weak_lock_key(ctx, second).ok()?),
        None => None,
    };
    let root = ctx.add_global_root(obj);
    (root != 0).then(|| RandomizedRoot {
        root,
        identity: ctx.identity_hash_code(obj),
        owners: randomized_owners(owner, second),
        keys: [Some(first_key), second_key],
    })
}

fn randomized_resolve_root(
    ctx: &dyn NativeContext,
    entry: RandomizedRoot,
    owners: RandomizedOwners,
) -> Option<ObjectRef> {
    if entry.owners != owners {
        return None;
    }
    let obj = ctx.resolve_global_root(entry.root)?;
    if ctx.identity_hash_code(obj) == entry.identity {
        Some(obj)
    } else {
        None
    }
}

fn randomized_release_root(ctx: &mut dyn NativeContext, entry: RandomizedRoot) {
    if entry.root != 0 {
        let _ = ctx.remove_global_root(entry.root);
    }
}

/// VM teardown (`lib.rs::forget_vm_native_root_stores`): drop `vm`'s rows in
/// the three RandomizedRunner caches and its queued root releases, releasing
/// nothing (the VM's global-ref table dies with it). Idempotent.
pub fn forget_vm_randomized_caches(vm: usize) {
    randomized_context_cache()
        .lock()
        .retain(|key, _| key.vm != vm);
    randomized_random_cache()
        .lock()
        .retain(|key, _| key.vm != vm);
    randomized_per_thread_cache()
        .lock()
        .retain(|key, _| key.vm != vm);
    randomized_deferred_releases().lock().remove(&vm);
}

/// The global roots of rows [`forget_randomized_keys`] dropped, per VM,
/// until that VM's next store or invalidation releases them
/// ([`randomized_release_deferred`]). A queued handle is still allocated, so
/// no `add_global_root` reuses it meanwhile, and a lookup that copied its row
/// before the drop still resolves the row's own object. A leaf lock
/// (`Scratch`: every guard is statement-scoped, nothing nests inside it).
fn randomized_deferred_releases(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashMap<usize, Vec<usize>>>
{
    static QUEUE: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashMap<usize, Vec<usize>>>,
    > = std::sync::OnceLock::new();
    QUEUE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Release the calling VM's queued row roots (see
/// [`randomized_deferred_releases`]). Allocates nothing and runs no Java, so
/// a caller's raw references stay current across it.
fn randomized_release_deferred(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    let handles = {
        let mut queue = randomized_deferred_releases().lock();
        if queue.is_empty() {
            return;
        }
        match queue.remove(&vm) {
            Some(handles) => handles,
            None => return,
        }
    };
    for handle in handles {
        let _ = ctx.remove_global_root(handle);
    }
}

/// Drop VM `vm`'s rows in `table` that name one of the freed keys, pushing
/// their roots onto `roots`.
fn randomized_drop_rows_of_freed_keys<K: Copy + Eq + std::hash::Hash>(
    table: &parking_lot::Mutex<std::collections::HashMap<K, RandomizedRoot>>,
    key_vm: fn(&K) -> usize,
    vm: usize,
    freed: &dyn Fn(usize) -> bool,
    roots: &mut Vec<usize>,
) {
    let mut rows = table.lock();
    if rows.is_empty() {
        return;
    }
    rows.retain(|key, row| {
        if key_vm(key) != vm || !row.keys.iter().flatten().any(|&k| freed(k)) {
            return true;
        }
        if row.root != 0 {
            roots.push(row.root);
        }
        false
    });
}

/// gc-common w32-a: the weak lock keys in `keys` were freed by VM `vm`'s
/// lock-key sweep or teardown (`lib.rs::{sweep_lock_keys,
/// forget_vm_lock_keys}`): their objects are dead. Drop every row of the
/// three RandomizedRunner caches that one of them owns and queue the rows'
/// global roots for the VM's next store ([`randomized_release_deferred`]):
/// this runs without a `NativeContext`. Every table lock is a leaf, taken on
/// its own; nothing here reaches Java or the heap. At teardown
/// [`forget_vm_randomized_caches`] drops the queue with the VM.
pub(crate) fn forget_randomized_keys(vm: usize, keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    // Built on first use: a sweep that frees keys usually finds these tables
    // empty (no RandomizedRunner in the VM).
    let set: std::cell::OnceCell<std::collections::HashSet<usize>> = std::cell::OnceCell::new();
    let freed = |k: usize| set.get_or_init(|| keys.iter().copied().collect()).contains(&k);
    let mut roots: Vec<usize> = Vec::new();
    randomized_drop_rows_of_freed_keys(
        randomized_context_cache(),
        |k: &RandomizedContextCacheKey| k.vm,
        vm,
        &freed,
        &mut roots,
    );
    randomized_drop_rows_of_freed_keys(
        randomized_random_cache(),
        |k: &RandomizedRandomCacheKey| k.vm,
        vm,
        &freed,
        &mut roots,
    );
    randomized_drop_rows_of_freed_keys(
        randomized_per_thread_cache(),
        |k: &RandomizedPerThreadCacheKey| k.vm,
        vm,
        &freed,
        &mut roots,
    );
    if !roots.is_empty() {
        randomized_deferred_releases()
            .lock()
            .entry(vm)
            .or_default()
            .extend(roots);
    }
}

fn randomized_context_cache() -> &'static parking_lot::Mutex<
    std::collections::HashMap<RandomizedContextCacheKey, RandomizedRoot>,
> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<RandomizedContextCacheKey, RandomizedRoot>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn randomized_context_cache_lookup(
    ctx: &dyn NativeContext,
    key: RandomizedContextCacheKey,
    owners: RandomizedOwners,
) -> Option<ObjectRef> {
    let entry = randomized_context_cache().lock().get(&key).copied()?;
    randomized_resolve_root(ctx, entry, owners)
}

/// File `obj` in `table` under `key`, owned by `owner` (and `second`), all
/// CURRENT; release the row it replaces and, first, the calling VM's queued
/// roots. Allocates nothing and runs no Java.
fn randomized_store_row<K: Eq + std::hash::Hash>(
    ctx: &mut dyn NativeContext,
    table: &parking_lot::Mutex<std::collections::HashMap<K, RandomizedRoot>>,
    key: K,
    obj: ObjectRef,
    owner: ObjectRef,
    second: Option<ObjectRef>,
) {
    randomized_release_deferred(ctx);
    let Some(entry) = randomized_root_entry(ctx, obj, owner, second) else {
        return;
    };
    let old = table.lock().insert(key, entry);
    if let Some(old) = old {
        if old.root != entry.root {
            randomized_release_root(ctx, old);
        }
    }
}

fn randomized_context_cache_store(
    ctx: &mut dyn NativeContext,
    key: RandomizedContextCacheKey,
    context: ObjectRef,
    thread: ObjectRef,
) {
    randomized_store_row(ctx, randomized_context_cache(), key, context, thread, None);
}

fn randomized_random_cache() -> &'static parking_lot::Mutex<
    std::collections::HashMap<RandomizedRandomCacheKey, RandomizedRoot>,
> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<RandomizedRandomCacheKey, RandomizedRoot>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn randomized_random_cache_lookup(
    ctx: &dyn NativeContext,
    key: RandomizedRandomCacheKey,
    owners: RandomizedOwners,
) -> Option<ObjectRef> {
    let entry = randomized_random_cache().lock().get(&key).copied()?;
    randomized_resolve_root(ctx, entry, owners)
}

fn randomized_random_cache_store(
    ctx: &mut dyn NativeContext,
    key: RandomizedRandomCacheKey,
    random: ObjectRef,
    context: ObjectRef,
    thread: ObjectRef,
) {
    randomized_store_row(ctx, randomized_random_cache(), key, random, context, Some(thread));
}

fn randomized_random_cache_invalidate(ctx: &mut dyn NativeContext, key: RandomizedRandomCacheKey) {
    randomized_release_deferred(ctx);
    let old = randomized_random_cache().lock().remove(&key);
    if let Some(old) = old {
        randomized_release_root(ctx, old);
    }
}

fn randomized_random_cache_key_for_context(
    ctx: &mut dyn NativeContext,
    context: ObjectRef,
) -> RandomizedRandomCacheKey {
    // `context`'s hash first: `current_thread_object` builds the thread mirror
    // on a thread's first call, an allocation (gc-common w31-b).
    let context = ctx.identity_hash_code(context);
    let thread = ctx.current_thread_object();
    RandomizedRandomCacheKey {
        vm: ctx.vm_identity(),
        context,
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
    // gc-common w31-b: `getThreadGroup` is Java, and `thread` is read after it
    // (its hash and address key the cache row; the error paths name it). It
    // is pinned before that call now; it used to be pinned only after it, at
    // its pre-call address.
    let thread_pin = ctx.pin_native_root(thread);
    let group_result = randomized_thread_group(ctx, thread);
    let thread = ctx.read_native_pin(thread_pin, thread);
    let group = match group_result {
        Ok(Some(Value::Object(Some(group)))) => group,
        Ok(_) => {
            ctx.unpin_native_roots(thread_pin);
            return Err(randomized_no_context_error(ctx, thread, true));
        }
        Err(e) => {
            ctx.unpin_native_roots(thread_pin);
            return Err(e);
        }
    };
    let key = RandomizedContextCacheKey {
        vm: ctx.vm_identity(),
        thread: ctx.identity_hash_code(thread),
        group: ctx.identity_hash_code(group),
    };
    // The row belongs to THIS thread (see `RandomizedRoot`); a live thread's
    // group is fixed, so the thread alone decides the answer.
    if let Some(context) =
        randomized_context_cache_lookup(ctx, key, randomized_owners(thread, None))
    {
        ctx.unpin_native_roots(thread_pin);
        return Ok(Some(Value::Object(Some(context))));
    }

    let contexts = match randomized_context_static_contexts(ctx) {
        Some(contexts) => contexts,
        None => {
            ctx.unpin_native_roots(thread_pin);
            return Err(randomized_no_context_error(ctx, thread, false));
        }
    };
    let mut current_group = group;
    // GC-safety: `contexts` was pinned per turn below and never READ BACK --
    // the pin kept the map alive and the local kept its pre-GC address, so from
    // the second turn on `get` dispatched on a stale receiver. `thread` is used
    // by the two error paths inside the loop for the same reason. Pin both
    // ONCE, outside, and re-read at the top of each turn; the per-turn
    // `contexts` pin is gone. (`thread` is pinned above.)
    let contexts_pin = ctx.pin_native_root(contexts);
    loop {
        let contexts = ctx.read_native_pin(contexts_pin, contexts);
        let thread = ctx.read_native_pin(thread_pin, thread);
        let group_pin = ctx.pin_native_root(current_group);
        let candidate = ctx.invoke_virtual(
            contexts,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(current_group))],
        )?;
        current_group = ctx.read_native_pin(group_pin, current_group);
        ctx.unpin_native_roots(group_pin);
        if let Some(Value::Object(Some(context))) = candidate {
            // `get` was Java: the owner's address is read again.
            let owner = ctx.read_native_pin(thread_pin, thread);
            randomized_context_cache_store(ctx, key, context, owner);
            // The oldest pin: releases `contexts_pin` too.
            ctx.unpin_native_roots(thread_pin);
            return Ok(Some(Value::Object(Some(context))));
        }
        let parent_result =
            ctx.invoke_virtual(current_group, "getParent", "()Ljava/lang/ThreadGroup;", &[])?;
        current_group = match parent_result {
            Some(Value::Object(Some(parent))) => parent,
            _ => {
                // `get` and `getParent` were Java: `thread` is read again
                // (gc-common w31-b; the error named its turn-start address).
                let thread = ctx.read_native_pin(thread_pin, thread);
                ctx.unpin_native_roots(thread_pin);
                return Err(randomized_no_context_error(ctx, thread, false));
            }
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
    // gc-common w31-b: the row belongs to THIS context on THIS thread (see
    // `RandomizedRoot`). The key's first `current_thread_object` may build
    // the thread mirror, and the randomness lookup is Java, so `context` is
    // pinned across both.
    let context_pin = ctx.pin_native_root(context);
    let key = randomized_random_cache_key_for_context(ctx, context);
    let context = ctx.read_native_pin(context_pin, context);
    let thread = ctx.current_thread_object();
    if let Some(random) =
        randomized_random_cache_lookup(ctx, key, randomized_owners(context, Some(thread)))
    {
        ctx.unpin_native_roots(context_pin);
        return Ok(Some(Value::Object(Some(random))));
    }
    let randomness = randomized_context_randomness(ctx, context);
    let context = ctx.read_native_pin(context_pin, context);
    ctx.unpin_native_roots(context_pin);
    let randomness = match randomness? {
        Some(Value::Object(Some(randomness))) => randomness,
        _ => return Ok(Some(Value::Object(None))),
    };
    let thread = ctx.current_thread_object();
    let random = match ctx.get_field_by_name(randomness, "random") {
        Value::Object(Some(random)) => random,
        _ => return Ok(Some(Value::Object(None))),
    };
    randomized_random_cache_store(ctx, key, random, context, thread);
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
    // gc-common w32-a: the key's `current_thread_object` may build the thread
    // mirror (an allocation) and `getPerThread` runs Java (`map.get`, the
    // `PerThreadResources` constructor, `Randomness.clone`); `this` was then
    // passed, and `randomness` pushed, through their entry addresses. Both
    // pinned; the oldest pin is released once, after the last read.
    let randomness_pin = ctx.pin_native_root(randomness);
    let this_pin = ctx.pin_native_root(this);
    let key = randomized_random_cache_key_for_context(ctx, this);
    randomized_random_cache_invalidate(ctx, key);
    let this = ctx.read_native_pin(this_pin, this);
    let per_thread = native_randomized_context_get_per_thread(ctx, &[Value::Object(Some(this))]);
    let randomness = ctx.read_native_pin(randomness_pin, randomness);
    ctx.unpin_native_roots(randomness_pin);
    let resources = match per_thread? {
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
    // gc-common w32-a: the key's `current_thread_object` may allocate (see
    // `native_randomized_context_push`); `this` is read back after it.
    let this_pin = ctx.pin_native_root(this);
    let key = randomized_random_cache_key_for_context(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
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
    std::collections::HashMap<RandomizedPerThreadCacheKey, RandomizedRoot>,
> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<RandomizedPerThreadCacheKey, RandomizedRoot>>,
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
    owners: RandomizedOwners,
) -> Option<ObjectRef> {
    let entry = randomized_per_thread_cache().lock().get(&key).copied()?;
    randomized_resolve_root(ctx, entry, owners)
}

fn randomized_per_thread_cache_store(
    ctx: &mut dyn NativeContext,
    key: RandomizedPerThreadCacheKey,
    resources: ObjectRef,
    context: ObjectRef,
    thread: ObjectRef,
) {
    randomized_store_row(
        ctx,
        randomized_per_thread_cache(),
        key,
        resources,
        context,
        Some(thread),
    );
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
                    .java_fma(float_array_elem(ctx, b, j + l), acc1[l]);
            }
            for l in 0..lanes {
                acc2[l] = float_array_elem(ctx, a, j + lanes + l)
                    .java_fma(float_array_elem(ctx, b, j + lanes + l), acc2[l]);
            }
            for l in 0..lanes {
                acc3[l] = float_array_elem(ctx, a, j + 2 * lanes + l)
                    .java_fma(float_array_elem(ctx, b, j + 2 * lanes + l), acc3[l]);
            }
            for l in 0..lanes {
                acc4[l] = float_array_elem(ctx, a, j + 3 * lanes + l)
                    .java_fma(float_array_elem(ctx, b, j + 3 * lanes + l), acc4[l]);
            }
            j += 4 * lanes;
        }
        while j < limit {
            for l in 0..lanes {
                acc1[l] = float_array_elem(ctx, a, j + l)
                    .java_fma(float_array_elem(ctx, b, j + l), acc1[l]);
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
        res = float_array_elem(ctx, a, i).java_fma(float_array_elem(ctx, b, i), res);
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
                acc1[l] = diff.java_fma(diff, acc1[l]);
            }
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + lanes + l)
                    - float_array_elem(ctx, b, j + lanes + l);
                acc2[l] = diff.java_fma(diff, acc2[l]);
            }
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + 2 * lanes + l)
                    - float_array_elem(ctx, b, j + 2 * lanes + l);
                acc3[l] = diff.java_fma(diff, acc3[l]);
            }
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + 3 * lanes + l)
                    - float_array_elem(ctx, b, j + 3 * lanes + l);
                acc4[l] = diff.java_fma(diff, acc4[l]);
            }
            j += 4 * lanes;
        }
        while j < limit {
            for l in 0..lanes {
                let diff = float_array_elem(ctx, a, j + l) - float_array_elem(ctx, b, j + l);
                acc1[l] = diff.java_fma(diff, acc1[l]);
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
        res = diff.java_fma(diff, res);
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
        sum = d.java_fma(d, sum);
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
        let dequantized = step.java_fma(q as f32, lower);
        let error = v - dequantized;
        e2 = error.java_fma(error, e2);
        xe = v.java_fma(error, xe);
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
        a = y.java_fma(y, a);
        b = y.java_fma(x, b);
        c = x.java_fma(x, c);
        d = y.java_fma(v, d);
        e = x.java_fma(v, e);
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
            dot_product = value.java_fma(dot_value, dot_product);
        }
        let c = value - center;
        set_float_array_elem(ctx, centered, i, c);
        min = java_f32_min(min, c);
        max = java_f32_max(max, c);
        norm2 = c.java_fma(c, norm2);
        let delta = c - mean;
        mean += delta / ((i + 1) as f32);
        let delta2 = c - mean;
        variance_sum = delta.java_fma(delta2, variance_sum);
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
    // gc-common w21-c: `this` is pinned before the first allocation. It used
    // to be pinned only after the score-doc array and every `ScoreDoc` had
    // been allocated (and `ScoreDoc.<clinit>` could have run), i.e. the pin
    // took a possibly stale address; and the array and the `Relation`
    // constant were held raw across `earlyTerminated()` / `visitedCount()`
    // (Java) into `native_es_make_top_docs`.
    let this_pin = ctx.pin_native_root(this);
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
    let score_docs_pin = ctx.pin_native_root(score_docs);
    if len > 0 {
        let score_doc_class = ctx.ensure_class_initialized("org/apache/lucene/search/ScoreDoc")?;
        let score_doc_fields = ctx.class_num_total_fields(score_doc_class).max(3);
        for (i, raw) in values.drain(..).enumerate() {
            let hit = native_es_new_score_doc(ctx, score_doc_class, score_doc_fields, raw);
            let score_docs = ctx.read_native_pin(score_docs_pin, score_docs);
            ctx.set_array_element(score_docs, i, Value::Object(Some(hit)));
        }
    }

    // NOT branch-exclusive, despite an `} else {` sitting between this call and
    // the `visitedCount()` below -- that else belongs to `relation_name`. Both
    // calls run, with an allocating `lucene_static_object` between them.
    let this = ctx.read_native_pin(this_pin, this);
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
    let relation_pin = ctx.pin_native_root(relation);
    let this = ctx.read_native_pin(this_pin, this);
    let visited_count = match ctx.invoke_virtual(this, "visitedCount", "()J", &[])? {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as i64,
        _ => 0,
    };
    let score_docs = ctx.read_native_pin(score_docs_pin, score_docs);
    let relation = ctx.read_native_pin(relation_pin, relation);
    ctx.unpin_native_roots(this_pin);
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
    // gc-common w19-f: the input, the query and the score arrays were held raw
    // across the scratch array's allocation and `readBytes` (Java), and the
    // scratch array itself across `readBytes`: every read and the score
    // stores went through pre-collection addresses. All rooted, re-read after.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let input_h = scope.root(input);
    let query_h = scope.root(query);
    let scores_h = scope.root(scores);
    let packed = scope.new_array(cratonvm_types::ArrayElementType::Byte, byte_len);
    let packed_h = scope.root(packed);
    let input = scope.get(&input_h);
    scope.invoke_virtual(
        input,
        "readBytes",
        "([BII)V",
        &[
            Value::Object(Some(packed)),
            Value::Int(0),
            Value::Int(byte_len as i32),
        ],
    )?;

    let query = scope.get(&query_h);
    let mut query_bytes = vec![0u8; dimensions as usize];
    if scope.read_byte_array_into(query, 0, &mut query_bytes) != query_bytes.len() {
        return Ok(None);
    }
    let packed = scope.get(&packed_h);
    let mut packed_bytes = vec![0u8; byte_len];
    if scope.read_byte_array_into(packed, 0, &mut packed_bytes) != byte_len {
        return Ok(None);
    }
    let scores = scope.get(&scores_h);
    for vector in 0..count as usize {
        let start = vector * dimensions as usize;
        let mut dot = 0i32;
        for dimension in 0..dimensions as usize {
            dot = dot.wrapping_add(
                (packed_bytes[start + dimension] as i8 as i32)
                    .wrapping_mul(query_bytes[dimension] as i8 as i32),
            );
        }
        scope.set_array_element(scores, vector, Value::Float(dot as f32));
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

    // Everything still live across the GC points below is rooted in one handle
    // scope: `score_docs`, `reader`, every hit (stored back into `score_docs`),
    // the three result arrays and `this`. gc-common w19-f: before, `this` was
    // never rooted (it is written last, after four Java calls and three
    // allocations), the `docs` / `scores` / `segmentStarts` arrays were held
    // raw across `leaves()`, `size()`, `get()` and `getContext()`, `reader`
    // was read raw after the `get` loop, and none of the pins was released.
    // This is the `format_impl` shape with a collection instead of one array.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let sd_h = scope.root(score_docs);
    let rd_h = scope.root(reader);
    let hit_hs: Vec<_> = hits.iter().map(|h| scope.root(h.2)).collect();
    let docs_arr = scope.new_array(cratonvm_types::ArrayElementType::Int, len);
    let docs_h = scope.root(docs_arr);
    let scores_arr = scope.new_array(cratonvm_types::ArrayElementType::Float, len);
    let scores_h = scope.root(scores_arr);
    let docs_arr = scope.get(&docs_h);
    let score_docs = scope.get(&sd_h);
    let mut docs = Vec::with_capacity(len);
    for (i, (doc, score, _, _)) in hits.into_iter().enumerate() {
        let hit = scope.get(&hit_hs[i]);
        docs.push(doc);
        scope.set_array_element(score_docs, i, Value::Object(Some(hit)));
        scope.set_array_element(docs_arr, i, Value::Int(doc));
        scope.set_array_element(scores_arr, i, Value::Float(score));
    }

    let reader = scope.get(&rd_h);
    let leaves = match scope.invoke_virtual(reader, "leaves", "()Ljava/util/List;", &[])? {
        Some(Value::Object(Some(leaves))) => leaves,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let leaves_h = scope.root(leaves);
    let leaves_size = match scope.invoke_virtual(leaves, "size", "()I", &[])? {
        Some(Value::Int(v)) if v >= 0 => v as usize,
        _ => 0,
    };
    let starts_len = leaves_size.saturating_add(1);
    let segment_starts = scope.new_array(cratonvm_types::ArrayElementType::Int, starts_len);
    let starts_h = scope.root(segment_starts);
    if starts_len > 0 {
        scope.set_array_element(segment_starts, starts_len - 1, Value::Int(len as i32));
    }
    if starts_len != 2 {
        let mut search_from = 0usize;
        for segment in 1..starts_len.saturating_sub(1) {
            let leaves = scope.get(&leaves_h);
            let leaf = match scope.invoke_virtual(
                leaves,
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Int(segment as i32)],
            )? {
                Some(Value::Object(Some(leaf))) => leaf,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let doc_base = scope
                .get_field_by_name(leaf, "docBase")
                .as_int()
                .unwrap_or(0);
            let rel = match docs[search_from..].binary_search(&doc_base) {
                Ok(idx) | Err(idx) => idx,
            };
            search_from = search_from.saturating_add(rel).min(docs.len());
            let segment_starts = scope.get(&starts_h);
            scope.set_array_element(segment_starts, segment, Value::Int(search_from as i32));
        }
    }

    let reader = scope.get(&rd_h);
    let context_identity = match scope.get_field_by_name(reader, "readerContext") {
        Value::Object(Some(context)) => scope.get_field_by_name(context, "identity"),
        _ => {
            let context = match scope.invoke_virtual(
                reader,
                "getContext",
                "()Lorg/apache/lucene/index/IndexReaderContext;",
                &[],
            )? {
                Some(Value::Object(Some(context))) => context,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            scope.get_field_by_name(context, "identity")
        }
    };

    let this = scope.get(&this_h);
    let docs_arr = scope.get(&docs_h);
    let scores_arr = scope.get(&scores_h);
    let segment_starts = scope.get(&starts_h);
    scope.set_field_by_name(
        this,
        "CLASS_NAME_HASH",
        Value::Int(java_string_hash_code_ascii(
            "org.elasticsearch.search.vectors.KnnScoreDocQuery",
        )),
    );
    scope.set_field_by_name(this, "docs", Value::Object(Some(docs_arr)));
    scope.set_field_by_name(this, "scores", Value::Object(Some(scores_arr)));
    scope.set_field_by_name(this, "segmentStarts", Value::Object(Some(segment_starts)));
    scope.set_field_by_name(this, "contextIdentity", context_identity);
    Ok(None)
}

pub(crate) fn native_randomized_context_get_per_thread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // gc-common w19-f: one handle scope. Before, `this` was unpinned after
    // `currentThread` and read again (`runner`) after `map.get` and the
    // `PerThreadResources` constructor (both Java), and every `?` exit leaked
    // the pins taken so far.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let thread = scope.current_thread_object();
    let this = scope.get(&this_h);

    let map = match scope.get_field_by_name(this, "perThreadResources") {
        Value::Object(Some(map)) => map,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cache_key = randomized_per_thread_key(&*scope, this, map, thread);
    // gc-common w31-b: the row belongs to THIS context on THIS thread (see
    // `RandomizedRoot`); a colliding (or dead) thread's row is a miss.
    if let Some(resources) = randomized_per_thread_cache_lookup(
        &*scope,
        cache_key,
        randomized_owners(this, Some(thread)),
    ) {
        return Ok(Some(Value::Object(Some(resources))));
    }

    let map_h = scope.root(map);
    let thread_h = scope.root(thread);
    let existing = scope.invoke_virtual(
        map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(thread))],
    )?;
    if let Some(Value::Object(Some(resources))) = existing {
        // `get` was Java: the owners are read again.
        let (context, thread) = (scope.get(&this_h), scope.get(&thread_h));
        randomized_per_thread_cache_store(&mut *scope, cache_key, resources, context, thread);
        return Ok(Some(Value::Object(Some(resources))));
    }

    let resources_result = scope.new_object_initialized(
        "com/carrotsearch/randomizedtesting/RandomizedContext$PerThreadResources",
        "(Lcom/carrotsearch/randomizedtesting/RandomizedContext$1;)V",
        &[Value::Object(None)],
    )?;
    let resources = match resources_result {
        Some(Value::Object(Some(resources))) => resources,
        _ => return Ok(Some(Value::Object(None))),
    };
    let resources_h = scope.root(resources);

    let deque = match scope.get_field_by_name(resources, "randomnesses") {
        Value::Object(Some(deque)) => deque,
        _ => return Ok(Some(Value::Object(None))),
    };
    let deque_h = scope.root(deque);
    let this = scope.get(&this_h);
    let runner = match scope.get_field_by_name(this, "runner") {
        Value::Object(Some(runner)) => runner,
        _ => return Ok(Some(Value::Object(None))),
    };
    let runner_randomness = match scope.get_field_by_name(runner, "runnerRandomness") {
        Value::Object(Some(randomness)) => randomness,
        _ => return Ok(Some(Value::Object(None))),
    };

    let thread = scope.get(&thread_h);
    let cloned_result = scope.invoke_virtual(
        runner_randomness,
        "clone",
        "(Ljava/lang/Thread;)Lcom/carrotsearch/randomizedtesting/Randomness;",
        &[Value::Object(Some(thread))],
    )?;
    let cloned = match cloned_result {
        Some(Value::Object(Some(cloned))) => cloned,
        _ => return Ok(Some(Value::Object(None))),
    };

    let deque = scope.get(&deque_h);
    scope.invoke_virtual(
        deque,
        "push",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(cloned))],
    )?;

    let map = scope.get(&map_h);
    let thread = scope.get(&thread_h);
    let resources = scope.get(&resources_h);
    scope.invoke_virtual(
        map,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(thread)), Value::Object(Some(resources))],
    )?;
    let resources = scope.get(&resources_h);
    let (context, thread) = (scope.get(&this_h), scope.get(&thread_h));

    randomized_per_thread_cache_store(&mut *scope, cache_key, resources, context, thread);
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
    // gc-common w32-a: a native pin, not a JNI global root. This runs once per
    // `writeByte` / `writeInt` / `writeLong` of a Lucene `ByteBuffersDataOutput`
    // and took the VM-wide global-ref table mutex twice per call; and a failed
    // `add_global_root` (handle 0) left `this` unrefreshed after
    // `appendBlock` (Java). The pin is thread-local and released on every
    // exit.
    let this_pin = ctx.pin_native_root(this);
    let result: MethodCallResult = (|| {
        let mut written = 0;
        while written < bytes.len() {
            let this = ctx.read_native_pin(this_pin, this);
            let mut block = lucene_byte_buffers_data_output_current_block(ctx, this)
                .ok_or_else(|| lucene_iobe("ByteBuffersDataOutput currentBlock is null"))?;
            if heap_byte_buffer_remaining(ctx, block) == 0 {
                block = lucene_byte_buffers_data_output_append_block(ctx, this)?;
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
    ctx.unpin_native_roots(this_pin);
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
        lucene_data_output_write_bytes_one_by_one(ctx, this, &buf[..len])
    }
}

/// `writeByte` each of `bytes` on a generic `DataOutput` (Java per byte),
/// `this` pinned across them. gc-common w32-a: the two vInt / vLong callers
/// never released their pin, so a native writing many vInts through a
/// non-`ByteBuffersDataOutput` output grew the thread's pin stack by one slot
/// per value until the native returned.
fn lucene_data_output_write_bytes_one_by_one(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let mut result: MethodCallResult = Ok(None);
    for b in bytes {
        let this = ctx.read_native_pin(this_pin, this);
        if let Err(e) = lucene_data_output_write_byte_direct(ctx, this, *b) {
            result = Err(e);
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    result
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
        lucene_data_output_write_bytes_one_by_one(ctx, this, &buf[..len])
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
    // gc-common w32-a: `this` and `input` are pinned once for the whole copy
    // (native pins, not a JNI global root plus four pins per block), and read
    // back at the top of every turn; the block is pinned across `readBytes`.
    let this_pin = ctx.pin_native_root(this);
    let input_pin = ctx.pin_native_root(input);
    let result: MethodCallResult = (|| {
        let mut remaining = remaining;
        while remaining > 0 {
            let this = ctx.read_native_pin(this_pin, this);
            let mut block = lucene_byte_buffers_data_output_current_block(ctx, this)
                .ok_or_else(|| lucene_iobe("ByteBuffersDataOutput currentBlock is null"))?;
            if heap_byte_buffer_remaining(ctx, block) == 0 {
                block = lucene_byte_buffers_data_output_append_block(ctx, this)?;
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
            // `appendBlock` above was Java: `input` is read back here (it used
            // to be pinned only now, through its pre-`appendBlock` address).
            let input = ctx.read_native_pin(input_pin, input);
            let block_pin = ctx.pin_native_root(block);
            let read = ctx.invoke_virtual(
                input,
                "readBytes",
                "([BII)V",
                &[
                    Value::Object(Some(hb)),
                    Value::Int(raw as i32),
                    Value::Int(n as i32),
                ],
            );
            let block = ctx.read_native_pin(block_pin, block);
            ctx.unpin_native_roots(block_pin);
            read?;
            ctx.set_field_by_name(block, "position", Value::Int(position + n as i32));
            remaining -= n;
        }
        Ok(None)
    })();
    // The oldest pin: releases `input_pin` too.
    ctx.unpin_native_roots(this_pin);
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
    // gc-common w32-a: a native pin across `next()` (Java), not a JNI global
    // root: this runs once per term of a merge, and took the VM-wide
    // global-ref table mutex twice per term.
    let this_pin = ctx.pin_native_root(this);
    let next = match lucene_field_obj(ctx, this, "termsEnum") {
        Ok(terms_enum) => ctx.invoke_virtual(
            terms_enum,
            "next",
            "()Lorg/apache/lucene/util/BytesRef;",
            &[],
        ),
        Err(e) => Err(e),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let current = match next? {
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
    // gc-common w32-a: native pins, not two JNI global roots (four VM-wide
    // global-ref table lock round trips per term copied).
    let state_pin = ctx.pin_native_root(state);
    let index_pin = ctx.pin_native_root(index);
    let result: MethodCallResult = (|| {
        let mut state = state;
        let mut index = index;
        let src = lucene_field_obj(ctx, index, "currentTerm")?;
        let (mut src_bytes, src_off, src_len) = lucene_bytes_ref_parts(ctx, src)?;
        let builder = lucene_field_obj(ctx, state, "term")?;
        let mut builder_ref = lucene_field_obj(ctx, builder, "ref")?;
        let mut dst_bytes = lucene_field_obj(ctx, builder_ref, "bytes")?;
        if ctx.array_length(dst_bytes) < src_len {
            ctx.invoke_virtual(builder, "growNoCopy", "(I)V", &[Value::Int(src_len as i32)])?;
            state = ctx.read_native_pin(state_pin, state);
            index = ctx.read_native_pin(index_pin, index);
            let builder = lucene_field_obj(ctx, state, "term")?;
            builder_ref = lucene_field_obj(ctx, builder, "ref")?;
            dst_bytes = lucene_field_obj(ctx, builder_ref, "bytes")?;
            // gc-common w20-g: the SOURCE bytes were read below through
            // their pre-`growNoCopy` address; re-derive them from `index`.
            let src = lucene_field_obj(ctx, index, "currentTerm")?;
            src_bytes = lucene_bytes_ref_parts(ctx, src)?.0;
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
    // The oldest pin: releases `index_pin` too.
    ctx.unpin_native_roots(state_pin);
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
    // gc-common w20-g: the heap array's allocation can collect; the three
    // stores below went through `this`'s entry address. Pinned across it.
    let this_pin = ctx.pin_native_root(this);
    let heap = ctx.new_array(cratonvm_types::ArrayElementType::Reference, heap_len);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
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
    // gc-common w32-a: a native pin, not a JNI global root (once per byte
    // written through a `MockDirectoryWrapper`).
    let this_pin = ctx.pin_native_root(this);
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
            this = ctx.read_native_pin(this_pin, this);
            if matches!(split, Some(Value::Int(0))) {
                ctx.invoke("java/lang/Thread", "yield", "()V", &[])?;
                this = ctx.read_native_pin(this_pin, this);
            }
        }

        let out = lucene_field_obj(ctx, this, "out")?;
        ctx.invoke_virtual(out, "writeByte", "(B)V", &[Value::Int(byte as i8 as i32)])?;
        this = ctx.read_native_pin(this_pin, this);

        let dir = lucene_field_obj(ctx, this, "dir")?;
        ctx.invoke_virtual(dir, "maybeThrowDeterministicException", "()V", &[])?;
        this = ctx.read_native_pin(this_pin, this);

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
    ctx.unpin_native_roots(this_pin);
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
    // gc-common w20-g: `new_object` allocates (and may initialise the class);
    // every field below was then copied from `this`'s entry address. Pinned
    // across it.
    let this_pin = ctx.pin_native_root(this);
    let created = ctx.new_object("org/apache/lucene/store/ByteBuffersDataInput");
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let new_obj: ObjectRef = match created? {
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
    // gc-common w20-g: `ctl.get()` is Java; when it answers no int the
    // fallback below read `this` through its entry address. Pinned across it.
    let this_pin = ctx.pin_native_root(this);
    if executor_has_real_workers(ctx, this) {
        if let Value::Object(Some(ctl)) = ctx.get_field_by_name(this, "ctl") {
            if let Some(Value::Int(state)) = ctx.invoke_virtual(ctl, "get", "()I", &[])? {
                // ThreadPoolExecutor encodes RUNNING with the sign bit set;
                // every shutdown/stop/terminated state is non-negative.
                return Ok(Some(Value::Int((state >= 0) as i32)));
            }
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
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

#[cfg(test)]
mod w22a_randomized_cache_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Private VM identities: no other test uses them.
    const W22A_VM_A: usize = 0x0A22_5A08;
    const W22A_VM_B: usize = 0x0A22_5B08;

    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_randomized_caches(W22A_VM_A);
            forget_vm_randomized_caches(W22A_VM_B);
            // gc-common w32-a: the rows' owner lock keys.
            forget_vm_lock_keys(W22A_VM_A);
            forget_vm_lock_keys(W22A_VM_B);
        }
    }

    /// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
    /// item 4): the three RandomizedRunner caches hold rooted handles per VM
    /// only -- a handle that no longer resolves is a miss, never a raw
    /// pre-collection address -- and a VM's teardown drops its rows and no
    /// other VM's.
    #[test]
    fn w22a_randomized_caches_are_rooted_per_vm_and_forgotten_with_the_vm() {
        let _forget = Forget;
        let mut a = crate::test_utils::mock_ctx();
        a.set_vm_identity(W22A_VM_A);
        let mut b = crate::test_utils::mock_ctx();
        b.set_vm_identity(W22A_VM_B);

        let context_a = a.fresh_object_ref();
        let key_a = RandomizedContextCacheKey {
            vm: W22A_VM_A,
            thread: 1,
            group: 2,
        };
        let key_b = RandomizedContextCacheKey {
            vm: W22A_VM_B,
            thread: 1,
            group: 2,
        };
        // gc-common w31-b: every row names its owners' addresses; these
        // stand-ins are never dereferenced (w32-a: the rows also mint their
        // weak lock keys, which hash and compare the address only).
        // SAFETY: keys only, 8-byte aligned (lesson o).
        let o1 = unsafe { ObjectRef::from_raw(0x22A0_1008usize as *mut u8) };
        let o2 = unsafe { ObjectRef::from_raw(0x22A0_2008usize as *mut u8) };
        let owners = randomized_owners(o1, Some(o2));
        let thread_owner = randomized_owners(o1, None);
        randomized_context_cache_store(&mut a, key_a, context_a, o1);
        assert_eq!(
            randomized_context_cache_lookup(&a, key_a, thread_owner),
            Some(context_a)
        );
        assert_eq!(randomized_context_cache_lookup(&b, key_b, thread_owner), None);
        // A's root is gone: the row is a miss (the old code answered its raw
        // fallback address here).
        let root = randomized_context_cache()
            .lock()
            .get(&key_a)
            .map(|e| e.root)
            .expect("A's context row");
        assert!(a.remove_global_root(root));
        assert_eq!(randomized_context_cache_lookup(&a, key_a, thread_owner), None);

        let random_b = b.fresh_object_ref();
        let random_key_b = RandomizedRandomCacheKey {
            vm: W22A_VM_B,
            context: 3,
            thread: 4,
        };
        randomized_random_cache_store(&mut b, random_key_b, random_b, o1, o2);
        let per_thread_a = a.fresh_object_ref();
        let per_thread_key_a = RandomizedPerThreadCacheKey {
            vm: W22A_VM_A,
            context: 5,
            map: 6,
            thread: 7,
        };
        randomized_per_thread_cache_store(&mut a, per_thread_key_a, per_thread_a, o1, o2);
        assert_eq!(
            randomized_per_thread_cache_lookup(&a, per_thread_key_a, owners),
            Some(per_thread_a)
        );

        forget_vm_randomized_caches(W22A_VM_A);
        assert!(!randomized_context_cache().lock().contains_key(&key_a));
        assert!(!randomized_per_thread_cache()
            .lock()
            .contains_key(&per_thread_key_a));
        assert_eq!(
            randomized_random_cache_lookup(&b, random_key_b, owners),
            Some(random_b),
            "B's rows survive A's teardown"
        );
        forget_vm_randomized_caches(W22A_VM_B);
        assert!(!randomized_random_cache().lock().contains_key(&random_key_b));
    }
}

/// gc-common w31-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 32): a row keyed by identity hashes answers only the objects it was
/// filed for. The mock's identity hash is the address truncated to `i32`
/// (lesson qq): two addresses 4 GiB apart share one. A private VM identity,
/// forgotten alone by the guard (lesson pp).
#[cfg(test)]
mod w31b_randomized_cache_owner_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const VM: usize = 0x31B0_1E08;

    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_randomized_caches(VM);
            forget_vm_lock_keys(VM); // gc-common w32-a: the owners' keys
        }
    }

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: a key only; hashed and compared by address, never
        // dereferenced (8-byte aligned, lesson o).
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn a_colliding_thread_or_context_is_not_served_another_ones_row() {
        let _forget = Forget;
        let mut c = crate::test_utils::mock_ctx();
        c.set_vm_identity(VM);
        let (context, map) = (at(0x31B0_2008), at(0x31B0_3008));
        let (t1, t2) = (at(0x1_31B0_4008), at(0x2_31B0_4008));

        // Per-thread resources: two threads, one hash, one context.
        let k1 = randomized_per_thread_key(&c, context, map, t1);
        let k2 = randomized_per_thread_key(&c, context, map, t2);
        assert!(k1 == k2, "premise: the colliding threads share a key");
        let resources = c.fresh_object_ref();
        randomized_per_thread_cache_store(&mut c, k1, resources, context, t1);
        assert_eq!(
            randomized_per_thread_cache_lookup(&c, k1, randomized_owners(context, Some(t1))),
            Some(resources)
        );
        assert_eq!(
            randomized_per_thread_cache_lookup(&c, k2, randomized_owners(context, Some(t2))),
            None,
            "t2 was served t1's PerThreadResources"
        );
        // A moved thread misses (its caller recomputes and re-files).
        assert_eq!(
            randomized_per_thread_cache_lookup(
                &c,
                k1,
                randomized_owners(context, Some(at(0x3_31B0_4008))),
            ),
            None
        );

        // Random: two contexts, one hash, one thread.
        let (c1, c2) = (at(0x1_31B0_5008), at(0x2_31B0_5008));
        assert_eq!(c.identity_hash_code(c1), c.identity_hash_code(c2), "premise");
        let key = RandomizedRandomCacheKey {
            vm: VM,
            context: c.identity_hash_code(c1),
            thread: c.identity_hash_code(t1),
        };
        let random = c.fresh_object_ref();
        randomized_random_cache_store(&mut c, key, random, c1, t1);
        assert_eq!(
            randomized_random_cache_lookup(&c, key, randomized_owners(c1, Some(t1))),
            Some(random)
        );
        assert_eq!(
            randomized_random_cache_lookup(&c, key, randomized_owners(c2, Some(t1))),
            None,
            "context c2 was served c1's Random"
        );
    }
}

/// gc-common w32-a (`common-w31b-randomized-runner-caches-root-dead-threads-rows-until-vm-teardown`):
/// a row goes with the first of its owners a lock-key sweep finds dead, and
/// its global root with the VM's next store. The sweep is the real one
/// (`lib.rs::gc_sweep_lock_keys`, which fans the freed keys out to
/// [`forget_randomized_keys`]). The mock's identity hash is the address
/// truncated to `i32` (lesson qq), so the 4 GiB-apart stand-ins share one.
/// Private VM identities, forgotten alone by the guard (lesson pp); only this
/// test's rows are counted (lesson nn).
#[cfg(test)]
mod w32a_randomized_weak_row_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // One VM per test (a sweep judges every slot of its VM): the first test
    // uses `VM_A`, the second `VM_B` and `VM_C`.
    const VM_A: usize = 0x32A0_1A08;
    const VM_B: usize = 0x32A0_1B08;
    const VM_C: usize = 0x32A0_1C08;

    /// Forgets exactly the VMs its test used.
    struct Forget(&'static [usize]);
    impl Drop for Forget {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_randomized_caches(vm);
                forget_vm_lock_keys(vm);
            }
        }
    }

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: a key only; hashed and compared by address, never
        // dereferenced (8-byte aligned, lesson o).
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn addr(obj: ObjectRef) -> usize {
        obj.as_ptr() as usize
    }

    fn per_thread_root(key: &RandomizedPerThreadCacheKey) -> Option<usize> {
        randomized_per_thread_cache().lock().get(key).map(|row| row.root)
    }

    #[test]
    fn a_dead_threads_rows_go_at_the_sweep_and_their_roots_at_the_next_store() {
        let _forget = Forget(&[VM_A]);
        let mut c = crate::test_utils::mock_ctx();
        c.set_vm_identity(VM_A);
        let base = c.global_root_count();
        let (context, map) = (at(0x32A0_2008), at(0x32A0_3008));
        let dead = at(0x1_32A0_4008);
        let live = at(0x32A0_5008);
        // `twin` shares `dead`'s hash (4 GiB apart) and stays alive.
        let twin = at(0x2_32A0_4008);
        assert_eq!(c.identity_hash_code(dead), c.identity_hash_code(twin), "premise");

        // The dead thread's three rows: per-thread resources, Random, context.
        let dead_key = randomized_per_thread_key(&c, context, map, dead);
        let dead_resources = c.fresh_object_ref();
        randomized_per_thread_cache_store(&mut c, dead_key, dead_resources, context, dead);
        let random_key = RandomizedRandomCacheKey {
            vm: VM_A,
            context: c.identity_hash_code(context),
            thread: c.identity_hash_code(dead),
        };
        let dead_random = c.fresh_object_ref();
        randomized_random_cache_store(&mut c, random_key, dead_random, context, dead);
        let context_key = RandomizedContextCacheKey {
            vm: VM_A,
            thread: c.identity_hash_code(dead),
            group: 0x32A0_6008,
        };
        let dead_context_obj = c.fresh_object_ref();
        randomized_context_cache_store(&mut c, context_key, dead_context_obj, dead);

        // A live thread's row.
        let live_key = randomized_per_thread_key(&c, context, map, live);
        assert!(live_key != dead_key, "premise: distinct hashes");
        let live_resources = c.fresh_object_ref();
        randomized_per_thread_cache_store(&mut c, live_key, live_resources, context, live);
        assert_eq!(c.global_root_count(), base + 4);

        let dead_roots = [
            per_thread_root(&dead_key).expect("dead per-thread row"),
            randomized_random_cache()
                .lock()
                .get(&random_key)
                .map(|row| row.root)
                .expect("dead Random row"),
            randomized_context_cache()
                .lock()
                .get(&context_key)
                .map(|row| row.root)
                .expect("dead context row"),
        ];

        // A collection finds `dead` gone (and everything else alive).
        let dropped = crate::gc_sweep_lock_keys(VM_A, &|a| a != addr(dead));
        assert_eq!(dropped, 1, "only the dead thread's key is freed");
        assert!(per_thread_root(&dead_key).is_none(), "dead per-thread row kept");
        assert!(!randomized_random_cache().lock().contains_key(&random_key));
        assert!(!randomized_context_cache().lock().contains_key(&context_key));
        assert_eq!(
            randomized_per_thread_cache_lookup(&c, live_key, randomized_owners(context, Some(live))),
            Some(live_resources),
            "a live thread's row survives"
        );
        // The sweep has no context: the roots are queued, still allocated.
        assert_eq!(c.global_root_count(), base + 4);

        // The next store releases them; `twin`, same hash as `dead`, files a
        // row of its own under the key `dead`'s row had.
        let twin_resources = c.fresh_object_ref();
        randomized_per_thread_cache_store(&mut c, dead_key, twin_resources, context, twin);
        for root in dead_roots {
            assert_eq!(c.resolve_global_root(root), None, "dead row's root {root} kept");
        }
        assert_eq!(c.global_root_count(), base + 2);
        assert_eq!(
            randomized_per_thread_cache_lookup(&c, dead_key, randomized_owners(context, Some(twin))),
            Some(twin_resources)
        );
        assert_eq!(
            randomized_per_thread_cache_lookup(&c, dead_key, randomized_owners(context, Some(dead))),
            None,
            "a dead thread is never served its same-hash successor's row"
        );

        // A second sweep that finds `dead` gone again frees nothing of `twin`'s:
        // its row names `twin`'s own key.
        crate::gc_sweep_lock_keys(VM_A, &|a| a != addr(dead));
        assert!(per_thread_root(&dead_key).is_some(), "twin's row dropped");

        // The context dies: every row it owns goes (the live thread's too).
        crate::gc_sweep_lock_keys(VM_A, &|a| a != addr(context));
        assert!(per_thread_root(&live_key).is_none());
        assert!(per_thread_root(&dead_key).is_none());
        randomized_release_deferred(&mut c);
        assert_eq!(c.global_root_count(), base);
    }

    #[test]
    fn a_sweep_of_one_vm_leaves_another_vms_rows() {
        let _forget = Forget(&[VM_B, VM_C]);
        let mut a = crate::test_utils::mock_ctx();
        a.set_vm_identity(VM_B);
        let mut b = crate::test_utils::mock_ctx();
        b.set_vm_identity(VM_C);
        // The same stand-in addresses in both VMs: each VM mints its own keys.
        let (context, map, thread) = (at(0x32A0_7008), at(0x32A0_8008), at(0x32A0_9008));
        let key_a = randomized_per_thread_key(&a, context, map, thread);
        let key_b = randomized_per_thread_key(&b, context, map, thread);
        assert!(key_a != key_b, "premise: rows are per VM");
        let resources_a = a.fresh_object_ref();
        let resources_b = b.fresh_object_ref();
        randomized_per_thread_cache_store(&mut a, key_a, resources_a, context, thread);
        randomized_per_thread_cache_store(&mut b, key_b, resources_b, context, thread);
        let owners = randomized_owners(context, Some(thread));

        // Everything of `a`'s VM dies.
        crate::gc_sweep_lock_keys(VM_B, &|_| false);
        assert!(per_thread_root(&key_a).is_none(), "A's row kept");
        assert_eq!(
            randomized_per_thread_cache_lookup(&b, key_b, owners),
            Some(resources_b),
            "B's row went with A's sweep"
        );
        // A's root is released through A's context, never B's.
        let b_roots = b.global_root_count();
        randomized_release_deferred(&mut b);
        assert_eq!(b.global_root_count(), b_roots);
        let a_roots = a.global_root_count();
        randomized_release_deferred(&mut a);
        assert_eq!(a.global_root_count(), a_roots - 1);
    }

    // `native_randomized_context_push`: the relocation test. Only that test
    // touches these.
    const VM_D: usize = 0x32A0_1D08;
    static OLD_RANDOMNESS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static NEW_RANDOMNESS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static RESOURCES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static PUSHED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn push_hook(
        ctx: &mut crate::test_utils::MockNativeContext,
        _receiver: ObjectRef,
        method: &str,
        _descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        use std::sync::atomic::Ordering::Relaxed;
        match method {
            // `perThreadResources.get(thread)` is Java: a collection moves
            // the `Randomness` being pushed.
            "get" => {
                ctx.remap_native_pin_addr_for_test(
                    OLD_RANDOMNESS.load(Relaxed),
                    NEW_RANDOMNESS.load(Relaxed),
                );
                Some(Ok(Some(Value::Object(Some(at(RESOURCES.load(Relaxed)))))))
            }
            "push" => {
                if let Some(Value::Object(Some(pushed))) = args.first() {
                    PUSHED.store(addr(*pushed), Relaxed);
                }
                Some(Ok(None))
            }
            _ => None,
        }
    }

    fn declare_field(
        ctx: &crate::test_utils::MockNativeContext,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) {
        ctx.set_declared_fields(
            class_id,
            vec![cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: class_id,
                is_static: false,
            }],
        );
    }

    /// gc-common w32-a: `RandomizedContext.push` held the `Randomness` raw
    /// across `getPerThread` (Java) and pushed its entry address.
    #[test]
    fn push_hands_the_deque_the_randomness_current_address() {
        use std::sync::atomic::Ordering::Relaxed;
        let _forget = Forget(&[VM_D]);
        let mut c = crate::test_utils::mock_ctx();
        c.set_vm_identity(VM_D);
        let context_class = ClassId::new(0x32A0_0D01);
        let resources_class = ClassId::new(0x32A0_0D02);
        declare_field(&c, context_class, "perThreadResources", "Ljava/util/Map;");
        declare_field(&c, resources_class, "randomnesses", "Ljava/util/ArrayDeque;");
        let context = c.alloc_object(context_class, 4);
        let resources = c.alloc_object(resources_class, 4);
        let map = c.fresh_object_ref();
        let deque = c.fresh_object_ref();
        c.set_field_by_name(context, "perThreadResources", Value::Object(Some(map)));
        c.set_field_by_name(resources, "randomnesses", Value::Object(Some(deque)));
        let randomness = c.fresh_object_ref();
        OLD_RANDOMNESS.store(addr(randomness), Relaxed);
        // The moved copy keeps the low 32 bits, i.e. the identity hash
        // (lesson qq). Never dereferenced: the hook only records it.
        NEW_RANDOMNESS.store(addr(randomness) + (1usize << 40), Relaxed);
        RESOURCES.store(addr(resources), Relaxed);
        c.set_invoke_virtual_hook(push_hook);
        let pins = c.native_pin_count_for_test();

        native_randomized_context_push(
            &mut c,
            &[Value::Object(Some(context)), Value::Object(Some(randomness))],
        )
        .expect("push");

        assert_eq!(
            PUSHED.load(Relaxed),
            NEW_RANDOMNESS.load(Relaxed),
            "push handed the deque the Randomness's pre-`getPerThread` address"
        );
        assert_eq!(c.native_pin_count_for_test(), pins, "push leaked a pin");
    }
}
