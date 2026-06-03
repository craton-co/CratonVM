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

fn table() -> &'static Mutex<FxHashMap<Key, Value>> {
    static T: OnceLock<Mutex<FxHashMap<Key, Value>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

#[inline]
fn key_for(cl: ObjectRef, this: ObjectRef) -> Key {
    (cl.as_ptr() as usize, this.as_ptr() as usize)
}

/// `get(ClassLoader)V` — return the stored value or null.
fn native_aclv_get(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cl = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = table().lock().get(&key_for(cl, this)).copied();
    Ok(Some(v.unwrap_or(Value::Object(None))))
}

/// `putIfAbsent(ClassLoader, V)V` — if no mapping exists for `(cl, this)`,
/// store `value` and return null. Otherwise return the existing value
/// (matching `Map.putIfAbsent` semantics).
fn native_aclv_put_if_absent(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cl = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let mut t = table().lock();
    let key = key_for(cl, this);
    if let Some(existing) = t.get(&key).copied() {
        return Ok(Some(existing));
    }
    if t.len() >= MAX_ENTRIES {
        // Refuse to grow further; treat as "already present with null".
        return Ok(Some(Value::Object(None)));
    }
    t.insert(key, value);
    Ok(Some(Value::Object(None)))
}

/// `remove(ClassLoader)V` — remove and return the previous value, or null.
fn native_aclv_remove(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cl = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let prev = table().lock().remove(&key_for(cl, this));
    Ok(Some(prev.unwrap_or(Value::Object(None))))
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
fn native_aclv_compute_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
            let v = table().lock().get(&key_for(cl, this)).copied();
            return Ok(Some(v.unwrap_or(Value::Object(None))));
        }
    };
    let key = key_for(cl, this);
    if let Some(existing) = table().lock().get(&key).copied() {
        return Ok(Some(existing));
    }
    // Invoke mappingFunction.apply(cl, this).
    let result = ctx.invoke_virtual(
        mapping_fn,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(cl)), Value::Object(Some(this))],
    )?;
    let val = result.unwrap_or(Value::Object(None));
    let mut t = table().lock();
    // Race: another caller may have raced ahead. If so, prefer the existing
    // entry (matches CHM.computeIfAbsent semantics).
    if let Some(existing) = t.get(&key).copied() {
        return Ok(Some(existing));
    }
    if t.len() < MAX_ENTRIES {
        t.insert(key, val);
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
        "remove",
        "(Ljava/lang/ClassLoader;)Ljava/lang/Object;",
        native_aclv_remove,
    );
    registry.register(
        class,
        "computeIfAbsent",
        "(Ljava/lang/ClassLoader;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_aclv_compute_if_absent,
    );
    registry.set_category(__prev_cat);
}
