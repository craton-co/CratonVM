// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Heap backing store: reserve address space, commit it in granules, and give
//! it back.
//!
//! # Why this exists
//!
//! [`crate::arena::Arena`] was a `Vec<u8>` of `capacity` bytes obtained from
//! `alloc_zeroed`, and every collector in this crate is built on it. That has
//! three consequences that are properties of the ALLOCATION, not of any
//! collector:
//!
//! 1. **The whole of `-Xmx` is charged at startup.** On Linux a multi-gigabyte
//!    `alloc_zeroed` falls through to `mmap` and the pages are demand-faulted,
//!    so the resident cost tracks use -- but on **Windows** the allocator hands
//!    a block that size to `VirtualAlloc(MEM_COMMIT | MEM_RESERVE)`, which
//!    takes the full commit charge against the page file immediately. A
//!    `-Xmx4g` process reserves 4 GB of commit before it has executed a
//!    bytecode, and `-Xms` has no meaning at all.
//! 2. **Nothing is ever returned to the OS.** The sweep retracts the bump
//!    cursor and the compactor empties whole megabytes, and the pages stay
//!    resident for the life of the process. A JVM that peaks and then idles
//!    holds its peak forever.
//! 3. **The capacity is fixed at construction.** `Arena::grow` exists but the
//!    ZGC path never calls it, and cannot: the object-start bitmap is gridded
//!    over the envelope captured at construction.
//!
//! A reservation fixes (1) and (2) directly, and it is the shape (3) needs.
//!
//! # The model
//!
//! Reserve `capacity` bytes of ADDRESS SPACE up front -- cheap, and it is what
//! makes the base pointer stable for the life of the heap, which the
//! object-start bitmap, `MOVABLE_BOUNDS` and `conservative_addr_span` all
//! depend on. Then commit in [`GRANULE`]-sized units as the allocator's cursors
//! reach them, and decommit a granule once the collector can prove nothing in
//! it is live.
//!
//! Committed memory reads as zero, on both platforms and in the fallback, which
//! is the contract `Arena` already relied on (`alloc_zeroed`) and which
//! `alloc_raw`'s per-object `write_bytes` exists to restore for REUSED blocks.
//!
//! # The fallback is not a lesser mode
//!
//! [`HeapStore::Owned`] is the `alloc_zeroed` block this replaces, kept for
//! three cases: a platform whose syscalls we do not declare, a reservation the
//! OS refuses, and `CRATONVM_GC_RESERVE=0`. It behaves exactly as before --
//! committing is a no-op because everything is already committed, and
//! decommitting is a no-op because there is nothing to give back. Callers
//! cannot tell the two apart except through [`HeapStore::committed_bytes`],
//! which is the point: nothing else in the crate has to know.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Commit granularity, in bytes.
///
/// 2 MiB, matching `ZgcRealHeap::Z_LOGICAL_PAGE_BYTES` so a granule and a
/// logical page are the same unit and a page the collector has emptied is a
/// granule this can hand back. Also a hugepage on x86-64, so a committed run is
/// a candidate for one rather than being split down the middle.
///
/// Smaller would track the live set more tightly and cost more syscalls; larger
/// would make the first allocation commit more than a fresh JVM ever uses.
pub const GRANULE: usize = 2 * 1024 * 1024;

/// Is the reserve/commit backing store in use? `CRATONVM_GC_RESERVE=0` forces
/// the `alloc_zeroed` block this replaced, byte for byte, so an A/B is a re-run
/// rather than a rebuild.
///
/// Read once per process: it decides how the heap is MAPPED, and a heap cannot
/// change its backing store mid-run.
fn reserve_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(
        || match cratonvm_types::flags::runtime_var_os("CRATONVM_GC_RESERVE") {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        },
    )
}

/// Bytes this process has committed across every live heap store. Diagnostics
/// only -- `--verbose:gc` reports it so "the heap is 4 GB" and "the process is
/// holding 4 GB" stop being the same sentence.
static COMMITTED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Total committed heap bytes, across every [`HeapStore`] alive in this
/// process.
pub fn process_committed_bytes() -> usize {
    COMMITTED_BYTES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The recently-decommitted ring
// ---------------------------------------------------------------------------

/// How many give-backs are remembered. Small, fixed, and statically allocated:
/// this is read from a SIGSEGV handler, where nothing may allocate or lock.
pub const RECENT_DECOMMITS: usize = 64;

/// A ring of the last [`RECENT_DECOMMITS`] spans this process handed back to
/// the OS, so a crash handler can answer **"is this fault address memory we
/// gave away?"**
///
/// # Why this exists
///
/// A use-after-free of HEAP memory reads, in a register dump, exactly like a
/// wild pointer: an address inside the heap's reservation, faulting, with
/// nothing to say why. Before the reserve/commit store the same defect merely
/// read stale bytes and was invisible; now it faults, which is an improvement
/// only if the crash names the mechanism. The JIT side already answers the
/// equivalent question for code (`recent_code_free_covering`), and that ring is
/// the model for this one, including the ordering rule below.
///
/// # Re-commit is the trap
///
/// A granule handed back can be committed again minutes later, at which point
/// the address is mapped and a fault there is NOT this. So a slot is not
/// erased on re-commit -- it is FLAGGED ([`DECOMMIT_RECOMMITTED`]), because
/// "we gave this away and then took it back" is itself worth reporting, and
/// erasing it would silently turn a real answer into "no record".
///
/// # Reading it
///
/// Atomic loads only, no allocation, no lock: async-signal-safe. `base` is
/// published last with `Release` and read first with `Acquire`, so a torn slot
/// reads as empty rather than as a wrong range.
static DECOMMIT_BASE: [AtomicUsize; RECENT_DECOMMITS] =
    [const { AtomicUsize::new(0) }; RECENT_DECOMMITS];
static DECOMMIT_LEN: [AtomicUsize; RECENT_DECOMMITS] =
    [const { AtomicUsize::new(0) }; RECENT_DECOMMITS];
static DECOMMIT_FLAGS: [AtomicUsize; RECENT_DECOMMITS] =
    [const { AtomicUsize::new(0) }; RECENT_DECOMMITS];
/// The `&'static str` naming the collector-level site that released the span.
/// Split into pointer and length because a `&str` is not atomically storable;
/// both halves point into `'static` data, so the handler may read them.
static DECOMMIT_SITE_PTR: [AtomicUsize; RECENT_DECOMMITS] =
    [const { AtomicUsize::new(0) }; RECENT_DECOMMITS];
static DECOMMIT_SITE_LEN: [AtomicUsize; RECENT_DECOMMITS] =
    [const { AtomicUsize::new(0) }; RECENT_DECOMMITS];
/// The ordinal each slot was written with, so a reader can prefer the NEWEST
/// entry covering an address rather than the first one it happens to scan.
static DECOMMIT_AT: [AtomicUsize; RECENT_DECOMMITS] =
    [const { AtomicUsize::new(0) }; RECENT_DECOMMITS];
static DECOMMIT_SEQ: AtomicUsize = AtomicUsize::new(0);

/// The span was committed again after this give-back, so its address is mapped
/// now and a fault inside it is not explained by the give-back alone.
pub const DECOMMIT_RECOMMITTED: usize = 1 << 0;

/// Total granule spans this process has handed back to the OS.
/// Async-signal-safe.
pub fn decommits_total() -> usize {
    DECOMMIT_SEQ.load(Ordering::Relaxed)
}

/// Remember that `[addr, addr + len)` went back to the OS from `site`.
fn record_decommit(addr: usize, len: usize, site: &'static str) {
    let seq = DECOMMIT_SEQ.fetch_add(1, Ordering::Relaxed);
    let i = seq % RECENT_DECOMMITS;
    // Base last, for the reason on the ring's doc.
    DECOMMIT_BASE[i].store(0, Ordering::Relaxed);
    DECOMMIT_LEN[i].store(len, Ordering::Relaxed);
    DECOMMIT_FLAGS[i].store(0, Ordering::Relaxed);
    DECOMMIT_SITE_PTR[i].store(site.as_ptr() as usize, Ordering::Relaxed);
    DECOMMIT_SITE_LEN[i].store(site.len(), Ordering::Relaxed);
    DECOMMIT_AT[i].store(seq.wrapping_add(1), Ordering::Relaxed);
    DECOMMIT_BASE[i].store(addr, Ordering::Release);
}

/// Forget every remembered span inside `[addr, addr + len)`.
///
/// # An address means nothing once the mapping under it is gone
///
/// The ring keeps ABSOLUTE addresses, and a reservation's address space goes
/// back to the OS when it is dropped. The very next mapping -- another heap, a
/// `malloc` arena, anything -- can be handed the same addresses, at which point
/// a surviving entry would make the crash handler report a live, unrelated
/// mapping as "memory the collector gave away". That is a confident WRONG
/// answer, which is worse than the silence this ring was built to end.
///
/// Found by the Linux workspace run of 2026-09-05: with tests running in
/// parallel, one reservation's freed addresses were re-issued to another's, and
/// `recent_decommit_covering` answered with the dead one's span -- a 3-granule
/// give-back reported for a 2-granule one. It passed on Windows, whose address
/// reuse happened not to collide.
fn forget_decommits_in(addr: usize, len: usize) {
    let end = addr.saturating_add(len);
    for i in 0..RECENT_DECOMMITS {
        let base = DECOMMIT_BASE[i].load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let blen = DECOMMIT_LEN[i].load(Ordering::Relaxed);
        if addr < base.saturating_add(blen) && base < end {
            // Base first and alone: a reader that sees 0 skips the slot, so
            // there is no window in which it reads a half-cleared entry.
            DECOMMIT_BASE[i].store(0, Ordering::Release);
        }
    }
}

/// Flag every remembered span that `[addr, addr + len)` overlaps as re-taken.
///
/// Called from the commit path, which reaches the OS only for a RUN OF
/// UNCOMMITTED granules -- an allocation into already-backed memory never gets
/// here -- so the fixed scan is off every hot path.
fn note_recommit(addr: usize, len: usize) {
    let end = addr.saturating_add(len);
    for i in 0..RECENT_DECOMMITS {
        let base = DECOMMIT_BASE[i].load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let blen = DECOMMIT_LEN[i].load(Ordering::Relaxed);
        if addr < base.saturating_add(blen) && base < end {
            DECOMMIT_FLAGS[i].fetch_or(DECOMMIT_RECOMMITTED, Ordering::Relaxed);
        }
    }
}

/// Was `addr` inside one of the last [`RECENT_DECOMMITS`] spans this process
/// handed back to the OS? Returns `(base, len, site, flags)`.
///
/// Check [`DECOMMIT_RECOMMITTED`] in `flags` BEFORE concluding anything: a
/// re-committed span is mapped again, so a fault inside it is a different
/// question. A hit WITHOUT that flag, on a fault address, is a use-after-free
/// of heap memory and names the release site that made it fatal.
///
/// Async-signal-safe: atomic loads and a `'static` string slice.
pub fn recent_decommit_covering(addr: usize) -> Option<(usize, usize, &'static str, usize)> {
    // NEWEST WINS. Scanning in slot order and taking the first hit lets a stale
    // entry outrank the accurate one for the same address -- the ring is a
    // circular buffer, so slot order is not time order.
    let mut best: Option<usize> = None;
    let mut best_at = 0usize;
    for i in 0..RECENT_DECOMMITS {
        let base = DECOMMIT_BASE[i].load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let len = DECOMMIT_LEN[i].load(Ordering::Relaxed);
        if addr >= base && addr < base.saturating_add(len) {
            let at = DECOMMIT_AT[i].load(Ordering::Relaxed);
            if best.is_none() || at > best_at {
                best = Some(i);
                best_at = at;
            }
        }
    }
    // Re-read the winner. A slot cleared between the scan and here reads as
    // base 0, and answering `None` for a span that has just been forgotten is
    // the safe direction: it costs a line in a crash report, where the other
    // direction names memory that now belongs to someone else.
    let i = best?;
    let base = DECOMMIT_BASE[i].load(Ordering::Acquire);
    if base == 0 {
        return None;
    }
    let len = DECOMMIT_LEN[i].load(Ordering::Relaxed);
    let ptr = DECOMMIT_SITE_PTR[i].load(Ordering::Relaxed) as *const u8;
    let slen = DECOMMIT_SITE_LEN[i].load(Ordering::Relaxed);
    let site = if ptr.is_null() || slen == 0 || slen > 128 {
        "?"
    } else {
        // SAFETY: written by `record_decommit` from a `&'static str`, whose
        // bytes are valid UTF-8 and outlive the process.
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, slen)) }
    };
    Some((base, len, site, DECOMMIT_FLAGS[i].load(Ordering::Relaxed)))
}

/// The heap's backing bytes.
pub enum HeapStore {
    /// The pre-existing `alloc_zeroed` block. Wholly committed at construction.
    Owned(Vec<u8>),
    /// Reserved address space, committed in [`GRANULE`] units.
    Reserved(Reservation),
}

impl HeapStore {
    /// Back a heap of `capacity` bytes.
    ///
    /// Falls back to [`HeapStore::Owned`] when the kill switch is set, when the
    /// capacity is below one granule (a reservation would round it up and the
    /// bookkeeping would cost more than the heap), or when the OS refuses the
    /// reservation.
    pub fn new(capacity: usize, region: &str) -> Self {
        if capacity == 0 {
            return HeapStore::Owned(Vec::new());
        }
        if reserve_enabled() && capacity >= GRANULE {
            if let Some(res) = Reservation::reserve(capacity) {
                return HeapStore::Reserved(res);
            }
            // A REFUSED RESERVATION IS NOT A FAILURE HERE. `alloc_zeroed` may
            // still succeed -- it can serve from an already-mapped arena the
            // process holds -- so falling through gives the run the same chance
            // it had before this module existed. `heap_reservation_failed` is
            // still reached if that fails too.
            tracing::warn!(
                target: "cratonvm::gc",
                capacity,
                region,
                "gc: could not reserve address space for the heap; falling back \
                 to a wholly-committed block (no lazy commit, no uncommit)"
            );
        }
        HeapStore::Owned(crate::arena::alloc_zeroed_heap(capacity, region))
    }

    /// Base of the backing bytes. Stable for the life of the store.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        match self {
            HeapStore::Owned(v) => v.as_ptr(),
            HeapStore::Reserved(r) => r.base,
        }
    }

    /// Base of the backing bytes, mutably.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            HeapStore::Owned(v) => v.as_mut_ptr(),
            HeapStore::Reserved(r) => r.base,
        }
    }

    /// Capacity in bytes. Every offset in `[0, len())` is addressable; only
    /// the committed prefix/suffix is WRITABLE, which is what
    /// [`Self::commit_range`] is for.
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            HeapStore::Owned(v) => v.len(),
            HeapStore::Reserved(r) => r.len,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes currently committed. Equal to [`Self::len`] on the owned arm.
    pub fn committed_bytes(&self) -> usize {
        match self {
            HeapStore::Owned(v) => v.len(),
            HeapStore::Reserved(r) => r.committed_granules() * GRANULE,
        }
    }

    /// Make `[offset, offset + len)` writable, committing whatever granules it
    /// touches.
    ///
    /// **Every write into the store must be preceded by one of these**, and the
    /// allocator's two cursors are where that obligation is discharged: a bump
    /// commits what it is about to hand out, and a free-list block is by
    /// construction below a cursor that already committed it.
    ///
    /// Idempotent and cheap when the range is already committed -- a granule
    /// bitmap test per granule, no syscall. Returns `false` when the OS refuses,
    /// which the caller must treat as an allocation failure rather than
    /// ignoring: writing into uncommitted reserved space is a segfault, not a
    /// zero.
    #[must_use = "an uncommitted range must not be written to"]
    pub fn commit_range(&mut self, offset: usize, len: usize) -> bool {
        match self {
            HeapStore::Owned(_) => true,
            HeapStore::Reserved(r) => r.commit_range(offset, len),
        }
    }

    /// Zero `[start, end)`.
    ///
    /// Replaces `self.data[start..end].fill(0)` on the `Vec` this used to be.
    /// On the reserved arm it skips uncommitted granules, which is both the
    /// correct answer (they already read as zero, and writing to one would
    /// fault) and the fast one: `Arena::reset`'s full clear on a heap that has
    /// only ever touched its first megabyte costs a `memset` of that megabyte
    /// rather than of `-Xmx`.
    pub fn fill_zero(&mut self, start: usize, end: usize) {
        match self {
            HeapStore::Owned(v) => {
                let end = end.min(v.len());
                if end > start {
                    v[start..end].fill(0);
                }
            }
            HeapStore::Reserved(r) => r.fill_zero(start, end),
        }
    }

    /// The COMMITTED parts of `[start, end)` as absolute `(addr, len)` spans,
    /// adjacent ones coalesced — exactly the bytes [`Self::fill_zero`] would
    /// touch, handed to a caller that will zero them somewhere else (the
    /// generational heap's off-pause wipe of its evacuated semi-space).
    pub fn committed_spans(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        match self {
            HeapStore::Owned(v) => {
                let end = end.min(v.len());
                if end > start {
                    vec![(v.as_ptr() as usize + start, end - start)]
                } else {
                    Vec::new()
                }
            }
            HeapStore::Reserved(r) => r.committed_spans(start, end),
        }
    }

    /// Grow the usable capacity to `new_capacity`, returning the (possibly
    /// new) base pointer.
    ///
    /// `Arena::grow`'s backing operation. On the reserved arm a growth that
    /// still fits inside the reservation is free -- the address space is
    /// already there and the granules commit on demand -- which is the case
    /// that matters, because a reservation is rounded UP to a granule. A growth
    /// past the reservation takes a fresh one and copies, exactly as
    /// `Vec::resize` would reallocate; callers already treat the returned
    /// pointer as authoritative for that reason.
    pub fn grow_to(&mut self, new_capacity: usize) -> *const u8 {
        match self {
            HeapStore::Owned(v) => {
                if new_capacity > v.len() {
                    v.resize(new_capacity, 0);
                }
                v.as_ptr()
            }
            HeapStore::Reserved(r) => {
                if new_capacity <= r.reserved_len {
                    r.len = r.len.max(new_capacity);
                    return r.base;
                }
                match Reservation::reserve(new_capacity) {
                    Some(mut next) => {
                        // COPY ONLY WHAT IS COMMITTED IN THE SOURCE.
                        //
                        // A first version copied `min(old_len, new_len)` bytes,
                        // reasoning that everything inside the usable length is
                        // readable. It is not: with a lazy commit the source's
                        // uncommitted granules are `PROT_NONE` / reserved-only,
                        // so the READ faults -- and it faults inside a
                        // `copy_nonoverlapping`, which is a bare
                        // `STATUS_ACCESS_VIOLATION` with the collector's own
                        // frames on the stack and no clue that a commit is what
                        // was missing. `large_array_survives_gc` found it.
                        //
                        // Granule by granule, skipping the uncommitted ones,
                        // which is exactly `fill_zero`'s shape and for the same
                        // reason: an uncommitted granule holds nothing, so
                        // there is nothing to move and the destination's own
                        // fresh granule already reads as zero.
                        let live = r.len.min(next.len);
                        r.copy_committed_to(&mut next, live);
                        *r = next;
                        r.base
                    }
                    // The OS refused a bigger reservation. Keep what we have --
                    // the caller reads the capacity back and will fail its own
                    // allocation rather than write past the end.
                    None => r.base,
                }
            }
        }
    }

    /// The per-granule commit bitmap, shareable without the arena lock.
    ///
    /// `None` on the [`HeapStore::Owned`] arm, which is wholly committed and
    /// therefore has nothing to screen — read that as "every offset in
    /// `[0, len())` is backed", NOT as "no information".
    ///
    /// # Why a reader needs this
    ///
    /// `-Xmx` buys ADDRESS SPACE; only the granules the allocator's cursor has
    /// reached are mapped. A reader that knows only `[base, base + capacity)`
    /// — which is what the generational heap's lock-free `region_bounds`
    /// mirror publishes — will happily dereference an address in the reserved
    /// middle and take a SIGSEGV. `is_object_address` did exactly that on
    /// every conservative stack word that happened to look like a heap
    /// pointer, which is how a third of the Spring Framework suite died under
    /// `-XX:+UseGenerationalGC` while ZGC (whose validator consults a live-base
    /// registry and never speculatively dereferences) passed all of it.
    ///
    /// The returned `Arc` keeps the words alive independently of the
    /// reservation, so a `grow_to` that swaps the whole `Reservation` out
    /// leaves a holder reading a stale-but-mapped bitmap rather than freed
    /// memory. Stale in the safe direction, too: the old map's bits are a
    /// SUBSET of the new one's for the range they share, so the worst case is
    /// declining a granule that has since been committed.
    pub fn commit_bits(&self) -> Option<Arc<[AtomicU64]>> {
        match self {
            HeapStore::Owned(_) => None,
            HeapStore::Reserved(r) => Some(Arc::clone(&r.granules)),
        }
    }

    /// Is the granule holding `offset` committed, i.e. safe to READ?
    ///
    /// Lazy commit turns a speculative read of untouched heap space from
    /// "returns zero" into "faults", which is a new hazard for the handful of
    /// callers that deliberately look at an address the allocator has retracted
    /// past. Always `true` on the owned arm, where everything is committed.
    pub fn is_committed_at(&self, offset: usize) -> bool {
        match self {
            HeapStore::Owned(v) => offset < v.len(),
            HeapStore::Reserved(r) => offset < r.len && r.is_committed(offset / GRANULE),
        }
    }

    /// Hand `[offset, offset + len)` back to the OS, keeping the address space
    /// reserved.
    ///
    /// Only WHOLE granules inside the range are released; a partial granule at
    /// either end is left committed, because the bytes beside it are still in
    /// use. Decommitted memory reads as zero again when re-committed, which is
    /// the same contract a fresh granule has.
    ///
    /// **The caller must prove the range holds nothing live.** There is no
    /// check here and there cannot be one: this type knows about bytes, not
    /// objects.
    ///
    /// `site` names the collector-level reason, and it is not decoration: it
    /// is stored in the recently-decommitted ring and printed by the crash
    /// handler, where it is the difference between "a free-listed span was
    /// still reachable" (a missing root) and "the cursor retracted over live
    /// bytes" (a sweep defect). See [`recent_decommit_covering`].
    pub fn decommit_range(&mut self, offset: usize, len: usize, site: &'static str) -> usize {
        match self {
            HeapStore::Owned(_) => 0,
            HeapStore::Reserved(r) => r.decommit_range(offset, len, site),
        }
    }
}

/// Is granule `g` marked committed in a bitmap obtained from
/// [`HeapStore::commit_bits`]?
///
/// `false` for an index past the end of the map: a granule the reservation
/// does not have cannot be backed. The load is `Acquire` so it pairs with the
/// `Release` in `Reservation::set_committed` — observing the bit implies
/// observing the commit syscall that made the granule readable.
#[inline]
pub fn granule_committed(bits: &[AtomicU64], g: usize) -> bool {
    bits.get(g >> 6)
        .is_some_and(|w| w.load(Ordering::Acquire) & (1u64 << (g & 63)) != 0)
}

/// Are all the granules spanned by `[offset, offset + len)` committed?
///
/// The question a reader actually has: a 16-byte object header two words
/// short of a granule boundary straddles two granules, and finding the first
/// backed says nothing about the second.
#[inline]
pub fn range_committed(bits: &[AtomicU64], offset: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let last = match offset.checked_add(len - 1) {
        Some(e) => e / GRANULE,
        None => return false,
    };
    (offset / GRANULE..=last).all(|g| granule_committed(bits, g))
}

/// Reserved address space with per-granule commit tracking.
pub struct Reservation {
    base: *mut u8,
    /// The capacity the heap ASKED for. `-Xmx` must mean what it says, so this
    /// is what `HeapStore::len` reports and what every offset is bounded by --
    /// not [`Self::reserved_len`], which is rounded up and would let a heap
    /// report up to one granule more than it was given.
    len: usize,
    /// Reserved address space, rounded up to a whole number of granules.
    reserved_len: usize,
    /// One bit per granule; set means committed.
    ///
    /// `AtomicU64` behind an `Arc` rather than a plain `Vec<u64>` so a READER
    /// can consult the map without taking the `Mutex<Arena>` that owns this
    /// reservation. That is what [`HeapStore::commit_bits`] exists for: the
    /// generational heap's conservative-root validator has to know whether a
    /// candidate address is backed before it dereferences it, and it runs on
    /// the mutator's own hot path where the arena lock is not available. The
    /// `Arc` keeps the words alive across a `grow_to` that replaces the whole
    /// reservation, so a reader holding the previous map sees stale bits
    /// rather than freed memory.
    granules: Arc<[AtomicU64]>,
    committed: usize,
}

// SAFETY: the same argument `Arena` already makes about its `Vec<u8>` -- the
// pointer is owned exclusively by this value, freed exactly once in `Drop`, and
// every access through it is serialised by the `Mutex<Arena>` that holds it.
unsafe impl Send for Reservation {}
// SAFETY: as above. `&Reservation` exposes no interior mutability.
unsafe impl Sync for Reservation {}

impl Reservation {
    fn reserve(capacity: usize) -> Option<Self> {
        let reserved_len = capacity.div_ceil(GRANULE) * GRANULE;
        let base = platform_reserve(reserved_len)?;
        let granule_count = reserved_len / GRANULE;
        Some(Self {
            base,
            len: capacity,
            reserved_len,
            granules: (0..granule_count.div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect(),
            committed: 0,
        })
    }

    /// Zero `[start, end)`, touching only the granules that are COMMITTED.
    ///
    /// An uncommitted granule already reads as zero and writing to it would
    /// fault, so skipping it is both the correct answer and the fast one -- a
    /// full-arena clear on a heap that has only ever used its first megabyte
    /// costs one `memset` of that megabyte rather than of `-Xmx`.
    fn fill_zero(&mut self, start: usize, end: usize) {
        let end = end.min(self.len);
        if end <= start {
            return;
        }
        let first = start / GRANULE;
        let last = (end - 1) / GRANULE;
        for g in first..=last {
            if !self.is_committed(g) {
                continue;
            }
            let lo = (g * GRANULE).max(start);
            let hi = ((g + 1) * GRANULE).min(end);
            // SAFETY: `[lo, hi)` lies inside a committed granule of this
            // reservation, so the bytes are mapped read-write.
            unsafe { std::ptr::write_bytes(self.base.add(lo), 0, hi - lo) };
        }
    }

    /// See [`HeapStore::committed_spans`].
    fn committed_spans(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let end = end.min(self.len);
        let mut out: Vec<(usize, usize)> = Vec::new();
        if end <= start {
            return out;
        }
        let first = start / GRANULE;
        let last = (end - 1) / GRANULE;
        for g in first..=last {
            if !self.is_committed(g) {
                continue;
            }
            let lo = (g * GRANULE).max(start);
            let hi = ((g + 1) * GRANULE).min(end);
            let addr = self.base as usize + lo;
            match out.last_mut() {
                Some((a, l)) if *a + *l == addr => *l += hi - lo,
                _ => out.push((addr, hi - lo)),
            }
        }
        out
    }

    fn committed_granules(&self) -> usize {
        self.committed
    }

    #[inline]
    fn is_committed(&self, g: usize) -> bool {
        granule_committed(&self.granules, g)
    }

    #[inline]
    fn set_committed(&mut self, g: usize, on: bool) {
        if let Some(w) = self.granules.get(g >> 6) {
            // `Release` on the SET so a reader that observes the bit also
            // observes the `mprotect`/`VirtualAlloc` that made the granule
            // readable (the syscall is ordered before this store on this
            // thread, and the reader's `Acquire` load pairs with it).
            if on {
                w.fetch_or(1u64 << (g & 63), Ordering::Release);
            } else {
                w.fetch_and(!(1u64 << (g & 63)), Ordering::Release);
            }
        }
    }

    /// Copy the COMMITTED granules of `[0, len)` into `dst`, committing each
    /// one there first. Uncommitted source granules are skipped: they hold
    /// nothing, and `dst`'s corresponding granules read as zero when they are
    /// eventually committed, which is the same thing.
    fn copy_committed_to(&self, dst: &mut Reservation, len: usize) {
        let len = len.min(self.len).min(dst.len);
        if len == 0 {
            return;
        }
        let last = (len - 1) / GRANULE;
        for g in 0..=last {
            if !self.is_committed(g) {
                continue;
            }
            let lo = g * GRANULE;
            let hi = ((g + 1) * GRANULE).min(len);
            if hi <= lo || !dst.commit_range(lo, hi - lo) {
                continue;
            }
            // SAFETY: `[lo, hi)` is committed in both -- in `self` by the test
            // above, in `dst` by the `commit_range` -- and the two reservations
            // are distinct mappings, so the ranges cannot overlap.
            unsafe { std::ptr::copy_nonoverlapping(self.base.add(lo), dst.base.add(lo), hi - lo) };
        }
    }

    fn commit_range(&mut self, offset: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let end = match offset.checked_add(len) {
            Some(e) if e <= self.reserved_len => e,
            // Out of the reservation entirely. Refusing is the only safe
            // answer: the caller is about to write there.
            _ => return false,
        };
        let first = offset / GRANULE;
        let last = (end - 1) / GRANULE;
        // ONE SYSCALL PER RUN, not per granule. A bump that crosses several
        // granules at once (a large array) is the common case for the high end.
        let mut run_start: Option<usize> = None;
        for g in first..=last {
            match (self.is_committed(g), run_start) {
                (false, None) => run_start = Some(g),
                (true, Some(s)) => {
                    if !self.commit_granules(s, g - s) {
                        return false;
                    }
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            if !self.commit_granules(s, last + 1 - s) {
                return false;
            }
        }
        true
    }

    fn commit_granules(&mut self, first: usize, count: usize) -> bool {
        if count == 0 {
            return true;
        }
        let bytes = count * GRANULE;
        // SAFETY: `first + count <= self.reserved_len / GRANULE` by the bounds
        // check, so the range is inside the reservation.
        let ok = unsafe { platform_commit(self.base.add(first * GRANULE), bytes) };
        if !ok {
            return false;
        }
        // This address is mapped again, so a later fault inside it is a
        // different question from the give-back that preceded it.
        note_recommit(self.base as usize + first * GRANULE, bytes);
        for g in first..first + count {
            if !self.is_committed(g) {
                self.set_committed(g, true);
                self.committed += 1;
            }
        }
        COMMITTED_BYTES.fetch_add(bytes, Ordering::Relaxed);
        true
    }

    fn decommit_range(&mut self, offset: usize, len: usize, site: &'static str) -> usize {
        if len == 0 || offset >= self.reserved_len {
            return 0;
        }
        let end = offset.saturating_add(len).min(self.reserved_len);
        // WHOLE granules only, so the round is INWARD at both ends: a partial
        // granule at either edge has live bytes beside it.
        let first = offset.div_ceil(GRANULE);
        let last = end / GRANULE; // exclusive
        if last <= first {
            return 0;
        }
        let mut released = 0usize;
        let mut run_start: Option<usize> = None;
        for g in first..last {
            match (self.is_committed(g), run_start) {
                (true, None) => run_start = Some(g),
                (false, Some(s)) => {
                    released += self.decommit_granules(s, g - s, site);
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            released += self.decommit_granules(s, last - s, site);
        }
        released
    }

    fn decommit_granules(&mut self, first: usize, count: usize, site: &'static str) -> usize {
        if count == 0 {
            return 0;
        }
        let bytes = count * GRANULE;
        // Remember the span BEFORE the syscall. A reader of the ring that sees
        // a span still mapped is harmless; one that faults on a span the ring
        // does not yet know about gets a crash report with no answer in it,
        // which is the whole failure this ring exists to prevent.
        record_decommit(self.base as usize + first * GRANULE, bytes, site);
        // CLEAR THE BITS BEFORE THE SYSCALL, not after.
        //
        // The bitmap is read lock-free (`HeapStore::commit_bits`, consulted by
        // the generational heap's conservative-root validator on the mutator's
        // own path). Unmapping first and clearing after leaves a window in
        // which a reader is told a granule is backed when it has already been
        // handed to the OS -- which is a SIGSEGV in the reader, not a wrong
        // answer. Clearing first makes the window fail-SAFE instead: a reader
        // declines a granule that is still mapped, and declining costs nothing
        // but a conservative root this pass will re-find on the next one.
        let mut cleared: Vec<usize> = Vec::new();
        for g in first..first + count {
            if self.is_committed(g) {
                self.set_committed(g, false);
                self.committed -= 1;
                cleared.push(g);
            }
        }
        // SAFETY: bounded by the caller, as `commit_granules`.
        if !unsafe { platform_decommit(self.base.add(first * GRANULE), bytes) } {
            // A REFUSED DECOMMIT IS NOT AN ERROR. The bytes stay committed and
            // usable; the only cost is that they stay resident. Reporting zero
            // keeps the accounting honest -- and the bits speculatively cleared
            // above have to go back, or the store would believe it owns less
            // than it does and re-commit a granule it never released.
            for g in cleared {
                self.set_committed(g, true);
                self.committed += 1;
            }
            return 0;
        }
        COMMITTED_BYTES.fetch_sub(bytes, Ordering::Relaxed);
        bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.committed != 0 {
            COMMITTED_BYTES.fetch_sub(self.committed * GRANULE, Ordering::Relaxed);
        }
        // BEFORE the addresses go back to the OS, not after: once released they
        // can be re-issued to anything, and a surviving ring entry would name
        // the new owner's memory. See `forget_decommits_in`.
        forget_decommits_in(self.base as usize, self.reserved_len);
        // SAFETY: `base`/`len` are exactly what `platform_reserve` returned and
        // this value owns them uniquely.
        unsafe { platform_release(self.base, self.reserved_len) };
    }
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------
//
// Declared directly rather than through `libc` / `windows-sys`, following
// `jit/src/platform.rs`, which does the same for the code cache: the gc crate
// has no platform dependency today and these are four functions with a stable
// ABI on each side.

#[cfg(target_os = "windows")]
const MEM_COMMIT: u32 = 0x1000;
#[cfg(target_os = "windows")]
const MEM_RESERVE: u32 = 0x2000;
#[cfg(target_os = "windows")]
const MEM_DECOMMIT: u32 = 0x4000;
#[cfg(target_os = "windows")]
const MEM_RELEASE: u32 = 0x8000;
#[cfg(target_os = "windows")]
const PAGE_READWRITE: u32 = 0x04;
#[cfg(target_os = "windows")]
const PAGE_NOACCESS: u32 = 0x01;

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn VirtualAlloc(
        lpAddress: *mut u8,
        dwSize: usize,
        flAllocationType: u32,
        flProtect: u32,
    ) -> *mut u8;
    fn VirtualFree(lpAddress: *mut u8, dwSize: usize, dwFreeType: u32) -> i32;
}

/// Reserve `len` bytes of address space with no commit charge.
#[cfg(target_os = "windows")]
fn platform_reserve(len: usize) -> Option<*mut u8> {
    // `PAGE_NOACCESS` because reserved-only pages have no protection of their
    // own; the commit below is what installs `PAGE_READWRITE`.
    let p = unsafe { VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE, PAGE_NOACCESS) };
    (!p.is_null()).then_some(p)
}

#[cfg(target_os = "windows")]
unsafe fn platform_commit(addr: *mut u8, len: usize) -> bool {
    // Committing an already-committed page is legal and a no-op, so the granule
    // bitmap is an optimisation rather than a correctness requirement here.
    !VirtualAlloc(addr, len, MEM_COMMIT, PAGE_READWRITE).is_null()
}

#[cfg(target_os = "windows")]
unsafe fn platform_decommit(addr: *mut u8, len: usize) -> bool {
    // MEM_DECOMMIT, never MEM_RELEASE: the address space must stay reserved or
    // another allocation could take it and the heap's base would no longer
    // describe a contiguous region.
    VirtualFree(addr, len, MEM_DECOMMIT) != 0
}

#[cfg(target_os = "windows")]
unsafe fn platform_release(addr: *mut u8, _len: usize) {
    // `MEM_RELEASE` requires a zero size and the original base.
    let _ = VirtualFree(addr, 0, MEM_RELEASE);
}

#[cfg(unix)]
const PROT_NONE: i32 = 0x0;
#[cfg(unix)]
const PROT_READ: i32 = 0x1;
#[cfg(unix)]
const PROT_WRITE: i32 = 0x2;
#[cfg(unix)]
const MAP_PRIVATE: i32 = 0x02;
#[cfg(all(unix, not(target_os = "macos")))]
const MAP_ANONYMOUS: i32 = 0x20;
#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: i32 = 0x1000;
#[cfg(unix)]
const MAP_FAILED: *mut u8 = !0usize as *mut u8;
/// `MADV_DONTNEED` on Linux. Drops the pages; the next touch faults in a fresh
/// zero page, which is the contract [`HeapStore::decommit_range`] states.
#[cfg(all(unix, not(target_os = "macos")))]
const MADV_DONTNEED: i32 = 4;
/// macOS spells the same intent `MADV_FREE`.
#[cfg(target_os = "macos")]
const MADV_DONTNEED: i32 = 5;

#[cfg(unix)]
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// Reserve `len` bytes with `PROT_NONE`, which charges no commit against the
/// overcommit accounting on any Unix and faults nothing in.
#[cfg(unix)]
fn platform_reserve(len: usize) -> Option<*mut u8> {
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    (p != MAP_FAILED && !p.is_null()).then_some(p)
}

#[cfg(unix)]
unsafe fn platform_commit(addr: *mut u8, len: usize) -> bool {
    mprotect(addr, len, PROT_READ | PROT_WRITE) == 0
}

#[cfg(unix)]
unsafe fn platform_decommit(addr: *mut u8, len: usize) -> bool {
    // `madvise` FIRST, then drop the protection. The advise is what actually
    // returns the physical pages; the `mprotect` back to `PROT_NONE` is what
    // makes a write to a decommitted granule a fault rather than a silent
    // resurrection of a page the collector has written off -- the same
    // behaviour Windows gives for free after `MEM_DECOMMIT`.
    let advised = madvise(addr, len, MADV_DONTNEED) == 0;
    let protected = mprotect(addr, len, PROT_NONE) == 0;
    advised && protected
}

#[cfg(unix)]
unsafe fn platform_release(addr: *mut u8, len: usize) {
    let _ = munmap(addr, len);
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_reserve(_len: usize) -> Option<*mut u8> {
    None
}
#[cfg(not(any(unix, target_os = "windows")))]
unsafe fn platform_commit(_addr: *mut u8, _len: usize) -> bool {
    false
}
#[cfg(not(any(unix, target_os = "windows")))]
unsafe fn platform_decommit(_addr: *mut u8, _len: usize) -> bool {
    false
}
#[cfg(not(any(unix, target_os = "windows")))]
unsafe fn platform_release(_addr: *mut u8, _len: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A reservation commits nothing until it is asked to.**
    ///
    /// The whole point on Windows, where the `alloc_zeroed` block this replaces
    /// takes the full `-Xmx` commit charge before the VM has run a bytecode.
    #[test]
    fn a_fresh_reservation_has_committed_nothing() {
        let Some(res) = Reservation::reserve(16 * GRANULE) else {
            return; // no reservation on this platform; the fallback covers it
        };
        assert_eq!(res.committed_granules(), 0);
        assert_eq!(res.len, 16 * GRANULE);
        assert_eq!(res.reserved_len, 16 * GRANULE);
    }

    /// **Committing is idempotent, and only the granules touched are charged.**
    #[test]
    fn committing_a_range_charges_only_the_granules_it_spans() {
        let Some(mut res) = Reservation::reserve(16 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 1));
        assert_eq!(
            res.committed_granules(),
            1,
            "one byte must commit one granule"
        );
        assert!(res.commit_range(0, 1));
        assert_eq!(
            res.committed_granules(),
            1,
            "committing twice must not double-charge"
        );
        // Spanning a boundary takes both.
        assert!(res.commit_range(GRANULE - 8, 16));
        assert_eq!(res.committed_granules(), 2);
    }

    /// **Committed memory is writable and reads as zero.**
    ///
    /// `Arena` inherited that contract from `alloc_zeroed` and depends on it:
    /// the bump path hands out memory without zeroing it, and only reused
    /// free-list blocks are explicitly cleared.
    #[test]
    fn committed_memory_is_zero_and_writable() {
        let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, GRANULE));
        // SAFETY: the range was just committed.
        unsafe {
            let p = res.base;
            for i in [0usize, 1, 4095, GRANULE - 1] {
                assert_eq!(*p.add(i), 0, "byte {i} of a fresh granule is not zero");
            }
            *p.add(1234) = 0xAB;
            assert_eq!(*p.add(1234), 0xAB);
        }
    }

    /// **Decommitting rounds INWARD, so a partial granule is never dropped.**
    ///
    /// The bytes beside a partial granule are still in use, and a decommit that
    /// rounded outward would take them with it -- silently, because the next
    /// read of a dropped page returns zero rather than faulting.
    #[test]
    fn decommit_releases_only_whole_granules_inside_the_range() {
        let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 8 * GRANULE));
        assert_eq!(res.committed_granules(), 8);
        // A range that starts and ends mid-granule: only granules 2..5 are
        // wholly inside it.
        let released = res.decommit_range(GRANULE + 16, 4 * GRANULE, "test");
        if released == 0 {
            return; // platform declined; nothing to assert about the split
        }
        assert_eq!(released, 3 * GRANULE);
        assert_eq!(res.committed_granules(), 5);
        assert!(
            res.is_committed(1),
            "the partial leading granule must survive"
        );
        assert!(
            res.is_committed(5),
            "the partial trailing granule must survive"
        );
        assert!(!res.is_committed(2));
        assert!(!res.is_committed(4));
    }

    /// **A decommitted granule comes back zeroed.**
    ///
    /// The contract `decommit_range` states, and the one that makes reuse safe:
    /// a re-committed granule must look like a fresh one, not like whatever the
    /// dead objects left.
    #[test]
    fn a_recommitted_granule_reads_as_zero_again() {
        let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 2 * GRANULE));
        // SAFETY: committed above.
        unsafe { *res.base.add(GRANULE + 99) = 0xCD };
        if res.decommit_range(GRANULE, GRANULE, "test") == 0 {
            return; // platform declined
        }
        assert!(res.commit_range(GRANULE, GRANULE));
        // SAFETY: re-committed above.
        assert_eq!(unsafe { *res.base.add(GRANULE + 99) }, 0);
    }

    /// **Committing past the end of the reservation is refused, not clamped.**
    ///
    /// The caller is about to WRITE there. A clamp would return `true` for a
    /// range it did not make writable.
    #[test]
    fn committing_out_of_bounds_is_refused() {
        let Some(mut res) = Reservation::reserve(2 * GRANULE) else {
            return;
        };
        assert!(!res.commit_range(0, 4 * GRANULE));
        assert!(!res.commit_range(2 * GRANULE, 1));
    }

    /// **THE RING NAMES THE SPAN, THE SITE, AND WHETHER IT CAME BACK.**
    ///
    /// This is the instrument a heap use-after-free is diagnosed with: the
    /// crash handler asks `recent_decommit_covering(fault_addr)` and prints the
    /// answer. Three things have to hold for that to be worth printing, and all
    /// three are asserted here, because each one silently degrades the report
    /// to a wrong answer rather than to no answer:
    ///
    ///  * an address inside a released span is FOUND, and one outside it is not
    ///    -- a ring that reports every address explains every crash as a
    ///    use-after-free;
    ///  * the SITE survives the round trip, since it is what distinguishes a
    ///    missing root from a mis-sized sweep;
    ///  * a span committed again is FLAGGED rather than forgotten. An erased
    ///    slot reads as "no record", which is indistinguishable from a wild
    ///    pointer -- the exact confusion the ring exists to end.
    #[test]
    fn the_decommit_ring_answers_for_a_released_span_and_flags_its_return() {
        let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 8 * GRANULE));
        let base = res.base as usize;
        let before = decommits_total();
        if res.decommit_range(2 * GRANULE, 2 * GRANULE, "free-list-low") == 0 {
            return; // platform declined the give-back; nothing was recorded
        }
        assert!(decommits_total() > before, "the give-back must be counted");

        let hit = recent_decommit_covering(base + 2 * GRANULE + 4096)
            .expect("an address inside the released span must be found");
        assert_eq!(hit.0, base + 2 * GRANULE);
        assert_eq!(hit.1, 2 * GRANULE);
        assert_eq!(hit.2, "free-list-low", "the site must survive the ring");
        assert_eq!(
            hit.3 & DECOMMIT_RECOMMITTED,
            0,
            "nothing has re-committed this span yet"
        );

        // Still committed, and never released: a hit here would make every
        // report a false positive.
        assert!(
            recent_decommit_covering(base + 7 * GRANULE).is_none(),
            "an address outside every released span must NOT be reported"
        );

        // And the trap: take it back, and the same address must now read as
        // reuse rather than as evidence.
        assert!(res.commit_range(2 * GRANULE, 2 * GRANULE));
        let after = recent_decommit_covering(base + 2 * GRANULE + 4096)
            .expect("the slot must be kept, not erased");
        assert_ne!(
            after.3 & DECOMMIT_RECOMMITTED,
            0,
            "a span committed again must be flagged, so the handler stops \
             calling it a use-after-free"
        );
    }

    /// **A DROPPED RESERVATION'S SPANS MUST BE FORGOTTEN**, or the ring names
    /// memory that now belongs to someone else.
    ///
    /// The ring keeps absolute addresses. A reservation's address space goes
    /// back to the OS when it drops, and the next mapping can be handed the
    /// same addresses — so an entry that outlived its reservation makes the
    /// crash handler report a live, unrelated mapping as "memory the collector
    /// gave away". A confident wrong answer, which is worse than the silence
    /// the ring exists to end.
    ///
    /// This is not hypothetical and it is not Windows-visible. The Linux
    /// workspace run of 2026-09-05 failed
    /// `the_decommit_ring_answers_for_a_released_span_and_flags_its_return`
    /// with `left: 6291456, right: 4194304`: tests run in parallel, one
    /// reservation's freed addresses were re-issued to another's, and the scan
    /// answered with the DEAD reservation's 3-granule span for a 2-granule
    /// give-back. The same code passed on Windows, whose reuse pattern happened
    /// not to collide — a platform difference standing in for a real defect.
    #[test]
    fn a_dropped_reservations_spans_are_forgotten() {
        let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 4 * GRANULE));
        let base = res.base as usize;
        if res.decommit_range(GRANULE, 2 * GRANULE, "free-list-low") == 0 {
            return; // platform declined; nothing was recorded to forget
        }
        let probe = base + GRANULE + 4096;
        assert!(
            recent_decommit_covering(probe).is_some(),
            "the span must be findable while its reservation is alive"
        );
        drop(res);
        assert!(
            recent_decommit_covering(probe).is_none(),
            "a span whose reservation has been released must not be reported:              those addresses can already belong to another mapping"
        );
    }

    /// **THE NEWEST ENTRY WINS**, because slot order is not time order.
    ///
    /// The ring is circular, so a scan that takes the first covering slot it
    /// meets can return a stale entry that a newer, accurate one has already
    /// superseded. Two give-backs at the same address, the second narrower:
    /// a first-match scan answers with whichever landed in the lower slot.
    #[test]
    fn the_ring_answers_with_the_most_recent_span_covering_an_address() {
        let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 8 * GRANULE));
        let base = res.base as usize;
        if res.decommit_range(GRANULE, 4 * GRANULE, "unbumped-middle") == 0 {
            return;
        }
        // Re-commit and give back a NARROWER span inside the first one, so both
        // entries cover the probe and only the second is current.
        assert!(res.commit_range(GRANULE, 4 * GRANULE));
        if res.decommit_range(2 * GRANULE, GRANULE, "free-list-high") == 0 {
            return;
        }
        let probe = base + 2 * GRANULE + 64;
        let hit = recent_decommit_covering(probe).expect("covered by both spans");
        assert_eq!(
            (hit.1, hit.2),
            (GRANULE, "free-list-high"),
            "the ring must answer with the LATEST give-back covering the              address, not with whichever slot the scan reached first"
        );
    }

    /// **The owned fallback behaves as the block it replaces.**
    #[test]
    fn the_owned_store_is_wholly_committed_and_ignores_decommit() {
        let mut store = HeapStore::Owned(vec![0u8; 4096]);
        assert_eq!(store.len(), 4096);
        assert_eq!(store.committed_bytes(), 4096);
        assert!(store.commit_range(0, 4096));
        assert_eq!(store.decommit_range(0, 4096, "test"), 0);
        assert_eq!(store.committed_bytes(), 4096);
    }
}
