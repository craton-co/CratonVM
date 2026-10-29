// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zt t22/zc4: the arena's concurrent-relocation groundwork -- relocation-set
//! selection from the mark's liveness, and a per-region forwarding with the
//! copy-then-CAS install and the in-flight accounting a mutator-side copy
//! needs.
//!
//! Proposal: `docs/feature-designs/zgc-architecture.md`.
//!
//! # Production callers (zt t23/zc5), both behind a switch
//!
//! * **R1, the selection census** (`CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS=1`,
//!   diagnostic): [`ZgcRealHeap::reloc_select_census_begin`] runs
//!   [`reloc_select_census`] on the mark's survivors at the closing pause,
//!   before the sweep (both sweep arms, zc2's deferred one included); the
//!   slide hands its own selection to
//!   [`ZgcRealHeap::reloc_select_census_note_slide`]; one `[GC] zgc-rsel:`
//!   line per whole-heap cycle compares the two. It reads headers and mark
//!   bits only and changes nothing.
//! * **R2, the stop-the-world relocation through the cycle**
//!   (`CRATONVM_ZGC_REGION_RELOCATE=1`, opt-in): the slide's evacuating arm
//!   copies its survivors through [`drain_stw`] (claim, `relocate_or_forward`
//!   into the to-space window, seal) instead of its own loop. zt t23/zc7: the
//!   slide's forward index then asks the cycle first
//!   (`ZForwardIndex::for_slide`), so the rewrite resolves the low end's moves
//!   through `cycle.forwarded`, and the registry rebuild re-registers the
//!   cycle's installs from its tables. See
//!   [`ZgcRealHeap::region_relocate_evacuate`] for what is and is not fed from
//!   the cycle.
//! * **R3, the cycle kept across the mutator interval** (zt t23/zc7,
//!   `CRATONVM_ZGC_REGION_QUARANTINE=1|2`, opt-in, with R2): the from-space is
//!   quarantined by the `CRATONVM_ZGC_EVAC_QUARANTINE` machinery, the cycle
//!   is stored in [`ZRegionRelocCounters`] ([`ZgcRealHeap::keep_region_cycle`])
//!   and answers `forwarded_after_slide` first
//!   ([`ZgcRealHeap::region_cycle_forwarded`]) instead of `relocations`, and
//!   both are released at the next mark end (`release_evac_quarantine` ->
//!   [`ZgcRealHeap::take_region_cycle`] / [`ZgcRealHeap::retire_region_cycle`]),
//!   where `=1` hands the pairs over to `relocations` as the fallback until R4.
//!
//! The load barrier's slow path still does not call this module; that is R6,
//! after the color flip (stage 2e) and R4-R5.
//!
//! # Why a new module and not more of `forwarding.rs` / `relocate.rs`
//!
//! Those two hold the lock-free table ([`ZForwardingTable`]), the selector
//! ([`ZRelocationSet::select`]) and a page evacuator written against
//! `page::ZPageAllocator`. This module REUSES the first two unchanged and adds
//! only what the arena-backed heap is missing for a concurrent relocate:
//!
//! * **Selection at mark end** ([`region_liveness`],
//!   [`select_relocation_regions`]): today the selector's input is built by
//!   `ZgcRealHeap::logical_pages` inside the closing pause, after the sweep.
//!   A cycle whose sweep is deferred (zt t21/zc2) never reaches it. Here the
//!   input is a pure function of the mark's survivors, so it can run as
//!   HotSpot's "Concurrent Select Relocation Set" does: after mark end, with
//!   mutators live, before any byte moves.
//! * **Region-indexed lookup without a lock** ([`ZRelocationCycle`]):
//!   `ZForwardingRegistry::get` takes an `RwLock` and hashes a page id, which
//!   is acceptable for a pause and not for the load barrier's slow path. A
//!   cycle's region -> table map is built once, before relocation starts, and
//!   is immutable afterwards (HotSpot's forwarding table is populated in Pause
//!   Relocate Start for the same reason), so a lookup is two array reads.
//! * **The arena encoding** ([`ZRegionForwarding::encode_to`]): the `from`
//!   key is region-relative and the stored destination is arena-relative plus
//!   an 8-byte bias, so both fit the packed entry on Linux (arena base near
//!   2^47) as on Windows. This is the arena-domain counterpart of
//!   `ZRelocate::encode_to` / `decode_to`; [`ZForwardingTable::find_payload`]
//!   is called only inside this file for this domain.
//! * **The in-flight count** ([`ZRegionForwarding::retain`] / [`ZRegionGuard`]
//!   / [`ZRegionForwarding::seal`]): HotSpot's `ZForwarding` reference count.
//!   A copier (mutator or GC worker) holds a guard while it reads from-space;
//!   the region is sealed once its worker has relocated every live object, and
//!   it may be freed (or quarantined) only when sealed AND no guard is left.
//!   `relocate.rs`'s `ZRemapState` tracks the region's lifetime across cycles
//!   (Selected / Relocated / Remapped); it has no in-flight count, and this
//!   one has no lifetime, so the two compose rather than overlap.
//!
//! # Preconditions the wiring must establish (none is checked here)
//!
//! 1. **From-space is immutable while it is relocated.** Every load and store
//!    of a reference into a selected region goes through a barrier that
//!    forwards (or relocates) first, so nobody writes a from-space object after
//!    relocation start. That is stage 2e's bad color plus every reference read
//!    routed (the census list empty); a raw holder (JNI critical region, raw
//!    `Unsafe` address, a conservative JIT root, a VM TLAB tail) keeps its
//!    region out of the set through [`select_relocation_regions`]'s `excluded`
//!    argument.
//! 2. **A selected region receives no allocation** after selection: its holes
//!    must not be on the free list the mutators allocate from, and no TLAB
//!    chunk may bump inside it. (zt t23/zc7: R3, with both ends of the
//!    relocation stop-the-world, needs only the weaker form -- no moved
//!    survivor's from-extent is re-issued while the cycle answers -- which the
//!    quarantine gives; the holes that were free before the slide stay on the
//!    free list. The strong form is R6's, where a region is freed whole at
//!    quiescence.)
//! 3. **The live-object counts are exact** (one per object the mark found
//!    live in the region), or a table can fill
//!    ([`ZRegionRelocateError::Table`] with `TableFull`).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashSet;

use super::{ZObjectStartsSnapshot, ZgcRealHeap};
use crate::arena::Arena;
use crate::heap::HEADER_SIZE;
use crate::zgc::forwarding::{
    PageCandidate, ZForwardingInsertError, ZForwardingPayload, ZForwardingTable, ZRelocationPolicy,
    ZRelocationSet, ZFWD_ALIGN_BYTES, ZFWD_MAX_FROM_OFFSET, ZFWD_MAX_PAYLOAD,
    ZFWD_SIZE_CLASS_SMALL,
};

/// The region size of the arena's relocation grid: 2 MiB, HotSpot's small
/// page. Must equal `ZgcRealHeap::Z_LOGICAL_PAGE_BYTES` so a region id here is
/// the same page id the existing selector and the remembered set use (pinned
/// by `region_size_is_the_logical_page_size`).
pub const ZRELOC_REGION_BYTES: usize = 2 * 1024 * 1024;

/// Added to every arena-relative destination before it is packed, so the
/// arena's first byte (relative offset 0, a legal destination) never encodes
/// to the zero payload `ZgcForwardingEntry::pack` refuses. One 8-byte granule:
/// keeps the alignment `pack` requires. Same rule as `ZRELOCATE_ENCODING_BIAS`.
pub const ZRELOC_ENCODING_BIAS: usize = ZFWD_ALIGN_BYTES as usize;

/// The relocation grid over one arena's low region: `[base, end)` cut into
/// equal power-of-two regions. `end` is the low bump extent (what was ever
/// allocated); bytes above it cost nothing to reclaim and are never counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZRegionGrid {
    base: usize,
    end: usize,
    region_bytes: usize,
}

impl ZRegionGrid {
    /// A grid over `[base, end)` with `region_bytes` regions.
    ///
    /// `None` when `region_bytes` is not a power of two, is below one object
    /// header, or is larger than the forwarding entry's from-offset field can
    /// key (32 MiB), when `base` is not 8-byte aligned, or when `end < base`.
    pub fn new(base: usize, end: usize, region_bytes: usize) -> Option<Self> {
        if !region_bytes.is_power_of_two()
            || region_bytes < HEADER_SIZE
            || (region_bytes as u64) > ZFWD_MAX_FROM_OFFSET + ZFWD_ALIGN_BYTES
            || !base.is_multiple_of(ZFWD_ALIGN_BYTES as usize)
            || end < base
        {
            return None;
        }
        Some(Self {
            base,
            end,
            region_bytes,
        })
    }

    /// The arena base (region 0's first byte).
    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    /// The low bump extent.
    #[inline]
    pub fn end(&self) -> usize {
        self.end
    }

    /// Bytes per region.
    #[inline]
    pub fn region_bytes(&self) -> usize {
        self.region_bytes
    }

    /// Regions that hold at least one allocated byte.
    #[inline]
    pub fn region_count(&self) -> usize {
        (self.end - self.base).div_ceil(self.region_bytes)
    }

    /// The region `addr` lies in, or `None` outside `[base, end)`.
    #[inline]
    pub fn region_of(&self, addr: usize) -> Option<u64> {
        if !(self.base..self.end).contains(&addr) {
            return None;
        }
        Some(((addr - self.base) / self.region_bytes) as u64)
    }

    /// First byte of region `idx`.
    #[inline]
    pub fn region_base(&self, idx: u64) -> usize {
        self.base + idx as usize * self.region_bytes
    }

    /// Allocated bytes of region `idx`: the bump extent inside it. The same
    /// measure `logical_pages` reports as a view's `used` and the selector
    /// reads as `capacity_bytes` (never the span: see `PageCandidate`).
    #[inline]
    pub fn used_in(&self, idx: u64) -> usize {
        self.end
            .saturating_sub(self.region_base(idx))
            .min(self.region_bytes)
    }
}

/// Per-region liveness at mark end: the input of [`select_relocation_regions`]
/// and the exact sizes of the forwarding tables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZRegionLiveness {
    /// Bytes of live objects STARTING in the region, whole size: the copy cost
    /// of evacuating it. The rule `logical_pages` uses (an object is booked to
    /// the page of its base), so the two selections agree.
    pub live_bytes: Vec<usize>,
    /// Live objects starting in the region: its forwarding table's entry count.
    pub live_objects: Vec<usize>,
    /// Bytes of the region covered by live objects that START in an earlier
    /// region. Nonzero means freeing the region whole would free part of a
    /// live object; [`select_relocation_regions`] never selects it.
    pub straddle_in_bytes: Vec<usize>,
    /// Objects whose base lies outside the grid (the high end's large objects,
    /// or a base the grid does not cover). Not booked anywhere.
    pub outside: usize,
    /// Objects reported smaller than a header (a corrupt size). Not booked:
    /// the region they sit in is then under-counted, which can only make it
    /// look sparser -- so they also count as a reason for the caller to refuse
    /// the cycle's selection; see [`ZRegionSelection::trustworthy`].
    pub unsizable: usize,
}

/// Book every live object `(base, size)` onto `grid`.
///
/// A pure function of the mark's survivors: on the production heap the
/// survivor list (`live`, ascending bases) and its size cells, available at
/// mark end before any sweep. Order does not matter.
pub fn region_liveness<I>(grid: &ZRegionGrid, objects: I) -> ZRegionLiveness
where
    I: IntoIterator<Item = (usize, usize)>,
{
    let n = grid.region_count();
    let mut out = ZRegionLiveness {
        live_bytes: vec![0; n],
        live_objects: vec![0; n],
        straddle_in_bytes: vec![0; n],
        outside: 0,
        unsizable: 0,
    };
    for (addr, size) in objects {
        let Some(idx) = grid.region_of(addr) else {
            out.outside += 1;
            continue;
        };
        if size < HEADER_SIZE {
            out.unsizable += 1;
            continue;
        }
        let i = idx as usize;
        out.live_bytes[i] += size;
        out.live_objects[i] += 1;
        // The object's tail in later regions, clipped to the grid.
        let obj_end = addr.saturating_add(size);
        let mut r = i + 1;
        while r < n {
            let r_base = grid.region_base(r as u64);
            if r_base >= obj_end {
                break;
            }
            let r_end = r_base + grid.region_bytes();
            out.straddle_in_bytes[r] += obj_end.min(r_end) - r_base;
            r += 1;
        }
    }
    out
}

/// What [`select_relocation_regions`] chose.
#[derive(Debug, Clone, Default)]
pub struct ZRegionSelection {
    /// The regions to relocate, from the existing selector (page id = region
    /// index).
    pub set: ZRelocationSet,
    /// Regions with allocated bytes and no live object: freed at selection
    /// without a forwarding table, as HotSpot frees its empty pages.
    pub empty: Vec<u64>,
    /// Regions the caller's `excluded` predicate kept out (raw holders,
    /// allocation targets).
    pub excluded: usize,
    /// Regions kept out because a live object from an earlier region reaches
    /// into them.
    pub straddled: usize,
    /// `ZRegionLiveness::unsizable` carried over.
    pub unsizable: usize,
}

impl ZRegionSelection {
    /// False when the liveness had a corrupt size: the caller should relocate
    /// nothing this cycle rather than act on an under-count.
    pub fn trustworthy(&self) -> bool {
        self.unsizable == 0
    }
}

/// Choose the regions to relocate from mark-end liveness.
///
/// Per region with allocated bytes, in this order: `excluded(idx)` keeps it
/// out (counted); a live straddle-in keeps it out (counted); no live object
/// makes it EMPTY (listed, no table); otherwise it is a candidate with
/// `live_bytes` as cost and `used_in` as capacity. The candidates go through
/// [`ZRelocationSet::select`] under `policy` unchanged, so the occupancy
/// cutoff, ranking and budget are the existing selector's.
pub fn select_relocation_regions<F>(
    grid: &ZRegionGrid,
    live: &ZRegionLiveness,
    policy: &ZRelocationPolicy,
    mut excluded: F,
) -> ZRegionSelection
where
    F: FnMut(u64) -> bool,
{
    let n = grid
        .region_count()
        .min(live.live_bytes.len())
        .min(live.live_objects.len())
        .min(live.straddle_in_bytes.len());
    let mut out = ZRegionSelection {
        unsizable: live.unsizable,
        ..ZRegionSelection::default()
    };
    let mut candidates: Vec<PageCandidate> = Vec::with_capacity(n);
    for i in 0..n {
        let idx = i as u64;
        let used = grid.used_in(idx);
        if used == 0 {
            continue;
        }
        if excluded(idx) {
            out.excluded += 1;
            continue;
        }
        if live.straddle_in_bytes[i] > 0 {
            out.straddled += 1;
            continue;
        }
        if live.live_objects[i] == 0 {
            out.empty.push(idx);
            continue;
        }
        candidates.push(PageCandidate {
            page_id: idx,
            live_bytes: live.live_bytes[i],
            capacity_bytes: used,
            size_class_index: ZFWD_SIZE_CLASS_SMALL,
        });
    }
    out.set = ZRelocationSet::select(&candidates, policy);
    out
}

// ===========================================================================
// Per-region forwarding
// ===========================================================================

/// Set once the region's worker has relocated every live object: no new
/// [`ZRegionForwarding::retain`] succeeds after it.
const ZRF_SEALED: u64 = 1 << 63;
/// The in-flight copier count.
const ZRF_COUNT_MASK: u64 = u32::MAX as u64;

/// Why [`ZRegionForwarding::relocate_or_forward`] produced no address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZRegionRelocateError {
    /// `from` is not an 8-byte-aligned address inside this region, or `size`
    /// is below a header. A caller bug.
    BadObject,
    /// The to-space allocator refused `size` bytes. HotSpot relocates the
    /// region in place in this case; that arm is not built (see the proposal).
    AllocFailed,
    /// The destination does not encode (below the arena base, misaligned or
    /// past the payload field).
    Unencodable,
    /// The region is sealed and has no entry for `from`: its worker sealed it
    /// without relocating this object, i.e. `from` was not live at mark end
    /// (a stale or dead reference) or the worker skipped it. A driver defect.
    SealedWithoutEntry,
    /// The table refused the install.
    Table(ZForwardingInsertError),
}

/// How [`ZRegionForwarding::relocate_or_forward`] answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZRegionRelocated {
    /// Someone had already relocated the object: no copy was made.
    Forwarded(usize),
    /// This call's copy won the install.
    Relocated(usize),
    /// This call copied, lost the install, and must use `winner`. `abandoned`
    /// (`size` bytes) is unreferenced to-space; the caller may undo the
    /// allocation if its allocator can, else the next cycle reclaims it.
    LostRace {
        /// The address every caller now agrees on.
        winner: usize,
        /// This call's discarded copy.
        abandoned: usize,
    },
}

impl ZRegionRelocated {
    /// The object's one new address, whichever arm.
    #[inline]
    pub fn address(self) -> usize {
        match self {
            Self::Forwarded(a) | Self::Relocated(a) => a,
            Self::LostRace { winner, .. } => winner,
        }
    }
}

/// One selected region's forwarding: the lock-free table plus the in-flight
/// copier count and the worker claim.
pub struct ZRegionForwarding {
    region: u64,
    region_base: usize,
    region_bytes: usize,
    arena_base: usize,
    live_objects: usize,
    live_bytes: usize,
    table: ZForwardingTable,
    /// `ZRF_SEALED | in-flight count`.
    state: AtomicU64,
    /// A GC worker owns the region's drain.
    claimed: AtomicBool,
    /// Installs this region won (objects relocated) and their bytes.
    won: AtomicUsize,
    won_bytes: AtomicUsize,
    /// Installs lost to another copier, and the bytes of the discarded copies.
    lost: AtomicUsize,
    abandoned_bytes: AtomicUsize,
}

impl ZRegionForwarding {
    /// A forwarding for region `region` of `grid`, sized for `live_objects`
    /// entries (the mark's exact count; the table runs at load factor 0.5).
    pub fn new(grid: &ZRegionGrid, region: u64, live_objects: usize, live_bytes: usize) -> Self {
        Self {
            region,
            region_base: grid.region_base(region),
            region_bytes: grid.region_bytes(),
            arena_base: grid.base(),
            live_objects,
            live_bytes,
            table: ZForwardingTable::for_page(region, live_objects),
            state: AtomicU64::new(0),
            claimed: AtomicBool::new(false),
            won: AtomicUsize::new(0),
            won_bytes: AtomicUsize::new(0),
            lost: AtomicUsize::new(0),
            abandoned_bytes: AtomicUsize::new(0),
        }
    }

    /// The region index (the selector's page id).
    #[inline]
    pub fn region(&self) -> u64 {
        self.region
    }

    /// `[start, end)` of the region.
    #[inline]
    pub fn span(&self) -> std::ops::Range<usize> {
        self.region_base..self.region_base + self.region_bytes
    }

    /// Live objects / bytes the mark found here.
    #[inline]
    pub fn live(&self) -> (usize, usize) {
        (self.live_objects, self.live_bytes)
    }

    /// The table's slot count.
    #[inline]
    pub fn table_capacity(&self) -> usize {
        self.table.capacity()
    }

    /// `(installs won, bytes won, installs lost, bytes abandoned)`.
    pub fn stats(&self) -> (usize, usize, usize, usize) {
        (
            self.won.load(Ordering::Relaxed),
            self.won_bytes.load(Ordering::Relaxed),
            self.lost.load(Ordering::Relaxed),
            self.abandoned_bytes.load(Ordering::Relaxed),
        )
    }

    /// The region-relative key of `from`, or `None` for an address outside
    /// the region or off the 8-byte grid.
    #[inline]
    fn key_of(&self, from: usize) -> Option<u64> {
        if !self.span().contains(&from) || !from.is_multiple_of(ZFWD_ALIGN_BYTES as usize) {
            return None;
        }
        Some((from - self.region_base) as u64)
    }

    /// Arena-relative destination plus [`ZRELOC_ENCODING_BIAS`].
    pub fn encode_to(&self, to: usize) -> Option<ZForwardingPayload> {
        let rel = to.checked_sub(self.arena_base)?;
        let enc = rel.checked_add(ZRELOC_ENCODING_BIAS)? as u64;
        if !enc.is_multiple_of(ZFWD_ALIGN_BYTES) || enc > ZFWD_MAX_PAYLOAD {
            return None;
        }
        Some(ZForwardingPayload::from_encoded(enc))
    }

    /// The inverse of [`Self::encode_to`]. The only decoder of this domain.
    #[inline]
    fn decode_to(&self, payload: ZForwardingPayload) -> usize {
        (payload.encoded() as usize) - ZRELOC_ENCODING_BIAS + self.arena_base
    }

    /// The object's new address if any copier has installed one. Lock-free;
    /// needs no guard (the table outlives every lookup through the cycle's
    /// `Arc`).
    pub fn forwarded(&self, from: usize) -> Option<usize> {
        let key = self.key_of(from)?;
        self.table.find_payload(key).map(|p| self.decode_to(p))
    }

    /// zt t23/zc7: every installed `(from, to)` of the region, in table slot
    /// order (not address order). No allocation.
    pub fn for_each_pair(&self, mut f: impl FnMut(usize, usize)) {
        self.table
            .for_each_payload(|key, p| f(self.region_base + key as usize, self.decode_to(p)));
    }

    /// Enter as a copier. `None` once the region is sealed (every live object
    /// already has an entry: look it up instead). The guard releases on drop,
    /// panic or not, so a copier that unwinds cannot hold the region forever.
    pub fn retain(&self) -> Option<ZRegionGuard<'_>> {
        let mut s = self.state.load(Ordering::Acquire);
        loop {
            if s & ZRF_SEALED != 0 || s & ZRF_COUNT_MASK == ZRF_COUNT_MASK {
                return None;
            }
            match self
                .state
                .compare_exchange_weak(s, s + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(ZRegionGuard { fwd: self }),
                Err(now) => s = now,
            }
        }
    }

    /// Copiers inside the region now.
    #[inline]
    pub fn in_flight(&self) -> usize {
        (self.state.load(Ordering::Acquire) & ZRF_COUNT_MASK) as usize
    }

    /// Refuse new copiers. Call once the region's worker has installed an
    /// entry for every live object. Returns the copiers still inside; the
    /// region may be freed only when [`Self::is_quiescent`].
    pub fn seal(&self) -> usize {
        let prev = self.state.fetch_or(ZRF_SEALED, Ordering::AcqRel);
        (prev & ZRF_COUNT_MASK) as usize
    }

    /// Whether the region is sealed.
    #[inline]
    pub fn is_sealed(&self) -> bool {
        self.state.load(Ordering::Acquire) & ZRF_SEALED != 0
    }

    /// Sealed and no copier inside: nobody reads from-space here any more.
    #[inline]
    pub fn is_quiescent(&self) -> bool {
        self.state.load(Ordering::Acquire) == ZRF_SEALED
    }

    /// Claim the region's drain for one GC worker. True exactly once.
    pub fn claim(&self) -> bool {
        self.claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Whether a worker claimed the drain.
    #[inline]
    pub fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }

    /// Relocate `from` (`size` bytes) or answer where it already went.
    ///
    /// Copy-then-install, HotSpot's order: look up; on a miss enter as a copier
    /// (a sealed region must already have the entry), `alloc(size)` a
    /// destination, `copy(from, to, size)`, then install with one CAS. The
    /// install returns the winner, and every caller -- mutator or worker --
    /// leaves with that one address, so the object has one identity however
    /// many threads raced. The table's release CAS orders the copy before the
    /// entry; a reader's acquire on the entry sees the copied bytes.
    pub fn relocate_or_forward<A, C>(
        &self,
        from: usize,
        size: usize,
        alloc: A,
        copy: C,
    ) -> Result<ZRegionRelocated, ZRegionRelocateError>
    where
        A: FnOnce(usize) -> Option<usize>,
        C: FnOnce(usize, usize, usize),
    {
        let key = self.key_of(from).ok_or(ZRegionRelocateError::BadObject)?;
        if size < HEADER_SIZE {
            return Err(ZRegionRelocateError::BadObject);
        }
        if let Some(p) = self.table.find_payload(key) {
            return Ok(ZRegionRelocated::Forwarded(self.decode_to(p)));
        }
        let Some(_guard) = self.retain() else {
            // Sealed between the lookup and the retain: the entry exists now,
            // or never will.
            return match self.table.find_payload(key) {
                Some(p) => Ok(ZRegionRelocated::Forwarded(self.decode_to(p))),
                None => Err(ZRegionRelocateError::SealedWithoutEntry),
            };
        };
        let to = alloc(size).ok_or(ZRegionRelocateError::AllocFailed)?;
        let Some(payload) = self.encode_to(to) else {
            return Err(ZRegionRelocateError::Unencodable);
        };
        copy(from, to, size);
        let winner = self
            .table
            .try_insert(key, payload)
            .map_err(ZRegionRelocateError::Table)?;
        if winner == payload {
            self.won.fetch_add(1, Ordering::Relaxed);
            self.won_bytes.fetch_add(size, Ordering::Relaxed);
            Ok(ZRegionRelocated::Relocated(to))
        } else {
            self.lost.fetch_add(1, Ordering::Relaxed);
            self.abandoned_bytes.fetch_add(size, Ordering::Relaxed);
            Ok(ZRegionRelocated::LostRace {
                winner: self.decode_to(winner),
                abandoned: to,
            })
        }
    }
}

impl std::fmt::Debug for ZRegionForwarding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZRegionForwarding")
            .field("region", &self.region)
            .field("live_objects", &self.live_objects)
            .field("in_flight", &self.in_flight())
            .field("sealed", &self.is_sealed())
            .finish()
    }
}

/// A copier's hold on a region (see [`ZRegionForwarding::retain`]).
pub struct ZRegionGuard<'a> {
    fwd: &'a ZRegionForwarding,
}

impl Drop for ZRegionGuard<'_> {
    fn drop(&mut self) {
        let prev = self.fwd.state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            prev & ZRF_COUNT_MASK != 0,
            "ZRegionGuard released a region with no copier inside"
        );
    }
}

// ===========================================================================
// The cycle's region map
// ===========================================================================

const ZRC_NOT_SELECTED: u32 = u32::MAX;

/// One cycle's relocation set as forwardings, with an O(1), lock-free
/// address -> forwarding map. Built before relocation starts and immutable
/// afterwards; shared by `Arc` with every copier and every barrier slow path,
/// and kept until the next mark has remapped every stale reference.
pub struct ZRelocationCycle {
    grid: ZRegionGrid,
    /// Per grid region: index into `regions`, or [`ZRC_NOT_SELECTED`].
    slot_of_region: Box<[u32]>,
    regions: Box<[ZRegionForwarding]>,
}

impl ZRelocationCycle {
    /// One forwarding per region of `set`, each sized from `live`'s object
    /// count for that region. Regions of `set` outside the grid or `live` are
    /// skipped (never invented).
    pub fn build(grid: &ZRegionGrid, live: &ZRegionLiveness, set: &ZRelocationSet) -> Self {
        let n = grid.region_count();
        let mut slot_of_region = vec![ZRC_NOT_SELECTED; n];
        let mut regions: Vec<ZRegionForwarding> = Vec::with_capacity(set.len());
        for p in set.pages() {
            let i = p.page_id as usize;
            if i >= n || i >= live.live_objects.len() || slot_of_region[i] != ZRC_NOT_SELECTED {
                continue;
            }
            slot_of_region[i] = regions.len() as u32;
            regions.push(ZRegionForwarding::new(
                grid,
                p.page_id,
                live.live_objects[i],
                live.live_bytes[i],
            ));
        }
        Self {
            grid: *grid,
            slot_of_region: slot_of_region.into_boxed_slice(),
            regions: regions.into_boxed_slice(),
        }
    }

    /// zt t23/zc5 (R2): one forwarding per region id of `regions`, IN THAT
    /// ORDER, each sized from `live`. [`Self::claim_next`] hands regions out in
    /// this order, so a caller that passes ascending ids drains from-space in
    /// address order (the stop-the-world arm relies on it to copy in exactly
    /// the order the evacuating slide does). Ids outside the grid or `live`,
    /// and repeats, are skipped (never invented).
    pub fn build_regions(grid: &ZRegionGrid, live: &ZRegionLiveness, regions: &[u64]) -> Self {
        let n = grid.region_count();
        let mut slot_of_region = vec![ZRC_NOT_SELECTED; n];
        let mut out: Vec<ZRegionForwarding> = Vec::with_capacity(regions.len());
        for &id in regions {
            let i = id as usize;
            if i >= n || i >= live.live_objects.len() || slot_of_region[i] != ZRC_NOT_SELECTED {
                continue;
            }
            slot_of_region[i] = out.len() as u32;
            out.push(ZRegionForwarding::new(
                grid,
                id,
                live.live_objects[i],
                live.live_bytes[i],
            ));
        }
        Self {
            grid: *grid,
            slot_of_region: slot_of_region.into_boxed_slice(),
            regions: out.into_boxed_slice(),
        }
    }

    /// The grid the cycle was built over.
    #[inline]
    pub fn grid(&self) -> &ZRegionGrid {
        &self.grid
    }

    /// The selected regions' forwardings.
    #[inline]
    pub fn regions(&self) -> &[ZRegionForwarding] {
        &self.regions
    }

    /// The forwarding of the region `addr` lies in, `None` when that region is
    /// not being relocated (the object did not move).
    #[inline]
    pub fn forwarding_for(&self, addr: usize) -> Option<&ZRegionForwarding> {
        let idx = self.grid.region_of(addr)? as usize;
        let slot = *self.slot_of_region.get(idx)?;
        if slot == ZRC_NOT_SELECTED {
            return None;
        }
        self.regions.get(slot as usize)
    }

    /// Where `addr` went, if it is in the set and has been relocated.
    #[inline]
    pub fn forwarded(&self, addr: usize) -> Option<usize> {
        self.forwarding_for(addr)?.forwarded(addr)
    }

    /// The next region no worker has claimed, claimed for the caller.
    pub fn claim_next(&self) -> Option<&ZRegionForwarding> {
        self.regions.iter().find(|r| r.claim())
    }

    /// Every region sealed with no copier inside: the from-space may go to the
    /// quarantine (it is freed for reuse only after the next mark remapped
    /// every reference into it).
    pub fn all_quiescent(&self) -> bool {
        self.regions.iter().all(|r| r.is_quiescent())
    }

    /// zt t23/zc7: installs won across the cycle (objects relocated).
    pub fn installed(&self) -> usize {
        self.regions.iter().map(|r| r.stats().0).sum()
    }

    /// zt t23/zc7: every installed `(from, to)`, region by region in the
    /// cycle's order, each region's in table slot order. No allocation.
    pub fn for_each_pair(&self, mut f: impl FnMut(usize, usize)) {
        for r in self.regions.iter() {
            r.for_each_pair(&mut f);
        }
    }

    /// zt t23/zc7: the bytes the cycle holds: 8 per table slot plus 4 per
    /// grid region of the region map (the `ZRegionForwarding` headers aside).
    pub fn table_bytes(&self) -> usize {
        self.regions
            .iter()
            .map(|r| r.table_capacity() * std::mem::size_of::<u64>())
            .sum::<usize>()
            + self.slot_of_region.len() * std::mem::size_of::<u32>()
    }
}

impl std::fmt::Debug for ZRelocationCycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZRelocationCycle")
            .field("grid", &self.grid)
            .field("regions", &self.regions.len())
            .finish()
    }
}

// ===========================================================================
// zt t23/zc5 -- R1: selection at mark end, as a census
// ===========================================================================

/// `CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS` (zt t23/zc5, R1; diagnostic `=1`):
/// one `[GC] zgc-rsel:` line per whole-heap ZGC cycle that compares the
/// mark-end selection ([`reloc_select_census`]) with the slide's own
/// selection on the same cycle. It reads headers and mark bits and changes
/// nothing. Read once per cycle.
pub(super) fn zgc_dbg_reloc_select_census_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS")
}

/// One cycle's R1 census: what [`select_relocation_regions`] chose from the
/// mark's survivors before the sweep, and, once the slide ran on the same
/// cycle, what the slide chose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZRelocSelectCensus {
    /// The collection this census belongs to (`gc_count + 1` when taken).
    pub cycle: u64,
    /// Grid regions with allocated bytes: the agreement's denominator.
    pub regions: usize,
    /// The regions R1 selected, ascending.
    pub set: Vec<u64>,
    /// Live bytes of `set`.
    pub set_live_bytes: usize,
    /// Regions with allocated bytes and no live object, ascending (freed
    /// whole, no table).
    pub empty: Vec<u64>,
    /// [`ZRegionSelection::straddled`].
    pub straddled: usize,
    /// [`ZRegionSelection::excluded`] (regions holding a pin word or a VM TLAB
    /// tail end, the same words the slide's pin screen reads).
    pub excluded: usize,
    /// [`ZRegionLiveness::unsizable`].
    pub unsizable: usize,
    /// [`ZRegionLiveness::outside`] (survivors of the large-object end).
    pub outside: usize,
    /// Microseconds the census took, inside the pause.
    pub us: u64,
    /// The slide's selection on the same cycle, after its pin screen and
    /// before its cycle gates (ascending ids), with the region count of the
    /// slide's own grid (its post-sweep cursor). `None` when no slide selected
    /// on this cycle: refused, young-skipped, deferred-sweep and
    /// relocation-off cycles.
    pub slide: Option<(Vec<u64>, usize)>,
}

/// How R1 and the slide agree on one cycle ([`ZRelocSelectCensus::agreement`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZRelocSelectAgreement {
    /// The slide selected on this cycle, so the counts below mean something.
    pub compared: bool,
    /// Regions `0..regions` on which both answer the same.
    pub agree: usize,
    /// Regions R1 reclaims and the slide does not.
    pub r1_only: usize,
    /// Regions the slide reclaims and R1 does not, plus slide ids at or above
    /// R1's grid.
    pub slide_only: usize,
    /// Regions of R1's grid at or above the slide's (the sweep retracted the
    /// cursor below them, so the slide's grid never saw them).
    pub retracted: usize,
}

impl ZRelocSelectCensus {
    /// Region by region over R1's grid. R1 reclaims a region when it is in
    /// `set` or in `empty`: the slide's selector admits a page with no
    /// survivor as its most profitable candidate (garbage ratio 1), and R1
    /// frees it whole, so both reclaim it. The slide reclaims a region when it
    /// selected it, or when the region lies at or above the slide's own grid:
    /// the sweep retracted the cursor below it, which frees it whole too.
    pub fn agreement(&self) -> ZRelocSelectAgreement {
        let Some((slide, slide_regions)) = self.slide.as_ref() else {
            return ZRelocSelectAgreement::default();
        };
        let mut out = ZRelocSelectAgreement {
            compared: true,
            ..ZRelocSelectAgreement::default()
        };
        for id in 0..self.regions as u64 {
            let r1_in =
                self.set.binary_search(&id).is_ok() || self.empty.binary_search(&id).is_ok();
            let slide_in = if id as usize >= *slide_regions {
                out.retracted += 1;
                true
            } else {
                slide.binary_search(&id).is_ok()
            };
            match (r1_in, slide_in) {
                (true, false) => out.r1_only += 1,
                (false, true) => out.slide_only += 1,
                _ => out.agree += 1,
            }
        }
        out.slide_only += slide
            .iter()
            .filter(|&&id| id >= self.regions as u64)
            .count();
        out
    }

    /// The `[GC] zgc-rsel:` line. Every key carries the `rsel_` prefix.
    pub fn line(&self, young_skipped: usize) -> String {
        let a = self.agreement();
        format!(
            "[GC] zgc-rsel: rsel_cycle={} rsel_regions={} rsel_set={} rsel_set_live_kb={} \
             rsel_empty={} rsel_straddled={} rsel_excluded={} rsel_unsizable={} rsel_outside={} \
             rsel_compared={} rsel_slide_set={} rsel_agree={} rsel_r1_only={} rsel_slide_only={} \
             rsel_retracted={} rsel_us={} rsel_young_skipped={}",
            self.cycle,
            self.regions,
            self.set.len(),
            self.set_live_bytes / 1024,
            self.empty.len(),
            self.straddled,
            self.excluded,
            self.unsizable,
            self.outside,
            u8::from(a.compared),
            self.slide.as_ref().map_or(0, |(s, _)| s.len()),
            a.agree,
            a.r1_only,
            a.slide_only,
            a.retracted,
            self.us,
            young_skipped,
        )
    }
}

/// R1 as a pure function: book `objects` (the mark's survivors, `(base,
/// size)`) onto `grid`, select under `policy` with `excluded` kept out, and
/// return the census (no slide side yet, `cycle` and `us` zero).
pub fn reloc_select_census<I, F>(
    grid: &ZRegionGrid,
    objects: I,
    policy: &ZRelocationPolicy,
    excluded: F,
) -> ZRelocSelectCensus
where
    I: IntoIterator<Item = (usize, usize)>,
    F: FnMut(u64) -> bool,
{
    let live = region_liveness(grid, objects);
    let sel = select_relocation_regions(grid, &live, policy, excluded);
    let mut set = sel.set.page_ids();
    set.sort_unstable();
    ZRelocSelectCensus {
        cycle: 0,
        regions: grid.region_count(),
        set,
        set_live_bytes: sel.set.total_live_bytes(),
        empty: sel.empty,
        straddled: sel.straddled,
        excluded: sel.excluded,
        unsizable: sel.unsizable,
        outside: live.outside,
        us: 0,
        slide: None,
    }
}

// ===========================================================================
// zt t23/zc5 -- R2: the stop-the-world relocation through the cycle
// ===========================================================================

/// `CRATONVM_ZGC_REGION_RELOCATE` (zt t23/zc5, R2; opt-in `=1`): the low
/// slide's evacuating arm copies its survivors through a [`ZRelocationCycle`]
/// ([`drain_stw`]) instead of its own loop. It asks for the evacuating arm by
/// itself (`CRATONVM_ZGC_SLIDE_EVACUATE` need not be set); a cycle that arm's
/// gate refuses (allocation failure, pressure tier, no tail headroom,
/// vacated publication off) slides as before. Unset or `=0`: today's slide,
/// byte for byte. Read once per slide.
pub(super) fn zgc_region_relocate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_REGION_RELOCATE")
}

/// What R3 does with a kept cycle's pairs when it is released (see
/// [`zgc_region_quarantine_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZRegionQuarantine {
    /// `=1` (and every other on-word): at the release the cycle's pairs go
    /// into the slide's `relocations` table, the fallback that answered a
    /// stale address held across the next pause before R3. Kept until R4
    /// heals every such address in the mark.
    Handover,
    /// `=2`: no hand-over. The cycle's pairs never enter `relocations`, so
    /// an address held across the next pause loses its answer at the
    /// release. R4's shape, for the rows that measure what R3 saves
    /// (`fp_relocations_peak`) and how many stale words it would cost
    /// (`rquar_stale_answered=`).
    NoHandover,
}

/// `CRATONVM_ZGC_REGION_QUARANTINE` (zt t23/zc7, R3; opt-in): the region
/// relocation cycle and its from-space outlive the closing pause. The
/// evacuating arm withholds the vacated spans exactly as
/// `CRATONVM_ZGC_EVAC_QUARANTINE=1` does; the cycle answers
/// `forwarded_after_slide` for the mutator interval, so the slide does not
/// publish the pairs it vouches for into `relocations`; both are released
/// at the next collection's mark end (`release_evac_quarantine`). Acts only
/// with `CRATONVM_ZGC_REGION_RELOCATE=1` (no cycle otherwise). Unset, empty
/// or an off-word: off. `=2`: [`ZRegionQuarantine::NoHandover`]; any other
/// on-word: [`ZRegionQuarantine::Handover`]. Read once per slide.
pub(super) fn zgc_region_quarantine_mode() -> Option<ZRegionQuarantine> {
    let raw = cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_REGION_QUARANTINE")?;
    let v = raw.to_string_lossy().trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "0" | "false" | "off" | "no" => None,
        "2" | "nohandover" => Some(ZRegionQuarantine::NoHandover),
        _ => Some(ZRegionQuarantine::Handover),
    }
}

/// zt t23/zc7 (R3): a cycle kept across the mutator interval.
#[derive(Debug)]
pub struct ZKeptRegionCycle {
    /// The drained cycle (every region sealed and quiescent).
    pub cycle: std::sync::Arc<ZRelocationCycle>,
    /// What the release does with its pairs.
    pub mode: ZRegionQuarantine,
    /// Its installs, booked when kept.
    pub pairs: usize,
}

/// What [`drain_stw`] did. The four lists are exactly what the evacuating
/// slide's own loop produces, in the same order, so everything after the copy
/// (the screen, the vacated spans, the rewrite, the registry rebuild, the
/// forwarding words, the `PointerMap`) runs unchanged on them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZRegionDrain {
    /// `(from, to)` per relocated survivor, ascending by `from`, each one the
    /// cycle's table vouches for (see `xcheck_bad`).
    pub pairs: Vec<(usize, usize)>,
    /// `(from, size)` per relocated survivor, ascending by `from`.
    pub moves: Vec<(usize, usize)>,
    /// `(from, end)` per survivor left in place; an unsizable one ends at the
    /// grid's end (fail-closed, as the slide's screen does).
    pub stays: Vec<(usize, usize)>,
    /// Bytes copied.
    pub moved_bytes: usize,
    /// Regions the cycle held (one forwarding table each).
    pub regions: usize,
    /// The drain stopped copying (on entry, at an unsizable survivor, at a
    /// to-space stop or at a refusal); every later survivor stayed.
    pub halted: bool,
    /// To-space allocations refused (the slide's `evac=` third field).
    pub alloc_stops: usize,
    /// `relocate_or_forward` answers other than a fresh copy or a to-space
    /// stop: a bad object, an unencodable destination, a full table, or a
    /// survivor listed twice. 0 on a healthy heap.
    pub refusals: usize,
    /// Copies whose address the cycle's table does not return afterwards.
    /// 0 by construction; the pair keeps the address the copy was made to.
    pub xcheck_bad: usize,
}

/// Drain one stop-the-world cycle over `survivors` (`(from, size)`,
/// ascending by `from`, every `from` inside `grid`; `size` `None` = the header
/// cannot be sized).
///
/// The cycle holds one forwarding per region that has a survivor, built in
/// ascending region order and sized from the survivors' exact count, so
/// [`ZRelocationCycle::claim_next`] hands regions out in address order and the
/// copies happen in the order the evacuating slide's loop makes them. Per
/// survivor, the slide's own rules: once `halted` every survivor stays; an
/// unsizable one stays with an end of `grid.end()` and halts; one above
/// `max_extent` stays; otherwise [`ZRegionForwarding::relocate_or_forward`]
/// with `alloc` / `copy`, a refused allocation halting. Each region is sealed
/// once its survivors are done. The pairs are then read back through
/// [`ZRelocationCycle::forwarded`].
pub fn drain_stw<A, C>(
    grid: &ZRegionGrid,
    survivors: &[(usize, Option<usize>)],
    halted_on_entry: bool,
    max_extent: usize,
    alloc: A,
    copy: C,
) -> ZRegionDrain
where
    A: FnMut(usize) -> Option<usize>,
    C: FnMut(usize, usize, usize),
{
    drain_stw_keep(grid, survivors, halted_on_entry, max_extent, alloc, copy).0
}

/// zt t23/zc7: [`drain_stw`], also handing back the cycle it drained (every
/// region sealed and quiescent), for a caller that reads it after the drain:
/// the slide's forward index and, under R3, the stale-address answers of the
/// mutator interval that follows.
pub fn drain_stw_keep<A, C>(
    grid: &ZRegionGrid,
    survivors: &[(usize, Option<usize>)],
    halted_on_entry: bool,
    max_extent: usize,
    mut alloc: A,
    mut copy: C,
) -> (ZRegionDrain, ZRelocationCycle)
where
    A: FnMut(usize) -> Option<usize>,
    C: FnMut(usize, usize, usize),
{
    let low_end = grid.end();
    let mut out = ZRegionDrain {
        halted: halted_on_entry,
        ..ZRegionDrain::default()
    };
    debug_assert!(
        survivors.windows(2).all(|w| w[0].0 < w[1].0),
        "drain_stw: survivors must ascend strictly by base"
    );
    let live = region_liveness(
        grid,
        survivors.iter().filter_map(|&(f, s)| s.map(|s| (f, s))),
    );
    let mut regions: Vec<u64> = survivors
        .iter()
        .filter_map(|&(f, _)| grid.region_of(f))
        .collect();
    regions.sort_unstable();
    regions.dedup();
    let cycle = ZRelocationCycle::build_regions(grid, &live, &regions);
    out.regions = cycle.regions().len();
    let mut copied: Vec<(usize, usize)> = Vec::with_capacity(survivors.len());
    let mut i = 0usize;
    while let Some(fwd) = cycle.claim_next() {
        let span_end = fwd.span().end;
        while i < survivors.len() && survivors[i].0 < span_end {
            let (from, size) = survivors[i];
            i += 1;
            if out.halted {
                out.stays.push((from, size.map_or(low_end, |s| from + s)));
                continue;
            }
            let Some(size) = size else {
                out.stays.push((from, low_end));
                out.halted = true;
                continue;
            };
            if size > max_extent {
                out.stays.push((from, from + size));
                continue;
            }
            match fwd.relocate_or_forward(from, size, |s| alloc(s), |f, t, s| copy(f, t, s)) {
                Ok(ZRegionRelocated::Relocated(to)) => {
                    copied.push((from, to));
                    out.moves.push((from, size));
                    out.moved_bytes += size;
                }
                // One copier and distinct survivors: an existing entry means
                // the survivor was listed twice. Its copy is already listed.
                Ok(ZRegionRelocated::Forwarded(_)) | Ok(ZRegionRelocated::LostRace { .. }) => {
                    out.refusals += 1;
                }
                Err(ZRegionRelocateError::AllocFailed) => {
                    out.alloc_stops += 1;
                    out.stays.push((from, from + size));
                    out.halted = true;
                }
                Err(_) => {
                    out.refusals += 1;
                    out.stays.push((from, from + size));
                    out.halted = true;
                }
            }
        }
        fwd.seal();
    }
    // Survivors past the last region (none when every base is inside the
    // grid, which the caller guarantees): they did not move.
    for &(from, size) in &survivors[i..] {
        out.refusals += 1;
        out.stays.push((from, size.map_or(low_end, |s| from + s)));
    }
    debug_assert!(cycle.all_quiescent(), "drain_stw: a region kept a copier");
    out.pairs.reserve_exact(copied.len());
    for (from, to) in copied {
        if cycle.forwarded(from) != Some(to) {
            out.xcheck_bad += 1;
        }
        out.pairs.push((from, to));
    }
    (out, cycle)
}

/// zt t23/zc5: the per-heap state of R1 and R2. Lives in `ZgcCounters`
/// (boxed), because the hot heap struct is size-ratcheted.
pub struct ZRegionRelocCounters {
    /// R1: the census of the cycle in flight. Set at the closing pause before
    /// the sweep, completed by the slide, taken by the emit.
    rsel_pending: Mutex<Option<ZRelocSelectCensus>>,
    /// R1: a census is pending, so the slide's note takes the lock only then.
    rsel_armed: AtomicBool,
    /// R1: young (floor-bounded) cycles the census skipped, cumulative.
    rsel_young_skipped: AtomicUsize,
    /// R2, cumulative: `[0]` slides drained through a cycle, `[1]` regions,
    /// `[2]` objects relocated, `[3]` bytes, `[4]` drains that halted, `[5]`
    /// cross-check mismatches (0 expected), `[6]` slides the switch asked for
    /// that the evacuating arm's gate kept on the slide, `[7]` drains refused
    /// whole (the arena base is off the 8-byte grid: no region grid).
    rreloc: [AtomicUsize; 8],
    /// zt t23/zc7 (R3): the cycle kept across the mutator interval, from the
    /// slide that drained it to the next mark end. One at a time.
    live_cycle: Mutex<Option<ZKeptRegionCycle>>,
    /// R3: `live_cycle` holds a cycle, so `forwarded_after_slide` takes the
    /// lock only then (one acquire load otherwise).
    live_cycle_armed: AtomicBool,
    /// R3, cumulative: `[0]` cycles kept, `[1]` pairs they held, `[2]` pairs
    /// the slides did NOT publish into `relocations` (the cycle answered
    /// them; equal to `[1]`), `[3]` stale-address lookups a live cycle
    /// answered, `[4]` cycles released, `[5]` pairs handed over to
    /// `relocations` at a release (`Handover` only), `[6]` cycles replaced
    /// by a later slide before any release (0 in production: every
    /// collection releases first), `[7]` stale words the dead-at-release
    /// census found that the released cycle answers.
    rquar: [AtomicUsize; 8],
}

impl ZRegionRelocCounters {
    /// Empty state: no census pending, no cycle kept, every count zero.
    pub fn new() -> Self {
        Self {
            rsel_pending: Mutex::new(None),
            rsel_armed: AtomicBool::new(false),
            rsel_young_skipped: AtomicUsize::new(0),
            rreloc: std::array::from_fn(|_| AtomicUsize::new(0)),
            live_cycle: Mutex::new(None),
            live_cycle_armed: AtomicBool::new(false),
            rquar: std::array::from_fn(|_| AtomicUsize::new(0)),
        }
    }

    /// R3's cumulative counts (see the field).
    pub fn rquar_stats(&self) -> [usize; 8] {
        std::array::from_fn(|i| self.rquar[i].load(Ordering::Relaxed))
    }

    /// R3: a cycle is kept right now.
    pub fn holds_cycle(&self) -> bool {
        self.live_cycle_armed.load(Ordering::Acquire)
    }

    /// R1: a census was taken this cycle and not yet printed.
    pub fn census_pending(&self) -> bool {
        self.rsel_armed.load(Ordering::Relaxed)
    }

    /// R2's cumulative counts (see the field).
    pub fn rreloc_stats(&self) -> [usize; 8] {
        std::array::from_fn(|i| self.rreloc[i].load(Ordering::Relaxed))
    }
}

impl Default for ZRegionRelocCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ZRegionRelocCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZRegionRelocCounters")
            .field("rsel_armed", &self.rsel_armed.load(Ordering::Relaxed))
            .field("rreloc", &self.rreloc_stats())
            .field("holds_cycle", &self.holds_cycle())
            .field("rquar", &self.rquar_stats())
            .finish()
    }
}

impl ZgcRealHeap {
    /// R1: the census, at the closing pause after the mark (and after the
    /// reference round and the finalizer resurrection: the mark bits are
    /// final) and BEFORE either sweep arm, so a cycle that defers its sweep
    /// (zt t21/zc2) is covered. A no-op unless
    /// `CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS=1`.
    ///
    /// Survivors are the registered bases whose mark bit is set, sized by
    /// `alloc_size` (a pure header read: `survivor_size` would bump the
    /// oversize counter, and the census must change nothing; an oversize
    /// survivor reads as unsizable here). The policy is the slide's: the
    /// evacuation budget and, when the slide's pressure tier would widen it,
    /// that occupancy. The tier's inputs are peeked, not consumed (the
    /// failure latch is the slide's to swap), and the bump headroom is the
    /// pre-sweep one. Excluded regions: every region holding a JNI critical
    /// pin, a conservative JIT root or either end of a published VM TLAB
    /// tail, the words the slide's pin screen drops pages for (it also drops
    /// the page of the object an interior pin lies in; R1 does not, which can
    /// read as `rsel_r1_only` on such a cycle).
    pub(super) fn reloc_select_census_begin(
        &self,
        registered: &ZObjectStartsSnapshot,
        sweep_floor: usize,
    ) {
        if !zgc_dbg_reloc_select_census_enabled() {
            return;
        }
        let st = &self.counters.region_reloc;
        if sweep_floor != 0 {
            // A young cycle marks only the nursery: the old objects below the
            // floor would read as dead. The slide does not run there either.
            st.rsel_young_skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let started = std::time::Instant::now();
        let (base, low_end, bump_free_permille) = {
            let arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            (
                base,
                base + arena.used_low_for_compaction(),
                arena.low_bump_headroom().saturating_mul(1000) / arena.capacity().max(1),
            )
        };
        let Some(grid) = ZRegionGrid::new(base, low_end, ZRELOC_REGION_BYTES) else {
            return;
        };
        let mut pins = self.critical_pin_addrs();
        pins.extend(crate::gc_quiescence::pinned_jit_roots_snapshot());
        for (tail_start, tail_end) in self.jit_tlab_skip_regions() {
            pins.push(tail_start);
            pins.push(tail_end.saturating_sub(1));
        }
        let mut excluded: Vec<u64> = pins.iter().filter_map(|&p| grid.region_of(p)).collect();
        excluded.sort_unstable();
        excluded.dedup();
        let mut objects: Vec<(usize, usize)> = Vec::new();
        registered.for_each_base_from(0, |b| {
            if !self.mark_is_set(b) {
                return;
            }
            let size = Self::alloc_size(self.header_ref(b as *mut u8)).unwrap_or(0);
            objects.push((b, size));
        });
        let mut policy = ZRelocationPolicy {
            max_evacuation_bytes: super::zgc_relocate_budget_bytes(),
            ..ZRelocationPolicy::default()
        };
        if let Some(tier) = super::relocate_pressure_tier(
            super::zgc_relocate_pressure_select_mode(),
            self.counters
                .cycle_after_alloc_failure
                .load(Ordering::Relaxed),
            bump_free_permille,
        ) {
            policy.max_live_occupancy = super::relocate_pressure_occupancy(tier);
        }
        let mut census = reloc_select_census(&grid, objects, &policy, |idx| {
            excluded.binary_search(&idx).is_ok()
        });
        census.cycle = self.gc_count.load(Ordering::Relaxed) as u64 + 1;
        census.us = started.elapsed().as_micros() as u64;
        *st.rsel_pending.lock() = Some(census);
        st.rsel_armed.store(true, Ordering::Relaxed);
    }

    /// R1: the slide's selection on this cycle (after its pin screen, before
    /// its cycle gates) and its grid's region count. One relaxed load when no
    /// census is pending.
    pub(super) fn reloc_select_census_note_slide(
        &self,
        selected: &FxHashSet<u64>,
        slide_regions: usize,
    ) {
        let st = &self.counters.region_reloc;
        if !st.rsel_armed.load(Ordering::Relaxed) {
            return;
        }
        let mut ids: Vec<u64> = selected.iter().copied().collect();
        ids.sort_unstable();
        if let Some(c) = st.rsel_pending.lock().as_mut() {
            c.slide = Some((ids, slide_regions));
        }
    }

    /// R1: print and drop the pending census. Called once per cycle at the
    /// end of the closing pause's relocation block, and before the early
    /// return of a cycle that defers its sweep. One relaxed swap when none is
    /// pending.
    pub(super) fn reloc_select_census_emit(&self) {
        let st = &self.counters.region_reloc;
        if !st.rsel_armed.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(census) = st.rsel_pending.lock().take() else {
            return;
        };
        eprintln!(
            "{}",
            census.line(st.rsel_young_skipped.load(Ordering::Relaxed))
        );
    }

    /// R2: the switch asked for the evacuating arm and its gate kept the slide.
    pub(super) fn note_region_relocate_fallback(&self) {
        self.counters.region_reloc.rreloc[6].fetch_add(1, Ordering::Relaxed);
    }

    /// R2: the evacuating slide's copy step through a [`ZRelocationCycle`].
    ///
    /// Called by `relocate_stw_admitted_hinted` in place of its evacuating
    /// loop, under the arena guard, with the to-space window it opened (or
    /// failed to open: `halted`), `survivors` = the selected pages' survivors
    /// with their size cells, ascending. Each survivor is sized exactly as
    /// that loop sizes it (`size_via_cell`), copied with
    /// `copy_nonoverlapping` into `Arena::alloc_to_space(size, 8)`, and the
    /// regions are claimed in ascending order, so the copies, their
    /// destinations and the stay list are the loop's, element for element.
    /// Books the slide's `evac=` third field for a to-space stop, as the loop
    /// does.
    ///
    /// WHAT IS FED FROM THE CYCLE. Every copy goes through the cycle's
    /// copy-then-install, each region is sealed and the cycle is quiescent at
    /// the end, and every pair handed on is one `cycle.forwarded` answers
    /// (`xcheck_bad` counts a disagreement, 0 by construction). zt t23/zc7:
    /// the cycle is returned (`None` when there is no grid, or when a single
    /// pair disagrees, so a doubtful cycle is never the source of an answer)
    /// and the slide reads it: the forward index asks it first, so the
    /// rewrite, the walkability screen and the verifier resolve the low end's
    /// moves through `cycle.forwarded` (`ZForwardIndex::for_slide`); the registry
    /// rebuild re-registers the cycle's installs from its own tables; and
    /// under R3 it is kept across the mutator interval
    /// ([`Self::keep_region_cycle`]). The `PointerMap` (the VM's monitors,
    /// JNI handles and reference rows) still reads the pair list: inverting
    /// that interface so each external table asks the cycle is R5. The
    /// forwarding words have nothing to read on this arm (the evacuating
    /// slide's stampable tail is empty: its cursor does not drop).
    pub(super) fn region_relocate_evacuate(
        &self,
        arena: &mut Arena,
        base: usize,
        low_end: usize,
        survivors: &[(usize, u32)],
        halted: bool,
    ) -> (ZRegionDrain, Option<std::sync::Arc<ZRelocationCycle>>) {
        let st = &self.counters.region_reloc;
        let sized: Vec<(usize, Option<usize>)> = survivors
            .iter()
            .map(|&(from, cell)| (from, self.size_via_cell(cell, from)))
            .collect();
        let Some(grid) = ZRegionGrid::new(base, low_end, ZRELOC_REGION_BYTES) else {
            // No region grid over this arena (its base is off the 8-byte
            // grid): copy nothing. Every survivor stays, exactly as on a
            // window that did not open.
            st.rreloc[7].fetch_add(1, Ordering::Relaxed);
            let mut out = ZRegionDrain {
                halted: true,
                ..ZRegionDrain::default()
            };
            for (from, size) in sized {
                out.stays.push((from, size.map_or(low_end, |s| from + s)));
            }
            return (out, None);
        };
        let panic_after_first = super::zgc_dbg_slide_panic_enabled();
        let mut copies = 0usize;
        let (out, cycle) = drain_stw_keep(
            &grid,
            &sized,
            halted,
            Self::MAX_ALLOC_EXTENT,
            |size| arena.alloc_to_space(size, 8).map(|p| p as usize),
            |from, to, size| {
                debug_assert!(to >= low_end, "to-space lies above the old cursor");
                // SAFETY: `size` bytes are live at `from` (a selected survivor,
                // sized above); `to` was just handed out by the to-space window
                // for `size` bytes (committed by `hand_out`) and lies at or
                // above the old low cursor, which every byte of the relocation
                // set is below, so the ranges cannot overlap. The same
                // argument as the evacuating loop's.
                unsafe { std::ptr::copy_nonoverlapping(from as *const u8, to as *mut u8, size) };
                copies += 1;
                // `CRATONVM_DBG_ZGC_SLIDE_PANIC=1`: the fail-stop's positive
                // control on this arm too.
                if copies == 1 && panic_after_first {
                    panic!("CRATONVM_DBG_ZGC_SLIDE_PANIC: injected after the first copy");
                }
            },
        );
        if out.alloc_stops > 0 {
            self.counters.slide_evacuate[2].fetch_add(out.alloc_stops, Ordering::Relaxed);
        }
        let r = &st.rreloc;
        r[0].fetch_add(1, Ordering::Relaxed);
        r[1].fetch_add(out.regions, Ordering::Relaxed);
        r[2].fetch_add(out.pairs.len(), Ordering::Relaxed);
        r[3].fetch_add(out.moved_bytes, Ordering::Relaxed);
        if out.halted && !halted {
            r[4].fetch_add(1, Ordering::Relaxed);
        }
        r[5].fetch_add(out.xcheck_bad, Ordering::Relaxed);
        if self.gc_log_enabled.load(Ordering::Relaxed) {
            eprintln!(
                "[GC] zgc-rreloc: rreloc_regions={} rreloc_objects={} rreloc_bytes={} \
                 rreloc_stays={} rreloc_halted={} rreloc_alloc_stops={} rreloc_refusals={} \
                 rreloc_xcheck_bad={} rreloc_cycles={} rreloc_fallbacks={}",
                out.regions,
                out.pairs.len(),
                out.moved_bytes,
                out.stays.len(),
                u8::from(out.halted),
                out.alloc_stops,
                out.refusals,
                out.xcheck_bad,
                r[0].load(Ordering::Relaxed),
                r[6].load(Ordering::Relaxed),
            );
        }
        // zt t23/zc7: hand the cycle on only when every pair is one it
        // answers and it holds nothing else.
        let keep = out.xcheck_bad == 0 && cycle.installed() == out.pairs.len();
        let cycle = keep.then(|| std::sync::Arc::new(cycle));
        (out, cycle)
    }

    // -----------------------------------------------------------------------
    // zt t23/zc7 -- R3: the cycle kept across the mutator interval
    // -----------------------------------------------------------------------

    /// R3: keep this slide's `cycle` (`pairs` installs) until the next mark
    /// end. Called by the slide with the world stopped, after the rewrite and
    /// the registry rebuild and BEFORE it publishes its own pairs, so that a
    /// cycle this one replaces (only a slide with no collection in between:
    /// tests) is handed over under an older epoch than this slide's pairs.
    pub(super) fn keep_region_cycle(
        &self,
        cycle: std::sync::Arc<ZRelocationCycle>,
        mode: ZRegionQuarantine,
        pairs: usize,
    ) {
        let st = &self.counters.region_reloc;
        // zt t31/zc11 (R4): the hot paths see the new cycle before the one it
        // replaces is retired below.
        self.remap_in_mark_publish(Some(&cycle));
        let prev = st
            .live_cycle
            .lock()
            .replace(ZKeptRegionCycle { cycle, mode, pairs });
        st.live_cycle_armed.store(true, Ordering::Release);
        let q = &st.rquar;
        q[0].fetch_add(1, Ordering::Relaxed);
        q[1].fetch_add(pairs, Ordering::Relaxed);
        q[2].fetch_add(pairs, Ordering::Relaxed);
        if prev.is_some() {
            q[6].fetch_add(1, Ordering::Relaxed);
            self.retire_region_cycle(prev);
        }
    }

    /// R3: take the kept cycle out (the next mark end; see
    /// `release_evac_quarantine`). From here on no stale-address lookup reads
    /// it. One acquire load when none is kept.
    pub(super) fn take_region_cycle(&self) -> Option<ZKeptRegionCycle> {
        let st = &self.counters.region_reloc;
        if !st.live_cycle_armed.load(Ordering::Acquire) {
            return None;
        }
        let mut guard = st.live_cycle.lock();
        st.live_cycle_armed.store(false, Ordering::Release);
        // zt t31/zc11 (R4): withdrawn before the caller can drop the `Arc`.
        self.remap_in_mark_publish(None);
        let kept = (*guard).take();
        drop(guard);
        kept
    }

    /// R3: end a cycle taken by [`Self::take_region_cycle`]: under
    /// [`ZRegionQuarantine::Handover`] its pairs go into `relocations` (a
    /// fresh slide epoch, older than any later slide's, so the chain rule
    /// still follows a later move of a `to`), then the cycle is dropped. One
    /// `[GC] zgc-rquar:` line under `-verbose:gc`. A no-op for `None`.
    pub(super) fn retire_region_cycle(&self, kept: Option<ZKeptRegionCycle>) {
        let Some(kept) = kept else {
            return;
        };
        let st = &self.counters.region_reloc;
        let q = &st.rquar;
        q[4].fetch_add(1, Ordering::Relaxed);
        let mut handed = 0usize;
        if kept.mode == ZRegionQuarantine::Handover {
            let mut pairs: Vec<(usize, usize)> = Vec::with_capacity(kept.pairs);
            kept.cycle.for_each_pair(|from, to| pairs.push((from, to)));
            handed = pairs.len();
            self.publish_relocations(&pairs);
            q[5].fetch_add(handed, Ordering::Relaxed);
        }
        if self.gc_log_enabled.load(Ordering::Relaxed) {
            let s = st.rquar_stats();
            eprintln!(
                "[GC] zgc-rquar: rquar_regions={} rquar_pairs={} rquar_table_kb={} \
                 rquar_handover={} rquar_handed={} rquar_kept={} rquar_released={} \
                 rquar_unpublished={} rquar_answered={} rquar_stale_answered={} \
                 rquar_replaced={}",
                kept.cycle.regions().len(),
                kept.pairs,
                kept.cycle.table_bytes() / 1024,
                u8::from(kept.mode == ZRegionQuarantine::Handover),
                handed,
                s[0],
                s[4],
                s[2],
                s[3],
                s[7],
                s[6],
            );
        }
    }

    /// R3: where a live cycle moved `addr`, if it did and the destination is
    /// a registered base (the same contract as every other answer of
    /// `forwarded_after_slide`). `None` at once when no cycle is kept.
    pub(super) fn region_cycle_forwarded(&self, addr: usize) -> Option<usize> {
        let st = &self.counters.region_reloc;
        if !st.live_cycle_armed.load(Ordering::Acquire) {
            return None;
        }
        let guard = st.live_cycle.lock();
        let to = (*guard).as_ref().and_then(|k| k.cycle.forwarded(addr));
        drop(guard);
        let to = to?;
        st.rquar[3].fetch_add(1, Ordering::Relaxed);
        self.registry.contains(to).then_some(to)
    }

    /// R3: stale words the dead-at-release census found that the released
    /// cycle answers (the words R4 would heal in the mark).
    pub(super) fn note_region_cycle_stale_answered(&self, n: usize) {
        if n != 0 {
            self.counters.region_reloc.rquar[7].fetch_add(n, Ordering::Relaxed);
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    const KIB: usize = 1024;
    /// A synthetic, 8-aligned arena base well above zero; nothing is mapped
    /// there -- every test copies through its own word array.
    const BASE: usize = 0x4000_0000;

    fn grid(regions: usize, region_bytes: usize) -> ZRegionGrid {
        ZRegionGrid::new(BASE, BASE + regions * region_bytes, region_bytes).unwrap()
    }

    #[test]
    fn region_size_is_the_logical_page_size() {
        assert_eq!(
            ZRELOC_REGION_BYTES,
            crate::zgc::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES
        );
        assert!(
            ZRegionGrid::new(BASE, BASE + 4 * ZRELOC_REGION_BYTES, ZRELOC_REGION_BYTES).is_some()
        );
    }

    #[test]
    fn grid_rejects_unusable_geometry() {
        assert!(ZRegionGrid::new(BASE, BASE + 100, 3 * KIB).is_none());
        assert!(ZRegionGrid::new(BASE, BASE + 100, 8).is_none());
        assert!(ZRegionGrid::new(BASE, BASE + 100, 64 * 1024 * KIB).is_none());
        assert!(ZRegionGrid::new(BASE + 4, BASE + 100, 64 * KIB).is_none());
        assert!(ZRegionGrid::new(BASE, BASE - 8, 64 * KIB).is_none());
        // 32 MiB is the largest region the from-offset field keys.
        assert!(ZRegionGrid::new(BASE, BASE + 100, 32 * 1024 * KIB).is_some());
    }

    #[test]
    fn grid_maps_addresses_and_used_extent() {
        let g = ZRegionGrid::new(BASE, BASE + 2 * 64 * KIB + 100, 64 * KIB).unwrap();
        assert_eq!(g.region_count(), 3);
        assert_eq!(g.region_of(BASE), Some(0));
        assert_eq!(g.region_of(BASE + 64 * KIB - 1), Some(0));
        assert_eq!(g.region_of(BASE + 64 * KIB), Some(1));
        assert_eq!(g.region_of(BASE - 1), None);
        assert_eq!(g.region_of(g.end()), None);
        assert_eq!(g.used_in(0), 64 * KIB);
        assert_eq!(g.used_in(2), 100);
        assert_eq!(g.used_in(3), 0);
    }

    #[test]
    fn liveness_books_objects_to_their_start_region_and_records_straddles() {
        let g = grid(4, 64 * KIB);
        let objs = [
            (BASE, 32),
            (BASE + 64, 48),
            // Starts in region 1, reaches 16 bytes into region 2.
            (BASE + 2 * 64 * KIB - 16, 32),
            // Outside the grid (the high end).
            (BASE + 10 * 64 * KIB, 64),
            // Corrupt size.
            (BASE + 3 * 64 * KIB, 8),
        ];
        let l = region_liveness(&g, objs);
        assert_eq!(l.live_bytes, vec![80, 32, 0, 0]);
        assert_eq!(l.live_objects, vec![2, 1, 0, 0]);
        assert_eq!(l.straddle_in_bytes, vec![0, 0, 16, 0]);
        assert_eq!(l.outside, 1);
        assert_eq!(l.unsizable, 1);
    }

    #[test]
    fn a_multi_region_object_straddles_every_region_it_covers() {
        let g = grid(4, 64 * KIB);
        // Starts 8 bytes before region 1 and ends exactly at region 3's base:
        // regions 1 and 2 are covered whole, region 3 not at all.
        let l = region_liveness(&g, [(BASE + 64 * KIB - 8, 2 * 64 * KIB + 8)]);
        assert_eq!(l.straddle_in_bytes, vec![0, 64 * KIB, 64 * KIB, 0]);
        let l = region_liveness(&g, [(BASE + 64 * KIB - 8, 2 * 64 * KIB + 16)]);
        assert_eq!(l.straddle_in_bytes, vec![0, 64 * KIB, 64 * KIB, 8]);
        assert_eq!(l.live_bytes[0], 2 * 64 * KIB + 16);
    }

    #[test]
    fn selection_frees_empty_regions_keeps_dense_and_excluded_ones_and_picks_sparse() {
        let g = grid(6, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let objs = vec![
            // 0: dense (one 60 KiB object): occupancy 0.94, never selected.
            (r(0), 60 * KIB),
            // 1: sparse: 1 KiB live of 64 KiB.
            (r(1), KIB),
            // 2: empty (no live object).
            // 3: sparse but excluded by the caller (a raw holder).
            (r(3), KIB),
            // 4: tail of an object from 3 reaches in -> straddled.
            (r(4) - 16, 32),
            // 5: sparse.
            (r(5) + 128, 2 * KIB),
        ];
        let l = region_liveness(&g, objs);
        let sel = select_relocation_regions(&g, &l, &ZRelocationPolicy::default(), |i| i == 3);
        assert!(sel.trustworthy());
        assert_eq!(sel.empty, vec![2]);
        assert_eq!(sel.excluded, 1);
        assert_eq!(sel.straddled, 1);
        let mut ids = sel.set.page_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 5]);
        // Most garbage per byte first: region 1 (63/64) before region 5 (62/64).
        assert_eq!(sel.set.pages()[0].page_id, 1);
        assert_eq!(sel.set.total_live_bytes(), 3 * KIB);
    }

    #[test]
    fn selection_honours_the_policy_budget_and_reports_corrupt_sizes() {
        let g = grid(3, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let l = region_liveness(&g, [(r(0), KIB), (r(1), 2 * KIB), (r(2), 4), (r(2), KIB)]);
        let policy = ZRelocationPolicy {
            max_evacuation_bytes: KIB,
            ..ZRelocationPolicy::default()
        };
        let sel = select_relocation_regions(&g, &l, &policy, |_| false);
        assert_eq!(sel.set.page_ids(), vec![0]);
        assert!(!sel.trustworthy());
    }

    #[test]
    fn encoding_round_trips_from_the_arena_base_and_refuses_below_it() {
        let g = grid(2, 64 * KIB);
        let f = ZRegionForwarding::new(&g, 1, 4, 64);
        // Offset 0 of the arena is a legal destination and must not encode to 0.
        let p = f.encode_to(BASE).unwrap();
        assert!(!p.is_null());
        assert_eq!(f.decode_to(p), BASE);
        assert_eq!(f.decode_to(f.encode_to(BASE + 4096).unwrap()), BASE + 4096);
        assert!(f.encode_to(BASE - 8).is_none());
        assert!(f.encode_to(BASE + 4).is_none());
        // A Linux-shaped arena base still encodes: the payload is relative.
        let hi =
            ZRegionGrid::new(0x7f00_0000_0000, 0x7f00_0000_0000 + 128 * KIB, 64 * KIB).unwrap();
        let fh = ZRegionForwarding::new(&hi, 0, 1, 32);
        let to = 0x7f00_0000_0000 + 64 * KIB + 8;
        assert_eq!(fh.decode_to(fh.encode_to(to).unwrap()), to);
    }

    /// A word-addressed fake heap: `copy` moves `size / 8` words.
    struct FakeHeap {
        words: Vec<AtomicU64>,
        bump: AtomicUsize,
    }

    impl FakeHeap {
        fn new(bytes: usize, to_space_from: usize) -> Self {
            Self {
                words: (0..bytes / 8).map(|_| AtomicU64::new(0)).collect(),
                bump: AtomicUsize::new(BASE + to_space_from),
            }
        }
        fn alloc(&self, size: usize) -> Option<usize> {
            let at = self.bump.fetch_add(size, Ordering::Relaxed);
            (at + size <= BASE + self.words.len() * 8).then_some(at)
        }
        fn word(&self, addr: usize) -> &AtomicU64 {
            &self.words[(addr - BASE) / 8]
        }
        fn copy(&self, from: usize, to: usize, size: usize) {
            for k in 0..size / 8 {
                let v = self.word(from + k * 8).load(Ordering::Relaxed);
                self.word(to + k * 8).store(v, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn first_copier_relocates_later_ones_are_forwarded_without_a_copy() {
        let g = grid(2, 64 * KIB);
        let heap = FakeHeap::new(2 * 64 * KIB, 64 * KIB);
        let f = ZRegionForwarding::new(&g, 0, 2, 64);
        let obj = BASE + 128;
        heap.word(obj).store(0xfeed, Ordering::Relaxed);
        let a = f
            .relocate_or_forward(obj, 32, |s| heap.alloc(s), |x, y, s| heap.copy(x, y, s))
            .unwrap();
        let ZRegionRelocated::Relocated(to) = a else {
            panic!("first copier must win: {a:?}")
        };
        assert_eq!(heap.word(to).load(Ordering::Relaxed), 0xfeed);
        let b = f
            .relocate_or_forward(
                obj,
                32,
                |_| panic!("no allocation on a hit"),
                |_, _, _| panic!("no copy on a hit"),
            )
            .unwrap();
        assert_eq!(b, ZRegionRelocated::Forwarded(to));
        assert_eq!(f.forwarded(obj), Some(to));
        assert_eq!(f.forwarded(obj + 32), None);
        assert_eq!(f.stats(), (1, 32, 0, 0));
        assert_eq!(f.in_flight(), 0);
    }

    #[test]
    fn a_lost_install_returns_the_winner_and_names_the_abandoned_copy() {
        let g = grid(2, 64 * KIB);
        let heap = FakeHeap::new(2 * 64 * KIB, 64 * KIB);
        let f = ZRegionForwarding::new(&g, 0, 1, 32);
        let obj = BASE + 64;
        // Another copier installs between our allocation and our install.
        let mut theirs = 0usize;
        let r = f
            .relocate_or_forward(
                obj,
                32,
                |s| heap.alloc(s),
                |x, y, s| {
                    heap.copy(x, y, s);
                    let inner = f
                        .relocate_or_forward(
                            x,
                            s,
                            |s2| heap.alloc(s2),
                            |a, b, n| heap.copy(a, b, n),
                        )
                        .unwrap();
                    theirs = inner.address();
                },
            )
            .unwrap();
        match r {
            ZRegionRelocated::LostRace { winner, abandoned } => {
                assert_eq!(winner, theirs);
                assert_ne!(abandoned, theirs);
            }
            other => panic!("expected a lost race: {other:?}"),
        }
        assert_eq!(f.forwarded(obj), Some(theirs));
        assert_eq!(f.stats(), (1, 32, 1, 32));
    }

    #[test]
    fn bad_objects_and_failed_allocations_are_refused() {
        let g = grid(2, 64 * KIB);
        let f = ZRegionForwarding::new(&g, 1, 1, 32);
        let in_region = BASE + 64 * KIB + 64;
        let none = |_: usize| -> Option<usize> { None };
        let nocopy = |_: usize, _: usize, _: usize| {};
        // Outside the region, misaligned, too small.
        for (from, size) in [(BASE + 64, 32), (in_region + 4, 32), (in_region, 8)] {
            assert_eq!(
                f.relocate_or_forward(from, size, none, nocopy),
                Err(ZRegionRelocateError::BadObject)
            );
        }
        assert_eq!(
            f.relocate_or_forward(in_region, 32, none, nocopy),
            Err(ZRegionRelocateError::AllocFailed)
        );
        assert_eq!(
            f.relocate_or_forward(in_region, 32, |_| Some(BASE - 64), nocopy),
            Err(ZRegionRelocateError::Unencodable)
        );
        // Every refusal released its hold.
        assert_eq!(f.in_flight(), 0);
    }

    #[test]
    fn seal_refuses_new_copiers_and_quiescence_waits_for_the_last_guard() {
        let g = grid(1, 64 * KIB);
        let heap = FakeHeap::new(2 * 64 * KIB, 64 * KIB);
        let f = ZRegionForwarding::new(&g, 0, 2, 64);
        let a = BASE + 64;
        let b = BASE + 128;
        let moved_a = f
            .relocate_or_forward(a, 32, |s| heap.alloc(s), |x, y, s| heap.copy(x, y, s))
            .unwrap()
            .address();
        let held = f.retain().expect("unsealed region admits a copier");
        assert_eq!(f.in_flight(), 1);
        assert_eq!(f.seal(), 1);
        assert!(f.is_sealed());
        assert!(!f.is_quiescent());
        assert!(f.retain().is_none());
        // A sealed region answers its entries and refuses to copy a new one.
        assert_eq!(
            f.relocate_or_forward(a, 32, |_| None, |_, _, _| {}),
            Ok(ZRegionRelocated::Forwarded(moved_a))
        );
        assert_eq!(
            f.relocate_or_forward(b, 32, |s| heap.alloc(s), |_, _, _| {}),
            Err(ZRegionRelocateError::SealedWithoutEntry)
        );
        drop(held);
        assert!(f.is_quiescent());
    }

    #[test]
    fn a_panicking_copier_still_releases_its_hold() {
        let g = grid(1, 64 * KIB);
        let f = ZRegionForwarding::new(&g, 0, 1, 32);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = f.relocate_or_forward(
                BASE + 64,
                32,
                |_| Some(BASE + 32 * KIB),
                |_, _, _| panic!("copy fault"),
            );
        }));
        assert!(r.is_err());
        assert_eq!(f.in_flight(), 0);
        assert_eq!(f.seal(), 0);
        assert!(f.is_quiescent());
    }

    #[test]
    fn racing_copiers_agree_on_one_address_per_object() {
        const THREADS: usize = 8;
        const OBJECTS: usize = 256;
        const SIZE: usize = 32;
        let region = 64 * KIB;
        let g = grid(1, region);
        let heap = Arc::new(FakeHeap::new(region + THREADS * OBJECTS * SIZE, region));
        for i in 0..OBJECTS {
            heap.word(BASE + i * SIZE)
                .store(i as u64 + 1, Ordering::Relaxed);
        }
        let f = Arc::new(ZRegionForwarding::new(&g, 0, OBJECTS, OBJECTS * SIZE));
        let start = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let (heap, f, start) = (Arc::clone(&heap), Arc::clone(&f), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    let mut seen = vec![0usize; OBJECTS];
                    let mut wins = 0usize;
                    for k in 0..OBJECTS {
                        // Each thread walks the objects from a different start.
                        let i = (k + t * 31) % OBJECTS;
                        let r = f
                            .relocate_or_forward(
                                BASE + i * SIZE,
                                SIZE,
                                |s| heap.alloc(s),
                                |x, y, s| heap.copy(x, y, s),
                            )
                            .unwrap();
                        if matches!(r, ZRegionRelocated::Relocated(_)) {
                            wins += 1;
                        }
                        seen[i] = r.address();
                    }
                    (seen, wins)
                })
            })
            .collect();
        let results: Vec<(Vec<usize>, usize)> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let total_wins: usize = results.iter().map(|r| r.1).sum();
        assert_eq!(total_wins, OBJECTS, "exactly one install per object");
        for i in 0..OBJECTS {
            let a = results[0].0[i];
            assert!(results.iter().all(|r| r.0[i] == a), "object {i} split");
            assert_eq!(f.forwarded(BASE + i * SIZE), Some(a));
            assert_eq!(heap.word(a).load(Ordering::Relaxed), i as u64 + 1);
        }
        let (won, won_bytes, lost, abandoned) = f.stats();
        assert_eq!((won, won_bytes), (OBJECTS, OBJECTS * SIZE));
        assert_eq!(abandoned, lost * SIZE);
        assert_eq!(f.in_flight(), 0);
    }

    #[test]
    fn the_cycle_maps_addresses_to_selected_regions_only() {
        let g = grid(4, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let l = region_liveness(
            &g,
            [
                (r(0), 60 * KIB),
                (r(1), KIB),
                (r(1) + KIB, KIB),
                (r(3), 2 * KIB),
            ],
        );
        let sel = select_relocation_regions(&g, &l, &ZRelocationPolicy::default(), |_| false);
        let cycle = ZRelocationCycle::build(&g, &l, &sel.set);
        assert_eq!(cycle.regions().len(), 2);
        assert!(cycle.forwarding_for(r(0) + 8).is_none());
        assert!(cycle.forwarding_for(r(2)).is_none());
        assert!(cycle.forwarding_for(BASE - 8).is_none());
        assert!(cycle.forwarding_for(r(4)).is_none());
        let f1 = cycle.forwarding_for(r(1) + 100).unwrap();
        assert_eq!(f1.region(), 1);
        assert_eq!(f1.live(), (2, 2 * KIB));
        assert_eq!(f1.span(), r(1)..r(2));
        assert_eq!(cycle.forwarding_for(r(3)).unwrap().region(), 3);
        assert_eq!(cycle.forwarded(r(1)), None);
    }

    #[test]
    fn workers_claim_each_region_once_and_the_cycle_is_quiescent_when_all_seal() {
        let g = grid(4, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let l = region_liveness(&g, [(r(0), KIB), (r(1), KIB), (r(2), KIB), (r(3), KIB)]);
        let sel = select_relocation_regions(&g, &l, &ZRelocationPolicy::default(), |_| false);
        let cycle = ZRelocationCycle::build(&g, &l, &sel.set);
        assert_eq!(cycle.regions().len(), 4);
        let mut claimed = Vec::new();
        while let Some(f) = cycle.claim_next() {
            claimed.push(f.region());
        }
        claimed.sort_unstable();
        assert_eq!(claimed, vec![0, 1, 2, 3]);
        assert!(!cycle.all_quiescent());
        for f in cycle.regions() {
            assert!(f.is_claimed());
            assert_eq!(f.seal(), 0);
        }
        assert!(cycle.all_quiescent());
    }

    // ---- zt t23/zc5: R1 (census) and R2 (the stop-the-world drain) --------

    #[test]
    fn zt23_build_regions_keeps_the_given_order_and_skips_repeats_and_strays() {
        let g = grid(4, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        // Region 1: two objects, 32 + 64 = 96 bytes; region 3: one, 64 bytes.
        let l = region_liveness(&g, [(r(1), 32), (r(1) + 64, 64), (r(3), 64)]);
        // 3 first, then 1; the repeated 3 and the stray 9 (past the grid's
        // four regions) are skipped.
        let cycle = ZRelocationCycle::build_regions(&g, &l, &[3, 1, 3, 9]);
        let ids: Vec<u64> = cycle.regions().iter().map(|f| f.region()).collect();
        assert_eq!(ids, vec![3, 1]);
        assert_eq!(cycle.regions()[1].live(), (2, 96));
        assert_eq!(cycle.regions()[0].live(), (1, 64));
        assert_eq!(cycle.claim_next().map(|f| f.region()), Some(3));
        assert_eq!(cycle.claim_next().map(|f| f.region()), Some(1));
        assert!(cycle.claim_next().is_none());
        assert!(cycle.forwarding_for(r(2)).is_none());
    }

    #[test]
    fn zt23_the_census_selects_from_survivors_and_lists_empty_regions_apart() {
        // The fixture of `selection_frees_empty_regions_...`: region 0 dense
        // (60 of 64 KiB live), 1 and 5 sparse, 2 empty, 3 excluded by the
        // caller, 4 straddled by region 3's last object (16 bytes reach in).
        let g = grid(6, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let objs = vec![
            (r(0), 60 * KIB),
            (r(1), KIB),
            (r(3), KIB),
            (r(4) - 16, 32),
            (r(5) + 128, 2 * KIB),
        ];
        let c = reloc_select_census(&g, objs, &ZRelocationPolicy::default(), |i| i == 3);
        assert_eq!(c.regions, 6);
        assert_eq!(c.set, vec![1, 5]);
        // 1 KiB (region 1) + 2 KiB (region 5).
        assert_eq!(c.set_live_bytes, 3 * KIB);
        assert_eq!(c.empty, vec![2]);
        assert_eq!(
            (c.straddled, c.excluded, c.unsizable, c.outside),
            (1, 1, 0, 0)
        );
        assert!(c.slide.is_none());
        assert_eq!(c.agreement(), ZRelocSelectAgreement::default());
    }

    #[test]
    fn zt23_agreement_is_counted_region_by_region() {
        // R1 reclaims regions 1 and 5 (set) and 2 (empty) of six.
        let r1 = ZRelocSelectCensus {
            regions: 6,
            set: vec![1, 5],
            empty: vec![2],
            ..ZRelocSelectCensus::default()
        };
        // The slide's grid stops at 5 regions (the sweep retracted the cursor
        // below region 5); it selected 1, 2, 4 and a stray 7.
        //   0: neither -> agree      1: both -> agree     2: both -> agree
        //   3: neither -> agree      4: slide only        5: R1 set, retracted
        //   (reclaimed by the slide too) -> agree;  stray 7 -> slide only.
        let c = ZRelocSelectCensus {
            slide: Some((vec![1, 2, 4, 7], 5)),
            ..r1.clone()
        };
        assert_eq!(
            c.agreement(),
            ZRelocSelectAgreement {
                compared: true,
                agree: 5,
                r1_only: 0,
                slide_only: 2,
                retracted: 1,
            }
        );
        // The slide selected nothing over a grid as tall as R1's: R1's three
        // reclaimed regions (1, 2, 5) are R1-only, the other three agree.
        let c = ZRelocSelectCensus {
            slide: Some((vec![], 6)),
            ..r1.clone()
        };
        assert_eq!(
            c.agreement(),
            ZRelocSelectAgreement {
                compared: true,
                agree: 3,
                r1_only: 3,
                slide_only: 0,
                retracted: 0,
            }
        );
        let line = c.line(7);
        assert!(line.starts_with("[GC] zgc-rsel: rsel_cycle=0 rsel_regions=6 rsel_set=2 "));
        assert!(line.contains(" rsel_compared=1 rsel_slide_set=0 rsel_agree=3 rsel_r1_only=3 "));
        assert!(line.ends_with(" rsel_young_skipped=7"));
        // 17 keys, each `rsel_`-prefixed and printed once.
        let keys: Vec<&str> = line
            .split_whitespace()
            .skip(2)
            .map(|kv| kv.split('=').next().unwrap_or(""))
            .collect();
        assert_eq!(keys.len(), 17);
        assert!(keys.iter().all(|k| k.starts_with("rsel_")));
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), keys.len());
        // No slide side: not compared, counts zero.
        assert!(r1
            .line(0)
            .contains(" rsel_compared=0 rsel_slide_set=0 rsel_agree=0 "));
    }

    #[test]
    fn zt23_the_drain_copies_in_address_order_into_to_space() {
        let g = grid(4, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        // To-space starts at the grid's end, as the arena's window opens at
        // the old low cursor.
        let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
        let t = BASE + 4 * 64 * KIB;
        let survivors = [
            (r(0) + 64, Some(32)),
            (r(0) + 256, Some(48)),
            (r(1) + 8, Some(32)),
            (r(3), Some(64)),
        ];
        for (k, &(from, _)) in survivors.iter().enumerate() {
            heap.word(from).store(k as u64 + 1, Ordering::Relaxed);
        }
        let d = drain_stw(
            &g,
            &survivors,
            false,
            usize::MAX,
            |s| heap.alloc(s),
            |f, to, s| heap.copy(f, to, s),
        );
        // Bumped in survivor order: 32, 48, 32 then 64 bytes, so the
        // destinations are t, t+32, t+80 and t+112.
        assert_eq!(
            d.pairs,
            vec![
                (r(0) + 64, t),
                (r(0) + 256, t + 32),
                (r(1) + 8, t + 80),
                (r(3), t + 112)
            ]
        );
        assert_eq!(
            d.moves,
            vec![
                (r(0) + 64, 32),
                (r(0) + 256, 48),
                (r(1) + 8, 32),
                (r(3), 64)
            ]
        );
        // 32 + 48 + 32 + 64.
        assert_eq!(d.moved_bytes, 176);
        // Regions 0, 1 and 3 hold survivors; region 2 gets no table.
        assert_eq!(d.regions, 3);
        assert!(d.stays.is_empty());
        assert!(!d.halted);
        assert_eq!((d.alloc_stops, d.refusals, d.xcheck_bad), (0, 0, 0));
        for (k, &(_, to)) in d.pairs.iter().enumerate() {
            assert_eq!(heap.word(to).load(Ordering::Relaxed), k as u64 + 1);
        }
    }

    #[test]
    fn zt23_the_drain_keeps_the_slides_stay_rules() {
        let g = grid(4, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
        let t = BASE + 4 * 64 * KIB;
        // The grid's end is the old cursor, where to-space begins.
        let end = g.end();
        assert_eq!(end, t);
        let survivors = [
            // 48 bytes is above the 40-byte extent limit: stays, no halt.
            (r(0) + 64, Some(48)),
            // Copied to the first to-space byte.
            (r(0) + 128, Some(32)),
            // Unsizable: stays, bounded by the grid's end, and halts.
            (r(0) + 256, None),
            // After the halt: stays.
            (r(1), Some(32)),
            // After the halt and unsizable: stays to the grid's end.
            (r(2), None),
        ];
        let d = drain_stw(
            &g,
            &survivors,
            false,
            40,
            |s| heap.alloc(s),
            |f, to, s| heap.copy(f, to, s),
        );
        assert_eq!(d.pairs, vec![(r(0) + 128, t)]);
        assert_eq!(d.moves, vec![(r(0) + 128, 32)]);
        // 64 + 48 = 112 for the first stay.
        assert_eq!(
            d.stays,
            vec![
                (r(0) + 64, r(0) + 112),
                (r(0) + 256, end),
                (r(1), r(1) + 32),
                (r(2), end)
            ]
        );
        assert!(d.halted);
        // Regions 0, 1, 2 each have a survivor (region 2's is unsizable, so
        // its table is sized for zero entries and receives none).
        assert_eq!(d.regions, 3);
        assert_eq!((d.alloc_stops, d.refusals), (0, 0));
    }

    #[test]
    fn zt23_a_refused_allocation_halts_the_drain_and_a_halted_entry_moves_nothing() {
        let g = grid(2, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let heap = FakeHeap::new(4 * 64 * KIB, 2 * 64 * KIB);
        let t = BASE + 2 * 64 * KIB;
        let survivors = [
            (r(0) + 64, Some(32)),
            (r(0) + 128, Some(32)),
            (r(1), Some(32)),
        ];
        // The first allocation succeeds, the second is refused.
        let calls = std::cell::Cell::new(0usize);
        let d = drain_stw(
            &g,
            &survivors,
            false,
            usize::MAX,
            |s| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    heap.alloc(s)
                } else {
                    None
                }
            },
            |f, to, s| heap.copy(f, to, s),
        );
        assert_eq!(d.pairs, vec![(r(0) + 64, t)]);
        // 128 + 32 = 160.
        assert_eq!(d.stays, vec![(r(0) + 128, r(0) + 160), (r(1), r(1) + 32)]);
        assert!(d.halted);
        assert_eq!((d.alloc_stops, d.refusals), (1, 0));
        assert_eq!(calls.get(), 2, "no allocation after the halt");
        // Halted on entry (the to-space window did not open): every survivor
        // stays and nothing is allocated or copied; both regions still get a
        // (sealed, empty) table.
        let d = drain_stw(
            &g,
            &survivors,
            true,
            usize::MAX,
            |_| panic!("no allocation on a halted drain"),
            |_, _, _| panic!("no copy on a halted drain"),
        );
        assert!(d.pairs.is_empty() && d.moves.is_empty());
        assert_eq!(
            d.stays,
            vec![
                (r(0) + 64, r(0) + 96),
                (r(0) + 128, r(0) + 160),
                (r(1), r(1) + 32)
            ]
        );
        assert_eq!(d.regions, 2);
        assert!(d.halted);
    }

    #[test]
    fn zt23_a_survivor_outside_the_grid_is_refused_and_stays() {
        let g = grid(2, 64 * KIB);
        let r = |i: usize| BASE + i * 64 * KIB;
        let heap = FakeHeap::new(4 * 64 * KIB, 3 * 64 * KIB);
        // r(2) + 8 is past the grid's end (r(2)): no region, no table.
        let survivors = [(r(1), Some(32)), (r(2) + 8, Some(32))];
        let d = drain_stw(
            &g,
            &survivors,
            false,
            usize::MAX,
            |s| heap.alloc(s),
            |f, to, s| heap.copy(f, to, s),
        );
        assert_eq!(d.pairs, vec![(r(1), BASE + 3 * 64 * KIB)]);
        // 8 + 32 = 40.
        assert_eq!(d.stays, vec![(r(2) + 8, r(2) + 40)]);
        assert_eq!((d.regions, d.refusals), (1, 1));
        assert!(!d.halted);
    }

    // -----------------------------------------------------------------------
    // zt t23/zc7: the cycle after the drain (R2's readers, R3's kept cycle)
    // -----------------------------------------------------------------------

    /// The drain of `zt23_the_drain_copies_in_address_order_into_to_space`,
    /// shared by the zc7 tests: survivors at r(0)+64 (32 B), r(0)+256 (48 B),
    /// r(1)+8 (32 B) and r(3) (64 B) of a 4 x 64 KiB grid, to-space from
    /// t = r(4).
    fn zc7_survivors() -> [(usize, Option<usize>); 4] {
        let r = |i: usize| BASE + i * 64 * KIB;
        [
            (r(0) + 64, Some(32)),
            (r(0) + 256, Some(48)),
            (r(1) + 8, Some(32)),
            (r(3), Some(64)),
        ]
    }

    fn zc7_drain() -> (ZRegionDrain, ZRelocationCycle) {
        let g = grid(4, 64 * KIB);
        let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
        drain_stw_keep(
            &g,
            &zc7_survivors(),
            false,
            usize::MAX,
            |s| heap.alloc(s),
            |f, to, s| heap.copy(f, to, s),
        )
    }

    #[test]
    fn zt23zc7_the_kept_cycle_enumerates_its_installs_and_sizes_its_tables() {
        let (d, cycle) = zc7_drain();
        let r = |i: usize| BASE + i * 64 * KIB;
        let t = r(4);
        // As in the zc5 test: bumped 32, 48, 32, 64 -> t, t+32, t+80, t+112.
        assert_eq!(
            d.pairs,
            vec![
                (r(0) + 64, t),
                (r(0) + 256, t + 32),
                (r(1) + 8, t + 80),
                (r(3), t + 112)
            ]
        );
        assert_eq!(cycle.installed(), 4);
        assert!(cycle.all_quiescent());
        let mut seen: Vec<(usize, usize)> = Vec::new();
        cycle.for_each_pair(|f, to| seen.push((f, to)));
        seen.sort_unstable();
        assert_eq!(seen, d.pairs, "the tables hold exactly the drained pairs");
        // Tables: region 0 holds 2 survivors, regions 1 and 3 one each.
        // `zfwd_capacity_for(2)` = next_pow2(4) = 4 and `zfwd_capacity_for(1)`
        // = next_pow2(2) = 2, both clamped up to ZFWD_MIN_CAPACITY = 8 slots:
        // 3 tables x 8 slots x 8 bytes = 192, plus the region map, 4 grid
        // regions x 4 bytes = 16. 208 bytes.
        assert_eq!(cycle.table_bytes(), 208);
        // `drain_stw` is the same drain without the cycle.
        let g = grid(4, 64 * KIB);
        let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
        let again = drain_stw(
            &g,
            &zc7_survivors(),
            false,
            usize::MAX,
            |s| heap.alloc(s),
            |f, to, s| heap.copy(f, to, s),
        );
        assert_eq!(again, d);
    }

    /// The slide's forward index through the cycle answers exactly what the
    /// sorted index over the same pair list answers, while holding only the
    /// pairs the cycle does not vouch for.
    #[test]
    fn zt23zc7_the_index_through_the_cycle_answers_as_the_pair_index() {
        use crate::zgc::relocate::ZForwardIndex;
        let (d, cycle) = zc7_drain();
        let r = |i: usize| BASE + i * 64 * KIB;
        let t = r(4);
        // Two pairs the cycle does not hold: one from region 2 (no survivor
        // there, so no table) and one above the grid (the large-object end's
        // shape).
        let h = BASE + 7 * 64 * KIB;
        let mut all = d.pairs.clone();
        all.push((h + 4096, h + 8192));
        all.push((r(2) + 16, BASE + 6 * 64 * KIB));
        let plain = ZForwardIndex::from_pairs(&all);
        let cycle = Arc::new(cycle);
        let with = ZForwardIndex::from_pairs_with_cycle(&all, Arc::clone(&cycle));
        assert_eq!(
            with.entries(),
            &[(r(2) + 16, BASE + 6 * 64 * KIB), (h + 4096, h + 8192)][..],
            "only the two pairs the cycle does not answer are listed"
        );
        // 2 listed + 4 installs: the same 6 moves the plain index lists.
        assert_eq!((with.len(), plain.len()), (6, 6));
        let mut probes: Vec<usize> =
            vec![BASE - 8, BASE, r(0), r(0) + 72, r(1), r(2), r(3) + 8, t, h];
        for &(f, to) in &all {
            probes.extend([f, to, f + 8, f + 3]);
        }
        for a in probes {
            assert_eq!(with.get(a), plain.get(a), "address {a:#x}");
            assert_eq!(with.resolve(a), plain.resolve(a), "address {a:#x}");
        }
        assert_eq!(with.get(r(1) + 8), Some(t + 80));
        assert_eq!(with.get(r(2) + 16), Some(BASE + 6 * 64 * KIB));
        assert_eq!(with.get(r(0) + 72), None, "an interior word did not move");
        // Every cycle region installed something; region 3 ends at the grid's
        // end, r(4).
        assert_eq!(
            with.cycle_source_spans(),
            vec![(r(0), r(1)), (r(1), r(2)), (r(3), r(4))]
        );
        assert!(plain.cycle_source_spans().is_empty());
        // `for_slide` picks the constructor.
        let none = ZForwardIndex::for_slide(&all, None);
        assert!(none.cycle().is_none());
        assert_eq!(none.entries().len(), 6);
        let some = ZForwardIndex::for_slide(&all, Some(&cycle));
        assert!(some.cycle().is_some());
        assert_eq!(some.entries().len(), 2);
    }

    #[test]
    fn zt23zc7_the_quarantine_switch_reads_its_two_modes() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_REGION_QUARANTINE", v)],
                zgc_region_quarantine_mode,
            )
        };
        assert_eq!(read(None), None);
        assert_eq!(read(Some("")), None);
        assert_eq!(read(Some("0")), None);
        assert_eq!(read(Some("off")), None);
        assert_eq!(read(Some("1")), Some(ZRegionQuarantine::Handover));
        assert_eq!(read(Some("yes")), Some(ZRegionQuarantine::Handover));
        assert_eq!(read(Some("2")), Some(ZRegionQuarantine::NoHandover));
        assert_eq!(
            read(Some(" NoHandover ")),
            Some(ZRegionQuarantine::NoHandover)
        );
    }
}
