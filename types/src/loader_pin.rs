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
/// take that test and pick the row it accepts (gc-common w6-a). The collectors
/// still call the VM-less forms; moving them over is
/// `docs/internal/gc-common-round-20260923/handoff-w6a-markers-pick-their-own-heaps-loader-pin.md`
/// (`docs/known-issues/gc/common-w4b-loader-pin-collision-unroots-another-vms-statics.md`).
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
/// `docs/internal/zgc-round-20260920/gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`
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
/// See `docs/known-issues/gc/common-w4b-loader-pin-collision-unroots-another-vms-statics.md`
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
/// `docs/internal/gc-common-round-20260923/handoff-w6a-markers-pick-their-own-heaps-loader-pin.md`.
#[inline]
pub fn loader_pin_addr_where<F: Fn(usize) -> bool>(class_id: u32, is_own: F) -> Option<usize> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    let g = store().read();
    pick_marker_row(g.get(&class_id)?, &is_own)
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
mod tests {
    use super::*;

    /// One process-global map and one process-global [`GENERATION`], so these
    /// run one at a time even though their VM ids differ: a generation
    /// assertion is about the WHOLE table, and a concurrently-running test that
    /// registers its own row ticks it. Mirrors `metadata_pin`'s own test lock.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn teardown_drops_only_the_torn_down_vms_rows() {
        const VM_A: usize = 0xA00;
        const VM_B: usize = 0xB00;
        let _g = TEST_LOCK.lock();
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

    /// **Every mutation must tick the generation, and a non-mutation must not.**
    ///
    /// A concurrent marker treats "the generation did not move" as a proof that
    /// its snapshot of the key set is still exact (see [`GENERATION`]). A
    /// mutator that forgets to tick therefore does not make the filter slower,
    /// it makes it *wrong*: the snapshot answers "this class has no pinned
    /// loader" for a class that has just acquired one, and the loader is swept
    /// while live. The other direction only costs a discarded snapshot, but a
    /// tick for an identical re-registration would discard one on every
    /// post-GC reconciliation, so it is asserted too.
    #[test]
    fn the_generation_ticks_for_every_real_mutation_and_only_those() {
        const VM_A: usize = 0xA02;
        const VM_B: usize = 0xB02;
        let _g = TEST_LOCK.lock();
        forget_vm_loader_pins(VM_A);
        forget_vm_loader_pins(VM_B);

        let g0 = generation();
        set_loader_pin(VM_A, 4001, 0x4000);
        let g1 = generation();
        assert!(g1 > g0, "a new pin must tick");

        // Idempotent re-registration: same VM, same class, same address.
        set_loader_pin(VM_A, 4001, 0x4000);
        assert_eq!(
            generation(),
            g1,
            "an identical re-registration is no change"
        );

        set_loader_pin(VM_A, 4001, 0x4100);
        let g2 = generation();
        assert!(g2 > g1, "a moved loader address must tick");

        // A removal aimed at a row another VM owns changes nothing.
        set_loader_pin(VM_B, 4002, 0x4200);
        let g3 = generation();
        remove_loader_pin(VM_A, 4002);
        assert_eq!(generation(), g3, "a refused removal is no change");
        remove_loader_pin(VM_B, 4002);
        let g4 = generation();
        assert!(g4 > g3, "a real removal must tick");

        // A teardown that drops nothing is not a change; one that drops a row is.
        forget_vm_loader_pins(VM_B);
        assert_eq!(generation(), g4, "VM B has no rows left");
        forget_vm_loader_pins(VM_A);
        assert!(generation() > g4);
    }

    /// **Two VMs on one class id keep one row each** (gc-common w5-b,
    /// `common-w4b-loader-pin-collision-unroots-another-vms-statics.md`).
    ///
    /// The per-VM lookup a root deferral uses must answer with the asking VM's
    /// own loader whatever order the VMs wrote in, and `None` for a VM with no
    /// row. The VM-less marker answer is the most recent writer, and falls
    /// back to the surviving VM's row when that writer unloads -- the single-
    /// row map lost it for good.
    #[test]
    fn colliding_class_ids_keep_one_row_per_vm() {
        const VM_A: usize = 0xA04;
        const VM_B: usize = 0xB04;
        const VM_C: usize = 0xC04;
        const CID: u32 = 4201;
        let _g = TEST_LOCK.lock();
        forget_vm_loader_pins(VM_A);
        forget_vm_loader_pins(VM_B);

        set_loader_pin(VM_A, CID, 0xa000);
        set_loader_pin(VM_B, CID, 0xb000);
        assert_eq!(loader_pin_addr_for_vm(VM_A, CID), Some(0xa000));
        assert_eq!(loader_pin_addr_for_vm(VM_B, CID), Some(0xb000));
        assert_eq!(
            loader_pin_addr_for_vm(VM_C, CID),
            None,
            "a VM with no user-loader class at this id must root, not defer"
        );
        assert_eq!(loader_pin_addr(CID), Some(0xb000), "most recent writer");
        let mut all = all_pinned_loaders();
        all.retain(|&a| a == 0xa000 || a == 0xb000);
        all.sort_unstable();
        assert_eq!(all, vec![0xa000, 0xb000], "every VM's loader is listed");

        // A's post-GC re-sync moves A's loader and makes A the latest writer;
        // B's row is untouched.
        replace_loader_pins(VM_A, &[(CID, 0xa100)]);
        assert_eq!(loader_pin_addr_for_vm(VM_A, CID), Some(0xa100));
        assert_eq!(loader_pin_addr_for_vm(VM_B, CID), Some(0xb000));
        assert_eq!(loader_pin_addr(CID), Some(0xa100));

        // A unloads the class: B's row becomes the VM-less answer again.
        remove_loader_pin(VM_A, CID);
        assert_eq!(loader_pin_addr_for_vm(VM_A, CID), None);
        assert_eq!(
            loader_pin_addr(CID),
            Some(0xb000),
            "the surviving VM's live loader keeps its marker edge"
        );
        assert_eq!(snapshot().and_then(|s| s.get(&CID).copied()), Some(0xb000));

        forget_vm_loader_pins(VM_B);
        assert_eq!(loader_pin_addr(CID), None);
        assert!(!pinned_class_ids().contains(&CID));
    }

    /// **A marker that can recognise its own heap follows its own VM's loader**
    /// (gc-common w6-a, the marker half of
    /// `common-w4b-loader-pin-collision-unroots-another-vms-statics.md`).
    ///
    /// Two VMs' user-loader classes on one id: the VM-less answer names the
    /// most recent writer, so the other VM's marker used to be handed a
    /// foreign address and its own loader lost the instance->loader edge.
    /// `loader_pin_addr_where` / `snapshot_where` pick the row the caller's
    /// heap test accepts, whatever the write order, and consult that test
    /// only when there is a choice to make.
    #[test]
    fn a_marker_picks_the_row_inside_its_own_heap() {
        const VM_A: usize = 0xA05;
        const VM_B: usize = 0xB05;
        const CID: u32 = 4301;
        const SOLO: u32 = 4302;
        let _g = TEST_LOCK.lock();
        forget_vm_loader_pins(VM_A);
        forget_vm_loader_pins(VM_B);
        // "Heap A" is [0xa000, 0xb000), "heap B" is [0xb000, 0xc000).
        let in_a = |addr: usize| (0xa000..0xb000).contains(&addr);
        let in_b = |addr: usize| (0xb000..0xc000).contains(&addr);

        set_loader_pin(VM_A, CID, 0xa100);
        set_loader_pin(VM_B, CID, 0xb100);
        assert_eq!(loader_pin_addr(CID), Some(0xb100), "VM-less: last writer");
        assert_eq!(loader_pin_addr_where(CID, in_a), Some(0xa100));
        assert_eq!(loader_pin_addr_where(CID, in_b), Some(0xb100));
        // Write order does not matter.
        replace_loader_pins(VM_A, &[(CID, 0xa200)]);
        assert_eq!(loader_pin_addr(CID), Some(0xa200));
        assert_eq!(loader_pin_addr_where(CID, in_b), Some(0xb100));
        assert_eq!(loader_pin_addr_where(CID, in_a), Some(0xa200));
        // A heap that owns neither row gets the VM-less answer back (its own
        // bounds check rejects it, exactly as before).
        assert_eq!(loader_pin_addr_where(CID, |_| false), Some(0xa200));

        let snap_b = snapshot_where(in_b).expect("rows exist");
        assert_eq!(snap_b.get(&CID).copied(), Some(0xb100));
        let snap_a = snapshot_where(in_a).expect("rows exist");
        assert_eq!(snap_a.get(&CID).copied(), Some(0xa200));

        // One row: returned without asking -- the per-object common case must
        // not pay for a heap test.
        set_loader_pin(VM_A, SOLO, 0xa300);
        let asked = std::cell::Cell::new(0u32);
        let counting = |_: usize| {
            asked.set(asked.get() + 1);
            false
        };
        assert_eq!(loader_pin_addr_where(SOLO, counting), Some(0xa300));
        assert_eq!(asked.get(), 0, "a single row needs no heap test");
        assert_eq!(loader_pin_addr_where(9_999_999, in_a), None);

        forget_vm_loader_pins(VM_A);
        forget_vm_loader_pins(VM_B);
        assert_eq!(loader_pin_addr_where(CID, in_a), None);
    }

    /// The paired reader must hand back a key set and a generation that were
    /// taken under one lock acquisition -- the property the marker's proof
    /// rests on.
    #[test]
    fn the_paired_reader_agrees_with_the_unpaired_ones() {
        const VM: usize = 0xA03;
        let _g = TEST_LOCK.lock();
        forget_vm_loader_pins(VM);
        set_loader_pin(VM, 4101, 0x4100);
        let (ids, gen) = pinned_class_ids_with_generation();
        assert_eq!(gen, generation());
        assert!(ids.contains(&4101));
        let mut a = ids;
        let mut b = pinned_class_ids();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
        forget_vm_loader_pins(VM);
    }
}
