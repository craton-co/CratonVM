// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! G1's owner-edge treatment of the external-root (collection-overlay) side
//! tables.
//!
//! # What this replaces
//!
//! `vm/src/memory/native_roots.rs::scan_collection_overlays` used to root
//! EVERY element of EVERY overlay-backed collection (`LinkedList`,
//! `LinkedHashMap`, `TreeMap`, ... whose backing objects live in Rust side
//! tables) on every G1 pause and in every G1 marking cycle, because no G1
//! marker followed the owner -> overlay edge. Two consequences
//! (`docs/internal/gc/common-b-g1-roots-every-overlay-element-unconditionally-FIXED-20261005.md`):
//! an overlay whose elements reach back to their owner was never collectible
//! (a `LinkedHashMap` whose values hold the map kept itself alive forever), and
//! every young pause walked the whole overlay population.
//!
//! The VM now defers the overlays to G1 on every pause (`G1 => true` in that
//! arm), and G1 follows the edges itself, exactly as the Generational and ZGC
//! markers do, in the three places it decides liveness:
//!
//! 1. **Evacuation pauses** (young and mixed, serial and parallel, after the
//!    closure): an overlay owner OUTSIDE the collection set is live for the
//!    pause by the same rule that makes every non-CSet object a remembered-set
//!    source, so its elements are evacuated as roots. An owner INSIDE the CSet
//!    is followed only once it is itself forwarded; that is a fixpoint, because
//!    an element can reach another owner. Owners in the CSet that were never
//!    forwarded died this pause, and are reported to the VM's post-pause prune
//!    ([`G1Collector::take_overlay_condemned_owners`]) so a row cannot outlive
//!    its owner with dangling elements. A young pause does not read every
//!    row: only the rows written since the last pause (the provider's
//!    written-key record, `native-collections/src/overlay_rows.rs`) and the
//!    rows that held a young owner or reference (gci i44/cd: or a humongous
//!    reference, which the eager reclaim must be told is live) when it ended
//!    ([`OverlayRemSet`]). Mixed pauses, the first pause, and a pause after an
//!    evacuation failure read them all and rebuild that set.
//! 2. **Concurrent marking**: overlay rows change with no write barrier, so the
//!    marker cannot read them live — an element removed from an unscanned
//!    owner and stored into an already-scanned object would never be marked,
//!    the lost-object case SATB exists to prevent. Instead the initial-mark
//!    pause takes a SNAPSHOT of every owner's rows; the marker treats an
//!    owner's snapshot rows as extra fields when it scans the owner. That is
//!    the SATB contract applied to the side table: everything reachable at the
//!    snapshot is marked, and anything added later is either allocated after
//!    the snapshot (above TAMS) or was reachable at the snapshot through some
//!    other preserved edge. The snapshot is carried across every evacuation
//!    pause of the cycle like the gray set: its values are kept alive (not
//!    grayed) and its keys and values are re-addressed through the pause's
//!    pointer map.
//! 3. **Cleanup**: after the remark drain (and remark-time resurrection), the
//!    rows of every owner the completed mark proves dead are pruned BEFORE
//!    cleanup can free the elements' regions — but only when the mark is
//!    trusted, the same gate cleanup uses for its own frees.

use super::*;

/// Words in [`G1Collector::overlay_snapshot_classes`]: 65,536 class ids, the
/// same bound `native-collections` uses for its owner-class gate.
pub(super) const OVERLAY_CLASS_WORDS: usize = 1024;

/// Shards of [`OverlayPrepass`]'s copy map.
const COPIED_SHARDS: usize = 64;

/// Overlay roots of NON-CSet owners (and snapshot values) that evacuation
/// pauses copied. Diagnostic census, never read on a decision path.
static OVERLAY_ROOT_COPIES: AtomicU64 = AtomicU64::new(0);
/// CSet owners whose edges an evacuation pause followed (forwarded owners).
static OVERLAY_OWNERS_FOLLOWED: AtomicU64 = AtomicU64::new(0);
/// CSet owners an evacuation pause found dead (never forwarded).
static OVERLAY_OWNERS_CONDEMNED: AtomicU64 = AtomicU64::new(0);
/// Owners captured by initial-mark snapshots.
static OVERLAY_SNAPSHOT_OWNERS: AtomicU64 = AtomicU64::new(0);
/// Snapshot edges the marker grayed (values handed to its enqueue).
static OVERLAY_SNAPSHOT_EDGES: AtomicU64 = AtomicU64::new(0);
/// Rows the post-mark prune was run for (trusted cycles only).
static OVERLAY_POST_MARK_PRUNES: AtomicU64 = AtomicU64::new(0);
/// Cycles whose post-mark prune was skipped because the mark was untrusted.
static OVERLAY_POST_MARK_PRUNES_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Evacuation pauses that ran the overlay phase, and the nanoseconds it took
/// them (begin + follow, i.e. enumeration, rooting and the overlay closure).
static OVERLAY_PAUSES: AtomicU64 = AtomicU64::new(0);
static OVERLAY_PAUSE_NS: AtomicU64 = AtomicU64::new(0);
/// CSet owners expanded inside the parallel closure (`SharedEvac::expand_overlay_owner`).
pub(super) static OVERLAY_PARALLEL_EXPANDED: AtomicU64 = AtomicU64::new(0);

/// The process-wide overlay census, for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct G1OverlayCensus {
    pub root_copies: u64,
    pub owners_followed: u64,
    pub owners_condemned: u64,
    pub snapshot_owners: u64,
    pub snapshot_edges: u64,
    pub post_mark_prunes: u64,
    pub post_mark_prunes_skipped: u64,
    pub pauses: u64,
    pub pause_ns: u64,
    /// CSet owners whose rows a parallel evacuation worker expanded.
    pub parallel_expanded: u64,
}

pub fn g1_overlay_census() -> G1OverlayCensus {
    G1OverlayCensus {
        root_copies: OVERLAY_ROOT_COPIES.load(Ordering::Relaxed),
        owners_followed: OVERLAY_OWNERS_FOLLOWED.load(Ordering::Relaxed),
        owners_condemned: OVERLAY_OWNERS_CONDEMNED.load(Ordering::Relaxed),
        snapshot_owners: OVERLAY_SNAPSHOT_OWNERS.load(Ordering::Relaxed),
        snapshot_edges: OVERLAY_SNAPSHOT_EDGES.load(Ordering::Relaxed),
        post_mark_prunes: OVERLAY_POST_MARK_PRUNES.load(Ordering::Relaxed),
        post_mark_prunes_skipped: OVERLAY_POST_MARK_PRUNES_SKIPPED.load(Ordering::Relaxed),
        pauses: OVERLAY_PAUSES.load(Ordering::Relaxed),
        pause_ns: OVERLAY_PAUSE_NS.load(Ordering::Relaxed),
        parallel_expanded: OVERLAY_PARALLEL_EXPANDED.load(Ordering::Relaxed),
    }
}

/// `CRATONVM_DBG_G1_OVERLAY=1`: one `[g1][overlay]` line per evacuation pause
/// (owners, roots, follows, condemned, microseconds). Read once per process.
fn dbg_overlay() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_G1_OVERLAY"))
}

/// The young-pause remembered set over overlay rows.
///
/// # What it remembers, and why that is enough
///
/// A young pause needs every overlay reference INTO the young generation. A
/// row can hold one only if (a) a mutator wrote the row since the last pause
/// (the provider's written-key record,
/// [`crate::external_roots::OwnerRowCallbacks::take_dirty_keys`]), or (b) the
/// row already held one when the last pause ended — its owner or one of its
/// references was young then, and an untouched row cannot gain a young
/// reference. This set is (b); (a) is read fresh each pause. Every row outside
/// both holds only old references and an old owner, so a young pause has
/// nothing to learn from it, and does not read it -- except a humongous
/// reference, which is why (b) also holds those rows (gci i44/cd, below).
///
/// It is rebuilt from a FULL read of every row whenever it cannot be trusted:
/// the first pause (and the one after the provider record is switched on, as
/// writes before that were not recorded), every mixed pause (its collection set
/// has old regions, which rows of OLD owners can name), a pause after an
/// evacuation failure (the drain moves objects after this set was computed),
/// and a pause after the provider table changed.
///
/// # gci i44/cd -- and every row that holds a HUMONGOUS reference
///
/// A young pause has a second thing to learn from a row: the eager humongous
/// reclaim (`eager_reclaim_humongous_locked`) judges a span by what THIS pause
/// names live -- heap slots it scanned, the span's own remembered set, roots,
/// and the overlay values the pause read ([`G1Collector::overlay_live_addrs_this_pause`]).
/// An overlay row is not a heap slot, so it adds no remembered-set entry to the
/// span it holds. A row with an OLD owner and a humongous value (a long-lived
/// `TreeMap<K, byte[]>` whose values are half a region or more) was read by the
/// pause after its write and then never again on a young pause: nothing named
/// the span live, and the next young pause outside a mark cycle freed it under
/// the row (a use-after-free the VM's post-pause prune cannot see, as the
/// owner is alive). So such a row is remembered too, and re-read by every young
/// pause, which names its humongous values live. Kill switch
/// `CRATONVM_G1_OVERLAY_REMEMBER_HUMONGOUS=0` (default ON, latched per
/// collector in [`Self::remember_humongous`]) restores the young-only rule.
#[derive(Default)]
pub(super) struct OverlayRemSet {
    /// `(provider index, row key)` of every row whose owner or a reference of
    /// which was young (or, see the type docs, humongous) when the last pause
    /// ended.
    /// std hasher, not Fx: the keys are packed object keys whose low half
    /// barely varies (see `native-collections` `OverlayRows::dirty`).
    keys: std::collections::HashSet<(usize, usize)>,
    /// Whether `keys` is complete for the next young pause.
    valid: bool,
    /// The provider-table generation `keys` was computed under.
    generation: u64,
    /// Whether this collector has switched the providers' record on.
    tracking: bool,
    /// gci i44/cd -- `CRATONVM_G1_OVERLAY_REMEMBER_HUMONGOUS` (default ON),
    /// read once per collector by the first rebuild and kept here so no
    /// process global is added. `None` until then.
    remember_humongous: Option<bool>,
}

/// gci i44/cd (lane ch's H1) -- must this pause switch the providers' written-key
/// record on? On the collector's first row read, and whenever the provider table
/// changed since the remembered set was last rebuilt (`remembered_generation`):
/// a provider registered in between was never enabled, and its
/// `take_dirty_keys` is incomplete by contract until it is. Pure.
#[inline]
pub(super) fn overlay_tracking_needs_enable(
    tracking: bool,
    remembered_generation: u64,
    generation: u64,
) -> bool {
    !tracking || remembered_generation != generation
}

/// gci i44/cd -- does a row holding `kind` have to be read by the next young
/// pause? A young object (it may move, and the row must be re-addressed), and,
/// with `remember_humongous`, a humongous one (the eager reclaim must be told
/// it is live; see [`OverlayRemSet`]). Pure, for the unit test.
#[inline]
pub(super) fn overlay_kind_pins_young_pause(kind: RegionType, remember_humongous: bool) -> bool {
    match kind {
        RegionType::Eden | RegionType::Survivor => true,
        RegionType::HumongousStart | RegionType::HumongousContinuation => remember_humongous,
        RegionType::Free | RegionType::Old => false,
    }
}

/// The mark snapshot, split by whether an evacuation pause can touch the
/// entry: `young` holds every entry whose owner or a value sits in a young
/// region, `old` the rest. A young pause roots and re-addresses `young` only —
/// an old object never moves in a young pause and never becomes young — so its
/// cost follows the young part of the snapshot, not the whole of it.
#[derive(Default)]
pub(super) struct OverlaySnapshot {
    young: FxHashMap<usize, Vec<usize>>,
    old: FxHashMap<usize, Vec<usize>>,
}

impl OverlaySnapshot {
    fn len(&self) -> usize {
        self.young.len() + self.old.len()
    }
    fn get(&self, owner: usize) -> Option<&Vec<usize>> {
        self.young.get(&owner).or_else(|| self.old.get(&owner))
    }
    fn clear(&mut self) {
        self.young.clear();
        self.old.clear();
    }
    fn insert(&mut self, young: bool, owner: usize, values: Vec<usize>) {
        let map = if young {
            &mut self.young
        } else {
            &mut self.old
        };
        map.entry(owner).or_default().extend(values);
    }
    /// The values of `owner`'s entry, for tests.
    #[cfg(test)]
    pub(super) fn values_of(&self, owner: usize) -> Option<Vec<usize>> {
        self.get(owner).cloned()
    }
}

/// One row an evacuation pause read.
struct PauseRow {
    /// `(provider, key)` when read through the row API (needed to remember
    /// it); `None` on the per-owner fallback.
    key: Option<(usize, usize)>,
    owner: usize,
    /// The row's references at pause start; `None` on the fallback, which
    /// fetches them per owner when the owner is found forwarded.
    refs: Option<Vec<usize>>,
    in_cset: bool,
    followed: bool,
}

/// The overlay rows a pause read before its closure; see
/// [`G1Collector::overlay_prepare`].
pub(super) struct OverlayPrepass {
    /// Whether anything is to be done (a provider or an open snapshot).
    active: bool,
    /// References that are roots this pause.
    pub(super) roots: Vec<usize>,
    /// Pre-pause address of every CSet owner read through the row API -> the
    /// index in `rows` of its newest row; `next_row` chains an owner's other
    /// rows (`u32::MAX` ends a chain). Indices, not copies: a pause over a
    /// large young overlay population would otherwise clone every row.
    owner_row: FxHashMap<usize, u32>,
    next_row: Vec<u32>,
    /// Owner copies made by the parallel closure: copy address -> pre-pause
    /// address, sharded by copy address. Written by the worker that copies an
    /// owner, taken by the worker that scans the copy.
    copied: Vec<Mutex<FxHashMap<usize, usize>>>,
    /// Per row of `rows`: whether a worker expanded it (its owner was copied
    /// and scanned inside the closure), so the serial fixpoint can skip it.
    expanded: Vec<AtomicBool>,
    /// Class ids of those owners, as a bitmap: the evacuator's per-copy gate.
    classes: Vec<u64>,
    rows: Vec<PauseRow>,
    remember: Option<u64>,
    incremental: bool,
    spent: std::time::Duration,
}

impl Default for OverlayPrepass {
    fn default() -> Self {
        Self {
            active: false,
            roots: Vec::new(),
            owner_row: FxHashMap::default(),
            next_row: Vec::new(),
            expanded: Vec::new(),
            copied: (0..COPIED_SHARDS)
                .map(|_| Mutex::new(FxHashMap::default()))
                .collect(),
            classes: vec![0; OVERLAY_CLASS_WORDS],
            rows: Vec::new(),
            remember: None,
            incremental: false,
            spent: std::time::Duration::ZERO,
        }
    }
}

impl OverlayPrepass {
    /// Whether a freshly copied object of `class_id` may be a CSet owner with
    /// rows: one load and a mask, for the evacuator's every copy.
    #[inline]
    pub(super) fn may_own(&self, class_id: u32) -> bool {
        let c = class_id as usize;
        !self.owner_row.is_empty()
            && self.classes[(c / 64) % OVERLAY_CLASS_WORDS] & (1u64 << (c % 64)) != 0
    }

    /// Remember that the CSet owner at pre-pause address `old` (if it is one)
    /// was copied to `new`, for the worker that scans the copy.
    pub(super) fn note_copied(&self, old: usize, new: usize) {
        if self.owner_row.contains_key(&old) {
            self.copied[(new >> 4) % COPIED_SHARDS]
                .lock()
                .insert(new, old);
        }
    }

    /// The pre-pause address of the owner whose copy is at `new`, once.
    pub(super) fn take_copied(&self, new: usize) -> Option<usize> {
        self.copied[(new >> 4) % COPIED_SHARDS].lock().remove(&new)
    }

    /// Call `f` with every reference of every row `owner` has this pause, and
    /// mark those rows expanded.
    pub(super) fn expand_owner_refs(&self, owner: usize, mut f: impl FnMut(usize)) -> bool {
        let Some(&first) = self.owner_row.get(&owner) else {
            return false;
        };
        let mut i = first;
        while i != u32::MAX {
            if let Some(refs) = self.rows.get(i as usize).and_then(|r| r.refs.as_ref()) {
                refs.iter().copied().for_each(&mut f);
            }
            if let Some(flag) = self.expanded.get(i as usize) {
                flag.store(true, Ordering::Relaxed);
            }
            i = self.next_row.get(i as usize).copied().unwrap_or(u32::MAX);
        }
        true
    }
}

/// One evacuation pause's overlay bookkeeping, between
/// [`G1Collector::overlay_pause_begin`] and [`G1Collector::overlay_pause_finish`].
pub(super) struct OverlayPause {
    /// Time spent in the overlay phase so far (begin plus every follow,
    /// not the Phase 3.5 work between them).
    spent: std::time::Duration,
    /// Of `spent`: the row read before the closure, and `finish`.
    read: std::time::Duration,
    /// References rooted (rows of non-CSet owners plus snapshot values), for
    /// the debug line.
    roots: usize,
    /// Rows read this pause, CSet-owned ones included.
    rows: Vec<PauseRow>,
    /// `Some(provider generation)` when the rows came through the row API, so
    /// `finish` can rebuild the remembered set.
    remember: Option<u64>,
    /// Whether this pause read only the written and remembered rows.
    incremental: bool,
    /// The pause's private Cheney list. Kept apart from the driver's own list
    /// so the overlay closure never re-scans what the main closure scanned.
    work_list: Vec<*mut u8>,
}

impl G1Collector {
    #[inline]
    fn overlay_in_cset(&self, cset: &RegionSet, addr: usize) -> bool {
        self.lookup_region_for_addr(addr)
            .is_some_and(|idx| cset.contains(&idx))
    }

    #[inline]
    fn overlay_in_heap(&self, addr: usize) -> bool {
        addr >= self.arena_base && addr < self.arena_end
    }

    /// Whether `addr` lies in a young (Eden or Survivor) region.
    #[inline]
    fn overlay_is_young(&self, regions: &[G1Region], addr: usize) -> bool {
        self.lookup_region_for_addr(addr)
            .and_then(|idx| regions.get(idx))
            .is_some_and(|r| matches!(r.region_type(), RegionType::Eden | RegionType::Survivor))
    }

    /// gci i44/cd -- [`overlay_kind_pins_young_pause`] for the object at
    /// `addr`: must the next young pause read a row that holds it?
    #[inline]
    fn overlay_pins_young_pause(
        &self,
        regions: &[G1Region],
        addr: usize,
        remember_humongous: bool,
    ) -> bool {
        self.lookup_region_for_addr(addr)
            .and_then(|idx| regions.get(idx))
            .is_some_and(|r| overlay_kind_pins_young_pause(r.region_type(), remember_humongous))
    }

    /// Record the overlay values this pause kept live that the eager
    /// humongous reclaim must see: only those in a humongous region (an
    /// overlay row is not a heap slot, so nothing else names such a span).
    /// Every other value would only cost the reclaim a lookup — half a million
    /// of them on a pause over a large young overlay population.
    fn note_overlay_live(&self, regions: &[G1Region], addrs: impl Iterator<Item = usize>) {
        let mut live = None;
        for addr in addrs {
            let humongous = self
                .lookup_region_for_addr(addr)
                .and_then(|idx| regions.get(idx))
                .is_some_and(|r| {
                    matches!(
                        r.region_type(),
                        RegionType::HumongousStart | RegionType::HumongousContinuation
                    )
                });
            if humongous {
                live.get_or_insert_with(|| self.overlay_live_this_pause.lock())
                    .push(addr);
            }
        }
    }

    /// Mark the overlay remembered set stale: the next pause reads every row.
    pub(super) fn overlay_rset_invalidate(&self) {
        self.overlay_rset.lock().valid = false;
    }

    /// Evacuate `addr` as a root of the overlay phase: no-op outside the CSet,
    /// an already-forwarded object is answered from its forward, and a fresh
    /// copy is queued for the overlay closure.
    #[allow(clippy::too_many_arguments)]
    fn overlay_evacuate_root(
        &self,
        regions: &mut Vec<G1Region>,
        cset_set: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
        site: &'static str,
        addr: usize,
    ) {
        if addr == 0 || !self.overlay_in_cset(cset_set, addr) {
            return;
        }
        if pointer_map.contains_key(&addr) {
            return;
        }
        // The header screen a reference SLOT gets (`scan_and_evacuate_refs`), not
        // the grid-proving supply screen: an overlay value is a typed reference
        // like a field value, not a derived address, and the grid proof walks
        // the region per call (seconds per pause over a 60k-collection young
        // population, measured).
        if !self.evacuation_candidate_is_an_object(regions, site, std::ptr::null_mut(), 0, addr) {
            return;
        }
        if let Some((new_ptr, fresh)) = self.evacuate_object(
            regions,
            addr as *mut u8,
            pointer_map,
            objects_copied,
            bytes_copied,
            cset_set,
        ) {
            if fresh {
                OVERLAY_ROOT_COPIES.fetch_add(1, Ordering::Relaxed);
                work_list.push(new_ptr);
            }
        }
    }

    /// Read this pause's rows. A provider with the row API is read through it:
    /// only its written and remembered rows on a young pause with a valid
    /// remembered set, all of them otherwise. A provider without one is read
    /// per owner, whole, every pause. Returns the rows, the provider generation
    /// to remember the row-API rows under, and whether the read was
    /// incremental.
    fn overlay_read_rows(
        &self,
        cset_set: &RegionSet,
        young_only: bool,
    ) -> (Vec<PauseRow>, Option<u64>, bool) {
        use crate::external_roots as er;
        let mut rows: Vec<PauseRow> = Vec::new();
        if er::has_rowless_providers() {
            let (owners, complete) = er::rowless_owner_addrs_and_completeness();
            let refs: Vec<usize> = if complete {
                rows.extend(
                    owners
                        .into_iter()
                        .filter(|&o| self.overlay_in_heap(o) && self.overlay_in_cset(cset_set, o))
                        .map(|owner| PauseRow {
                            key: None,
                            owner,
                            refs: None,
                            in_cset: true,
                            followed: false,
                        }),
                );
                // Every non-CSet owner's references, flat: this heap's owners
                // only (another VM's row can name only that VM's objects).
                er::rowless_roots_for_matching_owners(&|owner| {
                    self.overlay_in_heap(owner) && !self.overlay_in_cset(cset_set, owner)
                })
            } else {
                // A provider that cannot enumerate its owners cannot be
                // followed by owner, so every one of its rows is a root — the
                // treatment every row had before the marker followed edges.
                er::rowless_roots_for_matching_owners(&|_| true)
            }
            .into_iter()
            .map(|r| r.as_ptr() as usize)
            .collect();
            rows.push(PauseRow {
                key: None,
                owner: 0,
                refs: Some(refs),
                in_cset: false,
                followed: false,
            });
        }
        if !er::has_row_providers() {
            return (rows, None, false);
        }
        let generation = er::provider_table_generation();
        let mut rs = self.overlay_rset.lock();
        // gci i44/cd (lane ch's H1, page gci-i44ch-g1-overlay-late-row-provider-
        // never-tracks-dirty-keys): a provider registered since the set was
        // last rebuilt (the table generation moved) has never been told to
        // record its written keys. Enabling BEFORE this pause's read is what
        // makes the full read below (the generation mismatch forces one) cover
        // every write the new provider made before its record started.
        // Idempotent for the providers already recording.
        if overlay_tracking_needs_enable(rs.tracking, rs.generation, generation) {
            er::enable_owner_row_dirty_tracking();
            rs.tracking = true;
            rs.valid = false;
        }
        let incremental = young_only && rs.valid && rs.generation == generation;
        let in_heap = |owner: usize| self.overlay_in_heap(owner);
        // Always taken, so a full read also starts the next interval's record.
        let dirty = er::take_dirty_owner_row_keys(&in_heap);
        let read = if incremental {
            let mut keys: std::collections::HashSet<(usize, usize)> =
                rs.keys.iter().copied().collect();
            keys.extend(dirty);
            er::owner_rows_for_keys(&keys.into_iter().collect::<Vec<_>>())
        } else {
            er::all_owner_rows(&in_heap)
        };
        // Stale until `finish` rebuilds it from what this pause read.
        rs.valid = false;
        drop(rs);
        rows.extend(
            read.into_iter()
                .filter(|(_, row)| self.overlay_in_heap(row.owner))
                .map(|(provider, row)| PauseRow {
                    key: Some((provider, row.key)),
                    owner: row.owner,
                    in_cset: self.overlay_in_cset(cset_set, row.owner),
                    refs: Some(row.refs.iter().map(|r| r.as_ptr() as usize).collect()),
                    followed: false,
                }),
        );
        (rows, Some(generation), incremental)
    }

    /// Phase 3c of every evacuation driver: root the overlay references of
    /// every owner outside the collection set (plus, while a mark cycle is
    /// open, the mark snapshot's values), then follow forwarded CSet owners to
    /// a fixpoint. `young_only` is true on a young pause, whose collection set
    /// holds young regions only.
    ///
    /// Must run after the driver's closure is complete (so "forwarded" means
    /// "live this pause"), with the regions guard held and every parallel
    /// worker TLAB retired — the same state `resurrect_dead_finalizers` needs,
    /// and for the same reason: it evacuates through the serial
    /// `evacuate_object`.
    #[allow(clippy::too_many_arguments)]
    /// Read the pause's overlay rows BEFORE the closure: the roots (rows of
    /// non-CSet owners, plus the young — or, on a mixed pause, the whole —
    /// mark snapshot) and the rows of every CSet owner, so the PARALLEL
    /// evacuator can seed the roots and expand an owner's rows the moment a
    /// worker copies it (`SharedEvac::expand_overlay_owner`), instead of the
    /// serial phase after the closure copying a whole overlay-held young
    /// population on one thread. Nothing moves between this read and the
    /// closure: the world is stopped.
    pub(super) fn overlay_prepare(&self, cset_set: &RegionSet, young_only: bool) -> OverlayPrepass {
        let head = std::time::Instant::now();
        let mut prepass = OverlayPrepass::default();
        let snapshot_open = self.overlay_snapshot_len.load(Ordering::Acquire) != 0;
        if !crate::external_roots::has_providers() && !snapshot_open {
            return prepass;
        }
        prepass.active = true;
        if crate::external_roots::has_providers() {
            let (rows, remember, incremental) = self.overlay_read_rows(cset_set, young_only);
            prepass.next_row = vec![u32::MAX; rows.len()];
            prepass.expanded = (0..rows.len()).map(|_| AtomicBool::new(false)).collect();
            for (i, row) in rows.iter().enumerate() {
                match (&row.refs, row.in_cset) {
                    (Some(refs), false) => prepass.roots.extend(refs.iter().copied()),
                    (Some(_), true) => {
                        // SAFETY: a CSet owner, before the closure: its own
                        // header, unmoved.
                        let class_id = unsafe { &*(row.owner as *const ObjectHeader) }
                            .class_id
                            .as_u32() as usize;
                        prepass.classes[(class_id / 64) % OVERLAY_CLASS_WORDS] |=
                            1u64 << (class_id % 64);
                        if let Some(prev) = prepass.owner_row.insert(row.owner, i as u32) {
                            prepass.next_row[i] = prev;
                        }
                    }
                    (None, _) => {}
                }
            }
            prepass.rows = rows;
            prepass.remember = remember;
            prepass.incremental = incremental;
        }
        if snapshot_open {
            // SATB: a snapshot value is live-at-snapshot on behalf of an owner
            // the marker may not have scanned yet. Keep it alive across the
            // pause; the marker grays it when (if) it scans the owner. A young
            // pause can only move the young part.
            let snap = self.overlay_snapshot.read();
            for values in snap.young.values() {
                prepass.roots.extend(values.iter().copied());
            }
            if !young_only {
                for values in snap.old.values() {
                    prepass.roots.extend(values.iter().copied());
                }
            }
        }
        prepass.spent = head.elapsed();
        prepass
    }

    pub(super) fn overlay_pause_begin(
        &self,
        regions: &mut Vec<G1Region>,
        cset_set: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        prepass: OverlayPrepass,
    ) -> OverlayPause {
        let head = std::time::Instant::now();
        self.overlay_live_this_pause.lock().clear();
        let mut pause = OverlayPause {
            spent: prepass.spent,
            read: prepass.spent,
            roots: 0,
            rows: prepass.rows,
            remember: prepass.remember,
            incremental: prepass.incremental,
            work_list: Vec::new(),
        };
        if !prepass.active {
            return pause;
        }
        // Rows a worker already expanded: their owner was copied (so it is
        // live) and their references evacuated as its children. Only the
        // humongous ones still need naming for the eager reclaim.
        for (row, flag) in pause.rows.iter_mut().zip(prepass.expanded.iter()) {
            if row.in_cset && flag.load(Ordering::Relaxed) {
                row.followed = true;
                OVERLAY_OWNERS_FOLLOWED.fetch_add(1, Ordering::Relaxed);
                if let Some(refs) = &row.refs {
                    self.note_overlay_live(regions, refs.iter().copied());
                }
            }
        }
        let roots = prepass.roots;
        let mut work_list = std::mem::take(&mut pause.work_list);
        for &addr in &roots {
            self.overlay_evacuate_root(
                regions,
                cset_set,
                pointer_map,
                objects_copied,
                bytes_copied,
                &mut work_list,
                "overlay-root",
                addr,
            );
        }
        pause.roots = roots.len();
        self.note_overlay_live(regions, roots.iter().copied());
        pause.work_list = work_list;
        // The follow below charges itself.
        pause.spent += head.elapsed();
        self.overlay_pause_follow(
            &mut pause,
            regions,
            cset_set,
            pointer_map,
            objects_copied,
            bytes_copied,
        );
        pause
    }

    /// Drain the overlay closure, then follow every CSet owner that is now
    /// forwarded, until no new owner turns up. Re-entered after Phase 3.5,
    /// whose resurrected objects can be owners too.
    pub(super) fn overlay_pause_follow(
        &self,
        pause: &mut OverlayPause,
        regions: &mut Vec<G1Region>,
        cset_set: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
    ) {
        let t0 = std::time::Instant::now();
        let mut work_list = std::mem::take(&mut pause.work_list);
        let mut scan_idx = 0usize;
        let mut edges: Vec<usize> = Vec::new();
        loop {
            while scan_idx < work_list.len() {
                let obj_ptr = work_list[scan_idx];
                scan_idx += 1;
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                self.scan_and_evacuate_refs(
                    regions,
                    obj_ptr,
                    header,
                    cset_set,
                    pointer_map,
                    objects_copied,
                    bytes_copied,
                    &mut work_list,
                );
            }
            let mut progressed = false;
            for row in pause.rows.iter_mut() {
                if !row.in_cset || row.followed {
                    continue;
                }
                let Some(&new_addr) = pointer_map.get(&row.owner) else {
                    continue;
                };
                row.followed = true;
                progressed = true;
                OVERLAY_OWNERS_FOLLOWED.fetch_add(1, Ordering::Relaxed);
                edges.clear();
                match &row.refs {
                    Some(refs) => edges.extend(refs.iter().copied()),
                    None => {
                        // SAFETY: `new_addr` is this pause's forward of a CSet
                        // object, a copy (or a self-forward) the evacuator
                        // just wrote.
                        let class_id = unsafe { &*(new_addr as *const ObjectHeader) }
                            .class_id
                            .as_u32();
                        // A row-less provider's owner: its owner index is
                        // still keyed by the PRE-pause address (the VM re-keys
                        // it from this pause's pointer map afterwards).
                        let mut refs: Vec<ObjectRef> = Vec::new();
                        crate::external_roots::rowless_roots_for_owner_into(
                            row.owner,
                            Some(class_id),
                            &mut refs,
                        );
                        edges.extend(refs.iter().map(|r| r.as_ptr() as usize));
                    }
                }
                for &addr in &edges {
                    self.overlay_evacuate_root(
                        regions,
                        cset_set,
                        pointer_map,
                        objects_copied,
                        bytes_copied,
                        &mut work_list,
                        "overlay-owner-edge",
                        addr,
                    );
                }
                self.note_overlay_live(regions, edges.iter().copied());
            }
            if !progressed && scan_idx >= work_list.len() {
                break;
            }
        }
        work_list.clear();
        pause.work_list = work_list;
        pause.spent += t0.elapsed();
    }

    /// Close the pause's overlay phase: record every CSet owner it never
    /// forwarded as condemned (for the VM's post-pause prune), and rebuild the
    /// remembered set from the rows it read. Condemned owners accumulate across
    /// a collection's passes (the evacuation-failure drain does not run this,
    /// and has nothing to add: it only moves objects the first pass proved
    /// live).
    pub(super) fn overlay_pause_finish(
        &self,
        pause: OverlayPause,
        regions: &[G1Region],
        pointer_map: &cratonvm_types::PointerMap,
    ) {
        let t0 = std::time::Instant::now();
        let mut dead: FxHashSet<usize> = FxHashSet::default();
        for row in pause.rows.iter().filter(|r| r.in_cset && !r.followed) {
            dead.insert(row.owner);
        }
        let mut cset_rows = 0usize;
        let mut followed = 0usize;
        for row in pause.rows.iter().filter(|r| r.in_cset) {
            cset_rows += 1;
            followed += usize::from(row.followed);
        }
        let mut remembered = 0usize;
        if let Some(generation) = pause.remember {
            // gci i44/cd: the latch is read under the same lock the rebuild
            // publishes under, so it is read once per collector.
            let mut rs = self.overlay_rset.lock();
            let remember_humongous = *rs.remember_humongous.get_or_insert_with(|| {
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_G1_OVERLAY_REMEMBER_HUMONGOUS",
                )
            });
            // "young" keeps its pre-i44 name: with the switch on it also
            // answers yes for a humongous object (see `OverlayRemSet`).
            let young = |addr: usize| {
                let now = pointer_map.get(&addr).copied().unwrap_or(addr);
                self.overlay_pins_young_pause(regions, now, remember_humongous)
            };
            let mut keys: std::collections::HashSet<(usize, usize)> =
                std::collections::HashSet::new();
            for row in &pause.rows {
                let Some(key) = row.key else {
                    continue;
                };
                if row.in_cset && !row.followed {
                    continue; // died with its owner
                }
                let holds_young = young(row.owner)
                    || row
                        .refs
                        .as_ref()
                        .is_some_and(|refs| refs.iter().any(|&r| young(r)));
                if holds_young {
                    keys.insert(key);
                }
            }
            remembered = keys.len();
            rs.keys = keys;
            rs.generation = generation;
            rs.valid = true;
        }
        let finish_t = t0.elapsed();
        let spent = pause.spent + finish_t;
        let ns = spent.as_nanos() as u64;
        OVERLAY_PAUSES.fetch_add(1, Ordering::Relaxed);
        OVERLAY_PAUSE_NS.fetch_add(ns, Ordering::Relaxed);
        if dbg_overlay() {
            eprintln!(
                "[g1][overlay] mode={} rows={} cset_rows={cset_rows} followed={followed} \
                 condemned={} roots={} remembered={remembered} snapshot_owners={} us={} \
                 read_us={} finish_us={}",
                match (pause.remember.is_some(), pause.incremental) {
                    (true, true) => "incremental",
                    (true, false) => "full",
                    (false, _) => "per-owner",
                },
                pause.rows.len(),
                dead.len(),
                pause.roots,
                self.overlay_snapshot_len.load(Ordering::Relaxed),
                ns / 1000,
                pause.read.as_micros(),
                finish_t.as_micros(),
            );
        }
        if dead.is_empty() {
            return;
        }
        OVERLAY_OWNERS_CONDEMNED.fetch_add(dead.len() as u64, Ordering::Relaxed);
        self.overlay_condemned_owners.lock().extend(dead);
    }

    /// Owners the last collection found dead in its collection set, by their
    /// PRE-pause address. Taken (and so cleared) by the VM's post-pause overlay
    /// prune, which must drop them whatever the generic liveness answer says.
    ///
    /// Why the generic answer is not enough: it is `pointer_map` or
    /// `is_addr_in_live_region`, and a dead owner's region is not always free
    /// by then — an evacuation failure KEEPS a CSet region, and the drain that
    /// follows can claim a region this pause freed as its to-space. Either way
    /// the dead owner's address reads as live, its row survives, and its
    /// references (never evacuated: their owner was dead) dangle into reclaimed
    /// memory, to be rooted by the next pause.
    pub fn take_overlay_condemned_owners(&self) -> FxHashSet<usize> {
        std::mem::take(&mut *self.overlay_condemned_owners.lock())
    }

    /// Clear the condemned-owner set at the start of a collection.
    pub(super) fn overlay_collection_begin(&self) {
        self.overlay_condemned_owners.lock().clear();
    }

    /// The overlay-row values this pause treated as live (rooted or
    /// followed), for the eager humongous reclaim's live set.
    pub(super) fn overlay_live_addrs_this_pause(&self) -> Vec<usize> {
        self.overlay_live_this_pause.lock().clone()
    }

    // -- concurrent marking ---------------------------------------------------

    /// Capture the mark snapshot of every overlay row this heap owns. Called
    /// from `start_concurrent_mark`, stop-the-world, before any marker runs.
    pub(super) fn capture_overlay_snapshot(&self) {
        self.clear_overlay_snapshot();
        if !crate::external_roots::has_providers() {
            return;
        }
        let (owners, complete) = crate::external_roots::owner_addrs_and_completeness();
        let mut snap = OverlaySnapshot::default();
        let mut unowned: Vec<usize> = Vec::new();
        {
            let regions = self.regions.read();
            for owner in owners {
                let Some(idx) = self.lookup_region_for_addr(owner) else {
                    continue; // another VM's owner
                };
                let Some(region) = regions.get(idx) else {
                    continue;
                };
                if region.region_type() == RegionType::Free {
                    continue;
                }
                let base = region.data.as_ptr() as usize;
                if owner.wrapping_sub(base) >= region.cursor() {
                    continue;
                }
                // SAFETY: inside a non-Free region below its cursor. Every
                // overlay row's owner is a collection object the row's death
                // paths prune before its storage is reused (evacuation pauses
                // condemn dead CSet owners; cleanup prunes by the mark), so
                // this is the owner's own header.
                let class_id = unsafe { &*(owner as *const ObjectHeader) }
                    .class_id
                    .as_u32();
                let refs: Vec<usize> =
                    crate::external_roots::external_roots_for_owner(owner, Some(class_id))
                        .into_iter()
                        .map(|r| r.as_ptr() as usize)
                        .filter(|&r| r != 0)
                        .collect();
                if refs.is_empty() {
                    continue;
                }
                let word = (class_id as usize / 64) % OVERLAY_CLASS_WORDS;
                self.overlay_snapshot_classes[word]
                    .fetch_or(1u64 << (class_id % 64), Ordering::Relaxed);
                let young = self.overlay_is_young(&regions, owner)
                    || refs.iter().any(|&r| self.overlay_is_young(&regions, r));
                snap.insert(young, owner, refs);
            }
        }
        if !complete {
            // A provider that cannot enumerate its owners cannot be followed
            // by owner: root every row of this heap for this cycle, which is
            // what every row got before the marker followed edges.
            unowned.extend(
                crate::external_roots::external_roots_for_matching_owners(&|owner| {
                    self.overlay_in_heap(owner)
                })
                .into_iter()
                .map(|r| r.as_ptr() as usize),
            );
        }
        OVERLAY_SNAPSHOT_OWNERS.fetch_add(snap.len() as u64, Ordering::Relaxed);
        let len = snap.len();
        *self.overlay_snapshot.write() = snap;
        self.overlay_snapshot_len.store(len, Ordering::Release);
        if !unowned.is_empty() {
            let regions = self.regions.read();
            for addr in unowned {
                self.push_gray_or_mark(&regions, addr);
            }
        }
    }

    /// Drop the mark snapshot (cycle end or abort).
    pub(super) fn clear_overlay_snapshot(&self) {
        self.overlay_snapshot_len.store(0, Ordering::Release);
        self.overlay_snapshot.write().clear();
        for word in self.overlay_snapshot_classes.iter() {
            word.store(0, Ordering::Relaxed);
        }
    }

    /// The marker's owner -> overlay edge: hand the snapshot rows of the
    /// object at `addr` to `enqueue`, as if they were its fields. Called from
    /// `scan_object_refs` for every scanned non-array object; the class gate
    /// makes it two relaxed loads for every object whose class owns no row.
    ///
    /// The entry is NOT consumed: the overflow rescan re-walks marked objects
    /// through `scan_object_refs`, and a consumed entry would lose the edges a
    /// capped push dropped.
    #[inline]
    pub(super) fn overlay_snapshot_edges(
        &self,
        addr: usize,
        class_id: u32,
        enqueue: &mut dyn FnMut(usize),
    ) {
        if self.overlay_snapshot_len.load(Ordering::Acquire) == 0 {
            return;
        }
        let word = (class_id as usize / 64) % OVERLAY_CLASS_WORDS;
        if self.overlay_snapshot_classes[word].load(Ordering::Relaxed) & (1u64 << (class_id % 64))
            == 0
        {
            return;
        }
        let snap = self.overlay_snapshot.read();
        if let Some(values) = snap.get(addr) {
            OVERLAY_SNAPSHOT_EDGES.fetch_add(values.len() as u64, Ordering::Relaxed);
            for &value in values {
                enqueue(value);
            }
        }
    }

    /// Carry the mark snapshot across an evacuation pause, exactly as
    /// [`Self::remap_reference_skip_set`] carries the skip set: keys and
    /// values that moved are re-addressed; a key that sat in the CSet and was
    /// not forwarded is an owner that died, so its entry goes (it was not
    /// snapshot-live: a snapshot-live owner is reachable through preserved
    /// edges, which keep it alive at every pause); a value in the CSet was
    /// rooted by [`Self::overlay_pause_begin`], so an unforwarded one is
    /// defensive only and is dropped rather than left dangling. A young pause
    /// re-addresses the young part only; every entry is re-filed by where it
    /// now lives. Called after the pause freed its collection set, with the
    /// regions guard held.
    pub(super) fn remap_overlay_snapshot(
        &self,
        regions: &[G1Region],
        cset_set: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        young_only: bool,
    ) {
        if self.overlay_snapshot_len.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut snap = self.overlay_snapshot.write();
        let mut moving: Vec<(usize, Vec<usize>)> =
            std::mem::take(&mut snap.young).into_iter().collect();
        if !young_only {
            moving.extend(std::mem::take(&mut snap.old));
        }
        let resolve = |addr: usize| -> Option<usize> {
            if let Some(&new_addr) = pointer_map.get(&addr) {
                return Some(new_addr);
            }
            if self.overlay_in_cset(cset_set, addr) {
                None
            } else {
                Some(addr)
            }
        };
        for (owner, values) in moving {
            let Some(owner) = resolve(owner) else {
                continue;
            };
            let values: Vec<usize> = values.into_iter().filter_map(resolve).collect();
            if values.is_empty() {
                continue;
            }
            let young = self.overlay_is_young(regions, owner)
                || values.iter().any(|&v| self.overlay_is_young(regions, v));
            snap.insert(young, owner, values);
        }
        // The class gate needs no update: a class id travels with the object.
        self.overlay_snapshot_len
            .store(snap.len(), Ordering::Release);
    }

    /// Whether the completed mark is trustworthy enough to condemn overlay
    /// rows by — the same two conditions under which `cleanup` refuses every
    /// in-place free (`retain_all`). Read BEFORE cleanup, which consumes the
    /// implausible flag.
    fn overlay_mark_verdict_trusted(&self) -> bool {
        !self.mark_saw_implausible.load(Ordering::Relaxed) && self.gray_set_is_empty()
    }

    /// Prune every overlay row whose owner the completed mark proves dead.
    /// Called after the remark drain and remark-time resurrection, before
    /// `cleanup` frees anything: an unmarked owner's elements were not marked
    /// on its behalf, so once cleanup (or a later mixed pause) reclaims them
    /// the row would dangle. Skipped when the mark is untrusted — cleanup then
    /// frees nothing either, and the rows are judged again by the pauses.
    pub(crate) fn prune_overlays_after_mark(&self) {
        if !crate::external_roots::has_providers() {
            return;
        }
        if !self.overlay_mark_verdict_trusted() {
            OVERLAY_POST_MARK_PRUNES_SKIPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        OVERLAY_POST_MARK_PRUNES.fetch_add(1, Ordering::Relaxed);
        // gci i22/g1rk: `is_live_after_mark` per owner without the snapshot
        // mutex per call (`remark_liveness`; same verdict inside this pause).
        let view = self.remark_liveness();
        crate::external_roots::prune_external_roots_owned(
            Some((self.arena_base, self.arena_end)),
            &|owner| view.is_live(owner),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_roots::{ExternalRootProvider, OwnerPredicate};
    use std::collections::HashSet;

    // -- a test overlay provider ---------------------------------------------
    //
    // Rows are keyed by addresses in each test's own heap. Heaps that are
    // alive at the same time never share addresses, and every row naming a
    // heap is removed by the fixture's `Drop`, so a concurrently running test
    // (in this module or another collector's) never finds a row naming its
    // objects.

    /// One test row: a stable `key`, the owner's current address, its refs.
    #[derive(Clone)]
    struct TRow {
        key: usize,
        owner: usize,
        refs: Vec<usize>,
    }
    static ROWS: std::sync::Mutex<Vec<TRow>> = std::sync::Mutex::new(Vec::new());
    /// Keys written since the last take (the provider-side record).
    static DIRTY: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
    static NEXT_KEY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

    fn rows() -> std::sync::MutexGuard<'static, Vec<TRow>> {
        ROWS.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn dirty() -> std::sync::MutexGuard<'static, Vec<usize>> {
        DIRTY.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn obj(addr: usize) -> ObjectRef {
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }
    fn t_scan(out: &mut Vec<ObjectRef>) {
        for row in rows().iter() {
            out.extend(row.refs.iter().map(|&r| obj(r)));
        }
    }
    fn t_owner_addrs() -> Option<HashSet<usize>> {
        Some(rows().iter().map(|r| r.owner).collect())
    }
    fn t_roots_for_owner(owner: usize, _class: Option<u32>) -> Vec<ObjectRef> {
        rows()
            .iter()
            .filter(|r| r.owner == owner)
            .flat_map(|r| r.refs.iter().map(|&x| obj(x)))
            .collect()
    }
    fn t_matching(pred: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
        let snapshot: Vec<TRow> = rows().clone();
        snapshot
            .into_iter()
            .filter(|r| pred(r.owner))
            .flat_map(|r| r.refs.into_iter().map(obj))
            .collect()
    }
    fn t_remap(map: &cratonvm_types::PointerMap) {
        for row in rows().iter_mut() {
            if let Some(&n) = map.get(&row.owner) {
                row.owner = n;
            }
            for r in row.refs.iter_mut() {
                if let Some(&n) = map.get(r) {
                    *r = n;
                }
            }
        }
    }
    std::thread_local! {
        /// The arenas of the [`Fixture`]s alive on THIS thread. `ROWS` holds
        /// every running test's rows, and `prune` fans out to every provider
        /// from every test in the binary -- including
        /// `external_roots::tests::prune_condemns_only_the_collecting_heaps_owners`,
        /// whose deliberately unscreened predicate (`owned = None`) answers
        /// "dead" for every address but one constant. Judging all rows let
        /// that test delete a concurrently running fixture's row mid-pause
        /// (`a_rooted_cset_owner_keeps_its_row_parallel`, ~1 run in 20 of the
        /// full suite). A real overlay provider is per VM; this one is per
        /// test thread: a prune judges only the rows its own fixtures own.
        static MY_ARENAS: std::cell::RefCell<Vec<(usize, usize)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    fn owned_by_this_thread(owner: usize) -> bool {
        MY_ARENAS.with(|a| a.borrow().iter().any(|&(lo, hi)| owner >= lo && owner < hi))
    }
    fn t_prune(is_live: &OwnerPredicate<'_>) {
        let snapshot: Vec<TRow> = rows().clone();
        let dead: HashSet<usize> = snapshot
            .iter()
            .filter(|r| owned_by_this_thread(r.owner) && !is_live(r.owner))
            .map(|r| r.key)
            .collect();
        rows().retain(|r| !dead.contains(&r.key));
    }
    fn as_owner_row(r: &TRow) -> crate::external_roots::OwnerRow {
        crate::external_roots::OwnerRow {
            owner: r.owner,
            key: r.key,
            refs: r.refs.iter().map(|&x| obj(x)).collect(),
        }
    }
    fn t_all_rows(pred: &OwnerPredicate<'_>) -> Vec<crate::external_roots::OwnerRow> {
        let snapshot: Vec<TRow> = rows().clone();
        snapshot
            .iter()
            .filter(|r| pred(r.owner))
            .map(as_owner_row)
            .collect()
    }
    fn t_rows_for_keys(keys: &[usize]) -> Vec<crate::external_roots::OwnerRow> {
        let snapshot: Vec<TRow> = rows().clone();
        snapshot
            .iter()
            .filter(|r| keys.contains(&r.key))
            .map(as_owner_row)
            .collect()
    }
    fn t_take_dirty(pred: &OwnerPredicate<'_>) -> Vec<usize> {
        let snapshot: Vec<TRow> = rows().clone();
        let owner_of = |k: usize| snapshot.iter().find(|r| r.key == k).map(|r| r.owner);
        let mut d = dirty();
        let (mine, theirs): (Vec<usize>, Vec<usize>) = d
            .drain(..)
            .filter(|&k| owner_of(k).is_some())
            .partition(|&k| owner_of(k).is_some_and(|o| pred(o)));
        d.extend(theirs);
        mine
    }
    fn t_enable() {}

    fn register_provider() {
        crate::external_roots::register_external_root_provider(ExternalRootProvider {
            name: "g1-overlay-test-rows",
            scan: t_scan,
            owner_addrs: t_owner_addrs,
            roots_for_owner: t_roots_for_owner,
            roots_for_matching_owners: t_matching,
            remap: t_remap,
            prune: t_prune,
            gate_stats: None,
            owner_rows: Some(crate::external_roots::OwnerRowCallbacks {
                all_rows: t_all_rows,
                rows_for_keys: t_rows_for_keys,
                take_dirty_keys: t_take_dirty,
                enable_dirty_tracking: t_enable,
            }),
        });
    }

    /// One test's heap; every row naming it is dropped at the end.
    struct Fixture {
        gc: G1Collector,
    }
    impl Fixture {
        fn new() -> Self {
            register_provider();
            let config = G1CollectorConfig {
                heap_size: 8 * 1024 * 1024,
                initial_heap_size: 8 * 1024 * 1024,
                region_size: 1024 * 1024,
                max_gc_pause_ms: 200,
                ihop_percent: 45,
                promotion_age: 3,
                gc_worker_threads: 1,
                string_dedup_enabled: false,
                mixed_gc_count_target: 8,
                old_cset_region_threshold_percent: 10,
                mixed_gc_live_threshold_percent: 85,
                heap_waste_percent: 0,
            };
            let gc = G1Collector::new(config);
            MY_ARENAS.with(|a| a.borrow_mut().push((gc.arena_base, gc.arena_end)));
            Self { gc }
        }
        /// Write `owner`'s row (keeping its key if it has one) and record the
        /// write, as a mutator's overlay store does.
        fn set_row(&self, owner: ObjectRef, refs: &[ObjectRef]) {
            let owner = owner.as_ptr() as usize;
            let refs: Vec<usize> = refs.iter().map(|r| r.as_ptr() as usize).collect();
            let mut rows = rows();
            let key = match rows.iter_mut().find(|r| r.owner == owner) {
                Some(row) => {
                    row.refs = refs;
                    row.key
                }
                None => {
                    let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
                    rows.push(TRow { key, owner, refs });
                    key
                }
            };
            dirty().push(key);
        }
        fn row(&self, owner: usize) -> Option<Vec<usize>> {
            rows()
                .iter()
                .find(|r| r.owner == owner)
                .map(|r| r.refs.clone())
        }
        /// The VM's post-pause half: prune by the collector's verdict
        /// (forwarded, or in a live region and not condemned), then remap.
        fn after_pause(&self, result: &GcResult) {
            let condemned = self.gc.take_overlay_condemned_owners();
            let span = Some((self.gc.arena_base, self.gc.arena_end));
            crate::external_roots::prune_external_roots_owned(span, &|owner| {
                !condemned.contains(&owner)
                    && (result.pointer_map.contains_key(&owner)
                        || self.gc.is_addr_in_live_region(owner))
            });
            crate::external_roots::remap_external_roots(&result.pointer_map);
        }
        fn marked(&self, addr: usize) -> bool {
            let regions = self.gc.regions.read();
            let idx = self
                .gc
                .region_for_ptr(&regions, addr as *mut u8)
                .expect("in heap");
            regions[idx].mark_bitmap.is_marked(addr)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let (lo, hi) = (self.gc.arena_base, self.gc.arena_end);
            rows().retain(|r| !(r.owner >= lo && r.owner < hi));
            MY_ARENAS.with(|a| a.borrow_mut().retain(|&span| span != (lo, hi)));
        }
    }

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }
    fn stw() -> crate::collector::StopTheWorldToken {
        // SAFETY: single-threaded fixture.
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

    fn young(f: &Fixture, roots: &mut [ObjectRef], parallel: bool) -> GcResult {
        f.gc.overlay_collection_begin();
        if parallel {
            f.gc.young_collection_parallel(roots, &NoMonitors)
        } else {
            f.gc.young_collection_serial(roots, &NoMonitors)
        }
    }

    const OWNER_CLASS: u32 = 0x00C0_FFE1;
    const ELEM_CLASS: u32 = 0x00C0_FFE2;

    /// A live owner in the CSet keeps an element held ONLY by its row; the
    /// row is re-addressed to the copies.
    fn a_rooted_cset_owner_keeps_its_row(parallel: bool) {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.gc.set_field(elem, 0, Value::Int(77));
        f.set_row(owner, &[elem]);
        let mut roots = vec![owner];
        let result = young(&f, &mut roots, parallel);
        let new_owner = *result
            .pointer_map
            .get(&(owner.as_ptr() as usize))
            .expect("the rooted owner is evacuated");
        let new_elem = *result
            .pointer_map
            .get(&(elem.as_ptr() as usize))
            .expect("an element held only by a live owner's row must survive");
        f.after_pause(&result);
        assert_eq!(f.row(new_owner), Some(vec![new_elem]));
        assert_eq!(f.gc.get_field(obj(new_elem), 0).as_int(), Some(77));
    }

    /// The fixture's own isolation, which the parallel variant above depended
    /// on without having it: `prune` fans out to every provider in the binary,
    /// and another test's span-less, all-"dead" prune (as
    /// `external_roots::tests::prune_condemns_only_the_collecting_heaps_owners`
    /// issues) must not delete this thread's rows -- that deleted a row
    /// between a fixture's `set_row` and its pause, ~1 full-suite run in 20.
    #[test]
    fn a_prune_from_another_test_thread_leaves_this_threads_rows() {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.set_row(owner, &[elem]);
        std::thread::spawn(|| crate::external_roots::prune_external_roots_owned(None, &|_| false))
            .join()
            .expect("the foreign prune ran");
        assert_eq!(
            f.row(owner.as_ptr() as usize),
            Some(vec![elem.as_ptr() as usize]),
            "a prune on a thread with no fixture must not condemn this thread's row"
        );
        // ...while this thread's own prune still judges it.
        crate::external_roots::prune_external_roots_owned(None, &|_| false);
        assert_eq!(
            f.row(owner.as_ptr() as usize),
            None,
            "the owning thread's prune applies"
        );
    }

    #[test]
    fn a_rooted_cset_owner_keeps_its_row_serial() {
        a_rooted_cset_owner_keeps_its_row(false);
    }

    #[test]
    fn a_rooted_cset_owner_keeps_its_row_parallel() {
        a_rooted_cset_owner_keeps_its_row(true);
    }

    /// The defect this module closes: a dead owner whose element points back
    /// at it is collected, and its row is condemned rather than left holding
    /// a reference into a reclaimed region.
    fn a_self_referencing_dead_owner_is_collected(parallel: bool) {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.gc.set_field(elem, 0, Value::Object(Some(owner)));
        f.gc.set_field(owner, 0, Value::Int(1));
        f.set_row(owner, &[elem]);
        let owner_addr = owner.as_ptr() as usize;
        let mut roots: Vec<ObjectRef> = vec![];
        let result = young(&f, &mut roots, parallel);
        assert!(
            !result.pointer_map.contains_key(&owner_addr),
            "nothing but its own element names the owner: it is dead"
        );
        assert!(
            !result.pointer_map.contains_key(&(elem.as_ptr() as usize)),
            "the element is reachable only through a dead owner's row"
        );
        let condemned = f.gc.take_overlay_condemned_owners();
        assert!(
            condemned.contains(&owner_addr),
            "the dead owner is reported"
        );
        // Re-arm for the fixture's own prune.
        f.gc.overlay_condemned_owners.lock().extend(condemned);
        f.after_pause(&result);
        assert_eq!(f.row(owner_addr), None, "the dead owner's row is pruned");
    }

    #[test]
    fn a_self_referencing_dead_owner_is_collected_serial() {
        a_self_referencing_dead_owner_is_collected(false);
    }

    #[test]
    fn a_self_referencing_dead_owner_is_collected_parallel() {
        a_self_referencing_dead_owner_is_collected(true);
    }

    /// An owner outside the collection set (here: in a pinned region) is live
    /// for the pause, so its row's young element is a root.
    #[test]
    fn a_non_cset_owner_roots_its_row() {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let region_of = |o: ObjectRef| {
            let regions = f.gc.regions.read();
            f.gc.region_for_ptr(&regions, o.as_ptr()).unwrap()
        };
        let owner_region = region_of(owner);
        let mut elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        while region_of(elem) == owner_region {
            elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        }
        f.gc.set_field(elem, 0, Value::Int(5));
        f.set_row(owner, &[elem]);
        f.gc.pin_region(owner_region);
        let mut roots: Vec<ObjectRef> = vec![];
        let result = young(&f, &mut roots, false);
        let new_elem = *result
            .pointer_map
            .get(&(elem.as_ptr() as usize))
            .expect("a non-CSet owner's row is a root");
        f.after_pause(&result);
        assert_eq!(f.row(owner.as_ptr() as usize), Some(vec![new_elem]));
        f.gc.unpin_region(owner_region);
    }

    /// Owner -> row -> owner -> row: the fixpoint follows an owner that only
    /// becomes live through another owner's row.
    fn nested_owners_reach_a_fixpoint(parallel: bool) {
        let f = Fixture::new();
        let outer = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let inner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.set_row(outer, &[inner]);
        f.set_row(inner, &[elem]);
        let mut roots = vec![outer];
        let result = young(&f, &mut roots, parallel);
        for (what, o) in [("inner", inner), ("elem", elem)] {
            assert!(
                result.pointer_map.contains_key(&(o.as_ptr() as usize)),
                "{what} is live through the chain of rows"
            );
        }
        assert!(f.gc.take_overlay_condemned_owners().is_empty());
    }

    #[test]
    fn nested_owners_reach_a_fixpoint_serial() {
        nested_owners_reach_a_fixpoint(false);
    }

    #[test]
    fn nested_owners_reach_a_fixpoint_parallel() {
        nested_owners_reach_a_fixpoint(true);
    }

    /// Marking follows a live owner's row and not a dead one's, and the
    /// post-mark prune drops exactly the dead owner's row.
    #[test]
    fn marking_follows_live_owner_rows_only_and_prunes_the_dead() {
        let f = Fixture::new();
        let live_owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let live_elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        let dead_owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let dead_elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.gc.set_field(dead_elem, 0, Value::Object(Some(dead_owner)));
        f.set_row(live_owner, &[live_elem]);
        f.set_row(dead_owner, &[dead_elem]);

        f.gc.start_concurrent_mark(&stw());
        f.gc.remark(&stw(), &[live_owner]);
        while !f.gc.concurrent_mark_step(usize::MAX) {}

        assert!(f.marked(live_owner.as_ptr() as usize));
        assert!(
            f.marked(live_elem.as_ptr() as usize),
            "the marker follows a scanned owner's row"
        );
        assert!(!f.marked(dead_owner.as_ptr() as usize));
        assert!(
            !f.marked(dead_elem.as_ptr() as usize),
            "a dead owner's row is not a root: the self-referencing pair is garbage"
        );

        f.gc.prune_overlays_after_mark();
        assert!(f.row(live_owner.as_ptr() as usize).is_some());
        assert_eq!(f.row(dead_owner.as_ptr() as usize), None);
        f.gc.cleanup(&stw());
        assert_eq!(f.gc.overlay_snapshot_len.load(Ordering::Relaxed), 0);
    }

    /// SATB: an element removed from a row after the snapshot is still marked
    /// on behalf of its (live) owner — the mutator may have stored it into an
    /// object the marker already scanned.
    #[test]
    fn marking_uses_the_snapshot_row_not_the_current_one() {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.set_row(owner, &[elem]);
        f.gc.start_concurrent_mark(&stw());
        // The mutator empties the row after the snapshot.
        f.set_row(owner, &[]);
        f.gc.remark(&stw(), &[owner]);
        while !f.gc.concurrent_mark_step(usize::MAX) {}
        assert!(
            f.marked(elem.as_ptr() as usize),
            "a snapshot-live element is marked even though the row no longer holds it"
        );
        f.gc.cleanup(&stw());
    }

    /// A young pause inside a mark cycle keeps the snapshot's values alive and
    /// re-addresses the snapshot, so the marker still finds the edge.
    #[test]
    fn a_young_pause_carries_the_mark_snapshot() {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.set_row(owner, &[elem]);
        f.gc.start_concurrent_mark(&stw());
        // Removed from the row after the snapshot, so only the snapshot keeps
        // it alive across the pause.
        f.set_row(owner, &[]);
        let mut roots = vec![owner];
        let result = young(&f, &mut roots, false);
        let new_owner = *result.pointer_map.get(&(owner.as_ptr() as usize)).unwrap();
        let new_elem = *result
            .pointer_map
            .get(&(elem.as_ptr() as usize))
            .expect("a snapshot value survives a pause inside the cycle");
        f.after_pause(&result);
        assert_eq!(
            f.gc.overlay_snapshot.read().values_of(new_owner),
            Some(vec![new_elem]),
            "the snapshot is re-keyed and re-addressed through the pause"
        );
        f.gc.remark(&stw(), &[obj(new_owner)]);
        while !f.gc.concurrent_mark_step(usize::MAX) {}
        assert!(f.marked(new_elem));
        f.gc.cleanup(&stw());
    }

    fn key_of(owner: usize) -> usize {
        rows()
            .iter()
            .find(|r| r.owner == owner)
            .map(|r| r.key)
            .unwrap()
    }

    /// An owner outside every collection set: allocated, then its region
    /// pinned. Returns the owner and its region.
    fn pinned_owner(f: &Fixture) -> (ObjectRef, usize) {
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let region = {
            let regions = f.gc.regions.read();
            f.gc.region_for_ptr(&regions, owner.as_ptr()).unwrap()
        };
        // Move the allocation cursor off the pinned region so later objects
        // land in collectable Eden.
        loop {
            let probe = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
            let r = {
                let regions = f.gc.regions.read();
                f.gc.region_for_ptr(&regions, probe.as_ptr()).unwrap()
            };
            if r != region {
                break;
            }
        }
        f.gc.pin_region(region);
        (owner, region)
    }

    /// The remembered-set path end to end on a young pause: a row written
    /// after the full read is found through the provider's written-key record;
    /// the young reference it then holds is found again by the next pause with
    /// no write at all (the remembered set); once that reference is old, the
    /// row is forgotten.
    #[test]
    fn young_pauses_read_written_and_remembered_rows() {
        let f = Fixture::new();
        let (owner, region) = pinned_owner(&f);
        let owner_addr = owner.as_ptr() as usize;
        f.set_row(owner, &[]);
        let key = key_of(owner_addr);

        // Pause 1: the full read that makes the remembered set valid.
        let r1 = young(&f, &mut [], false);
        f.after_pause(&r1);
        assert!(
            f.gc.overlay_rset.lock().valid,
            "a full read leaves a valid set"
        );

        // A mutator stores a young object into the OLD owner's row.
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.gc.set_field(elem, 0, Value::Int(9));
        f.set_row(owner, &[elem]);
        let r2 = young(&f, &mut [], false);
        let e2 = *r2
            .pointer_map
            .get(&(elem.as_ptr() as usize))
            .expect("a row written since the last pause is read");
        f.after_pause(&r2);
        assert!(
            f.gc.overlay_rset.lock().keys.iter().any(|&(_, k)| k == key),
            "a row holding a young reference is remembered"
        );

        // No write this time: only the remembered set can find it.
        let r3 = young(&f, &mut [], false);
        let e3 = *r3
            .pointer_map
            .get(&e2)
            .expect("a remembered row's young reference survives a write-free pause");
        f.after_pause(&r3);
        assert_eq!(f.row(owner_addr), Some(vec![e3]));
        assert_eq!(f.gc.get_field(obj(e3), 0).as_int(), Some(9));

        // The owner sits in a pinned EDEN region, so it is young itself and
        // the row stays remembered for that reason alone. Unpin it, keep it
        // live through a root, and pause until owner and reference are both
        // promoted: then the row is forgotten.
        f.gc.unpin_region(region);
        let mut owner_now = owner_addr;
        let mut now = e3;
        let mut both_old = false;
        for _ in 0..12 {
            let mut roots = [obj(owner_now)];
            let r = young(&f, &mut roots, false);
            owner_now = roots[0].as_ptr() as usize;
            if let Some(&n) = r.pointer_map.get(&now) {
                now = n;
            }
            f.after_pause(&r);
            both_old = {
                let regions = f.gc.regions.read();
                !f.gc.overlay_is_young(&regions, now) && !f.gc.overlay_is_young(&regions, owner_now)
            };
            if both_old {
                break;
            }
        }
        assert!(
            both_old,
            "owner and reference are promoted within the tenuring threshold"
        );
        assert_eq!(f.row(owner_now), Some(vec![now]));
        assert_eq!(f.gc.get_field(obj(now), 0).as_int(), Some(9));
        assert!(
            !f.gc.overlay_rset.lock().keys.iter().any(|&(_, k)| k == key),
            "a row whose owner and references are all old is not remembered"
        );
    }

    /// An evacuation failure invalidates the remembered set, and the next
    /// young pause reads every row.
    #[test]
    fn an_invalidated_remembered_set_forces_a_full_read() {
        let f = Fixture::new();
        let (owner, region) = pinned_owner(&f);
        f.set_row(owner, &[]);
        let r1 = young(&f, &mut [], false);
        f.after_pause(&r1);
        assert!(f.gc.overlay_rset.lock().valid);
        f.gc.overlay_rset_invalidate();
        // A row written with NO record (as if the write predated tracking):
        // only a full read can find it.
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        {
            let owner_addr = owner.as_ptr() as usize;
            let mut rows = rows();
            let row = rows.iter_mut().find(|r| r.owner == owner_addr).unwrap();
            row.refs = vec![elem.as_ptr() as usize];
        }
        let r2 = young(&f, &mut [], false);
        assert!(
            r2.pointer_map.contains_key(&(elem.as_ptr() as usize)),
            "an invalid remembered set is never trusted"
        );
        f.after_pause(&r2);
        f.gc.unpin_region(region);
    }

    /// The parallel driver expands a rooted CSet owner's rows INSIDE its
    /// closure (the workers copy the row's graph), not only in the serial
    /// fixpoint after it: the row's element and the element's own field
    /// referent are both evacuated, and the expansion counter moves.
    #[test]
    fn the_parallel_closure_expands_owner_rows() {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        let leaf = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.gc.set_field(leaf, 0, Value::Int(31));
        f.gc.set_field(elem, 0, Value::Object(Some(leaf)));
        f.set_row(owner, &[elem]);
        let before = g1_overlay_census().parallel_expanded;
        let mut roots = vec![owner];
        let result = young(&f, &mut roots, true);
        assert!(
            g1_overlay_census().parallel_expanded > before,
            "a worker expanded the owner's rows"
        );
        let new_elem = *result
            .pointer_map
            .get(&(elem.as_ptr() as usize))
            .expect("the row's element is evacuated");
        let Value::Object(Some(new_leaf)) = f.gc.get_field(obj(new_elem), 0) else {
            panic!("the element's field still names an object");
        };
        assert_ne!(
            new_leaf.as_ptr(),
            leaf.as_ptr(),
            "the element's referent moved with it"
        );
        assert_eq!(f.gc.get_field(new_leaf, 0).as_int(), Some(31));
        f.after_pause(&result);
    }

    /// A humongous array named by NOTHING but a live owner's overlay row (a
    /// large `TreeMap`'s backing `data`) survives the eager humongous
    /// reclaim, which otherwise judges spans by heap slots and roots alone.
    fn an_overlay_held_humongous_span_is_not_eagerly_reclaimed(parallel: bool) {
        let f = Fixture::new();
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let big =
            f.gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let big_region = {
            let regions = f.gc.regions.read();
            f.gc.region_for_ptr(&regions, big.as_ptr()).unwrap()
        };
        assert_eq!(
            f.gc.regions.read()[big_region].region_type(),
            RegionType::HumongousStart
        );
        f.set_row(owner, &[big]);
        let mut roots = vec![owner];
        let result = young(&f, &mut roots, parallel);
        assert_eq!(
            f.gc.regions.read()[big_region].region_type(),
            RegionType::HumongousStart,
            "the overlay-held humongous span is still allocated"
        );
        f.after_pause(&result);
        assert_eq!(
            f.row(roots[0].as_ptr() as usize),
            Some(vec![big.as_ptr() as usize])
        );
    }

    #[test]
    fn an_overlay_held_humongous_span_is_not_eagerly_reclaimed_serial() {
        an_overlay_held_humongous_span_is_not_eagerly_reclaimed(false);
    }

    #[test]
    fn an_overlay_held_humongous_span_is_not_eagerly_reclaimed_parallel() {
        an_overlay_held_humongous_span_is_not_eagerly_reclaimed(true);
    }

    /// gci i44/cd -- the remembered-set predicate: a young object always pins
    /// its row for the next young pause, a humongous one only on the switch
    /// arm (`CRATONVM_G1_OVERLAY_REMEMBER_HUMONGOUS`, default ON), Old and Free
    /// never.
    #[test]
    fn cd44_a_humongous_reference_pins_the_row_for_young_pauses() {
        for on in [false, true] {
            assert!(overlay_kind_pins_young_pause(RegionType::Eden, on));
            assert!(overlay_kind_pins_young_pause(RegionType::Survivor, on));
            assert_eq!(overlay_kind_pins_young_pause(RegionType::HumongousStart, on), on);
            assert_eq!(
                overlay_kind_pins_young_pause(RegionType::HumongousContinuation, on),
                on
            );
            assert!(!overlay_kind_pins_young_pause(RegionType::Old, on));
            assert!(!overlay_kind_pins_young_pause(RegionType::Free, on));
        }
    }

    /// gci i44/cd -- an OLD owner whose row holds a humongous array and is
    /// not written again after the first pause. Promotes the owner (kept by a
    /// root) and then runs `pauses_after` write-free young pauses; returns
    /// `(alive, remembered)`: whether the humongous span is still allocated,
    /// and whether the row is in the overlay remembered set after the last
    /// pause. `remember_humongous` forces the per-collector latch
    /// (`OverlayRemSet::remember_humongous`) before the first pause, so the
    /// kill arm is reached through the field, not the environment.
    ///
    /// Before the fix the row left the overlay remembered set the pause its
    /// owner was promoted (neither the owner nor the value was young), the
    /// next young pause read the rows incrementally and did not read it, and
    /// the eager humongous reclaim -- which hears of an overlay-held span only
    /// through the rows a pause read -- freed the span under the row.
    fn old_owner_humongous_row(
        parallel: bool,
        pauses_after: usize,
        remember_humongous: bool,
    ) -> (bool, bool) {
        let f = Fixture::new();
        f.gc.overlay_rset.lock().remember_humongous = Some(remember_humongous);
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let big = f.gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let big_addr = big.as_ptr() as usize;
        let big_region = {
            let regions = f.gc.regions.read();
            f.gc.region_for_ptr(&regions, big.as_ptr()).unwrap()
        };
        assert_eq!(
            f.gc.regions.read()[big_region].region_type(),
            RegionType::HumongousStart
        );
        f.set_row(owner, &[big]);
        let key = key_of(owner.as_ptr() as usize);
        let mut owner_now = owner.as_ptr() as usize;
        let mut promoted = false;
        for _ in 0..12 {
            let mut roots = [obj(owner_now)];
            let r = young(&f, &mut roots, parallel);
            owner_now = roots[0].as_ptr() as usize;
            f.after_pause(&r);
            promoted = {
                let regions = f.gc.regions.read();
                !f.gc.overlay_is_young(&regions, owner_now)
            };
            if promoted {
                break;
            }
        }
        assert!(promoted, "the rooted owner is promoted within the tenuring threshold");
        assert_eq!(
            f.gc.regions.read()[big_region].region_type(),
            RegionType::HumongousStart,
            "while the owner is young its row is read every pause"
        );
        for _ in 0..pauses_after {
            let mut roots = [obj(owner_now)];
            let r = young(&f, &mut roots, parallel);
            owner_now = roots[0].as_ptr() as usize;
            f.after_pause(&r);
        }
        let alive = f.gc.regions.read()[big_region].region_type() == RegionType::HumongousStart;
        if alive {
            assert_eq!(f.row(owner_now), Some(vec![big_addr]));
        }
        let remembered = f
            .gc
            .overlay_rset
            .lock()
            .keys
            .iter()
            .any(|&(_, k)| k == key);
        (alive, remembered)
    }

    #[test]
    fn cd44_an_old_owners_humongous_row_value_survives_write_free_young_pauses_serial() {
        let (alive, remembered) = old_owner_humongous_row(false, 3, true);
        assert!(
            alive,
            "a humongous array held only by an old owner's overlay row was eagerly reclaimed"
        );
        assert!(remembered, "the row holding a humongous value stays remembered");
    }

    #[test]
    fn cd44_an_old_owners_humongous_row_value_survives_write_free_young_pauses_parallel() {
        let (alive, remembered) = old_owner_humongous_row(true, 3, true);
        assert!(
            alive,
            "a humongous array held only by an old owner's overlay row was eagerly reclaimed"
        );
        assert!(remembered, "the row holding a humongous value stays remembered");
    }

    /// gci i44/cd (lane ch's H1) -- the enable decision: the first row read,
    /// and every read after the provider table moved past the generation the
    /// remembered set was rebuilt under.
    #[test]
    fn cd44_a_provider_table_change_re_enables_dirty_tracking() {
        assert!(overlay_tracking_needs_enable(false, 0, 0));
        assert!(overlay_tracking_needs_enable(false, 7, 7));
        assert!(!overlay_tracking_needs_enable(true, 7, 7));
        assert!(overlay_tracking_needs_enable(true, 7, 8));
    }

    static LATE_ENABLES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn late_scan(_: &mut Vec<ObjectRef>) {}
    fn late_owner_addrs() -> Option<HashSet<usize>> {
        Some(HashSet::new())
    }
    fn late_roots_for_owner(_: usize, _: Option<u32>) -> Vec<ObjectRef> {
        Vec::new()
    }
    fn late_matching(_: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
        Vec::new()
    }
    fn late_remap(_: &cratonvm_types::PointerMap) {}
    fn late_prune(_: &OwnerPredicate<'_>) {}
    fn late_all_rows(_: &OwnerPredicate<'_>) -> Vec<crate::external_roots::OwnerRow> {
        Vec::new()
    }
    fn late_rows_for_keys(_: &[usize]) -> Vec<crate::external_roots::OwnerRow> {
        Vec::new()
    }
    fn late_take_dirty(_: &OwnerPredicate<'_>) -> Vec<usize> {
        Vec::new()
    }
    fn late_enable() {
        LATE_ENABLES.fetch_add(1, Ordering::Relaxed);
    }

    /// gci i44/cd (lane ch's H1) -- a row provider registered after this
    /// collector's first overlay pause (and after a rebuild under the old
    /// table generation) is told to record its written keys by the next row
    /// read. The provider has no rows, so registering it changes nothing for
    /// the other tests in the binary but forcing one full read. (Another test's
    /// first pause can also enable it, so this can only pass spuriously, never
    /// fail spuriously; the pure test above pins the decision itself.)
    #[test]
    fn cd44_a_late_row_provider_is_enabled_by_the_next_row_read() {
        let f = Fixture::new();
        // A full pause and its rebuild: tracking on, the set valid under the
        // current generation. The heap must hold something: a young pause over
        // an empty heap returns before its overlay phase (the first version of
        // this test paused an empty heap and never reached the row read).
        let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
        let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        f.set_row(owner, &[elem]);
        let mut roots = vec![owner];
        let r = young(&f, &mut roots, false);
        f.after_pause(&r);
        assert!(
            f.gc.overlay_rset.lock().tracking,
            "the pause read the rows through the row API"
        );
        crate::external_roots::register_external_root_provider(ExternalRootProvider {
            name: "g1-overlay-test-late-rows",
            scan: late_scan,
            owner_addrs: late_owner_addrs,
            roots_for_owner: late_roots_for_owner,
            roots_for_matching_owners: late_matching,
            remap: late_remap,
            prune: late_prune,
            gate_stats: None,
            owner_rows: Some(crate::external_roots::OwnerRowCallbacks {
                all_rows: late_all_rows,
                rows_for_keys: late_rows_for_keys,
                take_dirty_keys: late_take_dirty,
                enable_dirty_tracking: late_enable,
            }),
        });
        let before = LATE_ENABLES.load(Ordering::Relaxed);
        let (_, _, incremental) = f.gc.overlay_read_rows(&RegionSet::new(), true);
        assert!(!incremental, "a changed provider table forces a full read");
        assert!(
            LATE_ENABLES.load(Ordering::Relaxed) > before,
            "the late provider's written-key record was never switched on"
        );
    }

    /// gci i44/cd -- the kill arm, forced through the latch field (not the
    /// environment): once the owner is promoted the row is NOT in the
    /// remembered set, which is the defect's precondition (the next
    /// incremental young pause does not read it). Deterministic: no pause runs
    /// after the promotion, so nothing is reclaimed under the row. Whether the
    /// reclaim then actually fires depends on whether a concurrently running
    /// test moved the provider-table generation (a full read reads the row),
    /// which is why the reclaim itself is not asserted on this arm.
    #[test]
    fn cd44_the_kill_arm_drops_the_old_owners_humongous_row_from_the_set() {
        let (alive, remembered) = old_owner_humongous_row(false, 0, false);
        assert!(alive, "no pause ran after the promotion");
        assert!(
            !remembered,
            "with the latch off only young owners / values keep a row remembered"
        );
    }
}
