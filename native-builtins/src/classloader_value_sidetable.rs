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
//! public methods on `AbstractClassLoaderValue` that need to look up or
//! mutate the per-classloader map:
//!
//! - `get(ClassLoader)`             -> read from side-table
//! - `putIfAbsent(ClassLoader, V)`  -> non-CAS put-if-absent in side-table
//! - `remove(ClassLoader, Object)`  -> remove from side-table
//! - `computeIfAbsent(ClassLoader, BiFunction)` -> compute-and-insert
//! - `removeAll(ClassLoader)`       -> remove this value's row for the loader
//!
//! The side-table is keyed by `(loader key, this key)` -> [`Row`], each key a
//! GC-stable identity key from the lib.rs lock-key registry (see
//! [`clv_obj_key_for`]). Neither object is rooted by the table: a row is
//! dropped by the first collection that finds EITHER its loader or its
//! `ClassLoaderValue` dead (the JDK's rows live in `cl.classLoaderValueMap`,
//! so they die with the loader too).
//!
//! ## The value lives as long as its loader (gc-common w17-d)
//!
//! In the JDK the row's VALUE hangs off a field of the loader
//! (`cl.classLoaderValueMap`), so loader, map and value form one cycle that
//! is collected with the loader. Until w17-d the value here was a permanent
//! var-handle root instead: everything it reached stayed alive, and a
//! `ServicesCatalog` (value) -> `ServiceProvider` -> `Module` -> loader chain
//! pinned a custom `ModuleLayer`'s loader, every class it defined and their
//! statics for good. The loader then never died, so its weak key was never
//! freed and the row was never dropped
//! (`common-w16b-clv-values-are-permanent-roots-that-pin-their-loader`).
//!
//! Now the table owns the value itself, re-addressed after every moving
//! collection ([`gc_update_clv_value_refs`]), and reports it from the
//! `classloader-values` row of `vm/src/memory/native_roots.rs`
//! ([`gc_scan_clv_value_roots`]). On a cycle whose marker follows
//! `metadata_pin` (the loader-conditional licence the statics, `ClassValue`
//! results and the other loader-owned native caches use) a value whose row
//! names a loader OBJECT is not a root: it is pinned to that loader, and is
//! marked only if the loader is. That is the JDK's `loader -> map -> value`
//! edge, keyed by the one kind of object CratonVM already has ephemeron-like
//! edges for. The bootstrap loader's rows, and every row on a cycle without
//! the licence (a moving young collection, a G1 evacuation pause), are
//! rooted outright, which over-retains for one cycle and is never unsound.
//!
//! ## Semantics
//!
//! `putIfAbsent` returns the OLD value if one was already present, or `null`
//! if the new value was stored.  This matches `Map.putIfAbsent` semantics,
//! which is what `AbstractClassLoaderValue.putIfAbsent` returns after passing
//! the result through `extractValue` (which unwraps `Memoizer` placeholders;
//! we never store `Memoizer` here, so the raw stored value is correct).
//!
//! **`ClassLoaderValue.Sub` is keyed by identity here.** The JDK's `Sub`
//! defines `equals`/`hashCode` over `(parent, key)` and `sub(key)` returns a
//! fresh `Sub` per call, so a caller doing `clv.sub(k).computeIfAbsent(..)`
//! misses its own earlier row and recomputes. `Proxy`'s cache is the JDK
//! user of that shape, and CratonVM serves `Proxy` natively, so no reached
//! caller is known. Stated so the next reader does not assume `Sub` works.

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;

use cratonvm_native_api::registry::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::{error::MethodCallResult, ObjectRef, Value};

/// Bound the per-process map to defend against runaway leaks. In practice
/// the JDK creates O(few) AbstractClassLoaderValue instances (ServicesCatalog,
/// ArchivedClassLoaders, etc.) per ClassLoader, so this is generous.
const MAX_ENTRIES: usize = 100_000;

/// `(loader key, ClassLoaderValue key)`. The loader half is `None` for the
/// BOOTSTRAP loader (a null `ClassLoader` argument), which has no object.
type Key = (Option<usize>, usize);

/// One mapping.
#[derive(Clone, Copy)]
struct Row {
    /// The VM whose natives stored the row. The root scan and the remap walk
    /// only the collecting VM's rows: `loader` and `value` are addresses in
    /// that VM's heap.
    vm: usize,
    /// The row's loader, `None` for the bootstrap loader. NOT a root: it is
    /// kept only so the root scan can pin `value` to it, and is re-addressed
    /// by [`gc_update_clv_value_refs`]. A dead loader's row is dropped by
    /// [`forget_clv_keys`] in the same collection's epilogue.
    loader: Option<ObjectRef>,
    /// The stored value, always non-null (`putIfAbsent` and
    /// `computeIfAbsent` refuse a null, as the JDK's map does). Current after
    /// every moving collection ([`gc_update_clv_value_refs`]).
    value: Value,
    /// Unique per insert. `remove(cl, v)` compares the value OUTSIDE the lock
    /// (its `equals` runs Java), and deletes only if the row is still the one
    /// it compared.
    seq: u64,
}

struct Table {
    rows: FxHashMap<Key, Row>,
    next_seq: u64,
}

impl Table {
    fn insert(&mut self, key: Key, vm: usize, loader: Option<ObjectRef>, value: Value) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.rows.insert(
            key,
            Row {
                vm,
                loader,
                value,
                seq,
            },
        );
    }

    fn value_of(&self, key: &Key) -> Option<Value> {
        self.rows.get(key).map(|row| row.value)
    }
}

/// The process-wide table. Its rows are VM-scoped by their keys (lock keys
/// are unique across VMs) and by [`Row::vm`]. The lock is a leaf: nothing
/// that can allocate, run Java or take another lock runs under it, except
/// [`gc_scan_clv_value_roots`]'s `metadata_pin` insert, which is a leaf too.
fn table() -> &'static Mutex<Table> {
    static T: OnceLock<Mutex<Table>> = OnceLock::new();
    T.get_or_init(|| {
        Mutex::new(Table {
            rows: FxHashMap::default(),
            next_seq: 1,
        })
    })
}

/// GC-stable per-object key, from the lib.rs lock-key registry
/// (`gc_stable_weak_lock_key`).
///
/// gc-common w16-b (`common-w15b-three-more-identity-key-registry-copies`):
/// this used to be a private copy of the identity-hash + generation scheme
/// with no VM in its entries, no sweep, no remap, a "lone occupant" rebind
/// with no `hash != 0` guard, and generations minted as `slots.len()`. Two
/// VMs number identity hashes from the same seed, so VM B's
/// `AbstractClassLoaderValue.get(loader)` was handed VM A's key for the same
/// (loader, CLV) hash pair and read A's row -- whose stored `ObjectRef` is an
/// address in A's heap. The shared registry is VM-scoped, never re-mints a
/// key, re-addresses its slots after every moving collection, and drops a
/// dead object's slot at the first sweep that finds it dead; the sweep then
/// calls [`forget_clv_keys`] with the freed keys.
fn clv_obj_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

#[inline]
fn key_for(ctx: &dyn NativeContext, cl: ObjectRef, this: ObjectRef) -> Key {
    (Some(clv_obj_key_for(ctx, cl)), clv_obj_key_for(ctx, this))
}

/// The loader OBJECT a `ClassLoader` argument names; `None` for the
/// bootstrap loader (a null argument).
#[inline]
fn loader_arg(cl: Option<&Value>) -> Option<ObjectRef> {
    match cl {
        Some(Value::Object(Some(c))) => Some(*c),
        _ => None,
    }
}

/// Drop every row whose loader OR `ClassLoaderValue` key is in `keys`, which
/// the lock-key registry has just freed (the object died, or its VM is being
/// torn down) and will never mint again. Called by `lib.rs::sweep_lock_keys`
/// and `lib.rs::forget_vm_lock_keys` with the registry lock released; the
/// table lock is a leaf.
///
/// Dropping the row is all it takes to free the value (gc-common w17-d): the
/// row was the value's only native reference, and the value was never a
/// var-handle root.
///
/// **This hook is load-bearing since w17-d.** A row whose loader died but
/// which survived this hook would hand the next root scan a stale loader
/// address to pin a stale value to. It relies on both keys being WEAK lock
/// keys, which the registry frees (and reports here) at the first sweep that
/// finds the object dead. A HELD key of the same object
/// (`gc_stable_lock_key`, today only the `t27_tls` `SSLContext` tables) would
/// be tombstoned instead and never reported; no `ClassLoader` or
/// `ClassLoaderValue` is an `SSLContext`, so no row can meet one.
pub(crate) fn forget_clv_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut t = table().lock();
    if t.rows.is_empty() {
        return;
    }
    // The table holds a handful of rows (a few per loader) while `keys` holds
    // every weak lock key this collection freed (`Properties`, their
    // `WeakReference`s, lock objects...): index the rows' keys, not `keys`.
    let named: rustc_hash::FxHashSet<usize> = t
        .rows
        .keys()
        .flat_map(|&(loader, clv)| loader.into_iter().chain(std::iter::once(clv)))
        .collect();
    let freed: rustc_hash::FxHashSet<usize> =
        keys.iter().copied().filter(|k| named.contains(k)).collect();
    if freed.is_empty() {
        return;
    }
    t.rows.retain(|(loader, clv), _| {
        !freed.contains(clv) && !loader.is_some_and(|l| freed.contains(&l))
    });
}

/// Root scan for the `classloader-values` row of
/// `vm/src/memory/native_roots.rs` (gc-common w17-d).
///
/// For each of `vm_identity`'s rows holding an object value: when the row
/// names a loader OBJECT and `metadata_pin_deferrable(value)` holds (the
/// caller folds THIS VM's loader-conditional licence for this cycle into it,
/// exactly as for `phases_late::gc_scan_classvalue_cache_roots`), the value is
/// pinned to the loader's address in `metadata_pin` and is NOT a root: the
/// marker traces it only if it marks the loader. Every other value (a
/// bootstrap-loader row, a cycle without the licence, a still-young value on
/// a Generational cycle that takes the MOVING young collector) is pushed to
/// `out`. (gc-common w36-d: on a Generational cycle certain to take the
/// non-moving young marker a young value is deferred too; that marker follows
/// `metadata_pin` from young owners and seeds it from old ones.)
///
/// The pins are recorded after the table lock drops, so the table lock never
/// nests over the `metadata_pin` lock. A pin is recorded before its root is
/// skipped (both happen inside this call), so no value is ever left
/// unreported.
pub fn gc_scan_clv_value_roots(
    vm_identity: usize,
    out: &mut Vec<ObjectRef>,
    metadata_pin_deferrable: &dyn Fn(usize) -> bool,
) {
    let mut pins: Vec<(usize, usize)> = Vec::new();
    {
        let t = table().lock();
        for row in t.rows.values() {
            if row.vm != vm_identity {
                continue;
            }
            let Value::Object(Some(value)) = row.value else {
                continue;
            };
            let addr = value.as_ptr() as usize;
            match row.loader {
                Some(loader) if metadata_pin_deferrable(addr) => {
                    pins.push((loader.as_ptr() as usize, addr));
                }
                _ => out.push(value),
            }
        }
    }
    for (loader, value) in pins {
        cratonvm_types::metadata_pin::add_metadata_pin(vm_identity, loader, value);
    }
}

/// Post-collection remap for [`gc_scan_clv_value_roots`]'s rows: re-address
/// each of `vm_identity`'s loaders and object values that the collection
/// moved. A dead loader is absent from the map and keeps its stale address
/// until [`forget_clv_keys`] drops its row in the same epilogue; nothing reads
/// a row in between.
pub fn gc_update_clv_value_refs(vm_identity: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let moved = |obj: ObjectRef| -> Option<ObjectRef> {
        pointer_map.get(&(obj.as_ptr() as usize)).map(|&new_addr| {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: the collector's forwarding address for a live object.
            unsafe { ObjectRef::from_raw(new_addr as *mut u8) }
        })
    };
    let mut t = table().lock();
    for row in t.rows.values_mut() {
        if row.vm != vm_identity {
            continue;
        }
        if let Some(new_loader) = row.loader.and_then(&moved) {
            row.loader = Some(new_loader);
        }
        if let Value::Object(Some(value)) = row.value {
            if let Some(new_value) = moved(value) {
                row.value = Value::Object(Some(new_value));
            }
        }
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
/// The bootstrap loader's key component is `None`, which no loader object's
/// key can equal (gc-common w16-b: it used to be a `usize::MAX` sentinel in
/// the same space as the object keys).
fn key_for_loader(ctx: &dyn NativeContext, cl: Option<&Value>, this: ObjectRef) -> Key {
    match cl {
        Some(Value::Object(Some(c))) => key_for(ctx, *c, this),
        _ => (None, clv_obj_key_for(ctx, this)),
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
    let v = table().lock().value_of(&key);
    Ok(Some(v.unwrap_or(Value::Object(None))))
}

/// `putIfAbsent(ClassLoader, V)V` — if no mapping exists for `(cl, this)`,
/// store `value` and return null. Otherwise return the existing value
/// (matching `Map.putIfAbsent` semantics).
///
/// gc-common w17-d: one critical section. Nothing is rooted any more, so the
/// check and the insert no longer straddle a `rooted_entry` call, and a
/// losing `putIfAbsent` leaves nothing behind (the w16-b note's last window).
/// A null `value` throws `NullPointerException`, as the JDK's
/// `ConcurrentHashMap.putIfAbsent` does; this used to store it, and the
/// "present" null row then made every later `putIfAbsent` answer null
/// without storing.
fn native_aclv_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    if matches!(value, Value::Object(None)) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    // The bootstrap loader is a key -- see [`native_aclv_get`].
    let key = key_for_loader(ctx, args.get(1), this);
    let vm = ctx.vm_identity();
    let mut t = table().lock();
    if let Some(existing) = t.value_of(&key) {
        return Ok(Some(existing));
    }
    if t.rows.len() >= MAX_ENTRIES {
        // Refuse to grow further; treat as "already present with null".
        return Ok(Some(Value::Object(None)));
    }
    t.insert(key, vm, loader_arg(args.get(1)), value);
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
///
/// gc-common w17-d, three corrections to the JDK's
/// `ConcurrentHashMap.remove(key, value)`:
/// * a null `value` answers `false` (the JDK's `value != null && ...`); this
///   answered `true` for a null row, and rows are never null now;
/// * the comparison is `value.equals(current)`, the JDK's receiver order;
/// * the delete is conditional on the row still being the one compared
///   ([`Row::seq`]). `equals` runs Java outside the lock, and a concurrent
///   `remove` + `putIfAbsent` used to have its NEW row deleted here on the
///   strength of a comparison against the old one. A changed row is compared
///   again; `value` is rooted across the `equals` calls, which can collect.
fn native_aclv_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some(Value::Object(Some(expected))) = args.get(2).copied() else {
        return Ok(Some(Value::Int(0)));
    };
    // The bootstrap loader is a key -- see [`native_aclv_get`].
    let key = key_for_loader(ctx, args.get(1), this);
    let mut scope = NativeHandleScope::new(ctx);
    let expected_h = scope.root(expected);
    // A row that changes under every comparison is a caller racing itself;
    // the bound only keeps this loop finite, and answering `false` then is a
    // legal linearization (the value was not present at the last look).
    for _ in 0..16 {
        let Some(row) = table().lock().rows.get(&key).copied() else {
            return Ok(Some(Value::Int(0)));
        };
        let Value::Object(Some(held)) = row.value else {
            return Ok(Some(Value::Int(0)));
        };
        let expected = scope.get(&expected_h);
        let matches = expected == held
            || matches!(
                scope.invoke_virtual(
                    expected,
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(held))]
                ),
                Ok(Some(Value::Int(1)))
            );
        let mut t = table().lock();
        match t.rows.get(&key) {
            Some(now) if now.seq == row.seq => {
                if !matches {
                    return Ok(Some(Value::Int(0)));
                }
                t.rows.remove(&key);
                return Ok(Some(Value::Int(1)));
            }
            // Removed meanwhile: nothing left to remove.
            None => return Ok(Some(Value::Int(0))),
            // Replaced meanwhile: compare against the new row.
            Some(_) => {}
        }
    }
    Ok(Some(Value::Int(0)))
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
            let v = table().lock().value_of(&key);
            return Ok(Some(v.unwrap_or(Value::Object(None))));
        }
    };
    // GC-stable keys (cce0079 follow-up): the invoke below can move
    // `cl`/`this`; identity-hash keys stay valid across the move, so the
    // post-invoke racing re-check finds the same entry (the former
    // raw-address key silently missed it after a GC).
    let key = key_for_loader(ctx, args.get(1), this);
    if let Some(existing) = table().lock().value_of(&key) {
        return Ok(Some(existing));
    }
    let vm = ctx.vm_identity();
    // gc-common w17-d: the row now records the loader OBJECT (the root scan
    // pins the value to it), and `apply` can move it: root it across the
    // call and store its post-call address.
    let mut scope = NativeHandleScope::new(ctx);
    let loader_h = loader_arg(args.get(1)).map(|cl| scope.root(cl));
    // Invoke mappingFunction.apply(cl, this).
    let result = scope.invoke_virtual(
        mapping_fn,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[cl_val, Value::Object(Some(this))],
    )?;
    let loader = loader_h.as_ref().map(|h| scope.get(h));
    let val = result.unwrap_or(Value::Object(None));
    // A mapping function that answers NULL is an error, not an instruction to
    // store null. `AbstractClassLoaderValue.Memoizer.get()` does
    // `Objects.requireNonNull(v)` on the mapping function's result, so the JDK
    // throws NPE and stores nothing; this stored null and returned it, so the
    // next `computeIfAbsent` saw a "present" mapping and never recomputed.
    // MEASURED by `apps/probes/JdkInternalSweep.java`.
    if matches!(val, Value::Object(None)) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    // Race: another caller may have raced ahead while `apply` ran. If so,
    // prefer the existing entry (matches CHM.computeIfAbsent semantics). The
    // losing `val` is referenced by nothing here, so it goes with its caller.
    let mut t = table().lock();
    if let Some(existing) = t.value_of(&key) {
        return Ok(Some(existing));
    }
    if t.rows.len() < MAX_ENTRIES {
        t.insert(key, vm, loader, val);
    }
    Ok(Some(val))
}

/// `removeAll(ClassLoader)V` -- drop every mapping this value holds for `cl`.
///
/// The fifth operation on the side table, and left to real bytecode until now
/// for the same reason `remove` was: no row asked. Real
/// `AbstractClassLoaderValue.removeAll` walks the REAL map, which these natives
/// never populate, so it removed nothing a caller could observe and the
/// mapping stayed readable through `get`.
///
/// **The JDK also removes the values of this value's DESCENDANTS** ("this
/// ClassLoaderValue or any of its descendants"), which are `ClassLoaderValue
/// .Sub` instances chained off it. This side table is keyed by
/// `(loader, this)` and models no parent/child relationship between values, so
/// the descendant half is NOT reproduced -- a `Sub`'s own mappings survive a
/// `removeAll` on its parent. Stated rather than silently approximated: the
/// direct half is what every measured caller uses, and inventing a parent link
/// here would be a guess about a structure this table does not have.
fn native_aclv_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let key = key_for_loader(ctx, args.get(1), this);
    table().lock().rows.remove(&key);
    Ok(None)
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
    // ...and the fifth. `removeAll` was recorded as a residual beside `remove`
    // because no probe row asked it; the row exists now.
    registry.register(
        class,
        "removeAll",
        "(Ljava/lang/ClassLoader;)V",
        native_aclv_remove_all,
    );
    registry.set_category(__prev_cat);
}

/// gc-common w16-b (`common-w15b-three-more-identity-key-registry-copies`).
/// The mock's identity hash is the address truncated to `i32`, so addresses
/// 4 GiB apart share a hash (as two VMs' objects do). Fake addresses end in 0
/// and are never dereferenced. Each test has its own VM identities: the
/// lock-key registry and this table are process-wide.
#[cfg(test)]
mod w16b_clv_key_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: a key / stored value only; nothing on these paths
        // dereferences it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
        let c = crate::test_utils::mock_ctx();
        c.set_vm_identity(vm);
        c
    }

    fn get(
        ctx: &mut crate::test_utils::MockNativeContext,
        clv: ObjectRef,
        cl: Option<ObjectRef>,
    ) -> Option<ObjectRef> {
        match native_aclv_get(ctx, &[Value::Object(Some(clv)), Value::Object(cl)]) {
            Ok(Some(Value::Object(o))) => o,
            _ => panic!("AbstractClassLoaderValue.get answered a non-object"),
        }
    }

    /// `putIfAbsent`; answers the value it found already present, if any.
    fn put(
        ctx: &mut crate::test_utils::MockNativeContext,
        clv: ObjectRef,
        cl: Option<ObjectRef>,
        value: ObjectRef,
    ) -> Option<ObjectRef> {
        match native_aclv_put_if_absent(
            ctx,
            &[
                Value::Object(Some(clv)),
                Value::Object(cl),
                Value::Object(Some(value)),
            ],
        ) {
            Ok(Some(Value::Object(o))) => o,
            _ => panic!("AbstractClassLoaderValue.putIfAbsent answered a non-object"),
        }
    }

    /// The page's retire test: VM B's loader and CLV carry the identity hashes
    /// of VM A's pair. B used to be handed A's key and read A's row -- a raw
    /// address in A's heap. Now each VM sees only its own rows, for a real
    /// loader and for the bootstrap (null) loader alike.
    #[test]
    fn two_vms_with_one_identity_hash_pair_keep_distinct_rows() {
        const VM_A: usize = 0xB160_0001;
        const VM_B: usize = 0xB160_0002;
        let mut a = ctx_for(VM_A);
        let mut b = ctx_for(VM_B);
        let (cl_a, clv_a, val_a) = (obj(0x1_B16A_0010), obj(0x1_B16A_0020), obj(0x1_B16A_0030));
        let (cl_b, clv_b, val_b) = (obj(0x2_B16A_0010), obj(0x2_B16A_0020), obj(0x2_B16A_0030));
        assert_eq!(a.identity_hash_code(cl_a), b.identity_hash_code(cl_b));
        assert_eq!(a.identity_hash_code(clv_a), b.identity_hash_code(clv_b));

        assert_eq!(put(&mut a, clv_a, Some(cl_a), val_a), None);
        assert_eq!(put(&mut a, clv_a, None, val_a), None);
        assert_eq!(get(&mut b, clv_b, Some(cl_b)), None, "B read A's row");
        assert_eq!(get(&mut b, clv_b, None), None, "B read A's bootstrap row");
        assert_eq!(put(&mut b, clv_b, Some(cl_b), val_b), None);
        assert_eq!(get(&mut a, clv_a, Some(cl_a)), Some(val_a));
        assert_eq!(get(&mut b, clv_b, Some(cl_b)), Some(val_b));

        // Teardown of A takes A's rows, nothing of B's.
        crate::forget_vm_lock_keys(VM_A);
        assert_eq!(get(&mut b, clv_b, Some(cl_b)), Some(val_b));
        assert_eq!(get(&mut a, clv_a, Some(cl_a)), None);
        assert_eq!(get(&mut a, clv_a, None), None);
        crate::forget_vm_lock_keys(VM_A);
        crate::forget_vm_lock_keys(VM_B);
    }

    /// A row goes when EITHER of its two objects dies, and a later object
    /// with the dead one's identity hash inherits nothing (the old registry
    /// rebound the dead slot to it).
    #[test]
    fn a_row_goes_when_its_loader_or_its_value_key_dies() {
        const VM: usize = 0xB160_0003;
        let mut c = ctx_for(VM);
        let (cl, clv, clv2, val) = (
            obj(0x1_B16B_0010),
            obj(0x1_B16B_0020),
            obj(0x1_B16B_0040),
            obj(0x1_B16B_0030),
        );
        put(&mut c, clv, Some(cl), val);
        put(&mut c, clv2, Some(cl), val);
        put(&mut c, clv2, None, val);
        // The loader dies: both of its rows go, the bootstrap row stays.
        let loader = cl.as_ptr() as usize;
        crate::gc_sweep_lock_keys(VM, &|a| a != loader);
        assert_eq!(get(&mut c, clv, Some(cl)), None);
        assert_eq!(get(&mut c, clv2, Some(cl)), None);
        assert_eq!(get(&mut c, clv2, None), Some(val));
        // A new loader with the dead one's hash starts empty.
        let reborn = obj(0x3_B16B_0010);
        assert_eq!(get(&mut c, clv2, Some(reborn)), None);
        // The CLV dies: its bootstrap row goes too, and a new CLV with its
        // hash starts empty.
        let dead_clv = clv2.as_ptr() as usize;
        crate::gc_sweep_lock_keys(VM, &|a| a != dead_clv);
        assert_eq!(get(&mut c, obj(0x3_B16B_0040), None), None);
        crate::forget_vm_lock_keys(VM);
    }

    /// Both objects moved by a collection keep their row: the epilogue
    /// re-addresses their slots, and a later sweep judges them there.
    #[test]
    fn a_moved_loader_and_value_keep_their_row() {
        const VM: usize = 0xB160_0004;
        let mut c = ctx_for(VM);
        let (cl_old, cl_new) = (0x1_B16C_0010usize, 0x3_B16C_0010usize);
        let (clv_old, clv_new) = (0x1_B16C_0020usize, 0x3_B16C_0020usize);
        let val = obj(0x1_B16C_0030);
        put(&mut c, obj(clv_old), Some(obj(cl_old)), val);
        let mut moved = cratonvm_types::PointerMap::default();
        moved.insert(cl_old, cl_new);
        moved.insert(clv_old, clv_new);
        crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == cl_old || a == clv_old, &moved);
        assert_eq!(get(&mut c, obj(clv_new), Some(obj(cl_new))), Some(val));
        crate::gc_sweep_lock_keys(VM, &|a| a == cl_new || a == clv_new);
        assert_eq!(get(&mut c, obj(clv_new), Some(obj(cl_new))), Some(val));
        crate::forget_vm_lock_keys(VM);
    }
}

/// gc-common w17-d (`common-w16b-clv-values-are-permanent-roots-that-pin-their-loader`).
/// Same conventions as `w16b_clv_key_tests`: fake addresses end in 0 and are
/// never dereferenced, and each test has its own VM identity, because the
/// lock-key registry, this table and `metadata_pin` are process-wide. Every
/// test drops its own `metadata_pin` rows and lock keys on the way out.
#[cfg(test)]
mod w17d_clv_value_lifetime_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: a key / stored value only; nothing on these paths
        // dereferences it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn addr(o: ObjectRef) -> usize {
        o.as_ptr() as usize
    }

    fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
        let c = crate::test_utils::mock_ctx();
        c.set_vm_identity(vm);
        c
    }

    /// Drops the test's lock keys (and so its rows) and its `metadata_pin`
    /// rows even when an assertion fails first.
    struct Cleanup(usize);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(self.0);
            cratonvm_types::metadata_pin::forget_vm_metadata_pins(self.0);
        }
    }

    fn call(
        f: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
        ctx: &mut crate::test_utils::MockNativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        f(ctx, args)
    }

    fn put(
        ctx: &mut crate::test_utils::MockNativeContext,
        clv: ObjectRef,
        cl: Option<ObjectRef>,
        value: ObjectRef,
    ) -> Option<ObjectRef> {
        match call(
            native_aclv_put_if_absent,
            ctx,
            &[
                Value::Object(Some(clv)),
                Value::Object(cl),
                Value::Object(Some(value)),
            ],
        ) {
            Ok(Some(Value::Object(o))) => o,
            _ => panic!("AbstractClassLoaderValue.putIfAbsent answered a non-object"),
        }
    }

    fn get(
        ctx: &mut crate::test_utils::MockNativeContext,
        clv: ObjectRef,
        cl: Option<ObjectRef>,
    ) -> Option<ObjectRef> {
        match call(native_aclv_get, ctx, &[Value::Object(Some(clv)), Value::Object(cl)]) {
            Ok(Some(Value::Object(o))) => o,
            _ => panic!("AbstractClassLoaderValue.get answered a non-object"),
        }
    }

    fn remove(
        ctx: &mut crate::test_utils::MockNativeContext,
        clv: ObjectRef,
        cl: Option<ObjectRef>,
        value: Option<ObjectRef>,
    ) -> bool {
        match call(
            native_aclv_remove,
            ctx,
            &[
                Value::Object(Some(clv)),
                Value::Object(cl),
                Value::Object(value),
            ],
        ) {
            Ok(Some(Value::Int(z))) => z != 0,
            _ => panic!("AbstractClassLoaderValue.remove answered a non-boolean"),
        }
    }

    /// The roots one scan of `vm` reports, with the licence (the deferral
    /// predicate) fixed to `deferrable`. Pins go to `metadata_pin`.
    fn scan(vm: usize, deferrable: bool) -> Vec<usize> {
        let mut out = Vec::new();
        gc_scan_clv_value_roots(vm, &mut out, &|_| deferrable);
        out.into_iter().map(addr).collect()
    }

    fn pinned_to(loader: ObjectRef) -> Vec<usize> {
        cratonvm_types::metadata_pin::roots_for_loader(addr(loader)).unwrap_or_default()
    }

    /// The page's defect: the value of a loader's row was a permanent root,
    /// so a value that reaches its loader kept the loader alive. Under the
    /// loader-conditional licence it is now pinned to the loader and not
    /// rooted; without the licence it is rooted, as every loader-owned value
    /// is; a bootstrap row is always rooted (that loader never dies).
    #[test]
    fn a_loader_rows_value_is_pinned_to_its_loader_not_rooted() {
        const VM: usize = 0xD170_0001;
        let _cleanup = Cleanup(VM);
        let mut c = ctx_for(VM);
        let (cl, clv, val, boot_val) = (
            obj(0x1_D17A_0010),
            obj(0x1_D17A_0020),
            obj(0x1_D17A_0030),
            obj(0x1_D17A_0040),
        );
        assert_eq!(put(&mut c, clv, Some(cl), val), None);
        assert_eq!(put(&mut c, clv, None, boot_val), None);

        assert_eq!(scan(VM, true), vec![addr(boot_val)], "licensed: only the bootstrap row roots");
        assert_eq!(pinned_to(cl), vec![addr(val)], "licensed: the value hangs off its loader");

        let mut unlicensed = scan(VM, false);
        unlicensed.sort_unstable();
        let mut both = vec![addr(val), addr(boot_val)];
        both.sort_unstable();
        assert_eq!(unlicensed, both, "no licence: every value is a root");

        // Another VM's scan reports none of this VM's rows.
        assert!(scan(0xD170_00FF, false).is_empty());
    }

    /// The row is the value's only native reference: once the loader dies and
    /// the epilogue drops the row, the value is reported by nobody. (The var-
    /// handle root it used to be had no release, so it stayed until teardown.)
    #[test]
    fn a_dead_loader_takes_its_value_out_of_every_scan() {
        const VM: usize = 0xD170_0002;
        let _cleanup = Cleanup(VM);
        let mut c = ctx_for(VM);
        let (cl, clv, val) = (obj(0x1_D17B_0010), obj(0x1_D17B_0020), obj(0x1_D17B_0030));
        put(&mut c, clv, Some(cl), val);
        assert_eq!(scan(VM, false), vec![addr(val)]);
        let loader = addr(cl);
        crate::gc_sweep_lock_keys(VM, &|a| a != loader);
        assert!(scan(VM, false).is_empty(), "a dropped row's value is still reported");
        assert!(scan(VM, true).is_empty());
    }

    /// A losing `putIfAbsent` and a null value leave nothing behind.
    #[test]
    fn a_losing_or_null_put_if_absent_stores_nothing() {
        const VM: usize = 0xD170_0003;
        let _cleanup = Cleanup(VM);
        let mut c = ctx_for(VM);
        let (cl, clv, winner, loser) = (
            obj(0x1_D17C_0010),
            obj(0x1_D17C_0020),
            obj(0x1_D17C_0030),
            obj(0x1_D17C_0040),
        );
        assert_eq!(put(&mut c, clv, Some(cl), winner), None);
        assert_eq!(put(&mut c, clv, Some(cl), loser), Some(winner));
        assert_eq!(scan(VM, false), vec![addr(winner)], "the loser is reported");

        // A null value is the JDK map's NPE, and stores no "present" null.
        let clv2 = obj(0x1_D17C_0050);
        let npe = call(
            native_aclv_put_if_absent,
            &mut c,
            &[Value::Object(Some(clv2)), Value::Object(Some(cl)), Value::Object(None)],
        );
        assert!(npe.is_err(), "putIfAbsent(cl, null) must throw");
        assert_eq!(put(&mut c, clv2, Some(cl), winner), None, "a null row was stored");
    }

    /// The remap re-addresses the loader and the value, so the value is read
    /// at its new address and pinned to the loader's new address.
    #[test]
    fn a_moved_loader_and_value_are_re_addressed_in_the_row() {
        const VM: usize = 0xD170_0004;
        let _cleanup = Cleanup(VM);
        let mut c = ctx_for(VM);
        let (cl_old, cl_new) = (obj(0x1_D17D_0010), obj(0x3_D17D_0010));
        let (clv_old, clv_new) = (obj(0x1_D17D_0020), obj(0x3_D17D_0020));
        let (val_old, val_new) = (obj(0x1_D17D_0030), obj(0x3_D17D_0030));
        put(&mut c, clv_old, Some(cl_old), val_old);
        let mut moved = cratonvm_types::PointerMap::default();
        moved.insert(addr(cl_old), addr(cl_new));
        moved.insert(addr(clv_old), addr(clv_new));
        moved.insert(addr(val_old), addr(val_new));
        crate::gc_sweep_and_remap_lock_keys(
            VM,
            &|a| a == addr(cl_old) || a == addr(clv_old),
            &moved,
        );
        gc_update_clv_value_refs(VM, &moved);
        // Another VM's remap leaves this VM's row alone.
        gc_update_clv_value_refs(0xD170_00FF, &moved);
        assert_eq!(get(&mut c, clv_new, Some(cl_new)), Some(val_new));
        assert!(scan(VM, true).is_empty());
        assert_eq!(pinned_to(cl_new), vec![addr(val_new)]);
        assert!(pinned_to(cl_old).is_empty(), "pinned to the loader's old address");
    }

    const CIA_CL_OLD: usize = 0x1_D17E_0010;
    const CIA_CL_NEW: usize = 0x3_D17E_0010;
    const CIA_VAL: usize = 0x1_D17E_0030;

    /// `apply` "moves" the loader (the mock's pin slot is re-addressed, as a
    /// moving collection inside the call would) and answers `CIA_VAL`.
    fn apply_moves_the_loader(
        ctx: &mut crate::test_utils::MockNativeContext,
        _receiver: ObjectRef,
        method: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method != "apply" {
            return None;
        }
        ctx.remap_native_pin_addr_for_test(CIA_CL_OLD, CIA_CL_NEW);
        Some(Ok(Some(Value::Object(Some(obj(CIA_VAL))))))
    }

    /// `computeIfAbsent` stores the loader's POST-`apply` address, so the
    /// value is pinned to where the loader is, not where it was.
    #[test]
    fn compute_if_absent_records_the_loader_where_apply_left_it() {
        const VM: usize = 0xD170_0005;
        let _cleanup = Cleanup(VM);
        let mut c = ctx_for(VM);
        c.set_invoke_virtual_hook(apply_moves_the_loader);
        let (clv, func) = (obj(0x1_D17E_0020), obj(0x1_D17E_0040));
        let pins_before = c.native_pin_count_for_test();
        let got = call(
            native_aclv_compute_if_absent,
            &mut c,
            &[
                Value::Object(Some(clv)),
                Value::Object(Some(obj(CIA_CL_OLD))),
                Value::Object(Some(func)),
            ],
        );
        assert_eq!(got.ok().flatten(), Some(Value::Object(Some(obj(CIA_VAL)))));
        assert_eq!(c.native_pin_count_for_test(), pins_before, "the handle scope leaked");
        assert!(scan(VM, true).is_empty());
        assert_eq!(pinned_to(obj(CIA_CL_NEW)), vec![CIA_VAL]);
        assert!(pinned_to(obj(CIA_CL_OLD)).is_empty());
    }

    /// `remove(cl, v)`: identity match removes; a null `v` and a missing row
    /// answer false, as the JDK map does.
    #[test]
    fn remove_follows_the_jdk_maps_answers() {
        const VM: usize = 0xD170_0006;
        let _cleanup = Cleanup(VM);
        let mut c = ctx_for(VM);
        let (cl, clv, val, other) = (
            obj(0x1_D17F_0010),
            obj(0x1_D17F_0020),
            obj(0x1_D17F_0030),
            obj(0x1_D17F_0040),
        );
        put(&mut c, clv, Some(cl), val);
        assert!(!remove(&mut c, clv, Some(cl), None), "remove(cl, null) must be false");
        // The mock's `equals` answers nothing, i.e. "not equal".
        assert!(!remove(&mut c, clv, Some(cl), Some(other)));
        assert_eq!(get(&mut c, clv, Some(cl)), Some(val));
        assert!(remove(&mut c, clv, Some(cl), Some(val)));
        assert_eq!(get(&mut c, clv, Some(cl)), None);
        assert!(!remove(&mut c, clv, Some(cl), Some(val)), "nothing left to remove");
        assert!(scan(VM, false).is_empty());
    }
}
