// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HIB-CV-24 (Manifestation B) — class-loader liveness pinning registry.
//!
//! In HotSpot a `java.lang.Class` strongly references its defining
//! `ClassLoader`, so a *live instance* of a class keeps that class's loader
//! alive; the loader is only unloaded once none of its classes have live
//! instances and nothing else references it. CratonVM objects carry only a
//! `class_id` (an integer) and the defining loader lives in a side-table
//! (`native-builtins::classloader::defining_loader_store`), so a live instance
//! does *not* by itself keep its loader alive.
//!
//! Once the defining-loader side-table stops unconditionally GC-rooting every
//! user loader (so genuinely-unreachable loaders can be collected —
//! `CRATONVM_LOADER_UNLOAD`), that missing instance→loader edge must be
//! reinstated during marking, or a loader whose instance is intentionally
//! leaked (Hibernate's `ClassLoaderLeaksUtilityTest.testClassLoaderLeaksDetected`,
//! which stows an instance in a `ThreadLocal`) would be wrongly reclaimed.
//!
//! This is a process-global `class_id -> defining ClassLoader heap address`
//! registry that the GC marker consults: when it keeps an object alive it also
//! keeps that object's defining loader alive. It lives in `cratonvm-types` so
//! both the GC (`gen_heap` marker) and `native-builtins` (the side-table owner)
//! can reach it without a new crate dependency — mirroring the compact
//! reference-field layout registry already kept here.
//!
//! native-builtins is the single writer (it owns the authoritative side-table
//! that backs `getClassLoader()` identity); it keeps this registry in sync on
//! registration and post-GC reconciliation. Crucially the registry is NOT itself
//! a GC root — it only *extends* reachability from already-live objects — so a
//! loader with no live instances (and no other reference) is still collected.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

/// `class_id -> current defining-ClassLoader heap address`. Only user-defined
/// loaders are recorded (built-in/app/bootstrap classes are absent), so the map
/// stays small and a marker lookup is `None` for the overwhelmingly common case.
fn store() -> &'static RwLock<FxHashMap<u32, usize>> {
    static INSTANCE: OnceLock<RwLock<FxHashMap<u32, usize>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// `CRATONVM_LOADER_UNLOAD` gate (default ON). MUST mirror the native-builtins
/// `loader_unload_enabled()` gate so instance→loader pinning is active exactly
/// when the side-table stops rooting loaders. When off, loaders are
/// unconditionally rooted and this pinning is unnecessary (and skipped).
pub fn loader_pinning_enabled() -> bool {
    static GATE: OnceLock<bool> = OnceLock::new();
    *GATE.get_or_init(|| {
        crate::flags::runtime_var("CRATONVM_LOADER_UNLOAD")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// Fast "is the registry non-empty" check so the marker can skip the per-object
/// lookup entirely when no user loader has ever defined a class (the common
/// case — most runs never touch a custom `ClassLoader`).
static NON_EMPTY: AtomicBool = AtomicBool::new(false);

/// Record / update the defining-loader heap address for `class_id`.
pub fn set_loader_pin(class_id: u32, loader_addr: usize) {
    store().write().insert(class_id, loader_addr);
    NON_EMPTY.store(true, Ordering::Relaxed);
}

/// Remove the instance-to-loader edge for a class whose defining loader and
/// metadata have completed unloading.
pub fn remove_loader_pin(class_id: u32) {
    let mut pins = store().write();
    pins.remove(&class_id);
    if pins.is_empty() {
        NON_EMPTY.store(false, Ordering::Relaxed);
    }
}

/// The defining-loader heap address for `class_id`, if it was defined by a user
/// loader. Returns `None` for built-in/app/bootstrap classes. Hot path: a single
/// relaxed atomic load short-circuits when the registry is empty.
#[inline]
pub fn loader_pin_addr(class_id: u32) -> Option<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    store().read().get(&class_id).copied()
}

/// Replace the entire registry from the authoritative side-table snapshot
/// (`[(class_id, loader_addr)]`). Called by the post-GC reconciliation in
/// native-builtins after it has remapped/pruned the side-table, so the marker
/// sees current addresses on the next collection.
pub fn replace_loader_pins(entries: &[(u32, usize)]) {
    let mut g = store().write();
    g.clear();
    for &(cid, addr) in entries {
        g.insert(cid, addr);
    }
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

/// Drop all entries (new-VM reset).
pub fn clear_loader_pins() {
    store().write().clear();
    NON_EMPTY.store(false, Ordering::Relaxed);
}
