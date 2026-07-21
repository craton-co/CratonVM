// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.ClassValue<T>` — real lazily-computed, cached per-(instance,
//! `Class`) values (Java 7+).
//!
//! Previously `ClassValue.get(Class)` was a stub that unconditionally
//! returned `null` (never invoking the subclass's `computeValue(Class)`).
//! This is a general JDK mechanism — not Groovy-specific — used by (among
//! others) Apache Groovy's `GroovyClassValueJava7`, which backs
//! `ClassInfo`'s global per-`Class` registry
//! (`GroovyClassValueFactory.createGroovyClassValue` picks the
//! `java.lang.ClassValue`-backed impl whenever `groovy.use.classvalue`
//! defaults to `true`, which it does). With `get()` always null,
//! `ClassInfo.getClassInfo(cls)` always returned `null`, and
//! `ReflectionCache.getCachedClass` (`.getCachedClass()` on that null)
//! NPE'd deterministically the first time any Groovy code touched
//! `MetaClassImpl.setUpProperties` — see
//! `docs/known-issues/springboot/core-spring-boot-test-config-data-and-classpath-scan-cluster.md`,
//! Cluster C "Residual 5".
//!
//! Real semantics: `get(type)` lazily computes (via a virtual dispatch to
//! `computeValue(Class)`, so `GroovyClassValueJava7`'s override — or any
//! other `ClassValue` subclass's — runs) and caches the result per
//! `(ClassValue instance, Class)` pair; `remove(type)` evicts the cached
//! entry so the next `get` recomputes. `null` is itself a valid, cacheable
//! computed value (distinct from "not yet computed").
//!
//! Cache keys use a GC-stable identity — the raw heap address of a
//! `ClassValue` instance (or of the `Class` mirror argument) is not stable
//! under a moving GC; the algorithm here mirrors
//! `properties_sidetable::key_for` / `native-collections`' `widened_obj_key`
//! / `lib.rs`'s `gc_stable_lock_key`. Cached non-null values are held via
//! [`NativeContext::add_global_root`] (a persistent, GC-remapped root; the
//! same mechanism backing JNI global refs) so the returned `ObjectRef`
//! stays valid indefinitely and survives relocation.

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;

use cratonvm_native_api::registry::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

/// Cap on distinct `(ClassValue instance, Class)` pairs tracked at once.
/// Real applications touch at most a few thousand such pairs (one per
/// JDK/library `ClassValue` cache times the number of distinct `Class`es it
/// has ever seen); this is a generous ceiling that only exists to bound a
/// pathological caller, mirroring the caps in `properties_sidetable`.
const MAX_TOTAL_ENTRIES: usize = 200_000;

struct ObjKeyEntry {
    last_ptr: usize,
    generation: u32,
}

fn obj_key_registry() -> &'static Mutex<FxHashMap<u32, Vec<ObjKeyEntry>>> {
    static R: OnceLock<Mutex<FxHashMap<u32, Vec<ObjKeyEntry>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(FxHashMap::default()))
}

#[inline]
fn pack_obj_key(hash: u32, generation: u32) -> usize {
    ((hash as usize) << 32) | (generation as usize)
}

/// GC-stable identity key for an arbitrary heap object. See the module doc
/// comment; algorithm identical to `properties_sidetable::key_for`.
fn key_for(ctx: &mut dyn NativeContext, obj: ObjectRef) -> usize {
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let mut reg = obj_key_registry().lock();
    let slots = reg.entry(hash).or_default();
    if let Some(slot) = slots.iter().find(|s| s.last_ptr == ptr) {
        return pack_obj_key(hash, slot.generation);
    }
    if hash != 0 && slots.len() == 1 {
        slots[0].last_ptr = ptr;
        return pack_obj_key(hash, slots[0].generation);
    }
    let generation = slots.len() as u32;
    slots.push(ObjKeyEntry {
        last_ptr: ptr,
        generation,
    });
    pack_obj_key(hash, generation)
}

/// Cache entry: `None` means "computed, result was null"; `Some(handle)` is
/// a global-root handle (see `NativeContext::add_global_root`) for the
/// cached non-null value.
fn table() -> &'static Mutex<FxHashMap<(usize, usize), Option<usize>>> {
    static T: OnceLock<Mutex<FxHashMap<(usize, usize), Option<usize>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn trace_enabled() -> bool {
    std::env::var_os("CRATONVM_TRACE_CLASSVALUE").is_some()
}

/// `ClassValue.get(Class type)` — real lazily-computed, cached semantics.
pub fn classvalue_get(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    type_obj: ObjectRef,
) -> MethodCallResult {
    let cv_key = key_for(ctx, this);
    let type_key = key_for(ctx, type_obj);

    if let Some(entry) = table().lock().get(&(cv_key, type_key)) {
        if trace_enabled() {
            eprintln!("[classvalue] HIT cv={cv_key:x} type={type_key:x} cached={entry:?}");
        }
        return Ok(Some(match entry {
            Some(handle) => Value::Object(ctx.resolve_global_root(*handle)),
            None => Value::Object(None),
        }));
    }

    if trace_enabled() {
        eprintln!("[classvalue] MISS cv={cv_key:x} type={type_key:x} -> invoking computeValue");
    }
    // Not cached yet: dispatch virtually so the concrete subclass's
    // `computeValue` override runs (e.g. `GroovyClassValueJava7`'s, which
    // delegates to the `ComputeValue` lambda it was built with).
    let result = ctx.invoke_virtual(
        this,
        "computeValue",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        &[Value::Object(Some(type_obj))],
    )?;
    let value_obj = match result {
        Some(Value::Object(o)) => o,
        _ => None,
    };
    if trace_enabled() {
        eprintln!("[classvalue] computeValue -> {value_obj:?}");
    }

    let mut table = table().lock();
    if table.len() < MAX_TOTAL_ENTRIES {
        let stored = value_obj.map(|o| ctx.add_global_root(o));
        table.insert((cv_key, type_key), stored);
    }
    Ok(Some(Value::Object(value_obj)))
}

/// `ClassValue.remove(Class type)` — evicts the cached entry (if any) so a
/// later `get` recomputes it.
pub fn classvalue_remove(ctx: &mut dyn NativeContext, this: ObjectRef, type_obj: ObjectRef) {
    let cv_key = key_for(ctx, this);
    let type_key = key_for(ctx, type_obj);
    let removed = table().lock().remove(&(cv_key, type_key));
    if let Some(Some(handle)) = removed {
        ctx.remove_global_root(handle);
    }
}

/// Registers `java.lang.ClassValue#get`/`#remove`.
///
/// A standalone entry point (rather than folded into one of the giant
/// `register_p*_misc`-style grab-bag functions) so it can be called
/// explicitly from BOTH the synthetic-JDK bootstrap
/// (`register_synthetic_overrides` → `register_phase67_natives` →
/// `register_p67_misc`) and the real-JDK-mode `cratonvm-cli` bootstrap
/// (`vm/src/vm/vm_init.rs`'s `VmContext::new`) — the latter does NOT call
/// `register_synthetic_overrides` at all (real-JDK mode hand-picks a curated
/// subset of registration functions instead; synthetic overrides assume
/// synthetic field layouts and would corrupt real JDK objects), so this
/// registration must be reachable independently of that giant function or it
/// silently never runs under real-JDK mode — exactly how the original
/// always-null stub went unnoticed: it lived inside `register_p67_misc`,
/// which is genuinely dead code for the default (real-JDK) build.
///
/// `NativeKind::Bridge`, not `SyntheticStub`: this is a correct, real
/// implementation of a mechanism CratonVM cannot run as pure bytecode
/// (`ClassValue`'s real algorithm depends on CASing a hidden field on
/// `java.lang.Class` via `jdk.internal.misc.Unsafe`, which is not faithfully
/// reproducible against CratonVM's `Class` mirrors), not a placeholder.
pub fn register_classvalue_natives(r: &mut NativeMethodRegistry) {
    let cv = "java/lang/ClassValue";
    r.with_category(NativeKind::Bridge, |r| {
        r.register(
            cv,
            "get",
            "(Ljava/lang/Class;)Ljava/lang/Object;",
            |ctx, args| {
                let this = crate::obj_arg(args, 0)?;
                let type_obj = crate::obj_arg(args, 1)?;
                classvalue_get(ctx, this, type_obj)
            },
        );
        r.register(cv, "remove", "(Ljava/lang/Class;)V", |ctx, args| {
            let this = crate::obj_arg(args, 0)?;
            let type_obj = crate::obj_arg(args, 1)?;
            classvalue_remove(ctx, this, type_obj);
            Ok(None)
        });
    });
}
