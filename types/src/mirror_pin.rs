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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Monotonically increasing version of this registry's CONTENTS.
///
/// *Added for
/// `docs/internal/zgc-round-20260920/gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`
/// and its continuation `handoff-i-arm-root-filter-concurrently.md`. Same
/// contract, word for word, as [`crate::loader_pin`]'s — read that one for the
/// argument.*
///
/// A concurrent marker snapshots this table's OWNER key set into a lock-free
/// filter and then answers "that object owns no mirrors" from it, without a
/// lock, once per marked object. That answer is a **proof**, and it stops being
/// one the moment a mutator defines a class. [`NON_EMPTY`] cannot say so — it
/// answers "has anything ever been registered" — so this is the signal that can.
///
/// Bumped inside the same write lock that mutates the map, only when the map
/// really changed, and read paired with the key set under the read lock
/// ([`pinned_owner_addrs_with_generation`]) so the pair cannot straddle a
/// mutation. An unchanged value therefore proves the key set is unchanged.
///
/// Note it versions the contents, not just the key set: [`add_mirror_pin`]
/// appending a mirror to an EXISTING loader's row ticks it too. A consumer that
/// only cares about the keys may over-discard; one that cached the values needs
/// exactly this.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current contents version. See [`GENERATION`].
///
/// `0` means nothing has ever been registered.
#[inline]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// [`pinned_owner_addrs`] paired **exactly** with the [`generation`] it was
/// read at — both under one acquisition of the read lock, so the pair cannot
/// straddle a mutation.
///
/// Reading the latch or the generation *before* taking the lock is precisely
/// the ordering that turns this from a validity signal into a false proof: a
/// row added mid-walk would be reported with a generation that already counts
/// it, yet be absent from the returned vector. This runs once per collection,
/// so the lock costs nothing worth saving.
pub fn pinned_owner_addrs_with_generation() -> (Vec<usize>, u64) {
    let g = store().read();
    let gen = GENERATION.load(Ordering::Acquire);
    (g.keys().copied().collect(), gen)
}

/// Per-VM mirror addresses that must NEVER be pinned to their defining loader:
/// the mirrors of non-strong hidden classes (JEP 371, gc-common w18-d).
///
/// A hidden class defined without `ClassOption.STRONG` lives exactly as long
/// as its mirror and its instances, not as long as its loader. Its instances
/// keep the MIRROR alive instead (its `loader_pin` row names the mirror, see
/// `native-builtins::classloader::register_non_strong_hidden_mirror`), and the
/// mirror keeps the loader alive through its `classLoader` heap field. A
/// `(loader, mirror)` row here would add the reverse edge, loader -> mirror,
/// and a live loader (an application or platform loader never dies) would
/// then keep every such class loaded for the life of the VM, which is the
/// defect this set closes.
///
/// The two writers of this registry do not know which classes are hidden:
/// `vm::vm_object::get_or_create_class_mirror` ([`add_mirror_pin`]) and
/// `vm::memory::gc::rebuild_mirror_pins` ([`replace_mirror_pins`]) pin every
/// mirror of a class with a recorded user defining loader. So the filter is
/// applied here, where every row goes in. The addresses are current: the
/// owner re-publishes them (post-remap) from its post-GC reconcile, which runs
/// before `rebuild_mirror_pins` in the same epilogue
/// ([`replace_excluded_mirrors`]).
fn excluded() -> &'static RwLock<FxHashMap<usize, rustc_hash::FxHashSet<usize>>> {
    static INSTANCE: OnceLock<RwLock<FxHashMap<usize, rustc_hash::FxHashSet<usize>>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// Latch: has any VM ever excluded a mirror? Keeps [`add_mirror_pin`] and
/// [`replace_mirror_pins`] free of a second lock in every run that never
/// defines a non-strong hidden class.
static ANY_EXCLUDED: AtomicBool = AtomicBool::new(false);

/// Is `mirror_addr` one of `vm`'s excluded mirrors?
#[inline]
fn is_excluded(vm: usize, mirror_addr: usize) -> bool {
    if !ANY_EXCLUDED.load(Ordering::Acquire) {
        return false;
    }
    excluded()
        .read()
        .get(&vm)
        .is_some_and(|set| set.contains(&mirror_addr))
}

/// Never pin `mirror_addr` (a non-strong hidden class's mirror in `vm`'s heap)
/// to a loader, and drop the row [`add_mirror_pin`] may already have added
/// under `loader_addr` when the mirror was created.
///
/// `loader_addr` is the class's defining loader, when it has one: the only
/// row that can name this mirror (`get_or_create_class_mirror` pins a mirror
/// to its class's defining loader and to nothing else). `None` touches no row.
pub fn exclude_mirror(vm: usize, mirror_addr: usize, loader_addr: Option<usize>) {
    excluded()
        .write()
        .entry(vm)
        .or_default()
        .insert(mirror_addr);
    ANY_EXCLUDED.store(true, Ordering::Release);
    let Some(loader_addr) = loader_addr else {
        return;
    };
    let mut g = store().write();
    let mut changed = false;
    let mut now_empty = false;
    if let Some((owner, mirrors)) = g.get_mut(&loader_addr) {
        if *owner == vm {
            let before = mirrors.len();
            mirrors.retain(|&m| m != mirror_addr);
            changed = mirrors.len() != before;
            now_empty = mirrors.is_empty();
        }
    }
    if now_empty {
        g.remove(&loader_addr);
    }
    if changed {
        GENERATION.fetch_add(1, Ordering::Release);
    }
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

/// Replace `vm`'s excluded set with `mirror_addrs`, the current (post-remap)
/// addresses of its live non-strong hidden classes' mirrors. Called by the
/// owner's post-GC reconcile and when classes unload.
pub fn replace_excluded_mirrors(vm: usize, mirror_addrs: &[usize]) {
    let mut g = excluded().write();
    if mirror_addrs.is_empty() {
        g.remove(&vm);
        return;
    }
    g.insert(vm, mirror_addrs.iter().copied().collect());
    ANY_EXCLUDED.store(true, Ordering::Release);
}

/// Record that `loader_addr` (a user-defined `ClassLoader`'s current heap
/// address, in `vm`'s heap) defined the class whose mirror lives at
/// `mirror_addr`.
///
/// A mirror [`exclude_mirror`] names is not recorded (gc-common w18-d).
pub fn add_mirror_pin(vm: usize, loader_addr: usize, mirror_addr: usize) {
    if is_excluded(vm, mirror_addr) {
        return;
    }
    let mut g = store().write();
    let row = g.entry(loader_addr).or_insert_with(|| (vm, Vec::new()));
    row.0 = vm;
    row.1.push(mirror_addr);
    GENERATION.fetch_add(1, Ordering::Release);
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

/// Whole-registry snapshot, `loader address -> mirror addresses`, for a marker
/// that would otherwise call [`mirrors_for_loader`] per scanned object.
///
/// The per-object form is worse than [`crate::loader_pin::loader_pin_addr`]'s:
/// besides the process-global `RwLock`, it clones a `Vec` on every call that
/// finds a row. See [`crate::metadata_pin::snapshot`] for the full argument and
/// [`crate::loader_pin::snapshot`] for the freshness condition a snapshot
/// caller has to meet (the values are heap addresses, so the caller's view of
/// the heap must be frozen for as long as it holds one).
pub fn snapshot() -> Option<FxHashMap<usize, Vec<usize>>> {
    if !NON_EMPTY.load(Ordering::Relaxed) {
        return None;
    }
    let g = store().read();
    if g.is_empty() {
        None
    } else {
        Some(g.iter().map(|(&k, (_vm, v))| (k, v.clone())).collect())
    }
}

/// Every pinned mirror address, for a marker that cannot ask per loader.
///
/// See [`crate::loader_pin::all_pinned_loaders`] for why a generational young
/// cycle needs more than the per-loader form: it never visits an old loader,
/// so it never reaches the per-loader lookup, and a mirror reachable only that
/// way would be swept while its class is live. (The Generational non-moving
/// young marker closes that with an exact seed instead of this wholesale
/// form: it follows the [`snapshot`] rows of every OLD owner —
/// `gen_heap::seed_mirror_pins_of_old_owners`, 2026-09-23.)
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
///
/// Entries naming one of `vm`'s excluded mirrors ([`exclude_mirror`]) are
/// skipped (gc-common w18-d).
pub fn replace_mirror_pins(vm: usize, entries: &[(usize, usize)]) {
    // Read the excluded set BEFORE the registry lock: the two locks are never
    // held together.
    let skip: Option<rustc_hash::FxHashSet<usize>> = if ANY_EXCLUDED.load(Ordering::Acquire) {
        excluded().read().get(&vm).cloned()
    } else {
        None
    };
    let mut g = store().write();
    g.retain(|_, (owner, _)| *owner != vm);
    for &(loader_addr, mirror_addr) in entries {
        if skip.as_ref().is_some_and(|s| s.contains(&mirror_addr)) {
            continue;
        }
        let row = g.entry(loader_addr).or_insert_with(|| (vm, Vec::new()));
        row.0 = vm;
        row.1.push(mirror_addr);
    }
    // Unconditional: a wholesale post-GC replacement moves every address it
    // touches, which is a content change even when the key set is identical.
    GENERATION.fetch_add(1, Ordering::Release);
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

/// Drop every row `vm` owns, at that VM's teardown.
///
/// This replaces a blanket `clear()` that ran at VM *creation*, where the only
/// rows present belong to a concurrently-live VM — see [`store`].
pub fn forget_vm_mirror_pins(vm: usize) {
    // The excluded set is this VM's too (gc-common w18-d). Its own lock,
    // released before the registry's.
    excluded().write().remove(&vm);
    let mut g = store().write();
    let before = g.len();
    g.retain(|_, (owner, _)| *owner != vm);
    if g.len() != before {
        GENERATION.fetch_add(1, Ordering::Release);
    }
    NON_EMPTY.store(!g.is_empty(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One process-global map and one process-global [`GENERATION`]: a
    /// generation assertion is about the WHOLE table, so a concurrently-running
    /// test that registers its own row would tick it. Mirrors `metadata_pin`'s
    /// own test lock.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn teardown_drops_only_the_torn_down_vms_rows() {
        const VM_A: usize = 0xA00;
        const VM_B: usize = 0xB00;
        let _g = TEST_LOCK.lock();
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

    /// **A registration a concurrent marker cannot see must be a registration
    /// it can DETECT.**
    ///
    /// The extra-root filter snapshots the owner key set and then answers "this
    /// object owns no mirrors" from it without a lock. A mutator that adds a
    /// row without ticking [`GENERATION`] leaves that answer looking valid
    /// while it is false, and the mirror is swept with its class still live.
    /// Adding a mirror to an owner that is ALREADY in the table ticks it too:
    /// the filter's key set is unchanged there, but a consumer that cached the
    /// values is stale, and the cheap conservative rule is one counter for the
    /// contents.
    #[test]
    fn the_generation_ticks_for_every_real_mutation_and_only_those() {
        const VM_A: usize = 0xA02;
        const VM_B: usize = 0xB02;
        let _g = TEST_LOCK.lock();
        forget_vm_mirror_pins(VM_A);
        forget_vm_mirror_pins(VM_B);

        let g0 = generation();
        add_mirror_pin(VM_A, 0x5000, 0x5008);
        let g1 = generation();
        assert!(g1 > g0, "a new owner must tick");

        add_mirror_pin(VM_A, 0x5000, 0x5010);
        let g2 = generation();
        assert!(
            g2 > g1,
            "a new mirror under an existing owner must tick too"
        );

        replace_mirror_pins(VM_A, &[(0x5100, 0x5108)]);
        let g3 = generation();
        assert!(g3 > g2, "a post-GC wholesale replacement must tick");

        // A teardown that drops nothing is not a change.
        forget_vm_mirror_pins(VM_B);
        assert_eq!(generation(), g3, "VM B never had a row");
        forget_vm_mirror_pins(VM_A);
        assert!(generation() > g3, "a real teardown must tick");
    }

    /// gc-common w18-d: a non-strong hidden class's mirror is never pinned to
    /// its loader -- not by the row the mirror's creation already added, not
    /// by a later `add_mirror_pin`, and not by the post-GC rebuild -- while
    /// the loader's OTHER mirrors and another VM's rows are untouched. The
    /// exclusion follows the re-published addresses and dies with the VM.
    #[test]
    fn an_excluded_mirror_is_never_pinned_to_its_loader() {
        const VM_A: usize = 0xA04;
        const VM_B: usize = 0xB04;
        let _g = TEST_LOCK.lock();
        forget_vm_mirror_pins(VM_A);
        forget_vm_mirror_pins(VM_B);

        const LOADER: usize = 0x6000;
        const HIDDEN: usize = 0x6008;
        const PLAIN: usize = 0x6010;
        add_mirror_pin(VM_A, LOADER, HIDDEN);
        add_mirror_pin(VM_A, LOADER, PLAIN);
        // Another VM's row naming the same address must survive A's exclusion.
        add_mirror_pin(VM_B, 0x7000, HIDDEN);

        let g0 = generation();
        exclude_mirror(VM_A, HIDDEN, Some(LOADER));
        assert!(generation() > g0, "dropping a mirror from a row is a content change");
        assert_eq!(mirrors_for_loader(LOADER), Some(vec![PLAIN]));
        assert_eq!(mirrors_for_loader(0x7000), Some(vec![HIDDEN]), "VM B untouched");

        add_mirror_pin(VM_A, LOADER, HIDDEN);
        assert_eq!(mirrors_for_loader(LOADER), Some(vec![PLAIN]), "a later add is refused");

        replace_mirror_pins(VM_A, &[(LOADER, HIDDEN), (LOADER, PLAIN)]);
        assert_eq!(
            mirrors_for_loader(LOADER),
            Some(vec![PLAIN]),
            "the post-GC rebuild skips it too"
        );

        // A move: the owner re-publishes the new address.
        const MOVED: usize = 0x6108;
        replace_excluded_mirrors(VM_A, &[MOVED]);
        replace_mirror_pins(VM_A, &[(LOADER, MOVED), (LOADER, HIDDEN), (LOADER, PLAIN)]);
        let mut got = mirrors_for_loader(LOADER).unwrap_or_default();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![HIDDEN, PLAIN],
            "the new address is excluded and the old one no longer is"
        );

        forget_vm_mirror_pins(VM_A);
        add_mirror_pin(VM_A, LOADER, MOVED);
        assert_eq!(
            mirrors_for_loader(LOADER),
            Some(vec![MOVED]),
            "teardown drops the VM's exclusions with its rows"
        );
        forget_vm_mirror_pins(VM_A);
        forget_vm_mirror_pins(VM_B);
    }

    /// The paired reader must hand back a key set and a generation taken under
    /// one lock acquisition -- the property the marker's proof rests on.
    #[test]
    fn the_paired_reader_agrees_with_the_unpaired_one() {
        const VM: usize = 0xA03;
        let _g = TEST_LOCK.lock();
        forget_vm_mirror_pins(VM);
        add_mirror_pin(VM, 0x5200, 0x5208);
        let (owners, gen) = pinned_owner_addrs_with_generation();
        assert_eq!(gen, generation());
        assert!(owners.contains(&0x5200));
        let mut a = owners;
        let mut b = pinned_owner_addrs();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
        forget_vm_mirror_pins(VM);
    }
}
