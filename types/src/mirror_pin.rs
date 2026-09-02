// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class-mirror liveness pinning registry — companion to `loader_pin`, same
//! problem in the OPPOSITE direction.
//!
//! `loader_pin` propagates "a live INSTANCE of class C keeps C's defining
//! `ClassLoader` alive" (an edge the object header doesn't carry directly,
//! since it stores only an integer `class_id`). This module propagates the
//! reverse edge real JVMs get for free from `java.lang.ClassLoader`'s own
//! `classes` bookkeeping field: **a live `ClassLoader` keeps every `Class`
//! mirror it defined alive**, so a still-in-use loader (e.g. a webapp's
//! shared `WebappClassLoader`, reachable via the running `Context`) doesn't
//! lose its OTHER, still-live classes' mirrors just because nothing
//! currently holds a fresh `Class<?>` reference to them (identity reflection
//! results — `getClass()`, annotation scans — are typically used once and
//! discarded, not retained).
//!
//! Populated by `vm::vm_object::get_or_create_class_mirror` whenever it
//! creates a mirror for a class with a recorded user-defined defining loader
//! (`native-builtins::classloader::defining_loader_for`). Rebuilt from
//! scratch after every GC cycle (`vm::memory::gc::update_all_roots`) from the
//! authoritative, now-remapped `class_mirrors` / defining-loader side-tables,
//! so it can never hold a stale post-collection address. Lives in
//! `cratonvm-types` so both the GC marker (`gen_heap`) and the VM
//! (`get_or_create_class_mirror`) can reach it without a new crate
//! dependency, mirroring `loader_pin`.
//!
//! GC marker usage (mirrors `loader_pin`'s 3 call sites in `gen_heap.rs`):
//! whenever the marker marks an object alive, it ALSO checks whether that
//! object's OWN address is a known loader address here — keyed by address,
//! not `class_id`, since the propagation direction is loader-found -> its
//! classes, not object's-class -> its loader. If found, every listed mirror
//! address is marked alive too.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

/// `loader heap address -> (owning VM, [mirror heap addresses defined by that
/// loader])`. Only user-defined loaders ever appear (built-in/bootstrap
/// classes' mirrors are unconditionally rooted directly — see
/// `vm::memory::roots` step 6 — so they never need this propagation), so the
/// map stays small in the common case (no custom `ClassLoader` ever touched).
///
/// The key is a heap address, which is unique across every live VM in the
/// process, so the read path needs no VM and a second VM's rows can never be
/// mistaken for this one's. The owning VM is recorded only so
/// [`forget_vm_mirror_pins`] can drop exactly one VM's rows at teardown — which
/// is what the blanket wipe at VM *creation* got wrong: at that point every row
/// in the table belongs to somebody else.
fn store() -> &'static RwLock<FxHashMap<usize, (usize, Vec<usize>)>> {
    static INSTANCE: OnceLock<RwLock<FxHashMap<usize, (usize, Vec<usize>)>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// Fast "is the registry non-empty" check so the marker can skip the
/// per-object lookup entirely when no user loader has ever defined a class
/// (the common case — most runs never touch a custom `ClassLoader`).
static NON_EMPTY: AtomicBool = AtomicBool::new(false);

/// Record that `loader_addr` (a user-defined `ClassLoader`'s current heap
/// address, in `vm`'s heap) defined the class whose mirror lives at
/// `mirror_addr`.
pub fn add_mirror_pin(vm: usize, loader_addr: usize, mirror_addr: usize) {
    let mut g = store().write();
    let row = g.entry(loader_addr).or_insert_with(|| (vm, Vec::new()));
    row.0 = vm;
    row.1.push(mirror_addr);
    NON_EMPTY.store(true, Ordering::Relaxed);
}

/// Mirror addresses to also mark alive when `loader_addr` is marked alive.
/// Returns an owned copy (not a guard) so callers can mark the returned
/// addresses without holding this registry's lock re-entrantly.
#[inline]
pub fn mirrors_for_loader(loader_addr: usize) -> Option<Vec<usize>> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    let g = store().read();
    let (_vm, v) = g.get(&loader_addr)?;
    if v.is_empty() {
        None
    } else {
        Some(v.clone())
    }
}

/// Every pinned mirror address, for a marker that cannot ask per loader.
///
/// See [`crate::loader_pin::all_pinned_loaders`] for why a generational young
/// cycle needs the wholesale form: it never visits an old loader, so it never
/// reaches the per-loader lookup, and a mirror reachable only that way would be
/// swept while its class is live.
/// The loader addresses that OWN pinned mirrors -- the keys, not the values.
///
/// The per-object lookup is keyed by owner address, so a marker can snapshot
/// these once per collection and reject every other address without taking
/// the lock.
pub fn pinned_owner_addrs() -> Vec<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return Vec::new();
    }
    store().read().keys().copied().collect()
}

pub fn all_pinned_mirrors() -> Vec<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return Vec::new();
    }
    store()
        .read()
        .values()
        .flat_map(|(_vm, mirrors)| mirrors.iter().copied())
        .collect()
}

/// Replace the entire registry from an authoritative post-GC snapshot
/// (`[(loader_addr, mirror_addr)]`, both already remapped to their
/// post-collection addresses). Called once per GC cycle after `class_mirrors`
/// and the defining-loader side-table have both finished reconciling/
/// remapping, so the marker always sees current addresses on the next
/// collection.
/// Rows owned by another VM survive: this is one VM's collection, and its
/// pointer map says nothing about another heap's addresses.
pub fn replace_mirror_pins(vm: usize, entries: &[(usize, usize)]) {
    let mut g = store().write();
    g.retain(|_, (owner, _)| *owner != vm);
    for &(loader_addr, mirror_addr) in entries {
        let row = g.entry(loader_addr).or_insert_with(|| (vm, Vec::new()));
        row.0 = vm;
        row.1.push(mirror_addr);
    }
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

/// Drop every row `vm` owns, at that VM's teardown.
///
/// This replaces a blanket `clear()` that ran at VM *creation*, where the only
/// rows present belong to a concurrently-live VM — see [`store`].
pub fn forget_vm_mirror_pins(vm: usize) {
    let mut g = store().write();
    g.retain(|_, (owner, _)| *owner != vm);
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_drops_only_the_torn_down_vms_rows() {
        const VM_A: usize = 0xA00;
        const VM_B: usize = 0xB00;
        forget_vm_mirror_pins(VM_A);
        forget_vm_mirror_pins(VM_B);

        add_mirror_pin(VM_A, 0x1000, 0x1008);
        add_mirror_pin(VM_B, 0x2000, 0x2008);

        forget_vm_mirror_pins(VM_A);
        assert_eq!(mirrors_for_loader(0x1000), None);
        assert_eq!(
            mirrors_for_loader(0x2000),
            Some(vec![0x2008]),
            "a concurrently-live VM keeps its mirrors"
        );

        // One VM's post-GC re-sync must leave the other's rows in place.
        replace_mirror_pins(VM_A, &[(0x1100, 0x1108)]);
        assert_eq!(mirrors_for_loader(0x1100), Some(vec![0x1108]));
        assert_eq!(mirrors_for_loader(0x2000), Some(vec![0x2008]));

        forget_vm_mirror_pins(VM_A);
        forget_vm_mirror_pins(VM_B);
    }
}
