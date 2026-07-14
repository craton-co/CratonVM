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

/// `loader heap address -> [mirror heap addresses defined by that loader]`.
/// Only user-defined loaders ever appear (built-in/bootstrap classes' mirrors
/// are unconditionally rooted directly — see `vm::memory::roots` step 6 — so
/// they never need this propagation), so the map stays small in the common
/// case (no custom `ClassLoader` ever touched).
fn store() -> &'static RwLock<FxHashMap<usize, Vec<usize>>> {
    static INSTANCE: OnceLock<RwLock<FxHashMap<usize, Vec<usize>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// Fast "is the registry non-empty" check so the marker can skip the
/// per-object lookup entirely when no user loader has ever defined a class
/// (the common case — most runs never touch a custom `ClassLoader`).
static NON_EMPTY: AtomicBool = AtomicBool::new(false);

/// Record that `loader_addr` (a user-defined `ClassLoader`'s current heap
/// address) defined the class whose mirror lives at `mirror_addr`.
pub fn add_mirror_pin(loader_addr: usize, mirror_addr: usize) {
    store()
        .write()
        .entry(loader_addr)
        .or_default()
        .push(mirror_addr);
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
    let v = g.get(&loader_addr)?;
    if v.is_empty() {
        None
    } else {
        Some(v.clone())
    }
}

/// Replace the entire registry from an authoritative post-GC snapshot
/// (`[(loader_addr, mirror_addr)]`, both already remapped to their
/// post-collection addresses). Called once per GC cycle after `class_mirrors`
/// and the defining-loader side-table have both finished reconciling/
/// remapping, so the marker always sees current addresses on the next
/// collection.
pub fn replace_mirror_pins(entries: &[(usize, usize)]) {
    let mut g = store().write();
    g.clear();
    for &(loader_addr, mirror_addr) in entries {
        g.entry(loader_addr).or_default().push(mirror_addr);
    }
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

/// Drop all entries (new-VM reset).
pub fn clear_mirror_pins() {
    store().write().clear();
    NON_EMPTY.store(false, Ordering::Relaxed);
}
