// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

/// One class id's rows: `(owning VM, defining-ClassLoader heap address)`, at
/// most ONE per VM, the most recent writer LAST.
///
/// Almost always exactly one row. Class ids restart at 0 per VM, so a process
/// with several live VMs (`libcratonvm` embedders, this workspace's unit tests,
/// which build one `SharedVm` per test and run them in parallel) can have two
/// VMs' user-loader classes on one id; each keeps its own row.
type PinRows = Vec<(usize, usize)>;

/// `class_id -> rows` (see [`PinRows`]). Only user-defined loaders are
/// recorded (built-in/app/bootstrap classes are absent), so the map stays
/// small and a marker lookup is `None` for the overwhelmingly common case.
///
/// # Two kinds of reader, two answers
///
/// The authoritative side-table this mirrors
/// (`native-builtins::classloader::defining_loader_store`) is keyed by
/// `(vm_identity, class_id)`, and every writer here already holds that
/// `vm_identity`, so every row records its VM.
///
/// * A **root deferral** (`vm::memory::roots::vm_loader_pin_addr` and every
///   deferral that routes through it) REPLACES a root with a `metadata_pin` row
///   keyed by the loader address. It must use its OWN VM's row, or the value is
///   pinned to a loader no marker of this VM ever visits and is freed while
///   still referenced. It asks [`loader_pin_addr_for_vm`], which is exact.
/// * The GC **markers** (`gen_heap`, `gen_evac`, `g1`, `zgc`) have no VM
///   identity in hand and ask the VM-less [`loader_pin_addr`] / [`snapshot`] /
///   [`pinned_class_ids`], which answer from the most recent writer. For a
///   marker that is an extra edge at worst: an address in another VM's heap,
///   which its own bounds check rejects -- but see the collision case below,
///   which is what [`loader_pin_addr_where`] / [`snapshot_where`] are for.
///
/// Until gc-common w5-b (2026-09-24) the map held ONE `(vm, address)` per class
/// id and a second VM's write overwrote the first's. That made the root side
/// unsound (fixed there by a cross-check against the defining-loader table)
/// and lost the overwritten VM's row for good once the overwriting VM unloaded
/// the class -- the marker then had NO edge from that VM's live instances to
/// their loader until its next post-GC re-sync. Keeping one row per VM closes
/// both: a removal falls back to the surviving VM's row.
///
/// What a per-VM row can NOT fix on its own is the marker's collision case
/// (both VMs' classes live on one id): the VM-less answer names one of the two
/// loaders, and the other VM's marker gets a foreign address instead of its
/// own loader -- its own loader then loses the instance->loader edge.
/// A marker has no VM identity, but it does not need one: loader addresses are
/// heap addresses, unique across live VMs, and a marker can always say whether
/// an address is inside ITS heap. [`loader_pin_addr_where`] / [`snapshot_where`]
/// take that test and pick the row it accepts (gc-common w6-a). The
/// Generational, G1 and ZGC markers and copiers now call the `_where` forms
/// (`docs/feature-designs/profile-guided-inlining.md`,
/// `docs/feature-designs/real-jdk-mode.md`);
/// the VM-less [`loader_pin_addr`] / [`snapshot`] are left to tests (gci
/// i10/lp re-check, 2026-10-13: no production caller under `gc/src` or
/// `vm/src`). A stop-the-world copier that asks once per copied object should
/// take a [`snapshot_where`] once and ask [`loader_pin_addr_where_in`].
fn store() -> &'static RwLock<FxHashMap<u32, PinRows>> {
    static INSTANCE: OnceLock<RwLock<FxHashMap<u32, PinRows>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// The most recent writer's address: the VM-less answer.
#[inline]
fn latest_addr(rows: &PinRows) -> Option<usize> {
    rows.last().map(|&(_, addr)| addr)
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

/// Monotonically increasing version of this registry's CONTENTS.
///
/// *Added for
/// `docs/feature-designs/zgc-architecture.md`
/// and its continuation `handoff-i-arm-root-filter-concurrently.md`.*
///
/// A concurrent marker that has snapshotted this table's key set into a
/// lock-free filter needs to ask **"has anything been registered since?"**.
/// [`NON_EMPTY`] answers a different question — "has anything **ever** been
/// registered" — and re-reading the key set is `O(rows)` under the lock, which
/// is the cost the snapshot exists to remove. This is the third question, as
/// one relaxed load.
///
/// # The contract, which is the whole of its value
///
/// * It is bumped **inside the same write lock** that mutates the map, by every
///   mutator in this module, and only when the map really changed.
/// * A reader pairs it with a key set read under the **read** lock
///   ([`pinned_class_ids_with_generation`]), so the pair is exact rather than
///   merely ordered: no writer can be part-way through a mutation while that
///   lock is held.
/// * Therefore **`generation() == g` later ⟹ the key set is still exactly the
///   one that was read with `g`** — no addition, no removal, no in-place
///   change. That, and only that, is what makes a snapshot of this table a
///   proof outside a safepoint.
///
/// Note what it versions: the *contents*, including value-only updates
/// ([`set_loader_pin`] overwriting one class's loader address). That is
/// deliberately stronger than versioning the key set, because a consumer that
/// cached an address needs to know it moved.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current contents version. See [`GENERATION`].
///
/// `0` means nothing has ever been registered.
#[inline]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// [`pinned_class_ids`] paired **exactly** with the [`generation`] it was read
/// at.
///
/// Both reads happen under one acquisition of the read lock, so the pair cannot
/// straddle a mutation — which is the ordering hazard that would otherwise turn
/// this from a validity signal into a false proof (a row added mid-walk,
/// reported with the generation that already counts it, but absent from the
/// returned vector).
///
/// Deliberately does **not** take the [`NON_EMPTY`] short cut: reading the latch
/// and then the generation is exactly the straddle this function exists to rule
/// out. It runs once per collection, not once per object.
pub fn pinned_class_ids_with_generation() -> (Vec<u32>, u64) {
    let pins = store().read();
    let gen = GENERATION.load(Ordering::Acquire);
    (pins.keys().copied().collect(), gen)
}

/// Record / update the defining-loader heap address for `class_id`, owned by
/// `vm`. `vm`'s row becomes the most recent writer (the VM-less answer).
pub fn set_loader_pin(vm: usize, class_id: u32, loader_addr: usize) {
    let mut pins = store().write();
    let rows = pins.entry(class_id).or_default();
    // An identical re-registration is not a change, and a generation that ticks
    // for one would make a concurrent marker discard a snapshot that is still
    // exact. See [`GENERATION`]. (Nor is it re-ordered: the VM-less answer
    // does not move for a write that changes nothing.)
    if !rows.contains(&(vm, loader_addr)) {
        rows.retain(|&(owner, _)| owner != vm);
        rows.push((vm, loader_addr));
        GENERATION.fetch_add(1, Ordering::Release);
    }
    NON_EMPTY.store(true, Ordering::Relaxed);
}

/// Remove `vm`'s instance-to-loader edge for a class whose defining loader and
/// metadata have completed unloading.
///
/// Another VM's row for the same id is left alone -- the id is that VM's too,
/// and removing it would strip a live loader's pin -- and becomes the VM-less
/// answer again if it was shadowed.
pub fn remove_loader_pin(vm: usize, class_id: u32) {
    let mut pins = store().write();
    let (removed, now_empty) = match pins.get_mut(&class_id) {
        Some(rows) => {
            let before = rows.len();
            rows.retain(|&(owner, _)| owner != vm);
            (rows.len() != before, rows.is_empty())
        }
        None => (false, false),
    };
    if now_empty {
        pins.remove(&class_id);
    }
    if removed {
        GENERATION.fetch_add(1, Ordering::Release);
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
    let mut changed = false;
    pins.retain(|_, rows| {
        let before = rows.len();
        rows.retain(|&(owner, _)| owner != vm);
        changed |= rows.len() != before;
        !rows.is_empty()
    });
    if changed {
        GENERATION.fetch_add(1, Ordering::Release);
    }
    NON_EMPTY.store(!pins.is_empty(), Ordering::Relaxed);
}

/// The defining-loader heap address for `class_id`, if it was defined by a user
/// loader. Returns `None` for built-in/app/bootstrap classes. Hot path: a single
/// relaxed atomic load short-circuits when the registry is empty.
///
/// **For GC markers only.** With several live VMs the answer is the most
/// recent writer's row, which can be ANOTHER VM's loader -- harmless as a
/// marking edge (the marker's bounds check rejects it), a use-after-free as a
/// root deferral. A caller that has a VM identity -- every root scan does --
/// must use [`loader_pin_addr_for_vm`].
#[inline]
pub fn loader_pin_addr(class_id: u32) -> Option<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    store().read().get(&class_id).and_then(latest_addr)
}

/// `vm`'s OWN defining-loader heap address for `class_id`: `None` unless `vm`
/// registered a user loader for that id, whatever another VM holds at the same
/// id. The lookup every root DEFERRAL must use (see [`store`]); same
/// empty-registry short cut as [`loader_pin_addr`].
///
/// See `docs/feature-designs/real-jdk-mode.md`
/// (gc-common w5-b).
#[inline]
pub fn loader_pin_addr_for_vm(vm: usize, class_id: u32) -> Option<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    store()
        .read()
        .get(&class_id)?
        .iter()
        .find(|&&(owner, _)| owner == vm)
        .map(|&(_, addr)| addr)
}

/// The row a marker should follow: the only row, or -- when several VMs share
/// the id -- the most recent row `is_own` accepts.
///
/// One row (every single-VM process, and every id no two live VMs share) is
/// returned without consulting `is_own`, exactly as [`latest_addr`] would: the
/// marker's own bounds check rejects a foreign address, as it always has. With
/// several rows and none accepted, the most recent writer is returned, which
/// is again the VM-less answer; the predicate can only ever turn a foreign
/// answer into this heap's own.
#[inline]
fn pick_marker_row<F: Fn(usize) -> bool>(rows: &PinRows, is_own: &F) -> Option<usize> {
    match rows.as_slice() {
        [] => None,
        [(_, addr)] => Some(*addr),
        many => many
            .iter()
            .rev()
            .map(|&(_, addr)| addr)
            .find(|&addr| is_own(addr))
            .or_else(|| latest_addr(rows)),
    }
}

/// [`loader_pin_addr`] for a GC MARKER that can tell its own heap's addresses
/// from another VM's (`is_own`), but has no VM identity.
///
/// Answers the marker's collision case exactly: with two live VMs' user-loader
/// classes on one class id, the VM-less form names whichever VM wrote last, so
/// the OTHER VM's marker followed an instance to a foreign address (rejected
/// by its bounds check) instead of to its own defining loader -- which then
/// lost its only instance->loader edge and could be unloaded under a live
/// instance. Loader addresses are heap addresses and live heaps do not
/// overlap, so "the row inside my heap" is "my VM's row".
///
/// `is_own` runs under this registry's read lock and only when an id has more
/// than one row. It must be a pure address test (a heap-range or region-table
/// check) and must not call back into this module. The common case -- empty
/// registry, or one row -- costs what [`loader_pin_addr`] costs.
///
/// gc-common w6-a; adoption by the collectors is
/// `docs/feature-designs/profile-guided-inlining.md`.
#[inline]
pub fn loader_pin_addr_where<F: Fn(usize) -> bool>(class_id: u32, is_own: F) -> Option<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    let g = store().read();
    pick_marker_row(g.get(&class_id)?, &is_own)
}

/// [`loader_pin_addr_where`] answered from a [`snapshot_where`] taken with the
/// SAME `is_own` test when the caller has one, else from the live registry.
///
/// For a stop-the-world copier that would otherwise take this registry's read
/// lock and probe its map once per copied object (gci i10/lp,
/// `gci-i9yl-moving-copy-takes-the-loader-pin-lock-per-copied-object-RETIRED-UNFIXED-20261018`):
/// it takes the snapshot once at the top of the copy and passes `Some` here.
/// `snapshot_where` applies `pick_marker_row` per id with the same predicate,
/// so for every class id the two answers are equal as long as the rows the
/// predicate accepts did not change in between -- and those rows name the
/// caller's own heap, which only its own (stopped) mutators and its own
/// post-GC reconcile write. Another VM's concurrent write can only add or drop
/// a row the predicate rejects, which changes neither answer for an id that
/// has an own row, and for an id that has none the caller's bounds check
/// rejects both the old and the new foreign answer.
///
/// `None` (no snapshot: the switch is off, or the registry was empty when the
/// snapshot was taken) is exactly [`loader_pin_addr_where`].
#[inline]
pub fn loader_pin_addr_where_in<F: Fn(usize) -> bool>(
    snapshot: Option<&FxHashMap<u32, usize>>,
    class_id: u32,
    is_own: F,
) -> Option<usize> {
    match snapshot {
        Some(rows) => rows.get(&class_id).copied(),
        None => loader_pin_addr_where(class_id, is_own),
    }
}

/// [`snapshot`] with [`loader_pin_addr_where`]'s row choice: per class id, the
/// row inside the caller's heap when several VMs share the id.
pub fn snapshot_where<F: Fn(usize) -> bool>(is_own: F) -> Option<FxHashMap<u32, usize>> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    let g = store().read();
    if g.is_empty() {
        None
    } else {
        Some(
            g.iter()
                .filter_map(|(&cid, rows)| pick_marker_row(rows, &is_own).map(|addr| (cid, addr)))
                .collect(),
        )
    }
}

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

/// Whole-registry snapshot, `class_id -> defining loader address`, for a
/// marker that would otherwise call [`loader_pin_addr`] per scanned object.
///
/// Mirrors [`crate::metadata_pin::snapshot`], whose own doc makes the argument
/// in full: the per-object form takes this process-global `RwLock` for EVERY
/// marked object, which on a multi-million-object collection is both a
/// measurable cost and — once the marker runs on several threads — a shared
/// cache-line hot spot, for a registry that is empty in essentially every run.
/// `None` (the common case, the `NON_EMPTY` latch) lets the caller skip the
/// lookup entirely.
///
/// A snapshot is only equivalent to the live lookup for a caller whose view of
/// the HEAP is also frozen, because the values are heap addresses and a
/// collection rewrites them (`replace_loader_pins` is the post-GC
/// reconciliation that does it). G1's concurrent marker re-takes this whenever
/// the region table's epoch moves; a stop-the-world marker is frozen by
/// construction.
pub fn snapshot() -> Option<FxHashMap<u32, usize>> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    let g = store().read();
    if g.is_empty() {
        None
    } else {
        Some(
            g.iter()
                .filter_map(|(&cid, rows)| latest_addr(rows).map(|addr| (cid, addr)))
                .collect(),
        )
    }
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
/// (The Generational non-moving young marker does NOT use this form — rooting
/// every young pinned loader would stop an explicit `System.gc()` from
/// unloading a dead young loader. It marks a young loader that its own closure
/// did not reach only when an OLD object is an instance of one of its classes:
/// `gen_heap::seed_loaders_of_old_instances`, 2026-09-23.)
///
/// Mirrors [`crate::metadata_pin::snapshot`], whose own note makes the same
/// argument about the per-object variant being a shared-cache-line hot spot.
pub fn all_pinned_loaders() -> Vec<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return Vec::new();
    }
    // Every VM's rows, not only the most recent writer's: a caller that roots
    // these wholesale filters by its own heap (ZGC's `registry.contains`), and
    // a shadowed row is a live loader of the VM it belongs to.
    store()
        .read()
        .values()
        .flat_map(|rows| rows.iter().map(|&(_, addr)| addr))
        .collect()
}

/// Replace **`vm`'s** rows from the authoritative side-table snapshot
/// (`[(class_id, loader_addr)]`). Called by the post-GC reconciliation in
/// native-builtins after it has remapped/pruned the side-table, so the marker
/// sees current addresses on the next collection.
///
/// Rows owned by another VM survive: this is one VM's collection, and its
/// pointer map says nothing about another heap's addresses. `vm`'s re-written
/// rows become the most recent writer for their ids.
pub fn replace_loader_pins(vm: usize, entries: &[(u32, usize)]) {
    let mut g = store().write();
    g.retain(|_, rows| {
        rows.retain(|&(owner, _)| owner != vm);
        !rows.is_empty()
    });
    for &(cid, addr) in entries {
        let rows = g.entry(cid).or_default();
        // The side table has one loader per `(vm, class_id)`; stay defensive
        // against a duplicate id in `entries` anyway (one row per VM).
        rows.retain(|&(owner, _)| owner != vm);
        rows.push((vm, addr));
    }
    // Unconditional: a wholesale replacement is a content change even when the
    // row count happens to match, and this runs once per collection.
    GENERATION.fetch_add(1, Ordering::Release);
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

// There is deliberately no `clear_loader_pins()`: a blanket wipe cannot
// distinguish this VM's rows from a concurrently-live VM's, and the only
// caller it ever had ran at VM *creation*, where the sole rows in the table
// belong to somebody else. Use [`forget_vm_loader_pins`].

#[cfg(test)]
mod tests;
