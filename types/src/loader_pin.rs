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
/// `vm`.
pub fn set_loader_pin(vm: usize, class_id: u32, loader_addr: usize) {
    let mut pins = store().write();
    // An identical re-registration is not a change, and a generation that ticks
    // for one would make a concurrent marker discard a snapshot that is still
    // exact. See [`GENERATION`].
    if pins.insert(class_id, (vm, loader_addr)) != Some((vm, loader_addr)) {
        GENERATION.fetch_add(1, Ordering::Release);
    }
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
    let before = pins.len();
    pins.retain(|_, &mut (owner, _)| owner != vm);
    if pins.len() != before {
        GENERATION.fetch_add(1, Ordering::Release);
    }
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
        Some(g.iter().map(|(&cid, &(_vm, addr))| (cid, addr)).collect())
    }
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
