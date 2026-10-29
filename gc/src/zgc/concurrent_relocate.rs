//! zt t32: R5 / R6 of the concurrent-relocation design, **concurrent
//! relocation** (`CRATONVM_ZGC_CONCURRENT_RELOCATE=1`, opt-in). Sections 1 and
//! 2 of `docs/feature-designs/zgc-jit-load-barrier.md`;
//! the design is section 13 of
//! `docs/feature-designs/zgc-architecture.md`.
//!
//! # What changes
//!
//! R4 copies every selected survivor inside the closing pause (the slide's
//! region drain), so the pause scales with the moved bytes. Here the closing
//! pause PLANS the relocation instead of the slide: it selects regions
//! (`select_relocation_regions` over the survivors), allocates every selected
//! survivor's destination in a to-space window, and copies only the EAGER set
//! (below). Every other selected survivor is copied after the release, by the
//! first of:
//!
//! * the load barrier's slow path, which meets the object through a heap slot
//!   whose word is bad (the closing pause flipped the good color to
//!   `Remapped`, and a slot the closing mark healed carries the mark color);
//! * the next mark's slot visit and the SATB forward (R4's hooks);
//! * the collecting thread's drain after the release
//!   ([`super::ZgcRealHeap::run_concurrent_relocate_drain`]).
//!
//! A copy is claim, copy, register the destination, publish: one byte
//! (`ZLazyState`) per planned object, so a racing copier waits for the claimer
//! and every caller leaves with the one planned address.
//!
//! # The eager set (copied inside the pause)
//!
//! Anything dereferenced without a load barrier before the drain ends:
//!
//! * **Every word the pause cannot leave bad: legacy cells.** A word naming a
//!   planned object reads bad after the release when it is a colored slot in
//!   the closing mark's color. Every compact field and array element of a
//!   survivor is one (zt t33): the closing mark visited and healed it, or a
//!   store during the mark wrote it in that color, and no writer stores a
//!   plain word while the barrier is armed. The closing pause's own heals
//!   and stores, which come after its remap flip (the mark's last closure,
//!   reference processing, the VM's epilogue), write that color too: the
//!   heap's heal color stays the leaving mark color until the pause ends
//!   (`barrier_heal_mask`; before t33 they wrote `Remapped`, which reads good,
//!   and the walk caught them). A cycle marked stop-the-world has no mark
//!   color (its remap flip is a noop and the marker healed in `Remapped`), so
//!   the plan refuses it (`NoMarkColor`); the noop flip clears a color an
//!   earlier refused pause left unread. A legacy 16-byte cell is never
//!   colored, so the legacy pass walks every legacy-shaped survivor (zt t34:
//!   listed by the mark's claims and found among the objects of its
//!   blackened ranges, [`ZMarkLiveness`]; t33's sizing loop read every
//!   survivor's header for it): a cell naming a planned object is rewritten to
//!   the planned address, and its target is copied. All rewrites happen
//!   before any copy, so a holder copied later carries them. The rest of the
//!   live set is not walked; `CRATONVM_DBG_ZGC_CRL_WALK_CHECK=1` walks it
//!   anyway and counts what it would still rewrite (`crl_walk_missed=`, 0).
//! * **Every address the pause's remaps look up.** The pointer map this pause
//!   hands back holds EVERY planned pair, with a hit hook
//!   (`PointerMap::set_hit_hook`) that copies the object a lookup hits. The
//!   roots (rewritten through the map by `collect_garbage`), the monitor and
//!   reference-processor tables, the VM's whole remap epilogue and each
//!   thread's frame remap at resume learn a planned address only through a
//!   lookup, so each of them finds its object copied.
//! * **Objects that may own pin or overlay edges** (`extra_root_probe`): those
//!   side tables are keyed by the owner's address.
//!
//! # The from-space regions
//!
//! Selected regions leave the free list (their dead bytes are withheld), no
//! allocation lands in them, and a copied object's base leaves the registry.
//! At the next mark end (`release_evac_quarantine`, before the sweep) the
//! drain is finished if it is not, the cycle is withdrawn from the hot paths,
//! and the vacated spans return to the free list zeroed.
//!
//! # The plan's inputs come from the mark (zt t34)
//!
//! The pause selects regions from per-region liveness the MARK booked
//! ([`ZMarkLiveness`]), re-reads only the selected regions' survivors from
//! the registry (exact sizes, the straddle-in, a second selection over the
//! exact figures), and takes the legacy holders from the mark's list: no
//! header of an unselected survivor is read in the pause. A cycle the mark
//! did not book (`CRATONVM_ZGC_CRL_MARK_LIVENESS=0`, not armed at its mark
//! start) sizes the live set as t33 did.
//!
//! Census (`CRATONVM_GC_STATS=1`): `[GC] zgc-concurrent-relocate: crl_enabled=
//! crl_cycles= crl_refused= crl_last_refusal= crl_regions= crl_trimmed= crl_planned=
//! crl_planned_bytes= crl_eager= crl_hit= crl_barrier= crl_drain= crl_waits=
//! crl_released_bytes= crl_release_kept= crl_deferred= crl_legacy_holders=
//! crl_walk_checked= crl_walk_missed=plain/color/legacy crl_mark_plans=
//! crl_mark_fallbacks= crl_exact_dropped= crl_black_scanned= crl_black_legacy=
//! crl_liveness_check=plans/under/missed`.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use super::region_forwarding::{
    region_liveness, select_relocation_regions, ZRegionGrid, ZRegionLiveness, ZRELOC_REGION_BYTES,
};

/// `CRATONVM_ZGC_CONCURRENT_RELOCATE` (zt t32, R6; opt-in, `=1`). Read once per
/// heap, at construction.
pub(crate) fn zgc_concurrent_relocate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_CONCURRENT_RELOCATE")
}

/// `CRATONVM_DBG_ZGC_CRL_WALK_CHECK` (debug): after the plan's legacy pass,
/// walk every survivor's reference slots as the pause did before zt t33 and
/// count (and rewrite, so the run stays sound) every word it would still have
/// to rewrite: `crl_walk_missed=`. Must read 0. Read once per heap.
fn zgc_crl_walk_check_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ZGC_CRL_WALK_CHECK")
}

/// `CRATONVM_ZGC_CRL_MARK_LIVENESS` (zt t34; default ON, `=0` restores t33's
/// plan): the plan takes its region liveness and its legacy holders from the
/// mark ([`ZMarkLiveness`]) instead of sizing every survivor in the pause.
/// Read once per heap.
fn zgc_crl_mark_liveness_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_CRL_MARK_LIVENESS")
}

/// `CRATONVM_DBG_ZGC_CRL_LIVENESS_CHECK` (debug, zt t34): a plan made from the
/// mark's liveness also runs t33's sizing loop and header scan over the whole
/// live set and counts what the mark's inputs got wrong
/// (`crl_liveness_check=plans/under/legacy_missed`; both must read 0). Read
/// once per heap.
fn zgc_crl_liveness_check_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ZGC_CRL_LIVENESS_CHECK")
}

/// zt t34: the plan's inputs, kept by the MARK (`CRATONVM_ZGC_CRL_MARK_LIVENESS`).
///
/// Every survivor of a concurrent cycle had its mark bit set by one of three
/// writers, and each one books it here while the cycle is ARMED (from the
/// mark-start pause, when R6 may plan, until the plan takes the figures):
///
/// * a winning claim (`mark_claim`): the object's size onto the region of its
///   base, its tail onto the later regions it reaches (straddle-in), and the
///   base onto the legacy list when it is legacy-shaped (the one header read,
///   which the marker's visit then finds in cache);
/// * a per-object allocate-black (`allocate_black_if_marking`): the same --
///   and, after the mark has ended, every allocation of the closing pause
///   before the plan (they survive its sweep unmarked: the VM's reference and
///   finalizer work allocates there), so a legacy holder born there is listed;
/// * a blackened range (a TLAB chunk handed out during the mark, a live
///   TLAB's tail at mark start): the whole range as live bytes, every later
///   region it reaches as straddled, and the range itself, whose objects the
///   plan scans for legacy holders (their headers were written by allocation,
///   not read by the mark).
///
/// The figures are the SELECTION's input only. A selected region's survivors,
/// sizes and straddle-in are re-read exactly from the registry by the plan
/// (`O(selected survivors)`), and a region the exact figures reject leaves the
/// set, so a wrong estimate costs relocation quality, never soundness.
/// Estimates are upper bounds by construction (a blackened range counts its
/// unused tail), which `CRATONVM_DBG_ZGC_CRL_LIVENESS_CHECK` verifies.
pub(crate) struct ZMarkLiveness {
    /// `CRATONVM_ZGC_CRL_MARK_LIVENESS`.
    on: bool,
    armed: AtomicBool,
    /// Arms so far: a closing pause disarms only the cycle it closes
    /// ([`ZMarkLivenessPauseEnd`]).
    epoch: AtomicUsize,
    /// Arena base and region count the tables cover.
    base: usize,
    regions: usize,
    /// One table per shard ([`zml_shard`]), so concurrent markers do not
    /// share the counters' cache lines; summed by [`Self::take`].
    tables: std::sync::OnceLock<Box<[ZMarkLivenessTable]>>,
    unsizable: AtomicUsize,
    /// The legacy holders, per shard: a marker locks its own shard's list.
    legacy: [Mutex<Vec<usize>>; ZML_SHARDS],
    black: Mutex<Vec<(usize, usize)>>,
}

/// Shards of [`ZMarkLiveness`]'s tables and legacy lists.
const ZML_SHARDS: usize = 16;

/// This thread's shard: handed out round robin on its first booking.
#[inline]
fn zml_shard() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static SHARD: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    }
    SHARD.with(|s| {
        let v = s.get();
        if v != usize::MAX {
            return v;
        }
        let v = NEXT.fetch_add(1, Ordering::Relaxed) % ZML_SHARDS;
        s.set(v);
        v
    })
}

/// [`ZMarkLiveness::pause_end_guard`].
pub(super) struct ZMarkLivenessPauseEnd<'a> {
    live: &'a ZMarkLiveness,
    epoch: usize,
}

impl Drop for ZMarkLivenessPauseEnd<'_> {
    fn drop(&mut self) {
        if self.live.epoch.load(Ordering::Relaxed) == self.epoch {
            self.live.disarm();
        }
    }
}

/// One shard's counters. `u32`: a region holds at most
/// [`ZRELOC_REGION_BYTES`] bytes and objects, so no shard's figure can wrap.
struct ZMarkLivenessTable {
    live_bytes: Box<[AtomicU32]>,
    live_objects: Box<[AtomicU32]>,
    straddle_in_bytes: Box<[AtomicU32]>,
}

/// What a plan takes from [`ZMarkLiveness`].
struct ZMarkTaken {
    liveness: ZRegionLiveness,
    legacy: Vec<usize>,
    black: Vec<(usize, usize)>,
}

impl ZMarkLiveness {
    fn new(on: bool, base: usize, span: usize) -> Self {
        Self {
            on,
            armed: AtomicBool::new(false),
            epoch: AtomicUsize::new(0),
            base,
            regions: span.div_ceil(ZRELOC_REGION_BYTES),
            tables: std::sync::OnceLock::new(),
            unsizable: AtomicUsize::new(0),
            legacy: std::array::from_fn(|_| Mutex::new(Vec::new())),
            black: Mutex::new(Vec::new()),
        }
    }

    /// Is a mark booking into the tables? The marker's one extra load.
    #[inline]
    pub(crate) fn armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    fn tables(&self) -> &[ZMarkLivenessTable] {
        self.tables.get_or_init(|| {
            (0..ZML_SHARDS)
                .map(|_| {
                    let t = |_| AtomicU32::new(0);
                    ZMarkLivenessTable {
                        live_bytes: (0..self.regions).map(t).collect(),
                        live_objects: (0..self.regions).map(t).collect(),
                        straddle_in_bytes: (0..self.regions).map(t).collect(),
                    }
                })
                .collect()
        })
    }

    /// Zero the figures and start booking. **In the mark-start pause**,
    /// before the first claim or blackening of the cycle.
    fn arm(&self) {
        for t in self.tables() {
            for v in [&t.live_bytes, &t.live_objects, &t.straddle_in_bytes] {
                for a in v.iter() {
                    a.store(0, Ordering::Relaxed);
                }
            }
        }
        self.unsizable.store(0, Ordering::Relaxed);
        for s in self.legacy.iter() {
            s.lock().clear();
        }
        self.black.lock().clear();
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    /// Stop booking; `true` when the figures were being kept.
    fn disarm(&self) -> bool {
        self.armed.swap(false, Ordering::AcqRel)
    }

    /// A closing pause's guard: at its end the cycle it closes books no more,
    /// whether or not a plan took the figures (a refused or slid cycle would
    /// otherwise book the next interval's allocations). A cycle armed inside
    /// the pause is left armed.
    pub(super) fn pause_end_guard(&self) -> ZMarkLivenessPauseEnd<'_> {
        ZMarkLivenessPauseEnd {
            live: self,
            epoch: self.epoch.load(Ordering::Relaxed),
        }
    }

    /// One object at `base`, `size` bytes (`None`: unsizable).
    #[inline]
    fn note_object(&self, base: usize, size: Option<usize>, legacy: bool) {
        let shard = zml_shard();
        match size {
            Some(size) => self.book(shard, base, size, false),
            None => {
                self.unsizable.fetch_add(1, Ordering::Relaxed);
            }
        }
        if legacy {
            self.legacy[shard].lock().push(base);
        }
    }

    /// A blackened range `[lo, hi)`.
    pub(crate) fn note_range(&self, lo: usize, hi: usize) {
        if hi <= lo {
            return;
        }
        self.book(zml_shard(), lo, hi - lo, true);
        self.black.lock().push((lo, hi));
    }

    /// `size` bytes from `at` onto the region of `at`, the tail onto the
    /// later regions as straddle-in. A range books every byte it covers in a
    /// later region as live too (its objects may start there).
    #[inline]
    fn book(&self, shard: usize, at: usize, size: usize, range: bool) {
        let Some(off) = at.checked_sub(self.base) else {
            return;
        };
        let i = off / ZRELOC_REGION_BYTES;
        if i >= self.regions {
            return;
        }
        let t = &self.tables()[shard];
        let end = off.saturating_add(size);
        let first_end = ((i + 1) * ZRELOC_REGION_BYTES).min(end);
        // An object books its whole size onto its base's region, as
        // `region_liveness` does (the objects starting in one region are
        // disjoint, so a shard's sum stays under 2 MiB plus one object's
        // extent: no `u32` wraps).
        let first = if range { first_end - off } else { size };
        t.live_bytes[i].fetch_add(u32::try_from(first).unwrap_or(u32::MAX), Ordering::Relaxed);
        t.live_objects[i].fetch_add(1, Ordering::Relaxed);
        let mut r = i + 1;
        while r < self.regions && r * ZRELOC_REGION_BYTES < end {
            let r_lo = r * ZRELOC_REGION_BYTES;
            let part = (end.min(r_lo + ZRELOC_REGION_BYTES) - r_lo) as u32;
            t.straddle_in_bytes[r].fetch_add(part, Ordering::Relaxed);
            if range {
                t.live_bytes[r].fetch_add(part, Ordering::Relaxed);
                t.live_objects[r].fetch_add(1, Ordering::Relaxed);
            }
            r += 1;
        }
    }

    /// The figures over `grid`, disarming; `None` when the cycle was not
    /// booked (not armed, or a grid the tables do not cover).
    fn take(&self, grid: &ZRegionGrid) -> Option<ZMarkTaken> {
        if !self.disarm() {
            return None;
        }
        let n = grid.region_count();
        if grid.region_base(0) != self.base
            || grid.region_bytes() != ZRELOC_REGION_BYTES
            || n > self.regions
        {
            return None;
        }
        let tables = self.tables();
        let sum = |f: fn(&ZMarkLivenessTable) -> &[AtomicU32]| -> Vec<usize> {
            (0..n)
                .map(|i| {
                    tables
                        .iter()
                        .map(|t| f(t)[i].load(Ordering::Acquire) as usize)
                        .sum()
                })
                .collect()
        };
        let liveness = ZRegionLiveness {
            live_bytes: sum(|t| &t.live_bytes),
            live_objects: sum(|t| &t.live_objects),
            straddle_in_bytes: sum(|t| &t.straddle_in_bytes),
            outside: 0,
            unsizable: self.unsizable.load(Ordering::Acquire),
        };
        let mut legacy = Vec::new();
        for s in self.legacy.iter() {
            legacy.append(&mut s.lock());
        }
        let black = std::mem::take(&mut *self.black.lock());
        Some(ZMarkTaken {
            liveness,
            legacy,
            black,
        })
    }
}

/// `CRATONVM_DBG_ZGC_CRL_NO_LEGACY_PASS` (debug, the walk check's positive
/// control): the plan skips its legacy-holder pass, so a legacy cell naming a
/// planned object is left for the check to find. Read once per heap.
fn zgc_crl_no_legacy_pass() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ZGC_CRL_NO_LEGACY_PASS")
}

/// What one slot of a rewrite pass rewrote (the walk check's categories).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZRewriteKind {
    /// An uncolored word in a compact field or an array element.
    Plain,
    /// A colored word in another color than the closing mark's.
    Color,
    /// A legacy 16-byte cell's payload.
    Legacy,
}

/// Why a closing pause kept the slide (`crl_last_refusal=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ZConcRelocRefusal {
    /// The colored barrier with the color flip is not armed, or R4
    /// (`CRATONVM_ZGC_REMAP_IN_MARK`) is off: nothing would heal a stale word.
    NoBarrier = 1,
    /// Generational mode (a young mark does not visit the old generation).
    Generational = 2,
    /// The cycle was marked stop-the-world: no mark color to leave bad.
    NoMarkColor = 3,
    /// No region grid over the arena, or a survivor could not be sized.
    NoGrid = 4,
    /// Nothing worth relocating.
    Empty = 5,
    /// The to-space window could not hold the plan (R7's gate).
    ToSpace = 6,
    /// A cycle from an earlier pause is still published.
    Busy = 7,
}

const ZL_PENDING: u8 = 0;
const ZL_CLAIMED: u8 = 1;
const ZL_COPIED: u8 = 2;

/// Who copied a planned object (the census).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZLazyCopier {
    /// Inside the pause: the rewrite walk, the probe, the map's hit hook.
    Pause,
    /// A pointer-map lookup after the release (a thread's frame remap).
    Hit,
    /// The load barrier, the marker or the SATB forward.
    Barrier,
    /// The collecting thread's drain.
    Drain,
}

/// One planned survivor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZLazyObject {
    from: usize,
    size: usize,
    to: usize,
}

/// One selected region: its planned survivors (ascending by `from`), their copy
/// states, the free blocks the plan withheld, and the region's extent below
/// the plan's low end (absolute `[lo, hi)`).
struct ZLazyRegion {
    objects: Box<[ZLazyObject]>,
    state: Box<[AtomicU8]>,
    withheld: Vec<(usize, usize)>,
    extent: (usize, usize),
}

/// One pause's plan, shared by `Arc` with the hot paths until the release.
pub(crate) struct ZLazyCycle {
    grid: ZRegionGrid,
    /// Per grid region: index into `regions`, or `u32::MAX`.
    slot_of_region: Box<[u32]>,
    regions: Box<[ZLazyRegion]>,
    planned: usize,
    planned_bytes: usize,
    /// Objects copied so far (any copier).
    copied: AtomicUsize,
    /// The drain finished (every object copied).
    drained: AtomicBool,
    /// t1 wave 3 lane Q (R-2), under `CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK`:
    /// each copied object's from-space bytes as the copy read them
    /// (`(from, bytes)`), compared with the from-space at the release. A
    /// difference is a write that landed in from-space AFTER its object was
    /// copied (a pause consumer, or any path, writing through an address it
    /// held from before the plan): a lost write. `None` when the check is off.
    from_snapshots: Option<Mutex<Vec<(usize, Box<[u8]>)>>>,
}

impl ZLazyCycle {
    #[inline]
    fn find(&self, addr: usize) -> Option<(&ZLazyRegion, usize)> {
        let r = self.grid.region_of(addr)? as usize;
        let s = *self.slot_of_region.get(r)?;
        if s == u32::MAX {
            return None;
        }
        let reg = &self.regions[s as usize];
        let i = reg.objects.binary_search_by_key(&addr, |o| o.from).ok()?;
        Some((reg, i))
    }

    /// The planned destination of `addr`, if it is a planned survivor. No copy.
    #[inline]
    pub(crate) fn planned(&self, addr: usize) -> Option<usize> {
        self.find(addr).map(|(reg, i)| reg.objects[i].to)
    }

    /// Does `addr` lie in a selected region?
    #[inline]
    pub(crate) fn in_selected_region(&self, addr: usize) -> bool {
        self.grid
            .region_of(addr)
            .and_then(|r| self.slot_of_region.get(r as usize))
            .is_some_and(|&s| s != u32::MAX)
    }

    fn all_objects(&self) -> impl Iterator<Item = (&ZLazyRegion, usize)> + '_ {
        self.regions
            .iter()
            .flat_map(|reg| (0..reg.objects.len()).map(move |i| (reg, i)))
    }
}

/// The per-heap state (inside the boxed `ZgcCounters`).
pub(crate) struct ZConcRelocState {
    enabled: AtomicBool,
    /// `CRATONVM_DBG_ZGC_CRL_WALK_CHECK`.
    walk_check: AtomicBool,
    /// `CRATONVM_DBG_ZGC_CRL_NO_LEGACY_PASS`.
    no_legacy_pass: AtomicBool,
    /// The published cycle for the hot paths; null when none.
    published: AtomicPtr<ZLazyCycle>,
    /// The owner of the published cycle.
    cycle: Mutex<Option<Arc<ZLazyCycle>>>,
    cycles: AtomicUsize,
    refused: AtomicUsize,
    last_refusal: AtomicU8,
    regions: AtomicUsize,
    /// Selected regions left out of a plan: their survivors did not fit
    /// the to-space bump tail.
    trimmed: AtomicUsize,
    planned: AtomicUsize,
    planned_bytes: AtomicUsize,
    eager: AtomicUsize,
    hit: AtomicUsize,
    barrier: AtomicUsize,
    drain: AtomicUsize,
    waits: AtomicUsize,
    released_bytes: AtomicUsize,
    /// Regions released piecewise: a base was still registered inside.
    release_kept: AtomicUsize,
    /// Plans made by a pause that parked its sweep (6b).
    deferred: AtomicUsize,
    /// zt t33: legacy-shaped survivors the plans' legacy passes walked.
    legacy_holders: AtomicUsize,
    /// zt t33: plans the walk check ran on, and what it still had to rewrite
    /// (plain, other color, legacy).
    walk_checked: AtomicUsize,
    walk_missed: [AtomicUsize; 3],
    /// zt t34: the mark's inputs (`CRATONVM_ZGC_CRL_MARK_LIVENESS`).
    pub(crate) live: ZMarkLiveness,
    /// `CRATONVM_DBG_ZGC_CRL_LIVENESS_CHECK`.
    liveness_check: AtomicBool,
    /// zt t34: plans made from the mark's liveness, and plans that found it
    /// not kept for their cycle and sized the live set instead.
    mark_plans: AtomicUsize,
    mark_fallbacks: AtomicUsize,
    /// zt t34: selected regions the exact re-read of their survivors took out.
    exact_dropped: AtomicUsize,
    /// zt t34: objects in the mark's blackened ranges the plans scanned, and
    /// the legacy holders among them.
    black_scanned: AtomicUsize,
    black_legacy: AtomicUsize,
    /// zt t34, the liveness check: plans checked, regions whose estimate was
    /// below the exact figure, legacy holders the mark's list missed.
    check: [AtomicUsize; 3],
    /// t1 wave 3 lane Q (R-1 of `t1-Z-concurrent-relocation-and-satb-review-residuals`):
    /// the dead-at-release census of [`super::ZgcRealHeap::release_concurrent_relocate`]
    /// under `CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK=1`: releases checked,
    /// reference words (colored words decoded) that still named a released span,
    /// and the part of those whose holder the closing mark found live (marked
    /// or born black). `[2]` must read 0: a live holder naming freed memory is
    /// the use-after-free R-1 describes. `[1]` minus `[2]` is dead holders.
    dead_check: [AtomicUsize; 3],
    /// t1 Q (R-2): copied objects whose from-space changed between their copy
    /// and the release (under the same debug switch). Must read 0.
    from_writes: AtomicUsize,
}

/// `[GC] zgc-concurrent-relocate:` figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZConcRelocReport {
    pub enabled: bool,
    pub cycles: usize,
    pub refused: usize,
    pub last_refusal: u8,
    pub regions: usize,
    pub trimmed: usize,
    pub planned: usize,
    pub planned_bytes: usize,
    pub eager: usize,
    pub hit: usize,
    pub barrier: usize,
    pub drain: usize,
    pub waits: usize,
    pub released_bytes: usize,
    pub release_kept: usize,
    pub deferred: usize,
    pub legacy_holders: usize,
    pub walk_checked: usize,
    /// Plain, other color, legacy.
    pub walk_missed: [usize; 3],
    pub mark_plans: usize,
    pub mark_fallbacks: usize,
    pub exact_dropped: usize,
    pub black_scanned: usize,
    pub black_legacy: usize,
    /// Plans checked, regions under-estimated, legacy holders missed.
    pub check: [usize; 3],
    /// t1 Q (R-1): releases checked, stale words, stale words in live holders.
    pub dead_check: [usize; 3],
    /// t1 Q (R-2): copied objects written in from-space after their copy.
    pub from_writes: usize,
}

impl ZConcRelocState {
    /// `base` and `span`: the arena the mark's region tables cover.
    pub(crate) fn new(enabled: bool, base: usize, span: usize) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            walk_check: AtomicBool::new(enabled && zgc_crl_walk_check_enabled()),
            no_legacy_pass: AtomicBool::new(enabled && zgc_crl_no_legacy_pass()),
            published: AtomicPtr::new(std::ptr::null_mut()),
            cycle: Mutex::new(None),
            cycles: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            last_refusal: AtomicU8::new(0),
            regions: AtomicUsize::new(0),
            trimmed: AtomicUsize::new(0),
            planned: AtomicUsize::new(0),
            planned_bytes: AtomicUsize::new(0),
            eager: AtomicUsize::new(0),
            hit: AtomicUsize::new(0),
            barrier: AtomicUsize::new(0),
            drain: AtomicUsize::new(0),
            waits: AtomicUsize::new(0),
            released_bytes: AtomicUsize::new(0),
            release_kept: AtomicUsize::new(0),
            deferred: AtomicUsize::new(0),
            legacy_holders: AtomicUsize::new(0),
            walk_checked: AtomicUsize::new(0),
            walk_missed: Default::default(),
            live: ZMarkLiveness::new(zgc_crl_mark_liveness_enabled(), base, span),
            liveness_check: AtomicBool::new(enabled && zgc_crl_liveness_check_enabled()),
            mark_plans: AtomicUsize::new(0),
            mark_fallbacks: AtomicUsize::new(0),
            exact_dropped: AtomicUsize::new(0),
            black_scanned: AtomicUsize::new(0),
            black_legacy: AtomicUsize::new(0),
            check: Default::default(),
            dead_check: Default::default(),
            from_writes: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn published(&self) -> bool {
        !self.published.load(Ordering::Relaxed).is_null()
    }

    fn note_refused(&self, why: ZConcRelocRefusal) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        self.last_refusal.store(why as u8, Ordering::Relaxed);
    }

    fn note_copy(&self, who: ZLazyCopier) {
        let c = match who {
            ZLazyCopier::Pause => &self.eager,
            ZLazyCopier::Hit => &self.hit,
            ZLazyCopier::Barrier => &self.barrier,
            ZLazyCopier::Drain => &self.drain,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_deferred_plan(&self) {
        self.deferred.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn set_enabled_for_test(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn set_liveness_check_for_test(&self, on: bool) {
        self.liveness_check.store(on, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn set_walk_check_for_test(&self, check: bool, no_legacy_pass: bool) {
        self.walk_check.store(check, Ordering::Relaxed);
        self.no_legacy_pass.store(no_legacy_pass, Ordering::Relaxed);
    }
}

impl super::ZgcRealHeap {
    /// zt t32: the `[GC] zgc-concurrent-relocate:` summary figures.
    pub fn concurrent_relocate_report(&self) -> ZConcRelocReport {
        let st = &self.counters.crl;
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        ZConcRelocReport {
            enabled: st.enabled(),
            cycles: l(&st.cycles),
            refused: l(&st.refused),
            last_refusal: st.last_refusal.load(Ordering::Relaxed),
            regions: l(&st.regions),
            trimmed: l(&st.trimmed),
            planned: l(&st.planned),
            planned_bytes: l(&st.planned_bytes),
            eager: l(&st.eager),
            hit: l(&st.hit),
            barrier: l(&st.barrier),
            drain: l(&st.drain),
            waits: l(&st.waits),
            released_bytes: l(&st.released_bytes),
            release_kept: l(&st.release_kept),
            deferred: l(&st.deferred),
            legacy_holders: l(&st.legacy_holders),
            walk_checked: l(&st.walk_checked),
            walk_missed: [
                l(&st.walk_missed[0]),
                l(&st.walk_missed[1]),
                l(&st.walk_missed[2]),
            ],
            mark_plans: l(&st.mark_plans),
            mark_fallbacks: l(&st.mark_fallbacks),
            exact_dropped: l(&st.exact_dropped),
            black_scanned: l(&st.black_scanned),
            black_legacy: l(&st.black_legacy),
            check: st.check.each_ref().map(l),
            dead_check: st.dead_check.each_ref().map(l),
            from_writes: l(&st.from_writes),
        }
    }

    /// zt t32: may this closing pause relocate concurrently instead of
    /// sliding? The switch, then the refusals (counted). A `false` with the
    /// switch on keeps the slide.
    pub(super) fn concurrent_relocate_admitted(&self, young_cycle: bool) -> bool {
        let st = &self.counters.crl;
        if !st.enabled() {
            return false;
        }
        if !self.counters.rim.enabled()
            || !self.load_barrier_colored_stores()
            || !self.load_barrier_color_flip()
        {
            st.note_refused(ZConcRelocRefusal::NoBarrier);
            return false;
        }
        if self.generational_enabled() || young_cycle {
            st.note_refused(ZConcRelocRefusal::Generational);
            return false;
        }
        if st.published() {
            st.note_refused(ZConcRelocRefusal::Busy);
            return false;
        }
        true
    }

    /// zt t33: may a closing pause of this heap plan (R6)? The refusals of
    /// [`Self::concurrent_relocate_admitted`] that the pause's remap flip can
    /// already answer, uncounted: the switch, R4, the colored barrier with the
    /// flip, not generational. Decides the closing heal color
    /// (`barrier_heal_mask`); a `true` the pause then refuses costs nothing,
    /// since a slide ends that color first.
    pub(super) fn concurrent_relocate_may_plan(&self) -> bool {
        self.counters.crl.enabled()
            && self.counters.rim.enabled()
            && self.load_barrier_colored_stores()
            && self.load_barrier_color_flip()
            && !self.generational_enabled()
    }

    /// zt t34: start booking the R6 plan's inputs for the concurrent cycle
    /// this mark-start pause opens ([`ZMarkLiveness`]), when a plan may close
    /// it; otherwise keep none. **In the mark-start pause**, after the mark
    /// bits are cleared and before the first claim or blackening.
    pub(super) fn crl_liveness_arm_for_mark(&self) {
        let live = &self.counters.crl.live;
        if live.on && self.mark_bits.is_some() && self.concurrent_relocate_may_plan() {
            live.arm();
        } else {
            live.disarm();
        }
    }

    /// zt t34: this cycle keeps no liveness for an R6 plan (an abandoned
    /// concurrent mark, a stop-the-world mark).
    pub(super) fn crl_liveness_invalidate(&self) {
        self.counters.crl.live.disarm();
    }

    /// zt t34: book the object at `base`, just marked, into the R6 plan's
    /// liveness: its size and, when it is legacy-shaped, its base. The
    /// caller checked that the liveness is armed.
    #[inline(never)]
    pub(super) fn crl_liveness_note_marked(&self, base: usize) {
        let header = self.header_ref(base as *mut u8);
        let legacy = header.kind() == cratonvm_types::ObjectKind::Object
            && !cratonvm_types::is_compact_object(header);
        self.counters
            .crl
            .live
            .note_object(base, self.live_extent(base, header), legacy);
    }

    /// zt t32 (R6): the closing pause's plan, in place of the slide. `live`
    /// produces the post-sweep registry (ascending) for the paths that still
    /// read the whole live set: t33's sizing loop when the mark kept no
    /// liveness for this cycle (`CRATONVM_ZGC_CRL_MARK_LIVENESS=0`, a cycle not
    /// armed at its mark start), and the two debug checks. Returns `None`
    /// (counted) when the cycle keeps the slide after all, else the planned
    /// count and the pointer map of every planned pair, its hit hook armed
    /// (module doc). **Inside the pause, after the sweep**, the world stopped.
    ///
    /// t1 wave 3 lane Q (R-2), THE WRITE-AFTER-COPY INVARIANT: once this
    /// returns, no consumer may WRITE through a planned object's old address
    /// it held from before the plan. Every pause consumer that runs after it
    /// (the roots, the monitor and reference-processor tables, the VM's remap
    /// epilogue, each thread's frame remap) learns a planned address through a
    /// pointer-map lookup whose hit hook copies first, and writes the copy;
    /// one that skipped the lookup and wrote the old address would write
    /// from-space after (or before) the copy, and the write is lost either at
    /// the copy or at the release. Checked, not argued:
    /// `CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK=1` snapshots each copied
    /// object's from-space bytes at the copy and compares them at the release
    /// (`crl_from_writes=`, must read 0).
    pub(super) fn concurrent_relocate_start(
        &self,
        live: &dyn Fn() -> Vec<usize>,
    ) -> Option<(usize, cratonvm_types::PointerMap)> {
        let st = &self.counters.crl;
        let started = std::time::Instant::now();
        // The closing mark's color: the words the rewrite leaves bad. Read,
        // not taken: a refusal below leaves it to the slide's R4.
        let skip = self.counters.rim_last_mark_color.load(Ordering::Acquire);
        if skip == 0 {
            st.live.disarm();
            st.note_refused(ZConcRelocRefusal::NoMarkColor);
            return None;
        }
        // The slide's pins: JNI critical sections, the JIT's conservative
        // words, the published TLAB tails.
        let mut pins = self.critical_pin_addrs();
        pins.extend(crate::gc_quiescence::pinned_jit_roots_snapshot());
        for (lo, hi) in self.jit_tlab_skip_regions() {
            pins.push(lo);
            pins.push(hi - 1);
        }
        let (base, low_end) = {
            let arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            (base, base + arena.used_low_for_compaction())
        };
        let Some(grid) = ZRegionGrid::new(base, low_end, ZRELOC_REGION_BYTES) else {
            st.live.disarm();
            st.note_refused(ZConcRelocRefusal::NoGrid);
            return None;
        };
        // zt t34: the mark's figures (`ZMarkLiveness`), taken whatever follows.
        let marked = st.live.take(&grid);
        if marked.is_none() && st.live.on {
            st.mark_fallbacks.fetch_add(1, Ordering::Relaxed);
        }
        let check = marked.is_some() && st.liveness_check.load(Ordering::Relaxed);
        let walk_check = st.walk_check.load(Ordering::Relaxed);
        let full_live: Option<Vec<usize>> = (marked.is_none() || check || walk_check).then(live);
        let is_legacy = |header: &cratonvm_types::ObjectHeader| {
            header.kind() == cratonvm_types::ObjectKind::Object
                && !cratonvm_types::is_compact_object(header)
        };
        // t33's sizing loop over the whole live set: every survivor in the
        // grid sized, and the legacy-shaped ones wherever they lie (one header
        // read each). `None`: a survivor could not be sized.
        let size_all = |all: &[usize]| -> Option<(Vec<(usize, usize)>, Vec<usize>)> {
            let mut sized: Vec<(usize, usize)> = Vec::with_capacity(all.len());
            let mut legacy: Vec<usize> = Vec::new();
            for &b in all {
                let header = self.header_ref(b as *mut u8);
                if is_legacy(header) {
                    legacy.push(b);
                }
                if b < base || b >= low_end {
                    continue;
                }
                sized.push((b, self.survivor_size(b, header)?));
            }
            Some((sized, legacy))
        };
        let walked = match &full_live {
            Some(all) if marked.is_none() || check => match size_all(all) {
                Some(w) => Some(w),
                None => {
                    st.note_refused(ZConcRelocRefusal::NoGrid);
                    return None;
                }
            },
            _ => None,
        };
        let from_mark = marked.is_some();
        let mut black_scanned = 0usize;
        let (liveness, legacy, sized_all) = match marked {
            None => {
                let (sized, legacy) = walked.expect("sized above");
                (region_liveness(&grid, sized.iter().copied()), legacy, Some(sized))
            }
            Some(m) => {
                if m.liveness.unsizable > 0 {
                    st.note_refused(ZConcRelocRefusal::NoGrid);
                    return None;
                }
                // The claimed legacy holders still registered at their base (a
                // holder the previous plan copied during this mark was claimed
                // at its destination).
                let mut legacy: Vec<usize> = m
                    .legacy
                    .into_iter()
                    .filter(|&b| self.registry.contains(b))
                    .collect();
                // The legacy holders born black: the objects of the blackened
                // ranges, whose headers allocation wrote and the mark never
                // read.
                let mut ranges = m.black;
                ranges.sort_unstable();
                let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
                for (lo, hi) in ranges {
                    match merged.last_mut() {
                        Some(last) if last.1 >= lo => last.1 = last.1.max(hi),
                        _ => merged.push((lo, hi)),
                    }
                }
                let born: Vec<usize> = merged
                    .iter()
                    .flat_map(|&(lo, hi)| self.registry.bases_in_span(lo, hi))
                    .collect();
                black_scanned = born.len();
                let found: Vec<usize> = self
                    .reloc_par_chunks(&born, self.relocate_workers_for(born.len()), |chunk| {
                        chunk
                            .iter()
                            .copied()
                            .filter(|&b| is_legacy(self.header_ref(b as *mut u8)))
                            .collect::<Vec<usize>>()
                    })
                    .into_iter()
                    .flatten()
                    .collect();
                st.black_scanned.fetch_add(black_scanned, Ordering::Relaxed);
                st.black_legacy.fetch_add(found.len(), Ordering::Relaxed);
                legacy.extend(found);
                legacy.sort_unstable();
                legacy.dedup();
                // CRATONVM_DBG_ZGC_CRL_LIVENESS_CHECK: the estimate must bound
                // the exact figures, and the list must hold every legacy holder.
                if let Some((sized, walk_legacy)) = &walked {
                    let exact = region_liveness(&grid, sized.iter().copied());
                    let est = &m.liveness;
                    let under_at: Vec<usize> = (0..grid.region_count())
                        .filter(|&i| {
                            exact.live_bytes[i] > est.live_bytes[i]
                                || exact.straddle_in_bytes[i] > est.straddle_in_bytes[i]
                        })
                        .collect();
                    let under = under_at.len();
                    if self.gc_log_enabled.load(Ordering::Relaxed) {
                        for &i in under_at.iter().take(4) {
                            // The region's survivors the mark did not book, by
                            // what they are: in a blackened range or not, and
                            // in the previous plan's to-space or not.
                            let lo = grid.region_base(i as u64);
                            let hi = (lo + grid.region_bytes()).min(grid.end());
                            let objs = self.registry.bases_in_span(lo, hi);
                            let marked = objs.iter().filter(|&&b| self.mark_is_set(b)).count();
                            let sample: Vec<String> = objs
                                .iter()
                                .take(3)
                                .map(|&b| {
                                    let h = self.header_ref(b as *mut u8);
                                    format!(
                                        "{:#x}:{:?}/{}/m{}",
                                        b - base,
                                        h.kind(),
                                        h.class_id.as_u32(),
                                        self.mark_is_set(b) as u8
                                    )
                                })
                                .collect();
                            eprintln!(
                                "[GC] zgc-crl-liveness-under: region={i} est={}/{}/{} \
                                 exact={}/{}/{} registered={} marked={marked} sample={sample:?}",
                                est.live_bytes[i],
                                est.live_objects[i],
                                est.straddle_in_bytes[i],
                                exact.live_bytes[i],
                                exact.live_objects[i],
                                exact.straddle_in_bytes[i],
                                objs.len(),
                            );
                        }
                    }
                    let have: rustc_hash::FxHashSet<usize> = legacy.iter().copied().collect();
                    let missed = walk_legacy.iter().filter(|b| !have.contains(b)).count();
                    st.check[0].fetch_add(1, Ordering::Relaxed);
                    st.check[1].fetch_add(under, Ordering::Relaxed);
                    st.check[2].fetch_add(missed, Ordering::Relaxed);
                    if self.gc_log_enabled.load(Ordering::Relaxed) {
                        eprintln!(
                            "[GC] zgc-crl-liveness-check: regions={} under={under} \
                             legacy={} walk_legacy={} legacy_missed={missed} live={} \
                             est_live_bytes={} exact_live_bytes={}",
                            grid.region_count(),
                            legacy.len(),
                            walk_legacy.len(),
                            sized.len(),
                            est.live_bytes.iter().sum::<usize>(),
                            exact.live_bytes.iter().sum::<usize>(),
                        );
                    }
                }
                (m.liveness, legacy, None)
            }
        };
        let mut excluded: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
        for &p in &pins {
            if let Some(r) = grid.region_of(p) {
                excluded.insert(r);
            }
            for probe in [Some(p), p.checked_sub(1)].into_iter().flatten() {
                if let Some(obj) = self.resolve_interior_for_pin(probe) {
                    if let Some(r) = grid.region_of(obj.as_ptr() as usize) {
                        excluded.insert(r);
                    }
                }
            }
        }
        let mut policy = super::forwarding::ZRelocationPolicy::default();
        policy.max_evacuation_bytes = super::zgc_relocate_budget_bytes();
        let selection =
            select_relocation_regions(&grid, &liveness, &policy, |r| excluded.contains(&r));
        if !selection.trustworthy() || selection.set.is_empty() {
            st.note_refused(ZConcRelocRefusal::Empty);
            return None;
        }
        let mut ids = selection.set.page_ids();
        ids.sort_unstable();
        // zt t34: from the mark, the selected regions' survivors are read
        // exactly -- the registry's bases in each, sized, and the object
        // reaching in from below -- and the selection is made again over the
        // exact figures, among the regions the estimate chose: what it rejects
        // leaves the set. O(selected survivors), not O(live).
        let sized: Vec<(usize, usize)> = match sized_all {
            Some(sized) => sized,
            None => {
                let first: rustc_hash::FxHashSet<u64> = ids.iter().copied().collect();
                let mut objs: Vec<(usize, usize)> = Vec::new();
                for &id in &ids {
                    let lo = grid.region_base(id);
                    let hi = (lo + grid.region_bytes()).min(grid.end());
                    if lo > base {
                        if let Some(p) = self.registry.nearest_base_within_total(lo - 1, base) {
                            let header = self.header_ref(p as *mut u8);
                            match self.survivor_size(p, header) {
                                Some(sz) if p + sz > lo => objs.push((p, sz)),
                                Some(_) => {}
                                None => {
                                    st.note_refused(ZConcRelocRefusal::NoGrid);
                                    return None;
                                }
                            }
                        }
                    }
                    for b in self.registry.bases_in_span(lo, hi) {
                        let header = self.header_ref(b as *mut u8);
                        match self.survivor_size(b, header) {
                            Some(sz) => objs.push((b, sz)),
                            None => {
                                st.note_refused(ZConcRelocRefusal::NoGrid);
                                return None;
                            }
                        }
                    }
                }
                objs.sort_unstable();
                objs.dedup();
                let exact = region_liveness(&grid, objs.iter().copied());
                let again = select_relocation_regions(&grid, &exact, &policy, |r| {
                    excluded.contains(&r) || !first.contains(&r)
                });
                let mut kept = if again.trustworthy() {
                    again.set.page_ids()
                } else {
                    Vec::new()
                };
                kept.sort_unstable();
                st.exact_dropped
                    .fetch_add(ids.len() - kept.len(), Ordering::Relaxed);
                if kept.is_empty() {
                    st.note_refused(ZConcRelocRefusal::Empty);
                    return None;
                }
                ids = kept;
                objs
            }
        };
        if from_mark {
            st.mark_plans.fetch_add(1, Ordering::Relaxed);
        }
        // Phase marks for the plan's log line (`size_us` / `plan_us` / `eager_us`).
        let sized_us = started.elapsed().as_micros();
        // Survivors per selected region, ascending (`sized` is ascending).
        let mut slot_of_region = vec![u32::MAX; grid.region_count()].into_boxed_slice();
        for (k, &id) in ids.iter().enumerate() {
            slot_of_region[id as usize] = k as u32;
        }
        let mut per_region: Vec<Vec<ZLazyObject>> = vec![Vec::new(); ids.len()];
        for &(from, size) in &sized {
            if let Some(r) = grid.region_of(from) {
                let s = slot_of_region[r as usize];
                if s != u32::MAX {
                    per_region[s as usize].push(ZLazyObject { from, size, to: 0 });
                }
            }
        }
        // THE PLAN: every destination at the bump tail, in the slide's order.
        // A refused allocation abandons the plan; the bytes already bumped
        // hold no registered base, so the next complement sweep frees them.
        let mut arena = self.arena.lock();
        if arena.used_low_for_compaction() + base != low_end || arena.open_to_space().is_none() {
            st.note_refused(ZConcRelocRefusal::ToSpace);
            return None;
        }
        // A region whose survivors do not all fit leaves the set, with every
        // region after it: the plan keeps the regions it placed whole (the
        // set is in address order, the bump tail serves them in turn).
        let mut placed = per_region.len();
        'plan: for (k, objs) in per_region.iter_mut().enumerate() {
            for o in objs.iter_mut() {
                match arena.alloc_to_space(o.size, 8) {
                    Some(p) => o.to = p as usize,
                    None => {
                        placed = k;
                        break 'plan;
                    }
                }
            }
        }
        let _ = arena.close_to_space();
        if placed == 0 {
            st.note_refused(ZConcRelocRefusal::ToSpace);
            return None;
        }
        if placed < per_region.len() {
            st.trimmed
                .fetch_add(per_region.len() - placed, Ordering::Relaxed);
            for &id in &ids[placed..] {
                slot_of_region[id as usize] = u32::MAX;
            }
            per_region.truncate(placed);
            ids.truncate(placed);
        }
        // THE SELECTED REGIONS LEAVE THE FREE LIST: their holes are withheld
        // until the release, so no allocation lands among the planned objects.
        let placed_us = started.elapsed().as_micros();
        let blocks = arena.take_low_free_blocks();
        let mut kept: Vec<(usize, usize)> = Vec::with_capacity(blocks.len());
        let mut withheld: Vec<Vec<(usize, usize)>> = vec![Vec::new(); ids.len()];
        for (off, sz) in blocks {
            let (lo, hi) = (base + off, base + off + sz);
            let mut cur = lo;
            while cur < hi {
                let r = grid.region_of(cur);
                let r_end = r.map_or(hi, |r| (grid.region_base(r) + grid.region_bytes()).min(hi));
                let piece = (cur, r_end.max(cur + 1).min(hi));
                match r.map(|r| slot_of_region[r as usize]) {
                    Some(s) if s != u32::MAX => withheld[s as usize].push(piece),
                    _ => kept.push((piece.0 - base, piece.1 - piece.0)),
                }
                cur = piece.1;
            }
        }
        // Merge adjacent kept pieces back into whole blocks.
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(kept.len());
        for (off, len) in kept {
            match merged.last_mut() {
                Some(last) if last.0 + last.1 == off => last.1 += len,
                _ => merged.push((off, len)),
            }
        }
        arena.rebuild_low_free_list(&merged);
        drop(arena);
        let mut planned = 0usize;
        let mut planned_bytes = 0usize;
        let regions: Box<[ZLazyRegion]> = per_region
            .into_iter()
            .zip(withheld)
            .zip(ids.iter())
            .map(|((objs, withheld), &id)| {
                let lo = grid.region_base(id);
                let extent = (lo, (lo + grid.region_bytes()).min(grid.end()));
                planned += objs.len();
                planned_bytes += objs.iter().map(|o| o.size).sum::<usize>();
                let state = (0..objs.len()).map(|_| AtomicU8::new(ZL_PENDING)).collect();
                ZLazyRegion {
                    objects: objs.into_boxed_slice(),
                    state,
                    withheld,
                    extent,
                }
            })
            .collect();
        let cycle = Arc::new(ZLazyCycle {
            grid,
            slot_of_region,
            regions,
            planned,
            planned_bytes,
            copied: AtomicUsize::new(0),
            drained: AtomicBool::new(false),
            from_snapshots: super::zgc_dbg_quarantine_dead_check_enabled()
                .then(|| Mutex::new(Vec::new())),
        });
        // THE LEGACY PASS, before any copy (module doc): every legacy cell
        // naming a planned object is rewritten to the planned address; its
        // target joins the eager set. No other word can read good after the
        // release (module doc), so no other survivor is walked.
        // Fanned out on the relocation pool (`reloc_par_chunks`): every slot
        // is written by the one worker whose survivor holds it.
        let rewrite_pass = |holders: &[usize], missed: Option<&[AtomicUsize; 3]>| -> Vec<usize> {
            let workers = self.relocate_workers_for(holders.len());
            self.reloc_par_chunks(holders, workers, |chunk: &[usize]| {
                let mut part: Vec<usize> = Vec::new();
                for &b in chunk {
                    self.reference_slots_with(
                        b as u64,
                        &mut |obs: super::census::ZSlotObservation| {
                            let (shape, slot) = (obs.shape, obs.slot_addr);
                            // SAFETY: as in the rewrite (`slot_addr` of a survivor).
                            let before = unsafe {
                                (*(slot as *const std::sync::atomic::AtomicU64))
                                    .load(Ordering::Relaxed)
                            };
                            let kind =
                                self.concurrent_relocate_rewrite_slot(&cycle, skip, obs, &mut part);
                            if let (Some(kind), Some(missed)) = (kind, missed) {
                                let k = missed[kind as usize].fetch_add(1, Ordering::Relaxed);
                                if k < 24 {
                                    let h = self.header_ref(b as *mut u8);
                                    eprintln!(
                                        "[GC] zgc-crl-walk-miss: {kind:?} holder={b:#x} class={} \
                                         obj_kind={:?} compact={} shape={shape:?} off={} \
                                         meta={:#x} skip={skip:#x} word={before:#x} \
                                         marked={} heal={:#x}",
                                        h.class_id.as_u32(),
                                        h.kind(),
                                        cratonvm_types::is_compact_object(h) as u8,
                                        slot as usize - b,
                                        before & super::vaddr::Z_METADATA_MASK,
                                        self.mark_is_set(b) as u8,
                                        self.barrier_heal_mask.load(Ordering::Relaxed),
                                    );
                                }
                            }
                        },
                    );
                }
                part
            })
            .into_iter()
            .flatten()
            .collect()
        };
        let walk_started = std::time::Instant::now();
        st.legacy_holders.fetch_add(legacy.len(), Ordering::Relaxed);
        if st.walk_check.load(Ordering::Relaxed) && self.gc_log_enabled.load(Ordering::Relaxed) {
            // The legacy holders by (class id, field count), the largest first.
            let mut by: rustc_hash::FxHashMap<(u32, u32), usize> = Default::default();
            for &b in &legacy {
                let h = self.header_ref(b as *mut u8);
                *by.entry((h.class_id.as_u32(), h.num_slots())).or_default() += 1;
            }
            let mut by: Vec<_> = by.into_iter().collect();
            by.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            by.truncate(8);
            eprintln!("[GC] zgc-crl-legacy-classes: {by:?}");
        }
        let mut eager: Vec<usize> = if st.no_legacy_pass.load(Ordering::Relaxed) {
            Vec::new()
        } else {
            rewrite_pass(&legacy, None)
        };
        // The walk check (`CRATONVM_DBG_ZGC_CRL_WALK_CHECK`): the pre-t33 walk
        // over every survivor, after the legacy pass; anything it rewrites
        // is a word the claim above missed.
        let missed_before = st.walk_missed.each_ref().map(|a| a.load(Ordering::Relaxed));
        if st.walk_check.load(Ordering::Relaxed) {
            st.walk_checked.fetch_add(1, Ordering::Relaxed);
            eager.extend(rewrite_pass(
                full_live.as_deref().unwrap_or(&[]),
                Some(&st.walk_missed),
            ));
        }
        let walk_us = walk_started.elapsed().as_micros();
        // The pin / overlay owners (side tables keyed by the owner's base). The
        // filter is armed for the whole collection; disarmed it answers "may
        // own" for every base, so nothing is missed.
        for (reg, i) in cycle.all_objects() {
            let (pins, overlay) = self
                .counters
                .mark_root_filter
                .extra_root_probe(reg.objects[i].from);
            if pins || overlay {
                eager.push(reg.objects[i].from);
            }
        }
        for from in eager {
            if let Some((reg, i)) = cycle.find(from) {
                self.lazy_copy(&cycle, reg, i, ZLazyCopier::Pause);
            }
        }
        let eager_done_us = started.elapsed().as_micros();
        // THE MAP: every planned pair, a lookup copies (module doc).
        let mut pairs: Vec<(usize, usize)> = Vec::with_capacity(planned);
        for (reg, i) in cycle.all_objects() {
            pairs.push((reg.objects[i].from, reg.objects[i].to));
        }
        let mut map = cratonvm_types::PointerMap::from_pairs_sorted(pairs);
        // The heap by address: the hook runs after this call returns (each
        // thread's frame remap at resume), and a production heap is never
        // dropped (`ZgcRealHeap`'s `Drop` never runs in the shipped VM). A map
        // must not outlive the heap whose collection built it.
        let heap_addr = self as *const Self as usize;
        let hook_cycle = Arc::clone(&cycle);
        map.set_hit_hook(Some(Arc::new(move |from: usize| {
            // SAFETY: see `heap_addr` above.
            let heap = unsafe { &*(heap_addr as *const super::ZgcRealHeap) };
            if let Some((reg, i)) = hook_cycle.find(from) {
                let who = if heap.counters.crl.published() {
                    ZLazyCopier::Hit
                } else {
                    ZLazyCopier::Pause
                };
                heap.lazy_copy(&hook_cycle, reg, i, who);
            }
        })));
        self.counters
            .rim_last_mark_color
            .store(0, Ordering::Relaxed);
        st.cycles.fetch_add(1, Ordering::Relaxed);
        st.regions.fetch_add(ids.len(), Ordering::Relaxed);
        st.planned.fetch_add(planned, Ordering::Relaxed);
        st.planned_bytes.fetch_add(planned_bytes, Ordering::Relaxed);
        // Published last, still inside the pause.
        st.published
            .store(Arc::as_ptr(&cycle) as *mut ZLazyCycle, Ordering::Release);
        *st.cycle.lock() = Some(cycle);
        if self.gc_log_enabled.load(Ordering::Relaxed) {
            // Survivors: the walked list's length, or the mark's count.
            let live_n = full_live
                .as_ref()
                .map_or_else(|| liveness.live_objects.iter().sum(), Vec::len);
            let missed = st
                .walk_missed
                .each_ref()
                .map(|a| a.load(Ordering::Relaxed))
                .iter()
                .zip(missed_before)
                .map(|(now, before)| now - before)
                .collect::<Vec<_>>();
            eprintln!(
                "[GC] zgc-crl-plan: regions={} planned={planned} planned_bytes={planned_bytes} \
                 eager={} pins={} live={} legacy={} src={} sized={} black={black_scanned} \
                 size_us={sized_us} place_us={placed_us} \
                 walk_us={walk_us} eager_us={eager_done_us} missed={}/{}/{} us={}",
                ids.len(),
                st.eager.load(Ordering::Relaxed),
                pins.len(),
                live_n,
                legacy.len(),
                if from_mark { "mark" } else { "walk" },
                sized.len(),
                missed[0],
                missed[1],
                missed[2],
                started.elapsed().as_micros(),
            );
        }
        Some((planned, map))
    }

    /// One slot of a rewrite pass (the legacy pass, the walk check; see
    /// [`Self::concurrent_relocate_start`]): what it rewrote, if anything.
    fn concurrent_relocate_rewrite_slot(
        &self,
        cycle: &ZLazyCycle,
        skip: u64,
        obs: super::census::ZSlotObservation,
        eager: &mut Vec<usize>,
    ) -> Option<ZRewriteKind> {
        use super::census::ZSlotShape;
        if obs.raw_word == 0 {
            return None;
        }
        // `raw_word` is DECODED for the bare-word shapes (`read_ref_slot`
        // strips the color); the color decides the rule, so re-read the word
        // as stored, as R4's `remap_in_mark_skips` does.
        let word = match obs.shape {
            // SAFETY: an 8-byte-aligned reference word inside a survivor
            // (`ZSlotObservation::slot_addr`); the world is stopped.
            ZSlotShape::CompactField | ZSlotShape::ArrayElement => unsafe {
                (*(obs.slot_addr as *const std::sync::atomic::AtomicU64)).load(Ordering::Relaxed)
            },
            _ => obs.raw_word,
        };
        if word == 0 {
            return None;
        }
        let colored = super::vaddr::is_colored_word(word);
        // The common word first: a colored slot the closing mark visited
        // carries its color and stays bad, whatever it names (no lookup).
        if colored
            && word & super::vaddr::Z_METADATA_MASK == skip
            && matches!(
                obs.shape,
                ZSlotShape::CompactField | ZSlotShape::ArrayElement
            )
        {
            return None;
        }
        let target = if colored {
            cratonvm_types::narrow_oop::decode_colored_slot_word(word) as usize
        } else {
            word as usize
        };
        let to = cycle.planned(target)?;
        let kind = match obs.shape {
            ZSlotShape::CompactField | ZSlotShape::ArrayElement => {
                // (A word in the skip color returned above: bad from the
                // release on, healed lazily.)
                let desired = if colored {
                    self.gc_colored_word(to)
                } else {
                    to as u64
                };
                // SAFETY: an 8-byte-aligned reference word inside a survivor
                // (`ZSlotObservation::slot_addr`); the world is stopped.
                unsafe { std::ptr::write(obs.slot_addr as *mut u64, desired) };
                if colored {
                    ZRewriteKind::Color
                } else {
                    ZRewriteKind::Plain
                }
            }
            ZSlotShape::LegacyField => {
                if obs.legacy_tag != Some(cratonvm_types::FIELD_CELL_TAG_OBJECT) {
                    return None;
                }
                // SAFETY: as above; a legacy cell's payload word is plain.
                unsafe { std::ptr::write(obs.slot_addr as *mut u64, to as u64) };
                ZRewriteKind::Legacy
            }
            ZSlotShape::StaticField => return None,
        };
        eager.push(target);
        Some(kind)
    }

    /// Copy the planned object `i` of `reg` (or wait for the copier that
    /// claimed it), register its destination, drop its old base; the planned
    /// address.
    fn lazy_copy(
        &self,
        cycle: &ZLazyCycle,
        reg: &ZLazyRegion,
        i: usize,
        who: ZLazyCopier,
    ) -> usize {
        let o = reg.objects[i];
        let state = &reg.state[i];
        if state.load(Ordering::Acquire) == ZL_COPIED {
            return o.to;
        }
        if state
            .compare_exchange(ZL_PENDING, ZL_CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // SAFETY: `o.size` live bytes at `o.from` (a planned survivor, its
            // from-space intact until the release); `o.to` was handed out for
            // `o.size` bytes by this plan's to-space window, above every byte of
            // the selected regions. Nobody else writes either range: the claim
            // above is exclusive, and no mutator holds the from-space address.
            unsafe {
                std::ptr::copy_nonoverlapping(o.from as *const u8, o.to as *mut u8, o.size);
            }
            // t1 Q (R-2): the bytes the copy read, for the release's check.
            if let Some(snaps) = &cycle.from_snapshots {
                // SAFETY: the same `o.size` bytes the copy just read.
                let bytes = unsafe { std::slice::from_raw_parts(o.from as *const u8, o.size) };
                snaps.lock().push((o.from, bytes.into()));
            }
            self.registry.insert(o.to);
            self.registry.remove(o.from);
            state.store(ZL_COPIED, Ordering::Release);
            cycle.copied.fetch_add(1, Ordering::Relaxed);
            self.counters.crl.note_copy(who);
            return o.to;
        }
        self.counters.crl.waits.fetch_add(1, Ordering::Relaxed);
        // t1 wave 3 lane Q (R-3): every claimer today runs Rust with no
        // safepoint poll between its claim and its publish, so this wait ends;
        // a future claimer a pause could stop inside that window would hang the
        // waiting pause. Bounded so that shows: one error line after
        // `Z_LAZY_WAIT_WARN`, and a panic naming the object in debug builds
        // after `Z_LAZY_WAIT_PANIC`. t1 wave 5 (V3 item 2): the clock starts at
        // the FIRST yield (it started at the 1024th spin), so the bounds are
        // measured from the wait's start. A waiter must not hold the registry
        // lock or `from_snapshots` while it waits: the claimer takes both, so
        // such a waiter would deadlock (and, in debug, panic here at 60 s).
        const Z_LAZY_WAIT_WARN: std::time::Duration = std::time::Duration::from_secs(5);
        #[cfg(debug_assertions)]
        const Z_LAZY_WAIT_PANIC: std::time::Duration = std::time::Duration::from_secs(60);
        let mut spins = 0u32;
        let mut since: Option<std::time::Instant> = None;
        let mut warned = false;
        while state.load(Ordering::Acquire) != ZL_COPIED {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                let t0 = *since.get_or_insert_with(std::time::Instant::now);
                std::thread::yield_now();
                if spins % 1024 == 0 {
                    let waited = t0.elapsed();
                    if !warned && waited >= Z_LAZY_WAIT_WARN {
                        warned = true;
                        tracing::error!(
                            target: "zgc",
                            from = o.from,
                            to = o.to,
                            who = ?who,
                            "zgc concurrent relocate: a lazy copy has waited {waited:?} for \
                             the copier that claimed it (a claimer stopped between its claim \
                             and its publish?)"
                        );
                    }
                    #[cfg(debug_assertions)]
                    if waited >= Z_LAZY_WAIT_PANIC {
                        panic!(
                            "zgc concurrent relocate: lazy copy of {:#x} -> {:#x} ({who:?}) \
                             waited {waited:?} for its claimer",
                            o.from, o.to
                        );
                    }
                }
            }
        }
        o.to
    }

    /// zt t32: the hot paths' forward through the published plan -- the
    /// object's planned address, copied first. `None` when no plan is
    /// published or `addr` is not a planned survivor. One relaxed load when
    /// none is published.
    #[inline]
    pub(super) fn lazy_forwarded(&self, addr: usize) -> Option<usize> {
        let p = self.counters.crl.published.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: published from a live `Arc` the state's `cycle` slot owns
        // until `release_concurrent_relocate` withdraws the pointer, inside a
        // pause, before it drops the `Arc`; the hot paths hold no safepoint.
        let cycle = unsafe { &*p };
        let (reg, i) = cycle.find(addr)?;
        Some(self.lazy_copy(cycle, reg, i, ZLazyCopier::Barrier))
    }

    /// zt t32: is a plan published (the hot paths' cheap gate)?
    #[inline]
    pub(super) fn lazy_cycle_published(&self) -> bool {
        self.counters.crl.published()
    }

    /// zt t32 (R6): the collecting thread's drain after the release: copy every
    /// planned object no barrier copied yet, then zero the from-space. `true`
    /// when this call did work. Runs no Java and polls no safepoint (a pause
    /// requested meanwhile waits for it, as for the parked sweep).
    pub fn run_concurrent_relocate_drain(&self) -> bool {
        let Some(cycle) = self.counters.crl.cycle.lock().clone() else {
            return false;
        };
        if cycle.drained.load(Ordering::Acquire) {
            return false;
        }
        self.drain_lazy_cycle(&cycle, ZLazyCopier::Drain);
        true
    }

    fn drain_lazy_cycle(&self, cycle: &ZLazyCycle, who: ZLazyCopier) {
        for (reg, i) in cycle.all_objects() {
            self.lazy_copy(cycle, reg, i, who);
        }
        // The from-space stays as it is until the release zeroes it: a
        // deferred sweep parked by the planning pause sizes the planned
        // survivors from their old headers (its registry snapshot predates
        // every copy).
        cycle.drained.store(true, Ordering::Release);
    }

    /// zt t32 (R6, 6b): the selected regions' extents as arena offsets
    /// (`[lo, hi)`, ascending), for the sweep that a planning pause parked: no
    /// free span it installs may reach into them (the release frees them
    /// whole). Empty when nothing is planned.
    pub(super) fn concurrent_relocate_extents(&self) -> Vec<(usize, usize)> {
        let Some(cycle) = self.counters.crl.cycle.lock().clone() else {
            return Vec::new();
        };
        let base = self.arena_base;
        cycle
            .regions
            .iter()
            .map(|r| (r.extent.0 - base, r.extent.1 - base))
            .collect()
    }

    /// zt t32 (R6): the next mark end (`release_evac_quarantine`, before the
    /// sweep): finish the drain, withdraw the plan from the hot paths, and give
    /// the vacated spans and the withheld holes back to the free list. A no-op
    /// when nothing is planned. **Inside a pause.**
    pub(super) fn release_concurrent_relocate(&self) {
        let st = &self.counters.crl;
        let Some(cycle) = st.cycle.lock().take() else {
            return;
        };
        self.drain_lazy_cycle(&cycle, ZLazyCopier::Drain);
        st.published.store(std::ptr::null_mut(), Ordering::Release);
        // Each region whole: its survivors are copied, its free blocks were
        // withheld, and every dead base in it left the registry (the in-pause
        // sweep, or a deferred cycle's prune). A region where a base is still
        // registered gives back only its vacated spans and withheld blocks.
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for reg in cycle.regions.iter() {
            let (lo, hi) = reg.extent;
            if self.registry.bases_in_span(lo, hi).is_empty() {
                spans.push((lo, hi));
            } else {
                st.release_kept.fetch_add(1, Ordering::Relaxed);
                spans.extend(reg.objects.iter().map(|o| (o.from, o.from + o.size)));
                spans.extend(reg.withheld.iter().copied());
            }
        }
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (lo, hi) in spans {
            match merged.last_mut() {
                Some(last) if last.1 >= lo => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        // t1 Q (R-1): the dead-at-release census, BEFORE the zeroing and the
        // arena lock (it reads headers and the registry only): no registered
        // object outside the released spans may still name one. A census, not
        // a gate. The spans are absolute addresses, so the base is 0.
        if super::zgc_dbg_quarantine_dead_check_enabled() && !merged.is_empty() {
            let (_holders, stale, _screened, _answered, live_stale) =
                self.quarantine_stale_slots(&merged, 0, None, false);
            st.dead_check[0].fetch_add(1, Ordering::Relaxed);
            st.dead_check[1].fetch_add(stale, Ordering::Relaxed);
            st.dead_check[2].fetch_add(live_stale, Ordering::Relaxed);
        }
        // t1 Q (R-2): no write may reach a copied object's from-space after its
        // copy. Compared before the zeroing below.
        if let Some(snaps) = &cycle.from_snapshots {
            let mut reported = 0usize;
            for (from, bytes) in snaps.lock().iter() {
                // SAFETY: a planned survivor's from-space, intact until the
                // zeroing below (no allocation lands in a selected region).
                let now = unsafe { std::slice::from_raw_parts(*from as *const u8, bytes.len()) };
                if now == &bytes[..] {
                    continue;
                }
                st.from_writes.fetch_add(1, Ordering::Relaxed);
                if reported < 16 {
                    reported += 1;
                    let at = now.iter().zip(bytes.iter()).position(|(a, b)| a != b);
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        from = *from,
                        size = bytes.len(),
                        first_diff = ?at,
                        "zgc concurrent relocate: a copied object's from-space was written \
                         after its copy (a lost write)"
                    );
                }
            }
        }
        let mut arena = self.arena.lock();
        let base = arena.base_ptr() as usize;
        let mut bytes = 0usize;
        for (lo, hi) in merged {
            // SAFETY: a selected region's extent (or a withheld / vacated span
            // of it): free bytes, dead bytes or a copied object's old bytes, no
            // registered base inside, nothing reads it; zeroed for the walkers,
            // as a vacated span is.
            unsafe { std::ptr::write_bytes(lo as *mut u8, 0, hi - lo) };
            arena.add_free_block(lo - base, hi - lo);
            bytes += hi - lo;
        }
        st.released_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader is opt-in.
    #[test]
    fn zt32_the_concurrent_relocate_switch_is_opt_in() {
        let run = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_CONCURRENT_RELOCATE", v)],
                zgc_concurrent_relocate_enabled,
            )
        };
        assert!(!run(None));
        assert!(run(Some("1")));
        assert!(!run(Some("0")));
    }

    /// The plan end to end on a real heap. Reference arrays packed on the
    /// first pages, then spaced 128 KiB apart over garbage (sparse regions the
    /// selector takes), linked in a ring by element 0 and across by element 1.
    /// A simulated concurrent mark leaves every element in the mark color, so
    /// the plan copies nothing in the pause; the map's lookups (the roots)
    /// copy their objects; a barriered read of element 0 copies its target;
    /// the drain copies the rest; the next mark's visits heal element 1; the
    /// release gives the regions back. Every raw word then names the planned
    /// address, and no planned object is left at its old base in the registry.
    #[test]
    fn zt32_the_plan_copies_lazily_and_the_ring_survives() {
        use crate::collector::GarbageCollector;
        use crate::heap::ArrayElementType;
        use cratonvm_types::{ClassId, ObjectRef, Value, ARRAY_DATA_OFFSET};
        const PAGE: usize = super::super::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        const FIELDS: usize = 500;
        let _colored = crate::zgc::tests::colored_arming_test_lock();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
            || {
                let heap = super::super::ZgcRealHeap::with_capacity(64 * 1024 * 1024);
                heap.set_tlab_enabled(false);
                assert!(heap.arm_load_barrier_colored_stores());
                assert!(heap.arm_load_barrier_color_flip());
                heap.counters.crl.set_enabled_for_test(true);
                let base = heap.arena.lock().base_ptr() as usize;
                let arr = |heap: &super::super::ZgcRealHeap| {
                    heap.try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 2)
                        .expect("room")
                };
                let mut kept: Vec<ObjectRef> = Vec::new();
                loop {
                    let o = arr(&heap);
                    kept.push(o);
                    if o.as_ptr() as usize >= base + 2 * PAGE {
                        break;
                    }
                }
                loop {
                    let o = arr(&heap);
                    kept.push(o);
                    if o.as_ptr() as usize >= base + 6 * PAGE {
                        break;
                    }
                    let start = o.as_ptr() as usize;
                    while heap.alloc_object(ClassId::new(1), FIELDS).as_ptr() as usize - start
                        < 128 * 1024
                    {}
                }
                let n = kept.len();
                for i in 0..n {
                    heap.set_array_element(kept[i], 0, Value::Object(Some(kept[(i + 1) % n])))
                        .unwrap();
                    heap.set_array_element(kept[i], 1, Value::Object(Some(kept[(i * 7 + 3) % n])))
                        .unwrap();
                }
                let mut live: Vec<usize> = kept.iter().map(|o| o.as_ptr() as usize).collect();
                live.sort_unstable();
                let visit_all = |heap: &super::super::ZgcRealHeap, objs: &[usize]| {
                    heap.set_mark_active(true);
                    heap.color_flip_to_mark();
                    for &b in objs {
                        heap.visit_strong_refs_at(b as *mut u8, None, &mut |_| {});
                    }
                    heap.set_mark_active(false);
                    heap.color_flip_to_remap();
                };
                // ---- the closing mark, then the plan
                visit_all(&heap, &live);
                heap.counters.mark_root_filter.rebuild();
                let skip = heap.counters.rim_last_mark_color.load(Ordering::Acquire);
                let stride = cratonvm_types::ref_element_size();
                let not_skipped = live
                    .iter()
                    .flat_map(|&b| {
                        [0, 1].map(|k| (b + ARRAY_DATA_OFFSET + k * stride) as *const u64)
                    })
                    .filter(|&p| unsafe { *p } & super::super::vaddr::Z_METADATA_MASK != skip)
                    .count();
                assert_eq!(not_skipped, 0, "every element in the mark color {skip:#x}");
                // The pin / overlay owners the filter answers "may own" for:
                // none in a fresh process, every base when a provider some
                // other test registered process-wide forces the overlay
                // answer. Only they are copied in the pause.
                let owners: std::collections::HashSet<usize> = live
                    .iter()
                    .copied()
                    .filter(|&b| {
                        let (p, o) = heap.counters.mark_root_filter.extra_root_probe(b);
                        p || o
                    })
                    .collect();
                let (planned, map) = heap
                    .concurrent_relocate_start(&|| live.clone())
                    .expect("a plan");
                heap.counters.mark_root_filter.disarm();
                assert!(planned > 0, "the sparse regions were planned");
                let planned_owners = map.iter().filter(|(k, _)| owners.contains(k)).count();
                let r = heap.concurrent_relocate_report();
                assert_eq!((r.cycles, r.planned), (1, planned));
                assert_eq!(
                    r.eager, planned_owners,
                    "every element left in the mark color: only the owners are eager"
                );
                // ---- the roots: the map's lookups copy their objects
                let now: Vec<usize> = kept
                    .iter()
                    .map(|o| {
                        map.get(&(o.as_ptr() as usize))
                            .copied()
                            .unwrap_or(o.as_ptr() as usize)
                    })
                    .collect();
                let moved = (0..n)
                    .filter(|&i| now[i] != kept[i].as_ptr() as usize)
                    .count();
                assert_eq!(moved, planned, "the map names every planned object");
                assert_eq!(
                    heap.concurrent_relocate_report().hit,
                    planned - planned_owners,
                    "each lookup copied what the pause had not"
                );
                // ---- the mutator interval: element 0, barriered
                for i in 0..n {
                    let at = unsafe { ObjectRef::from_raw(now[i] as *mut u8) };
                    match heap.get_array_element(at, 0) {
                        Ok(Value::Object(Some(t))) => {
                            assert_eq!(t.as_ptr() as usize, now[(i + 1) % n], "element 0 of {i}");
                        }
                        other => panic!("element 0 of {i}: {other:?}"),
                    }
                }
                let _ = heap.run_concurrent_relocate_drain();
                // ---- the next mark heals element 1; its end releases
                let mut sorted = now.clone();
                sorted.sort_unstable();
                visit_all(&heap, &sorted);
                heap.release_evac_quarantine();
                let r = heap.concurrent_relocate_report();
                assert!(r.released_bytes > 0, "the regions went back");
                assert!(!heap.counters.crl.published(), "withdrawn");
                let stride = cratonvm_types::ref_element_size();
                for i in 0..n {
                    let data = unsafe { (now[i] as *mut u8).add(ARRAY_DATA_OFFSET) };
                    let e0 = unsafe { cratonvm_types::narrow_oop::read_ref_slot(data) };
                    let e1 = unsafe { cratonvm_types::narrow_oop::read_ref_slot(data.add(stride)) };
                    assert_eq!(e0 as usize, now[(i + 1) % n], "raw element 0 of {i}");
                    assert_eq!(e1 as usize, now[(i * 7 + 3) % n], "raw element 1 of {i}");
                    assert!(
                        heap.registry.contains(now[i]),
                        "{i} registered at its new base"
                    );
                    if now[i] != kept[i].as_ptr() as usize {
                        assert!(
                            !heap.registry.contains(kept[i].as_ptr() as usize),
                            "{i}: the old base left the registry"
                        );
                    }
                }
            },
        );
    }

    /// zt t33: the plan walks only the legacy holders. Positive controls of
    /// the walk check (`CRATONVM_DBG_ZGC_CRL_WALK_CHECK`): a plain element
    /// planted after the closing mark is found as `Plain`; with the legacy
    /// pass skipped (`CRATONVM_DBG_ZGC_CRL_NO_LEGACY_PASS`) a legacy cell
    /// naming a planned object is found as `Legacy`; with the pass on, the
    /// check finds no legacy cell and the cells name the planned addresses.
    #[test]
    fn zt33_the_walk_check_finds_what_the_legacy_pass_leaves() {
        use crate::collector::GarbageCollector;
        use crate::heap::ArrayElementType;
        use cratonvm_types::{ClassId, ObjectRef, Value, ARRAY_DATA_OFFSET};
        const PAGE: usize = super::super::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        // No layout is ever registered for this class: its objects are legacy.
        const LEGACY_CLASS: u32 = 0x00f3_3333;
        let _colored = crate::zgc::tests::colored_arming_test_lock();
        let run = |skip_legacy_pass: bool| -> (ZConcRelocReport, usize, usize) {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
                || {
                    let heap = super::super::ZgcRealHeap::with_capacity(64 * 1024 * 1024);
                    heap.set_tlab_enabled(false);
                    assert!(heap.arm_load_barrier_colored_stores());
                    assert!(heap.arm_load_barrier_color_flip());
                    heap.counters.crl.set_enabled_for_test(true);
                    heap.counters
                        .crl
                        .set_walk_check_for_test(true, skip_legacy_pass);
                    let base = heap.arena.lock().base_ptr() as usize;
                    let arr = |heap: &super::super::ZgcRealHeap| {
                        heap.try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 2)
                            .expect("room")
                    };
                    // Sparse survivors over pages 2..6 (planned), dense below.
                    let mut kept: Vec<ObjectRef> = Vec::new();
                    loop {
                        let o = arr(&heap);
                        kept.push(o);
                        if o.as_ptr() as usize >= base + 2 * PAGE {
                            break;
                        }
                    }
                    loop {
                        let o = arr(&heap);
                        kept.push(o);
                        if o.as_ptr() as usize >= base + 6 * PAGE {
                            break;
                        }
                        let start = o.as_ptr() as usize;
                        while heap.alloc_object(ClassId::new(1), 500).as_ptr() as usize - start
                            < 128 * 1024
                        {}
                    }
                    let n = kept.len();
                    // One legacy holder naming every survivor.
                    let holder = heap.alloc_object(ClassId::new(LEGACY_CLASS), n);
                    assert!(
                        !cratonvm_types::is_compact_object(heap.header_ref(holder.as_ptr())),
                        "the holder is legacy-shaped"
                    );
                    for i in 0..n {
                        heap.set_array_element(kept[i], 0, Value::Object(Some(kept[(i + 1) % n])))
                            .unwrap();
                        heap.set_field(holder, i, Value::Object(Some(kept[i])));
                    }
                    let mut live: Vec<usize> = kept.iter().map(|o| o.as_ptr() as usize).collect();
                    live.push(holder.as_ptr() as usize);
                    live.sort_unstable();
                    // The closing mark.
                    heap.set_mark_active(true);
                    heap.color_flip_to_mark();
                    for &b in &live {
                        heap.visit_strong_refs_at(b as *mut u8, None, &mut |_| {});
                    }
                    heap.set_mark_active(false);
                    heap.color_flip_to_remap();
                    heap.counters.mark_root_filter.rebuild();
                    // A plain element 1 in every array, as a raw writer would
                    // leave it after the mark.
                    let stride = cratonvm_types::ref_element_size();
                    for i in 0..n {
                        let slot =
                            (kept[i].as_ptr() as usize + ARRAY_DATA_OFFSET + stride) as *mut u8;
                        let t = kept[(i * 7 + 3) % n].as_ptr() as u64;
                        unsafe { std::ptr::write(slot as *mut u64, t) };
                    }
                    let (planned, map) = heap
                        .concurrent_relocate_start(&|| live.clone())
                        .expect("a plan");
                    heap.counters.mark_root_filter.disarm();
                    assert!(planned > 0, "the sparse regions were planned");
                    let plain_expected = (0..n)
                        .filter(|&i| {
                            map.get(&(kept[(i * 7 + 3) % n].as_ptr() as usize))
                                .is_some()
                        })
                        .count();
                    let legacy_expected = (0..n)
                        .filter(|&i| map.get(&(kept[i].as_ptr() as usize)).is_some())
                        .count();
                    // Every cell and element names the planned address now.
                    for i in 0..n {
                        let slot =
                            (kept[i].as_ptr() as usize + ARRAY_DATA_OFFSET + stride) as *const u64;
                        let t = kept[(i * 7 + 3) % n].as_ptr() as usize;
                        let want = map.get(&t).copied().unwrap_or(t);
                        assert_eq!(unsafe { *slot } as usize, want, "element 1 of {i}");
                    }
                    let h_now = map
                        .get(&(holder.as_ptr() as usize))
                        .copied()
                        .unwrap_or(holder.as_ptr() as usize);
                    let h_now = unsafe { ObjectRef::from_raw(h_now as *mut u8) };
                    for i in 0..n {
                        let t = kept[i].as_ptr() as usize;
                        let want = map.get(&t).copied().unwrap_or(t);
                        match heap.get_field(h_now, i) {
                            Value::Object(Some(o)) => {
                                assert_eq!(o.as_ptr() as usize, want, "cell {i}")
                            }
                            other => panic!("cell {i}: {other:?}"),
                        }
                    }
                    (
                        heap.concurrent_relocate_report(),
                        plain_expected,
                        legacy_expected,
                    )
                },
            )
        };
        let (r, plain, legacy) = run(false);
        assert_eq!((r.walk_checked, r.legacy_holders), (1, 1));
        assert!(plain > 0 && legacy > 0, "planned targets on both sides");
        assert_eq!(r.walk_missed, [plain, 0, 0], "the pass left no legacy cell");
        let (r, plain, legacy) = run(true);
        assert_eq!(
            r.walk_missed,
            [plain, 0, legacy],
            "without the pass the check finds every planned cell"
        );
    }

    /// zt t33: from the closing pause's remap flip until its end, a store
    /// writes the mark color the flip left (bad after the release), when R6
    /// may plan; the pause's end and a slide give the good color back. Off
    /// R6 the flip's heal color is the good color, as before.
    #[test]
    fn zt33_the_closing_pause_stores_the_mark_color() {
        use super::super::vaddr::{Z_METADATA_MASK, Z_REMAPPED};
        use crate::collector::GarbageCollector;
        use crate::heap::ArrayElementType;
        use cratonvm_types::{ClassId, Value, ARRAY_DATA_OFFSET};
        let _colored = crate::zgc::tests::colored_arming_test_lock();
        let color_after_store = |plan: bool| -> (u64, u64, u64) {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
                || {
                    let heap = super::super::ZgcRealHeap::with_capacity(16 * 1024 * 1024);
                    heap.set_tlab_enabled(false);
                    assert!(heap.arm_load_barrier_colored_stores());
                    assert!(heap.arm_load_barrier_color_flip());
                    heap.counters.crl.set_enabled_for_test(plan);
                    let a = heap
                        .try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 1)
                        .expect("room");
                    let b = heap
                        .try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 1)
                        .expect("room");
                    let slot = (a.as_ptr() as usize + ARRAY_DATA_OFFSET) as *const u64;
                    let store = || {
                        heap.set_array_element(a, 0, Value::Object(Some(b)))
                            .unwrap();
                        (unsafe { *slot }) & Z_METADATA_MASK
                    };
                    heap.set_mark_active(true);
                    heap.color_flip_to_mark();
                    let marked = store();
                    heap.set_mark_active(false);
                    heap.color_flip_to_remap();
                    let closing = store();
                    heap.end_closing_heal_color();
                    let after = store();
                    assert_ne!(marked, Z_REMAPPED, "the mark stores its color");
                    (marked, closing, after)
                },
            )
        };
        let (marked, closing, after) = color_after_store(true);
        assert_eq!(
            closing, marked,
            "the closing pause stores the leaving mark color"
        );
        assert_eq!(
            after, Z_REMAPPED,
            "the pause's end gives the good color back"
        );
        let (_, closing, after) = color_after_store(false);
        assert_eq!(
            (closing, after),
            (Z_REMAPPED, Z_REMAPPED),
            "off R6: the good color"
        );
    }

    /// zt t33: a pause whose relocation refused leaves its mark color unread;
    /// the next cycle marked stop-the-world (no mark flip, so the remap flip
    /// is a noop) must not plan by it. Before the fix the plan read the stale
    /// color and planned objects whose words the marker had healed to
    /// Remapped, which reads good: round stress lost weak referents.
    #[test]
    fn zt33_a_stop_the_world_mark_does_not_inherit_a_stale_mark_color() {
        let _colored = crate::zgc::tests::colored_arming_test_lock();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
            || {
                let heap = super::super::ZgcRealHeap::with_capacity(16 * 1024 * 1024);
                heap.set_tlab_enabled(false);
                assert!(heap.arm_load_barrier_colored_stores());
                assert!(heap.arm_load_barrier_color_flip());
                heap.counters.crl.set_enabled_for_test(true);
                heap.set_mark_active(true);
                heap.color_flip_to_mark();
                heap.set_mark_active(false);
                heap.color_flip_to_remap();
                assert_ne!(
                    heap.counters.rim_last_mark_color.load(Ordering::Acquire),
                    0,
                    "a concurrent mark leaves its color"
                );
                heap.end_closing_heal_color();
                // The refused pause never slid; the next mark is stop-the-world.
                heap.color_flip_to_remap();
                assert_eq!(
                    heap.counters.rim_last_mark_color.load(Ordering::Acquire),
                    0,
                    "a noop flip leaves no mark color"
                );
                assert!(heap.concurrent_relocate_start(&Vec::new).is_none());
                assert_eq!(
                    heap.counters.crl.last_refusal.load(Ordering::Relaxed),
                    ZConcRelocRefusal::NoMarkColor as u8
                );
            },
        );
    }

    /// zt t34: the mark books a straddling object onto every region it
    /// reaches, and a blackened range as live wherever it lies.
    #[test]
    fn zt34_the_mark_books_objects_and_ranges_by_region() {
        const R: usize = ZRELOC_REGION_BYTES;
        let base = 0x1000_0000usize;
        let live = ZMarkLiveness::new(true, base, 4 * R);
        live.arm();
        assert!(live.armed());
        live.note_object(base + 64, Some(48), false);
        // From region 0 into region 2.
        live.note_object(base + R - 16, Some(R + 32), true);
        live.note_range(base + 3 * R - 8, base + 3 * R + 100);
        live.note_object(base + 3 * R + 200, None, false);
        let grid = ZRegionGrid::new(base, base + 4 * R, R).expect("grid");
        let m = live.take(&grid).expect("armed");
        assert!(!live.armed(), "taking disarms");
        assert_eq!(m.liveness.live_bytes, vec![48 + R + 32, 0, 8, 100]);
        assert_eq!(m.liveness.live_objects, vec![2, 0, 1, 1]);
        assert_eq!(m.liveness.straddle_in_bytes, vec![0, R, 16, 100]);
        assert_eq!(m.liveness.unsizable, 1);
        assert_eq!(m.legacy, vec![base + R - 16]);
        assert_eq!(m.black, vec![(base + 3 * R - 8, base + 3 * R + 100)]);
        assert!(live.take(&grid).is_none(), "nothing kept after a take");
        live.arm();
        assert_eq!(live.take(&grid).expect("armed").legacy, Vec::<usize>::new());
    }

    /// zt t34: the plan from the mark's liveness selects what t33's sizing
    /// loop selects on the ring heap, and finds the legacy holders without a
    /// header scan: one the mark claimed, one born black in a blackened
    /// range. The liveness check reads 0 under-estimated regions and 0
    /// missed holders, and every cell of both holders names the planned
    /// address after the plan.
    #[test]
    fn zt34_the_plan_from_the_mark_matches_the_sizing_plan() {
        use crate::collector::GarbageCollector;
        use crate::heap::ArrayElementType;
        use cratonvm_types::{ClassId, ObjectRef, Value};
        const PAGE: usize = super::super::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        const LEGACY_CLASS: u32 = 0x00f3_4444;
        let _colored = crate::zgc::tests::colored_arming_test_lock();
        let run = |from_mark: bool| -> (Vec<usize>, ZConcRelocReport) {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
                || {
                    let heap = super::super::ZgcRealHeap::with_capacity(64 * 1024 * 1024);
                    heap.set_tlab_enabled(false);
                    assert!(heap.arm_load_barrier_colored_stores());
                    assert!(heap.arm_load_barrier_color_flip());
                    heap.counters.crl.set_enabled_for_test(true);
                    heap.counters.crl.set_liveness_check_for_test(true);
                    let base = heap.arena.lock().base_ptr() as usize;
                    let arr = |heap: &super::super::ZgcRealHeap| {
                        heap.try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 2)
                            .expect("room")
                    };
                    let mut kept: Vec<ObjectRef> = Vec::new();
                    loop {
                        let o = arr(&heap);
                        kept.push(o);
                        if o.as_ptr() as usize >= base + 2 * PAGE {
                            break;
                        }
                    }
                    loop {
                        let o = arr(&heap);
                        kept.push(o);
                        if o.as_ptr() as usize >= base + 6 * PAGE {
                            break;
                        }
                        let start = o.as_ptr() as usize;
                        while heap.alloc_object(ClassId::new(1), 500).as_ptr() as usize - start
                            < 128 * 1024
                        {}
                    }
                    let n = kept.len();
                    let claimed = heap.alloc_object(ClassId::new(LEGACY_CLASS), n);
                    let born = heap.alloc_object(ClassId::new(LEGACY_CLASS), n);
                    for i in 0..n {
                        heap.set_array_element(kept[i], 0, Value::Object(Some(kept[(i + 1) % n])))
                            .unwrap();
                        heap.set_field(claimed, i, Value::Object(Some(kept[i])));
                        heap.set_field(born, i, Value::Object(Some(kept[(i + 5) % n])));
                    }
                    let mut live: Vec<usize> = kept.iter().map(|o| o.as_ptr() as usize).collect();
                    live.push(claimed.as_ptr() as usize);
                    live.push(born.as_ptr() as usize);
                    live.sort_unstable();
                    // The closing mark: claims (booked when armed) and heals;
                    // `born` is blackened as a TLAB chunk would be.
                    if from_mark {
                        heap.crl_liveness_arm_for_mark();
                        assert!(heap.counters.crl.live.armed(), "R6 may plan: armed");
                    }
                    heap.set_mark_active(true);
                    heap.color_flip_to_mark();
                    let born_at = born.as_ptr() as usize;
                    let born_end = born_at
                        + heap
                            .live_extent(born_at, heap.header_ref(born.as_ptr()))
                            .expect("sized");
                    for &b in &live {
                        if b == born_at {
                            assert!(heap.blacken_range(born_at, born_end));
                        } else {
                            assert!(heap.mark_claim(b));
                        }
                        heap.visit_strong_refs_at(b as *mut u8, None, &mut |_| {});
                    }
                    heap.set_mark_active(false);
                    heap.color_flip_to_remap();
                    // The sweep's registry prune (`starts &= marks`): the plan
                    // runs after it, so the registry is the live set.
                    heap.registry
                        .retain_marked(heap.mark_bits.as_ref().expect("bitmap arm"));
                    heap.counters.mark_root_filter.rebuild();
                    let (planned, map) = heap
                        .concurrent_relocate_start(&|| live.clone())
                        .unwrap_or_else(|| {
                            panic!(
                                "no plan (mark={from_mark}): refusal {}",
                                heap.counters.crl.last_refusal.load(Ordering::Relaxed)
                            )
                        });
                    heap.counters.mark_root_filter.disarm();
                    assert!(planned > 0, "the sparse regions were planned");
                    let froms: Vec<usize> = live
                        .iter()
                        .copied()
                        .filter(|b| map.get(b).is_some())
                        .collect();
                    assert_eq!(froms.len(), planned);
                    for (holder, shift) in [(claimed, 0usize), (born, 5)] {
                        let h = holder.as_ptr() as usize;
                        let h_now = map.get(&h).copied().unwrap_or(h);
                        let h_now = unsafe { ObjectRef::from_raw(h_now as *mut u8) };
                        for i in 0..n {
                            let t = kept[(i + shift) % n].as_ptr() as usize;
                            let want = map.get(&t).copied().unwrap_or(t);
                            match heap.get_field(h_now, i) {
                                Value::Object(Some(o)) => {
                                    assert_eq!(o.as_ptr() as usize, want, "cell {i} of {h:#x}")
                                }
                                other => panic!("cell {i}: {other:?}"),
                            }
                        }
                    }
                    // Arena offsets: the two heaps are laid out alike.
                    let froms = froms.iter().map(|&b| b - base).collect();
                    (froms, heap.concurrent_relocate_report())
                },
            )
        };
        let (walk_set, r_walk) = run(false);
        let (mark_set, r_mark) = run(true);
        assert_eq!((r_walk.mark_plans, r_mark.mark_plans), (0, 1));
        assert_eq!(r_mark.check, [1, 0, 0], "the estimate bounds the exact figures");
        assert_eq!(r_mark.black_legacy, 1, "the holder born black was found");
        assert_eq!((r_walk.legacy_holders, r_mark.legacy_holders), (2, 2));
        assert_eq!(mark_set, walk_set, "the same survivors planned");
    }
}
