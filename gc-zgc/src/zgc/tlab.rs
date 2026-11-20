// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Thread-local allocation buffers for the real, page-backed ZGC heap.
//!
//! # Reuse verdict: this is an ADAPTER over [`cratonvm_gc_core::tlab::Tlab`], not a rewrite
//!
//! The first question this module had to answer was whether the shared TLAB in
//! `gc/src/tlab.rs` could simply be reused. **It can, and it is.** [`ZTlab`]
//! wraps a `cratonvm_gc_core::tlab::Tlab` by value and inherits, verbatim:
//!
//! * the bump-and-bounds-check fast path ([`cratonvm_gc_core::tlab::Tlab::alloc`]),
//!   including the "reserve the aligned FOOTPRINT, not merely an aligned start"
//!   rule that keeps a linear walker from landing in inter-object padding;
//! * the tail filler ([`cratonvm_gc_core::tlab::Tlab::install_tail_filler`]) with **both**
//!   of its cases — the `int[]` filler for a tail >= `HEADER_SIZE` and the
//!   `GAP_FILLER_CLASS_ID` sentinel for the sub-header tail, which exists
//!   because a zeroed sub-header run is byte-identical to a live
//!   `new Object()` and desyncs a linear walk (Bug-D, 2026-06-12);
//! * the adaptive sizer ([`cratonvm_gc_core::tlab::TlabPressureTracker`]) with its
//!   JIT-blindness fix — the one-way shrink ratchet that dropped compiled
//!   threads to `MIN_TLAB_SIZE` forever because `alloc_count` cannot see the
//!   JIT's inline bump, closed by feeding `cursor - start` in as the byte
//!   high-water mark;
//! * [`cratonvm_gc_core::tlab::Tlab::reserved_tail`], which is what a cross-thread STW
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
//!
//! # ADOPTION STATUS (2026-09-20) — and a WARNING about the invariant above
//!
//! **No production allocation goes through this file.** [`ZTlab`] carves from
//! [`ZPageReal`], and `ZgcRealHeap` has no pages — it allocates from one
//! `Mutex<cratonvm_gc_core::arena::Arena>` (see `zgc::page`'s own adoption note). The
//! buffers a running VM bumps into are `zgc::arena_tlab::ZArenaTlab` and the VM
//! thread's bare `cratonvm_gc_core::tlab::Tlab` (`zgc::vm_tlab`). `arena_tlab.rs` states
//! the converse at its head and gives the reason this type could not simply be
//! reused: [`ZTlab::refill`] is private and takes a `&ZPageAllocator`, and
//! `ZgcRealHeap`'s sweep returns dead objects to the *arena* free list under
//! nothing but a `base >= arena_base` screen.
//!
//! The warning is about "THE INVARIANT" above, because the two allocators
//! disagree and both are right about themselves. Over a page, retiring before a
//! walk is a **correctness precondition**: an un-retired chunk tail is zeroed
//! memory below the page's `top`, and a zeroed `HEADER_SIZE` run is
//! byte-identical to a live `new Object()`, so a `top`-bounded walk strides off
//! the object grid. Over the arena it is **not**: every walker there is driven
//! by `ZgcRealHeap::registry`, so an un-handed-out tail is invisible rather than
//! misparsed — which is why `ZgcRealHeap::retire_all_tlabs` may `try_lock` and
//! *skip* a contended buffer (`arena_tlab.rs`, "why blocking here would be a
//! hang, not a handshake") where [`ZTlabRegistry::retire_all`] must block.
//! Carrying either rule across to the other allocator is a use-after-free in
//! one direction and a permanent hang in the other.
//!
//! ## The guard that makes this unassumable (2026-09-20, wave 2)
//!
//! `zgc::page` now carries the view/storage distinction as a *field* rather
//! than a doc paragraph: [`ZPageReal::is_owned_storage`] is `false` for the
//! accounting overlays `ZgcRealHeap` builds over its arena, and `alloc`,
//! `reset_shared` and `ZPageAllocator::free_page` all refuse one.
//!
//! That matters here because **every chunk this module hands out comes from
//! [`ZPageReal::alloc`]** ([`ZTlab::refill`]). So if a future adoption wires
//! this registry to pages that are in fact views — the exact mistake this note
//! exists to prevent, and the one a reader makes by taking `page.rs` at face
//! value — the refill now fails closed with a `debug_assert!` naming the page,
//! instead of bumping into the arena's free list and letting two allocators
//! fill the same span.
//!
//! See `docs/feature-designs/zgc-architecture.md`:
//! `gap-c-page-rs-is-a-complete-allocator-the-collector-does-not-use.md` and
//! `proposal-c-page-based-heap.md`.

use std::sync::Arc;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use cratonvm_gc_core::tlab::{self, Tlab, GAP_FILLER_CLASS_ID, TLAB_FILLER_CLASS_ID};
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
/// Defaults come from [`cratonvm_gc_core::tlab`]'s constants so ZGC and the generational
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
    /// [`cratonvm_gc_core::tlab::TlabPressureTracker::next_refill_size`] additionally
    /// clamps to `cratonvm_gc_core::tlab::min_tlab_size()` internally, so this can only
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
            // Heap-internal staging — see `TlabAccounting`.
            inner: Tlab::empty_heap_staging(),
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

    /// Retarget the buffer at another generation. Returns whether the retarget
    /// took effect.
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
    ///
    /// # A live chunk makes this a REFUSAL, not just a failed assertion
    ///
    /// The `debug_assert!` below has always been there, and in a debug build it
    /// is the whole story. In release it was not: the call fell through and
    /// wrote `self.generation` anyway, leaving the buffer pointed at one
    /// generation while still holding a page (and a half-filled chunk) taken for
    /// the other. The very next [`Self::refill`] would then carve an
    /// old-generation chunk out of a young page — the mixing this field exists
    /// to prevent — and its own guard is a `debug_assert!` too, so *both*
    /// tripwires are silent in exactly the build where the damage is real.
    ///
    /// So the release path now refuses as well: with a live chunk nothing is
    /// changed, `false` comes back, and the caller's own retire-then-retarget
    /// sequence is the only way through. Refusing keeps the invariant
    /// "`page_generation == Some(generation)` whenever a page is held" true by
    /// construction rather than by assertion.
    ///
    /// The `debug_assert!` that used to guard this is **replaced** by
    /// `#[must_use]` plus a warning, and that is a strengthening rather than a
    /// relaxation: `#[must_use]` refuses to compile a caller that ignores the
    /// answer, in *every* build profile, where the assert only fired in one —
    /// and the warning names the buffer in a release log, where the assert said
    /// nothing at all. It also makes the refusal reachable from a test, which a
    /// `debug_assert!` on the same condition makes impossible.
    #[must_use = "a retarget on a live chunk is REFUSED; retire the buffer first \
                  and retry, or the buffer keeps allocating into the old generation"]
    pub fn set_generation(&mut self, generation: ZTlabGeneration) -> bool {
        if self.chunk.is_some() {
            tracing::warn!(
                target: "zgc",
                from = self.generation.as_str(),
                to = generation.as_str(),
                "ZTlab::set_generation refused on a live chunk — a chunk serves one \
                 generation, so retire the buffer before retargeting it",
            );
            return false;
        }
        if self.page_generation.is_some_and(|g| g != generation) {
            // Releasing is safe only with no live chunk: `release_page` moves
            // the page to `Relocatable`, and a collector may not select a page
            // a mutator is still bumping into.
            self.release_page();
        }
        self.generation = generation;
        true
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
    /// Delegates to [`cratonvm_gc_core::tlab::Tlab::reserved_tail`]. This is the tripwire
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
    ///    threshold defaults to `cratonvm_gc_core::tlab::tlab_max_alloc()` (32 KiB), which
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
        //
        //    A refill that FAILS must not take the request down with it. The
        //    two requests are not the same size: a refill asks for a whole
        //    `want`-byte chunk (up to a Small page), while the object that
        //    forced it is at most `max_tlab_alloc` — three orders of magnitude
        //    smaller at the defaults. A heap with room for the object but not
        //    for a fresh chunk is therefore an ordinary state, not a corner,
        //    and propagating the chunk's `OutOfCapacity` would report heap
        //    exhaustion while the shared page had room for hundreds more
        //    objects. That is the shape of this tree's two recorded allocator
        //    defects (`zgc-oom-with-84-percent-of-the-heap-free`,
        //    `zgc-low-end-fragmentation-was-the-starved-tlab-rung`), and it is
        //    cheap to refuse here: fall through to the same direct path the
        //    waste policy uses, and only report the refill's error if *that*
        //    also fails.
        //
        //    `retire_chunk` has already run inside `refill`, so the buffer is
        //    empty either way; the next allocation retries the refill. The cost
        //    of the failed attempt is therefore one abandoned tail, which was
        //    abandoned before the failure was known.
        if let Err(refill_err) = self.refill(bytes, align, pages, hooks) {
            return self
                .allocate_direct(bytes, align, pages, hooks)
                .map_err(|_| refill_err);
        }
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
        // The floor the retry depends on: the request's own footprint plus the
        // worst-case LEADING padding `Tlab::alloc` may insert inside the chunk
        // when `min_align` exceeds the grid the chunk base sits on.
        //
        // Rounded UP to the object grid **before** the mask below, not after.
        // The mask rounds `want` DOWN, so raising `want` to an unaligned `need`
        // and masking afterwards could land the chunk up to `GRID - 1` bytes
        // *below* the floor it was just raised to — at which point the caller's
        // "retry cannot fail for want of room" guarantee is false and the
        // object falls to the direct path immediately after paying for a fresh
        // chunk. Reachable only for `min_align < GRID`, which no production
        // caller uses (every one passes 8) — which is exactly why it would have
        // sat here unnoticed.
        let need = footprint_of(min_bytes, min_align)
            .saturating_add(min_align)
            .saturating_add(ZPAGE_OBJECT_GRID - 1)
            & !(ZPAGE_OBJECT_GRID - 1);
        want &= !(ZPAGE_OBJECT_GRID - 1);
        if want < need {
            want = need;
        }
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
                // `new_heap_staging`, not `new`: see `TlabAccounting`.
                self.inner = unsafe { Tlab::new_heap_staging(addr as *mut u8, want) };
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
/// `T: Send`, and `ZTlab` is `Send` because `cratonvm_gc_core::tlab::Tlab` is (its raw
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
/// Re-exported from [`cratonvm_gc_core::tlab`] rather than minted afresh, so ZGC's walkers
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
mod tests;
