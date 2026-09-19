// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational ZGC (JEP 439) over **real** page storage — the young/old split
//! that `ZgcRealHeap` does not have.
//!
//! # Why this module exists
//!
//! `ZgcRealHeap` (in [`crate::zgc`]) is a correct but **whole-heap**,
//! stop-the-world, non-moving mark-sweep: one `Mutex<Arena>`, one flat address
//! registry, and one trace of everything on every collection. It has no notion
//! of a generation, so a workload that allocates a great deal of short-lived
//! garbage pays a full-heap trace per collection.
//!
//! The full Spring Boot suite comparison recorded in
//! `zgc-real-fullsuite-regression-RETIRED-20260807.md` (same binary, only
//! `-XX:+UseZGC` toggled) regressed 46 classes, 35 of them PASS → HANG, and the
//! regressions cluster on `*AutoConfigurationTests` — the shape that builds and
//! tears down many Spring `ApplicationContext`s in a loop, i.e. the most
//! allocation-churn-heavy workload in the suite.
//!
//! **The working hypothesis is that the missing generational split is the
//! cause. It is a hypothesis, not a demonstrated fact** — a sibling module is
//! building the pause instrumentation that would confirm or refute it. This
//! module exists so the hypothesis is *testable*: it gives the young/old policy
//! real storage to stand on. Nothing here should be read as a claim that the
//! regression is fixed.
//!
//! # What was ported, and from where
//!
//! The **policy** is ported from the `GenerationalZgc` simulation in
//! [`crate::zgc`] (`GenerationalZgc`, `GenerationalTriggerKind`,
//! `GenerationalZgcStats`, `DEFAULT_PROMOTION_AGE`,
//! `YOUNG_OCCUPANCY_MINOR_THRESHOLD`, `OLD_OCCUPANCY_MAJOR_THRESHOLD`). That
//! simulation is *correct policy over fake storage*: it schedules minor and
//! major cycles, ages pages, promotes on age, and reports occupancy — it simply
//! has no bytes behind any of it (`ZgcHeap::add_page` hands out synthetic `u64`
//! offsets). The scheduling shape, the aging rule, the occupancy thresholds and
//! the "old pressure outranks young pressure" ordering are all its design, kept
//! deliberately recognisable so the two can be diffed.
//!
//! What had to change for real storage is listed at
//! [`ZGenerationalHeap::collect_major`] and in the "Deviations" section below.
//!
//! [`crate::gen_heap`] (the working generational collector this VM actually
//! ships) is the correctness oracle for *what a generational collection must
//! do here*: all allocation starts young, a minor cycle takes the old→young
//! remembered set as extra roots and never walks old, and promotion is by
//! survival count.
//!
//! # Shape at a glance
//!
//! ```text
//!   ZGenerationalHeap
//!     allocator: Arc<ZPageAllocator>        ← real memory (zgc::page)
//!     young: ZYoungGeneration               ← page id -> Arc<ZPageReal>
//!     old:   ZOldGeneration                 ← page id -> Arc<ZPageReal>
//!     promotion: ZPromotionPolicy           ← per-PAGE aging (see below)
//!     policy: ZGenerationalPolicy           ← minor/major trigger + re-arm floor
//!
//!   collect_young(roots, remembered, marker)
//!     1. retire the mutator's shared pages   (page.rs closes them)
//!     2. snapshot the young page set
//!     3. build a ZGenerationScope over YOUNG PAGES ONLY
//!     4. roots ∪ remembered-set young targets, filtered through the scope
//!     5. marker.mark_from_roots(.., &scope)  ← cannot see an old page at all
//!     6. free dead young pages, age + promote survivors
//! ```
//!
//! # What this module does NOT do
//!
//! * **No remembered set.** A sibling module (`zgc::remembered`) owns old→young
//!   tracking. This module depends on [`ZRememberedSetView`], a trait it
//!   defines, plus a test implementation. Do not add a card table here.
//! * **No marking.** A sibling module owns the tracer. This module depends on
//!   [`ZGenerationMarker`], a trait it defines, plus test implementations.
//! * **No relocation, no barrier, no colored-pointer encoding.** Every address
//!   crossing this module's API is an **uncolored machine address** (a `usize`,
//!   carried as `u64` at the trait boundaries to match the sibling signatures).
//!   Colored words belong to `zgc::vaddr` and the load barrier; a colored word
//!   must never be handed to anything here.
//!
//! # Scoping
//!
//! Every piece of state is instance-owned. There is no `static`, no
//! `OnceLock`, and no process-global cache — this tree has had parallel-test
//! crashes caused by process-global GC caches, and a VM host may own more than
//! one heap at a time.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::zgc::page::{ZPageAllocator, ZPageError, ZPageReal, ZPageSizeClass, ZPageState};

// ---------------------------------------------------------------------------
// Tunables (ported from the GenerationalZgc simulation)
// ---------------------------------------------------------------------------

/// Young-generation share of the total heap budget.
///
/// Ported verbatim from `zgc::DEFAULT_YOUNG_FRACTION`.
pub const Z_DEFAULT_YOUNG_FRACTION: f64 = 0.25;

/// Young occupancy above which a minor cycle is wanted.
///
/// Ported from `zgc::YOUNG_OCCUPANCY_MINOR_THRESHOLD`.
pub const Z_YOUNG_OCCUPANCY_MINOR_THRESHOLD: f64 = 0.50;

/// Old occupancy above which a major cycle is wanted.
///
/// Ported from `zgc::OLD_OCCUPANCY_MAJOR_THRESHOLD`.
pub const Z_OLD_OCCUPANCY_MAJOR_THRESHOLD: f64 = 0.80;

/// Minor cycles a page must survive before it is promoted.
///
/// Ported from `zgc::DEFAULT_PROMOTION_AGE`, which is itself the same figure
/// [`crate::gen_heap`] uses (`PROMOTION_AGE: u8 = 3`).
pub const Z_DEFAULT_PROMOTION_AGE: u32 = 3;

/// How many minor cycles may run before the next pressure-driven collection is
/// upgraded to a major one.
///
/// **Not** in the simulation, which only ever ran a major on old-occupancy
/// pressure. Without this, a heap whose old generation grows slowly but never
/// crosses [`Z_OLD_OCCUPANCY_MAJOR_THRESHOLD`] would never collect old at all,
/// and floating garbage promoted out of young would accumulate forever. See
/// [`ZGenerationalPolicy::should_collect`] for why this cannot itself become a
/// GC storm.
pub const Z_DEFAULT_MINORS_PER_MAJOR: u64 = 8;

/// Absolute floor on how much *fresh* allocation must happen after a collection
/// before the trigger re-arms. 64 KiB, matching `ZgcRealHeap`'s `gc_rearm`.
pub const Z_REARM_MIN_BYTES: usize = 64 * 1024;

/// The re-arm floor also demands `remaining_headroom / N` fresh bytes. `N = 4`,
/// matching `ZgcRealHeap`'s `gc_rearm`.
pub const Z_REARM_HEADROOM_DIVISOR: usize = 4;

// ---------------------------------------------------------------------------
// ZGeneration / ZGenerationId
// ---------------------------------------------------------------------------

/// Which generation a page belongs to.
///
/// # Why the mapping lives here and not on the page
///
/// [`ZPageReal`] is owned by `zgc::page` and has no generation field (it has an
/// `age`, which this module drives, but not an owner). Rather than reach across
/// a module boundary to add one, the mapping is held **in this module's own
/// structures**, as `page id -> Arc<ZPageReal>` maps inside
/// [`ZYoungGeneration`] and [`ZOldGeneration`]. Three consequences worth
/// stating:
///
/// 1. **The key is [`ZPageReal::id`], which is stable across
///    [`ZPageReal::reset`]** — so a page that is freed and later recycled by
///    the allocator comes back with the *same* id. Ownership is therefore
///    current, not historical: every transition must update the map (free
///    removes, allocation adds, promotion moves). Getting this wrong shows up
///    as a page in both maps, which
///    [`ZGenerationalHeap::debug_assert_generations_disjoint`] catches.
/// 2. **A page belongs to exactly one generation for its whole life in that
///    generation.** That is what makes promotion a map move rather than an
///    object copy, and it is why the old generation must never allocate through
///    [`ZPageAllocator::alloc_object`] — see [`ZOldGeneration::allocate`].
/// 3. Generation lookup by *address* costs one O(1)
///    [`ZPageAllocator::page_for`] plus one hash probe; see
///    [`ZGenerationalHeap::generation_of_address`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZGeneration {
    /// Where every allocation starts.
    Young,
    /// Where pages that survived [`ZPromotionPolicy::promotion_age`] minor
    /// cycles end up.
    Old,
}

impl ZGeneration {
    /// Human-readable tag for tracing.
    pub fn as_str(self) -> &'static str {
        match self {
            ZGeneration::Young => "young",
            ZGeneration::Old => "old",
        }
    }
}

/// A generation plus the cycle number that is collecting it.
///
/// Handed to [`ZGenerationMarker::mark_from_roots`] so a marker can label its
/// own tracing and its own per-cycle state without this module having to expose
/// its counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ZGenerationId {
    /// The generation this cycle is named for.
    pub generation: ZGeneration,
    /// Monotonic per-generation cycle number, starting at 1.
    pub cycle: u64,
}

impl ZGenerationId {
    /// Build an id.
    pub fn new(generation: ZGeneration, cycle: u64) -> Self {
        ZGenerationId { generation, cycle }
    }
}

// ---------------------------------------------------------------------------
// ZGenerationScope — the structural guard
// ---------------------------------------------------------------------------

/// One page's live address range inside a [`ZGenerationScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZScopeEntry {
    /// [`ZPageReal::id`].
    pub page_id: u64,
    /// First byte of the page.
    pub start: usize,
    /// One past the last **allocated** byte — [`ZPageReal::walk_bounds`]'s
    /// upper bound, not the page end. Everything above it is untouched
    /// reservation and can never be an object.
    pub end: usize,
}

/// The set of pages a collection cycle is allowed to look at.
///
/// # This is the entire performance claim, made structural
///
/// A minor cycle is only cheaper than a full collection if it does not trace
/// into the old generation. Comments and discipline are not enough for that, so
/// the scope makes it a property of what the tracer can *see*:
///
/// * The scope is built from **one generation's page list only**. Old pages are
///   not in `entries` and not in `page_ids`, so [`Self::page_of`] returns
///   `None` and [`Self::admits`] returns `false` for every old address.
/// * [`ZGenerationMarker::mark_from_roots`] receives a `&ZGenerationScope` and
///   **nothing else that can classify an address** — no `&ZPageAllocator`, no
///   `&ZPageTable`, no generation handle. A conforming marker filters every
///   pointer it discovers through [`Self::admits`] before following it, and a
///   marker cannot resolve an old address to a page even if it tries.
/// * The driver then debug-asserts that every page id in the returned
///   [`ZMarkReport`] is in the scope, and that the old generation's live-byte
///   total did not move across the cycle. See
///   [`ZGenerationalHeap::collect_young`].
///
/// [`Self::admitted`] / [`Self::rejected`] count the two answers, which is what
/// makes "did this cycle even try to look at old?" a testable number rather
/// than a claim.
///
/// # Lookup cost
///
/// `entries` is sorted by `start`, so [`Self::page_of`] is a binary search:
/// O(log pages), with pages being per-2-MiB at the default geometry. It
/// deliberately does **not** consult [`crate::zgc::page::ZPageTable`], even
/// though that would be O(1) — the page table covers the *whole heap*, and
/// handing the marker something that can resolve an old address would give away
/// the property this type exists to enforce.
#[derive(Debug)]
pub struct ZGenerationScope {
    /// Which generation this scope names.
    generation: ZGeneration,
    /// The cycle that built it.
    cycle: u64,
    /// True when the scope deliberately spans both generations (a major
    /// cycle's whole-heap mark). A minor cycle's scope is never whole-heap and
    /// [`ZGenerationalHeap::collect_young`] asserts as much.
    whole_heap: bool,
    /// Page extents, sorted by `start`.
    entries: Vec<ZScopeEntry>,
    /// Page ids in this scope, for O(1) membership.
    page_ids: FxHashSet<u64>,
    /// Addresses [`Self::page_of`] resolved. `Relaxed`: a pure statistic with
    /// no happens-before relationship to any other state; nothing branches on
    /// it, tests read it after the cycle has been joined.
    admitted: AtomicU64,
    /// Addresses [`Self::page_of`] refused — i.e. pointers the marker offered
    /// that are outside this generation.
    rejected: AtomicU64,
}

impl ZGenerationScope {
    /// Build a scope over `pages`.
    ///
    /// `whole_heap` records that the caller deliberately unioned both
    /// generations (major cycle). A minor cycle passes `false`.
    pub fn from_pages(
        generation: ZGeneration,
        cycle: u64,
        pages: &[Arc<ZPageReal>],
        whole_heap: bool,
    ) -> ZGenerationScope {
        let mut entries: Vec<ZScopeEntry> = Vec::with_capacity(pages.len());
        let mut page_ids: FxHashSet<u64> = FxHashSet::default();
        for page in pages.iter() {
            let (start, end) = page.walk_bounds();
            entries.push(ZScopeEntry {
                page_id: page.id(),
                start,
                end,
            });
            page_ids.insert(page.id());
        }
        entries.sort_by_key(|e| e.start);
        ZGenerationScope {
            generation,
            cycle,
            whole_heap,
            entries,
            page_ids,
            admitted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    /// The generation this scope names.
    pub fn generation(&self) -> ZGeneration {
        self.generation
    }

    /// The cycle that built this scope.
    pub fn cycle(&self) -> u64 {
        self.cycle
    }

    /// Does this scope deliberately span both generations?
    ///
    /// `true` only for a major cycle's whole-heap mark. A minor cycle's scope
    /// answers `false`, and [`ZGenerationalHeap::collect_young`] asserts it.
    pub fn covers_whole_heap(&self) -> bool {
        self.whole_heap
    }

    /// Pages in this scope, sorted by base address. This is the marker's
    /// enumeration handle — it is also the *only* set of pages a marker can
    /// reach through this type.
    pub fn entries(&self) -> &[ZScopeEntry] {
        &self.entries
    }

    /// Number of pages in scope.
    pub fn page_count(&self) -> usize {
        self.entries.len()
    }

    /// Is this page id in scope? O(1).
    pub fn contains_page(&self, page_id: u64) -> bool {
        self.page_ids.contains(&page_id)
    }

    /// The in-scope page containing `addr`, or `None`.
    ///
    /// Counting: every call bumps [`Self::admitted`] or [`Self::rejected`].
    /// O(log pages).
    pub fn page_of(&self, addr: usize) -> Option<ZScopeEntry> {
        let hit = self.find(addr);
        if hit.is_some() {
            // Relaxed: statistics only, see the field docs.
            self.admitted.fetch_add(1, Ordering::Relaxed);
        } else {
            self.rejected.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    /// May a tracer follow `addr` during this cycle?
    ///
    /// `false` for every address outside this generation — including every old
    /// address during a minor cycle, and including an address inside an
    /// in-scope page but above its allocation extent (untouched reservation,
    /// never an object).
    pub fn admits(&self, addr: usize) -> bool {
        self.page_of(addr).is_some()
    }

    /// Addresses resolved to an in-scope page.
    pub fn admitted(&self) -> u64 {
        self.admitted.load(Ordering::Relaxed)
    }

    /// Addresses refused as out of scope.
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }

    /// Non-counting lookup.
    fn find(&self, addr: usize) -> Option<ZScopeEntry> {
        if self.entries.is_empty() {
            return None;
        }
        // Number of entries whose base is at or below `addr`; the candidate is
        // the last of them, because the entries are disjoint and sorted.
        let idx = self.entries.partition_point(|e| e.start <= addr);
        if idx == 0 {
            return None;
        }
        let candidate = self.entries[idx - 1];
        if addr >= candidate.start && addr < candidate.end {
            Some(candidate)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Decoupling traits
// ---------------------------------------------------------------------------

/// Read side of the old→young remembered set, owned by a sibling module.
///
/// # Why the callback yields YOUNG targets, not old slots
///
/// This signature is load-bearing for the no-tracing-into-old property. If the
/// view yielded *old slot addresses*, a minor cycle would have to dereference
/// old memory to find out what each slot points at — which is exactly the
/// old-generation traversal a minor cycle must not perform, reintroduced
/// through the back door.
///
/// So the contract is: **`iterate_old_to_young` yields the addresses of
/// young-generation objects that are referenced from the old generation.** The
/// card scan (or whatever the implementation uses) happens inside the
/// remembered-set module, which legitimately owns old memory; the minor cycle
/// consumes the result as extra roots and never looks at old at all.
///
/// Entries are treated as *hints*: a stale entry whose target is no longer a
/// young address is dropped by the scope filter, not trusted.
pub trait ZRememberedSetView {
    /// Invoke `f` once per recorded old→young reference target.
    fn iterate_old_to_young(&self, f: &mut dyn FnMut(u64));

    /// Approximate entry count, for reporting only. Defaults to 0 so an
    /// implementation that cannot cheaply count need not.
    fn entry_count(&self) -> usize {
        0
    }
}

/// A remembered set with nothing in it.
///
/// The correct view for a heap that has never promoted anything, and the
/// negative control for tests: a minor cycle driven with this must still find
/// the young objects its `roots` reach, and must still find *nothing* in old.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZEmptyRememberedSet;

impl ZRememberedSetView for ZEmptyRememberedSet {
    fn iterate_old_to_young(&self, _f: &mut dyn FnMut(u64)) {}
}

/// What a mark pass found.
///
/// Deliberately *per page*, not per object: this module's unit of reclamation
/// and of promotion is the page (see [`ZPromotionPolicy`]), so the only thing
/// it needs back from a tracer is how many live bytes each page holds. That
/// also keeps the trait boundary narrow enough that a test marker is a dozen
/// lines.
#[derive(Debug, Default, Clone)]
pub struct ZMarkReport {
    /// Page id → live bytes found in that page. A page absent from this map
    /// held no live object and is reclaimable.
    pub live_bytes_by_page: FxHashMap<u64, usize>,
    /// Objects the tracer marked, for reporting.
    pub objects_marked: u64,
}

impl ZMarkReport {
    /// An empty report — "nothing is live".
    pub fn new() -> ZMarkReport {
        ZMarkReport::default()
    }

    /// Credit `bytes` of live data to `page_id`.
    pub fn record(&mut self, page_id: u64, bytes: usize) {
        let slot = self.live_bytes_by_page.entry(page_id).or_insert(0);
        *slot = slot.saturating_add(bytes);
    }

    /// Total live bytes across every page in the report.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes_by_page.values().copied().sum()
    }
}

/// The tracer, owned by a sibling module.
///
/// The implementation must treat `scope` as its **only** authority on which
/// addresses may be followed: check [`ZGenerationScope::admits`] before
/// dereferencing any discovered pointer, and report liveness only against page
/// ids the scope contains. See [`ZGenerationScope`] for why that is enforced by
/// what the marker can see rather than by convention alone.
pub trait ZGenerationMarker {
    /// Trace from `roots` (uncolored machine addresses) within `scope`.
    fn mark_from_roots(
        &self,
        id: ZGenerationId,
        roots: &[u64],
        scope: &ZGenerationScope,
    ) -> ZMarkReport;
}

// ---------------------------------------------------------------------------
// Promotion policy
// ---------------------------------------------------------------------------

/// When a survivor moves from young to old.
///
/// # Per-page, not per-object — and why
///
/// **CratonVM's generational ZGC promotes whole pages.** OpenJDK's Generational
/// ZGC does the same, but the deciding argument here is local to this tree:
///
/// * [`ZPageReal`] already carries an `age` (`AtomicU32`, cleared by
///   [`ZPageReal::reset`]) explicitly reserved for exactly this. There is no
///   comparable per-object age this module can reach: `CompactHeader`'s 7-bit
///   GC age exists, but bumping it per surviving object means an
///   O(live objects) header write pass per minor cycle — the whole-heap cost
///   this module is trying to remove.
/// * Promotion of a page is a **map move**: the `Arc<ZPageReal>` leaves
///   [`ZYoungGeneration`]'s map and enters [`ZOldGeneration`]'s. Nothing is
///   copied, no address changes, and no pointer needs fixing up. Per-object
///   promotion in a non-moving collector cannot be expressed at all without
///   either copying the object (which needs the relocation machinery a sibling
///   module owns) or splitting a page's ownership, which would destroy the
///   O(1) `page → generation` answer this module is built on.
/// * The `GenerationalZgc` simulation being ported already ages per page
///   (`page_ages: FxHashMap<u64, u32>`), so the policy transfers unchanged.
///
/// The cost, stated plainly: a page is promoted as a unit, so one long-lived
/// object drags its whole page's worth of short-lived neighbours into old,
/// where they will not be reclaimed until a major cycle. That is the standard
/// generational-ZGC trade and it is why [`Z_DEFAULT_MINORS_PER_MAJOR`] exists —
/// old must be swept periodically even when it is not under occupancy pressure.
///
/// # 2026-08-07 — `ZPageReal::age` is a survival count and nothing else
///
/// Audit finding N6: `tlab.rs::refill` had a second, independently written
/// meaning for this word. It stamped `page.set_age(generation.page_age())` —
/// `0` for a young TLAB, `1` for an old one — on every page it took, as a
/// *generation tag*. An `Old` TLAB therefore handed a page one minor cycle of
/// survival credit it had never earned, and if such a page ever reached
/// [`ZYoungGeneration`]'s map it would have promoted after two cycles instead
/// of [`Z_DEFAULT_PROMOTION_AGE`].
///
/// The two writers never met in a shipped run — young pages enter the map via
/// `alloc_object`'s *shared* pages while a TLAB takes *private* pages via
/// `alloc_page`, and every hand-off between the subsystems passes through
/// `ZPageAllocator::free_page`, which resets the age — but nothing asserted
/// that, and the corrupt value was globally reachable the whole time
/// (`alloc_page` installs the page in the page table before returning, and the
/// TLAB publishes it as `Relocatable` on release, where `relocate.rs`'s
/// `gen_hint` reads it with *this* module's meaning).
///
/// Resolved in `tlab.rs`, which no longer writes `age` at all: the field's
/// declaration in `page.rs:461-463` owns its meaning, and the TLAB's generation
/// tag moved to the TLAB. The writers of this word are now exactly two, both in
/// this file: the survival increment in `sweep_young` and the reset to `0` in
/// [`ZOldGeneration::adopt_page`]. **Keep it that way** — a page-side generation
/// tag would be a new `ZPageReal` field, not a second reading of this one.
#[derive(Debug, Clone, PartialEq)]
pub struct ZPromotionPolicy {
    /// Minor cycles a page must survive before promotion. `0` promotes every
    /// survivor on its first cycle. Ported from `zgc::DEFAULT_PROMOTION_AGE`,
    /// whose rule was `age + 1 >= promotion_age`.
    pub promotion_age: u32,
    /// Adapted from [`crate::gen_heap`]'s `GC_PROMOTE_PRESSURE_PERCENT`
    /// ("premature promotion" / "always tenure"): when the fraction of young
    /// pages surviving a cycle exceeds this, promote every survivor regardless
    /// of age, because the working set is evidently long-lived and aging it
    /// further just repeats the scan.
    ///
    /// **Defaults to `None` (disabled).** `gen_heap` can afford it because it
    /// is a copying collector that pays a real per-object cost for survivors;
    /// here it would promote whole pages of possibly-young data on one bad
    /// cycle, and there is no measurement yet that says it helps. Left as a
    /// wired-in knob, off, until the pause instrumentation says otherwise.
    pub promote_all_on_survival_above: Option<f64>,
}

impl Default for ZPromotionPolicy {
    fn default() -> Self {
        ZPromotionPolicy {
            promotion_age: Z_DEFAULT_PROMOTION_AGE,
            promote_all_on_survival_above: None,
        }
    }
}

impl ZPromotionPolicy {
    /// A policy with an explicit age and no survival-pressure rule.
    pub fn with_age(promotion_age: u32) -> Self {
        ZPromotionPolicy {
            promotion_age,
            promote_all_on_survival_above: None,
        }
    }

    /// Should a page whose age is now `age_after_cycle` be promoted?
    ///
    /// `age_after_cycle` is the age *after* this cycle's increment, which makes
    /// this the same predicate as the simulation's `age + 1 >= promotion_age`.
    pub fn should_promote(&self, age_after_cycle: u32) -> bool {
        age_after_cycle >= self.promotion_age
    }

    /// Should *every* survivor be promoted this cycle because the survival rate
    /// says the young generation is not actually young?
    ///
    /// Always `false` while [`Self::promote_all_on_survival_above`] is `None`.
    pub fn promote_all_survivors(&self, survivors: usize, examined: usize) -> bool {
        let threshold = match self.promote_all_on_survival_above {
            Some(t) => t,
            None => return false,
        };
        if examined == 0 {
            return false;
        }
        (survivors as f64 / examined as f64) > threshold
    }
}

// ---------------------------------------------------------------------------
// Trigger policy
// ---------------------------------------------------------------------------

/// What the scheduler decided to run.
///
/// Defined here rather than importing `zgc::GenerationalTriggerKind`: that enum
/// has a `None` variant because the simulation returned it alongside a result
/// struct, whereas this API returns `Option<ZGenerationalTrigger>` and lets
/// `None` be `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZGenerationalTrigger {
    /// Collect the young generation only.
    Minor,
    /// Collect both generations.
    Major,
}

/// A snapshot of both generations, and the input to
/// [`ZGenerationalPolicy::should_collect`].
///
/// Mirrors `zgc::GenerationalZgcStats` with the page-level figures the real
/// storage makes available added.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ZGenerationalStats {
    /// Bytes handed out by young pages (their bump cursors).
    pub young_used: usize,
    /// Young generation's byte budget.
    pub young_capacity: usize,
    /// Bytes the last mark found live in young.
    pub young_live: usize,
    /// Young page count.
    pub young_pages: usize,
    /// Bytes handed out by old pages.
    pub old_used: usize,
    /// Old generation's byte budget.
    pub old_capacity: usize,
    /// Bytes the last mark found live in old.
    pub old_live: usize,
    /// Old page count.
    pub old_pages: usize,
    /// Lifetime minor cycles.
    pub minor_cycles: u64,
    /// Lifetime major cycles.
    pub major_cycles: u64,
    /// Minor cycles since the last major one.
    pub minors_since_major: u64,
    /// Lifetime pages promoted young → old.
    pub promoted_pages: u64,
    /// Lifetime bytes promoted young → old (live bytes at promotion time).
    pub promoted_bytes: u64,
}

impl ZGenerationalStats {
    /// `young_used / young_capacity`, or 0 for an unsized generation.
    pub fn young_occupancy(&self) -> f64 {
        if self.young_capacity == 0 {
            return 0.0;
        }
        self.young_used as f64 / self.young_capacity as f64
    }

    /// `old_used / old_capacity`, or 0 for an unsized generation.
    pub fn old_occupancy(&self) -> f64 {
        if self.old_capacity == 0 {
            return 0.0;
        }
        self.old_used as f64 / self.old_capacity as f64
    }
}

/// The minor/major scheduler.
///
/// # Ported policy
///
/// From `zgc::GenerationalZgc::scheduled_collect`: old-occupancy pressure
/// outranks young-occupancy pressure, each has its own threshold, and neither
/// firing means no collection.
///
/// # The re-arm floor — do not remove it
///
/// `ZgcRealHeap` shipped a static threshold with no floor and it produced a
/// **livelock-grade GC storm**. Its own field comment records the mechanism:
/// `needs_gc` was `allocated >= gc_threshold`, `maybe_gc` polls it after *every*
/// allocation bytecode, and a live set that legitimately sat above the static
/// 75% threshold latched `needs_gc` permanently true — so the VM ran a full
/// stop-the-world mark-sweep **per allocation**, forever, without ever
/// surfacing an `OutOfMemoryError`.
///
/// The fix there, and the rule reproduced here, is a post-collection floor:
///
/// ```text
///   floor = used_after_collection + max(headroom_after / 4, 64 KiB)
///   fire  = occupancy > threshold  AND  used >= floor
/// ```
///
/// so *fresh allocation*, not merely a high occupancy, is what re-arms the
/// trigger. A live set parked above the threshold cannot fire twice without at
/// least `max(headroom/4, 64 KiB)` of new bytes in between, which bounds the
/// collection rate by the allocation rate instead of by the poll rate.
///
/// Two corollaries this module honours:
///
/// * **The periodic major ([`Z_DEFAULT_MINORS_PER_MAJOR`]) can never storm**,
///   because it is not an independent trigger: it only *upgrades* a minor that
///   young pressure already armed. If nothing is under pressure, no cycle fires
///   no matter how many minors have elapsed. (A standalone "N minors have
///   passed, collect" rule would be a second latch with no floor — precisely
///   the bug being avoided.)
/// * **Allocation failure must bypass the floor**, or a heap whose headroom is
///   genuinely exhausted (floor above capacity) would never collect again.
///   That path is [`Self::should_collect_ignoring_rearm`], driven by a failed
///   allocation rather than by a poll, exactly as `ZgcRealHeap` notes:
///   "Allocation-failure GCs are driven by the fallible alloc paths and ignore
///   this gate."
#[derive(Debug)]
pub struct ZGenerationalPolicy {
    young_minor_threshold: f64,
    old_major_threshold: f64,
    minors_per_major: u64,
    rearm_min_bytes: usize,
    rearm_headroom_divisor: usize,
    /// Young bytes-used floor below which no poll-driven minor fires.
    ///
    /// `Relaxed` on both sides: this is a heuristic threshold, not a
    /// synchronisation flag. It is written once per collection (at a
    /// safepoint, by the thread that just collected) and read by the
    /// allocation poll; no other state is published through it, so there is
    /// nothing for an `Acquire`/`Release` pair to order. The worst a stale read
    /// can do is delay or advance one collection decision by one poll, and the
    /// decision is re-evaluated on the next allocation anyway.
    young_rearm_floor: AtomicUsize,
    /// Old bytes-used floor. Same reasoning.
    old_rearm_floor: AtomicUsize,
}

impl Default for ZGenerationalPolicy {
    fn default() -> Self {
        ZGenerationalPolicy {
            young_minor_threshold: Z_YOUNG_OCCUPANCY_MINOR_THRESHOLD,
            old_major_threshold: Z_OLD_OCCUPANCY_MAJOR_THRESHOLD,
            minors_per_major: Z_DEFAULT_MINORS_PER_MAJOR,
            rearm_min_bytes: Z_REARM_MIN_BYTES,
            rearm_headroom_divisor: Z_REARM_HEADROOM_DIVISOR,
            young_rearm_floor: AtomicUsize::new(0),
            old_rearm_floor: AtomicUsize::new(0),
        }
    }
}

impl ZGenerationalPolicy {
    /// Build a policy with explicit thresholds. `rearm_headroom_divisor` is
    /// clamped to at least 1 so a caller cannot divide by zero.
    pub fn new(
        young_minor_threshold: f64,
        old_major_threshold: f64,
        minors_per_major: u64,
        rearm_min_bytes: usize,
        rearm_headroom_divisor: usize,
    ) -> Self {
        ZGenerationalPolicy {
            young_minor_threshold,
            old_major_threshold,
            minors_per_major,
            rearm_min_bytes,
            rearm_headroom_divisor: rearm_headroom_divisor.max(1),
            young_rearm_floor: AtomicUsize::new(0),
            old_rearm_floor: AtomicUsize::new(0),
        }
    }

    /// Young occupancy above which a minor is wanted.
    pub fn young_minor_threshold(&self) -> f64 {
        self.young_minor_threshold
    }

    /// Old occupancy above which a major is wanted.
    pub fn old_major_threshold(&self) -> f64 {
        self.old_major_threshold
    }

    /// Minor cycles allowed between majors. `0` disables the periodic upgrade.
    pub fn minors_per_major(&self) -> u64 {
        self.minors_per_major
    }

    /// The current young re-arm floor in bytes.
    pub fn young_rearm_floor(&self) -> usize {
        self.young_rearm_floor.load(Ordering::Relaxed)
    }

    /// The current old re-arm floor in bytes.
    pub fn old_rearm_floor(&self) -> usize {
        self.old_rearm_floor.load(Ordering::Relaxed)
    }

    /// **The scheduler.** `None` means "do not collect now".
    ///
    /// Honours the re-arm floor; see the type docs for why that is not
    /// optional.
    pub fn should_collect(&self, stats: &ZGenerationalStats) -> Option<ZGenerationalTrigger> {
        self.decide(stats, true)
    }

    /// The allocation-failure variant: same rules, floor ignored.
    ///
    /// Call this only from a path that has already *failed* to allocate (a
    /// [`ZPageError::OutOfCapacity`] from
    /// [`ZGenerationalHeap::allocate_young`]), never from a per-allocation
    /// poll — a poll that ignores the floor is the GC storm.
    pub fn should_collect_ignoring_rearm(
        &self,
        stats: &ZGenerationalStats,
    ) -> Option<ZGenerationalTrigger> {
        self.decide(stats, false)
    }

    fn decide(
        &self,
        stats: &ZGenerationalStats,
        honour_rearm: bool,
    ) -> Option<ZGenerationalTrigger> {
        // 1. Old pressure outranks young pressure (ported from
        //    `scheduled_collect`: the major branch is tested first).
        if stats.old_occupancy() > self.old_major_threshold {
            let armed =
                !honour_rearm || stats.old_used >= self.old_rearm_floor.load(Ordering::Relaxed);
            if armed {
                return Some(ZGenerationalTrigger::Major);
            }
        }

        // 2. Young pressure. The periodic-major rule only UPGRADES this
        //    decision; it is never a trigger of its own, so it cannot fire on
        //    an idle heap and cannot latch. See the type docs.
        if stats.young_occupancy() > self.young_minor_threshold {
            let armed =
                !honour_rearm || stats.young_used >= self.young_rearm_floor.load(Ordering::Relaxed);
            if armed {
                if self.minors_per_major > 0 && stats.minors_since_major >= self.minors_per_major {
                    return Some(ZGenerationalTrigger::Major);
                }
                return Some(ZGenerationalTrigger::Minor);
            }
        }

        None
    }

    /// Re-arm the young trigger after a minor cycle.
    ///
    /// `used_after` is the young generation's bytes-used once dead pages have
    /// been freed and promoted pages have left; `capacity` is its budget.
    pub fn note_minor_complete(&self, used_after: usize, capacity: usize) {
        let floor = self.rearm_floor(used_after, capacity);
        self.young_rearm_floor.store(floor, Ordering::Relaxed);
        tracing::debug!(
            target: "zgc",
            "ZGenerationalPolicy: minor complete, young re-arm floor {} B (used {} B of {} B)",
            floor, used_after, capacity,
        );
    }

    /// Re-arm both triggers after a major cycle. A major collects young too, so
    /// both floors move.
    pub fn note_major_complete(
        &self,
        young_used_after: usize,
        young_capacity: usize,
        old_used_after: usize,
        old_capacity: usize,
    ) {
        let young_floor = self.rearm_floor(young_used_after, young_capacity);
        let old_floor = self.rearm_floor(old_used_after, old_capacity);
        self.young_rearm_floor.store(young_floor, Ordering::Relaxed);
        self.old_rearm_floor.store(old_floor, Ordering::Relaxed);
        tracing::debug!(
            target: "zgc",
            "ZGenerationalPolicy: major complete, re-arm floors young {} B / old {} B",
            young_floor, old_floor,
        );
    }

    /// `used_after + max(headroom / divisor, min_bytes)` — the same arithmetic
    /// `ZgcRealHeap`'s sweep uses to set `gc_rearm`.
    fn rearm_floor(&self, used_after: usize, capacity: usize) -> usize {
        let headroom = capacity.saturating_sub(used_after);
        let step = (headroom / self.rearm_headroom_divisor).max(self.rearm_min_bytes);
        used_after.saturating_add(step)
    }
}

// ---------------------------------------------------------------------------
// Allocation results
// ---------------------------------------------------------------------------

/// An allocation, plus the page it landed in.
///
/// The page id is returned rather than looked up again because the allocation
/// path has already resolved it, and because the generation maps are keyed by
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZGenerationAllocation {
    /// Base machine address of the object. **Uncolored.**
    pub address: usize,
    /// The page that served it.
    pub page_id: u64,
}

// ---------------------------------------------------------------------------
// Young generation
// ---------------------------------------------------------------------------

/// The young generation: every allocation starts here.
///
/// # Allocation path
///
/// Young allocation goes through [`ZPageAllocator::alloc_object`], which is
/// already the lock-free CAS-on-`top` fast path with a shared page per size
/// class. This type adds one thing: it learns which page served the allocation
/// (an O(1) [`ZPageAllocator::page_for`]) and records that page as young.
///
/// Registering a page costs a mutex acquisition, which would be unacceptable
/// per allocation, so the two shared pages' ids are cached in `hot_small` /
/// `hot_medium`. A small page serves hundreds of objects, so in steady state
/// the per-allocation cost is one atomic load and one comparison and the mutex
/// is taken once per *page*, matching `page.rs`'s own "the state mutex is taken
/// only to swap in a new page" design. Large objects get a bespoke page every
/// time and are always registered.
///
/// # Locking
///
/// `pages` is a `parking_lot::Mutex`. **Lock order is
/// `ZYoungGeneration::pages` → `ZPageAllocator`'s internal state, never the
/// reverse** — the allocator never calls back into this module. Do not format
/// this type with `{:?}` while holding its lock; `parking_lot::Mutex` is not
/// reentrant and the `Debug` impl takes it.
pub struct ZYoungGeneration {
    allocator: Arc<ZPageAllocator>,
    /// Page id → page, for every page currently owned by young.
    pages: Mutex<FxHashMap<u64, Arc<ZPageReal>>>,
    /// Id of the page that most recently served a Small allocation. `0` is
    /// "none" — [`ZPageAllocator`] ids start at 1.
    ///
    /// `Relaxed`: a pure cache whose only effect is skipping a map insert that
    /// is idempotent anyway (`entry().or_insert()`). A stale value costs one
    /// redundant lock, never a missing registration.
    hot_small: AtomicU64,
    /// Same for Medium.
    hot_medium: AtomicU64,
    /// Lifetime bytes requested through [`Self::allocate`]. `Relaxed`:
    /// monotonic counter, reporting only.
    allocated_bytes: AtomicU64,
    /// Byte budget.
    capacity: usize,
}

impl ZYoungGeneration {
    /// Build an empty young generation over `allocator` with a `capacity`-byte
    /// budget.
    pub fn new(allocator: Arc<ZPageAllocator>, capacity: usize) -> Self {
        ZYoungGeneration {
            allocator,
            pages: Mutex::new(FxHashMap::default()),
            hot_small: AtomicU64::new(0),
            hot_medium: AtomicU64::new(0),
            allocated_bytes: AtomicU64::new(0),
            capacity,
        }
    }

    /// Allocate `bytes` at `align` in the young generation.
    ///
    /// `align == 0` means the 8-byte object grid (see [`ZPageReal::alloc`]).
    pub fn allocate(
        &self,
        bytes: usize,
        align: usize,
    ) -> Result<ZGenerationAllocation, ZPageError> {
        let address = self.allocator.alloc_object(bytes, align)?;
        let page = match self.allocator.page_for(address) {
            Some(page) => page,
            // Unreachable in practice: `alloc_object` only returns an address
            // it just carved out of an installed page. Reported rather than
            // asserted because a GC data structure must not panic the VM.
            None => {
                return Err(ZPageError::AllocationRefused {
                    bytes,
                    page_size: 0,
                })
            }
        };
        let page_id = page.id();
        let hot: Option<&AtomicU64> = match page.size_class() {
            ZPageSizeClass::Small => Some(&self.hot_small),
            ZPageSizeClass::Medium => Some(&self.hot_medium),
            ZPageSizeClass::Large => None,
        };
        let already_known = match hot {
            Some(cell) => cell.load(Ordering::Relaxed) == page_id,
            None => false,
        };
        if !already_known {
            {
                let mut map = self.pages.lock();
                map.entry(page_id).or_insert_with(|| Arc::clone(&page));
            }
            if let Some(cell) = hot {
                cell.store(page_id, Ordering::Relaxed);
            }
        }
        self.allocated_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        Ok(ZGenerationAllocation { address, page_id })
    }

    /// Forget the cached shared-page ids.
    ///
    /// Must be called whenever pages may leave the young generation (start of
    /// every cycle), because a freed page's id can be handed straight back out
    /// by the allocator's page cache — the id is stable across
    /// [`ZPageReal::reset`], so a stale cache entry would suppress the
    /// re-registration of a page that is no longer in the map.
    pub fn invalidate_hot_pages(&self) {
        self.hot_small.store(0, Ordering::Relaxed);
        self.hot_medium.store(0, Ordering::Relaxed);
    }

    /// Every page currently owned by young.
    pub fn snapshot(&self) -> Vec<Arc<ZPageReal>> {
        let map = self.pages.lock();
        map.values().map(Arc::clone).collect()
    }

    /// Is this page young?
    pub fn contains_page(&self, page_id: u64) -> bool {
        self.pages.lock().contains_key(&page_id)
    }

    /// Drop `ids` from the young page set, returning the pages removed.
    pub fn remove_pages(&self, ids: &[u64]) -> Vec<Arc<ZPageReal>> {
        let mut map = self.pages.lock();
        let mut out: Vec<Arc<ZPageReal>> = Vec::with_capacity(ids.len());
        for id in ids.iter() {
            if let Some(page) = map.remove(id) {
                out.push(page);
            }
        }
        out
    }

    /// Number of young pages.
    pub fn page_count(&self) -> usize {
        self.pages.lock().len()
    }

    /// Bytes handed out by young pages.
    pub fn used(&self) -> usize {
        let map = self.pages.lock();
        map.values().map(|p| p.used()).sum()
    }

    /// Bytes the last mark found live in young.
    pub fn live_bytes(&self) -> usize {
        let map = self.pages.lock();
        map.values().map(|p| p.live_bytes()).sum()
    }

    /// Lifetime bytes requested through [`Self::allocate`].
    pub fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes.load(Ordering::Relaxed)
    }

    /// The young byte budget.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Compact by design — a derived impl would print every page.
impl std::fmt::Debug for ZYoungGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.pages.lock();
        f.debug_struct("ZYoungGeneration")
            .field("pages", &map.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Old generation
// ---------------------------------------------------------------------------

/// Everything [`ZOldGeneration`] mutates, behind one mutex.
///
/// One lock rather than one per field for the same reason `page.rs` gives:
/// installing a fresh allocation page touches the page map and the current-page
/// slots together, and splitting them would create a lock-ordering problem for
/// no gain.
#[derive(Debug)]
struct ZOldState {
    /// Page id → page, for every page currently owned by old.
    pages: FxHashMap<u64, Arc<ZPageReal>>,
    /// Page currently taking Small old allocations.
    small: Option<Arc<ZPageReal>>,
    /// Page currently taking Medium old allocations.
    medium: Option<Arc<ZPageReal>>,
}

/// The old generation: promoted pages, plus its own allocation path.
///
/// # Why old does NOT use `ZPageAllocator::alloc_object`
///
/// [`ZPageAllocator::alloc_object`] serves Small and Medium requests from *one
/// shared page per size class*. If old allocated through it, young and old
/// objects would land in the same page — and this module's whole model is that
/// **a page belongs to exactly one generation** (that is what makes promotion a
/// map move and what makes [`ZGenerationScope`] able to exclude old wholesale).
///
/// So old keeps its own current page per size class, obtained with
/// [`ZPageAllocator::alloc_page`] and bumped with [`ZPageReal::alloc`]. The
/// shared pages inside the allocator are, by construction of this module, the
/// young generation's — and a young cycle calls
/// [`ZPageAllocator::retire_shared_pages`] before it starts, so no page is
/// simultaneously an allocation target and a collection candidate.
pub struct ZOldGeneration {
    allocator: Arc<ZPageAllocator>,
    state: Mutex<ZOldState>,
    /// Lifetime pages adopted from young. `Relaxed`: reporting counter.
    promoted_pages: AtomicU64,
    /// Lifetime live bytes adopted from young.
    promoted_bytes: AtomicU64,
    /// Byte budget.
    capacity: usize,
}

impl ZOldGeneration {
    /// Build an empty old generation over `allocator` with a `capacity`-byte
    /// budget.
    pub fn new(allocator: Arc<ZPageAllocator>, capacity: usize) -> Self {
        ZOldGeneration {
            allocator,
            state: Mutex::new(ZOldState {
                pages: FxHashMap::default(),
                small: None,
                medium: None,
            }),
            promoted_pages: AtomicU64::new(0),
            promoted_bytes: AtomicU64::new(0),
            capacity,
        }
    }

    /// Allocate `bytes` at `align` directly in the old generation.
    ///
    /// This is the path a relocator/evacuator uses to place a promoted
    /// *object* (as opposed to [`Self::adopt_page`], which promotes a whole
    /// page), and the path a caller uses for an object known up front to be
    /// long-lived. It never touches the allocator's shared young pages.
    pub fn allocate(
        &self,
        bytes: usize,
        align: usize,
    ) -> Result<ZGenerationAllocation, ZPageError> {
        let class = self.allocator.config().class_for(bytes);
        let mut state = self.state.lock();

        if class == ZPageSizeClass::Large {
            // One object per page, exactly as `alloc_object` does for Large:
            // close the page immediately so nothing lands in the rounding tail.
            let page = self.allocator.alloc_page(class, bytes)?;
            let address = page
                .alloc(bytes, align)
                .ok_or(ZPageError::AllocationRefused {
                    bytes,
                    page_size: page.size(),
                })?;
            page.set_state(ZPageState::Relocatable);
            let page_id = page.id();
            state.pages.insert(page_id, page);
            return Ok(ZGenerationAllocation { address, page_id });
        }

        // Try the current page for this class. Cloned out of `state` so the
        // borrow ends before the refill path mutates it.
        let current: Option<Arc<ZPageReal>> = match class {
            ZPageSizeClass::Small => state.small.as_ref().map(Arc::clone),
            ZPageSizeClass::Medium => state.medium.as_ref().map(Arc::clone),
            ZPageSizeClass::Large => None,
        };
        if let Some(page) = current {
            if let Some(address) = page.alloc(bytes, align) {
                return Ok(ZGenerationAllocation {
                    address,
                    page_id: page.id(),
                });
            }
        }

        // Refill. Lock order: our state (held) → allocator state.
        let fresh = self.allocator.alloc_page(class, 0)?;
        let retired: Option<Arc<ZPageReal>> = match class {
            ZPageSizeClass::Small => state.small.take(),
            ZPageSizeClass::Medium => state.medium.take(),
            ZPageSizeClass::Large => None,
        };
        if let Some(page) = retired {
            // A full allocation page is a relocation candidate, same rule
            // `ZPageAllocator::refill_shared` applies to the young side.
            page.set_state(ZPageState::Relocatable);
        }
        let address = fresh
            .alloc(bytes, align)
            .ok_or(ZPageError::AllocationRefused {
                bytes,
                page_size: fresh.size(),
            })?;
        let page_id = fresh.id();
        state.pages.insert(page_id, Arc::clone(&fresh));
        match class {
            ZPageSizeClass::Small => state.small = Some(fresh),
            ZPageSizeClass::Medium => state.medium = Some(fresh),
            ZPageSizeClass::Large => {}
        }
        Ok(ZGenerationAllocation { address, page_id })
    }

    /// Take ownership of a page promoted out of young.
    ///
    /// The caller must have removed it from the young map first — a page in
    /// both maps is the bug [`ZGenerationalHeap::debug_assert_generations_disjoint`]
    /// exists to catch.
    ///
    /// The page's age is reset to 0, matching the simulation's rehoming
    /// (`page.age = 0` in `GenerationalZgc::minor_collect`): once a page is
    /// old, its young survival count means nothing.
    pub fn adopt_page(&self, page: Arc<ZPageReal>, live_bytes: usize) {
        page.set_age(0);
        if page.state() == ZPageState::Allocating {
            // Should already be closed by `retire_shared_pages`; belt and
            // braces, because an `Allocating` page in old would take young
            // objects through the allocator's shared slot.
            page.set_state(ZPageState::Relocatable);
        }
        let page_id = page.id();
        {
            let mut state = self.state.lock();
            state.pages.insert(page_id, page);
        }
        self.promoted_pages.fetch_add(1, Ordering::Relaxed);
        self.promoted_bytes
            .fetch_add(live_bytes as u64, Ordering::Relaxed);
    }

    /// Every page currently owned by old.
    pub fn snapshot(&self) -> Vec<Arc<ZPageReal>> {
        let state = self.state.lock();
        state.pages.values().map(Arc::clone).collect()
    }

    /// Is this page old?
    pub fn contains_page(&self, page_id: u64) -> bool {
        self.state.lock().pages.contains_key(&page_id)
    }

    /// Drop `ids` from the old page set, returning the pages removed. Also
    /// clears any current-allocation slot that pointed at one of them.
    pub fn remove_pages(&self, ids: &[u64]) -> Vec<Arc<ZPageReal>> {
        let mut state = self.state.lock();
        let mut out: Vec<Arc<ZPageReal>> = Vec::with_capacity(ids.len());
        for id in ids.iter() {
            if let Some(page) = state.pages.remove(id) {
                if state.small.as_ref().map(|p| p.id()) == Some(*id) {
                    state.small = None;
                }
                if state.medium.as_ref().map(|p| p.id()) == Some(*id) {
                    state.medium = None;
                }
                out.push(page);
            }
        }
        out
    }

    /// Close the current old allocation pages so a collection sees no page in
    /// [`ZPageState::Allocating`]. The old-side twin of
    /// [`ZPageAllocator::retire_shared_pages`].
    pub fn retire_allocation_pages(&self) {
        let mut state = self.state.lock();
        if let Some(page) = state.small.take() {
            page.set_state(ZPageState::Relocatable);
        }
        if let Some(page) = state.medium.take() {
            page.set_state(ZPageState::Relocatable);
        }
    }

    /// Number of old pages.
    pub fn page_count(&self) -> usize {
        self.state.lock().pages.len()
    }

    /// Bytes handed out by old pages.
    pub fn used(&self) -> usize {
        let state = self.state.lock();
        state.pages.values().map(|p| p.used()).sum()
    }

    /// Bytes the last mark found live in old.
    pub fn live_bytes(&self) -> usize {
        let state = self.state.lock();
        state.pages.values().map(|p| p.live_bytes()).sum()
    }

    /// Lifetime pages adopted from young.
    pub fn promoted_pages(&self) -> u64 {
        self.promoted_pages.load(Ordering::Relaxed)
    }

    /// Lifetime live bytes adopted from young.
    pub fn promoted_bytes(&self) -> u64 {
        self.promoted_bytes.load(Ordering::Relaxed)
    }

    /// The old byte budget.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Compact by design — see [`ZYoungGeneration`]'s impl.
impl std::fmt::Debug for ZOldGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("ZOldGeneration")
            .field("pages", &state.pages.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Cycle reports
// ---------------------------------------------------------------------------

/// What one minor cycle did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ZMinorCycleReport {
    /// Minor cycle number (starts at 1).
    pub cycle: u64,
    /// Young pages the cycle considered.
    pub pages_examined: usize,
    /// Young pages found wholly dead and returned to the allocator.
    pub pages_freed: usize,
    /// Bytes those pages had handed out.
    pub bytes_freed: usize,
    /// Young pages promoted to old.
    pub pages_promoted: usize,
    /// Live bytes on the promoted pages.
    pub bytes_promoted: usize,
    /// Live bytes still in young after the cycle.
    pub young_live_bytes: usize,
    /// Bytes young pages have handed out after the cycle.
    pub young_used_after: usize,
    /// Objects the marker reported.
    pub objects_marked: u64,
    /// Roots the cycle handed the marker (caller roots plus remembered-set
    /// targets, after scope filtering).
    pub roots_scanned: usize,
    /// Remembered-set entries offered by the view.
    pub remembered_entries: usize,
    /// Addresses the scope admitted.
    pub scope_admitted: u64,
    /// Addresses the scope refused — every one of these is a pointer the cycle
    /// declined to follow out of the young generation.
    pub scope_rejected: u64,
}

/// What one major cycle did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ZMajorCycleReport {
    /// Major cycle number (starts at 1).
    pub cycle: u64,
    /// Young-side outcome of the same whole-heap mark.
    pub young: ZMinorCycleReport,
    /// Old pages the cycle considered.
    pub old_pages_examined: usize,
    /// Old pages found wholly dead and returned to the allocator.
    pub old_pages_freed: usize,
    /// Bytes those pages had handed out.
    pub old_bytes_freed: usize,
    /// Live bytes still in old after the cycle.
    pub old_live_bytes: usize,
    /// Bytes old pages have handed out after the cycle.
    pub old_used_after: usize,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How to size and schedule a [`ZGenerationalHeap`].
#[derive(Debug, Clone)]
pub struct ZGenerationalConfig {
    /// Young share of the allocator's budget, clamped to `0.05..=0.95`.
    pub young_fraction: f64,
    /// Aging / promotion rule.
    pub promotion: ZPromotionPolicy,
    /// Young occupancy above which a minor is wanted.
    pub young_minor_threshold: f64,
    /// Old occupancy above which a major is wanted.
    pub old_major_threshold: f64,
    /// Minor cycles between periodic majors; `0` disables the upgrade.
    pub minors_per_major: u64,
    /// Absolute re-arm floor step.
    pub rearm_min_bytes: usize,
    /// Headroom divisor for the re-arm floor step.
    pub rearm_headroom_divisor: usize,
}

impl Default for ZGenerationalConfig {
    fn default() -> Self {
        ZGenerationalConfig {
            young_fraction: Z_DEFAULT_YOUNG_FRACTION,
            promotion: ZPromotionPolicy::default(),
            young_minor_threshold: Z_YOUNG_OCCUPANCY_MINOR_THRESHOLD,
            old_major_threshold: Z_OLD_OCCUPANCY_MAJOR_THRESHOLD,
            minors_per_major: Z_DEFAULT_MINORS_PER_MAJOR,
            rearm_min_bytes: Z_REARM_MIN_BYTES,
            rearm_headroom_divisor: Z_REARM_HEADROOM_DIVISOR,
        }
    }
}

// ---------------------------------------------------------------------------
// ZGenerationalHeap — the driver
// ---------------------------------------------------------------------------

/// The generational layer over a [`ZPageAllocator`].
///
/// # Lock order
///
/// `ZYoungGeneration::pages` → `ZOldGeneration::state` → `ZPageAllocator`'s
/// internal state. Never the reverse; the allocator never calls back into this
/// module. No method here holds two of this module's locks at once.
///
/// # Deviations from the `GenerationalZgc` simulation
///
/// * **The major cycle is one whole-heap mark, not "a minor followed by an old
///   pass".** See [`Self::collect_major`] for why the simulation's shape is
///   unsafe over real storage.
/// * **Promotion moves the page's `Arc`, not a copy.** The simulation removed a
///   page from one `Vec` and pushed it onto another with a fresh id; here the
///   id is the map key and must not change, so only the map membership moves.
/// * **A dead page is returned to [`ZPageAllocator`]**, which zeroes it (see
///   [`ZPageReal::reset`] on why the zero-fill is mandatory under conservative
///   scanning) and caches it. The simulation simply dropped a struct.
/// * **The trigger has a re-arm floor.** The simulation had none, and neither
///   did `ZgcRealHeap`, which is what produced the documented GC storm. See
///   [`ZGenerationalPolicy`].
pub struct ZGenerationalHeap {
    allocator: Arc<ZPageAllocator>,
    young: ZYoungGeneration,
    old: ZOldGeneration,
    promotion: ZPromotionPolicy,
    policy: ZGenerationalPolicy,
    /// Lifetime minor cycles. `Relaxed`: cycle numbering and reporting; the
    /// cycles themselves are serialized by the caller's safepoint, so nothing
    /// is published through this counter.
    minor_cycles: AtomicU64,
    /// Lifetime major cycles. Same reasoning.
    major_cycles: AtomicU64,
    /// Minor cycles since the last major. Same reasoning.
    minors_since_major: AtomicU64,
}

impl ZGenerationalHeap {
    /// Build a generational heap over `allocator`.
    ///
    /// The budget split follows `GenerationalZgc::with_total_size`: young takes
    /// `young_fraction` (clamped to `0.05..=0.95`) of
    /// [`ZPageAllocator::max_capacity`], old takes the rest. The split is a
    /// *policy* figure used for occupancy and the trigger; it is not a hard
    /// partition of the reservation, because pages are drawn from one granule
    /// pool and either generation may hold any page.
    pub fn new(allocator: Arc<ZPageAllocator>, config: ZGenerationalConfig) -> Self {
        let total = allocator.max_capacity();
        let young_capacity = ((total as f64) * config.young_fraction.clamp(0.05, 0.95)) as usize;
        let young_capacity = young_capacity.max(1);
        let old_capacity = total.saturating_sub(young_capacity).max(1);

        tracing::debug!(
            target: "zgc",
            "ZGenerationalHeap: young {} B / old {} B of {} B; promotion_age={}, \
             minor@{:.2}, major@{:.2}, minors_per_major={}",
            young_capacity,
            old_capacity,
            total,
            config.promotion.promotion_age,
            config.young_minor_threshold,
            config.old_major_threshold,
            config.minors_per_major,
        );

        ZGenerationalHeap {
            young: ZYoungGeneration::new(Arc::clone(&allocator), young_capacity),
            old: ZOldGeneration::new(Arc::clone(&allocator), old_capacity),
            promotion: config.promotion,
            policy: ZGenerationalPolicy::new(
                config.young_minor_threshold,
                config.old_major_threshold,
                config.minors_per_major,
                config.rearm_min_bytes,
                config.rearm_headroom_divisor,
            ),
            allocator,
            minor_cycles: AtomicU64::new(0),
            major_cycles: AtomicU64::new(0),
            minors_since_major: AtomicU64::new(0),
        }
    }

    /// Build with [`ZGenerationalConfig::default`].
    pub fn with_defaults(allocator: Arc<ZPageAllocator>) -> Self {
        Self::new(allocator, ZGenerationalConfig::default())
    }

    /// The underlying page allocator.
    pub fn allocator(&self) -> &Arc<ZPageAllocator> {
        &self.allocator
    }

    /// The young generation.
    pub fn young(&self) -> &ZYoungGeneration {
        &self.young
    }

    /// The old generation.
    pub fn old(&self) -> &ZOldGeneration {
        &self.old
    }

    /// The promotion rule in force.
    pub fn promotion_policy(&self) -> &ZPromotionPolicy {
        &self.promotion
    }

    /// The scheduler.
    pub fn policy(&self) -> &ZGenerationalPolicy {
        &self.policy
    }

    // -- allocation ---------------------------------------------------------

    /// Allocate in the young generation. **Every allocation starts here** —
    /// promotion is the only way into old.
    pub fn allocate_young(
        &self,
        bytes: usize,
        align: usize,
    ) -> Result<ZGenerationAllocation, ZPageError> {
        let allocation = self.young.allocate(bytes, align)?;
        debug_assert!(
            !self.old.contains_page(allocation.page_id),
            "young allocation landed in page {} which the old generation owns — the \
             one-generation-per-page invariant is broken",
            allocation.page_id,
        );
        Ok(allocation)
    }

    /// Allocate directly in the old generation. See [`ZOldGeneration::allocate`]
    /// for when that is legitimate.
    pub fn allocate_old(
        &self,
        bytes: usize,
        align: usize,
    ) -> Result<ZGenerationAllocation, ZPageError> {
        let allocation = self.old.allocate(bytes, align)?;
        debug_assert!(
            !self.young.contains_page(allocation.page_id),
            "old allocation landed in page {} which the young generation owns",
            allocation.page_id,
        );
        Ok(allocation)
    }

    // -- generation mapping -------------------------------------------------

    /// Which generation owns this page, if either.
    pub fn page_generation(&self, page_id: u64) -> Option<ZGeneration> {
        if self.young.contains_page(page_id) {
            return Some(ZGeneration::Young);
        }
        if self.old.contains_page(page_id) {
            return Some(ZGeneration::Old);
        }
        None
    }

    /// Which generation owns the page containing `addr`, if any.
    ///
    /// One O(1) [`ZPageAllocator::page_for`] plus one hash probe per
    /// generation.
    pub fn generation_of_address(&self, addr: usize) -> Option<ZGeneration> {
        let page = self.allocator.page_for(addr)?;
        self.page_generation(page.id())
    }

    /// Debug-only invariant: no page is in both generations.
    ///
    /// O(young + old) pages, so it is a `debug_assertions`-gated audit, not
    /// something a cycle runs unconditionally.
    pub fn debug_assert_generations_disjoint(&self) {
        #[cfg(debug_assertions)]
        {
            let young: FxHashSet<u64> = self.young.snapshot().iter().map(|p| p.id()).collect();
            for page in self.old.snapshot() {
                debug_assert!(
                    !young.contains(&page.id()),
                    "page {} is in BOTH generations",
                    page.id(),
                );
            }
        }
    }

    // -- statistics ---------------------------------------------------------

    /// Snapshot both generations. O(pages).
    pub fn stats(&self) -> ZGenerationalStats {
        ZGenerationalStats {
            young_used: self.young.used(),
            young_capacity: self.young.capacity(),
            young_live: self.young.live_bytes(),
            young_pages: self.young.page_count(),
            old_used: self.old.used(),
            old_capacity: self.old.capacity(),
            old_live: self.old.live_bytes(),
            old_pages: self.old.page_count(),
            minor_cycles: self.minor_cycles.load(Ordering::Relaxed),
            major_cycles: self.major_cycles.load(Ordering::Relaxed),
            minors_since_major: self.minors_since_major.load(Ordering::Relaxed),
            promoted_pages: self.old.promoted_pages(),
            promoted_bytes: self.old.promoted_bytes(),
        }
    }

    /// Ask the scheduler what, if anything, to run now.
    pub fn should_collect(&self) -> Option<ZGenerationalTrigger> {
        self.policy.should_collect(&self.stats())
    }

    // -- minor cycle --------------------------------------------------------

    /// **Minor collection: young only.**
    ///
    /// 1. Retire the mutator's shared allocation pages
    ///    ([`ZPageAllocator::retire_shared_pages`]) so no page is both an
    ///    allocation target and a collection candidate, and forget the cached
    ///    hot-page ids.
    /// 2. Snapshot the young page set and build a [`ZGenerationScope`] over it
    ///    **and nothing else**.
    /// 3. Root set = caller roots ∪ remembered-set young targets, each filtered
    ///    through the scope. An entry that no longer names a young address is
    ///    dropped as a stale card rather than followed.
    /// 4. Mark, via [`ZGenerationMarker`], inside the scope.
    /// 5. Free wholly dead young pages, age the survivors, promote per
    ///    [`ZPromotionPolicy`].
    ///
    /// # Why this cannot trace into old — and what enforces it
    ///
    /// * The scope is built from young pages only, so
    ///   [`ZGenerationScope::admits`] is `false` for every old address and
    ///   [`ZGenerationScope::page_of`] returns `None`. The marker gets no other
    ///   handle capable of classifying an address.
    /// * The remembered-set view yields **young targets**, so no old memory is
    ///   dereferenced to expand the root set (see [`ZRememberedSetView`]).
    /// * `debug_assert!`s enforce it after the fact, and they are the assertions
    ///   to look at if this ever regresses:
    ///   - the scope is not a whole-heap scope;
    ///   - **every page id in the [`ZMarkReport`] is in the scope**, i.e. the
    ///     marker credited liveness only to young pages;
    ///   - **the old generation's total live bytes is identical before and
    ///     after the cycle**, i.e. nothing about old was even written.
    ///
    /// The caller is responsible for running this at a safepoint; nothing here
    /// stops the world.
    pub fn collect_young(
        &self,
        roots: &[u64],
        remembered: &dyn ZRememberedSetView,
        marker: &dyn ZGenerationMarker,
    ) -> ZMinorCycleReport {
        let cycle = self.minor_cycles.fetch_add(1, Ordering::Relaxed) + 1;
        let id = ZGenerationId::new(ZGeneration::Young, cycle);

        // 1. Close the mutator's allocation pages.
        self.allocator.retire_shared_pages();
        self.young.invalidate_hot_pages();

        // 2. Snapshot + scope.
        let young_pages = self.young.snapshot();
        let scope = ZGenerationScope::from_pages(ZGeneration::Young, cycle, &young_pages, false);
        debug_assert!(
            !scope.covers_whole_heap(),
            "a minor cycle must never be handed a whole-heap scope",
        );

        // Captured BEFORE the marker runs, so the post-cycle comparison covers
        // the mark as well as the sweep.
        #[cfg(debug_assertions)]
        let old_live_before = self.old.live_bytes();

        // 3. Root set.
        let mut root_set: Vec<u64> = Vec::with_capacity(roots.len());
        for root in roots.iter() {
            if scope.admits(*root as usize) {
                root_set.push(*root);
            }
        }
        let mut remembered_entries: usize = 0;
        remembered.iterate_old_to_young(&mut |addr: u64| {
            remembered_entries += 1;
            if scope.admits(addr as usize) {
                root_set.push(addr);
            }
        });

        // 4. Mark.
        let report = marker.mark_from_roots(id, &root_set, &scope);
        #[cfg(debug_assertions)]
        {
            for page_id in report.live_bytes_by_page.keys() {
                debug_assert!(
                    scope.contains_page(*page_id),
                    "minor cycle {cycle}: marker credited liveness to page {page_id}, \
                     which is not in the young scope — the minor cycle traced out of young",
                );
            }
        }

        // 5. Sweep + promote.
        let mut out = self.sweep_young(&young_pages, &report, cycle);
        out.objects_marked = report.objects_marked;
        out.roots_scanned = root_set.len();
        out.remembered_entries = remembered.entry_count().max(remembered_entries);
        out.scope_admitted = scope.admitted();
        out.scope_rejected = scope.rejected();

        // A minor cycle must not *trace* into old — but promotion legitimately
        // hands old some pages, and `ZOldGeneration::live_bytes()` sums over the
        // old page map, so it rises by exactly the promoted bytes. The original
        // form of this assertion compared against `old_live_before` alone and so
        // fired on every cycle that promoted anything, i.e. in every debug build
        // the moment the promotion policy did its job. The invariant worth
        // keeping is the *accounted* one: old changed by promotion and by
        // nothing else.
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            old_live_before + out.bytes_promoted,
            self.old.live_bytes(),
            "minor cycle {} changed the old generation's live-byte total by \
             something other than promotion ({} bytes promoted) — a minor cycle \
             must not trace into old",
            cycle,
            out.bytes_promoted,
        );
        self.debug_assert_generations_disjoint();

        self.minors_since_major.fetch_add(1, Ordering::Relaxed);
        self.policy
            .note_minor_complete(out.young_used_after, self.young.capacity());

        tracing::debug!(
            target: "zgc",
            "ZGC minor {}: examined {} young pages, freed {} ({} B), promoted {} ({} B), \
             live {} B, scope admitted {} / rejected {}",
            cycle, out.pages_examined, out.pages_freed, out.bytes_freed,
            out.pages_promoted, out.bytes_promoted, out.young_live_bytes,
            out.scope_admitted, out.scope_rejected,
        );
        out
    }

    // -- major cycle --------------------------------------------------------

    /// **Major collection: one whole-heap mark applied to both generations.**
    ///
    /// # Why not the simulation's shape
    ///
    /// `GenerationalZgc::major_collect` runs a minor cycle and *then* a
    /// separate old-generation pass rooted only in the caller's roots. Over the
    /// simulation's fake storage that is harmless. Over real storage it is a
    /// correctness bug: an old object reachable only through a young survivor
    /// (`root → young object → old object`) is invisible to the second pass,
    /// which would then free a page holding a live object. The remembered set
    /// does not help — it records old→young, which is the other direction.
    ///
    /// So the major cycle here builds **one scope spanning both generations**
    /// and marks once. That is also strictly less work than marking twice.
    /// [`ZGenerationScope::covers_whole_heap`] reports `true` for it, which is
    /// exactly the flag a minor cycle asserts is `false`.
    ///
    /// The remembered set is *not* consulted: a whole-heap mark subsumes it,
    /// and consuming it here would be redundant work. (Whether the set should
    /// be *reset* after a major is the remembered-set module's decision, not
    /// this one's.)
    pub fn collect_major(
        &self,
        roots: &[u64],
        marker: &dyn ZGenerationMarker,
    ) -> ZMajorCycleReport {
        let cycle = self.major_cycles.fetch_add(1, Ordering::Relaxed) + 1;
        let id = ZGenerationId::new(ZGeneration::Old, cycle);

        self.allocator.retire_shared_pages();
        self.young.invalidate_hot_pages();
        self.old.retire_allocation_pages();

        let young_pages = self.young.snapshot();
        let old_pages = self.old.snapshot();
        let mut all_pages: Vec<Arc<ZPageReal>> =
            Vec::with_capacity(young_pages.len() + old_pages.len());
        all_pages.extend(young_pages.iter().map(Arc::clone));
        all_pages.extend(old_pages.iter().map(Arc::clone));
        let scope = ZGenerationScope::from_pages(ZGeneration::Old, cycle, &all_pages, true);

        let mut root_set: Vec<u64> = Vec::with_capacity(roots.len());
        for root in roots.iter() {
            if scope.admits(*root as usize) {
                root_set.push(*root);
            }
        }

        let report = marker.mark_from_roots(id, &root_set, &scope);

        let mut young_out = self.sweep_young(&young_pages, &report, cycle);
        young_out.objects_marked = report.objects_marked;
        young_out.roots_scanned = root_set.len();
        young_out.scope_admitted = scope.admitted();
        young_out.scope_rejected = scope.rejected();
        let old_out = self.sweep_old(&old_pages, &report);

        self.debug_assert_generations_disjoint();
        self.minors_since_major.store(0, Ordering::Relaxed);
        self.policy.note_major_complete(
            young_out.young_used_after,
            self.young.capacity(),
            old_out.old_used_after,
            self.old.capacity(),
        );

        tracing::debug!(
            target: "zgc",
            "ZGC major {}: young freed {} ({} B) promoted {}; old examined {} freed {} ({} B), \
             old live {} B",
            cycle, young_out.pages_freed, young_out.bytes_freed, young_out.pages_promoted,
            old_out.old_pages_examined, old_out.old_pages_freed, old_out.old_bytes_freed,
            old_out.old_live_bytes,
        );

        ZMajorCycleReport {
            cycle,
            young: young_out,
            old_pages_examined: old_out.old_pages_examined,
            old_pages_freed: old_out.old_pages_freed,
            old_bytes_freed: old_out.old_bytes_freed,
            old_live_bytes: old_out.old_live_bytes,
            old_used_after: old_out.old_used_after,
        }
    }

    // -- sweeps -------------------------------------------------------------

    /// Apply a mark report to the young page set: publish live bytes, free the
    /// dead, age and promote the survivors.
    fn sweep_young(
        &self,
        young_pages: &[Arc<ZPageReal>],
        report: &ZMarkReport,
        cycle: u64,
    ) -> ZMinorCycleReport {
        let mut out = ZMinorCycleReport {
            cycle,
            pages_examined: young_pages.len(),
            ..ZMinorCycleReport::default()
        };

        // Pass 1: classify. A page absent from the report holds nothing live.
        let mut dead_ids: Vec<u64> = Vec::new();
        let mut dead_pages: Vec<Arc<ZPageReal>> = Vec::new();
        // (page, live bytes, age after this cycle)
        let mut survivors: Vec<(Arc<ZPageReal>, usize, u32)> = Vec::new();
        for page in young_pages.iter() {
            let live = report
                .live_bytes_by_page
                .get(&page.id())
                .copied()
                .unwrap_or(0);
            page.set_live_bytes(live);
            if live == 0 {
                out.pages_freed += 1;
                out.bytes_freed += page.used();
                dead_ids.push(page.id());
                dead_pages.push(Arc::clone(page));
                continue;
            }
            let age = page.age().saturating_add(1);
            page.set_age(age);
            survivors.push((Arc::clone(page), live, age));
        }

        // Pass 2: promotion. The survival-pressure rule (disabled by default)
        // needs the survivor count, which is why this is a second pass.
        let promote_all = self
            .promotion
            .promote_all_survivors(survivors.len(), young_pages.len());
        let mut promote_ids: Vec<u64> = Vec::new();
        let mut promote_pages: Vec<(Arc<ZPageReal>, usize)> = Vec::new();
        for (page, live, age) in survivors.iter() {
            if promote_all || self.promotion.should_promote(*age) {
                out.pages_promoted += 1;
                out.bytes_promoted += *live;
                promote_ids.push(page.id());
                promote_pages.push((Arc::clone(page), *live));
            } else {
                out.young_live_bytes += *live;
                out.young_used_after += page.used();
            }
        }

        // Remove from young BEFORE handing a page anywhere else: a freed page
        // keeps its id and can be recycled by the allocator immediately, and a
        // promoted page must never be in both maps.
        self.young.remove_pages(&dead_ids);
        self.young.remove_pages(&promote_ids);
        for page in dead_pages.iter() {
            self.allocator.free_page(page);
        }
        for (page, live) in promote_pages.into_iter() {
            self.old.adopt_page(page, live);
        }

        out
    }

    /// Apply a mark report to the old page set. Old pages are never aged and
    /// never promoted — there is nowhere for them to go.
    fn sweep_old(&self, old_pages: &[Arc<ZPageReal>], report: &ZMarkReport) -> OldSweepOutcome {
        let mut out = OldSweepOutcome {
            old_pages_examined: old_pages.len(),
            ..OldSweepOutcome::default()
        };
        let mut dead_ids: Vec<u64> = Vec::new();
        let mut dead_pages: Vec<Arc<ZPageReal>> = Vec::new();
        for page in old_pages.iter() {
            let live = report
                .live_bytes_by_page
                .get(&page.id())
                .copied()
                .unwrap_or(0);
            page.set_live_bytes(live);
            if live == 0 {
                out.old_pages_freed += 1;
                out.old_bytes_freed += page.used();
                dead_ids.push(page.id());
                dead_pages.push(Arc::clone(page));
                continue;
            }
            out.old_live_bytes += live;
            out.old_used_after += page.used();
        }
        self.old.remove_pages(&dead_ids);
        for page in dead_pages.iter() {
            self.allocator.free_page(page);
        }
        out
    }
}

/// Old-side half of a major cycle's outcome.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct OldSweepOutcome {
    old_pages_examined: usize,
    old_pages_freed: usize,
    old_bytes_freed: usize,
    old_live_bytes: usize,
    old_used_after: usize,
}

/// Compact by design — the generations print their own page counts.
impl std::fmt::Debug for ZGenerationalHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZGenerationalHeap")
            .field("young", &self.young)
            .field("old", &self.old)
            .field("minor_cycles", &self.minor_cycles.load(Ordering::Relaxed))
            .field("major_cycles", &self.major_cycles.load(Ordering::Relaxed))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::zgc::page::ZPageConfig;

    /// The same miniature geometry `page.rs`'s own tests use: OpenJDK's shape
    /// at 1/512 scale, so a test heap is 256 KiB instead of 256 MiB.
    fn test_page_config() -> ZPageConfig {
        ZPageConfig {
            granule_size: 4096,
            small_page_size: 8192,
            medium_page_size: 65536,
            small_object_limit: 1024,
            medium_object_limit: 8192,
            max_capacity: 4096 * 64,
        }
    }

    fn allocator() -> Arc<ZPageAllocator> {
        Arc::new(ZPageAllocator::new(test_page_config()).expect("test geometry must validate"))
    }

    fn heap_with(promotion: ZPromotionPolicy) -> ZGenerationalHeap {
        let config = ZGenerationalConfig {
            promotion,
            ..ZGenerationalConfig::default()
        };
        ZGenerationalHeap::new(allocator(), config)
    }

    /// 512-byte objects: Small class, 16 to an 8 KiB page.
    const OBJ: usize = 512;

    // -- test markers -------------------------------------------------------

    /// Marks nothing. Every page in scope is reclaimable.
    struct MarkNothing;

    impl ZGenerationMarker for MarkNothing {
        fn mark_from_roots(
            &self,
            _id: ZGenerationId,
            _roots: &[u64],
            _scope: &ZGenerationScope,
        ) -> ZMarkReport {
            ZMarkReport::new()
        }
    }

    /// Marks every page in scope fully live.
    struct MarkAllInScope;

    impl ZGenerationMarker for MarkAllInScope {
        fn mark_from_roots(
            &self,
            _id: ZGenerationId,
            _roots: &[u64],
            scope: &ZGenerationScope,
        ) -> ZMarkReport {
            let mut report = ZMarkReport::new();
            for entry in scope.entries() {
                let extent = entry.end.saturating_sub(entry.start);
                if extent > 0 {
                    report.record(entry.page_id, extent);
                    report.objects_marked += 1;
                }
            }
            report
        }
    }

    /// The instrument for the no-tracing-into-old claim.
    ///
    /// Records every page the scope exposed, and probes a set of known
    /// old-generation addresses through the scope, counting how many the scope
    /// was willing to admit. The count must be zero.
    struct CountingMarker {
        /// Addresses inside old pages, supplied by the test.
        old_probes: Vec<usize>,
        visited_pages: Mutex<Vec<u64>>,
        old_admitted: AtomicU64,
        whole_heap_scopes: AtomicU64,
        calls: AtomicU64,
    }

    impl CountingMarker {
        fn new(old_probes: Vec<usize>) -> Self {
            CountingMarker {
                old_probes,
                visited_pages: Mutex::new(Vec::new()),
                old_admitted: AtomicU64::new(0),
                whole_heap_scopes: AtomicU64::new(0),
                calls: AtomicU64::new(0),
            }
        }
    }

    impl ZGenerationMarker for CountingMarker {
        fn mark_from_roots(
            &self,
            _id: ZGenerationId,
            _roots: &[u64],
            scope: &ZGenerationScope,
        ) -> ZMarkReport {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if scope.covers_whole_heap() {
                self.whole_heap_scopes.fetch_add(1, Ordering::Relaxed);
            }
            {
                let mut visited = self.visited_pages.lock();
                for entry in scope.entries() {
                    visited.push(entry.page_id);
                }
            }
            for probe in self.old_probes.iter() {
                if scope.admits(*probe) {
                    self.old_admitted.fetch_add(1, Ordering::Relaxed);
                }
            }
            ZMarkReport::new()
        }
    }

    /// A remembered set backed by a fixed list of young targets.
    struct FixedRememberedSet {
        targets: Vec<u64>,
    }

    impl ZRememberedSetView for FixedRememberedSet {
        fn iterate_old_to_young(&self, f: &mut dyn FnMut(u64)) {
            for t in self.targets.iter() {
                f(*t);
            }
        }

        fn entry_count(&self) -> usize {
            self.targets.len()
        }
    }

    // -- allocation routing -------------------------------------------------

    #[test]
    fn young_allocation_routes_to_young_pages() {
        let heap = heap_with(ZPromotionPolicy::default());
        let mut addrs: Vec<usize> = Vec::new();
        for _ in 0..40 {
            let a = heap.allocate_young(OBJ, 8).expect("young allocation");
            addrs.push(a.address);
        }

        assert!(heap.young().page_count() >= 2, "40 x 512 B spans >1 page");
        assert_eq!(heap.old().page_count(), 0, "nothing may reach old yet");

        for addr in addrs.iter() {
            assert_eq!(
                heap.generation_of_address(*addr),
                Some(ZGeneration::Young),
                "address {addr:#x} did not resolve to the young generation",
            );
        }
        let stats = heap.stats();
        assert!(stats.young_used >= 40 * OBJ);
        assert_eq!(stats.old_used, 0);
        assert_eq!(stats.promoted_pages, 0);
    }

    #[test]
    fn old_allocation_uses_pages_young_never_owns() {
        let heap = heap_with(ZPromotionPolicy::default());
        for _ in 0..20 {
            heap.allocate_young(OBJ, 8).expect("young allocation");
        }
        for _ in 0..20 {
            heap.allocate_old(OBJ, 8).expect("old allocation");
        }

        let young: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
        let old: FxHashSet<u64> = heap.old().snapshot().iter().map(|p| p.id()).collect();
        assert!(!young.is_empty());
        assert!(!old.is_empty());
        assert!(
            young.intersection(&old).next().is_none(),
            "a page is owned by both generations: young={young:?} old={old:?}",
        );
        heap.debug_assert_generations_disjoint();
    }

    // -- THE central test ---------------------------------------------------

    /// A minor cycle must free dead young pages and must not be able to see the
    /// old generation at all.
    #[test]
    fn minor_cycle_frees_dead_young_pages_and_never_visits_old() {
        let heap = heap_with(ZPromotionPolicy::default());

        for _ in 0..32 {
            heap.allocate_young(OBJ, 8).expect("young allocation");
        }
        let mut old_addrs: Vec<usize> = Vec::new();
        for _ in 0..8 {
            old_addrs.push(heap.allocate_old(OBJ, 8).expect("old allocation").address);
        }

        let young_ids: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
        let old_pages = heap.old().snapshot();
        let old_ids: FxHashSet<u64> = old_pages.iter().map(|p| p.id()).collect();
        assert!(!young_ids.is_empty());
        assert!(!old_ids.is_empty());

        // Sentinel: if the minor cycle touched old at all, this moves.
        for page in old_pages.iter() {
            page.set_live_bytes(1234);
        }
        let old_page_count_before = heap.old().page_count();

        let marker = CountingMarker::new(old_addrs.clone());
        let report = heap.collect_young(&[], &ZEmptyRememberedSet, &marker);

        // The marker was offered young pages only.
        let visited: FxHashSet<u64> = marker.visited_pages.lock().iter().copied().collect();
        assert!(
            visited.intersection(&old_ids).next().is_none(),
            "the minor cycle exposed old pages {:?} to the marker",
            visited.intersection(&old_ids).collect::<Vec<_>>(),
        );
        assert_eq!(
            visited, young_ids,
            "the scope must be exactly the young set"
        );

        // Every probe of a real old address was refused.
        assert_eq!(
            marker.old_admitted.load(Ordering::Relaxed),
            0,
            "the young scope admitted an old-generation address",
        );
        assert_eq!(
            marker.whole_heap_scopes.load(Ordering::Relaxed),
            0,
            "a minor cycle handed out a whole-heap scope",
        );
        assert_eq!(marker.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            report.scope_rejected,
            old_addrs.len() as u64,
            "every out-of-scope probe should have been counted as a rejection",
        );

        // Old is untouched: same pages, same live bytes.
        assert_eq!(heap.old().page_count(), old_page_count_before);
        for page in old_pages.iter() {
            assert_eq!(
                page.live_bytes(),
                1234,
                "the minor cycle rewrote an old page's live bytes",
            );
        }

        // Young is reclaimed.
        assert_eq!(heap.young().page_count(), 0, "all young pages were dead");
        assert_eq!(report.pages_examined, young_ids.len());
        assert_eq!(report.pages_freed, young_ids.len());
        assert_eq!(report.pages_promoted, 0);
        assert!(report.bytes_freed >= 32 * OBJ);
        assert_eq!(report.young_live_bytes, 0);
    }

    #[test]
    fn minor_cycle_takes_remembered_set_targets_as_roots_and_filters_stale_ones() {
        let heap = heap_with(ZPromotionPolicy::default());
        let mut young_addrs: Vec<u64> = Vec::new();
        for _ in 0..16 {
            young_addrs.push(heap.allocate_young(OBJ, 8).expect("young").address as u64);
        }
        let old_addr = heap.allocate_old(OBJ, 8).expect("old").address as u64;

        // One live young target, one address that is not young at all (an old
        // address standing in for a stale card).
        let rset = FixedRememberedSet {
            targets: vec![young_addrs[0], old_addr],
        };
        let report = heap.collect_young(&[], &rset, &MarkAllInScope);

        assert_eq!(report.remembered_entries, 2);
        assert_eq!(
            report.roots_scanned, 1,
            "only the young target may become a root; the old address is a stale card",
        );
    }

    // -- promotion ----------------------------------------------------------

    #[test]
    fn pages_promote_after_surviving_n_minor_cycles() {
        let heap = heap_with(ZPromotionPolicy::with_age(2));
        for _ in 0..20 {
            heap.allocate_young(OBJ, 8).expect("young allocation");
        }
        let young_before: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
        assert_eq!(
            young_before.len(),
            2,
            "20 x 512 B fills 1 page and starts a 2nd"
        );

        // Cycle 1: everything survives, age 0 -> 1, nothing promoted.
        let first = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkAllInScope);
        assert_eq!(first.pages_promoted, 0);
        assert_eq!(first.pages_freed, 0);
        assert_eq!(heap.young().page_count(), 2);
        assert_eq!(heap.old().page_count(), 0);

        // Cycle 2: age 1 -> 2 >= promotion_age, both pages promote.
        let second = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkAllInScope);
        assert_eq!(second.pages_promoted, 2);
        assert_eq!(heap.young().page_count(), 0);
        assert_eq!(heap.old().page_count(), 2);

        let young_after: FxHashSet<u64> = heap.old().snapshot().iter().map(|p| p.id()).collect();
        assert_eq!(
            young_after, young_before,
            "promotion must move the same pages, not copies",
        );
        let stats = heap.stats();
        assert_eq!(stats.promoted_pages, 2);
        assert_eq!(stats.minor_cycles, 2);
        heap.debug_assert_generations_disjoint();
    }

    #[test]
    fn promotion_age_zero_promotes_every_first_cycle_survivor() {
        let heap = heap_with(ZPromotionPolicy::with_age(0));
        for _ in 0..8 {
            heap.allocate_young(OBJ, 8).expect("young allocation");
        }
        let report = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkAllInScope);
        assert_eq!(report.pages_promoted, 1);
        assert_eq!(heap.old().page_count(), 1);
    }

    // -- trigger policy -----------------------------------------------------

    fn pressure_stats(young_used: usize, old_used: usize) -> ZGenerationalStats {
        ZGenerationalStats {
            young_used,
            young_capacity: 4 * 1024 * 1024,
            old_used,
            old_capacity: 12 * 1024 * 1024,
            ..ZGenerationalStats::default()
        }
    }

    #[test]
    fn young_pressure_fires_a_minor_before_a_major() {
        let policy = ZGenerationalPolicy::default();
        // Young 75% (> 0.50), old 10% (< 0.80).
        let stats = pressure_stats(3 * 1024 * 1024, 1024 * 1024);
        assert_eq!(
            policy.should_collect(&stats),
            Some(ZGenerationalTrigger::Minor),
        );
    }

    #[test]
    fn old_pressure_outranks_young_pressure() {
        let policy = ZGenerationalPolicy::default();
        // Both over threshold: the major branch is tested first.
        let stats = pressure_stats(3 * 1024 * 1024, 11 * 1024 * 1024);
        assert_eq!(
            policy.should_collect(&stats),
            Some(ZGenerationalTrigger::Major),
        );
    }

    #[test]
    fn an_idle_heap_collects_nothing() {
        let policy = ZGenerationalPolicy::default();
        let stats = pressure_stats(1024 * 1024, 1024 * 1024);
        assert_eq!(policy.should_collect(&stats), None);
    }

    /// Regression test for the documented `ZgcRealHeap` GC storm: a live set
    /// parked above the static threshold must not be able to fire a collection
    /// on every poll.
    #[test]
    fn rearm_floor_prevents_back_to_back_collection() {
        let policy = ZGenerationalPolicy::default();
        let young_capacity = 4 * 1024 * 1024;
        let live_after = 3 * 1024 * 1024; // 75% — permanently above the 50% trigger

        // Before any collection the trigger is armed.
        let stats = pressure_stats(live_after, 0);
        assert_eq!(
            policy.should_collect(&stats),
            Some(ZGenerationalTrigger::Minor),
        );

        // A minor runs and reclaims nothing: occupancy is still 75%.
        policy.note_minor_complete(live_after, young_capacity);

        // The next poll must NOT fire — this is the whole point.
        assert_eq!(
            policy.should_collect(&stats),
            None,
            "a live set above the threshold re-fired with no fresh allocation: \
             this is the per-allocation GC-storm livelock",
        );
        // Nor may a thousand more polls.
        for _ in 0..1000 {
            assert_eq!(policy.should_collect(&stats), None);
        }

        // The floor is `live + max(headroom/4, 64 KiB)` = 3 MiB + 256 KiB.
        let headroom = young_capacity - live_after;
        let expected_floor = live_after + (headroom / 4).max(Z_REARM_MIN_BYTES);
        assert_eq!(policy.young_rearm_floor(), expected_floor);

        // One byte short of the floor: still silent.
        let almost = pressure_stats(expected_floor - 1, 0);
        assert_eq!(policy.should_collect(&almost), None);

        // At the floor, fresh allocation has re-armed it.
        let armed = pressure_stats(expected_floor, 0);
        assert_eq!(
            policy.should_collect(&armed),
            Some(ZGenerationalTrigger::Minor),
        );

        // The allocation-failure path bypasses the floor by design, so a heap
        // whose headroom is genuinely exhausted can still collect.
        assert_eq!(
            policy.should_collect_ignoring_rearm(&stats),
            Some(ZGenerationalTrigger::Minor),
        );
    }

    #[test]
    fn major_is_scheduled_periodically_but_only_under_pressure() {
        let policy = ZGenerationalPolicy::new(
            Z_YOUNG_OCCUPANCY_MINOR_THRESHOLD,
            Z_OLD_OCCUPANCY_MAJOR_THRESHOLD,
            2, // minors_per_major
            Z_REARM_MIN_BYTES,
            Z_REARM_HEADROOM_DIVISOR,
        );

        let mut under_pressure = pressure_stats(3 * 1024 * 1024, 1024 * 1024);

        under_pressure.minors_since_major = 1;
        assert_eq!(
            policy.should_collect(&under_pressure),
            Some(ZGenerationalTrigger::Minor),
        );

        under_pressure.minors_since_major = 2;
        assert_eq!(
            policy.should_collect(&under_pressure),
            Some(ZGenerationalTrigger::Major),
            "the minor budget is spent, so the next pressure-driven cycle upgrades",
        );

        // And the periodic rule must never be a trigger of its own: an idle
        // heap collects nothing however many minors have elapsed. A standalone
        // "N minors have passed" rule would be a second unfloored latch.
        let mut idle = pressure_stats(1024 * 1024, 1024 * 1024);
        idle.minors_since_major = 500;
        assert_eq!(policy.should_collect(&idle), None);
    }

    // -- major cycle --------------------------------------------------------

    #[test]
    fn major_cycle_uses_one_whole_heap_scope_and_frees_dead_old_pages() {
        let heap = heap_with(ZPromotionPolicy::default());
        for _ in 0..16 {
            heap.allocate_young(OBJ, 8).expect("young allocation");
        }
        for _ in 0..16 {
            heap.allocate_old(OBJ, 8).expect("old allocation");
        }
        let young_ids: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
        let old_ids: FxHashSet<u64> = heap.old().snapshot().iter().map(|p| p.id()).collect();
        assert!(!young_ids.is_empty() && !old_ids.is_empty());

        let marker = CountingMarker::new(Vec::new());
        let report = heap.collect_major(&[], &marker);

        // The major's scope spans BOTH generations — that is the deviation from
        // the simulation's minor-then-old-pass shape.
        assert_eq!(marker.whole_heap_scopes.load(Ordering::Relaxed), 1);
        let visited: FxHashSet<u64> = marker.visited_pages.lock().iter().copied().collect();
        let expected: FxHashSet<u64> = young_ids.union(&old_ids).copied().collect();
        assert_eq!(visited, expected);

        // Nothing was marked, so both generations are reclaimed.
        assert_eq!(heap.young().page_count(), 0);
        assert_eq!(heap.old().page_count(), 0);
        assert_eq!(report.old_pages_examined, old_ids.len());
        assert_eq!(report.old_pages_freed, old_ids.len());
        assert_eq!(report.young.pages_freed, young_ids.len());
        assert_eq!(heap.stats().minors_since_major, 0);
    }

    #[test]
    fn major_cycle_keeps_marked_old_pages() {
        let heap = heap_with(ZPromotionPolicy::default());
        for _ in 0..16 {
            heap.allocate_old(OBJ, 8).expect("old allocation");
        }
        let old_before = heap.old().page_count();
        assert!(old_before > 0);

        let report = heap.collect_major(&[], &MarkAllInScope);
        assert_eq!(heap.old().page_count(), old_before);
        assert_eq!(report.old_pages_freed, 0);
        assert!(report.old_live_bytes > 0);
    }

    // -- scope mechanics ----------------------------------------------------

    #[test]
    fn scope_admits_only_allocated_extents_of_its_own_pages() {
        let heap = heap_with(ZPromotionPolicy::default());
        let a = heap.allocate_young(OBJ, 8).expect("young allocation");
        let old = heap.allocate_old(OBJ, 8).expect("old allocation");

        let young_pages = heap.young().snapshot();
        let scope = ZGenerationScope::from_pages(ZGeneration::Young, 1, &young_pages, false);

        assert!(scope.admits(a.address));
        assert!(
            !scope.admits(old.address),
            "an old address must never be admitted"
        );
        assert!(scope.contains_page(a.page_id));
        assert!(!scope.contains_page(old.page_id));

        // Above the page's allocation extent is untouched reservation, never an
        // object — see `ZPageReal::walk_bounds`.
        let page = heap.allocator().page_for(a.address).expect("page");
        let (_, end) = page.walk_bounds();
        assert!(!scope.admits(end));
        assert!(!scope.admits(page.end() - 1));

        assert_eq!(scope.admitted(), 1);
        assert!(scope.rejected() >= 3);
        assert!(!scope.covers_whole_heap());
        assert_eq!(scope.generation(), ZGeneration::Young);
    }

    #[test]
    fn an_empty_scope_admits_nothing() {
        let scope = ZGenerationScope::from_pages(ZGeneration::Young, 7, &[], false);
        assert_eq!(scope.page_count(), 0);
        assert!(!scope.admits(0));
        assert!(!scope.admits(usize::MAX));
        assert_eq!(scope.admitted(), 0);
        assert_eq!(scope.rejected(), 2);
        assert_eq!(scope.cycle(), 7);
    }

    // -- policy arithmetic --------------------------------------------------

    #[test]
    fn promotion_policy_matches_the_simulations_rule() {
        let policy = ZPromotionPolicy::with_age(3);
        // The simulation's predicate was `age + 1 >= promotion_age`, i.e. the
        // age AFTER this cycle's increment.
        assert!(!policy.should_promote(1));
        assert!(!policy.should_promote(2));
        assert!(policy.should_promote(3));
        assert!(policy.should_promote(4));
        // The survival-pressure rule is off by default.
        assert!(!policy.promote_all_survivors(10, 10));
    }

    #[test]
    fn survival_pressure_rule_fires_only_when_configured() {
        let policy = ZPromotionPolicy {
            promotion_age: 100,
            promote_all_on_survival_above: Some(0.75),
        };
        assert!(policy.promote_all_survivors(9, 10));
        assert!(!policy.promote_all_survivors(7, 10));
        assert!(!policy.promote_all_survivors(0, 0));
    }

    #[test]
    fn occupancy_is_zero_for_an_unsized_generation() {
        let stats = ZGenerationalStats::default();
        assert_eq!(stats.young_occupancy(), 0.0);
        assert_eq!(stats.old_occupancy(), 0.0);
    }

    #[test]
    fn generation_split_follows_the_configured_fraction() {
        let alloc = allocator();
        let total = alloc.max_capacity();
        let heap = ZGenerationalHeap::with_defaults(alloc);
        let expected_young = ((total as f64) * Z_DEFAULT_YOUNG_FRACTION) as usize;
        assert_eq!(heap.young().capacity(), expected_young);
        assert_eq!(heap.old().capacity(), total - expected_young);
    }

    #[test]
    fn a_freed_young_page_can_be_recycled_into_young_again() {
        // The allocator's page cache hands a freed page back with the SAME id,
        // so the generation map must be keyed on current ownership. If
        // `invalidate_hot_pages` were skipped, the stale hot-page id would
        // suppress the re-registration and the page would be invisible to the
        // next cycle.
        let heap = heap_with(ZPromotionPolicy::default());
        for _ in 0..16 {
            heap.allocate_young(OBJ, 8).expect("young allocation");
        }
        let first: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
        heap.collect_young(&[], &ZEmptyRememberedSet, &MarkNothing);
        assert_eq!(heap.young().page_count(), 0);

        for _ in 0..16 {
            heap.allocate_young(OBJ, 8).expect("young allocation");
        }
        assert!(heap.young().page_count() >= 1);
        let second: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
        assert!(
            second.intersection(&first).next().is_some(),
            "the allocator should have recycled a cached page; if it did not, this \
             test no longer exercises the id-reuse path",
        );

        // And the recycled page is collectable again.
        let report = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkNothing);
        assert_eq!(report.pages_examined, second.len());
        assert_eq!(heap.young().page_count(), 0);
    }
}
