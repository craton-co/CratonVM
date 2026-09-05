// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Thread-local allocation buffers for the real, page-backed ZGC heap.
//!
//! # Reuse verdict: this is an ADAPTER over [`crate::tlab::Tlab`], not a rewrite
//!
//! The first question this module had to answer was whether the shared TLAB in
//! `gc/src/tlab.rs` could simply be reused. **It can, and it is.** [`ZTlab`]
//! wraps a `crate::tlab::Tlab` by value and inherits, verbatim:
//!
//! * the bump-and-bounds-check fast path ([`crate::tlab::Tlab::alloc`]),
//!   including the "reserve the aligned FOOTPRINT, not merely an aligned start"
//!   rule that keeps a linear walker from landing in inter-object padding;
//! * the tail filler ([`crate::tlab::Tlab::install_tail_filler`]) with **both**
//!   of its cases — the `int[]` filler for a tail >= `HEADER_SIZE` and the
//!   `GAP_FILLER_CLASS_ID` sentinel for the sub-header tail, which exists
//!   because a zeroed sub-header run is byte-identical to a live
//!   `new Object()` and desyncs a linear walk (Bug-D, 2026-06-12);
//! * the adaptive sizer ([`crate::tlab::TlabPressureTracker`]) with its
//!   JIT-blindness fix — the one-way shrink ratchet that dropped compiled
//!   threads to `MIN_TLAB_SIZE` forever because `alloc_count` cannot see the
//!   JIT's inline bump, closed by feeding `cursor - start` in as the byte
//!   high-water mark;
//! * [`crate::tlab::Tlab::reserved_tail`], which is what a cross-thread STW
//!   scan needs from a TLAB whose owner was forcibly stopped (BUG-03);
//! * the `#[repr(C)]` cursor/end layout the JIT's inline `new` fast path reads
//!   at fixed offsets.
//!
//! Every one of those carries a scar from a real defect. Re-deriving them here
//! would mean re-deriving the defects, and the two copies would then have to be
//! fixed twice — which is exactly the failure the in-tree
//! `tlab-and-card-audit.md` was written about. So this file adds only
//! what is genuinely ZGC-specific and cannot live in the shared type:
//!
//! 1. **The refill source.** A generational TLAB is carved from the young
//!    arena via `GenerationalHeap::refill_tlab`. A ZGC TLAB is carved from a
//!    [`ZPageReal`] the thread holds **privately** — see [`ZTlab::chunk`].
//! 2. **The page handle.** Retirement has to hand the page back to the page
//!    lifecycle (`Allocating -> Relocatable`), which has no analogue in the
//!    arena world.
//! 3. **The allocation color.** A freshly allocated object must be born with
//!    [`crate::zgc::vaddr::ZGoodMask::allocation_color`], or the load barrier's
//!    fast path rejects every new object forever.
//! 4. **The registry/accounting hooks.** `ZgcRealHeap`'s slow path is what
//!    inserts into the address registry and bumps the `allocated` counter; a
//!    TLAB bypasses that slow path, so the bookkeeping has to be re-attached
//!    explicitly ([`ZTlabHeapHooks`]).
//! 5. **A registry of live TLABs** so a safepoint can retire all of them
//!    ([`ZTlabRegistry`]).
//!
//! # Why this exists at all
//!
//! `ZgcRealHeap` allocates through a single `Mutex<Arena>` plus a
//! `Mutex<FxHashSet<usize>>` registry insert — **per object, on every thread**.
//! The generational and G1 backends have TLABs; ZGC has none. On the most
//! allocation-heavy workload in the suite (Spring `ApplicationContext` boot and
//! teardown churn) the ZGC-only regression is 46 classes with 35 PASS -> HANG
//! (`zgc-real-fullsuite-regression-RETIRED-20260807.md`).
//!
//! **This module does not claim to be the cause of that.** The leading
//! hypothesis is the missing generational split, and a sibling agent is
//! building the instrumentation that will attribute the cost. Single-mutex
//! allocation is an *independent, additive* suspect on the same workload, and
//! it is the cheaper of the two to remove. This file removes it; it does not
//! diagnose anything.
//!
//! # THE INVARIANT: retire before any page walk
//!
//! [`ZPageReal::alloc`] bumps the page's `top` by the whole TLAB **chunk** at
//! reservation time, but the chunk is filled object-by-object *afterwards*.
//! Between a refill and a retire, `[chunk_cursor, chunk_end)` therefore lies
//! **below** the page's `top` — inside [`ZPageReal::walk_bounds`] — and holds
//! no object headers. It is zero. A zeroed run is not a terminator: a zeroed
//! `HEADER_SIZE` region is byte-identical to a live `new Object()` (class_id 0,
//! shape 0), so a `top`-bounded page walk that meets an un-retired chunk tail
//! does not stop — it decodes a phantom object and strides off the object grid.
//! That is the same desync the generational sweep hit and the reason
//! `Tlab::install_tail_filler` exists.
//!
//! > **Every collection phase that walks page bytes must be preceded by
//! > [`ZTlabRegistry::retire_all`], and `retire_all` returns only after every
//! > registered TLAB has (a) installed a filler covering `[cursor, chunk_end)`,
//! > (b) flushed its pending registry batch, and (c) released its private page
//! > from `Allocating` to `Relocatable`.**
//!
//! Clause (a) is what makes `walk_bounds()` mean what `page.rs` says it means.
//! Clause (b) is what makes the address registry complete at the only instants
//! a collection can observe it (see [`ZTlabHeapHooks`] on batching). Clause (c)
//! matters because a *private* page is not in the allocator's `shared_small` /
//! `shared_medium` slots, so `ZPageAllocator::retire_shared_pages` does not
//! touch it: without (c) a private page stays `Allocating` forever and its
//! garbage is never reclaimable.
//!
//! [`ZTlabRegistry::reserved_tails`] is the tripwire — it must return an empty
//! vector immediately after `retire_all`, and a non-empty one names exactly the
//! TLABs that would have poisoned the walk.
//!
//! # Locking
//!
//! [`ZTlab::alloc`] is a **pure bump with no atomic read-modify-write and no
//! lock**; that is the entire point of the type and it is asserted by
//! `bump_within_a_chunk_never_touches_shared_state`. The only synchronisation
//! left inside it is the `Release` *fence* that `Tlab::alloc_initialized`
//! executes between the object stores and the cursor commit — on x86-64 that
//! compiles to nothing, and it is load-bearing for the STW tail-publication
//! protocol, so it stays.
//!
//! [`ZTlabCell`] wraps the buffer in a `parking_lot::Mutex` so that a safepoint
//! can retire a peer's TLAB without an OS-level suspension handshake. In steady
//! state that lock is uncontended and thread-private: one CAS on a cache line
//! no other core touches, versus `ZgcRealHeap`'s two globally-contended mutexes
//! per object. Removing even that CAS means publishing a raw `*mut ZTlab` and
//! reading it while the peer is stopped — what `ThreadRegistry::tlab_addr` does
//! for the generational TLAB — which requires the vm-crate thread registry and
//! an exclusion protocol this module cannot establish on its own. A caller that
//! owns its `ZTlab` by value (the JIT, eventually) can drive [`ZTlab::alloc`]
//! with no cell and no lock at all.
//!
//! **Lock order: registry mutex -> cell mutex -> `ZPageAllocator` state mutex.**
//! [`ZTlabRegistry::retire_all`] snapshots the `Arc`s under the registry lock
//! and *drops it* before locking any cell, so the reverse edge never exists.
//!
//! # Scoping
//!
//! Instance state only — no `static`, no `OnceLock`. This tree has already paid
//! for process-global GC caches once (parallel-test crashes traced to
//! process-global native caches); a `ZTlabRegistry` is owned by the heap that
//! created it and dies with it.

use std::sync::Arc;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::tlab::{self, Tlab, GAP_FILLER_CLASS_ID, TLAB_FILLER_CLASS_ID};
use crate::zgc::page::{
    ZPageAllocator, ZPageError, ZPageReal, ZPageSizeClass, ZPageState, ZPAGE_OBJECT_GRID,
};
use crate::zgc::vaddr::ZColor;

// ---------------------------------------------------------------------------
// Generation tag
// ---------------------------------------------------------------------------

/// Which generation a TLAB allocates into.
///
/// **Deliberately a local enum.** This is a plain two-variant tag that a later
/// integration can map onto [`crate::zgc::generation::ZGeneration`] in one
/// place; importing that type here would couple the allocator to the
/// collector's API for no gain today. (The original reason given — "the sibling
/// module is not on disk yet" — is stale: `generation.rs` ships `ZGeneration`
/// with a byte-identical `as_str`.)
///
/// # 2026-08-07 — this tag is **not** `ZPageReal::age`, and used to be
///
/// `ZTlab::refill` used to stamp `page.set_age(self.generation.page_age())` on
/// every page it took: `0` for young, `1` for old. That wrote a *generation
/// tag* into the `AtomicU32` that `generation.rs` maintains as a **survival
/// count** — the number of minor cycles a page has lived through, tested
/// against [`crate::zgc::generation::Z_DEFAULT_PROMOTION_AGE`] by
/// `ZPromotionPolicy::should_promote` (`generation.rs:1906-1907`, `:577-579`).
/// `page.rs:461-463` declares the field for exactly that reading and clears it
/// in `reset_shared`; `relocate.rs:2057` and `:2450` read it the same way for
/// their `gen_hint`. One word, two meanings, and neither module knew the other
/// wrote it. An `Old` TLAB handed a page a full minor cycle of survival credit
/// it had never earned, which is one cycle of premature promotion for any page
/// that later entered the young map.
///
/// The two writers never actually met — but by *luck*, not by construction: a
/// young page reaches `ZYoungGeneration`'s map through `alloc_object`'s shared
/// pages, while a TLAB takes private pages through `alloc_page`, and every
/// transfer between the two subsystems passes through `free_page`, which resets
/// the age. The value was never *invisible*, though: `alloc_page` installs the
/// page in the page table and `live_pages` **before it returns**, and
/// `release_page` publishes it as `Relocatable` with the tag still on it, so
/// the corrupt value sat in a globally reachable page for the whole life of the
/// buffer.
///
/// **Resolution.** `page.rs` owns the field and its declared meaning wins;
/// `tlab.rs` no longer writes `age` at all, and `ZTlabGeneration::page_age()`
/// is gone. The generation a page was carved for is now recorded in the TLAB's
/// own `page_generation` field (see `ZTlab::page_generation`), which is where it
/// belonged: a TLAB already knows which generation it allocates into, and the
/// only consumer that ever wanted the answer was the TLAB itself, guarding
/// against carving two generations' chunks out of one page. If a page ever does
/// need to carry its carving generation, that is a **new field on
/// `ZPageReal`**, not a second meaning for this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ZTlabGeneration {
    /// Newly allocated objects. The default, and the only one a mutator uses
    /// today: promotion is a collector-side decision, not an allocator-side
    /// one.
    Young = 0,
    /// Promoted objects. Reserved for the relocation path, which allocates
    /// into old-generation pages while evacuating.
    Old = 1,
}

impl ZTlabGeneration {
    /// Human-readable tag for tracing.
    pub fn as_str(self) -> &'static str {
        match self {
            ZTlabGeneration::Young => "young",
            ZTlabGeneration::Old => "old",
        }
    }
}

impl Default for ZTlabGeneration {
    fn default() -> Self {
        ZTlabGeneration::Young
    }
}

// ---------------------------------------------------------------------------
// Heap hooks
// ---------------------------------------------------------------------------

/// The bookkeeping `ZgcRealHeap` must still perform for objects that never
/// touched its slow path.
///
/// # Why this is a trait and not a `&ZgcRealHeap`
///
/// Two reasons, one practical and one structural. Practically, this module is
/// written alongside seven siblings and cannot depend on the exact shape
/// `ZgcRealHeap` will have when they all land. Structurally, the *relocator*
/// will also want TLABs (evacuation allocates into fresh pages), and its
/// bookkeeping is not the mutator's — a trait lets the same buffer serve both.
///
/// # Batching: `register_allocations` takes a SLICE, and that is the point
///
/// `ZgcRealHeap::registry` is a `Mutex<FxHashSet<usize>>` consulted per
/// conservative-root candidate, and today it is written **once per object**.
/// Keeping that would have handed back most of what the TLAB just won: the
/// arena mutex would be gone and the registry mutex would still be there, on
/// exactly the same path and with exactly the same contention.
///
/// So the registry insert **is batched**, per TLAB chunk. Addresses accumulate
/// in a thread-private `Vec` inside the [`ZTlab`] and are handed over in one
/// call, capped at [`ZTlabConfig::registry_batch`] entries so the buffer is
/// bounded. At a 256 KiB chunk of ~48-byte objects that is one lock
/// acquisition per ~512 objects instead of 512 — three orders of magnitude of
/// mutex traffic removed, and the `FxHashSet` gets a `reserve`-able bulk insert
/// instead of 512 independent rehash-checks.
///
/// **The hazard, stated plainly:** between an object's allocation and the
/// flush, `is_object_address(addr)` answers `false` for it. If a collection
/// could observe the registry in that window, a young object reachable only
/// from a conservative root would not be recognised as an object, would not be
/// marked, and would be reclaimed under the mutator's feet. That is a
/// use-after-free, not a missed optimisation.
///
/// **Why it is safe anyway:** a collection only ever observes the registry at a
/// safepoint, and [`ZTlabRegistry::retire_all`] — which every collection must
/// call first, see the module invariant — flushes every pending batch before it
/// returns. The registry is therefore complete at every instant it can be read.
/// This is the same discipline SATB uses for its per-thread buffers, which are
/// likewise invisible to the collector until the remark safepoint drains them.
/// If a future change lets a collection walk the heap without going through
/// `retire_all`, the batching must be reverted *in the same commit*.
pub trait ZTlabHeapHooks: Send + Sync {
    /// Register a batch of freshly allocated object base addresses, whose
    /// footprints total `bytes`.
    ///
    /// `bytes` is the **reserved footprint** (the size actually consumed from
    /// the page), not the requested size, because that is what
    /// `ZgcRealHeap::allocated` has to count for `needs_gc` to be right. Note
    /// the `gc_rearm` scar: `needs_gc` once latched permanently true and ran a
    /// full mark-sweep per allocation. Under-counting here re-arms too late
    /// (heap exhaustion); over-counting re-arms too early (a GC storm). Count
    /// what the heap actually gave away, once.
    fn register_allocations(&self, addrs: &[usize], bytes: usize);

    /// The color a freshly allocated object's references must be born with.
    ///
    /// Wire this to [`crate::zgc::vaddr::ZGoodMask::allocation_color`]. Getting
    /// it wrong is not a slow path, it is a *permanent* slow path: an object
    /// born with a stale color fails the load barrier's mask test on every
    /// single load until something recolors it.
    ///
    /// Returns [`ZColor`] rather than the raw `u64` the task sketch used,
    /// because that is what `ZGoodMask::allocation_color()` actually returns;
    /// [`Self::allocation_color_bit`] gives the `u64` spelling for a caller
    /// that wants to OR it straight into a word.
    fn allocation_color(&self) -> ZColor;

    /// Identity-hash source, equivalent to `ZgcRealHeap::next_hash`.
    ///
    /// Needed because `ZgcRealHeap::alloc_array` stamps a hash into the array
    /// header at allocation time (objects get theirs lazily from the mark
    /// word). A TLAB-allocated array still needs one.
    fn next_hash(&self) -> i32;

    /// Report bytes consumed from a page that will never hold a live object:
    /// the abandoned tail of a retired chunk.
    ///
    /// Default no-op. A heap that wants `allocated` to reflect true page
    /// consumption (and therefore trigger a collection at the right time)
    /// should add this to the same counter `register_allocations` feeds.
    /// The tail is dead by construction — it is covered by a filler sentinel
    /// and must **not** be passed to `register_allocations`, or a conservative
    /// probe that lands on filler bytes would be told it found an object.
    fn note_waste(&self, bytes: usize) {
        let _ = bytes;
    }

    /// Convenience single-address form. Defaults to a one-element batch so an
    /// implementation only has to write [`Self::register_allocations`].
    fn register_allocation(&self, addr: usize, bytes: usize) {
        self.register_allocations(std::slice::from_ref(&addr), bytes);
    }

    /// [`Self::allocation_color`] as the raw metadata bit.
    fn allocation_color_bit(&self) -> u64 {
        self.allocation_color().bit()
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// TLAB geometry and policy.
///
/// Defaults come from [`crate::tlab`]'s constants so ZGC and the generational
/// collector are tuned from one place. Every field is public so a unit test can
/// drive a 16 KiB chunk instead of a 256 KiB one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZTlabConfig {
    /// Chunk size for a thread's very first refill, before the adaptive sizer
    /// has any history. Deliberately not routed through the sizer: a fresh
    /// [`Tlab`] reports zero elapsed time and zero consumption, which the
    /// grow/shrink heuristic reads as "filled instantly" and doubles.
    pub initial_chunk: usize,
    /// Floor on a refill request. Note that
    /// [`crate::tlab::TlabPressureTracker::next_refill_size`] additionally
    /// clamps to `crate::tlab::min_tlab_size()` internally, so this can only
    /// narrow the range, never widen it below that floor.
    pub min_chunk: usize,
    /// Ceiling on a refill request. Also clamped down to the small page size at
    /// refill time — a chunk that cannot fit in one page could never be served.
    pub max_chunk: usize,
    /// Largest object served from a TLAB. Anything bigger takes the
    /// [large-object bypass](ZTlab::allocate).
    pub max_tlab_alloc: usize,
    /// HotSpot's `TLABRefillWasteFraction`. When a request does not fit, the
    /// buffer is thrown away only if its remainder is at most
    /// `chunk_size / refill_waste_fraction`; a bigger remainder is worth
    /// keeping, and the object goes direct to the page allocator instead. See
    /// [`ZTlab::allocate`] for the fragmentation argument.
    pub refill_waste_fraction: usize,
    /// HotSpot's `TLABWasteIncrement`. Added to the waste limit each time the
    /// direct path is taken, so a thread whose object sizes straddle the
    /// remainder cannot ping-pong down the direct path forever. Zero disables
    /// the ratchet (used by the threshold-boundary test, which wants the
    /// policy isolated).
    pub waste_increment: usize,
    /// Cap on pending registry entries before a flush is forced. Mirrors
    /// SATB's per-thread buffer capacity. Bounds the thread-private `Vec` and
    /// bounds how stale the registry can get between safepoints.
    pub registry_batch: usize,
}

impl Default for ZTlabConfig {
    fn default() -> Self {
        Self {
            initial_chunk: tlab::initial_refill_size(),
            min_chunk: tlab::min_tlab_size(),
            max_chunk: tlab::max_tlab_size(),
            max_tlab_alloc: tlab::tlab_max_alloc(),
            // OpenJDK defaults.
            refill_waste_fraction: 64,
            waste_increment: 4 * ZPAGE_OBJECT_GRID,
            registry_batch: 512,
        }
    }
}

impl ZTlabConfig {
    /// Normalise a configuration into something the allocator can honour:
    /// non-zero divisors, an ordered `[min, max]` range, grid-aligned chunk
    /// sizes, and a non-empty batch.
    ///
    /// Clamping rather than erroring, deliberately: these values can come from
    /// a command-line flag, and a silly flag should shrink the TLAB, not refuse
    /// to start the VM. The one thing that *cannot* be papered over —
    /// `max_tlab_alloc` larger than any chunk we can obtain — is fixed by
    /// lowering `max_tlab_alloc`, so the invariant "a TLAB-eligible object
    /// always fits a chunk we are allowed to ask for" holds by construction.
    /// (A request larger than the *sizer's* current pick is fine: `refill`
    /// raises the request to fit. A request larger than `max_chunk` is not,
    /// because raising past the ceiling would then also have to be checked
    /// against the page tier.)
    pub fn normalized(&self) -> ZTlabConfig {
        let grid = ZPAGE_OBJECT_GRID;
        let round_down = |v: usize| v & !(grid - 1);
        let mut min_chunk = round_down(self.min_chunk.max(grid));
        let mut max_chunk = round_down(self.max_chunk.max(grid));
        if max_chunk < min_chunk {
            max_chunk = min_chunk;
        }
        let initial_chunk = round_down(self.initial_chunk).clamp(min_chunk, max_chunk);
        if min_chunk > initial_chunk {
            min_chunk = initial_chunk;
        }
        let max_tlab_alloc = self.max_tlab_alloc.min(max_chunk);
        ZTlabConfig {
            initial_chunk,
            min_chunk,
            max_chunk,
            max_tlab_alloc,
            refill_waste_fraction: self.refill_waste_fraction.max(1),
            waste_increment: self.waste_increment,
            registry_batch: self.registry_batch.max(1),
        }
    }
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Per-TLAB (and, aggregated, per-registry) accounting.
///
/// Plain `u64` fields, not atomics: a [`ZTlab`] is thread-private and the
/// aggregate is assembled at a safepoint. Adding an atomic here would put a
/// shared cache line back on the fast path, which is the one thing this module
/// exists to remove.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZTlabStats {
    /// Objects served by the in-buffer bump.
    pub fast_allocations: u64,
    /// Footprint bytes served by the in-buffer bump.
    pub fast_bytes: u64,
    /// Objects that fit a TLAB but were served straight from the page
    /// allocator because the current buffer's remainder was too big to waste.
    pub direct_allocations: u64,
    /// Footprint bytes served by the waste-policy direct path.
    pub direct_bytes: u64,
    /// Objects too big for a TLAB, served by the large-object bypass.
    pub large_allocations: u64,
    /// Footprint bytes served by the large-object bypass.
    pub large_bytes: u64,
    /// Chunks carved from a page.
    pub refills: u64,
    /// Bytes reserved by those chunks (>= the bytes actually handed out).
    pub refill_bytes: u64,
    /// Chunk retirements (each installs a tail filler).
    pub retires: u64,
    /// Chunk-tail bytes abandoned at retirement. Covered by a filler; dead.
    pub waste_bytes: u64,
    /// Private pages taken from [`ZPageAllocator::alloc_page`].
    pub pages_taken: u64,
    /// Batched registry hand-offs — i.e. registry mutex acquisitions.
    pub registry_flushes: u64,
    /// Object addresses handed to the registry.
    pub registered_objects: u64,
}

impl ZTlabStats {
    /// Fold `other` into `self`.
    pub fn add(&mut self, other: &ZTlabStats) {
        self.fast_allocations += other.fast_allocations;
        self.fast_bytes += other.fast_bytes;
        self.direct_allocations += other.direct_allocations;
        self.direct_bytes += other.direct_bytes;
        self.large_allocations += other.large_allocations;
        self.large_bytes += other.large_bytes;
        self.refills += other.refills;
        self.refill_bytes += other.refill_bytes;
        self.retires += other.retires;
        self.waste_bytes += other.waste_bytes;
        self.pages_taken += other.pages_taken;
        self.registry_flushes += other.registry_flushes;
        self.registered_objects += other.registered_objects;
    }

    /// Every object served, by whichever path.
    pub fn total_allocations(&self) -> u64 {
        self.fast_allocations + self.direct_allocations + self.large_allocations
    }

    /// Every footprint byte served, by whichever path. Excludes
    /// [`Self::waste_bytes`], which is dead filler rather than an object.
    pub fn total_bytes(&self) -> u64 {
        self.fast_bytes + self.direct_bytes + self.large_bytes
    }

    /// Fraction of allocations that took the in-buffer bump. The number to
    /// watch: below ~0.99 the waste policy or the chunk sizing is wrong.
    pub fn fast_path_ratio(&self) -> f64 {
        let total = self.total_allocations();
        if total == 0 {
            return 0.0;
        }
        self.fast_allocations as f64 / total as f64
    }
}

// ---------------------------------------------------------------------------
// ZTlab
// ---------------------------------------------------------------------------

/// The reserved footprint of a `bytes`/`align` request.
///
/// Mirrors `Tlab::alloc_initialized`'s own `footprint` computation exactly, so
/// the accounting the hooks receive is the accounting the buffer performed.
/// It excludes the *leading* alignment padding, which is zero whenever
/// `align <= ZPAGE_OBJECT_GRID` because the cursor is left on the grid — and
/// every production allocation path uses `align == 8`.
#[inline]
fn footprint_of(bytes: usize, align: usize) -> usize {
    let a = if align == 0 { ZPAGE_OBJECT_GRID } else { align };
    debug_assert!(a.is_power_of_two(), "alignment must be a power of two");
    (bytes.saturating_add(a - 1)) & !(a - 1)
}

/// A thread-private allocation buffer over one [`ZPageReal`].
///
/// # Layout
///
/// `#[repr(C)]` with [`Self::inner`] first so the shared `Tlab`'s JIT contract
/// survives the wrapping: `Tlab::CURSOR_OFFSET` (0) and `Tlab::END_OFFSET` (8)
/// remain valid relative to a `*mut ZTlab`. A future ZGC inline-allocation
/// fast path can therefore reuse the x64 emitter unchanged. Enforced by
/// `inner_tlab_sits_at_offset_zero`.
///
/// # The no-atomics invariant, and what would break it
///
/// [`Self::alloc`] is a plain load, add, compare, store on thread-private
/// memory. There is no CAS, no lock, and no shared cache line — that is the
/// entire reason a TLAB is faster than [`ZPageReal::alloc`], which *is* a CAS,
/// or than `ZgcRealHeap`'s arena mutex, which is a globally contended lock.
///
/// **A future concurrent relocator is the thing that can violate this.** If a
/// GC worker were allowed to touch a live TLAB's chunk while the owner is
/// running — to evacuate an object out of it, or to rewrite a cursor — the
/// plain load/store pair becomes a data race with no synchronisation anywhere
/// to make it well-defined. The rule is: a TLAB's chunk belongs to its owner
/// until [`Self::retire`] hands it back, and the way a collector gets access is
/// by driving [`ZTlabRegistry::retire_all`] at a safepoint, not by reaching
/// into a live buffer.
#[repr(C)]
pub struct ZTlab {
    /// The shared TLAB doing the actual bump. **Must stay first** — see the
    /// layout note.
    inner: Tlab,
    /// The page the current chunk was carved from, held privately by this
    /// thread while it is `Allocating`. `None` before the first refill and
    /// after a retire.
    page: Option<Arc<ZPageReal>>,
    /// `[start, end)` of the current chunk, or `None` when the buffer is
    /// retired. Recorded separately from `inner` because `Tlab::retire` nulls
    /// its own pointers and the retire path still needs the extent for
    /// accounting.
    chunk: Option<(usize, usize)>,
    /// Byte size of the current chunk; the base for the adaptive sizer and for
    /// the waste limit.
    chunk_size: usize,
    /// Remainder (bytes) below which the buffer is cheap enough to throw away.
    /// Reset to `chunk_size / refill_waste_fraction` at every refill and
    /// ratcheted up by `waste_increment` on each direct-path allocation.
    waste_limit: usize,
    /// What the shared adaptive sizer asked for at the last retirement.
    ///
    /// Computed inside [`Self::retire_chunk`] **before** `Tlab::retire` nulls
    /// the cursor, because `Tlab::next_refill_size` reads the live
    /// `cursor - start` span and debug-asserts that it is called on a
    /// non-retired buffer. Carrying the answer forward in a field is what lets
    /// a refill happen an arbitrary time after the retirement that preceded it
    /// — which is the normal case, since a safepoint retires every buffer and
    /// the owning threads refill later.
    next_chunk_hint: Option<usize>,
    /// Which generation this buffer allocates into.
    generation: ZTlabGeneration,
    /// Which generation the privately-held `page` was taken for, or `None` when
    /// no page is held. Moves in lock-step with `page`.
    ///
    /// **This is where the tag lives, and the reason it is not on the page** —
    /// see the 2026-08-07 note on [`ZTlabGeneration`]. It exists for one
    /// consumer: [`Self::refill`] must not carve an old-generation chunk out of
    /// a page it took for young (or the reverse), and [`Self::set_generation`]
    /// uses it to drop a page whose generation no longer matches. Nothing
    /// outside this type reads it, which is precisely why it never needed to be
    /// stored on a shared `ZPageReal` word.
    page_generation: Option<ZTlabGeneration>,
    /// Batched registry addresses awaiting a flush. Pre-sized at construction
    /// and only ever `clear()`ed, so the fast-path `push` never reallocates.
    pending: Vec<usize>,
    /// Footprint bytes represented by `pending`.
    pending_bytes: usize,
    /// Normalised policy.
    config: ZTlabConfig,
    /// Accounting.
    stats: ZTlabStats,
}

impl ZTlab {
    /// Byte offset of the wrapped [`Tlab`], and therefore of its `cursor`.
    pub const INNER_TLAB_OFFSET: usize = 0;

    /// An empty (retired) buffer. Serves nothing until the first refill.
    pub fn new(config: ZTlabConfig, generation: ZTlabGeneration) -> Self {
        let config = config.normalized();
        let batch = config.registry_batch;
        Self {
            inner: Tlab::empty(),
            page: None,
            chunk: None,
            chunk_size: 0,
            waste_limit: 0,
            next_chunk_hint: None,
            generation,
            page_generation: None,
            pending: Vec::with_capacity(batch),
            pending_bytes: 0,
            config,
            stats: ZTlabStats::default(),
        }
    }

    /// The generation this buffer allocates into.
    pub fn generation(&self) -> ZTlabGeneration {
        self.generation
    }

    /// The generation the privately-held page was taken for, or `None` when no
    /// page is held.
    ///
    /// Equal to `ZTlab::generation` whenever a page is held — the two can only
    /// differ transiently inside [`Self::set_generation`], which closes the gap
    /// by releasing the page. See the 2026-08-07 note on [`ZTlabGeneration`] for
    /// why this is a TLAB field and not a `ZPageReal` word.
    pub fn page_generation(&self) -> Option<ZTlabGeneration> {
        self.page_generation
    }

    /// Retarget the buffer at another generation.
    ///
    /// Takes `&mut self` and does **not** retire the chunk: the caller must
    /// retire first if the current chunk belongs to the other generation,
    /// because a chunk holds objects of one generation only and this call cannot
    /// flush the registry batch (it has no `hooks`).
    ///
    /// It *does* release a private page taken for the previous generation, once
    /// there is no live chunk in it. That is the guarantee the old
    /// `page.set_age(generation.page_age())` was pretending to give: a page must
    /// not serve two generations, and the check now lives on the field this type
    /// owns rather than on a shared page word `generation.rs` means something
    /// else by.
    pub fn set_generation(&mut self, generation: ZTlabGeneration) {
        debug_assert!(
            self.chunk.is_none(),
            "ZTlab::set_generation on a live chunk — retire first, a chunk serves one generation",
        );
        if self.chunk.is_none() && self.page_generation.is_some_and(|g| g != generation) {
            // Releasing is safe only with no live chunk: `release_page` moves
            // the page to `Relocatable`, and a collector may not select a page
            // a mutator is still bumping into.
            self.release_page();
        }
        self.generation = generation;
    }

    /// The policy in force.
    pub fn config(&self) -> &ZTlabConfig {
        &self.config
    }

    /// Accounting snapshot.
    pub fn stats(&self) -> ZTlabStats {
        self.stats
    }

    /// `[start, end)` of the current chunk, or `None` when retired.
    pub fn chunk(&self) -> Option<(usize, usize)> {
        self.chunk
    }

    /// The privately-held page, if any.
    pub fn page(&self) -> Option<&Arc<ZPageReal>> {
        self.page.as_ref()
    }

    /// Bytes still available in the current chunk.
    pub fn remaining(&self) -> usize {
        self.inner.remaining()
    }

    /// Has this buffer been retired (or never refilled)?
    pub fn is_retired(&self) -> bool {
        self.inner.is_retired()
    }

    /// Pending, not-yet-registered object addresses.
    pub fn pending_registrations(&self) -> usize {
        self.pending.len()
    }

    /// The un-allocated `[cursor, end)` tail of the live chunk, or `None`.
    ///
    /// Delegates to [`crate::tlab::Tlab::reserved_tail`]. This is the tripwire
    /// value for the module invariant: after [`Self::retire`] it must be
    /// `None`, and a `Some` at a walk point names the exact span that would
    /// have desynced the walk.
    pub fn reserved_tail(&self) -> Option<(usize, usize)> {
        self.inner.reserved_tail()
    }

    // -----------------------------------------------------------------------
    // The fast path
    // -----------------------------------------------------------------------

    /// **Fast path.** Bump-allocate `bytes` at `align` from the current chunk.
    ///
    /// Returns the object's base address, or `None` when the caller must take
    /// the slow path ([`Self::allocate`]). Two distinct conditions both report
    /// `None`, and the slow path distinguishes them:
    ///
    /// 1. the chunk cannot fit the request (`self.remaining()` is short), or
    /// 2. the pending registry batch is full ([`Self::needs_registry_flush`]).
    ///
    /// Conflating them keeps the fast path to a single failure branch, which is
    /// what an inline JIT sequence wants.
    ///
    /// # No atomics
    ///
    /// Load `cursor`, align, add, compare against `end`, store `cursor`, push
    /// one `usize`. No CAS, no lock. The `Release` fence inside
    /// `Tlab::alloc_initialized` remains and is intentional — it orders the
    /// (caller's, later) header stores ahead of the cursor commit for a
    /// cross-thread STW reader, and on x86-64 it emits no instruction.
    ///
    /// The returned memory is **already zero**: page storage is a zeroed
    /// reservation and [`ZPageReal::reset_shared`] re-zeroes every recycled
    /// page, so unlike the generational path there is no `write_bytes` at
    /// refill time.
    #[inline]
    pub fn alloc(&mut self, bytes: usize, align: usize) -> Option<usize> {
        if self.pending.len() >= self.config.registry_batch {
            return None;
        }
        let align = if align == 0 { ZPAGE_OBJECT_GRID } else { align };
        let ptr = self.inner.alloc(bytes, align)?;
        let addr = ptr as usize;
        let footprint = footprint_of(bytes, align);
        self.pending.push(addr);
        self.pending_bytes += footprint;
        self.stats.fast_allocations += 1;
        self.stats.fast_bytes += footprint as u64;
        Some(addr)
    }

    /// Is the pending registry batch at capacity?
    #[inline]
    pub fn needs_registry_flush(&self) -> bool {
        self.pending.len() >= self.config.registry_batch
    }

    // -----------------------------------------------------------------------
    // The slow path
    // -----------------------------------------------------------------------

    /// Allocate `bytes` at `align`, refilling, bypassing or wasting as needed.
    ///
    /// The full decision tree, in order:
    ///
    /// 1. **Large-object bypass.** `bytes > config.max_tlab_alloc` goes
    ///    straight to [`ZPageAllocator::alloc_object`], which routes it to the
    ///    Small / Medium / Large tier by
    ///    [`crate::zgc::page::ZPageConfig::class_for`]. The
    ///    threshold defaults to `crate::tlab::tlab_max_alloc()` (32 KiB), which
    ///    sits an order of magnitude below `page.rs`'s
    ///    `small_object_limit` (256 KiB): every TLAB-eligible object is
    ///    comfortably a Small object, so the bypass never *changes* an object's
    ///    tier — it only decides whether the object is worth carving a private
    ///    chunk for. Bypassed objects are registered immediately rather than
    ///    batched: they are rare by construction, and their page is the shared
    ///    one, not this thread's.
    /// 2. **Flush** if the pending batch is full.
    /// 3. **Bump**, and return if it fits.
    /// 4. **Waste policy.** The request did not fit, so the chunk's remainder
    ///    is now dead weight if we refill. Throw the buffer away only when
    ///    `remaining <= waste_limit` (default `chunk_size / 64`, HotSpot's
    ///    `TLABRefillWasteFraction`); otherwise keep it and serve *this* object
    ///    directly from the page allocator.
    ///
    ///    The fragmentation cost of the threshold, both ways: setting it too
    ///    high abandons up to that many bytes per refill — at the default that
    ///    is 1/64 of a chunk, i.e. under 2% of the heap turned into
    ///    filler-covered dead space, and the filler makes it walkable but not
    ///    reusable until the page is relocated. Setting it too low sends every
    ///    slightly-oversized object down the direct path, which is a CAS on the
    ///    *shared* page and scatters same-thread objects across pages, costing
    ///    locality and defeating the point. `waste_increment` breaks the
    ///    pathological middle: a thread whose object sizes straddle the
    ///    remainder would otherwise take the direct path indefinitely, so each
    ///    direct allocation raises the bar for keeping the buffer.
    /// 5. **Refill** and retry the bump once. If it still does not fit — only
    ///    reachable if the geometry is inconsistent — fall through to a direct
    ///    allocation rather than looping.
    pub fn allocate(
        &mut self,
        bytes: usize,
        align: usize,
        pages: &ZPageAllocator,
        hooks: &dyn ZTlabHeapHooks,
    ) -> Result<usize, ZPageError> {
        let align = if align == 0 { ZPAGE_OBJECT_GRID } else { align };

        // 1. Large-object bypass.
        if bytes > self.config.max_tlab_alloc {
            let addr = pages.alloc_object(bytes, align)?;
            let footprint = footprint_of(bytes, align);
            self.stats.large_allocations += 1;
            self.stats.large_bytes += footprint as u64;
            hooks.register_allocation(addr, footprint);
            self.stats.registry_flushes += 1;
            self.stats.registered_objects += 1;
            tracing::trace!(
                target: "zgc",
                "ZTlab: large bypass, {} B at {:#x} (> {} B TLAB limit)",
                bytes, addr, self.config.max_tlab_alloc,
            );
            return Ok(addr);
        }

        // 2. Keep the batch bounded.
        if self.needs_registry_flush() {
            self.flush_pending(hooks);
        }

        // 3. Bump.
        if let Some(addr) = self.alloc(bytes, align) {
            return Ok(addr);
        }

        // 4. Waste policy.
        let remaining = self.inner.remaining();
        if !self.inner.is_empty() && remaining > self.waste_limit {
            self.waste_limit = self.waste_limit.saturating_add(self.config.waste_increment);
            return self.allocate_direct(bytes, align, pages, hooks);
        }

        // 5. Refill, then retry exactly once.
        self.refill(bytes, align, pages, hooks)?;
        if let Some(addr) = self.alloc(bytes, align) {
            return Ok(addr);
        }
        self.allocate_direct(bytes, align, pages, hooks)
    }

    /// Serve one object from the shared page allocator, keeping this buffer.
    fn allocate_direct(
        &mut self,
        bytes: usize,
        align: usize,
        pages: &ZPageAllocator,
        hooks: &dyn ZTlabHeapHooks,
    ) -> Result<usize, ZPageError> {
        let addr = pages.alloc_object(bytes, align)?;
        let footprint = footprint_of(bytes, align);
        self.stats.direct_allocations += 1;
        self.stats.direct_bytes += footprint as u64;
        hooks.register_allocation(addr, footprint);
        self.stats.registry_flushes += 1;
        self.stats.registered_objects += 1;
        Ok(addr)
    }

    /// Retire the current chunk and carve a fresh one.
    ///
    /// `min_bytes`/`min_align` are the request that forced the refill; the new
    /// chunk is guaranteed large enough to serve it, so the caller's retry
    /// cannot fail for want of room.
    fn refill(
        &mut self,
        min_bytes: usize,
        min_align: usize,
        pages: &ZPageAllocator,
        hooks: &dyn ZTlabHeapHooks,
    ) -> Result<(), ZPageError> {
        // ORDER IS LOAD-BEARING, and it is enforced one level down:
        // `retire_chunk` computes the adaptive next-size hint from the live
        // `cursor - start` span BEFORE `Tlab::retire` nulls it. A caller that
        // sized after retiring would read `consumed == 0`, which the sizer
        // takes for "idle" and halves — the one-way shrink ratchet documented
        // on `Tlab::next_refill_size`, which debug-asserts against exactly this
        // mistake. Retiring first here is therefore safe *because* the hint is
        // already banked; it also means a refill may legitimately happen long
        // after the retirement (a safepoint retires everyone; threads refill
        // whenever they next allocate).
        self.retire_chunk(hooks);
        let want = self
            .next_chunk_hint
            .take()
            .unwrap_or(self.config.initial_chunk);

        // Clamp: our own range, then the page tier (a chunk that cannot fit in
        // one Small page could never be served), then the object grid.
        let page_capacity = pages.config().small_page_size;
        let mut want = want
            .clamp(self.config.min_chunk, self.config.max_chunk)
            .min(page_capacity);
        let need = footprint_of(min_bytes, min_align).saturating_add(min_align);
        if want < need {
            want = need;
        }
        want &= !(ZPAGE_OBJECT_GRID - 1);
        if want > page_capacity {
            // `normalized()` caps `max_tlab_alloc` at `max_chunk`, so this is
            // only reachable with a page geometry smaller than the TLAB range.
            return Err(ZPageError::AllocationRefused {
                bytes: want,
                page_size: page_capacity,
            });
        }

        // Two attempts: the current private page, then a fresh one. A fresh
        // page has `remaining() == size >= want`, so it always serves.
        for attempt in 0..2 {
            debug_assert!(
                self.page.is_none() || self.page_generation == Some(self.generation),
                "ZTlab::refill would carve a chunk for one generation out of a page \
                 taken for another — `set_generation` must have released it",
            );
            let carved = match self.page.as_ref() {
                Some(page) => page.alloc(want, ZPAGE_OBJECT_GRID),
                None => None,
            };
            if let Some(addr) = carved {
                // SAFETY: `[addr, addr + want)` was reserved by this page's CAS
                // bump and is therefore owned exclusively by this thread until
                // retire. It lies inside the allocator's reservation, which is
                // never resized and outlives every page. It is zero: page
                // storage starts zeroed and `ZPageReal::reset_shared` re-zeroes
                // every recycled page. `addr` and `want` are both multiples of
                // the 8-byte object grid, so `addr + want` is 8-aligned, which
                // is what `Tlab::new`'s tail-filler contract requires.
                self.inner = unsafe { Tlab::new(addr as *mut u8, want) };
                self.chunk = Some((addr, addr + want));
                self.chunk_size = want;
                self.waste_limit = want / self.config.refill_waste_fraction;
                self.stats.refills += 1;
                self.stats.refill_bytes += want as u64;
                tracing::trace!(
                    target: "zgc",
                    "ZTlab({}): refilled {} B chunk at {:#x} from page {}",
                    self.generation.as_str(),
                    want,
                    addr,
                    self.page.as_ref().map(|p| p.id()).unwrap_or(0),
                );
                return Ok(());
            }
            if attempt == 0 {
                self.release_page();
                let page = pages.alloc_page(ZPageSizeClass::Small, 0)?;
                // The generation this page was carved for is recorded HERE, not
                // in `page.set_age(..)`. `ZPageReal::age` is `generation.rs`'s
                // survival counter and writing a generation tag into it handed
                // an old-generation page a minor cycle of promotion credit it
                // never earned — see the 2026-08-07 note on `ZTlabGeneration`.
                self.stats.pages_taken += 1;
                self.page = Some(page);
                self.page_generation = Some(self.generation);
            }
        }

        Err(ZPageError::AllocationRefused {
            bytes: want,
            page_size: page_capacity,
        })
    }

    // -----------------------------------------------------------------------
    // Retirement
    // -----------------------------------------------------------------------

    /// Hand the pending registry batch to the heap.
    ///
    /// Idempotent and cheap when empty, so a retire path can call it
    /// unconditionally.
    pub fn flush_pending(&mut self, hooks: &dyn ZTlabHeapHooks) {
        if self.pending.is_empty() {
            return;
        }
        hooks.register_allocations(&self.pending, self.pending_bytes);
        self.stats.registry_flushes += 1;
        self.stats.registered_objects += self.pending.len() as u64;
        // `clear` keeps the capacity, so the fast-path `push` never allocates
        // again after construction.
        self.pending.clear();
        self.pending_bytes = 0;
    }

    /// Close the current chunk: install the tail filler and flush the batch.
    ///
    /// This is clause (a) + (b) of the module invariant. It does **not** touch
    /// the page — [`Self::retire`] does that.
    fn retire_chunk(&mut self, hooks: &dyn ZTlabHeapHooks) {
        if !self.inner.is_empty() {
            // Bank the adaptive sizer's answer while the cursor is still live.
            // See the field note on `next_chunk_hint`.
            self.next_chunk_hint = Some(self.inner.next_refill_size());
            let waste = self.inner.remaining();
            // Installs an `int[]` filler over `[cursor, end)`, or the
            // `GAP_FILLER_CLASS_ID` sentinel when the tail is shorter than a
            // header. Either way the span becomes decodable, which is the whole
            // point: it sits BELOW the page's `top`, so a `top`-bounded walker
            // is going to read it.
            self.inner.retire();
            debug_assert!(
                self.inner.reserved_tail().is_none(),
                "ZTlab::retire_chunk left a reserved tail — a top-bounded page walk \
                 would decode it as objects",
            );
            self.stats.retires += 1;
            self.stats.waste_bytes += waste as u64;
            if waste > 0 {
                hooks.note_waste(waste);
            }
        }
        self.chunk = None;
        self.chunk_size = 0;
        self.waste_limit = 0;
        self.flush_pending(hooks);
    }

    /// Give the private page back to the page lifecycle.
    ///
    /// `Allocating -> Relocatable` via [`ZPageReal::try_transition`], so the
    /// collector can select it. Ignores a lost transition: another phase may
    /// already have moved the page to `InRelocationSet`, and forcing it back
    /// would steal a page out from under an in-flight evacuation.
    ///
    /// The page's un-carved remainder — from `top` up to `size` — is *not*
    /// waste in the walk sense: `page.rs` guarantees everything above `top` is
    /// untouched reservation that no walker decodes. It is committed-but-idle
    /// until the page is freed, which is the price of a private page and the
    /// reason chunks are large relative to objects but small relative to a
    /// page (a 2 MiB Small page yields eight 256 KiB chunks).
    fn release_page(&mut self) {
        self.page_generation = None;
        if let Some(page) = self.page.take() {
            let moved = page.try_transition(ZPageState::Allocating, ZPageState::Relocatable);
            tracing::trace!(
                target: "zgc",
                "ZTlab({}): released page {} ({} B used); transition={}",
                self.generation.as_str(), page.id(), page.used(), moved,
            );
        }
    }

    /// **Full retirement.** Close the chunk, flush the batch, release the page.
    ///
    /// Idempotent — the second call finds an empty buffer, an empty batch and
    /// no page, and does nothing. That matters because several paths can retire
    /// the same buffer with no synchronisation between them (a safepoint, then
    /// a thread teardown), exactly as `Tlab::retire`'s own idempotence note
    /// describes.
    ///
    /// After this returns, [`Self::reserved_tail`] is `None` and the buffer's
    /// page is no longer `Allocating`. Those two facts together are the module
    /// invariant.
    pub fn retire(&mut self, hooks: &dyn ZTlabHeapHooks) {
        self.retire_chunk(hooks);
        self.release_page();
        debug_assert!(self.is_retired());
        debug_assert!(self.pending.is_empty());
    }
}

impl std::fmt::Debug for ZTlab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZTlab")
            .field("generation", &self.generation)
            .field("chunk", &self.chunk.map(|(s, e)| (s, e)))
            .field("chunk_size", &self.chunk_size)
            .field("remaining", &self.inner.remaining())
            .field("waste_limit", &self.waste_limit)
            .field("pending", &self.pending.len())
            .field("page", &self.page.as_ref().map(|p| p.id()))
            .field("page_generation", &self.page_generation)
            .field("stats", &self.stats)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ZTlabCell
// ---------------------------------------------------------------------------

/// A [`ZTlab`] a safepoint can reach.
///
/// The mutex is a **safepoint lock, not an allocation lock** — see the module
/// note on locking. In steady state it is acquired only by the owning thread
/// and is therefore uncontended; the alternative (publishing a raw pointer and
/// relying on OS-level thread suspension) is a follow-up that needs the
/// vm-crate thread registry.
///
/// `Send + Sync` falls out for free: `parking_lot::Mutex<T>` is `Sync` whenever
/// `T: Send`, and `ZTlab` is `Send` because `crate::tlab::Tlab` is (its raw
/// pointers are into allocator-owned storage and are exclusive to one thread).
pub struct ZTlabCell {
    tlab: Mutex<ZTlab>,
}

impl ZTlabCell {
    fn new(tlab: ZTlab) -> Self {
        Self {
            tlab: Mutex::new(tlab),
        }
    }

    /// Direct access, for a caller that wants several allocations under one
    /// acquisition (the batching win, taken one level higher).
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, ZTlab> {
        self.tlab.lock()
    }

    /// Allocate one object.
    pub fn allocate(
        &self,
        bytes: usize,
        align: usize,
        pages: &ZPageAllocator,
        hooks: &dyn ZTlabHeapHooks,
    ) -> Result<usize, ZPageError> {
        self.tlab.lock().allocate(bytes, align, pages, hooks)
    }

    /// Retire this buffer. See [`ZTlab::retire`].
    pub fn retire(&self, hooks: &dyn ZTlabHeapHooks) {
        self.tlab.lock().retire(hooks);
    }

    /// Accounting snapshot.
    pub fn stats(&self) -> ZTlabStats {
        self.tlab.lock().stats()
    }

    /// The live reserved tail, if any. See [`ZTlab::reserved_tail`].
    pub fn reserved_tail(&self) -> Option<(usize, usize)> {
        self.tlab.lock().reserved_tail()
    }
}

impl std::fmt::Debug for ZTlabCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZTlabCell")
            .field("tlab", &*self.tlab.lock())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ZTlabRegistry
// ---------------------------------------------------------------------------

/// Key a thread by its [`std::thread::ThreadId`].
///
/// The same idiom `satb.rs::shard_for_current_thread` uses: `ThreadId` does not
/// expose its inner integer on stable, but its `Hash` impl is stable within a
/// thread, which is all a key needs. Std guarantees a `ThreadId` is never
/// reused within a process, so a key cannot be inherited by a later thread.
///
/// A hash collision between two live threads is possible in principle and
/// harmless in practice: they would share one [`ZTlabCell`], which is
/// mutex-protected, so the result is contention, not a race.
pub fn current_thread_key() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    h.finish()
}

/// What a [`ZTlabRegistry::retire_all`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZTlabRetireSummary {
    /// Buffers visited.
    pub tlabs: usize,
    /// Buffers that actually held a chunk (the rest were already retired).
    pub live_chunks: usize,
    /// Tail bytes made parseable by a filler.
    pub waste_bytes: u64,
    /// Object addresses flushed into the registry by this retirement.
    pub registered_objects: u64,
}

/// Every live TLAB, so a safepoint can retire all of them.
///
/// # Scoping
///
/// **Instance-owned. No `static`, no `OnceLock`, no thread-local.** A
/// process-global registry is how this tree got parallel-test crashes from
/// process-global native caches, and how a `OnceLock` latched a first guess
/// forever. The heap owns its registry; two heaps in one process (which the gc
/// unit tests routinely create) get two independent sets of buffers.
///
/// The consequence is that a thread cannot find "its" TLAB without a handle on
/// the registry — [`Self::attach`] returns an `Arc<ZTlabCell>` the caller is
/// expected to cache in whatever per-thread structure it already has. That is
/// deliberate: the alternative is a thread-local keyed by nothing, which is a
/// process-global by another name.
///
/// # Lock order
///
/// `registry.slots -> ZTlabCell.tlab -> ZPageAllocator.state`. Every method
/// here snapshots under `slots` and releases it before touching a cell, so the
/// reverse edge does not exist and a thread blocked mid-allocation cannot
/// deadlock a safepoint.
pub struct ZTlabRegistry {
    config: ZTlabConfig,
    generation: ZTlabGeneration,
    slots: Mutex<FxHashMap<u64, Arc<ZTlabCell>>>,
    /// Stats folded in from detached (dead-thread) buffers, so an aggregate
    /// does not lose a terminated thread's history.
    detached: Mutex<ZTlabStats>,
}

impl ZTlabRegistry {
    /// A registry handing out `config`-shaped buffers for `generation`.
    pub fn new(config: ZTlabConfig, generation: ZTlabGeneration) -> Self {
        Self {
            config: config.normalized(),
            generation,
            slots: Mutex::new(FxHashMap::default()),
            detached: Mutex::new(ZTlabStats::default()),
        }
    }

    /// A registry with the default (shared-TLAB-derived) policy, young gen.
    pub fn with_defaults() -> Self {
        Self::new(ZTlabConfig::default(), ZTlabGeneration::Young)
    }

    /// The policy every buffer is created with.
    pub fn config(&self) -> &ZTlabConfig {
        &self.config
    }

    /// The generation every buffer allocates into.
    pub fn generation(&self) -> ZTlabGeneration {
        self.generation
    }

    /// The calling thread's buffer, creating and registering it on first use.
    pub fn attach(&self) -> Arc<ZTlabCell> {
        self.attach_key(current_thread_key())
    }

    /// [`Self::attach`] for an explicit key. Exposed so a VM that already has
    /// its own thread ids can key by those instead of by `ThreadId`, and so
    /// tests can drive several buffers from one thread.
    pub fn attach_key(&self, key: u64) -> Arc<ZTlabCell> {
        let mut slots = self.slots.lock();
        if let Some(existing) = slots.get(&key) {
            return Arc::clone(existing);
        }
        let cell = Arc::new(ZTlabCell::new(ZTlab::new(
            self.config.clone(),
            self.generation,
        )));
        slots.insert(key, Arc::clone(&cell));
        tracing::debug!(
            target: "zgc",
            "ZTlabRegistry: attached {} TLAB for thread key {:#x} ({} live)",
            self.generation.as_str(), key, slots.len(),
        );
        cell
    }

    /// The calling thread's buffer if it has one, without creating it.
    pub fn peek(&self) -> Option<Arc<ZTlabCell>> {
        self.slots.lock().get(&current_thread_key()).map(Arc::clone)
    }

    /// Retire and unregister the calling thread's buffer.
    ///
    /// Call this from thread teardown. Skipping it is not unsafe — the buffer
    /// stays registered and `retire_all` still finds it — but it holds the
    /// thread's private page `Arc` alive until the next `retire_all`, so the
    /// page cannot be reclaimed in the meantime.
    pub fn detach(&self, hooks: &dyn ZTlabHeapHooks) {
        self.detach_key(current_thread_key(), hooks);
    }

    /// [`Self::detach`] for an explicit key.
    pub fn detach_key(&self, key: u64, hooks: &dyn ZTlabHeapHooks) {
        // Remove under the registry lock, retire OUTSIDE it: lock order.
        let cell = self.slots.lock().remove(&key);
        if let Some(cell) = cell {
            let mut tlab = cell.lock();
            tlab.retire(hooks);
            self.detached.lock().add(&tlab.stats());
        }
    }

    /// Number of registered buffers.
    pub fn len(&self) -> usize {
        self.slots.lock().len()
    }

    /// Are there no registered buffers?
    pub fn is_empty(&self) -> bool {
        self.slots.lock().is_empty()
    }

    /// **Retire every registered TLAB.** The safepoint entry point, and the
    /// precondition for any page walk — see the module invariant.
    ///
    /// Snapshots the cells under the registry lock and releases it before
    /// locking any of them, so a thread that is mid-allocation (holding its
    /// cell, waiting on the page allocator) can never be the far side of a
    /// deadlock with a thread that is attaching.
    ///
    /// Blocking on a peer's cell here is the *mechanism*, not an accident: it
    /// is what replaces an OS-level thread-suspension handshake. When this
    /// returns, no registered buffer holds a live chunk and no page in the heap
    /// has a zeroed, header-less span below its `top` that came from a TLAB.
    pub fn retire_all(&self, hooks: &dyn ZTlabHeapHooks) -> ZTlabRetireSummary {
        let cells: Vec<Arc<ZTlabCell>> = {
            let slots = self.slots.lock();
            slots.values().map(Arc::clone).collect()
        };
        let mut summary = ZTlabRetireSummary {
            tlabs: cells.len(),
            ..ZTlabRetireSummary::default()
        };
        for cell in cells.iter() {
            let mut tlab = cell.lock();
            if tlab.chunk().is_some() {
                summary.live_chunks += 1;
            }
            let before = tlab.stats();
            tlab.retire(hooks);
            let after = tlab.stats();
            summary.waste_bytes += after.waste_bytes - before.waste_bytes;
            summary.registered_objects += after.registered_objects - before.registered_objects;
        }
        tracing::debug!(
            target: "zgc",
            "ZTlabRegistry: retired {} TLAB(s), {} with a live chunk, {} B tail waste, \
             {} object(s) flushed to the registry",
            summary.tlabs, summary.live_chunks, summary.waste_bytes, summary.registered_objects,
        );
        debug_assert!(
            self.reserved_tails().is_empty(),
            "ZTlabRegistry::retire_all left a reserved tail — a top-bounded page walk \
             would decode zeroed bytes as objects",
        );
        summary
    }

    /// **The tripwire.** Reserved `[cursor, end)` tails of every registered
    /// buffer that still holds a live chunk.
    ///
    /// Must be empty at any point a collector walks page bytes. A non-empty
    /// result names exactly the spans that would desync the walk, which is far
    /// more useful than the SIGSEGV three phases later — the generational
    /// collector learned this the hard way and grew the same tripwire
    /// (`tlab-audit` in `gen_heap.rs`).
    ///
    /// Also usable the other way round, as the BUG-03 skip list: a collector
    /// that genuinely cannot retire a peer (forcibly stopped in compiled code)
    /// can skip these spans instead.
    pub fn reserved_tails(&self) -> Vec<(usize, usize)> {
        let cells: Vec<Arc<ZTlabCell>> = {
            let slots = self.slots.lock();
            slots.values().map(Arc::clone).collect()
        };
        let mut out: Vec<(usize, usize)> = Vec::new();
        for cell in cells.iter() {
            if let Some(span) = cell.reserved_tail() {
                out.push(span);
            }
        }
        out.sort_unstable();
        out
    }

    /// Aggregate accounting across live and detached buffers.
    pub fn stats(&self) -> ZTlabStats {
        let cells: Vec<Arc<ZTlabCell>> = {
            let slots = self.slots.lock();
            slots.values().map(Arc::clone).collect()
        };
        let mut total = *self.detached.lock();
        for cell in cells.iter() {
            total.add(&cell.stats());
        }
        total
    }
}

impl std::fmt::Debug for ZTlabRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZTlabRegistry")
            .field("generation", &self.generation)
            .field("live_tlabs", &self.slots.lock().len())
            .finish()
    }
}

/// The two synthetic class ids a retired chunk tail can carry.
///
/// Re-exported from [`crate::tlab`] rather than minted afresh, so ZGC's walkers
/// screen the *same* sentinels every other walker in the tree already screens.
/// Introducing a third filler kind would inherit the `HumongousFiller`
/// screening problem (unscreened at 24 of 26 call sites) for no gain.
pub const ZTLAB_TAIL_FILLER_CLASS_IDS: [cratonvm_types::ClassId; 2] =
    [TLAB_FILLER_CLASS_ID, GAP_FILLER_CLASS_ID];

/// Is `class_id` one of the tail-filler sentinels a retired chunk can leave?
///
/// A page walker must check this **before** decoding a header as a real object.
#[inline]
pub fn is_tlab_filler_class(class_id: u32) -> bool {
    class_id == TLAB_FILLER_CLASS_ID.as_u32() || class_id == GAP_FILLER_CLASS_ID.as_u32()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::HEADER_SIZE;
    use crate::zgc::generation::{ZPromotionPolicy, Z_DEFAULT_PROMOTION_AGE};
    use crate::zgc::page::ZPageConfig;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

    /// A miniature page geometry — the same *shape* as OpenJDK's at 1/32 the
    /// scale, so a test heap is 4 MiB and a Small page holds several chunks.
    fn page_config() -> ZPageConfig {
        ZPageConfig {
            granule_size: 4096,
            small_page_size: 64 * 1024,
            medium_page_size: 256 * 1024,
            small_object_limit: 8 * 1024,
            medium_object_limit: 32 * 1024,
            max_capacity: 4 * 1024 * 1024,
        }
    }

    fn pages() -> Arc<ZPageAllocator> {
        Arc::new(ZPageAllocator::new(page_config()).expect("test geometry must validate"))
    }

    /// A TLAB policy scaled to the miniature pages. Note `min_chunk` is
    /// `crate::tlab::min_tlab_size()` (8 KiB) because the shared adaptive sizer
    /// clamps to that internally regardless of what we ask for — see
    /// [`ZTlabConfig::min_chunk`].
    fn tlab_config() -> ZTlabConfig {
        ZTlabConfig {
            initial_chunk: 16 * 1024,
            min_chunk: 8 * 1024,
            max_chunk: 32 * 1024,
            max_tlab_alloc: 1024,
            refill_waste_fraction: 64, // waste limit = 256 B on a 16 KiB chunk
            waste_increment: 0,        // isolate the threshold in the boundary test
            registry_batch: 512,
        }
    }

    /// Records everything the heap would have done, so a test can assert on it.
    #[derive(Debug, Default)]
    struct TestHooks {
        batches: Mutex<Vec<Vec<usize>>>,
        registered: Mutex<Vec<usize>>,
        registered_bytes: AtomicUsize,
        waste_bytes: AtomicUsize,
        hash: AtomicI32,
    }

    impl TestHooks {
        fn new() -> Self {
            Self {
                hash: AtomicI32::new(1),
                ..Self::default()
            }
        }

        fn batch_count(&self) -> usize {
            self.batches.lock().len()
        }

        fn registered_count(&self) -> usize {
            self.registered.lock().len()
        }

        fn bytes(&self) -> usize {
            self.registered_bytes.load(Ordering::Relaxed)
        }

        fn contains(&self, addr: usize) -> bool {
            self.registered.lock().iter().any(|a| *a == addr)
        }
    }

    impl ZTlabHeapHooks for TestHooks {
        fn register_allocations(&self, addrs: &[usize], bytes: usize) {
            self.batches.lock().push(addrs.to_vec());
            self.registered.lock().extend_from_slice(addrs);
            self.registered_bytes.fetch_add(bytes, Ordering::Relaxed);
        }

        fn allocation_color(&self) -> ZColor {
            // The quiescent good color; a real heap wires this to
            // `ZGoodMask::allocation_color()`.
            ZColor::Remapped
        }

        fn next_hash(&self) -> i32 {
            self.hash.fetch_add(1, Ordering::Relaxed)
        }

        fn note_waste(&self, bytes: usize) {
            self.waste_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// A distinctive class id for test objects. Not zero (which is a legal
    /// class id and therefore indistinguishable from zeroed memory) and not
    /// either filler sentinel.
    const TEST_CID: u32 = 0x0BAD_0001;
    /// Test object size: a multiple of the object grid and at least a header,
    /// so a linear walk over them is well-formed whatever `HEADER_SIZE` is.
    const OBJ: usize = 64;

    /// Stamp a recognisable class id at offset 0, the one header-layout fact
    /// `install_tail_filler` itself relies on.
    fn stamp(addr: usize) {
        assert_eq!(addr % 8, 0, "object base must be on the 8-byte grid");
        unsafe { std::ptr::write(addr as *mut u32, TEST_CID) };
    }

    fn read_cid(addr: usize) -> u32 {
        unsafe { std::ptr::read(addr as *const u32) }
    }

    // -- fast path ----------------------------------------------------------

    #[test]
    fn bump_within_a_chunk_never_touches_shared_state() {
        let pages = pages();
        let hooks = TestHooks::new();
        let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Young);

        // One slow-path allocation to install a chunk.
        let first = tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let page_used_after_refill = tlab.page().unwrap().used();
        let refills_after_first = tlab.stats().refills;

        // Everything from here is the pure bump.
        let mut last = first;
        for _ in 0..64 {
            let addr = tlab.alloc(OBJ, 8).expect("chunk has room");
            assert_eq!(addr % 8, 0, "bump must stay on the object grid");
            assert_eq!(addr, last + OBJ, "bump must be dense and increasing");
            last = addr;
        }

        // The proxy for "took no lock": no shared structure moved. The page's
        // CAS cursor is untouched (only the chunk reservation moves it), no
        // refill happened, and the registry was never handed anything.
        assert_eq!(tlab.page().unwrap().used(), page_used_after_refill);
        assert_eq!(tlab.stats().refills, refills_after_first);
        assert_eq!(hooks.batch_count(), 0, "the batch must not have flushed");
        assert_eq!(tlab.pending_registrations(), 65);
    }

    #[test]
    fn inner_tlab_sits_at_offset_zero() {
        // The JIT's inline `new` reads `cursor` at +0 and `end` at +8 from a
        // TLAB pointer. Wrapping must not move them.
        let tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Young);
        let base = &tlab as *const ZTlab as usize;
        let inner = &tlab.inner as *const Tlab as usize;
        assert_eq!(inner - base, ZTlab::INNER_TLAB_OFFSET);
        assert_eq!(ZTlab::INNER_TLAB_OFFSET, 0);
        assert_eq!(Tlab::CURSOR_OFFSET, 0);
        assert_eq!(Tlab::END_OFFSET, 8);
    }

    // -- refill -------------------------------------------------------------

    #[test]
    fn refill_installs_a_fresh_chunk_on_exhaustion() {
        let pages = pages();
        let hooks = TestHooks::new();
        let cfg = tlab_config();
        let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);

        let per_chunk = cfg.initial_chunk / OBJ; // 256
        for _ in 0..per_chunk {
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        }
        assert_eq!(tlab.stats().refills, 1, "one chunk should have sufficed");
        assert_eq!(tlab.remaining(), 0, "the chunk should be exactly drained");
        let first_chunk = tlab.chunk().unwrap();

        // The next object cannot fit -> retire + refill.
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        assert_eq!(tlab.stats().refills, 2);
        let second_chunk = tlab.chunk().unwrap();
        assert_ne!(first_chunk, second_chunk);
        assert_eq!(
            second_chunk.0, first_chunk.1,
            "chunks must be contiguous within a page"
        );
        // Retiring the drained chunk flushed its batch.
        assert!(hooks.registered_count() >= per_chunk);
    }

    #[test]
    fn first_refill_uses_the_configured_initial_chunk_not_the_sizer() {
        // A fresh `Tlab` reports zero elapsed time and zero consumption, which
        // the shared adaptive sizer reads as "filled instantly" and doubles.
        // The first refill must therefore bypass it.
        let pages = pages();
        let hooks = TestHooks::new();
        let cfg = tlab_config();
        let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let (start, end) = tlab.chunk().unwrap();
        assert_eq!(end - start, cfg.initial_chunk);
    }

    // -- waste policy -------------------------------------------------------

    #[test]
    fn waste_policy_keeps_a_fat_remainder_and_discards_a_thin_one() {
        let pages = pages();
        let hooks = TestHooks::new();
        let cfg = tlab_config();
        // waste limit = 16384 / 64 = 256 bytes, ratchet disabled.
        let waste_limit = cfg.initial_chunk / cfg.refill_waste_fraction;
        assert_eq!(waste_limit, 256);

        let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();

        // Drain to a remainder of 320 bytes — ABOVE the limit, so the buffer is
        // worth keeping.
        while tlab.remaining() > 320 {
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        }
        assert_eq!(tlab.remaining(), 320);
        let refills_before = tlab.stats().refills;

        // A 512-byte object does not fit in 320 bytes.
        let addr = tlab.allocate(512, 8, &pages, &hooks).unwrap();
        assert_eq!(
            tlab.stats().refills,
            refills_before,
            "a remainder above the waste limit must NOT be thrown away"
        );
        assert_eq!(tlab.stats().direct_allocations, 1);
        assert_eq!(tlab.remaining(), 320, "the buffer must be untouched");
        assert!(
            hooks.contains(addr),
            "a direct allocation is registered immediately, not batched"
        );

        // Now drain to 64 bytes — BELOW the limit, so it is cheap to discard.
        while tlab.remaining() > 64 {
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        }
        assert_eq!(tlab.remaining(), 64);
        let refills_before = tlab.stats().refills;
        let direct_before = tlab.stats().direct_allocations;
        tlab.allocate(512, 8, &pages, &hooks).unwrap();
        assert_eq!(
            tlab.stats().refills,
            refills_before + 1,
            "a remainder at or below the waste limit must be retired"
        );
        assert_eq!(tlab.stats().direct_allocations, direct_before);
        assert_eq!(tlab.stats().waste_bytes, 64);
    }

    #[test]
    fn waste_increment_stops_the_direct_path_ratcheting_forever() {
        let pages = pages();
        let hooks = TestHooks::new();
        let mut cfg = tlab_config();
        cfg.waste_increment = 512;
        // Raise the bypass threshold so a 2 KiB request stays a TLAB request
        // rather than becoming a large-object bypass.
        cfg.max_tlab_alloc = 4096;
        let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        while tlab.remaining() > 1024 {
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        }
        assert_eq!(tlab.remaining(), 1024);
        // First oversized request: remainder 1024 > limit 256 -> direct, and
        // the limit ratchets to 768.
        tlab.allocate(2048, 8, &pages, &hooks).unwrap();
        assert_eq!(tlab.stats().direct_allocations, 1);
        assert_eq!(tlab.stats().refills, 1);
        // Second: remainder is still 1024 > 768 -> direct again, limit -> 1280.
        // Third: 1024 <= 1280, so the buffer finally loses and is retired.
        tlab.allocate(2048, 8, &pages, &hooks).unwrap();
        tlab.allocate(2048, 8, &pages, &hooks).unwrap();
        assert!(
            tlab.stats().refills >= 2,
            "the ratchet must eventually retire a buffer the policy kept re-skipping \
             (refills={}, direct={})",
            tlab.stats().refills,
            tlab.stats().direct_allocations,
        );
    }

    // -- large bypass -------------------------------------------------------

    #[test]
    fn large_objects_bypass_the_tlab_entirely() {
        let pages = pages();
        let hooks = TestHooks::new();
        let cfg = tlab_config();
        let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let remaining_before = tlab.remaining();
        let refills_before = tlab.stats().refills;

        // Above max_tlab_alloc (1 KiB) but still a Small object by page.rs's
        // routing (small_object_limit is 8 KiB) — the bypass decides whether to
        // carve a chunk, not which tier the object lands in.
        let small = tlab.allocate(4096, 8, &pages, &hooks).unwrap();
        // Above medium_object_limit (32 KiB) — its own Large page.
        let large = tlab.allocate(40_000, 8, &pages, &hooks).unwrap();

        assert_eq!(tlab.stats().large_allocations, 2);
        assert_eq!(
            tlab.remaining(),
            remaining_before,
            "the buffer is untouched"
        );
        assert_eq!(tlab.stats().refills, refills_before);
        assert!(pages.table().contains(small));
        assert!(pages.table().contains(large));
        assert!(hooks.contains(small) && hooks.contains(large));
        assert_eq!(
            pages.page_for(large).unwrap().size_class(),
            ZPageSizeClass::Large,
        );
    }

    // -- THE retirement test ------------------------------------------------

    /// The most important test in the file: after `retire_all`, every page's
    /// `[base, top)` decodes cleanly end to end with no zeroed hole.
    ///
    /// The walk strides test objects by their known size and jumps a retired
    /// chunk tail to the chunk's recorded end. It deliberately does **not**
    /// decode the filler's own length: the only header-layout fact this file
    /// may rely on is "class_id is a `u32` at offset 0", which is exactly what
    /// `Tlab::install_tail_filler` relies on for its `GAP_FILLER` sentinel.
    /// The header's field packing is mid-refactor; the sentinel is not.
    #[test]
    fn retire_all_leaves_every_page_top_consistent_and_walkable() {
        let pages = pages();
        let hooks = TestHooks::new();
        let cfg = tlab_config();
        let registry = ZTlabRegistry::new(cfg.clone(), ZTlabGeneration::Young);
        let cell = registry.attach();

        let per_chunk = cfg.initial_chunk / OBJ; // 256, an exact fit
        let mut chunks: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, objects)
        let mut current: Option<(usize, usize)> = None;
        let mut in_chunk = 0usize;

        for _ in 0..(per_chunk + 100) {
            let mut tlab = cell.lock();
            let addr = tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
            stamp(addr);
            let chunk = tlab.chunk().expect("a bumped object lives in a chunk");
            if current != Some(chunk) {
                if let Some(prev) = current {
                    chunks.push((prev.0, prev.1, in_chunk));
                }
                current = Some(chunk);
                in_chunk = 0;
            }
            in_chunk += 1;
        }
        if let Some(prev) = current {
            chunks.push((prev.0, prev.1, in_chunk));
        }
        assert_eq!(chunks.len(), 2, "expected exactly two chunks: {chunks:?}");

        // BEFORE retirement the hazard is real and observable.
        let tails = registry.reserved_tails();
        assert_eq!(tails.len(), 1, "the partially-filled chunk has a live tail");
        let (tail_lo, tail_hi) = tails[0];
        assert_eq!(read_cid(tail_lo), 0, "an un-retired tail is ZEROED memory");
        assert!(
            tail_lo < pages.page_for(tail_lo).unwrap().top_addr(),
            "and it sits BELOW the page's top, i.e. inside walk_bounds()"
        );
        assert!(tail_hi > tail_lo);

        let summary = registry.retire_all(&hooks);
        assert_eq!(summary.tlabs, 1);
        assert_eq!(summary.live_chunks, 1);
        assert!(summary.waste_bytes > 0);

        // Clause (a): no reserved tails remain.
        assert!(registry.reserved_tails().is_empty());
        // Clause (b): every object reached the registry.
        assert_eq!(hooks.registered_count(), per_chunk + 100);
        // Clause (c): no page is still Allocating for this thread.
        for page in pages.pages() {
            assert_ne!(
                page.state(),
                ZPageState::Allocating,
                "page {} was left Allocating after retire_all",
                page.id(),
            );
        }

        // The walk itself. One thread, one private page, no bypass, so the
        // whole heap is one page and the recorded chunks tile it exactly —
        // which is what lets the walk below carry `p` across chunk boundaries.
        assert_eq!(pages.pages().len(), 1, "this test assumes a single page");
        let mut walked_objects = 0usize;
        for page in pages.pages() {
            let (lo, hi) = page.walk_bounds();
            let mut p = lo;
            for &(cstart, cend, n) in chunks.iter() {
                if cstart < lo || cend > hi {
                    continue;
                }
                assert_eq!(p, cstart, "walk must arrive exactly at the chunk base");
                for _ in 0..n {
                    assert_eq!(
                        read_cid(p),
                        TEST_CID,
                        "walk desynced: {p:#x} is not a test object",
                    );
                    p += OBJ;
                    walked_objects += 1;
                }
                if p < cend {
                    let cid = read_cid(p);
                    assert_ne!(
                        cid, 0,
                        "the chunk tail at {p:#x} is still ZERO — a top-bounded walk \
                         would decode it as a live `new Object()` and stride off grid",
                    );
                    assert!(
                        is_tlab_filler_class(cid),
                        "the chunk tail at {p:#x} must carry a filler sentinel, got {cid:#x}",
                    );
                    let tail = cend - p;
                    if tail < HEADER_SIZE {
                        assert_eq!(cid, GAP_FILLER_CLASS_ID.as_u32());
                    } else {
                        assert_eq!(cid, TLAB_FILLER_CLASS_ID.as_u32());
                    }
                    p = cend;
                }
            }
            assert_eq!(p, hi, "the walk must land exactly on the page's top");
        }
        assert_eq!(walked_objects, per_chunk + 100);
    }

    #[test]
    fn retire_is_idempotent() {
        let pages = pages();
        let hooks = TestHooks::new();
        let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
        let cell = registry.attach();
        cell.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();

        registry.retire_all(&hooks);
        let after_first = cell.stats();
        registry.retire_all(&hooks);
        let after_second = cell.stats();
        assert_eq!(after_first.retires, after_second.retires);
        assert_eq!(after_first.waste_bytes, after_second.waste_bytes);
        assert_eq!(
            after_first.registered_objects,
            after_second.registered_objects
        );
        assert!(cell.lock().is_retired());
    }

    // -- registry batching --------------------------------------------------

    #[test]
    fn registry_inserts_are_batched_not_per_object() {
        let pages = pages();
        let hooks = TestHooks::new();
        let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
        let cell = registry.attach();

        for _ in 0..200 {
            let addr = cell.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
            stamp(addr);
        }
        // The whole point: 200 objects, zero registry mutex acquisitions so far.
        assert_eq!(hooks.batch_count(), 0);
        assert_eq!(hooks.registered_count(), 0);

        registry.retire_all(&hooks);

        assert_eq!(hooks.batch_count(), 1, "one hand-off for 200 objects");
        assert_eq!(hooks.registered_count(), 200);
        assert_eq!(hooks.bytes(), 200 * OBJ);
    }

    #[test]
    fn a_full_batch_flushes_without_disturbing_the_buffer() {
        let pages = pages();
        let hooks = TestHooks::new();
        let mut cfg = tlab_config();
        cfg.registry_batch = 4;
        let mut tlab = ZTlab::new(cfg, ZTlabGeneration::Young);

        for _ in 0..20 {
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        }
        assert_eq!(hooks.registered_count(), 16, "4 full batches flushed");
        assert_eq!(tlab.pending_registrations(), 4);
        assert_eq!(tlab.stats().refills, 1, "flushing must not refill");
        assert_eq!(tlab.stats().fast_allocations, 20);
    }

    // -- adaptive sizing ----------------------------------------------------

    #[test]
    fn adaptive_sizing_grows_a_hot_thread_and_shrinks_a_cold_one() {
        let pages = pages();
        let hooks = TestHooks::new();
        let cfg = tlab_config();
        let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);

        // GROW: drain the chunk completely. The shared sizer's "well used"
        // arm (>= 75% of the chunk consumed) fires, so this does not depend on
        // the sub-millisecond fill-time arm and survives a loaded host.
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let first = tlab.chunk().unwrap();
        assert_eq!(first.1 - first.0, cfg.initial_chunk);
        while tlab.remaining() >= OBJ {
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        }
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap(); // forces the refill
        let second = tlab.chunk().unwrap();
        assert!(
            second.1 - second.0 > first.1 - first.0,
            "a drained buffer must grow: {} -> {}",
            first.1 - first.0,
            second.1 - second.0,
        );

        // SHRINK: barely touch the fresh chunk, then ask the sizer what it
        // would request next.
        //
        // Three conditions have to hold for the shrink arm and nothing else to
        // fire, and all three are properties of the ALLOCATION PATTERN, not of
        // the clock: (1) the elapsed window must exceed the sizer's 1 ms
        // "filled instantly" threshold — `sleep` never undersleeps, so 3 ms is
        // a lower bound, and this is a bound on the SIZE decision rather than a
        // wall-clock assertion; (2) the object must be at or below the sizer's
        // 256-byte "large allocation" mark, or its few-but-large grow arm
        // fires; (3) the chunk must stay far from drained, or its well-used
        // grow arm fires.
        std::thread::sleep(std::time::Duration::from_millis(3));
        let big = second.1 - second.0;
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        assert!(tlab.remaining() > big / 2, "the chunk must stay cold");
        let sized = tlab.next_refill_size_for_test();
        assert!(
            sized < big,
            "a barely-touched buffer must shrink: {big} -> {sized}",
        );
    }

    impl ZTlab {
        /// Test-only view of what the shared adaptive sizer would ask for next.
        /// Must be called before a retire — see `Tlab::next_refill_size`.
        fn next_refill_size_for_test(&mut self) -> usize {
            self.inner
                .next_refill_size()
                .clamp(self.config.min_chunk, self.config.max_chunk)
        }
    }

    // -- accounting ---------------------------------------------------------

    #[test]
    fn accounting_totals_match_the_sum_of_allocations() {
        let pages = pages();
        let hooks = TestHooks::new();
        let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
        let cell = registry.attach();

        let sizes: [usize; 6] = [OBJ, 128, 40, 512, 4096, 40_000];
        let mut expected = 0usize;
        for round in 0..40 {
            let size = sizes[round % sizes.len()];
            cell.lock().allocate(size, 8, &pages, &hooks).unwrap();
            expected += footprint_of(size, 8);
        }
        registry.retire_all(&hooks);

        let stats = registry.stats();
        assert_eq!(stats.total_allocations(), 40);
        assert_eq!(stats.total_bytes(), expected as u64);
        // Every byte the hooks were told about is an object byte; the tail
        // waste goes through `note_waste`, never through the registry.
        assert_eq!(hooks.bytes(), expected);
        assert_eq!(hooks.registered_count(), 40);
        assert_eq!(
            hooks.waste_bytes.load(Ordering::Relaxed) as u64,
            stats.waste_bytes,
        );
    }

    #[test]
    fn detach_folds_a_dead_threads_stats_into_the_aggregate() {
        let pages = pages();
        let hooks = TestHooks::new();
        let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
        let cell = registry.attach_key(7);
        for _ in 0..10 {
            cell.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
        }
        assert_eq!(registry.len(), 1);
        registry.detach_key(7, &hooks);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
        let stats = registry.stats();
        assert_eq!(stats.fast_allocations, 10);
        assert_eq!(hooks.registered_count(), 10);
    }

    // -- concurrency --------------------------------------------------------

    /// The correctness test that matters: N threads bumping in parallel must
    /// never hand out overlapping memory.
    #[test]
    fn concurrent_threads_produce_disjoint_address_ranges() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 150;

        let pages = pages();
        let hooks = Arc::new(TestHooks::new());
        let registry = Arc::new(ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young));

        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let pages = Arc::clone(&pages);
            let hooks = Arc::clone(&hooks);
            let registry = Arc::clone(&registry);
            handles.push(std::thread::spawn(move || {
                let cell = registry.attach();
                let mut mine: Vec<usize> = Vec::with_capacity(PER_THREAD);
                for _ in 0..PER_THREAD {
                    let addr = cell
                        .lock()
                        .allocate(OBJ, 8, &pages, &*hooks)
                        .expect("test heap is large enough");
                    // Each thread stamps its own objects: if two threads ever
                    // shared a span this would be a data race AND the range
                    // check below would fail.
                    stamp(addr);
                    mine.push(addr);
                }
                mine
            }));
        }

        let mut all: Vec<usize> = Vec::new();
        for h in handles {
            all.extend(h.join().expect("worker must not panic"));
        }
        assert_eq!(all.len(), THREADS * PER_THREAD);
        assert_eq!(registry.len(), THREADS);

        // Set-disjointness, not timing: sort the ranges and prove no two touch.
        all.sort_unstable();
        for w in all.windows(2) {
            assert!(
                w[0] + OBJ <= w[1],
                "overlapping allocations: [{:#x}, {:#x}) and [{:#x}, ...)",
                w[0],
                w[0] + OBJ,
                w[1],
            );
        }
        for &addr in all.iter() {
            assert_eq!(
                read_cid(addr),
                TEST_CID,
                "object at {addr:#x} was clobbered"
            );
            assert!(pages.table().contains(addr));
        }

        registry.retire_all(&*hooks);
        assert!(registry.reserved_tails().is_empty());
        assert_eq!(hooks.registered_count(), THREADS * PER_THREAD);
        // One hand-off per thread's chunk, not one per object.
        assert!(
            hooks.batch_count() <= THREADS * 2,
            "registry batching regressed: {} hand-offs for {} objects",
            hooks.batch_count(),
            THREADS * PER_THREAD,
        );
    }

    #[test]
    fn every_thread_gets_its_own_buffer_and_its_own_page() {
        let pages = pages();
        let hooks = TestHooks::new();
        let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
        let a = registry.attach_key(1);
        let b = registry.attach_key(2);
        assert!(!Arc::ptr_eq(&a, &b));
        a.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
        b.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
        let page_a = a.lock().page().map(|p| p.id());
        let page_b = b.lock().page().map(|p| p.id());
        assert!(page_a.is_some() && page_b.is_some());
        assert_ne!(page_a, page_b, "private pages must not be shared");
        // Re-attaching the same key returns the SAME buffer.
        assert!(Arc::ptr_eq(&a, &registry.attach_key(1)));
    }

    // -- the generation tag vs. ZPageReal::age (2026-08-07) ------------------

    #[test]
    fn tlab_refill_never_writes_the_pages_generational_age() {
        let pages = pages();
        let hooks = TestHooks::new();
        // `Old` is the arm that used to stamp `age = 1`. (`Young` stamped `0`,
        // which is what a fresh page already carries — the young arm was a
        // no-op and that is exactly why the collision went unnoticed.)
        let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Old);
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let page = Arc::clone(tlab.page().expect("the first allocation refills"));
        assert_eq!(
            page.age(),
            0,
            "a page a TLAB just took has survived zero minor cycles — \
             ZPageReal::age is generation.rs's survival count, not a generation tag",
        );
        assert_eq!(
            tlab.page_generation(),
            Some(ZTlabGeneration::Old),
            "the generation tag belongs to the TLAB, which is the only thing that reads it",
        );

        // Drive the buffer until it abandons this page for a fresh one: neither
        // carving more chunks out of it nor releasing it may touch the age.
        let mut took_a_second_page = false;
        for _ in 0..4096 {
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
            let still_the_same_page = tlab.page().is_some_and(|held| Arc::ptr_eq(held, &page));
            if !still_the_same_page {
                took_a_second_page = true;
                break;
            }
        }
        assert!(
            took_a_second_page,
            "4096 x {} B must outlast a {} B page",
            OBJ,
            page_config().small_page_size,
        );
        assert_eq!(
            page.age(),
            0,
            "releasing a private page must leave its survival count alone",
        );
        assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Old));
    }

    /// The regression the audit's N6 describes, end to end: a page that is a
    /// TLAB refill target **and** a young-generation survivor must promote on
    /// the cycle `ZPromotionPolicy` says, not one early.
    ///
    /// The aging step is `generation::sweep_young`'s, replayed verbatim
    /// (`let age = page.age() + 1; page.set_age(age)`), because that is the
    /// writer whose meaning `page.rs` endorses. Counted in cycles, not seconds.
    #[test]
    fn a_tlab_refill_target_still_promotes_at_the_right_age() {
        let pages = pages();
        let hooks = TestHooks::new();
        let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Old);
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let page = Arc::clone(tlab.page().expect("the first allocation refills"));

        let policy = ZPromotionPolicy::default();
        let mut promoted_after: Option<u32> = None;
        for cycle in 1..=Z_DEFAULT_PROMOTION_AGE {
            // The buffer keeps bumping into the same page across the cycles.
            tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
            let age = page.age().saturating_add(1);
            page.set_age(age);
            if promoted_after.is_none() && policy.should_promote(age) {
                promoted_after = Some(cycle);
            }
        }
        assert_eq!(
            promoted_after,
            Some(Z_DEFAULT_PROMOTION_AGE),
            "promoted after {:?} minor cycles instead of {} — somebody other than \
             sweep_young wrote ZPageReal::age",
            promoted_after,
            Z_DEFAULT_PROMOTION_AGE,
        );
        assert_eq!(
            page.age(),
            Z_DEFAULT_PROMOTION_AGE,
            "the age must be exactly the number of cycles the page survived",
        );
    }

    #[test]
    fn the_generation_tag_tracks_the_private_page_not_the_page_word() {
        let pages = pages();
        let hooks = TestHooks::new();
        let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Young);
        assert_eq!(tlab.page_generation(), None, "no page, no tag");
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let page = Arc::clone(tlab.page().expect("the first allocation refills"));
        assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Young));

        tlab.retire(&hooks);
        assert!(tlab.page().is_none());
        assert_eq!(
            tlab.page_generation(),
            None,
            "the tag is released with the page, so it can never go stale",
        );
        assert_eq!(page.age(), 0, "retirement is not a minor cycle");

        // Retargeting a retired buffer takes effect on the next refill, and the
        // new page carries the new tag.
        tlab.set_generation(ZTlabGeneration::Old);
        assert_eq!(tlab.generation(), ZTlabGeneration::Old);
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Old));
        assert_eq!(
            tlab.page().expect("refilled").age(),
            0,
            "an old-generation TLAB may not hand its page any survival credit",
        );
    }

    // -- configuration ------------------------------------------------------

    #[test]
    fn normalized_config_keeps_a_tlab_eligible_object_fitting_a_chunk() {
        let cfg = ZTlabConfig {
            initial_chunk: 8 * 1024,
            min_chunk: 8 * 1024,
            max_chunk: 16 * 1024,
            // Absurd: bigger than any chunk we will ever hand out.
            max_tlab_alloc: 1024 * 1024,
            refill_waste_fraction: 0,
            waste_increment: 0,
            registry_batch: 0,
        }
        .normalized();
        assert_eq!(cfg.max_tlab_alloc, cfg.max_chunk);
        assert_eq!(cfg.refill_waste_fraction, 1, "a zero divisor is clamped");
        assert_eq!(cfg.registry_batch, 1, "a zero batch is clamped");
        assert_eq!(cfg.initial_chunk % ZPAGE_OBJECT_GRID, 0);
    }

    #[test]
    fn default_config_tracks_the_shared_tlab_constants() {
        let cfg = ZTlabConfig::default().normalized();
        assert_eq!(cfg.initial_chunk, tlab::initial_refill_size());
        assert_eq!(cfg.min_chunk, tlab::min_tlab_size());
        assert_eq!(cfg.max_chunk, tlab::max_tlab_size());
        assert_eq!(cfg.max_tlab_alloc, tlab::tlab_max_alloc());
        // The bypass threshold must sit well below page.rs's Small-object
        // limit, or the TLAB would be deciding an object's page TIER.
        assert!(cfg.max_tlab_alloc < ZPageConfig::default().small_object_limit);
    }

    #[test]
    fn filler_sentinels_are_the_shared_ones_not_new_ones() {
        assert!(is_tlab_filler_class(TLAB_FILLER_CLASS_ID.as_u32()));
        assert!(is_tlab_filler_class(GAP_FILLER_CLASS_ID.as_u32()));
        assert!(!is_tlab_filler_class(TEST_CID));
        assert!(!is_tlab_filler_class(0));
        assert_eq!(ZTLAB_TAIL_FILLER_CLASS_IDS.len(), 2);
    }
}
