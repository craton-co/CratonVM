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

/// `class_id -> (owning VM, current defining-ClassLoader heap address)`. Only
/// user-defined loaders are recorded (built-in/app/bootstrap classes are
/// absent), so the map stays small and a marker lookup is `None` for the
/// overwhelmingly common case.
///
/// # Why the value carries a VM and the key does not
///
/// The authoritative side-table this mirrors
/// (`native-builtins::classloader::defining_loader_store`) is keyed by
/// `(vm_identity, class_id)`, and every writer here already holds that
/// `vm_identity`. It is recorded so [`forget_vm_loader_pins`] can drop exactly
/// one VM's rows at teardown — which is what a second, concurrently-live VM
/// needs, and what the old blanket `clear()` at VM *creation* destroyed.
///
/// The **key** stays the bare `class_id` because the GC marker that reads this
/// registry has no VM identity in hand, and threading one through every
/// collector (`gen_heap`, `g1`, `zgc`) to disambiguate would buy nothing: class
/// ids restart at 0 per VM, so two live VMs can collide on one id, and the
/// collision resolves to whichever VM wrote last. That is an
/// *over*-approximation — the marker is handed an address in another VM's heap,
/// which its own bounds check rejects — and over-approximation is the safe
/// direction for a liveness pin. The old behaviour was the unsafe direction: a
/// blanket wipe left the marker with *no* pin for a class whose loader was
/// still live, which is a missing root.
fn store() -> &'static RwLock<FxHashMap<u32, (usize, usize)>> {
    static INSTANCE: OnceLock<RwLock<FxHashMap<u32, (usize, usize)>>> = OnceLock::new();
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

/// Record / update the defining-loader heap address for `class_id`, owned by
/// `vm`.
pub fn set_loader_pin(vm: usize, class_id: u32, loader_addr: usize) {
    store().write().insert(class_id, (vm, loader_addr));
    NON_EMPTY.store(true, Ordering::Relaxed);
}

/// Remove the instance-to-loader edge for a class whose defining loader and
/// metadata have completed unloading.
///
/// A row another VM has since overwritten is left alone: the id is that VM's
/// now, and removing it would strip a live loader's pin.
pub fn remove_loader_pin(vm: usize, class_id: u32) {
    let mut pins = store().write();
    if pins.get(&class_id).is_some_and(|&(owner, _)| owner == vm) {
        pins.remove(&class_id);
    }
    if pins.is_empty() {
        NON_EMPTY.store(false, Ordering::Relaxed);
    }
}

/// Drop every row `vm` owns. Called when that VM tears down, in place of the
/// blanket wipe a *new* VM used to perform — which could only ever destroy a
/// concurrently-live VM's rows, never its own (a fresh VM has none).
pub fn forget_vm_loader_pins(vm: usize) {
    let mut pins = store().write();
    pins.retain(|_, &mut (owner, _)| owner != vm);
    NON_EMPTY.store(!pins.is_empty(), Ordering::Relaxed);
}

/// The defining-loader heap address for `class_id`, if it was defined by a user
/// loader. Returns `None` for built-in/app/bootstrap classes. Hot path: a single
/// relaxed atomic load short-circuits when the registry is empty.
#[inline]
pub fn loader_pin_addr(class_id: u32) -> Option<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    store().read().get(&class_id).map(|&(_, addr)| addr)
}

/// Every pinned loader address, for a marker that cannot ask per class.
///
/// # Why a wholesale reader exists
///
/// [`loader_pin_addr`] answers "which loader pins THIS class", which is the
/// question a marker asks while visiting the class's objects. A generational
/// young cycle never visits an old object at all, so it never asks -- and a
/// loader whose only reference is one of its own loaded classes would be swept,
/// taking every mirror and method structure with it. A young cycle therefore
/// roots the whole registry once instead, which is over-approximate (a loader
/// pinned for a class that is itself dead survives one more cycle) in the safe
/// direction.
///
/// Mirrors [`crate::metadata_pin::snapshot`], whose own note makes the same
/// argument about the per-object variant being a shared-cache-line hot spot.
/// The class ids that HAVE a pinned loader.
///
/// For a marker that wants to answer "does this class have one?" without a
/// lock and a hash per marked object: the answer is a property of the class,
/// so a caller can snapshot these once per collection and index a bitmap.
pub fn pinned_class_ids() -> Vec<u32> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return Vec::new();
    }
    store().read().keys().copied().collect()
}

pub fn all_pinned_loaders() -> Vec<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return Vec::new();
    }
    store().read().values().map(|&(_, addr)| addr).collect()
}

/// Replace **`vm`'s** rows from the authoritative side-table snapshot
/// (`[(class_id, loader_addr)]`). Called by the post-GC reconciliation in
/// native-builtins after it has remapped/pruned the side-table, so the marker
/// sees current addresses on the next collection.
///
/// Rows owned by another VM survive: this is one VM's collection, and its
/// pointer map says nothing about another heap's addresses.
pub fn replace_loader_pins(vm: usize, entries: &[(u32, usize)]) {
    let mut g = store().write();
    g.retain(|_, &mut (owner, _)| owner != vm);
    for &(cid, addr) in entries {
        g.insert(cid, (vm, addr));
    }
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

// There is deliberately no `clear_loader_pins()`: a blanket wipe cannot
// distinguish this VM's rows from a concurrently-live VM's, and the only
// caller it ever had ran at VM *creation*, where the sole rows in the table
// belong to somebody else. Use [`forget_vm_loader_pins`].

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_drops_only_the_torn_down_vms_rows() {
        const VM_A: usize = 0xA00;
        const VM_B: usize = 0xB00;
        forget_vm_loader_pins(VM_A);
        forget_vm_loader_pins(VM_B);

        set_loader_pin(VM_A, 1, 0x1000);
        set_loader_pin(VM_B, 2, 0x2000);
        assert_eq!(loader_pin_addr(1), Some(0x1000));
        assert_eq!(loader_pin_addr(2), Some(0x2000));

        // The wipe this replaced ran here, at VM creation, and took both rows.
        forget_vm_loader_pins(VM_A);
        assert_eq!(loader_pin_addr(1), None);
        assert_eq!(
            loader_pin_addr(2),
            Some(0x2000),
            "a concurrently-live VM keeps its pin"
        );

        // One VM's post-GC re-sync must not touch the other's addresses.
        set_loader_pin(VM_A, 3, 0x3000);
        replace_loader_pins(VM_A, &[(3, 0x3100)]);
        assert_eq!(loader_pin_addr(3), Some(0x3100));
        assert_eq!(loader_pin_addr(2), Some(0x2000));

        // Unload of a class id another VM has since claimed leaves it alone.
        set_loader_pin(VM_B, 3, 0x3200);
        remove_loader_pin(VM_A, 3);
        assert_eq!(loader_pin_addr(3), Some(0x3200));

        forget_vm_loader_pins(VM_A);
        forget_vm_loader_pins(VM_B);
    }
}
