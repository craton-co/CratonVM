// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real, memory-backed ZGC page management — the three-tier ZPage model over
//! storage the VM actually owns.
//!
//! # Why this module exists
//!
//! The original [`crate::zgc`] page model ([`crate::zgc::ZPage`] /
//! [`crate::zgc::ZgcHeap`]) is a *metadata simulation*: `ZPage::virtual_start`
//! and `physical_start` are synthetic `u64` offsets handed out by an internal
//! counter (`ZgcHeap::add_page`), there is no `*mut u8` behind them, and no
//! Java object byte ever lands in a "page". The functioning collector that
//! replaced it for real work — `ZgcRealHeap` — went the other way and dropped
//! pages entirely: **one** `Mutex<Arena>` for every object plus a flat address
//! registry, swept non-moving over the whole heap. That is correct but it has
//! no regions, no page tiers, and no compaction, so ZGC's actual shape (per-page
//! liveness, a relocation set chosen per page, evacuation into fresh pages) has
//! nowhere to live.
//!
//! This module is the missing middle: **real owned memory, partitioned into
//! typed pages, with O(1) address → page lookup**. It is deliberately built as
//! a sibling of [`crate::region`] (G1's region manager), which is the in-tree
//! precedent for memory-backed, region-partitioned heap storage.
//!
//! # Shape at a glance
//!
//! ```text
//!   ZPageAllocator                        ← owns the reservation + budget
//!     backing: Owned(Vec<u8>)             ← ONE contiguous owned block
//!            | Borrowed{raw,len}          ← ...or a survey over somebody
//!                                            else's (see C0 below)
//!     base ..= base + max_capacity        ← granule-aligned usable window
//!     granules[]                          ← free/used bitmap, 1 bit per granule
//!     ZPageTable                          ← granule index → Arc<ZPageReal>
//!       slots: Vec<Option<Arc<ZPageReal>>>  (O(1) lookup, see below)
//!     free cache: small / medium          ← recycled pages, granules retained
//! ```
//!
//! # What this module does NOT do
//!
//! It is storage and bookkeeping only. There is no marking, no load barrier,
//! no colored pointer, and no relocation *policy* here — a page carries a
//! [`ZPageState`] and a `live_bytes` counter so a later agent can select a
//! relocation set, but choosing and evacuating one is not this file's job.
//!
//! # ADOPTION STATUS (2026-09-20) — read this before trusting the prose above
//!
//! **The collector that runs does not allocate from this module.**
//! `ZgcRealHeap` allocates from one `Mutex<crate::arena::Arena>` and never
//! constructs a [`ZPageAllocator`]. The one thing it takes from here is
//! [`ZPageReal::view`] — an *accounting overlay* over arena bytes, which
//! allocates nothing and frees nothing — so that the page-keyed consumers
//! (`forwarding`'s relocation-set selector, `generation`'s scope,
//! `remembered`'s per-page table) can be driven without the arena being
//! replaced first. `zgc.rs::logical_pages` is the only caller.
//!
//! Everything else in this file — the reservation, the granule bitmap, the page
//! cache, [`ZPageTable`], [`ZPageReal::alloc`]'s CAS bump,
//! [`ZPageReal::reset_shared`]'s mandatory zero-fill, the whole
//! `Free -> Allocating -> Relocatable -> InRelocationSet` lifecycle — is
//! exercised by this file's own tests, by `zgc::tlab`, and by
//! `zgc::generation`, none of which is on the production path either. The
//! buffers a running VM actually bumps into are `zgc::arena_tlab::ZArenaTlab`
//! and the VM thread's `crate::tlab::Tlab` (`zgc::vm_tlab`), both carved from
//! the arena.
//!
//! ## The guard, so the mistake cannot be made by accident (2026-09-20, wave 2)
//!
//! A note at the head of a file is read by whoever reads the file. The way a
//! reader is actually harmed here is by *not* reading it: they find a
//! complete, correct-looking page allocator, are handed an `Arc<ZPageReal>`
//! from somewhere in the crate, and use it as storage. Today the pages in
//! circulation are views, and the three methods that would hurt them are
//! ordinary `pub fn`s.
//!
//! So the view/storage distinction is now a **field and three refusals**
//! rather than three paragraphs:
//!
//! | call on a view | before | now |
//! |---|---|---|
//! | [`ZPageReal::alloc`] | bumps, hands out an address the arena will hand out again | `None`, `debug_assert!` |
//! | [`ZPageReal::reset_shared`] / `reset` | `memset`s live Java objects to zero | refused, logged, `debug_assert!` |
//! | [`ZPageAllocator::free_page`] | releases granules never claimed; double free | refused, logged, `debug_assert!` |
//!
//! [`ZPageReal::is_owned_storage`] is the question to ask, and it is the only
//! thing separating "a page" from "a description of somebody else's bytes".
//!
//! **This is a guard, not an adoption.** Nothing here became live. The point
//! is that the next reader cannot assume it is.
//!
//! This is a design gap, not a defect, and it is not repairable from inside
//! this file: adopting page storage requires the sweep
//! (`arena.add_free_block(base - arena_base, size)`), `ZObjectStarts`'
//! arena-sized bitmap, and the JIT's published heap bounds to move with it.
//! The inventory of what is live and what is dead, and the staged route out,
//! are in `docs/internal/zgc-round-20260920/` —
//! `gap-c-page-rs-is-a-complete-allocator-the-collector-does-not-use.md` and
//! `proposal-c-page-based-heap.md`.
//!
//! ## C0 STAGE 0 — one reservation, two views (2026-09-21, wave 4)
//!
//! The paragraph above says adoption "is not repairable from inside this
//! file". That is true of *adoption* and it was being read as true of
//! *progress*, which it is not. The roadmap
//! (`docs/feature-designs/zgc-roadmap-20260920.md`, phase 1, **C0**) schedules
//! one item first precisely because it is repairable from inside this file:
//! make the page allocator's reservation **be** the arena's backing store, so
//! a page address and an arena offset are the same number plus `base`.
//!
//! [`ZPageAllocator::over_reservation`] is that constructor. The allocator it
//! builds is a **survey**: it indexes and describes a window it does not own
//! and refuses to carve a byte out of it ([`ZPageError::SurveyOnly`]). What
//! that buys, all three of it, is on `over_reservation`'s own doc; the short
//! version is that `sweep.rs`'s `add_free_block(base - arena_base, size)`
//! becomes correct by construction, `logical_pages`' per-cycle grid gets
//! somewhere to live ([`ZPageAllocator::adopt_views`]), and [`ZPageTable`]
//! becomes an O(1) address → page filter over the running heap.
//!
//! **It is still not wired.** The construction site is `zgc.rs`, which this
//! lane does not own; the flag ([`page_survey_enabled`],
//! `CRATONVM_ZGC_PAGE_SURVEY`, **default off**) and the API are here, and
//! `docs/internal/zgc-round-20260920/handoff-s-page-survey-stage-0.md` is the
//! four-line edit on the other side. Until that lands,
//! [`ZPageAllocator::survey_stats`] reads all zeros, and **zero is the answer
//! that says so** — the adoption failure this module has is exactly the
//! failure of a number nobody could read.
//!
//! Stage 1 is `CRATONVM_ZGC_PAGE_CHUNKS`: route `arena_tlab.rs`'s
//! `carve_tlab_chunk_inner` — the single chunk source for both buffer kinds —
//! through `alloc_page(Small)` + `ZPageReal::alloc`. That is the stage where
//! bytes change hands, and it needs a survey to stop being a survey first.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::heap::HEADER_SIZE;

// ---------------------------------------------------------------------------
// Grid / size-class constants
// ---------------------------------------------------------------------------

/// The heap-wide object grid. Every allocation start and every page `top`
/// stays a multiple of this, matching the invariant `Arena::alloc` enforces
/// (`size = (size + 7) & !7`) and that every linear heap walker in the tree
/// assumes when it strides object to object.
pub const ZPAGE_OBJECT_GRID: usize = 8;

/// Smallest allocation a page will hand out: one [`HEADER_SIZE`] object
/// header.
///
/// A Java allocation is always header + body, so no real caller asks for
/// less. Clamping here buys a walker invariant for free: two distinct object
/// bases in a page are never closer together than a header, so a linear walk
/// that reads a header at `p` can always read the *next* header without first
/// proving it did not land inside the previous object.
pub const ZPAGE_MIN_ALLOC: usize = HEADER_SIZE;

/// OpenJDK ZGC's granule size (`ZGranuleSize`, `zGlobals.hpp`): 2 MiB, i.e.
/// `1 << ZGranuleSizeShift` with `ZGranuleSizeShift == 21`.
///
/// The granule is the *unit of virtual/physical bookkeeping* — every page is a
/// whole number of granules, and OpenJDK's own `ZPageTable` is a flat array
/// indexed by `addr >> ZGranuleSizeShift`. We keep both properties because the
/// second one is what makes address → page lookup O(1) (see [`ZPageTable`]).
pub const ZPAGE_DEFAULT_GRANULE: usize = 2 * 1024 * 1024;

/// OpenJDK `ZPageSizeSmall` = `ZGranuleSize` = 2 MiB.
///
/// Small pages are the common case: per-thread allocation buffers are carved
/// out of them and the overwhelming majority of Java objects live here.
pub const ZPAGE_DEFAULT_SMALL: usize = 2 * 1024 * 1024;

/// OpenJDK `ZPageSizeMedium` = 32 MiB (`ZPageSizeMediumShift == 25`).
///
/// Sixteen granules. Medium pages exist so that objects too big to waste a
/// small page on (a few hundred KiB up to a few MiB) do not each burn a whole
/// bespoke mapping.
pub const ZPAGE_DEFAULT_MEDIUM: usize = 32 * 1024 * 1024;

/// OpenJDK `ZObjectSizeLimitSmall` = `ZPageSizeSmall / 8` = 256 KiB.
///
/// The `/ 8` is a fragmentation bound, not a round number: an object at the
/// limit wastes at most 7/8 of a page in the worst case, so a small page is
/// guaranteed to hold at least 8 maximum-sized objects.
pub const ZPAGE_DEFAULT_SMALL_OBJECT_LIMIT: usize = ZPAGE_DEFAULT_SMALL / 8;

/// OpenJDK `ZObjectSizeLimitMedium` = `ZPageSizeMedium / 8` = 4 MiB. Same
/// 1-in-8 fragmentation bound as the small tier.
pub const ZPAGE_DEFAULT_MEDIUM_OBJECT_LIMIT: usize = ZPAGE_DEFAULT_MEDIUM / 8;

/// Default reserved heap budget. Matches [`crate::zgc::ZgcConfig::default`]'s
/// `heap_size` (256 MiB) so switching a caller from the simulation to this
/// manager does not silently change how much memory the VM takes.
pub const ZPAGE_DEFAULT_MAX_CAPACITY: usize = 256 * 1024 * 1024;

/// Bound on the retry loop in [`ZPageAllocator::alloc_object`].
///
/// Each iteration either allocates into the shared page or replaces it, and a
/// replacement can only lose the install race to another thread that just
/// installed a *fresh* page — so under any real contention the second attempt
/// succeeds. The bound exists so a pathological racer can never spin forever;
/// exhausting it reports [`ZPageError::Fragmented`] rather than hanging.
const ALLOC_ATTEMPTS: usize = 8;

// ---------------------------------------------------------------------------
// Size classes
// ---------------------------------------------------------------------------

/// The three ZGC page tiers.
///
/// Routing is by *object size* alone (see [`ZPageConfig::class_for`]); unlike
/// G1 there is no Eden/Survivor/Old typing here, because ZGC's generational
/// split (JEP 439) is a per-page *age*, not a per-page kind. [`ZPageReal::age`]
/// carries that for a later agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZPageSizeClass {
    /// 2 MiB page, objects up to 256 KiB by default.
    Small,
    /// 32 MiB page, objects up to 4 MiB by default.
    Medium,
    /// One page per object, page size rounded up to a whole granule.
    ///
    /// Large pages are **never relocated** in OpenJDK ZGC: relocating a
    /// multi-MiB object buys nothing (the page holds exactly one object, so
    /// there is no fragmentation to recover) and costs a full copy. We keep
    /// that rule — see [`ZPageReal::is_relocation_candidate`].
    Large,
}

impl ZPageSizeClass {
    /// Human-readable tag for tracing.
    pub fn as_str(self) -> &'static str {
        match self {
            ZPageSizeClass::Small => "small",
            ZPageSizeClass::Medium => "medium",
            ZPageSizeClass::Large => "large",
        }
    }
}

// ---------------------------------------------------------------------------
// Page state
// ---------------------------------------------------------------------------

/// Lifecycle state of a page.
///
/// ```text
///     Free ──(hand out)──► Allocating ──(retire / fills up)──► Relocatable
///       ▲                                                          │
///       │                                                   (selected)
///       │                                                          ▼
///       └──────────────(evacuated, reset)────────────── InRelocationSet
/// ```
///
/// Stored as a `u8` in an atomic so the state can be read on the lookup path
/// without taking the allocator lock, and so a relocation-set selector can
/// claim a page with a single CAS ([`ZPageReal::try_transition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ZPageState {
    /// Owns granules but holds nothing; sitting in the allocator's page cache.
    Free = 0,
    /// Accepting bump allocations. Only this state permits
    /// [`ZPageReal::alloc`].
    Allocating = 1,
    /// Closed to allocation, holds live and dead objects, eligible to be
    /// chosen for a relocation set.
    Relocatable = 2,
    /// Chosen for the current relocation set: its survivors are being (or are
    /// about to be) evacuated elsewhere. Must not accept allocation, and must
    /// not be reset until evacuation completes.
    InRelocationSet = 3,
}

impl ZPageState {
    fn from_u8(v: u8) -> ZPageState {
        match v {
            0 => ZPageState::Free,
            1 => ZPageState::Allocating,
            2 => ZPageState::Relocatable,
            3 => ZPageState::InRelocationSet,
            // Unreachable: the field is only ever written from this enum.
            // Fail SAFE (a page nobody will allocate into or evacuate) rather
            // than panicking inside a GC phase.
            _ => ZPageState::Free,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a page or object allocation could not be satisfied.
///
/// Every variant is a *returned* error, never a panic: turning heap exhaustion
/// into `OutOfMemoryError` is the caller's job (the VM must be able to throw a
/// Java-visible OOME, run finalizers, and keep going), and a `panic!` inside
/// the allocator would take the whole process down instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZPageError {
    /// The reserved budget is spent: committing this page would push
    /// `committed` past `max_capacity`.
    OutOfCapacity {
        requested: usize,
        committed: usize,
        max_capacity: usize,
    },
    /// There is budget left, but no *contiguous* run of free granules long
    /// enough. Only reachable for pages spanning more than one granule
    /// (Medium and Large); a caller may respond by relocating to compact the
    /// granule space.
    Fragmented {
        requested_granules: usize,
        free_granules: usize,
    },
    /// A page was obtained but refused the allocation. Indicates a sizing bug
    /// (a Large page smaller than its object, or an alignment demand wider
    /// than the page), not heap pressure.
    AllocationRefused { bytes: usize, page_size: usize },
    /// [`ZPageConfig::validate`] rejected the configuration.
    InvalidConfig(&'static str),
    /// The allocator was built by [`ZPageAllocator::over_reservation`] and
    /// **does not own its bytes** — another allocator (today
    /// `ZgcRealHeap`'s `Arena`) is still handing them out. It may index and
    /// describe that window; it may not carve pages out of it.
    ///
    /// This is a refusal, not heap pressure: retrying, flushing caches or
    /// raising the budget cannot make it succeed. See
    /// [`ZPageAllocator::over_reservation`].
    SurveyOnly,
}

impl std::fmt::Display for ZPageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZPageError::OutOfCapacity {
                requested,
                committed,
                max_capacity,
            } => write!(
                f,
                "zpage: out of capacity (requested {requested} B, committed {committed} B, \
                 max {max_capacity} B)"
            ),
            ZPageError::Fragmented {
                requested_granules,
                free_granules,
            } => write!(
                f,
                "zpage: no contiguous run of {requested_granules} granules \
                 ({free_granules} free but scattered)"
            ),
            ZPageError::AllocationRefused { bytes, page_size } => write!(
                f,
                "zpage: page of {page_size} B refused a {bytes} B allocation"
            ),
            ZPageError::InvalidConfig(why) => write!(f, "zpage: invalid configuration: {why}"),
            ZPageError::SurveyOnly => write!(
                f,
                "zpage: this allocator is a SURVEY over a reservation it does not own \
                 (ZPageAllocator::over_reservation); it may index that window but never \
                 carve pages out of it"
            ),
        }
    }
}

impl std::error::Error for ZPageError {}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Page geometry. Defaults are OpenJDK ZGC's real numbers (see the constants
/// above); every field is tunable so unit tests can drive a heap of a few
/// hundred KiB instead of committing 256 MiB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZPageConfig {
    /// Bookkeeping unit. Must be a power of two; every page size and the
    /// capacity are whole multiples of it. Also the page-table index stride.
    pub granule_size: usize,
    /// Size of a Small page.
    pub small_page_size: usize,
    /// Size of a Medium page.
    pub medium_page_size: usize,
    /// Objects up to and including this size go on a Small page.
    pub small_object_limit: usize,
    /// Objects up to and including this size go on a Medium page; anything
    /// larger gets its own Large page.
    pub medium_object_limit: usize,
    /// Total reserved budget. Refusing to exceed this is what makes heap
    /// exhaustion an error instead of an unbounded `Vec` growth.
    pub max_capacity: usize,
}

impl Default for ZPageConfig {
    fn default() -> Self {
        Self {
            granule_size: ZPAGE_DEFAULT_GRANULE,
            small_page_size: ZPAGE_DEFAULT_SMALL,
            medium_page_size: ZPAGE_DEFAULT_MEDIUM,
            small_object_limit: ZPAGE_DEFAULT_SMALL_OBJECT_LIMIT,
            medium_object_limit: ZPAGE_DEFAULT_MEDIUM_OBJECT_LIMIT,
            max_capacity: ZPAGE_DEFAULT_MAX_CAPACITY,
        }
    }
}

impl ZPageConfig {
    /// The default geometry with a different budget.
    pub fn with_max_capacity(max_capacity: usize) -> Self {
        Self {
            max_capacity,
            ..Self::default()
        }
    }

    /// Route an object of `bytes` to its page tier.
    ///
    /// The comparisons are inclusive (`<=`) on purpose: OpenJDK's
    /// `ZObjectSizeLimitSmall` is the largest size that still *fits* the small
    /// tier, so an object of exactly the limit is Small, and limit + 1 is the
    /// first Medium object.
    pub fn class_for(&self, bytes: usize) -> ZPageSizeClass {
        if bytes <= self.small_object_limit {
            ZPageSizeClass::Small
        } else if bytes <= self.medium_object_limit {
            ZPageSizeClass::Medium
        } else {
            ZPageSizeClass::Large
        }
    }

    /// Page size for `class`. For [`ZPageSizeClass::Large`] the page is the
    /// object rounded up to a whole granule; `object_bytes` is ignored for the
    /// other two.
    ///
    /// # The rounding SATURATES, and that is load bearing
    ///
    /// `object_bytes` reaches here straight from a Java allocation request, and
    /// [`Self::class_for`] routes *everything* above `medium_object_limit` to
    /// Large — its own test asserts `class_for(usize::MAX / 2) == Large`. A
    /// plain `div_ceil(g) * g` on a request that large overflows: it panics in
    /// debug, and in release it **wraps to a small number**, which is the
    /// dangerous half. A wrapped `page_size` sails through
    /// [`ZPageAllocator::alloc_page_locked`]'s budget check, claims a handful of
    /// granules, installs a page whose `size` bears no relation to the object,
    /// and then either refuses the allocation or hands out an address the page
    /// table does not cover. Saturating instead turns the same request into an
    /// honest [`ZPageError::OutOfCapacity`] — no heap on this planet has
    /// `usize::MAX` bytes of budget — which is what a caller can turn into a
    /// Java `OutOfMemoryError`.
    pub fn page_size_for(&self, class: ZPageSizeClass, object_bytes: usize) -> usize {
        match class {
            ZPageSizeClass::Small => self.small_page_size,
            ZPageSizeClass::Medium => self.medium_page_size,
            ZPageSizeClass::Large => {
                let g = self.granule_size;
                // A Large page must hold header + body with room for the
                // worst-case start alignment, hence the MIN_ALLOC floor.
                let want = object_bytes.max(ZPAGE_MIN_ALLOC);
                want.div_ceil(g).saturating_mul(g)
            }
        }
    }

    /// `log2(granule_size)`. Only meaningful after [`Self::validate`] has
    /// confirmed the granule is a power of two.
    pub fn granule_shift(&self) -> u32 {
        self.granule_size.trailing_zeros()
    }

    /// Reject geometries the allocator's invariants cannot hold up under.
    ///
    /// This is a `Result`, not an `assert!`: the configuration can come from a
    /// command-line flag, and a bad flag must produce a diagnosable startup
    /// error rather than a panic in a GC data structure.
    pub fn validate(&self) -> Result<(), ZPageError> {
        if self.granule_size == 0 || !self.granule_size.is_power_of_two() {
            return Err(ZPageError::InvalidConfig(
                "granule_size must be a non-zero power of two",
            ));
        }
        if self.granule_size < ZPAGE_OBJECT_GRID {
            return Err(ZPageError::InvalidConfig(
                "granule_size must be at least the 8-byte object grid",
            ));
        }
        if self.small_page_size == 0 || self.small_page_size % self.granule_size != 0 {
            return Err(ZPageError::InvalidConfig(
                "small_page_size must be a non-zero multiple of granule_size",
            ));
        }
        if self.medium_page_size == 0 || self.medium_page_size % self.granule_size != 0 {
            return Err(ZPageError::InvalidConfig(
                "medium_page_size must be a non-zero multiple of granule_size",
            ));
        }
        if self.small_page_size > self.medium_page_size {
            return Err(ZPageError::InvalidConfig(
                "small_page_size must not exceed medium_page_size",
            ));
        }
        if self.small_object_limit == 0 || self.small_object_limit > self.small_page_size {
            return Err(ZPageError::InvalidConfig(
                "small_object_limit must be non-zero and fit inside a small page",
            ));
        }
        if self.medium_object_limit < self.small_object_limit
            || self.medium_object_limit > self.medium_page_size
        {
            return Err(ZPageError::InvalidConfig(
                "medium_object_limit must be >= small_object_limit and fit inside a medium page",
            ));
        }
        if self.max_capacity < self.granule_size {
            return Err(ZPageError::InvalidConfig(
                "max_capacity must be at least one granule",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ZPageReal — a page over real memory
// ---------------------------------------------------------------------------

/// A ZGC page backed by real, owned bytes.
///
/// # Where the memory comes from
///
/// A page does **not** own an allocation of its own. Exactly as
/// [`crate::region::RegionHeap`] does with its regions, the whole heap is one
/// contiguous owned block held by the [`ZPageAllocator`], and a page is a
/// `[base, base + size)` window into it. Two reasons, both load-bearing:
///
/// 1. **O(1) address → page lookup.** Per-page `Arena`s would scatter page
///    bases across the process heap, and `addr >> granule_shift` would index
///    nothing. One reservation makes the page table a flat array
///    ([`ZPageTable`]).
/// 2. **Send/Sync without `unsafe impl`.** The page stores its window as
///    `usize`, not `*mut u8`, so the struct is `Send + Sync` by ordinary
///    auto-derivation. Compare `ZgcRealHeap`, which needs
///    `unsafe impl Send/Sync` precisely because it holds raw arena pointers.
///
/// The corollary is a lifetime rule callers must respect: **every address a
/// page hands out dangles once its `ZPageAllocator` is dropped.** The
/// allocator never resizes its reservation (there is no `grow`), so addresses
/// are stable for as long as it lives — the hazard `Arena::grow` panics about
/// cannot arise here.
#[derive(Debug)]
pub struct ZPageReal {
    /// Monotonic page identity, stable across [`Self::reset`]. Used for
    /// logging and for keying the allocator's live-page map.
    id: u64,
    /// Tier this page was created for. Never changes: a page is recycled
    /// within its class or released wholesale.
    size_class: ZPageSizeClass,
    /// First byte of the page, an absolute address inside the reservation.
    base: usize,
    /// Page length in bytes; always a whole number of granules.
    size: usize,
    /// Bump cursor as an OFFSET from `base`.
    ///
    /// Atomic, and moved with a compare-exchange loop, so [`Self::alloc`] is
    /// already lock-free — a later agent can point per-thread allocation
    /// buffers straight at it without redesigning this type.
    top: AtomicUsize,
    /// Bytes found live by the most recent mark. Written by the collector,
    /// read by relocation-set selection ([`Self::live_ratio`]). Not maintained
    /// by [`Self::alloc`]: allocated is not live.
    live_bytes: AtomicUsize,
    /// [`ZPageState`] as a `u8`.
    state: AtomicU8,
    /// Generational ZGC (JEP 439) page age. Untouched by this module beyond
    /// being cleared on reset; a later agent owns the aging policy.
    age: AtomicU32,
    /// **This page owns no memory.** Set by [`Self::view`], clear for every
    /// page a [`ZPageAllocator`] hands out.
    ///
    /// # Why a field and not just the doc that was here before
    ///
    /// [`Self::view`]'s doc has always said a view allocates nothing, that
    /// `alloc` on one would hand out bytes the arena believes it still owns,
    /// and that `free_page` on one would be a double free. Every one of those
    /// is a prose contract over a `pub` method, on a type whose *other*
    /// constructor makes them all legal — and `page.rs` reads, from its API
    /// surface alone, like the allocator the collector uses. It is not (see
    /// the ADOPTION STATUS note at the head of this module), and the specific
    /// way a reader is led astray is by finding a complete-looking allocator
    /// and assuming the pages in front of them are its.
    ///
    /// The views in front of them are `ZgcRealHeap`'s accounting overlay over
    /// arena bytes. `alloc` on one returns an address inside the arena's own
    /// free list; `reset_shared` on one `memset`s live Java objects to zero;
    /// `free_page` on one pushes arena memory onto the granule pool. All three
    /// are silent — the first two corrupt the heap and the third double-frees
    /// it — and all three are reachable today through `pub fn`s with nothing
    /// but a doc paragraph in the way.
    ///
    /// So the contract is a flag, and the three methods check it. The cost is
    /// one byte per page and one predictable branch per call; the alternative
    /// is a contract that can only be enforced by the next reader having read
    /// this file first.
    is_view: bool,
}

impl ZPageReal {
    fn new(id: u64, size_class: ZPageSizeClass, base: usize, size: usize) -> Self {
        Self {
            id,
            size_class,
            base,
            size,
            top: AtomicUsize::new(0),
            live_bytes: AtomicUsize::new(0),
            state: AtomicU8::new(ZPageState::Free as u8),
            age: AtomicU32::new(0),
            is_view: false,
        }
    }

    /// A page **view** over memory this struct does not own.
    ///
    /// [`ZPageAllocator`] owns the pages it hands out and is the only thing
    /// that may call [`Self::new`]. This constructor exists for the opposite
    /// arrangement: `ZgcRealHeap` allocates from a single flat `Arena`, and
    /// imposes a logical grid over it so that the page-keyed consumers in this
    /// crate -- `forwarding`'s relocation-set selector, `generation`'s scope,
    /// `remembered`'s per-page table -- have the three things they actually
    /// need (an id, a byte range, and live/used accounting) without the arena
    /// being replaced by the page allocator first.
    ///
    /// The distinction matters and must not blur: a view **allocates nothing**
    /// and frees nothing. `alloc` on one would hand out bytes the arena
    /// believes it still owns, so a caller that builds views must never use
    /// them to allocate; they are an accounting overlay and their lifecycle
    /// state is meaningless. `free_page` on one would be a double free.
    ///
    /// **Those three sentences are now enforced, not merely stated** — see
    /// [`Self::is_owned_storage`]. [`Self::alloc`] returns `None` on a view,
    /// [`Self::reset_shared`] refuses to zero it, and
    /// [`ZPageAllocator::free_page`] declines it; each logs and each
    /// `debug_assert!`s.
    ///
    /// `used` is supplied rather than bumped, because the arena already knows
    /// how far into each grid cell it has allocated.
    pub(crate) fn view(
        id: u64,
        size_class: ZPageSizeClass,
        base: usize,
        size: usize,
        used: usize,
        live_bytes: usize,
    ) -> Self {
        let mut page = Self::new(id, size_class, base, size);
        page.is_view = true;
        page.top.store(used.min(size), Ordering::Release);
        page.live_bytes.store(live_bytes, Ordering::Release);
        // `Allocating` rather than `Free`: a walker that checks state before
        // decoding must not skip a view holding live objects.
        page.state
            .store(ZPageState::Allocating as u8, Ordering::Release);
        page
    }

    /// **Does this page own the bytes it describes?**
    ///
    /// `true` for a page a [`ZPageAllocator`] carved out of its reservation.
    /// `false` for a [`Self::view`] — an accounting overlay over memory
    /// somebody else owns (today: `ZgcRealHeap`'s arena, through
    /// `zgc.rs::logical_pages`, which is the only caller of `view` in the
    /// tree).
    ///
    /// Ask this before treating a page as storage. Every method on this type
    /// that *writes* the page's bytes or returns them to a pool checks it and
    /// refuses; the accessors (`base`, `size`, `used`, `live_bytes`, `age`,
    /// `live_ratio`, `contains`, the state machine) are meaningful on both and
    /// are what a view exists to provide.
    pub fn is_owned_storage(&self) -> bool {
        !self.is_view
    }

    /// Stable page identity.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Tier this page belongs to.
    pub fn size_class(&self) -> ZPageSizeClass {
        self.size_class
    }

    /// First byte of the page.
    pub fn base(&self) -> usize {
        self.base
    }

    /// Page length in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// One past the last byte of the page.
    pub fn end(&self) -> usize {
        self.base + self.size
    }

    /// Bytes handed out so far (the bump cursor as an offset).
    pub fn used(&self) -> usize {
        self.top.load(Ordering::Acquire)
    }

    /// Bytes still available to the bump cursor.
    pub fn remaining(&self) -> usize {
        self.size.saturating_sub(self.used())
    }

    /// Address one past the last allocated byte.
    pub fn top_addr(&self) -> usize {
        self.base + self.used()
    }

    /// **The walk contract.** Returns `[start, end)` — the only byte range in
    /// this page that a heap walker may decode as objects.
    ///
    /// See the module note on fillers under [`ZPageAllocator`]: everything at
    /// or above `end` is untouched reservation, never an object and never a
    /// filler header. A walker that bounds itself here cannot decode padding.
    pub fn walk_bounds(&self) -> (usize, usize) {
        let top = self.used();
        (self.base, self.base + top)
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ZPageState {
        ZPageState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Unconditionally set the lifecycle state.
    pub fn set_state(&self, state: ZPageState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// Atomically move `from` → `to`, returning whether this caller won.
    ///
    /// Relocation-set selection uses this to claim a page exactly once even
    /// when several GC worker threads scan the page list concurrently.
    pub fn try_transition(&self, from: ZPageState, to: ZPageState) -> bool {
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Bytes the last mark found live.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes.load(Ordering::Acquire)
    }

    /// Overwrite the live-byte count (mark phase, once per page per cycle).
    pub fn set_live_bytes(&self, bytes: usize) {
        self.live_bytes.store(bytes, Ordering::Release);
    }

    /// Add to the live-byte count (mark phase, per marked object). Atomic so
    /// parallel markers can credit the same page without a lock.
    pub fn add_live_bytes(&self, bytes: usize) {
        self.live_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    /// Generational page age.
    pub fn age(&self) -> u32 {
        self.age.load(Ordering::Acquire)
    }

    /// Set the generational page age.
    pub fn set_age(&self, age: u32) {
        self.age.store(age, Ordering::Release);
    }

    /// Live bytes as a fraction of the page's *used* extent — this module's
    /// **only** occupancy figure, and the one relocation-set selection must
    /// use.
    ///
    /// Denominator is `top`, not `size`: a page that is only 10% filled but
    /// 100% live has nothing to recover, and dividing by `size` would rank it
    /// as prime relocation material. OpenJDK measures the same ratio against
    /// the page's live+garbage extent.
    ///
    /// # Who may use this, and who may not
    ///
    /// * **May:** [`Self::is_relocation_candidate`]; any relocation-set
    ///   selector; and anything constructing a `forwarding::PageCandidate` —
    ///   whose `capacity_bytes` must be filled from
    ///   [`Self::relocation_capacity_bytes`] so that its `live_occupancy()`
    ///   *is* this number. That is not a style preference; see the dated note
    ///   on [`Self::relocation_capacity_bytes`] for what the other denominator
    ///   does to `ZRelocationSet::select`.
    /// * **May not:** memory-footprint accounting. This ratio says nothing
    ///   about how much of the page's *committed span* is idle — that is
    ///   `1 - used()/size()`, a different question with a different benefit
    ///   (committed bytes returned, not garbage reclaimed) and a different
    ///   cost model. Do not reach for `size()` here to express it.
    pub fn live_ratio(&self) -> f64 {
        let top = self.used();
        if top == 0 {
            return 0.0;
        }
        self.live_bytes() as f64 / top as f64
    }

    /// Garbage bytes: allocated minus live. Mirrors
    /// [`crate::region::Region::garbage_bytes`].
    pub fn garbage_bytes(&self) -> usize {
        self.used().saturating_sub(self.live_bytes())
    }

    /// **The relocation-selection denominator.** The one figure a
    /// `forwarding::PageCandidate::capacity_bytes` may be built from, and the
    /// reason `adapters.rs` does not need a choice at the call site.
    ///
    /// It is [`Self::used`] — the bump cursor — so that, for the same page,
    ///
    /// ```text
    ///   candidate.garbage_bytes()  == page.garbage_bytes()
    ///   candidate.live_occupancy() == page.live_ratio()      (used() > 0)
    /// ```
    ///
    /// and `forwarding`'s `max_live_occupancy` gate and this module's
    /// [`Self::is_relocation_candidate`] gate cannot disagree about the same
    /// page. It is a distinct method from `used()` rather than a doc line on
    /// `used()` because the ambiguity it settles is about *meaning*, and a
    /// caller writing `capacity_bytes: page.used()` has no way to show it meant
    /// it.
    ///
    /// # 2026-08-07: the two modules divided by different denominators
    ///
    /// `page.rs` computed occupancy as `live / used()` ([`Self::live_ratio`]),
    /// while `forwarding.rs` computed `PageCandidate::live_occupancy` as
    /// `live / capacity_bytes` with that field documented as *"Total bytes the
    /// page spans"* — i.e. [`Self::size`]. `adapters.rs` (correctly) refused to
    /// pick, shipped both mappings (`page_candidate_by_page_span` and
    /// `page_candidate_by_allocated_extent`) and a test asserting the two
    /// measures diverge. The decision is recorded *here* because this module
    /// owns the ratio.
    ///
    /// **Decision: the allocated extent (`used()`) wins.** Three reasons, in
    /// order of force:
    ///
    /// 1. **The span denominator makes the selector's own arithmetic lie.**
    ///    `ZRelocationSet::select` drops any candidate whose `garbage_bytes()`
    ///    is zero *specifically* to avoid pure-copy-no-reclaim work, and
    ///    `ZRelocationPolicy::max_live_occupancy` is documented as *"copy 1 byte
    ///    to reclaim at least 3"*. Fed `size()`, a half-filled but wholly-live
    ///    page reports `size - live` bytes of "garbage" that is **unallocated
    ///    address space no object ever occupied**: it clears the zero-garbage
    ///    filter, reports a low occupancy, copies its entire live set and
    ///    reclaims nothing. Worse, `select` sorts by `garbage / capacity`
    ///    descending — so the emptier the page, the *higher* it ranks, and the
    ///    least profitable page in the heap is evacuated first.
    /// 2. **It is reachable, on the pages this allocator retires by design.**
    ///    See [`ZPageAllocator::retire_shared_pages`]: at the start of every
    ///    cycle the shared Small and Medium allocation pages go
    ///    `Allocating -> Relocatable` wherever their cursor happens to sit. A
    ///    32 MiB Medium page holding 4 MiB of wholly-live data then reads as
    ///    occupancy 0.125 with 28 MiB of "garbage" under the span measure
    ///    (selected, ranked first, reclaims nothing) and as occupancy 1.0 under
    ///    this one (rejected, correctly). The `refill_shared` rollover inside
    ///    [`ZPageAllocator::alloc_object`] retires a page as soon as *one*
    ///    allocation does not fit, which for a Medium page can be
    ///    `medium_object_limit` bytes short of `size`. This is not an
    ///    unreachable corner.
    /// 3. **Two gates that are meant to agree.** `forwarding` documents
    ///    [`Self::is_relocation_candidate`] and its own occupancy cutoff as two
    ///    independent gates on the same decision. Two gates are only worth
    ///    having if they are measuring the same quantity; with different
    ///    denominators they are two *different* filters wearing one threshold.
    ///
    /// # What this decision deliberately does not buy
    ///
    /// A retired page that is sparsely filled but wholly live keeps its whole
    /// span committed, and no relocation set will ever compact it away. That is
    /// a real cost, and it is **not** an argument for the span denominator: it
    /// is a different objective (footprint / page-count compaction) whose
    /// benefit is committed bytes returned rather than garbage reclaimed, and
    /// it needs its own predicate and its own budget. Expressing it as a
    /// denominator makes every profitability number in `select` wrong in order
    /// to say it. If it is wanted, add a separate criterion over
    /// `used()` against `size()`; do not widen this one.
    ///
    /// # The `used() == 0` case
    ///
    /// A page retired with an empty cursor answers `0` here, and
    /// `ZRelocationSet::select` skips `capacity_bytes == 0` outright. That is
    /// the right outcome: an empty page must be **freed**
    /// ([`ZPageAllocator::free_page`]), not evacuated. Nothing in this module
    /// currently performs that sweep — the gap is recorded, not patched.
    #[inline]
    pub fn relocation_capacity_bytes(&self) -> usize {
        self.used()
    }

    /// May this page be put in a relocation set?
    ///
    /// Large pages are excluded on principle (one object per page — there is
    /// no fragmentation to recover and the copy is enormous), matching
    /// OpenJDK. A page must also be `Relocatable`: `Allocating` pages are
    /// still receiving objects and `InRelocationSet` ones are already claimed.
    pub fn is_relocation_candidate(&self, live_ratio_threshold: f64) -> bool {
        self.size_class != ZPageSizeClass::Large
            && self.state() == ZPageState::Relocatable
            && self.live_ratio() < live_ratio_threshold
    }

    /// Is `addr` inside this page?
    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.size
    }

    /// Bump-allocate `bytes` at `align`, returning the object's base ADDRESS.
    ///
    /// `None` means "this page cannot serve the request" — full, or not in
    /// [`ZPageState::Allocating`]. It is not an error; the caller moves to a
    /// fresh page.
    ///
    /// # Concurrency
    ///
    /// A compare-exchange loop on `top`, so several mutator threads may share
    /// one `Allocating` page with no lock. The loop is bounded in practice by
    /// contention only: every failed CAS means some other thread made
    /// progress. `bytes` is clamped up to [`ZPAGE_MIN_ALLOC`] and the cursor
    /// is left on the [`ZPAGE_OBJECT_GRID`], so the next allocation in this
    /// page starts 8-aligned no matter what the caller asked for — the same
    /// invariant `Arena::alloc` enforces, and the one every linear walker
    /// depends on.
    pub fn alloc(&self, bytes: usize, align: usize) -> Option<usize> {
        // A VIEW OWNS NOTHING. Bumping one hands out an address inside
        // `ZgcRealHeap`'s arena that the arena believes is still its to give,
        // and the two allocators then fill the same span. `view`'s doc has
        // said so since it was written; this is the enforcement.
        if self.is_view {
            debug_assert!(
                !self.is_view,
                "ZPageReal::alloc on a VIEW (page {}, base {:#x}): a view is an \
                 accounting overlay over memory another allocator owns -- bumping it \
                 hands out bytes that allocator will hand out again. See \
                 ZPageReal::view and the ADOPTION STATUS note at the head of page.rs.",
                self.id, self.base,
            );
            return None;
        }
        if self.state() != ZPageState::Allocating {
            return None;
        }
        // Normalise BEFORE asserting: 0 is the documented "use the object
        // grid" spelling, and `0.is_power_of_two()` is false.
        let align = if align == 0 { ZPAGE_OBJECT_GRID } else { align };
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        let bytes = bytes.max(ZPAGE_MIN_ALLOC);

        let mut cur = self.top.load(Ordering::Acquire);
        loop {
            let cur_addr = self.base.checked_add(cur)?;
            let aligned = cur_addr.checked_add(align - 1)? & !(align - 1);
            let padding = aligned - cur_addr;
            // Reserve the aligned FOOTPRINT, not merely an aligned start, and
            // round the cursor back onto the object grid — otherwise a
            // 1..7-byte tail sits between two objects and a linear walk reads
            // it as a phantom header (`Arena::alloc` carries the same note).
            let need = padding.checked_add(bytes)?;
            let need = need.checked_add(ZPAGE_OBJECT_GRID - 1)? & !(ZPAGE_OBJECT_GRID - 1);
            let new_top = cur.checked_add(need)?;
            if new_top > self.size {
                return None;
            }
            match self
                .top
                .compare_exchange_weak(cur, new_top, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(aligned),
                Err(actual) => cur = actual,
            }
        }
    }

    /// Recycle a swept/evacuated page: zero the used extent, rewind the
    /// cursor, drop the liveness data, and return the page to
    /// [`ZPageState::Free`].
    ///
    /// # Why the zero-fill is not optional
    ///
    /// `Arena::reset` documents the same rule at length and refuses to use its
    /// `reset_no_zero` twin for exactly this reason: CratonVM's root scanning
    /// is *conservative*. Any 8-byte-aligned address inside the heap can be
    /// fed to an `is_object_address`-style check and then dereferenced as an
    /// `ObjectHeader`. Leaving a dead object's header bytes in a recycled page
    /// therefore resurrects garbage — the scanner sees a plausible header and
    /// treats the corpse as live. Zeroing is the cost of a conservative
    /// scanner; do not "optimise" it away until root scanning is precise.
    ///
    /// # Safety contract for the caller (not a language-level one)
    ///
    /// The page must not be reachable by any mutator when this runs — call it
    /// at a safepoint, after evacuation has copied out every survivor. The
    /// page stays installed in the [`ZPageTable`] (its address range is
    /// unchanged), so a concurrent conservative probe during the wipe would
    /// see a half-zeroed header.
    pub fn reset(&mut self) {
        self.reset_shared();
    }

    /// [`Self::reset`] through a shared reference — the form needed when the
    /// page is held as an `Arc<ZPageReal>` (which it always is once it has
    /// been published to the page table, so `Arc::get_mut` would fail). All
    /// mutated fields are atomics, so `&self` is sufficient; the same
    /// caller-side safepoint contract applies.
    pub fn reset_shared(&self) {
        // A VIEW OWNS NOTHING, and this method's whole body is a `memset` of
        // `[base, base + top)`. On a view that span is live Java objects in
        // somebody else's arena, and zeroing them is heap corruption with no
        // error and no trace. Refuse, loudly.
        if self.is_view {
            debug_assert!(
                !self.is_view,
                "ZPageReal::reset_shared on a VIEW (page {}, base {:#x}): this would \
                 zero {} bytes of live objects in the arena this view merely describes. \
                 See ZPageReal::view.",
                self.id,
                self.base,
                self.top.load(Ordering::Relaxed),
            );
            tracing::error!(
                target: "cratonvm::gc::guard",
                page = self.id,
                base = format_args!("{:#x}", self.base),
                "zgc page: reset() on a page VIEW was refused -- it would have zeroed \
                 live arena objects. This is a caller bug; see ZPageReal::view.",
            );
            return;
        }
        let top = self.top.swap(0, Ordering::AcqRel);
        if top > 0 {
            // SAFETY: `[base, base + top)` lies inside `[base, base + size)`,
            // which is a window into the allocator's reservation that this
            // page exclusively owns for as long as it is not Free. The
            // allocator outlives every page it created and never resizes the
            // reservation, so the range is mapped and writable. Exclusivity
            // against mutators is the caller's safepoint contract, documented
            // on `reset`.
            unsafe {
                std::ptr::write_bytes(self.base as *mut u8, 0, top);
            }
        }
        self.live_bytes.store(0, Ordering::Release);
        self.age.store(0, Ordering::Release);
        self.set_state(ZPageState::Free);
    }
}

// ---------------------------------------------------------------------------
// ZPageTable — address → page
// ---------------------------------------------------------------------------

/// Address → page lookup over the reserved range.
///
/// # Why a flat array, and what it costs
///
/// This structure is consulted **per conservative-root candidate** (every
/// operand-stack slot and every JIT-frame qword the scanner cannot prove is
/// not a pointer) and **per relocation**, so its complexity is not an academic
/// question. `ZgcRealHeap` shipped a `Vec` linear scan in this exact role and
/// it made root collection O(roots x live) — it read as a *hang* at scale, and
/// the fix was to move to a hash set. This module does not repeat that: lookup
/// is
///
/// > **O(1)** — one bounds check, one shift, one indexed load.
///
/// The array holds one slot per *granule*, and every granule a page covers
/// points at that same page (OpenJDK's `ZPageTable` is built the same way), so
/// a multi-granule page needs no run search at lookup time. Space is one
/// pointer-sized `Option<Arc<_>>` per granule: at the default 2 MiB granule a
/// 256 MiB heap costs 128 slots, and a 4 TiB heap costs 2 M slots (16 MiB) —
/// bounded and proportional, unlike an `FxHashMap` keyed by object address,
/// which would grow with the *object* count and pay a hash per probe.
///
/// An `FxHashMap<granule, Arc<_>>` was the alternative and is also O(1); the
/// flat array wins because the key is already a dense small integer, so the
/// hash is pure overhead and the array has no rehash, no collision chain, and
/// perfect cache behaviour on a sequential root scan.
///
/// # Locking
///
/// The slot vector sits behind a `parking_lot::RwLock`. Lookups take the read
/// side (an uncontended read lock is a single atomic under parking_lot);
/// install/remove take the write side and happen only when a page is created
/// or released. **Lock order is `ZPageAllocator` state mutex → page-table
/// lock, never the reverse**, and [`Self::with_page`]'s closure must not call
/// back into the allocator — holding a read lock across a call that wants the
/// write lock is the classic self-deadlock.
///
/// A later agent that needs a genuinely lock-free reader can swap the slot
/// vector for `Vec<AtomicPtr<ZPageReal>>` without changing this API.
pub struct ZPageTable {
    /// First address covered by slot 0.
    base: usize,
    /// `log2(granule_size)`.
    granule_shift: u32,
    /// Number of granules in the reserved range.
    granule_count: usize,
    /// `slots[i]` is the page covering granule `i`, if any.
    slots: RwLock<Vec<Option<Arc<ZPageReal>>>>,
}

impl ZPageTable {
    fn new(base: usize, granule_count: usize, granule_shift: u32) -> Self {
        let mut slots: Vec<Option<Arc<ZPageReal>>> = Vec::with_capacity(granule_count);
        for _ in 0..granule_count {
            slots.push(None);
        }
        Self {
            base,
            granule_shift,
            granule_count,
            slots: RwLock::new(slots),
        }
    }

    /// First address covered by the table.
    pub fn base(&self) -> usize {
        self.base
    }

    /// One past the last address covered by the table.
    pub fn end(&self) -> usize {
        self.base + (self.granule_count << self.granule_shift)
    }

    /// Number of granule slots.
    pub fn granule_count(&self) -> usize {
        self.granule_count
    }

    /// Granule index for `addr`, or `None` when `addr` is outside the reserved
    /// range. Pure arithmetic — no lock, no allocation.
    #[inline]
    pub fn granule_index(&self, addr: usize) -> Option<usize> {
        if addr < self.base {
            return None;
        }
        let idx = (addr - self.base) >> self.granule_shift;
        if idx < self.granule_count {
            Some(idx)
        } else {
            None
        }
    }

    /// The page containing `addr`, as an owned handle. O(1).
    ///
    /// Prefer [`Self::with_page`] on the conservative-scan hot path: this
    /// clones an `Arc` (an atomic increment) on every hit.
    pub fn lookup(&self, addr: usize) -> Option<Arc<ZPageReal>> {
        let idx = self.granule_index(addr)?;
        let slots = self.slots.read();
        slots[idx].clone()
    }

    /// Run `f` against the page containing `addr` without cloning the `Arc`.
    /// O(1).
    ///
    /// `f` runs while the table read lock is held: it must not allocate a
    /// page, free a page, or otherwise re-enter [`ZPageAllocator`].
    pub fn with_page<R>(&self, addr: usize, f: impl FnOnce(&ZPageReal) -> R) -> Option<R> {
        let idx = self.granule_index(addr)?;
        let slots = self.slots.read();
        slots[idx].as_ref().map(|p| f(&**p))
    }

    /// Does `addr` fall inside a page currently installed in the table? O(1).
    ///
    /// This is the conservative-root filter: a `true` answer means the address
    /// is inside real page storage. It does **not** claim `addr` is an object
    /// base — bound it with [`ZPageReal::walk_bounds`] and the caller's own
    /// header check for that.
    pub fn contains(&self, addr: usize) -> bool {
        self.with_page(addr, |page| page.contains(addr))
            .unwrap_or(false)
    }

    /// Install `page` over the `granules` slots starting at `first_granule`.
    fn install(&self, first_granule: usize, granules: usize, page: &Arc<ZPageReal>) {
        let mut slots = self.slots.write();
        for i in first_granule..first_granule + granules {
            debug_assert!(
                slots[i].is_none(),
                "zpage table slot {i} already occupied — granule bookkeeping is out of sync",
            );
            slots[i] = Some(Arc::clone(page));
        }
    }

    /// Clear `granules` slots starting at `first_granule`.
    fn uninstall(&self, first_granule: usize, granules: usize) {
        let mut slots = self.slots.write();
        for i in first_granule..first_granule + granules {
            slots[i] = None;
        }
    }

    /// Drop every entry and install `pages` in its place, under **one** write
    /// lock. Returns `(pages installed, granule slots filled)`.
    ///
    /// This is the survey path ([`ZPageAllocator::adopt_views`]), not the
    /// allocation path: `install` / `uninstall` are driven by the granule
    /// bitmap and assert that a slot they are about to fill is empty, because
    /// on that path an occupied slot is a bookkeeping bug. Here the caller is
    /// re-describing the *same* window every cycle, so a previously occupied
    /// slot is the normal case and clearing first is the whole operation.
    ///
    /// Out-of-range pages are **skipped rather than clamped**: a page that
    /// does not fit the grid is a caller who built its views against a
    /// different base, and silently indexing it at the nearest slot would
    /// answer `page_for` with the wrong page — the exact class of "plausible
    /// number rather than a failure" this file's guards exist to refuse. The
    /// skip is visible as a shortfall in the returned count.
    fn replace_all(&self, pages: &[Arc<ZPageReal>]) -> (usize, usize) {
        let mut slots = self.slots.write();
        for slot in slots.iter_mut() {
            *slot = None;
        }
        let mut installed = 0usize;
        let mut filled = 0usize;
        for page in pages {
            if page.base() < self.base {
                continue;
            }
            let first = (page.base() - self.base) >> self.granule_shift;
            // `size` is a whole number of granules for every page this module
            // builds, and `view` is handed `Z_LOGICAL_PAGE_BYTES` — but round
            // up rather than assume, so a caller with an odd span occupies the
            // slots it actually covers instead of one too few.
            let granules = page.size().div_ceil(1usize << self.granule_shift);
            if first >= self.granule_count || granules == 0 {
                continue;
            }
            let last = (first + granules).min(self.granule_count);
            for slot in slots[first..last].iter_mut() {
                *slot = Some(Arc::clone(page));
                filled += 1;
            }
            installed += 1;
        }
        (installed, filled)
    }

    /// Every distinct page currently installed, in granule order.
    ///
    /// O(granules), and granules are coarse (one per 2 MiB by default), so
    /// this is a short walk — but it is still a scan, so it belongs in
    /// diagnostics and phase setup, not on the allocation path.
    pub fn pages(&self) -> Vec<Arc<ZPageReal>> {
        let slots = self.slots.read();
        let mut out: Vec<Arc<ZPageReal>> = Vec::new();
        for slot in slots.iter() {
            if let Some(page) = slot {
                let dup = out.last().map(|p| Arc::ptr_eq(p, page)).unwrap_or(false);
                if !dup {
                    out.push(Arc::clone(page));
                }
            }
        }
        out
    }
}

/// Hand-written so a `{:?}` on a heap never dumps one entry per granule (a
/// 4 TiB heap has 2 M of them). Same reason `RegionHeap` writes its own.
impl std::fmt::Debug for ZPageTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let occupied = {
            let slots = self.slots.read();
            slots.iter().filter(|s| s.is_some()).count()
        };
        f.debug_struct("ZPageTable")
            .field("base", &format_args!("{:#x}", self.base))
            .field("granule_count", &self.granule_count)
            .field("occupied_granules", &occupied)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Allocator statistics
// ---------------------------------------------------------------------------

/// A snapshot of the allocator's accounting. Every field is a byte count
/// except the page counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ZPageAllocatorStats {
    /// The configured budget. `committed` never exceeds it.
    pub max_capacity: usize,
    /// Bytes backing pages that currently own granules. Includes pages sitting
    /// in the free cache — they are committed, just idle.
    pub committed: usize,
    /// Sum of every non-`Free` page's bump cursor: bytes actually handed to
    /// object allocations.
    pub used: usize,
    /// `max_capacity - used`: bytes an allocation could still reach, whether
    /// they are committed yet or not.
    pub free: usize,
    /// Committed bytes parked in the free-page cache, reusable without
    /// touching the granule map.
    pub cached: usize,
    /// Sum of every page's `live_bytes` (whatever the last mark recorded).
    pub live: usize,
    /// Granules not owned by any page.
    pub free_granules: usize,
    /// Pages per tier, cache included.
    pub small_pages: usize,
    pub medium_pages: usize,
    pub large_pages: usize,
}

// ---------------------------------------------------------------------------
// Allocator internal state
// ---------------------------------------------------------------------------

/// Everything the allocator mutates, behind one mutex.
///
/// One lock rather than a lock per field on purpose: page creation touches the
/// granule map, the counters, the live-page map and the page table together,
/// and splitting them would make that sequence a lock-ordering problem for no
/// throughput gain (page creation is per-2-MiB, not per-object; the per-object
/// path is [`ZPageReal::alloc`], which takes no lock at all).
#[derive(Debug)]
struct ZPageAllocatorState {
    /// `granule_free[i] == true` when granule `i` is owned by no page.
    granule_free: Vec<bool>,
    /// Rotating start point for the free-run search, so repeated allocations
    /// do not rescan the same used prefix.
    scan_hint: usize,
    /// Every page that currently owns granules, keyed by page id.
    live_pages: FxHashMap<u64, Arc<ZPageReal>>,
    /// Recycled Small pages: `Free`, granules retained, ready to hand back out
    /// with no granule-map work. OpenJDK calls this the page cache.
    cache_small: Vec<Arc<ZPageReal>>,
    /// Recycled Medium pages.
    cache_medium: Vec<Arc<ZPageReal>>,
    /// Page currently taking Small object allocations.
    shared_small: Option<Arc<ZPageReal>>,
    /// Page currently taking Medium object allocations.
    shared_medium: Option<Arc<ZPageReal>>,
    /// Bytes of granules owned by pages.
    committed: usize,
    /// Bytes parked in the two caches.
    cached: usize,
    small_pages: usize,
    medium_pages: usize,
    large_pages: usize,
    next_page_id: u64,
}

impl ZPageAllocatorState {
    fn shared(&self, class: ZPageSizeClass) -> Option<&Arc<ZPageReal>> {
        match class {
            ZPageSizeClass::Small => self.shared_small.as_ref(),
            ZPageSizeClass::Medium => self.shared_medium.as_ref(),
            ZPageSizeClass::Large => None,
        }
    }

    fn set_shared(&mut self, class: ZPageSizeClass, page: Option<Arc<ZPageReal>>) {
        match class {
            ZPageSizeClass::Small => self.shared_small = page,
            ZPageSizeClass::Medium => self.shared_medium = page,
            // A Large page holds exactly one object, so there is nothing to
            // share; the caller allocates a bespoke page every time.
            ZPageSizeClass::Large => {}
        }
    }

    fn cache_mut(&mut self, class: ZPageSizeClass) -> Option<&mut Vec<Arc<ZPageReal>>> {
        match class {
            ZPageSizeClass::Small => Some(&mut self.cache_small),
            ZPageSizeClass::Medium => Some(&mut self.cache_medium),
            // Large pages are bespoke-sized; caching one would only ever serve
            // an object of exactly the same rounded size, so they are released
            // outright instead.
            ZPageSizeClass::Large => None,
        }
    }

    fn bump_class_count(&mut self, class: ZPageSizeClass, delta: isize) {
        let slot = match class {
            ZPageSizeClass::Small => &mut self.small_pages,
            ZPageSizeClass::Medium => &mut self.medium_pages,
            ZPageSizeClass::Large => &mut self.large_pages,
        };
        if delta >= 0 {
            *slot += delta as usize;
        } else {
            *slot = slot.saturating_sub((-delta) as usize);
        }
    }
}

/// `Option<&Arc<T>>` identity comparison — `None == None`, `Some(a) == Some(b)`
/// iff they are the same allocation.
fn ptr_eq_opt(a: Option<&Arc<ZPageReal>>, b: Option<&Arc<ZPageReal>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// ZPageAllocator
// ---------------------------------------------------------------------------

/// Owns the reserved heap, hands out pages per size class, recycles them, and
/// refuses to exceed its budget.
///
/// # Humongous / Large objects, fillers, and how a walker must behave
///
/// G1 handles a humongous object by spanning contiguous *regions* and writing
/// a synthetic `ObjectKind::HumongousFiller` header at the start of every
/// continuation region, so a linear walker sees one dark region instead of
/// decoding zeroed bytes as a chain of phantom headers
/// ([`crate::region::RegionHeap::alloc_humongous`]). That sentinel is load
/// bearing there — and it has a scar: the GC walker was found to leave the
/// filler *unscreened at 24 of 26 call sites*, i.e. most walkers decoded the
/// sentinel as if it were a real object.
///
/// **This module deliberately writes no fillers at all.** The design that
/// makes them unnecessary:
///
/// * A Large object gets its **own page**, sized `ceil(bytes / granule)`. It
///   sits at the page base and is the only object there. Nothing follows it,
///   so there is no continuation region to darken.
/// * The rounding tail — from `top` up to the page end — is padding. It is
///   *never* an object and *never* a filler header; it is untouched
///   reservation, and it is zero.
/// * Therefore every walker must be **page-driven and `top`-bounded**:
///   iterate [`Self::pages`], and inside a page walk exactly
///   [`ZPageReal::walk_bounds`] — `[base, base + top)`. Padding is above
///   `top`, so a conforming walker never sees it and needs no screening rule.
///
/// The rule to remember: *do not add a filler here to make a non-page-driven
/// walker work.* Bound the walker instead. Introducing a second
/// `HumongousFiller` producer would inherit the 24-of-26 screening problem for
/// no gain, because unlike G1 this allocator always knows a page's exact
/// object extent.
///
/// A Medium or Small page is the same story with many objects instead of one:
/// objects are dense from `base` to `top` with only 8-byte-grid alignment
/// padding between them (which [`ZPageReal::alloc`] folds into the preceding
/// allocation's footprint, so it is never a gap a walker can land in).
///
/// # Memory ownership
///
/// The reservation is a single `Vec<u8>` — the same choice
/// [`crate::region::RegionHeap`] makes (`vec![0u8; actual_capacity]`) and
/// `Arena::new` makes. It is allocated once and never resized, so every
/// address handed out is stable for the allocator's lifetime. It is also
/// *committed* eagerly by that `vec!` (zeroed pages), which is why
/// [`ZPageAllocatorStats::committed`] is a bookkeeping figure rather than an
/// OS-level commit: a later agent that swaps the `Vec` for `mmap`/
/// `VirtualAlloc` with lazy commit gets a real number for free, and nothing
/// else in this file changes.
///
/// **Unless it does not own it.** [`Self::over_reservation`] builds an
/// allocator over a window somebody else owns — stage 0 of
/// `proposal-c-page-based-heap.md`, "one reservation, two views". Such an
/// allocator is a **survey**: it indexes, describes and hands back pages over
/// that window and refuses to carve a single byte out of it. See
/// [`Self::over_reservation`] and [`Self::is_survey_only`].
///
/// # Thread safety
///
/// No raw pointers are stored anywhere (page windows are `usize`), so `Send`
/// and `Sync` are derived by the compiler — no `unsafe impl` is required, in
/// contrast with `ZgcRealHeap`, which needs one because it holds `*mut u8`
/// arena pointers.
/// Where a [`ZPageAllocator`]'s bytes come from.
///
/// The variant is the whole of the storage/survey distinction at the allocator
/// level, and it is the allocator-level twin of
/// [`ZPageReal::is_owned_storage`]: a page view describes bytes it does not
/// own, and a survey allocator describes a *window* it does not own. Both
/// refuse to write.
#[derive(Debug)]
enum ZPageBacking {
    /// This allocator owns the bytes. Over-allocated by one granule so `base`
    /// can be granule-aligned. Never resized, and never read through this
    /// handle — it exists to OWN the bytes that every page address points
    /// into, so dropping the allocator is what unmaps the heap.
    Owned(Vec<u8>),
    /// Somebody else owns the bytes — today `ZgcRealHeap`'s
    /// `Mutex<crate::arena::Arena>`, whose `Vec<u8>` this window is a slice
    /// of. Stored as `usize` rather than a pointer or a borrow, exactly as
    /// [`ZPageReal`] stores its window, so the type stays `Send + Sync`
    /// without an `unsafe impl`.
    ///
    /// **Lifetime rule, and it is the caller's to keep:** every address this
    /// allocator reports dangles the moment the owner frees the window. The
    /// only construction in the tree holds the allocator *inside* the heap
    /// that owns the arena, so the two die together.
    Borrowed { raw: usize, len: usize },
}

/// What a survey allocator has been asked to describe — the engagement
/// counters for [`ZPageAllocator::over_reservation`].
///
/// Every one of these reads `0` on an owning allocator, and on a survey
/// allocator that nothing ever called. That is the point: this file's whole
/// history is a module that looks live and is not, so the first thing stage 0
/// ships is the number that says whether it ran.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZPageSurveyStats {
    /// Calls to [`ZPageAllocator::adopt_views`]. One per collection on the
    /// wired path, so `0` means the survey is not wired — never that the heap
    /// had no pages.
    pub grid_refreshes: u64,
    /// Views accepted by the most recent refresh.
    pub views_installed: u64,
    /// Views the most recent refresh **rejected** because they fell outside
    /// the surveyed window. Any non-zero value means the caller built its grid
    /// against a different base and the survey describes the wrong heap; it is
    /// a wiring defect, not a measurement.
    pub views_rejected: u64,
    /// Granule slots filled by the most recent refresh.
    pub granules_indexed: u64,
}

/// `CRATONVM_ZGC_PAGE_SURVEY` — **default OFF.** Should `ZgcRealHeap` build a
/// [`ZPageAllocator::over_reservation`] survey over its arena?
///
/// This is the on switch for stage 0 of `proposal-c-page-based-heap.md`, and
/// it is off by default for one reason only: the survey costs a page-table
/// write per collection, and until `logical_pages` is reading its grid back
/// out that is a cost with no payer. It is **not** off because it is risky —
/// a survey cannot allocate, cannot free and cannot write a byte (see
/// [`ZPageAllocator::over_reservation`]), so the arm changes no behaviour
/// whatsoever. What it changes is whether `page.rs` is *reachable*, which is
/// the thing the adoption gap has needed for two rounds.
///
/// Read once per heap at construction and stored, never per allocation:
/// `flags::runtime_var_os`'s own census note is that an uncached flag read on
/// a repeated path is itself the bug.
///
/// Declared in `types/src/flag_groups.rs` with **`off_word: None`** — the
/// default-OFF shape. `Some("0")` is the default-ON shape and publishes the
/// switch as the opposite of what it is; `flag_groups.rs:1616` records that
/// exact mistake being found and fixed once already, and the round this flag
/// belongs to made it again on `zgc-page-evac`.
pub fn page_survey_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_PAGE_SURVEY")
}

pub struct ZPageAllocator {
    /// Validated geometry.
    config: ZPageConfig,
    /// The reservation, owned or borrowed — see [`ZPageBacking`].
    backing: ZPageBacking,
    /// Granule-aligned first usable address. `[base, base + max_capacity)` is
    /// the heap.
    base: usize,
    /// `config.max_capacity` rounded down to a whole number of granules.
    max_capacity: usize,
    /// `log2(granule_size)`.
    granule_shift: u32,
    /// Number of granules in `[base, base + max_capacity)`.
    granule_count: usize,
    /// Address → page. Its own lock; always taken *after* `state`.
    table: ZPageTable,
    /// Everything mutable.
    state: Mutex<ZPageAllocatorState>,
    /// [`ZPageSurveyStats::grid_refreshes`].
    survey_refreshes: AtomicU64,
    /// [`ZPageSurveyStats::views_installed`], last refresh.
    survey_views: AtomicU64,
    /// [`ZPageSurveyStats::views_rejected`], last refresh.
    survey_rejected: AtomicU64,
    /// [`ZPageSurveyStats::granules_indexed`], last refresh.
    survey_granules: AtomicU64,
}

impl ZPageAllocator {
    /// Reserve a heap and build an empty page manager.
    ///
    /// `config.max_capacity` is rounded *down* to a whole granule (the same
    /// fail-safe direction `Arena::new` takes with its `capacity & !7`), so an
    /// odd figure from a command-line flag shrinks the heap by less than a
    /// granule instead of failing.
    pub fn new(config: ZPageConfig) -> Result<Self, ZPageError> {
        config.validate()?;

        let granule = config.granule_size;
        let granule_shift = config.granule_shift();
        let max_capacity = (config.max_capacity / granule) * granule;
        if max_capacity == 0 {
            return Err(ZPageError::InvalidConfig(
                "max_capacity rounds down to zero granules",
            ));
        }
        let granule_count = max_capacity >> granule_shift;

        // Over-allocate by one granule so the usable window can start on a
        // granule boundary: a `Vec<u8>` is only 1-aligned, and the page table
        // index (`(addr - base) >> granule_shift`) is only meaningful when
        // `base` is granule-aligned.
        let reservation: Vec<u8> = vec![0u8; max_capacity + granule];
        let raw = reservation.as_ptr() as usize;
        let base = (raw + granule - 1) & !(granule - 1);
        debug_assert!(base + max_capacity <= raw + reservation.len());

        let state = ZPageAllocatorState {
            granule_free: vec![true; granule_count],
            scan_hint: 0,
            live_pages: FxHashMap::default(),
            cache_small: Vec::new(),
            cache_medium: Vec::new(),
            shared_small: None,
            shared_medium: None,
            committed: 0,
            cached: 0,
            small_pages: 0,
            medium_pages: 0,
            large_pages: 0,
            next_page_id: 1,
        };

        tracing::debug!(
            target: "zgc",
            "ZPageAllocator: reserved {} B ({} granules of {} B) at base {:#x}; \
             small={} B (objects <= {} B), medium={} B (objects <= {} B)",
            max_capacity,
            granule_count,
            granule,
            base,
            config.small_page_size,
            config.small_object_limit,
            config.medium_page_size,
            config.medium_object_limit,
        );

        Ok(Self {
            config,
            backing: ZPageBacking::Owned(reservation),
            base,
            max_capacity,
            granule_shift,
            granule_count,
            table: ZPageTable::new(base, granule_count, granule_shift),
            state: Mutex::new(state),
            survey_refreshes: AtomicU64::new(0),
            survey_views: AtomicU64::new(0),
            survey_rejected: AtomicU64::new(0),
            survey_granules: AtomicU64::new(0),
        })
    }

    /// Build with the default (OpenJDK) geometry and a 256 MiB budget.
    pub fn with_defaults() -> Result<Self, ZPageError> {
        Self::new(ZPageConfig::default())
    }

    /// **Stage 0 of `proposal-c-page-based-heap.md`: one reservation, two
    /// views.** Build a page manager over `[base, base + len)` — a window this
    /// allocator does **not** own.
    ///
    /// # What this is for
    ///
    /// `ZgcRealHeap` allocates from one `Mutex<crate::arena::Arena>` and has
    /// never constructed a [`ZPageAllocator`]; the only thing it takes from
    /// this module is [`ZPageReal::view`] (see the ADOPTION STATUS note at the
    /// head of this file). The route out, per
    /// `docs/feature-designs/zgc-roadmap-20260920.md` phase 1, item **C0**,
    /// does not start by replacing the arena. It starts by making the page
    /// allocator's reservation **be** the arena's backing store, so that a
    /// page address and an arena offset are the same number plus `base`.
    ///
    /// Three things fall out of that the moment it exists, and they are the
    /// whole of C0's case:
    ///
    /// 1. `sweep.rs`'s `arena.add_free_block(base - arena_base, size)` is
    ///    correct **by construction** for any page address, because there are
    ///    no page addresses outside the arena. That is the blocker
    ///    `gap-c-page-rs-is-a-complete-allocator-…md` names for every later
    ///    stage, and it is removed before stage 1 starts rather than by it.
    /// 2. `zgc.rs::logical_pages` — which re-synthesises a `ZPageReal::view`
    ///    grid on every cycle, one header read per live object — has somewhere
    ///    to put that grid: [`Self::adopt_views`], after which
    ///    [`Self::pages`] is a `Vec` of `Arc` clones.
    /// 3. [`ZPageTable`] becomes available as an O(1) address → page filter in
    ///    front of `ZObjectStarts`, **in addition to** it rather than instead
    ///    of it.
    ///
    /// # What it refuses, and why that is the design and not timidity
    ///
    /// A survey allocator **never allocates**. [`Self::alloc_page`],
    /// [`Self::alloc_object`] and everything routed through them return
    /// [`ZPageError::SurveyOnly`]; [`Self::free_page`] is already refused for
    /// the pages a survey holds, because they are [`ZPageReal::view`]s.
    ///
    /// The reason is exactly the reason `is_view` exists one level down: the
    /// arena is still handing these bytes out. A page carved here would be a
    /// window the arena believes it owns, and the first object allocated into
    /// it would be written over a live Java object — silently, since both
    /// allocators think they are right. Stage 1
    /// (`CRATONVM_ZGC_PAGE_CHUNKS`) is where bytes change hands, and it does
    /// so by routing the **arena's own** chunk source through a page, not by
    /// letting two allocators bump into one window.
    ///
    /// So: stage 0 changes no behaviour, and this constructor cannot make it
    /// change any. That is what makes it the roadmap's only free item.
    ///
    /// # Geometry
    ///
    /// `base` is taken **verbatim** as the grid origin and is not rounded:
    /// the caller's own page grid (`zgc.rs`'s `Z_LOGICAL_PAGE_BYTES`, which is
    /// [`ZPAGE_DEFAULT_GRANULE`]) is indexed from the arena base, and rounding here
    /// would put granule *i* and logical page *i* one slack region apart — a
    /// `page_for` that answers with the neighbouring page, which is worse than
    /// no answer. `len` is rounded **down** to whole granules, the same
    /// fail-safe direction [`Self::new`] takes.
    ///
    /// [`Self::base_is_granule_aligned`] reports whether `base` happened to be
    /// aligned. Nothing in stage 0 needs it — the grid is self-consistent
    /// either way — and stage 1 will, because a real page must start on a
    /// granule. It is recorded rather than enforced so that the stage that
    /// needs it can check it instead of assuming it.
    pub fn over_reservation(
        base: usize,
        len: usize,
        config: ZPageConfig,
    ) -> Result<Self, ZPageError> {
        config.validate()?;

        let granule = config.granule_size;
        let granule_shift = config.granule_shift();
        // The *window* decides the capacity, not `config.max_capacity`: the
        // arena's size is a fact about the heap that is already running, and a
        // budget that disagreed with it would describe granules that are not
        // there.
        let max_capacity = (len / granule) * granule;
        if max_capacity == 0 {
            return Err(ZPageError::InvalidConfig(
                "over_reservation: window rounds down to zero granules",
            ));
        }
        if base.checked_add(max_capacity).is_none() {
            return Err(ZPageError::InvalidConfig(
                "over_reservation: window wraps the address space",
            ));
        }
        let granule_count = max_capacity >> granule_shift;

        let state = ZPageAllocatorState {
            granule_free: vec![false; granule_count],
            scan_hint: 0,
            live_pages: FxHashMap::default(),
            cache_small: Vec::new(),
            cache_medium: Vec::new(),
            shared_small: None,
            shared_medium: None,
            committed: 0,
            cached: 0,
            small_pages: 0,
            medium_pages: 0,
            large_pages: 0,
            next_page_id: 1,
        };

        tracing::debug!(
            target: "zgc",
            "ZPageAllocator: SURVEY over {} B ({} granules of {} B) at base {:#x} \
             (aligned={}); owns nothing, allocates nothing -- proposal-c stage 0",
            max_capacity,
            granule_count,
            granule,
            base,
            base & (granule - 1) == 0,
        );

        Ok(Self {
            config,
            backing: ZPageBacking::Borrowed {
                raw: base,
                len: max_capacity,
            },
            base,
            max_capacity,
            granule_shift,
            granule_count,
            table: ZPageTable::new(base, granule_count, granule_shift),
            state: Mutex::new(state),
            survey_refreshes: AtomicU64::new(0),
            survey_views: AtomicU64::new(0),
            survey_rejected: AtomicU64::new(0),
            survey_granules: AtomicU64::new(0),
        })
    }

    /// Does this allocator own the bytes its pages point into?
    ///
    /// `true` for [`Self::new`] / [`Self::with_defaults`], `false` for
    /// [`Self::over_reservation`]. The allocator-level twin of
    /// [`ZPageReal::is_owned_storage`], and the thing to ask before treating a
    /// `ZPageAllocator` as storage.
    pub fn owns_reservation(&self) -> bool {
        matches!(self.backing, ZPageBacking::Owned(_))
    }

    /// Is this a **survey** — an allocator that describes a window and
    /// refuses to carve pages out of it? Exactly `!`[`Self::owns_reservation`],
    /// spelled the way the refusal reads.
    pub fn is_survey_only(&self) -> bool {
        !self.owns_reservation()
    }

    /// Did [`Self::base`] land on a granule boundary?
    ///
    /// Always `true` for an owning allocator, which aligns its own base.
    /// For a survey it is whatever the owner's allocation happened to be, and
    /// it is **stage 1's** precondition, not stage 0's — see
    /// [`Self::over_reservation`].
    pub fn base_is_granule_aligned(&self) -> bool {
        self.base & (self.config.granule_size - 1) == 0
    }

    /// Replace the survey's page grid with `pages` and return how many were
    /// accepted.
    ///
    /// The caller owns the grid (today: `zgc.rs::logical_pages`, one
    /// [`ZPageReal::view`] per `Z_LOGICAL_PAGE_BYTES` cell); this indexes it,
    /// so that [`Self::page_for`] and [`ZPageTable::contains`] answer in O(1)
    /// and [`Self::pages`] hands the same grid back without rebuilding it.
    ///
    /// # Refusals
    ///
    /// * On an **owning** allocator this is refused outright: its grid is the
    ///   granule bitmap's, and overwriting it with views would make
    ///   `page_for` answer with pages the allocator does not believe it has.
    /// * A page that is not a [`ZPageReal::view`] is refused *individually*
    ///   and counted in [`ZPageSurveyStats::views_rejected`]. A survey may
    ///   only ever hold views, because a non-view page claims to own bytes the
    ///   arena is still handing out.
    /// * A page outside `[base, end)` is refused the same way, by the table's
    ///   own `replace_all`, which skips rather than clamps — indexing a page
    ///   at the nearest slot would answer [`Self::page_for`] with the wrong
    ///   page, which is worse than no answer.
    ///
    /// Returns `0` on an owning allocator and leaves its table untouched.
    pub fn adopt_views(&self, pages: &[Arc<ZPageReal>]) -> usize {
        if self.owns_reservation() {
            debug_assert!(
                self.is_survey_only(),
                "ZPageAllocator::adopt_views on an allocator that OWNS its reservation: \
                 its page grid is the granule bitmap's, and views over the same window \
                 would answer page_for with pages it never allocated.",
            );
            tracing::error!(
                target: "cratonvm::gc::guard",
                base = format_args!("{:#x}", self.base),
                "zgc page allocator: adopt_views refused -- this allocator owns its \
                 reservation, so its page grid is the granule bitmap's. Views belong to \
                 a survey built by ZPageAllocator::over_reservation.",
            );
            return 0;
        }
        let mut accepted: Vec<Arc<ZPageReal>> = Vec::with_capacity(pages.len());
        let mut rejected = 0u64;
        for page in pages {
            if page.is_owned_storage() {
                // A survey that indexed a storage page would be claiming the
                // arena's bytes belong to a page allocator. Refuse the page,
                // keep the rest: a partial grid is a visible shortfall, a
                // wrong grid is not.
                rejected += 1;
                continue;
            }
            accepted.push(Arc::clone(page));
        }
        let (installed, granules) = self.table.replace_all(&accepted);
        // `replace_all` skips anything the grid cannot index; count that as a
        // rejection too, so `views_installed + views_rejected == pages.len()`
        // and a reader can tell "the survey saw everything" from "the survey
        // saw most of it".
        let out_of_range = accepted.len().saturating_sub(installed) as u64;
        self.survey_refreshes.fetch_add(1, Ordering::Relaxed);
        self.survey_views.store(installed as u64, Ordering::Relaxed);
        self.survey_rejected
            .store(rejected + out_of_range, Ordering::Relaxed);
        self.survey_granules
            .store(granules as u64, Ordering::Relaxed);
        installed
    }

    /// Drop every page a survey is indexing. Leaves an owning allocator alone.
    pub fn clear_views(&self) {
        if self.owns_reservation() {
            return;
        }
        let _ = self.table.replace_all(&[]);
        self.survey_views.store(0, Ordering::Relaxed);
        self.survey_granules.store(0, Ordering::Relaxed);
    }

    /// The survey's engagement counters — see [`ZPageSurveyStats`].
    pub fn survey_stats(&self) -> ZPageSurveyStats {
        ZPageSurveyStats {
            grid_refreshes: self.survey_refreshes.load(Ordering::Relaxed),
            views_installed: self.survey_views.load(Ordering::Relaxed),
            views_rejected: self.survey_rejected.load(Ordering::Relaxed),
            granules_indexed: self.survey_granules.load(Ordering::Relaxed),
        }
    }

    /// The geometry in force.
    pub fn config(&self) -> &ZPageConfig {
        &self.config
    }

    /// First address of the heap.
    pub fn base(&self) -> usize {
        self.base
    }

    /// One past the last address of the heap.
    pub fn end(&self) -> usize {
        self.base + self.max_capacity
    }

    /// The budget, rounded to whole granules.
    pub fn max_capacity(&self) -> usize {
        self.max_capacity
    }

    /// Bytes actually held by the backing store — [`Self::max_capacity`] plus
    /// the one granule of alignment slack on an owning allocator, and exactly
    /// the surveyed window on one built by [`Self::over_reservation`] (which
    /// has no slack: it does not choose its own base). Diagnostics only.
    pub fn reserved_bytes(&self) -> usize {
        match &self.backing {
            ZPageBacking::Owned(reservation) => reservation.len(),
            ZPageBacking::Borrowed { len, .. } => *len,
        }
    }

    /// Is `addr` inside the reserved range? Cheap arithmetic; says nothing
    /// about whether a page is mapped there (use [`ZPageTable::contains`]).
    #[inline]
    pub fn in_reserved_range(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.max_capacity
    }

    /// The address → page table. This is the handle a conservative root
    /// scanner and the relocation code should hold.
    pub fn table(&self) -> &ZPageTable {
        &self.table
    }

    /// The page containing `addr`. O(1) — see [`ZPageTable`].
    pub fn page_for(&self, addr: usize) -> Option<Arc<ZPageReal>> {
        self.table.lookup(addr)
    }

    /// Every page that currently owns granules, cache included.
    ///
    /// On a **survey** ([`Self::is_survey_only`]) there are no such pages, and
    /// this returns the view grid [`Self::adopt_views`] was given instead —
    /// which is the point of C0's second bullet: `logical_pages` builds the
    /// grid once and every later consumer reads it from here rather than
    /// rebuilding it.
    pub fn pages(&self) -> Vec<Arc<ZPageReal>> {
        if self.is_survey_only() {
            return self.table.pages();
        }
        let state = self.state.lock();
        state.live_pages.values().map(Arc::clone).collect()
    }

    // -----------------------------------------------------------------------
    // Page allocation
    // -----------------------------------------------------------------------

    /// Obtain a page of `class`, in [`ZPageState::Allocating`].
    ///
    /// `object_bytes` sizes the page for [`ZPageSizeClass::Large`] and is
    /// ignored otherwise.
    pub fn alloc_page(
        &self,
        class: ZPageSizeClass,
        object_bytes: usize,
    ) -> Result<Arc<ZPageReal>, ZPageError> {
        let mut state = self.state.lock();
        self.alloc_page_locked(&mut state, class, object_bytes)
    }

    fn alloc_page_locked(
        &self,
        state: &mut ZPageAllocatorState,
        class: ZPageSizeClass,
        object_bytes: usize,
    ) -> Result<Arc<ZPageReal>, ZPageError> {
        // A SURVEY NEVER CARVES. The window belongs to `ZgcRealHeap`'s arena,
        // which is still serving objects out of it; a page created here would
        // be a range two allocators both believe they own, and the first
        // object written into it would land on a live Java object with no
        // error on either side.
        //
        // The refusal sits in the *locked* helper so all three entry points
        // (`alloc_page`, `alloc_object` and `refill_shared`) are covered once,
        // the same structure `free_page_locked`'s view guard uses. Its
        // granule bitmap is all-false, so this is belt and braces — but a
        // bitmap that says "nothing free" produces `Fragmented`, which reads
        // as heap pressure and invites a retry, and this is not that.
        if self.is_survey_only() {
            debug_assert!(
                self.owns_reservation(),
                "ZPageAllocator::alloc_page on a SURVEY (base {:#x}, {} B): this \
                 allocator does not own its reservation -- the arena is still handing \
                 those bytes out. See ZPageAllocator::over_reservation.",
                self.base,
                self.max_capacity,
            );
            tracing::error!(
                target: "cratonvm::gc::guard",
                base = format_args!("{:#x}", self.base),
                class = class.as_str(),
                object_bytes,
                "zgc page allocator: page allocation refused -- this allocator is a \
                 SURVEY over a reservation it does not own. Carving a page here would \
                 hand out bytes the arena is still allocating from. See \
                 ZPageAllocator::over_reservation.",
            );
            return Err(ZPageError::SurveyOnly);
        }
        // 1) Page cache. A recycled page keeps its granules and its table
        //    entries, so this path touches neither the granule map nor the
        //    page table — the whole point of the cache.
        //
        //    The pop is bound to a local (rather than nested inside an
        //    `if let` over `cache_mut`) so the mutable borrow of `state` ends
        //    before the counters below are touched.
        let cached_page: Option<Arc<ZPageReal>> = match state.cache_mut(class) {
            Some(cache) => cache.pop(),
            None => None,
        };
        if let Some(page) = cached_page {
            let size = page.size();
            state.cached = state.cached.saturating_sub(size);
            page.set_state(ZPageState::Allocating);
            tracing::trace!(
                target: "zgc",
                "ZPageAllocator: reused cached {} page {} ({} B) at {:#x}",
                class.as_str(), page.id(), size, page.base(),
            );
            return Ok(page);
        }

        let page_size = self.config.page_size_for(class, object_bytes);
        let granules = page_size >> self.granule_shift;

        // 2) Budget. Refuse rather than panic — the caller turns this into a
        //    Java OutOfMemoryError.
        //
        //    `checked_add`, not `+`: `page_size` for a Large object is the
        //    request rounded up to a granule and saturates at `usize::MAX` for
        //    an absurd request (see `ZPageConfig::page_size_for`). A plain add
        //    would then wrap past `max_capacity` and report "there is room",
        //    which is the one arithmetic mistake this check exists to make
        //    impossible. An overflow is definitionally over budget.
        let over_budget = |committed: usize| match committed.checked_add(page_size) {
            Some(total) => total > self.max_capacity,
            None => true,
        };
        if over_budget(state.committed) {
            // Before giving up, hand the caches' granules back: cached pages
            // are committed-but-idle, and a Large request cannot use them as
            // pages even though it can use their granules.
            self.flush_caches_locked(state);
        }
        if over_budget(state.committed) {
            return Err(ZPageError::OutOfCapacity {
                requested: page_size,
                committed: state.committed,
                max_capacity: self.max_capacity,
            });
        }

        // 3) Granules. Each borrow of `state` is finished before the next
        //    starts — deliberately sequential rather than nested, so the
        //    immutable scan and the mutable cache flush never overlap.
        let mut first_run = self.find_free_run(state, granules);
        if first_run.is_none() {
            // Budget says yes but no contiguous run: the cache may be holding
            // granules hostage in the wrong shape. Give them back and retry
            // once.
            self.flush_caches_locked(state);
            first_run = self.find_free_run(state, granules);
        }
        let first = match first_run {
            Some(first) => first,
            None => {
                let free = state.granule_free.iter().filter(|&&f| f).count();
                return Err(ZPageError::Fragmented {
                    requested_granules: granules,
                    free_granules: free,
                });
            }
        };

        for i in first..first + granules {
            state.granule_free[i] = false;
        }
        state.scan_hint = first + granules;

        let id = state.next_page_id;
        state.next_page_id += 1;
        let page_base = self.base + (first << self.granule_shift);
        let page = Arc::new(ZPageReal::new(id, class, page_base, page_size));
        page.set_state(ZPageState::Allocating);

        // Lock order: state mutex (held) -> page-table write lock. Never the
        // reverse. Installing before returning means the page is visible to a
        // conservative scanner from the first byte the caller allocates.
        self.table.install(first, granules, &page);

        state.live_pages.insert(id, Arc::clone(&page));
        state.committed += page_size;
        state.bump_class_count(class, 1);

        tracing::trace!(
            target: "zgc",
            "ZPageAllocator: new {} page {} ({} B, {} granules) at {:#x}; committed {} B",
            class.as_str(), id, page_size, granules, page_base, state.committed,
        );
        Ok(page)
    }

    /// First-fit search for `granules` contiguous free granules, starting from
    /// the rotating hint and wrapping once.
    ///
    /// Mirrors [`crate::region::RegionHeap::find_contiguous_free`]. O(granule
    /// count), and granules are *coarse* — one per 2 MiB, so a 256 MiB heap
    /// has 128 of them. That is why a linear scan is acceptable here and was
    /// not acceptable in the page table: this runs once per page (per 2 MiB of
    /// allocation), the table runs once per root candidate.
    fn find_free_run(&self, state: &ZPageAllocatorState, granules: usize) -> Option<usize> {
        if granules == 0 || granules > self.granule_count {
            return None;
        }
        let n = self.granule_count;
        let start = if state.scan_hint >= n {
            0
        } else {
            state.scan_hint
        };

        // Two passes so the wrap-around is explicit and no run is counted
        // across the seam (a run must be contiguous in index space).
        for pass in 0..2 {
            let (lo, hi) = if pass == 0 { (start, n) } else { (0, start) };
            let mut run_start = 0usize;
            let mut run_len = 0usize;
            for i in lo..hi {
                if state.granule_free[i] {
                    if run_len == 0 {
                        run_start = i;
                    }
                    run_len += 1;
                    if run_len >= granules {
                        return Some(run_start);
                    }
                } else {
                    run_len = 0;
                }
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Page release / recycling
    // -----------------------------------------------------------------------

    /// Return a page to the allocator.
    ///
    /// The page is reset (zeroed, cursor rewound — see [`ZPageReal::reset`] for
    /// why the zeroing is mandatory under conservative scanning) and then
    /// either cached for reuse (Small/Medium) or released outright (Large).
    ///
    /// Caller contract: the page must be unreachable by mutators — no object
    /// in it is live, and no shared allocation buffer still points at it.
    pub fn free_page(&self, page: &Arc<ZPageReal>) {
        let mut state = self.state.lock();
        self.free_page_locked(&mut state, page);
    }

    fn free_page_locked(&self, state: &mut ZPageAllocatorState, page: &Arc<ZPageReal>) {
        // A VIEW OWNS NOTHING, so freeing one is a double free: the granules
        // it "returns" were never taken from this allocator's bitmap, and the
        // bytes belong to an arena that is still using them. `ZPageReal::view`
        // has always said so; this is the enforcement, and it sits in the
        // *locked* helper so both entry points (`free_page` and
        // `retire_shared_pages` below) are covered by one check.
        if !page.is_owned_storage() {
            debug_assert!(
                page.is_owned_storage(),
                "ZPageAllocator::free_page on a page VIEW (page {}, base {:#x}): a view \
                 describes memory this allocator never reserved. Freeing it releases \
                 granules that were never claimed and resets bytes another allocator \
                 still owns. See ZPageReal::view.",
                page.id(),
                page.base(),
            );
            tracing::error!(
                target: "cratonvm::gc::guard",
                page = page.id(),
                base = format_args!("{:#x}", page.base()),
                "zgc page allocator: free_page on a page VIEW was refused -- it does not \
                 own its bytes and this allocator never reserved them. See ZPageReal::view.",
            );
            return;
        }
        // Never recycle a page that is still the shared allocation target.
        if ptr_eq_opt(state.shared(page.size_class()), Some(page)) {
            state.set_shared(page.size_class(), None);
        }
        page.reset_shared();

        let class = page.size_class();
        let size = page.size();
        if class == ZPageSizeClass::Large {
            self.release_granules_locked(state, page);
            return;
        }
        // Same borrow dance as `alloc_page_locked`: finish with `cache_mut`
        // before touching the byte counters.
        let cached = match state.cache_mut(class) {
            Some(cache) => {
                cache.push(Arc::clone(page));
                true
            }
            None => false,
        };
        if cached {
            state.cached += size;
        }
    }

    /// Hand a page's granules back: uninstall its table entries, mark its
    /// granules free, drop it from the live map, and un-commit its bytes.
    fn release_granules_locked(&self, state: &mut ZPageAllocatorState, page: &Arc<ZPageReal>) {
        let size = page.size();
        let granules = size >> self.granule_shift;
        let first = (page.base() - self.base) >> self.granule_shift;

        self.table.uninstall(first, granules);
        for i in first..first + granules {
            state.granule_free[i] = true;
        }
        if first < state.scan_hint {
            state.scan_hint = first;
        }
        state.live_pages.remove(&page.id());
        state.committed = state.committed.saturating_sub(size);
        state.bump_class_count(page.size_class(), -1);

        tracing::trace!(
            target: "zgc",
            "ZPageAllocator: released {} page {} ({} B) at {:#x}; committed {} B",
            page.size_class().as_str(), page.id(), size, page.base(), state.committed,
        );
    }

    /// Release every cached page's granules, giving the budget back to pages
    /// of other sizes. Returns the bytes recovered.
    pub fn flush_caches(&self) -> usize {
        let mut state = self.state.lock();
        self.flush_caches_locked(&mut state)
    }

    fn flush_caches_locked(&self, state: &mut ZPageAllocatorState) -> usize {
        let mut drained: Vec<Arc<ZPageReal>> = Vec::new();
        drained.append(&mut state.cache_small);
        drained.append(&mut state.cache_medium);
        let mut freed = 0usize;
        for page in drained.iter() {
            freed += page.size();
            self.release_granules_locked(state, page);
        }
        state.cached = state.cached.saturating_sub(freed);
        if freed > 0 {
            tracing::debug!(
                target: "zgc",
                "ZPageAllocator: flushed page cache, {} B returned to the granule pool",
                freed,
            );
        }
        freed
    }

    /// Close the current shared allocation pages: whatever they hold becomes
    /// [`ZPageState::Relocatable`] and no further object lands in them.
    ///
    /// Call this at the start of a collection so the pages the mutator was
    /// filling become relocation candidates like every other full page.
    ///
    /// # An EMPTY shared page is freed, not retired
    ///
    /// A page whose cursor never moved has nothing to relocate, and
    /// [`ZPageReal::relocation_capacity_bytes`] says so explicitly: its own doc
    /// notes that `ZRelocationSet::select` skips a `capacity_bytes == 0`
    /// candidate outright and that "an empty page must be **freed** …, not
    /// evacuated. Nothing in this module currently performs that sweep — the
    /// gap is recorded, not patched." This is the patch, at the one place that
    /// can see an empty page without a heap walk.
    ///
    /// **Today this is a guard, not a live leak, and the distinction is worth
    /// stating so nobody deletes it as dead.** [`Self::alloc_object`] installs a
    /// shared page only from inside [`Self::refill_shared`], and always serves
    /// the request that forced the refill on the very next loop iteration, so an
    /// installed page currently reaches a collection with at least one object in
    /// it. The moment those two steps can be separated — a TLAB that carves its
    /// chunk out of the shared slot instead of a private page, an install that
    /// loses its retry to a concurrent `free_page`, a caller that pre-warms a
    /// page — an empty shared page becomes ordinary, and a retired one is
    /// `Relocatable` **forever**: no selector will take it (zero capacity, zero
    /// garbage, and `ZRelocationSet::select` skips `capacity_bytes == 0`
    /// outright), and nothing else in this module frees it. Its granules would
    /// be gone for the life of the heap, one more pair stranded per cycle.
    /// Freeing it costs a cache push and a state store.
    pub fn retire_shared_pages(&self) {
        let mut state = self.state.lock();
        for class in [ZPageSizeClass::Small, ZPageSizeClass::Medium] {
            // Bound to a local first: an `if let` keeps its scrutinee's
            // temporaries alive for the whole body, which would hold the
            // borrow of `state` across `set_shared`.
            let current = state.shared(class).map(Arc::clone);
            if let Some(page) = current {
                if page.used() == 0 {
                    // `free_page_locked` clears the shared slot itself before
                    // it resets the page, so there is no `set_shared` here and
                    // no window in which a recycled page is still shared.
                    self.free_page_locked(&mut state, &page);
                } else {
                    page.set_state(ZPageState::Relocatable);
                    state.set_shared(class, None);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Object allocation
    // -----------------------------------------------------------------------

    /// Allocate `bytes` for one object and return its base address.
    ///
    /// Routes by [`ZPageConfig::class_for`]:
    /// * Small/Medium share one `Allocating` page per class; when it is full a
    ///   fresh page replaces it and the old one becomes
    ///   [`ZPageState::Relocatable`].
    /// * Large gets its own page every time.
    ///
    /// The common case takes **no lock** — it is a CAS on the shared page's
    /// cursor. The state mutex is taken only to swap in a new page, i.e. once
    /// per page-worth of allocation.
    pub fn alloc_object(&self, bytes: usize, align: usize) -> Result<usize, ZPageError> {
        let class = self.config.class_for(bytes);
        if class == ZPageSizeClass::Large {
            let page = self.alloc_page(class, bytes)?;
            let addr = page
                .alloc(bytes, align)
                .ok_or(ZPageError::AllocationRefused {
                    bytes,
                    page_size: page.size(),
                })?;
            // A Large page holds exactly one object and is now full. Close it
            // immediately so nothing else can land in the rounding tail —
            // that tail is what the "no filler needed" walk contract depends
            // on staying untouched.
            page.set_state(ZPageState::Relocatable);
            return Ok(addr);
        }

        for _ in 0..ALLOC_ATTEMPTS {
            let current: Option<Arc<ZPageReal>> = {
                let state = self.state.lock();
                state.shared(class).map(Arc::clone)
            };
            if let Some(page) = current.as_ref() {
                if let Some(addr) = page.alloc(bytes, align) {
                    return Ok(addr);
                }
            }
            self.refill_shared(class, current.as_ref())?;
        }

        // Only reachable if this thread lost the install race
        // ALLOC_ATTEMPTS times in a row, which means other threads were
        // making progress the whole time.
        Err(ZPageError::Fragmented {
            requested_granules: self.config.page_size_for(class, bytes) >> self.granule_shift,
            free_granules: 0,
        })
    }

    /// Swap in a fresh `Allocating` page for `class`, but only if the shared
    /// page is still the `failed` one — otherwise another thread already
    /// refilled it and this caller should simply retry.
    ///
    /// Doing the check and the install under a single lock acquisition is what
    /// makes the retry loop race-free: there is no window in which two threads
    /// both install.
    ///
    /// # The fresh page is obtained BEFORE the old one is retired
    ///
    /// The order used to be the other way round — close the old page, clear the
    /// slot, then ask for a new one — and it turned one refusal into two
    /// losses. `alloc_page_locked` fails when the budget is spent or the
    /// granule space is too fragmented for a whole page; with the old page
    /// already `Relocatable` and the slot already `None`, that failure
    /// *abandoned* whatever was left in the old page. The next allocation then
    /// found no shared page at all and asked for a fresh one again, so a heap
    /// that could still have served hundreds of small objects out of the
    /// current page's remainder instead reported `OutOfCapacity` on the first
    /// object that happened not to fit it. That is the "OOM with most of the
    /// heap free" shape this tree has already paid for twice
    /// (`zgc-oom-with-84-percent-of-the-heap-free`,
    /// `zgc-low-end-fragmentation-was-the-starved-tlab-rung`).
    ///
    /// Asking first makes the failure a no-op: on `Err` the shared slot still
    /// holds the old `Allocating` page, and the only thing lost is the one
    /// allocation that did not fit it. Nothing else has to change, because
    /// `alloc_page_locked` never looks at the shared slots — its cache pop
    /// takes `Free` pages and `flush_caches_locked` releases `Free` pages, and
    /// a shared page is `Allocating`.
    fn refill_shared(
        &self,
        class: ZPageSizeClass,
        failed: Option<&Arc<ZPageReal>>,
    ) -> Result<(), ZPageError> {
        let mut state = self.state.lock();
        if !ptr_eq_opt(state.shared(class), failed) {
            return Ok(()); // someone else refilled; the caller retries
        }
        let old = state.shared(class).map(Arc::clone);
        let page = self.alloc_page_locked(&mut state, class, 0)?;
        if let Some(old) = old {
            // A full allocation page is exactly a relocation candidate: it
            // holds live and dead objects and takes no more allocations.
            old.set_state(ZPageState::Relocatable);
        }
        state.set_shared(class, Some(page));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Statistics
    // -----------------------------------------------------------------------

    /// Snapshot the accounting.
    ///
    /// O(live pages) because `used` and `live` are summed from the pages
    /// themselves rather than mirrored into counters — a mirrored counter has
    /// to be updated on the lock-free allocation fast path, which would
    /// reintroduce a shared atomic per object. Page counts are per-2-MiB, so
    /// the sum is short, and this is a diagnostics call.
    ///
    /// **On a survey every field reads zero except `max_capacity` and
    /// `free`**, and that is correct rather than broken: these count the
    /// allocator's *own* storage, and a survey has none. The numbers about a
    /// surveyed window are [`Self::survey_stats`], and the bytes in it belong
    /// to the arena, which has its own accounting.
    pub fn stats(&self) -> ZPageAllocatorStats {
        let state = self.state.lock();
        let mut used = 0usize;
        let mut live = 0usize;
        for page in state.live_pages.values() {
            if page.state() != ZPageState::Free {
                used += page.used();
            }
            live += page.live_bytes();
        }
        let free_granules = state.granule_free.iter().filter(|&&f| f).count();
        ZPageAllocatorStats {
            max_capacity: self.max_capacity,
            committed: state.committed,
            used,
            free: self.max_capacity.saturating_sub(used),
            cached: state.cached,
            live,
            free_granules,
            small_pages: state.small_pages,
            medium_pages: state.medium_pages,
            large_pages: state.large_pages,
        }
    }
}

/// Hand-written for the same reason [`crate::region::RegionHeap`] writes its
/// own: a derived `Debug` would print the entire multi-hundred-megabyte
/// `reservation` byte by byte.
///
/// Takes the state lock (via [`ZPageAllocator::stats`]), so do not format an
/// allocator from inside a `*_locked` helper — `parking_lot::Mutex` is not
/// reentrant.
impl std::fmt::Debug for ZPageAllocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        f.debug_struct("ZPageAllocator")
            .field("base", &format_args!("{:#x}", self.base))
            .field("max_capacity", &self.max_capacity)
            .field("granule_size", &self.config.granule_size)
            .field("owns_reservation", &self.owns_reservation())
            .field("committed", &stats.committed)
            .field("used", &stats.used)
            .field("cached", &stats.cached)
            .field("small", &stats.small_pages)
            .field("medium", &stats.medium_pages)
            .field("large", &stats.large_pages)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature geometry: the same *shape* as OpenJDK's (granule, two fixed
    /// tiers each with a 1-in-8 object limit, bespoke large pages) at 1/512
    /// the scale, so a test heap is 256 KiB instead of 256 MiB.
    fn test_config() -> ZPageConfig {
        ZPageConfig {
            granule_size: 4096,
            small_page_size: 8192,          // 2 granules
            medium_page_size: 65536,        // 16 granules
            small_object_limit: 8192 / 8,   // 1 KiB
            medium_object_limit: 65536 / 8, // 8 KiB
            max_capacity: 4096 * 64,        // 256 KiB
        }
    }

    fn allocator() -> ZPageAllocator {
        ZPageAllocator::new(test_config()).expect("test geometry must validate")
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn page_types_are_send_and_sync_without_unsafe_impls() {
        // Storing page windows as `usize` rather than `*mut u8` is what buys
        // this; `ZgcRealHeap` needs an `unsafe impl` because it does the
        // opposite.
        assert_send_sync::<ZPageReal>();
        assert_send_sync::<ZPageTable>();
        assert_send_sync::<ZPageAllocator>();
    }

    #[test]
    fn default_config_matches_openjdk_size_classes() {
        let c = ZPageConfig::default();
        assert!(c.validate().is_ok());
        assert_eq!(c.granule_size, 2 * 1024 * 1024);
        assert_eq!(c.small_page_size, 2 * 1024 * 1024);
        assert_eq!(c.medium_page_size, 32 * 1024 * 1024);
        assert_eq!(c.small_object_limit, 256 * 1024);
        assert_eq!(c.medium_object_limit, 4 * 1024 * 1024);
    }

    #[test]
    fn size_class_routing_across_the_thresholds() {
        let c = test_config();
        // The limits are inclusive: an object of exactly the limit still fits
        // its tier, limit + 1 is the first object of the next one.
        assert_eq!(c.class_for(1), ZPageSizeClass::Small);
        assert_eq!(c.class_for(c.small_object_limit), ZPageSizeClass::Small);
        assert_eq!(
            c.class_for(c.small_object_limit + 1),
            ZPageSizeClass::Medium
        );
        assert_eq!(c.class_for(c.medium_object_limit), ZPageSizeClass::Medium);
        assert_eq!(
            c.class_for(c.medium_object_limit + 1),
            ZPageSizeClass::Large
        );
        assert_eq!(c.class_for(usize::MAX / 2), ZPageSizeClass::Large);

        // Large page sizes round up to a whole granule.
        assert_eq!(
            c.page_size_for(ZPageSizeClass::Large, c.medium_object_limit + 1),
            (c.medium_object_limit + 1).div_ceil(c.granule_size) * c.granule_size,
        );
        assert_eq!(c.page_size_for(ZPageSizeClass::Large, 4096), 4096);
        assert_eq!(c.page_size_for(ZPageSizeClass::Large, 4097), 8192);
    }

    #[test]
    fn invalid_configs_are_rejected_not_panicked() {
        let mut c = test_config();
        c.granule_size = 3000; // not a power of two
        assert!(matches!(c.validate(), Err(ZPageError::InvalidConfig(_))));

        let mut c = test_config();
        c.small_page_size = 4096 + 8; // not a granule multiple
        assert!(matches!(c.validate(), Err(ZPageError::InvalidConfig(_))));

        let mut c = test_config();
        c.small_object_limit = c.small_page_size * 2; // cannot fit its page
        assert!(matches!(c.validate(), Err(ZPageError::InvalidConfig(_))));
    }

    #[test]
    fn page_alloc_bumps_until_full_then_returns_none() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        assert_eq!(page.size(), 8192);
        assert_eq!(page.used(), 0);
        assert_eq!(page.state(), ZPageState::Allocating);

        let chunk = 64usize;
        let mut last = page.base();
        let mut count = 0usize;
        while let Some(addr) = page.alloc(chunk, 8) {
            assert!(
                page.contains(addr),
                "handed out an address outside the page"
            );
            assert_eq!(addr % 8, 0, "allocations must stay on the object grid");
            if count > 0 {
                assert_eq!(addr - last, chunk, "bump allocation must be dense");
            }
            last = addr;
            count += 1;
            assert!(count <= 8192 / chunk, "page handed out more than it holds");
        }
        assert_eq!(count, 8192 / chunk);
        assert_eq!(page.used(), 8192);
        assert_eq!(page.remaining(), 0);
        // Full is not an error, it is a `None`.
        assert!(page.alloc(8, 8).is_none());
    }

    #[test]
    fn page_alloc_refuses_when_not_allocating() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        assert!(page.alloc(64, 8).is_some());
        page.set_state(ZPageState::Relocatable);
        assert!(
            page.alloc(64, 8).is_none(),
            "a page closed to allocation must refuse, not silently accept",
        );
        assert!(page.try_transition(ZPageState::Relocatable, ZPageState::InRelocationSet));
        assert!(!page.try_transition(ZPageState::Relocatable, ZPageState::InRelocationSet));
        assert_eq!(page.state(), ZPageState::InRelocationSet);
    }

    #[test]
    fn small_allocation_rounds_up_to_a_header() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        let a = page.alloc(1, 8).unwrap();
        let b = page.alloc(1, 8).unwrap();
        assert!(
            b - a >= ZPAGE_MIN_ALLOC,
            "two object bases must never be closer than one header apart",
        );
    }

    #[test]
    fn page_table_lookup_at_boundaries_and_out_of_range() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        let base = page.base();
        let end = page.end();

        // First byte, an interior byte, and the last byte all map to the page.
        for addr in [base, base + 1, base + 4095, base + 4096, end - 1] {
            let found = alloc.page_for(addr).expect("address inside the page");
            assert!(Arc::ptr_eq(&found, &page));
            assert!(alloc.table().contains(addr));
        }

        // One past the end is NOT in this page. It may be inside the reserved
        // range (a later granule), but no page is installed there yet.
        assert!(alloc.page_for(end).is_none());
        assert!(!alloc.table().contains(end));

        // Below the base and past the reservation are both out of range.
        assert!(alloc.table().granule_index(alloc.base() - 1).is_none());
        assert!(alloc.page_for(alloc.base() - 1).is_none());
        assert!(alloc.page_for(alloc.end()).is_none());
        assert!(alloc.page_for(alloc.end() + 4096).is_none());
        assert!(alloc.page_for(0).is_none());
        assert!(!alloc.in_reserved_range(alloc.end()));
    }

    #[test]
    fn every_granule_of_a_multi_granule_page_resolves_to_that_page() {
        let alloc = allocator();
        // A medium page is 16 granules; each one must index back to it, so
        // lookup never has to search a run.
        let page = alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap();
        let granules = page.size() / 4096;
        assert_eq!(granules, 16);
        for g in 0..granules {
            let addr = page.base() + g * 4096 + 17;
            let found = alloc.page_for(addr).expect("granule must map to the page");
            assert!(Arc::ptr_eq(&found, &page));
        }
        assert_eq!(alloc.table().pages().len(), 1);
    }

    #[test]
    fn large_object_gets_its_own_granule_rounded_page_with_no_filler() {
        let alloc = allocator();
        let bytes = alloc.config().medium_object_limit + 1; // 8193 -> 12288
        let addr = alloc.alloc_object(bytes, 8).unwrap();
        let page = alloc.page_for(addr).unwrap();
        assert_eq!(page.size_class(), ZPageSizeClass::Large);
        assert_eq!(page.size(), 12288);
        assert_eq!(addr, page.base(), "the object sits at the page base");

        // The walk bound stops at `top`, so the rounding tail is outside it —
        // a page-driven walker never decodes the padding and therefore needs
        // no HumongousFiller sentinel to screen.
        let (start, end) = page.walk_bounds();
        assert_eq!(start, page.base());
        assert_eq!(end, page.base() + page.used());
        assert!(end < page.end(), "this page has a rounding tail");
        // And that tail is zero, exactly as the reservation left it.
        for off in page.used()..page.size() {
            // SAFETY: the tail is inside the page's own window.
            let b = unsafe { *((page.base() + off) as *const u8) };
            assert_eq!(b, 0, "page padding must stay zero, never a filler header");
        }
    }

    #[test]
    fn freed_small_page_is_recycled_from_the_cache() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        let id = page.id();
        let base = page.base();
        assert!(page.alloc(64, 8).is_some());

        let committed_before = alloc.stats().committed;
        alloc.free_page(&page);
        let after_free = alloc.stats();
        assert_eq!(
            after_free.committed, committed_before,
            "a cached page keeps its granules — committed must not drop",
        );
        assert_eq!(after_free.cached, 8192);
        assert_eq!(page.state(), ZPageState::Free);
        assert_eq!(page.used(), 0, "reset must rewind the cursor");

        let again = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        assert_eq!(again.id(), id, "the cached page must be reused, not remade");
        assert_eq!(again.base(), base);
        assert_eq!(again.state(), ZPageState::Allocating);
        assert_eq!(alloc.stats().cached, 0);
        assert_eq!(
            alloc.stats().small_pages,
            1,
            "recycling must not double-count"
        );
    }

    #[test]
    fn reset_zeroes_the_used_extent() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        let addr = page.alloc(64, 8).unwrap();
        // SAFETY: `addr` is a live allocation inside the page.
        unsafe {
            std::ptr::write_bytes(addr as *mut u8, 0xAB, 64);
            assert_eq!(*(addr as *const u8), 0xAB);
        }
        page.reset_shared();
        // A conservative scanner must not be able to see the corpse's bytes.
        for off in 0..64 {
            // SAFETY: inside the page window.
            let b = unsafe { *((addr + off) as *const u8) };
            assert_eq!(b, 0, "reset must zero the used extent");
        }
    }

    #[test]
    fn freed_large_page_returns_its_granules() {
        let alloc = allocator();
        let bytes = alloc.config().medium_object_limit + 1;
        let page = alloc.alloc_page(ZPageSizeClass::Large, bytes).unwrap();
        let base = page.base();
        let before = alloc.stats();
        assert_eq!(before.large_pages, 1);
        assert_eq!(before.committed, 12288);

        alloc.free_page(&page);
        let after = alloc.stats();
        assert_eq!(after.committed, 0, "a large page is released, never cached");
        assert_eq!(after.cached, 0);
        assert_eq!(after.large_pages, 0);
        assert_eq!(after.free_granules, before.free_granules + 3);
        assert!(
            alloc.page_for(base).is_none(),
            "a released page must be uninstalled from the table",
        );
    }

    #[test]
    fn capacity_exhaustion_returns_an_error_not_a_panic() {
        let alloc = allocator();
        // 256 KiB budget / 64 KiB medium pages = exactly four pages.
        let mut pages = Vec::new();
        for _ in 0..4 {
            pages.push(alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap());
        }
        let err = alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap_err();
        match err {
            ZPageError::OutOfCapacity {
                requested,
                committed,
                max_capacity,
            } => {
                assert_eq!(requested, 65536);
                assert_eq!(committed, 262144);
                assert_eq!(max_capacity, 262144);
            }
            other => panic!("expected OutOfCapacity, got {other:?}"),
        }
        // The error is Display-able for the OOME message path.
        assert!(!format!("{err}").is_empty());

        // Freeing one page makes room again, via the cache.
        let victim = pages.pop().unwrap();
        alloc.free_page(&victim);
        assert!(alloc.alloc_page(ZPageSizeClass::Medium, 0).is_ok());
    }

    #[test]
    fn object_allocation_exhaustion_surfaces_as_an_error() {
        let alloc = allocator();
        // Fill the whole 256 KiB budget with small objects. 32 small pages of
        // 8 KiB, 8 KiB / 1 KiB = 8 objects each => 256 objects.
        let mut n = 0usize;
        loop {
            match alloc.alloc_object(1024, 8) {
                Ok(_) => {
                    n += 1;
                    assert!(n <= 4096, "allocator handed out more than the budget holds");
                }
                Err(e) => {
                    assert!(
                        matches!(e, ZPageError::OutOfCapacity { .. }),
                        "expected OutOfCapacity, got {e:?}",
                    );
                    break;
                }
            }
        }
        assert_eq!(n, 256, "budget / object size must be handed out exactly");
    }

    #[test]
    fn cache_flush_frees_granules_for_a_different_size_class() {
        let alloc = allocator();
        // Spend the whole budget on small pages, then release them all: the
        // cache still owns every granule, so a medium page can only be served
        // if the allocator flushes the cache rather than reporting OOM.
        let mut pages = Vec::new();
        while let Ok(p) = alloc.alloc_page(ZPageSizeClass::Small, 0) {
            pages.push(p);
        }
        assert_eq!(pages.len(), 32);
        for p in &pages {
            alloc.free_page(p);
        }
        assert_eq!(alloc.stats().cached, 262144);
        assert_eq!(alloc.stats().committed, 262144);

        let medium = alloc
            .alloc_page(ZPageSizeClass::Medium, 0)
            .expect("cache flush must recover the granules");
        assert_eq!(medium.size(), 65536);
        assert_eq!(alloc.stats().committed, 65536);
        assert_eq!(alloc.stats().small_pages, 0);
    }

    #[test]
    fn shared_page_rolls_over_and_retires_the_full_one() {
        let alloc = allocator();
        // 8 objects of 1 KiB fill one small page; the 9th must roll onto a
        // fresh page and leave the old one Relocatable.
        let mut first_page = None;
        for i in 0..9 {
            let addr = alloc.alloc_object(1024, 8).unwrap();
            let page = alloc.page_for(addr).unwrap();
            if i == 0 {
                first_page = Some(page);
            } else if i == 8 {
                let first = first_page.as_ref().unwrap();
                assert!(!Arc::ptr_eq(&page, first), "the 9th object must roll over");
                assert_eq!(
                    first.state(),
                    ZPageState::Relocatable,
                    "a filled allocation page becomes a relocation candidate",
                );
            }
        }
        assert_eq!(alloc.stats().small_pages, 2);

        // Retiring closes the current page too.
        alloc.retire_shared_pages();
        for page in alloc.pages() {
            assert_ne!(page.state(), ZPageState::Allocating);
        }
    }

    #[test]
    fn stats_account_for_used_free_cached_and_per_class_pages() {
        let alloc = allocator();
        let empty = alloc.stats();
        assert_eq!(empty.max_capacity, 262144);
        assert_eq!(empty.committed, 0);
        assert_eq!(empty.used, 0);
        assert_eq!(empty.free, 262144);
        assert_eq!(empty.free_granules, 64);
        assert_eq!(
            empty.small_pages + empty.medium_pages + empty.large_pages,
            0
        );

        let small = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        assert!(small.alloc(1024, 8).is_some());
        assert!(small.alloc(512, 8).is_some());
        let medium = alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap();
        assert!(medium.alloc(4096, 8).is_some());
        let large = alloc.alloc_page(ZPageSizeClass::Large, 20000).unwrap();
        assert!(large.alloc(20000, 8).is_some());

        small.set_live_bytes(1024);
        medium.add_live_bytes(2048);

        let s = alloc.stats();
        assert_eq!(s.small_pages, 1);
        assert_eq!(s.medium_pages, 1);
        assert_eq!(s.large_pages, 1);
        // 8192 + 65536 + ceil(20000/4096)*4096 == 8192 + 65536 + 20480
        assert_eq!(s.committed, 8192 + 65536 + 20480);
        assert_eq!(s.used, 1536 + 4096 + 20000);
        assert_eq!(s.free, s.max_capacity - s.used);
        assert_eq!(s.live, 1024 + 2048);
        assert_eq!(s.cached, 0);
        assert_eq!(s.free_granules, 64 - (2 + 16 + 5));

        // live_ratio is against the used extent, not the page size.
        assert!((small.live_ratio() - 1024.0 / 1536.0).abs() < 1e-9);
        assert_eq!(small.garbage_bytes(), 1536 - 1024);
        // Large pages are never relocation candidates.
        large.set_state(ZPageState::Relocatable);
        large.set_live_bytes(0);
        assert!(!large.is_relocation_candidate(0.5));
        small.set_state(ZPageState::Relocatable);
        small.set_live_bytes(1);
        assert!(small.is_relocation_candidate(0.5));
    }

    #[test]
    fn addresses_from_different_pages_never_overlap() {
        let alloc = allocator();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for _ in 0..4 {
            let p = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
            spans.push((p.base(), p.end()));
        }
        for p in alloc.pages() {
            assert!(
                p.base() >= alloc.base() && p.end() <= alloc.end(),
                "every page must live inside the reservation",
            );
            assert_eq!(
                (p.base() - alloc.base()) % alloc.config().granule_size,
                0,
                "every page must start on a granule boundary",
            );
        }
        for i in 0..spans.len() {
            for j in (i + 1)..spans.len() {
                let (a0, a1) = spans[i];
                let (b0, b1) = spans[j];
                assert!(a1 <= b0 || b1 <= a0, "page windows must not overlap");
            }
        }
    }

    // -----------------------------------------------------------------------
    // The relocation-selection denominator (2026-08-07)
    // -----------------------------------------------------------------------

    /// PINS the denominator decision recorded on
    /// [`ZPageReal::relocation_capacity_bytes`]: the allocated extent, not the
    /// page span. A wholly-live but sparsely-filled page must be rejected, and
    /// the span measure — computed here so the divergence is pinned in this
    /// module rather than inferred from another one — must be the measure that
    /// understates its occupancy.
    #[test]
    fn the_relocation_denominator_is_the_used_extent_not_the_page_span() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        // 512 B allocated in an 8 KiB page, every byte of it live: evacuating
        // this page copies 512 B and reclaims nothing.
        assert!(page.alloc(512, 8).is_some());
        page.set_live_bytes(512);
        page.set_state(ZPageState::Relocatable);

        assert_eq!(page.relocation_capacity_bytes(), page.used());
        assert_eq!(page.relocation_capacity_bytes(), 512);
        assert!(page.relocation_capacity_bytes() < page.size());

        // The chosen denominator: 100% occupied, zero garbage, so no threshold
        // short of 1.0 admits it.
        assert_eq!(page.garbage_bytes(), 0);
        // Exact: 512/512. The comment above says "no threshold short of 1.0
        // admits it", which a tolerance quietly contradicts.
        assert_eq!(page.live_ratio().to_bits(), 1.0f64.to_bits());
        assert!(!page.is_relocation_candidate(0.25));
        assert!(!page.is_relocation_candidate(0.99));

        // The rejected denominator: dividing by `size()` reports 7680 B of
        // "garbage" that is untouched reservation, and an occupancy of 1/16 —
        // which clears `ZRelocationPolicy::max_live_occupancy`'s 0.25 cutoff
        // and, because `select` sorts on garbage/capacity descending, ranks
        // this page ABOVE a genuinely half-dead one.
        let span_garbage = page.size() - page.live_bytes();
        let span_ratio = page.live_bytes() as f64 / page.size() as f64;
        assert_eq!(span_garbage, 8192 - 512);
        assert!(span_ratio < 0.25);
        assert!(
            span_ratio < page.live_ratio(),
            "the page-span denominator must be the one that understates occupancy",
        );
    }

    /// PINS that the divergence is **reachable**, and reachable on the ordinary
    /// path rather than in a corner: `retire_shared_pages` is what every cycle
    /// calls, and it makes the mutator's half-filled page `Relocatable` exactly
    /// where the cursor stood.
    #[test]
    fn retiring_the_shared_page_makes_a_partly_filled_page_relocatable() {
        let alloc = allocator();
        let addr = alloc.alloc_object(64, 8).unwrap();
        let page = alloc.page_for(addr).unwrap();
        assert_eq!(page.state(), ZPageState::Allocating);

        alloc.retire_shared_pages();

        assert_eq!(page.state(), ZPageState::Relocatable);
        assert_eq!(page.used(), 64);
        assert!(
            page.used() < page.size(),
            "a retired shared page keeps its cursor wherever the mutator left \
             it — this is what makes used() and size() diverge for a page that \
             a relocation set can actually see",
        );
        assert_eq!(page.relocation_capacity_bytes(), 64);

        // With every allocated byte live, the two denominators disagree about
        // whether to evacuate at all.
        page.set_live_bytes(64);
        assert!(!page.is_relocation_candidate(0.25));
        assert!((page.live_bytes() as f64 / page.size() as f64) < 0.25);
    }

    /// PINS the other half: once a page is full the two denominators converge,
    /// and the extent measure still admits a genuinely mostly-dead page. Without
    /// this, `relocation_capacity_bytes` reading "reject everything" would pass.
    #[test]
    fn a_full_page_converges_and_a_mostly_dead_one_is_still_selected() {
        let alloc = allocator();
        let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        while page.alloc(1024, 8).is_some() {}
        assert_eq!(page.used(), page.size());
        assert_eq!(page.relocation_capacity_bytes(), page.size());

        page.set_live_bytes(512);
        page.set_state(ZPageState::Relocatable);
        // Exact: 512/8192 is 2^-4, representable with no rounding.
        assert_eq!(page.live_ratio().to_bits(), (512.0f64 / 8192.0).to_bits());
        assert_eq!(page.garbage_bytes(), 8192 - 512);
        assert!(
            page.is_relocation_candidate(0.25),
            "the extent denominator must still admit a genuinely mostly-dead page",
        );
    }

    // -- the view guard -----------------------------------------------------
    //
    // `ZPageReal::view` has always documented that a view allocates nothing,
    // frees nothing, and must never be reset. These pin the enforcement, which
    // exists because the ADOPTION STATUS note at the head of this module is
    // only read by someone who already suspects something is wrong: the whole
    // failure mode is a reader finding a complete-looking page allocator and
    // assuming the `Arc<ZPageReal>` in their hand is one of its pages. Today
    // every page in circulation outside this file's own tests is a view over
    // `ZgcRealHeap`'s arena.

    /// A view is not storage, and says so.
    #[test]
    fn a_view_is_not_owned_storage_and_a_real_page_is() {
        let alloc = allocator();
        let page = alloc
            .alloc_page(ZPageSizeClass::Small, 64)
            .expect("a small page");
        assert!(page.is_owned_storage());

        let view = ZPageReal::view(99, ZPageSizeClass::Small, 0x4000, 8192, 512, 128);
        assert!(!view.is_owned_storage());
        // The accounting a view exists to carry is unaffected.
        assert_eq!(view.id(), 99);
        assert_eq!(view.base(), 0x4000);
        assert_eq!(view.used(), 512);
        assert_eq!(view.live_bytes(), 128);
        assert_eq!(view.walk_bounds(), (0x4000, 0x4000 + 512));
    }

    /// Bumping a view would hand out an address the arena will hand out again.
    ///
    /// In a debug build the `debug_assert!` fires first, which is the louder
    /// and preferred outcome; in release the call returns `None` and nothing
    /// is handed out. Both are enforcement, so the test is written twice.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ZPageReal::alloc on a VIEW")]
    fn allocating_from_a_view_is_refused() {
        let view = ZPageReal::view(1, ZPageSizeClass::Small, 0x4000, 8192, 0, 0);
        let _ = view.alloc(64, 8);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn allocating_from_a_view_is_refused() {
        let view = ZPageReal::view(1, ZPageSizeClass::Small, 0x4000, 8192, 0, 0);
        assert!(view.alloc(64, 8).is_none());
        assert_eq!(view.used(), 0, "a refused alloc must not move the cursor");
    }

    /// Resetting a view would `memset` live Java objects in somebody else's
    /// arena. The address here is deliberately bogus: if the guard ever stops
    /// working this test segfaults rather than passing quietly.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ZPageReal::reset_shared on a VIEW")]
    fn resetting_a_view_is_refused() {
        let view = ZPageReal::view(2, ZPageSizeClass::Small, 0x4000, 8192, 4096, 64);
        view.reset_shared();
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn resetting_a_view_is_refused() {
        let view = ZPageReal::view(2, ZPageSizeClass::Small, 0x4000, 8192, 4096, 64);
        view.reset_shared();
        assert_eq!(
            view.used(),
            4096,
            "a refused reset must not rewind the cursor either -- a view's `used` is \
             the arena's bump extent and is not this type's to change",
        );
        assert_eq!(view.state(), ZPageState::Allocating);
    }

    /// Freeing a view releases granules that were never claimed and resets
    /// bytes another allocator still owns.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "free_page on a page VIEW")]
    fn freeing_a_view_is_refused() {
        let alloc = allocator();
        let view = Arc::new(ZPageReal::view(
            3,
            ZPageSizeClass::Small,
            alloc.base(),
            8192,
            4096,
            64,
        ));
        alloc.free_page(&view);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn freeing_a_view_is_refused() {
        let alloc = allocator();
        let before = alloc.stats();
        let view = Arc::new(ZPageReal::view(
            3,
            ZPageSizeClass::Small,
            alloc.base(),
            8192,
            4096,
            64,
        ));
        alloc.free_page(&view);
        let after = alloc.stats();
        assert_eq!(
            before, after,
            "a refused free must leave the allocator's committed/cached accounting \
             untouched -- the granules were never this allocator's to reclaim",
        );
        assert_eq!(view.used(), 4096);
    }

    // -----------------------------------------------------------------------
    // C0 stage 0 — one reservation, two views
    // -----------------------------------------------------------------------

    /// A window that stands in for `ZgcRealHeap`'s arena: real owned bytes,
    /// granule-aligned, that the "arena" (here, the `Vec`) keeps.
    fn surveyed_window(granules: usize) -> (Vec<u8>, usize, usize) {
        let c = test_config();
        let backing = vec![0u8; c.granule_size * (granules + 1)];
        let raw = backing.as_ptr() as usize;
        let base = (raw + c.granule_size - 1) & !(c.granule_size - 1);
        (backing, base, c.granule_size * granules)
    }

    fn survey(granules: usize) -> (Vec<u8>, ZPageAllocator) {
        let (backing, base, len) = surveyed_window(granules);
        let alloc = ZPageAllocator::over_reservation(base, len, test_config())
            .expect("a granule-aligned window must survey");
        (backing, alloc)
    }

    /// The window decides the capacity, not `config.max_capacity`: surveying a
    /// heap that is already running and then disagreeing with it about how big
    /// it is would describe granules that are not there.
    #[test]
    fn a_survey_takes_its_geometry_from_the_window() {
        let (_backing, alloc) = survey(8);
        let c = test_config();
        assert!(alloc.is_survey_only());
        assert!(!alloc.owns_reservation());
        assert!(alloc.base_is_granule_aligned());
        assert_eq!(alloc.max_capacity(), c.granule_size * 8);
        assert_eq!(alloc.reserved_bytes(), c.granule_size * 8);
        assert_eq!(alloc.end(), alloc.base() + c.granule_size * 8);
        assert_eq!(alloc.table().granule_count(), 8);
        // `new` still owns its bytes, and says so.
        assert!(allocator().owns_reservation());
        assert!(!allocator().is_survey_only());
    }

    /// A window shorter than one granule is not a heap. Refused, not clamped
    /// up to one granule that is not there.
    #[test]
    fn a_window_below_one_granule_is_refused() {
        let c = test_config();
        let err = ZPageAllocator::over_reservation(c.granule_size, c.granule_size - 1, c);
        assert!(matches!(err, Err(ZPageError::InvalidConfig(_))));
    }

    /// THE REFUSAL THAT MAKES STAGE 0 FREE. The arena is still handing these
    /// bytes out; a page carved here is a range two allocators both believe
    /// they own, and the first object written into it lands on a live Java
    /// object with no error on either side.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "alloc_page on a SURVEY")]
    fn a_survey_refuses_to_allocate_a_page() {
        let (_backing, alloc) = survey(8);
        let _ = alloc.alloc_page(ZPageSizeClass::Small, 0);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn a_survey_refuses_to_allocate_a_page() {
        let (_backing, alloc) = survey(8);
        assert!(matches!(
            alloc.alloc_page(ZPageSizeClass::Small, 0),
            Err(ZPageError::SurveyOnly),
        ));
        // And `SurveyOnly` rather than `Fragmented`/`OutOfCapacity`: those two
        // read as heap pressure and invite a retry. This one cannot ever
        // succeed.
        assert!(matches!(
            alloc.alloc_object(64, 8),
            Err(ZPageError::SurveyOnly),
        ));
        let stats = alloc.stats();
        assert_eq!(stats.committed, 0);
        assert_eq!(stats.small_pages, 0);
        assert_eq!(
            stats.free_granules, 0,
            "a survey owns no free granule -- every byte in the window belongs to the \
             arena",
        );
    }

    /// The grid `zgc.rs::logical_pages` builds every cycle, handed to the
    /// survey instead of being thrown away: `page_for` then answers in O(1)
    /// and `pages()` hands the same grid back.
    #[test]
    fn a_survey_indexes_the_view_grid_it_is_given() {
        let (_backing, alloc) = survey(8);
        let c = test_config();
        let views: Vec<Arc<ZPageReal>> = (0..8)
            .map(|i| {
                Arc::new(ZPageReal::view(
                    i as u64,
                    ZPageSizeClass::Small,
                    alloc.base() + i * c.granule_size,
                    c.granule_size,
                    c.granule_size / 2,
                    64,
                ))
            })
            .collect();
        assert_eq!(alloc.adopt_views(&views), 8);

        let stats = alloc.survey_stats();
        assert_eq!(stats.grid_refreshes, 1);
        assert_eq!(stats.views_installed, 8);
        assert_eq!(stats.views_rejected, 0);
        assert_eq!(stats.granules_indexed, 8);

        // O(1) address -> page over the running heap, which is C0's third
        // bullet and the one `ZObjectStarts` cannot do in one shift.
        let probe = alloc.base() + 3 * c.granule_size + 17;
        assert_eq!(alloc.page_for(probe).map(|p| p.id()), Some(3));
        assert!(alloc.table().contains(probe));
        assert!(alloc.page_for(alloc.end() + 4096).is_none());
        assert_eq!(alloc.pages().len(), 8);

        // A refresh REPLACES; it does not accumulate. Two cycles of the same
        // heap must not report sixteen pages.
        assert_eq!(alloc.adopt_views(&views), 8);
        assert_eq!(alloc.pages().len(), 8);
        assert_eq!(alloc.survey_stats().grid_refreshes, 2);
        assert_eq!(alloc.survey_stats().views_installed, 8);

        alloc.clear_views();
        assert!(alloc.pages().is_empty());
        assert!(alloc.page_for(probe).is_none());
    }

    /// A page that claims to OWN its bytes has no business in a survey: it
    /// would be asserting that the arena's storage belongs to a page
    /// allocator. Refused per page and counted, so a partial grid is a visible
    /// shortfall rather than a silent wrong answer.
    #[test]
    fn a_survey_refuses_storage_pages_and_out_of_range_views() {
        let (_backing, alloc) = survey(4);
        let c = test_config();
        let owner = allocator();
        let storage = owner
            .alloc_page(ZPageSizeClass::Small, 0)
            .expect("the owning allocator still allocates");
        let inside = Arc::new(ZPageReal::view(
            1,
            ZPageSizeClass::Small,
            alloc.base(),
            c.granule_size,
            0,
            0,
        ));
        let outside = Arc::new(ZPageReal::view(
            2,
            ZPageSizeClass::Small,
            alloc.end() + c.granule_size,
            c.granule_size,
            0,
            0,
        ));
        let installed = alloc.adopt_views(&[Arc::clone(&inside), storage, outside]);
        assert_eq!(installed, 1);
        let stats = alloc.survey_stats();
        assert_eq!(stats.views_installed, 1);
        assert_eq!(
            stats.views_rejected, 2,
            "installed + rejected must account for every page offered, so a reader can \
             tell 'the survey saw everything' from 'the survey saw most of it'",
        );
    }

    /// An owning allocator's grid is the granule bitmap's. Letting views
    /// overwrite it would make `page_for` answer with pages it never
    /// allocated.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "adopt_views on an allocator that OWNS")]
    fn an_owning_allocator_refuses_a_view_grid() {
        let alloc = allocator();
        let view = Arc::new(ZPageReal::view(
            1,
            ZPageSizeClass::Small,
            alloc.base(),
            test_config().granule_size,
            0,
            0,
        ));
        let _ = alloc.adopt_views(&[view]);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn an_owning_allocator_refuses_a_view_grid() {
        let alloc = allocator();
        let page = alloc
            .alloc_page(ZPageSizeClass::Small, 0)
            .expect("the owning allocator still allocates");
        let view = Arc::new(ZPageReal::view(
            1,
            ZPageSizeClass::Small,
            alloc.base(),
            test_config().granule_size,
            0,
            0,
        ));
        assert_eq!(alloc.adopt_views(&[view]), 0);
        assert_eq!(
            alloc.page_for(page.base()).map(|p| p.id()),
            Some(page.id()),
            "a refused adopt must leave the allocator's own page table intact",
        );
    }

    /// The survey is **off by default**, and a default-off flag that reads as
    /// on is the specific mistake this repo has now made twice. `off_word:
    /// None` in `types/src/flag_groups.rs` is the declaration that matches.
    #[test]
    fn the_survey_flag_is_default_off() {
        if std::env::var_os("CRATONVM_ZGC_PAGE_SURVEY").is_none() {
            assert!(!page_survey_enabled());
        }
    }
}
