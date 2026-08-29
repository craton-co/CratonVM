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
/// [`key_for`] with a loader argument that may legitimately be NULL.
///
/// A null `ClassLoader` names the BOOTSTRAP loader, which is an ordinary key in
/// this map and the one the JDK's own users of `AbstractClassLoaderValue`
/// (`ArchivedClassLoaders`, `ServicesCatalog`) reach for. The bodies here used
/// to return early on it, so a mapping stored against it was unreadable.
///
/// The bootstrap loader is given a fixed key component distinct from any
/// identity hash a real loader object could produce.
fn key_for_loader(ctx: &dyn NativeContext, cl: Option<&Value>, this: ObjectRef) -> Key {
    match cl {
        Some(Value::Object(Some(c))) => key_for(ctx, *c, this),
        // `clv_obj_key_for` derives its value from an identity hash and a
        // generation counter, neither of which can reach `usize::MAX`, so this
        // sentinel cannot collide with a real loader's key.
        _ => (usize::MAX, clv_obj_key_for(ctx, this)),
    }
}

fn native_aclv_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // A NULL loader is the BOOTSTRAP loader: a legitimate, common key, not an
    // absent argument. All four bodies here treated it as "no loader given"
    // and returned early, so a value stored against the bootstrap loader could
    // never be read back -- and `ArchivedClassLoaders`/`ServicesCatalog`, which
    // are the JDK's own users of this class, key on exactly that loader.
    // MEASURED by `apps/probes/JdkInternalSweep.java`.
    let key = key_for_loader(ctx, args.get(1), this);
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
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // The bootstrap loader is a key -- see [`native_aclv_get`].
    let key = key_for_loader(ctx, args.get(1), this);
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

/// `remove(ClassLoader, Object)Z` — `Map.remove(key, value)` semantics: remove
/// the mapping only if it currently holds `value`, and say whether it did.
///
/// This body used to be shaped for a `remove(ClassLoader)` that returns the
/// previous value, and was NOT REGISTERED at all -- so real
/// `AbstractClassLoaderValue.remove` bytecode ran against the real map while
/// `get`/`putIfAbsent`/`computeIfAbsent` used this side table. The two never
/// met: a mapping removed through the real method stayed readable through the
/// natives.
///
/// Registering it is what makes the family coherent. Three of the four
/// operations on one storage and the fourth on another is not a partial
/// implementation, it is a contradiction -- and it is invisible until someone
/// removes something. MEASURED by `apps/probes/JdkInternalSweep.java`.
fn native_aclv_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // The bootstrap loader is a key -- see [`native_aclv_get`].
    let key = key_for_loader(ctx, args.get(1), this);
    let expected = args.get(2).copied().unwrap_or(Value::Object(None));
    let current = table().lock().get(&key).copied();
    let Some(entry) = current else {
        return Ok(Some(Value::Int(0)));
    };
    let held = resolve_entry(ctx, entry);
    // `Map.remove(k, v)` compares with `equals`, not identity: a caller that
    // put a `String` and removes an equal one must succeed.
    let matches = match (held, expected) {
        (Value::Object(Some(a)), Value::Object(Some(b))) => {
            a == b
                || matches!(
                    ctx.invoke_virtual(a, "equals", "(Ljava/lang/Object;)Z",
                        &[Value::Object(Some(b))]),
                    Ok(Some(Value::Int(1)))
                )
        }
        (Value::Object(None), Value::Object(None)) => true,
        _ => false,
    };
    if !matches {
        return Ok(Some(Value::Int(0)));
    }
    table().lock().remove(&key);
    Ok(Some(Value::Int(1)))
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
    let cl_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let mapping_fn = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        // No mapping function — behave like `get`.
        _ => {
            let key = key_for_loader(ctx, args.get(1), this);
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
    let key = key_for_loader(ctx, args.get(1), this);
    if let Some(existing) = table().lock().get(&key).copied() {
        return Ok(Some(resolve_entry(ctx, existing)));
    }
    // Invoke mappingFunction.apply(cl, this).
    let result = ctx.invoke_virtual(
        mapping_fn,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[cl_val, Value::Object(Some(this))],
    )?;
    let val = result.unwrap_or(Value::Object(None));
    // A mapping function that answers NULL is an error, not an instruction to
    // store null. `AbstractClassLoaderValue.Memoizer.get()` does
    // `Objects.requireNonNull(v)` on the mapping function's result, so the JDK
    // throws NPE and stores nothing; this stored null and returned it, so the
    // next `computeIfAbsent` saw a "present" mapping and never recomputed.
    // MEASURED by `apps/probes/JdkInternalSweep.java`.
    if matches!(val, Value::Object(None)) {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException { message: None }
            .into());
    }
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
    // The fourth operation on the same storage as the other three -- see
    // [`native_aclv_remove`] for why leaving it to real bytecode was a
    // contradiction rather than a gap.
    registry.register(
        class,
        "remove",
        "(Ljava/lang/ClassLoader;Ljava/lang/Object;)Z",
        native_aclv_remove,
    );
    registry.set_category(__prev_cat);
}
