// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Side-table-backed implementation of `jdk/internal/loader/AbstractClassLoaderValue`.
//!
//! ## Why
//!
//! Spring Boot apps (eureka-server, letsgo-main, sportme) hang in
//! `AbstractClassLoaderValue.putIfAbsent` (pc=29). The frame chain is:
//!
//! ```text
//! ClassLoader.getResource -> BootLoader.findResource -> ArchivedClassLoaders.archive
//!   -> ArchivedClassLoaders.<init> -> ServicesCatalog.getServicesCatalog (pc=27)
//!   -> AbstractClassLoaderValue.putIfAbsent (pc=29)  <-- infinite loop here
//! ```
//!
//! The JDK 25 source for `putIfAbsent` is:
//!
//! ```java
//! public V putIfAbsent(ClassLoader cl, V value) {
//!     ConcurrentHashMap<CLV, Object> map = map(cl);
//!     Object val = map.putIfAbsent(this, value);
//!     return extractValue(val);
//! }
//! ```
//!
//! `map(cl)` returns `cl.classLoaderValueMap`, lazily creating it via a CAS
//! on `ClassLoader.classLoaderValueMap`. Inside `ConcurrentHashMap.putIfAbsent`
//! the implementation drives a CAS loop on `Node.val` / `table` slots whose
//! field offsets are synthetic in our Unsafe layer. Round 9 partially fixed
//! Unsafe field-offset CAS, but ConcurrentHashMap-internal CAS still livelocks
//! for these particular call sites.
//!
//! ## Strategy
//!
//! Bypass the broken `ConcurrentHashMap` CAS path entirely by intercepting the
//! four public methods on `AbstractClassLoaderValue` that need to look up or
//! mutate the per-classloader map:
//!
//! - `get(ClassLoader)`             -> read from side-table
//! - `putIfAbsent(ClassLoader, V)`  -> non-CAS put-if-absent in side-table
//! - `remove(ClassLoader)`          -> remove from side-table
//! - `computeIfAbsent(ClassLoader, BiFunction)` -> compute-and-insert
//!
//! The side-table is keyed by `(ClassLoader.ptr, this.ptr)` -> `Value`.
//! Both objects are GC roots while the table holds them (the JDK keeps them
//! reachable through `cl.classLoaderValueMap` in the real implementation).
//!
//! ## Semantics
//!
//! `putIfAbsent` returns the OLD value if one was already present, or `null`
//! if the new value was stored.  This matches `Map.putIfAbsent` semantics,
//! which is what `AbstractClassLoaderValue.putIfAbsent` returns after passing
//! the result through `extractValue` (which unwraps `Memoizer` placeholders;
//! we never store `Memoizer` here, so the raw stored value is correct).

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;

use cratonvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{error::MethodCallResult, ObjectRef, Value};

/// Bound the per-process map to defend against runaway leaks. In practice
/// the JDK creates O(few) AbstractClassLoaderValue instances (ServicesCatalog,
/// ArchivedClassLoaders, etc.) per ClassLoader, so this is generous.
const MAX_ENTRIES: usize = 100_000;

type Key = (usize, usize);

/// GC note (cce0079 follow-up): entries are `(identity_key, Value)` pairs —
/// object values are registered as VarHandle roots at insert (kept alive +
/// registry-remapped across moving GCs) and re-resolved to their CURRENT
/// address on every read. Non-object values store `identity_key = 0`.
fn table() -> &'static Mutex<FxHashMap<Key, (i32, Value)>> {
    static T: OnceLock<Mutex<FxHashMap<Key, (i32, Value)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// GC-stable per-object key: identity hash + per-hash generation (same
/// pattern as `xnio_async::xnio_obj_key_for`). The former raw-address keys
/// went stale after every moving GC — lookups missed (a fresh
/// ServicesCatalog got computed per GC cycle) and a recycled address
/// aliased an unrelated loader's entry.
struct ClvObjKeyEntry {
    last_ptr: usize,
    generation: u32,
}

fn clv_obj_key_registry() -> &'static Mutex<FxHashMap<u32, Vec<ClvObjKeyEntry>>> {
    static R: OnceLock<Mutex<FxHashMap<u32, Vec<ClvObjKeyEntry>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn clv_obj_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let mut reg = clv_obj_key_registry().lock();
    let slots = reg.entry(hash).or_default();
    if let Some(slot) = slots.iter().find(|s| s.last_ptr == ptr) {
        return ((hash as usize) << 32) | (slot.generation as usize);
    }
    if slots.len() == 1 {
        slots[0].last_ptr = ptr;
        return ((hash as usize) << 32) | (slots[0].generation as usize);
    }
    let generation = slots.len() as u32;
    slots.push(ClvObjKeyEntry {
        last_ptr: ptr,
        generation,
    });
    ((hash as usize) << 32) | (generation as usize)
}

#[inline]
fn key_for(ctx: &dyn NativeContext, cl: ObjectRef, this: ObjectRef) -> Key {
    (clv_obj_key_for(ctx, cl), clv_obj_key_for(ctx, this))
}

/// Root an object value at insert and pair it with its identity key so
/// readers can re-resolve the current address.
fn rooted_entry(ctx: &mut dyn NativeContext, value: Value) -> (i32, Value) {
    if let Value::Object(Some(o)) = value {
        ctx.register_var_handle_root(o);
        (ctx.identity_hash_code(o), value)
    } else {
        (0, value)
    }
}

/// Resolve a stored entry to the value's CURRENT address.
fn resolve_entry(ctx: &dyn NativeContext, entry: (i32, Value)) -> Value {
    match entry {
        (vkey, Value::Object(Some(stored))) if vkey != 0 => {
            Value::Object(Some(ctx.read_var_handle_root(vkey).unwrap_or(stored)))
        }
        (_, v) => v,
    }
}

/// `get(ClassLoader)V` — return the stored value or null.
fn native_aclv_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cl = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = key_for(ctx, cl, this);
    let v = table().lock().get(&key).copied();
    Ok(Some(
        v.map(|e| resolve_entry(ctx, e))
            .unwrap_or(Value::Object(None)),
    ))
}

/// `putIfAbsent(ClassLoader, V)V` — if no mapping exists for `(cl, this)`,
/// store `value` and return null. Otherwise return the existing value
/// (matching `Map.putIfAbsent` semantics).
fn native_aclv_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cl = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let key = key_for(ctx, cl, this);
    // Root outside the table lock (rooted_entry only touches VM-side
    // registries, but keep the lock's critical section minimal).
    let entry = rooted_entry(ctx, value);
    let mut t = table().lock();
    if let Some(existing) = t.get(&key).copied() {
        drop(t);
        return Ok(Some(resolve_entry(ctx, existing)));
    }
    if t.len() >= MAX_ENTRIES {
        // Refuse to grow further; treat as "already present with null".
        return Ok(Some(Value::Object(None)));
    }
    t.insert(key, entry);
    Ok(Some(Value::Object(None)))
}

/// `remove(ClassLoader)V` — remove and return the previous value, or null.
fn native_aclv_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cl = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = key_for(ctx, cl, this);
    let prev = table().lock().remove(&key);
    Ok(Some(
        prev.map(|e| resolve_entry(ctx, e))
            .unwrap_or(Value::Object(None)),
    ))
}

/// `computeIfAbsent(ClassLoader, BiFunction)V` — JDK source:
///
/// ```java
/// public V computeIfAbsent(ClassLoader cl, BiFunction<? super ClassLoader, ? super CLV, ? extends V> mappingFunction) {
///     // ... uses Memoizer; on miss, invokes mappingFunction.apply(cl, this) once
/// }
/// ```
///
/// We reproduce the observable effect: if a mapping already exists, return it.
/// Otherwise call `mappingFunction.apply(cl, this)`, store the result, and
/// return it.
fn native_aclv_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cl = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mapping_fn = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        // No mapping function — behave like `get`.
        _ => {
            let key = key_for(ctx, cl, this);
            let v = table().lock().get(&key).copied();
            return Ok(Some(
                v.map(|e| resolve_entry(ctx, e))
                    .unwrap_or(Value::Object(None)),
            ));
        }
    };
    // GC-stable keys (cce0079 follow-up): the invoke below can move
    // `cl`/`this`; identity-hash keys stay valid across the move, so the
    // post-invoke racing re-check finds the same entry (the former
    // raw-address key silently missed it after a GC).
    let key = key_for(ctx, cl, this);
    if let Some(existing) = table().lock().get(&key).copied() {
        return Ok(Some(resolve_entry(ctx, existing)));
    }
    // Invoke mappingFunction.apply(cl, this).
    let result = ctx.invoke_virtual(
        mapping_fn,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(cl)), Value::Object(Some(this))],
    )?;
    let val = result.unwrap_or(Value::Object(None));
    let entry = rooted_entry(ctx, val);
    let mut t = table().lock();
    // Race: another caller may have raced ahead. If so, prefer the existing
    // entry (matches CHM.computeIfAbsent semantics).
    if let Some(existing) = t.get(&key).copied() {
        drop(t);
        return Ok(Some(resolve_entry(ctx, existing)));
    }
    if t.len() < MAX_ENTRIES {
        t.insert(key, entry);
    }
    Ok(Some(val))
}

/// Register the AbstractClassLoaderValue natives on the abstract base class
/// itself. JDK subclasses (`Sub`, `ServicesCatalog$ProvidersCache`,
/// `ArchivedClassLoaders.RESOURCES_CACHE` etc.) inherit these methods, so
/// dispatch on a subclass receiver still resolves up to the base.
pub fn register_classloader_value_sidetable(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let class = "jdk/internal/loader/AbstractClassLoaderValue";
    registry.register(
        class,
        "get",
        "(Ljava/lang/ClassLoader;)Ljava/lang/Object;",
        native_aclv_get,
    );
    registry.register(
        class,
        "putIfAbsent",
        "(Ljava/lang/ClassLoader;Ljava/lang/Object;)Ljava/lang/Object;",
        native_aclv_put_if_absent,
    );
    registry.register(
        class,
        "computeIfAbsent",
        "(Ljava/lang/ClassLoader;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_aclv_compute_if_absent,
    );
    registry.set_category(__prev_cat);
}
