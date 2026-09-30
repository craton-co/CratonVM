// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loader-owned heap metadata edges used by the non-moving generational marker.
//!
//! Static reference fields, synthetic class locks, condy values and reflective
//! descriptor caches must live while their defining loader is live, but must
//! not independently root that loader forever. This registry models the
//! missing `ClassLoaderData -> metadata oops` edge in CratonVM's side-table
//! class-loader representation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};

/// A row holding at most this many objects de-duplicates by a linear scan.
/// A bigger row builds a hash index. See [`Row`].
const LINEAR_DEDUP_MAX: usize = 32;

/// One loader's row: the owning VM and the loader-owned heap addresses, each
/// recorded once.
///
/// # Why a row can carry an index (gc-common w8-e)
///
/// `collect_roots` fills this registry one [`add_metadata_pin`] at a time:
/// every deferred static, class lock, condy value, proxy `Method` and native
/// cache value of every user-loader class, on every root scan, inside the
/// pause. Until w8-e each insert de-duplicated with `Vec::contains`. That is
/// quadratic in the number of values one loader owns. A webapp or Spring Boot
/// loader with 20,000 deferred static values costs about 2*10^8 comparisons
/// per collection, under this process-global write lock. A small row keeps the
/// scan, because it allocates nothing. Past [`LINEAR_DEDUP_MAX`] the row
/// builds a hash set once and each insert is O(1). `objects` keeps insertion
/// order, and the readers hand out clones of it as before.
struct Row {
    vm: usize,
    objects: Vec<usize>,
    index: Option<FxHashSet<usize>>,
}

impl Row {
    fn new(vm: usize) -> Self {
        Row {
            vm,
            objects: Vec::new(),
            index: None,
        }
    }

    /// Record `object` once. Returns whether it was new.
    fn insert(&mut self, object: usize) -> bool {
        let new = match self.index.as_mut() {
            Some(index) => index.insert(object),
            None => !self.objects.contains(&object),
        };
        if !new {
            return false;
        }
        if self.index.is_none() && self.objects.len() >= LINEAR_DEDUP_MAX {
            let mut index: FxHashSet<usize> = self.objects.iter().copied().collect();
            index.insert(object);
            self.index = Some(index);
        }
        self.objects.push(object);
        true
    }
}

/// `loader heap address -> (owning VM, [loader-owned heap addresses])`.
///
/// The key is a heap address, unique across every live VM in the process, so
/// the marker's read path needs no VM identity. The owning VM is recorded so
/// [`forget_vm_metadata_pins`] can drop exactly one VM's rows at teardown, in
/// place of the blanket wipe that used to run at VM *creation* — where every
/// row present belongs to a concurrently-live VM.
fn store() -> &'static RwLock<FxHashMap<usize, Row>> {
    static STORE: OnceLock<RwLock<FxHashMap<usize, Row>>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

// There is deliberately no process-global weak-mode flag any more (gc-common
// w6-a, 2026-09-24). `WEAK_MODE` was one `AtomicBool` for the whole process,
// written by every VM's `collect_roots` and read through a VM-less
// `metadata_weak_mode()`, so VM B's root scan could defer under VM A's mode
// (`docs/internal/gc-common-round-20260923/common-b-process-global-root-sources-FIXED-20260923.md`). Every
// reader now takes its own VM's licence from the scan that published it
// (`vm::memory::roots::loader_metadata_licence`); the last one,
// `phases_late::gc_scan_classvalue_cache_roots`, went in wave 5. What remains
// of the switch is [`set_metadata_weak_mode`]'s two effects, both per-VM or
// content-versioned: the generation tick and the drop of that VM's rows.

/// Is the registry non-empty? Maintained by every mutator below.
///
/// # Why the read path needs a latch
///
/// [`roots_for_loader`] is called **once per marked object** by every marker in
/// the tree, and [`snapshot`]'s own note says what that costs: the registry
/// `RwLock` plus a `Vec` clone per object, for a registry that is empty in
/// essentially every run. `snapshot` was written to remove it and has no caller,
/// so the cheap half of the same argument belongs on the per-object path — one
/// relaxed load, which is exactly what [`crate::loader_pin`] and
/// [`crate::mirror_pin`] have always had.
///
/// The 2026-08-14 parallel-marking measurement is why this is not cosmetic: four
/// workers cost **+153% pause** against zero, and the rise is monotonic in
/// worker count, which is a lock rather than a start-up offset. An uncontended
/// `RwLock` read is a few nanoseconds; the same read from eight workers is a
/// shared cache line bouncing between eight cores, once per object.
///
/// Ordering is `Relaxed` in both directions, deliberately. A stale `false` can
/// only be read by a marker racing a *registration*, and a registration happens
/// at class definition under the VM's own synchronisation, never concurrently
/// with the collection that would consult it — the marker runs at a safepoint or
/// with the registry already published. Making it `Release`/`Acquire` would buy
/// nothing here and would put a fence on the per-object path.
static NON_EMPTY: AtomicBool = AtomicBool::new(false);

/// Monotonically increasing version of this registry's CONTENTS **and** of
/// every VM's weak-mode switch ([`set_metadata_weak_mode`]).
///
/// *Added for
/// `docs/internal/zgc-round-20260920/gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`
/// and its continuation `handoff-i-arm-root-filter-concurrently.md`. Same
/// contract as [`crate::loader_pin`]'s and [`crate::mirror_pin`]'s.*
///
/// [`NON_EMPTY`] above answers "has anything **ever** been registered", which is
/// what a per-object fast path needs. A concurrent marker needs the other
/// question — "has anything been registered **since** I snapshotted the key
/// set?" — because its snapshot's `false` is a proof and a registration
/// invalidates it. This is that question, as one relaxed load.
///
/// Bumped inside the same write lock that mutates the map, only on a real
/// change, and read paired with the key set under the read lock
/// ([`pinned_owner_addrs_with_generation`]) so the pair cannot straddle a
/// mutation.
///
/// **[`set_metadata_weak_mode`] ticks it too**, even when it drops no row.
/// Weak mode decides whether these roots are conditional at all, so a consumer
/// that cached a decision taken under one mode must re-take it under the other.
/// The tick is unconditional: `collect_roots` step 1 calls it once per root
/// scan of every VM, whether or not the mode changed (gc-common w7-a corrected
/// an older "a handful of times per process" here). One atomic add per scan is
/// free; the cost is to a ZGC concurrent phase, whose root filter
/// (`zgc/mark_roots.rs`) stops trusting its snapshot once the generation moves.
/// Within one VM, root scans run in pauses. In a multi-VM process, another VM's
/// pause can land inside this VM's concurrent phase and cost it the filter.
/// That is a performance loss, never a wrong answer.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current contents version. See [`GENERATION`].
///
/// `0` means nothing has ever been registered and weak mode has never moved.
#[inline]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// The owner (loader) addresses that have metadata rows, paired **exactly**
/// with the [`generation`] they were read at.
///
/// [`snapshot`] is the values form; this is the key-set form the extra-root
/// filter indexes, and the generation is what lets it stay a proof outside a
/// safepoint. Both reads happen under one acquisition of the read lock, so the
/// pair cannot straddle a mutation — reading the latch or the generation first
/// is exactly the ordering that would make it a false proof.
pub fn pinned_owner_addrs_with_generation() -> (Vec<usize>, u64) {
    let pins = store().read();
    let gen = GENERATION.load(Ordering::Acquire);
    (pins.keys().copied().collect(), gen)
}

/// `vm`'s collection decided whether its loader-owned metadata roots are
/// conditional this cycle.
///
/// Stores no mode: the only consumer of the decision is that VM's own root
/// scan, which holds it (`vm::memory::roots::loader_metadata_licence`). A
/// process-global copy (`WEAK_MODE` / `metadata_weak_mode()`) let one VM's
/// scan defer under another VM's decision and was deleted in gc-common w6-a
/// once its last reader was gone.
///
/// `enabled == false` drops **`vm`'s** rows: with weak mode off this VM roots
/// its loader-owned metadata unconditionally and the registry is dead weight
/// for it, but another VM may still be mid-collection with weak mode on.
pub fn set_metadata_weak_mode(vm: usize, enabled: bool) {
    // Ticked on every call, before the drop below: a consumer that cached a
    // decision taken under one mode must re-take it, and a reader that sees
    // the drop also sees a generation that has already moved past the
    // snapshot it took before it. See [`GENERATION`].
    GENERATION.fetch_add(1, Ordering::Release);
    if !enabled {
        forget_vm_metadata_pins(vm);
    }
}

/// Replace **`vm`'s** rows with the current loader-owned root snapshot.
pub fn replace_metadata_pins(vm: usize, entries: &[(usize, usize)]) {
    let mut pins = store().write();
    pins.retain(|_, row| row.vm != vm);
    // `NON_EMPTY` is recomputed at the end of this function, not here: rows for
    // other VMs survive the retain, so "we just cleared ours" does not mean the
    // registry is empty.
    for &(loader, object) in entries {
        let row = pins.entry(loader).or_insert_with(|| Row::new(vm));
        row.vm = vm;
        row.insert(object);
    }
    // Unconditional: a wholesale replacement is a content change even when the
    // key set survives it unchanged.
    GENERATION.fetch_add(1, Ordering::Release);
    NON_EMPTY.store(!pins.is_empty(), Ordering::Relaxed);
}

/// Add one root discovered by a process-global native cache scanner.
pub fn add_metadata_pin(vm: usize, loader: usize, object: usize) {
    let mut pins = store().write();
    let row = pins.entry(loader).or_insert_with(|| Row::new(vm));
    row.vm = vm;
    if row.insert(object) {
        GENERATION.fetch_add(1, Ordering::Release);
    }
    NON_EMPTY.store(true, Ordering::Relaxed);
}

/// Snapshot heap addresses owned by a loader that has just become marked.
pub fn roots_for_loader(loader: usize) -> Option<Vec<usize>> {
    // THE LATCH FIRST -- see [`NON_EMPTY`] for what this per-object lock cost.
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    store().read().get(&loader).map(|row| row.objects.clone())
}

/// Whole-registry snapshot for a stop-the-world marker.
///
/// `roots_for_loader` takes the registry `RwLock` and clones a `Vec` for
/// EVERY marked object, which on a multi-million-object young collection is
/// both a measurable cost and (once the marker runs on several threads) a
/// shared-cache-line hot spot -- for a registry that is empty in essentially
/// every run. The marker is at a safepoint, so one snapshot taken before the
/// closure starts is exactly equivalent, and `None` (the common case) lets it
/// skip the lookup entirely.
pub fn snapshot() -> Option<FxHashMap<usize, Vec<usize>>> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    let g = store().read();
    if g.is_empty() {
        None
    } else {
        Some(g.iter().map(|(&k, row)| (k, row.objects.clone())).collect())
    }
}

/// Drop every row `vm` owns, at that VM's teardown.
pub fn forget_vm_metadata_pins(vm: usize) {
    let mut pins = store().write();
    let before = pins.len();
    pins.retain(|_, row| row.vm != vm);
    if pins.len() != before {
        GENERATION.fetch_add(1, Ordering::Release);
    }
    NON_EMPTY.store(!pins.is_empty(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The `NON_EMPTY` latch must never say `false` while a row exists.**
    ///
    /// A latch on a per-object fast path is a correctness device, not an
    /// optimisation: a stale `false` makes [`roots_for_loader`] return `None`
    /// for a loader that DOES own metadata roots, and the marker then sweeps a
    /// static reference field's target while its defining loader is live. Every
    /// mutator has to maintain it, and the one that is easy to get wrong is
    /// `replace_metadata_pins` -- its `retain` drops one VM's rows while another
    /// VM's survive, so "I just cleared mine" is not "the registry is empty".
    #[test]
    fn the_non_empty_latch_tracks_the_registry_through_every_mutator() {
        const A: usize = 0xA10;
        const B: usize = 0xB10;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(A);
        forget_vm_metadata_pins(B);
        assert!(!NON_EMPTY.load(Ordering::Relaxed), "both VMs cleared");

        add_metadata_pin(A, 0x1000, 0x2000);
        assert!(NON_EMPTY.load(Ordering::Relaxed));
        assert!(
            roots_for_loader(0x1000).is_some(),
            "and the read path agrees"
        );

        // TWO VMs. Clearing A must NOT lower the latch, because B still has a
        // row -- and B's row is exactly what a stale `false` would hide.
        add_metadata_pin(B, 0x3000, 0x4000);
        replace_metadata_pins(A, &[]);
        assert!(
            NON_EMPTY.load(Ordering::Relaxed),
            "VM B still owns a row; a lowered latch here drops its roots"
        );
        assert!(roots_for_loader(0x3000).is_some());

        forget_vm_metadata_pins(B);
        assert!(!NON_EMPTY.load(Ordering::Relaxed), "now it is really empty");
        assert!(roots_for_loader(0x3000).is_none());
    }

    /// The store and [`GENERATION`] are process-wide, so these run one at a
    /// time even though their VM ids differ.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn loader_owned_roots_are_replaced_and_deduplicated() {
        const VM: usize = 0xA00;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(VM);
        set_metadata_weak_mode(VM, true);
        replace_metadata_pins(VM, &[(10, 100), (10, 100), (10, 101)]);
        assert_eq!(roots_for_loader(10), Some(vec![100, 101]));
        add_metadata_pin(VM, 10, 102);
        assert_eq!(roots_for_loader(10), Some(vec![100, 101, 102]));
        replace_metadata_pins(VM, &[(20, 200)]);
        assert!(roots_for_loader(10).is_none());
        forget_vm_metadata_pins(VM);
    }

    #[test]
    fn one_vms_reset_leaves_a_concurrently_live_vms_roots_alone() {
        const VM_A: usize = 0xA01;
        const VM_B: usize = 0xB01;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(VM_A);
        forget_vm_metadata_pins(VM_B);
        add_metadata_pin(VM_A, 0x1000, 0x1008);
        add_metadata_pin(VM_B, 0x2000, 0x2008);

        // Both of the wipes this replaced — VM creation, and weak mode going
        // off — took the whole table.
        replace_metadata_pins(VM_A, &[]);
        set_metadata_weak_mode(VM_A, false);
        assert_eq!(roots_for_loader(0x1000), None);
        assert_eq!(roots_for_loader(0x2000), Some(vec![0x2008]));

        forget_vm_metadata_pins(VM_B);
    }

    /// **The generation is the concurrent marker's only lock-free way to learn
    /// that its snapshot stopped being a proof.**
    ///
    /// The extra-root filter snapshots this table's owner key set and then
    /// answers "that object owns no loader metadata" from a bloom, per marked
    /// object, without a lock. A mutator that changes the table without ticking
    /// leaves that `false` looking valid while it is wrong -- a static
    /// reference field's target swept with its defining loader live, which is
    /// the same failure `the_non_empty_latch_tracks_the_registry_through_every_mutator`
    /// guards from the other side.
    #[test]
    fn the_generation_ticks_for_every_real_mutation() {
        const VM_A: usize = 0xA02;
        const VM_B: usize = 0xB02;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(VM_A);
        forget_vm_metadata_pins(VM_B);

        let g0 = generation();
        add_metadata_pin(VM_A, 0x6000, 0x6008);
        let g1 = generation();
        assert!(g1 > g0, "a new owner must tick");

        add_metadata_pin(VM_A, 0x6000, 0x6010);
        let g2 = generation();
        assert!(g2 > g1, "a new root under an existing owner must tick");

        // The de-duplicating arm records nothing, so it changes nothing.
        add_metadata_pin(VM_A, 0x6000, 0x6010);
        assert_eq!(generation(), g2, "a duplicate is not a change");

        replace_metadata_pins(VM_A, &[(0x6100, 0x6108)]);
        let g3 = generation();
        assert!(g3 > g2, "a wholesale replacement must tick");

        // Weak mode gates whether these roots are conditional at all.
        set_metadata_weak_mode(VM_A, true);
        let g4 = generation();
        assert!(g4 > g3, "a weak-mode change must tick");

        forget_vm_metadata_pins(VM_B);
        assert_eq!(generation(), g4, "VM B never had a row");
        forget_vm_metadata_pins(VM_A);
        assert!(generation() > g4, "a real teardown must tick");
        // Turning weak mode off drops VM A's rows and still ticks.
        add_metadata_pin(VM_A, 0x6300, 0x6308);
        let g5 = generation();
        set_metadata_weak_mode(VM_A, false);
        assert!(generation() > g5, "weak mode going off must tick");
        assert!(roots_for_loader(0x6300).is_none(), "and drop the VM's rows");
    }

    /// gc-common w8-e: a row past [`LINEAR_DEDUP_MAX`] switches to its hash
    /// index. It must still record each object once, keep insertion order, and
    /// tick the generation only for a new object, on both write paths.
    #[test]
    fn a_large_row_deduplicates_through_its_index_in_insertion_order() {
        const VM: usize = 0xA04;
        const LOADER: usize = 0x9000;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(VM);

        let n = LINEAR_DEDUP_MAX * 4 + 3;
        let objects: Vec<usize> = (0..n).map(|i| 0x10_0000 + i * 16).collect();
        for &o in &objects {
            add_metadata_pin(VM, LOADER, o);
        }
        let g = generation();
        // Every object again, both before and after the switch point.
        for &o in objects.iter().rev() {
            add_metadata_pin(VM, LOADER, o);
        }
        assert_eq!(generation(), g, "re-adding known objects is not a change");
        assert_eq!(roots_for_loader(LOADER), Some(objects.clone()));

        add_metadata_pin(VM, LOADER, 0x20_0000);
        assert!(generation() > g, "a new object under a big row must tick");
        let mut expected = objects.clone();
        expected.push(0x20_0000);
        assert_eq!(roots_for_loader(LOADER), Some(expected));

        // The wholesale path, with every object listed twice.
        let entries: Vec<(usize, usize)> = objects
            .iter()
            .chain(objects.iter())
            .map(|&o| (LOADER, o))
            .collect();
        replace_metadata_pins(VM, &entries);
        assert_eq!(roots_for_loader(LOADER), Some(objects.clone()));
        let snap = snapshot().expect("a row exists");
        assert_eq!(snap.get(&LOADER), Some(&objects));

        forget_vm_metadata_pins(VM);
        assert!(roots_for_loader(LOADER).is_none());
    }

    /// The key-set reader must agree with [`snapshot`]'s keys and carry a
    /// generation taken under the same lock acquisition.
    #[test]
    fn the_paired_key_set_reader_agrees_with_the_values_snapshot() {
        const VM: usize = 0xA03;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(VM);
        add_metadata_pin(VM, 0x6200, 0x6208);
        let (owners, gen) = pinned_owner_addrs_with_generation();
        assert_eq!(gen, generation());
        let values = snapshot().expect("just registered a row");
        let mut a = owners;
        let mut b: Vec<usize> = values.keys().copied().collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
        forget_vm_metadata_pins(VM);
    }
}
