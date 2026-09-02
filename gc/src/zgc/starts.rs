// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The object-start bitmap: which addresses are allocation bases, and which
//! objects this cycle has marked.
//!
//! Split out of `zgc.rs` on 2026-09-02. It is one cohesive structure with a
//! narrow interface -- `insert` / `contains` / `remove` / `claim` /
//! `clear_all` / `set_range`, plus a frozen `snapshot` the collection phases
//! iterate -- and it was ~1,150 lines in the middle of a 24,000-line file.
//!
//! A CHILD module of `zgc`, which is what makes the move free: a child sees
//! its parent's private items, and `ZgcRealHeap` reaches these through the
//! `pub(crate)` markers below. Nothing outside this file changed visibility.
//!
//! Two instances exist over the identical geometry -- the object-start
//! registry and the mark bits (`ZgcRealHeap::mark_bits`) -- built through the
//! same constructor so they cannot disagree about which addresses are
//! representable. The `label` field is what tells their warnings apart.

use std::sync::atomic::{AtomicBool, AtomicUsize, AtomicU64, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashSet;

// ---------------------------------------------------------------------------
// Object-start membership for `ZgcRealHeap` — the allocation-path bitmap
// ---------------------------------------------------------------------------
//
// # Why (a measurement with an in-tree precedent, not a hypothesis)
//
// [`ZgcRealHeap::registry`] holds the base address of every live allocation and
// is written on EVERY allocation, from every mutator thread. It used to be a
// `Mutex<FxHashSet<usize>>`. `bench/BinTreesClassic.java`, release build,
// ABBA-interleaved, checksums identical on both sides (so this backend is
// CORRECT, just slow), 2026-08-07:
//
// ```text
//   depth 12   generational   35 ms    zgc    231 ms     6.6x
//   depth 14   generational  138 ms    zgc   1566 ms    11.3x
//   depth 16   generational  621 ms    zgc  11417 ms    18.4x
//   depth 18   generational  ~7.9 s    zgc   ~116 s     14.6x
// ```
//
// The ratio is SUPERLINEAR: each depth step is ~4x the objects, and the
// generational collector scales ~4x while this one scales ~7x. And
// `--verbose:gc` shows the depth-16 run performed exactly ONE collection
// (1.1 ms, `bytes_freed=0`), finishing at 719 MB against a 4.2 GB heap. So the
// cost is not the collector, and it is not allocation locking — TLABs
// ([`ZgcRealHeap::tlabs`]) landed first and did not move the number. It is
// per-allocation work on the mutator path that grows with the LIVE-OBJECT
// COUNT. Tens of millions of entries in one global hash set, behind one global
// mutex, probed with cache-missing scatter reads over a multi-gigabyte table,
// is exactly that shape and nothing else on the path is.
//
// # The precedent
//
// `gen_heap` had this defect and it was diagnosed and fixed on 2026-07-26
// (landed `7307935bc`). There a `young_object_starts: FxHashSet<usize>` made a
// moving collection cost O(objects *allocated*) instead of O(objects
// *surviving*): `perf` over bt18 measured `HashMap::insert` at 40.7% of the
// whole process plus `reserve_rehash` at 8.3% — 49% together. The fix was
// [`crate::young_mark::ObjectStartBits`], one bit per 8 bytes of
// `[base, base + used)`, and bt18 went 5.0x -> 2.1x. That finding's own
// generalisation is why this section exists: *any membership-over-a-contiguous-
// arena set in this GC is a bitmap candidate.* This registry is precisely that,
// and worse than the one that was fixed — `gen_heap`'s set was built once per
// collection; this one is written on every allocation from every thread under a
// single lock.
//
// # Why the two existing bitmaps could not be reused as-is
//
// * [`crate::young_mark::ObjectStartBits`] has exactly the right *semantics*
//   (it is alignment-exact: an unaligned address is rejected rather than
//   aliased onto a neighbour's bit) but its accessors take `&mut self` over a
//   plain `Vec<u64>`, because its own doc says it is "single-threaded by
//   construction — the walk and the forwarding that reads it both run inside
//   the collection's stop-the-world region". This registry is written from
//   concurrent mutators, so it cannot serve. It also has no `remove`, which the
//   sweep's in-place prune needs.
// * [`crate::young_mark::YoungMarkBits`] IS atomic and IS the allocation model
//   copied below (`alloc_zeroed` + `*mut AtomicU64` + `fetch_or`), but its
//   `locate` deliberately does NOT check alignment — `addr` and `addr + 4` map
//   to the same bit — because its callers pre-screen. Here that would be
//   unsound: [`ZgcRealHeap::is_object_address`] would answer `Some` for an
//   INTERIOR conservative-root candidate and hand back an `ObjectRef` pointing
//   four bytes into an object, which is the `is_addr_live` unsoundness
//   [`ZgcRealHeap::conservative_addr_span`] documents. It also has no `remove`.
//
// Neither file is edited. [`ZObjectStartBits`] below is the atomic twin:
// `YoungMarkBits`'s storage and RMW discipline, `ObjectStartBits`'s exactness
// rule, plus the `remove` the sweep needs.
//
// # Why the bitmap is EXACT here (three legs, each verified against the code)
//
// 1. **One contiguous region with a stable base.** [`ZgcRealHeap::with_capacity`]
//    creates a single [`Arena`] and captures `arena_base`/`arena_end` from it;
//    there is no `Arena::grow` call anywhere in this file and `alloc_raw` is the
//    single arena chokepoint, so the envelope never moves. Those two fields
//    already back [`ZgcRealHeap::conservative_addr_span`] for exactly this
//    reason.
// 2. **Every footprint is a multiple of 8.** `Arena::alloc` opens with
//    `let size = size.checked_add(7)? & !7` (`arena.rs:579`) and warns on an
//    unrounded request, so consecutive starts sit on the 8-byte grid.
// 3. **Every start is 8-aligned relative to `arena_base`.** All three of this
//    heap's paths into the arena ask for `align == 8`: `alloc_raw` calls
//    `arena.alloc(size, 8)`, `tlab_refill` calls `arena.alloc(want,
//    ZGC_TLAB_ALIGN)` with [`ZGC_TLAB_ALIGN`]` == 8`, and the bump path aligns
//    the *offset* (`arena.rs:634`) so `addr - arena_base` is a multiple of 8 by
//    construction.
//
// Therefore `(addr - arena_base) / 8` is a total, collision-free encoding of
// "is this an object start", and a non-multiple-of-8 offset is *provably* not
// one — which is what lets [`ZObjectStartBits::locate`] reject it instead of
// aliasing, preserving the exact-base answer the `FxHashSet` gave.
//
// **TLAB-served allocations preserve all three.** A TLAB is not a second
// allocator: `tlab_refill` carves its chunk out of this same arena with
// `arena.alloc(want, ZGC_TLAB_ALIGN)`, so the chunk base is on the grid; every
// object inside is bumped at [`ZGC_TLAB_ALIGN`] (the constant's own doc pins it
// at 8 precisely so "the cursor can never leave the object grid"); and
// [`zgc_tlab_footprint`] rounds each request to that alignment. So a TLAB start
// is `chunk_base + 8k` and `chunk_base` is `arena_base + 8j`.
//
// # The one leg that is NOT a proof, and the fallback that covers it
//
// `Arena`'s backing store is a `Vec<u8>`, whose pointer Rust only guarantees to
// be 1-aligned; and `Arena::alloc`'s FREE-LIST tiers align the *absolute*
// address (`arena.rs:481`, `:548`) while the bump tier aligns the *offset*
// (`arena.rs:634`). The two agree iff `arena_base` is itself 8-aligned, which
// every real allocator delivers for a multi-megabyte block but no type in this
// tree asserts. Rather than bet on it, [`ZObjectStartBits`] keeps an
// [`overflow`](ZObjectStartBits::overflow) set for any address the grid cannot
// encode. It is empty on every real run (and a one-shot `tracing::warn!` says
// so if it is not), it costs one relaxed load of a never-written cache line on
// the query path, and it means a base can never be LOST — losing one would make
// `is_object_address` deny a reachable object and drop it from conservative
// rooting, which is a crash, not a slowdown.

/// Runtime kill switch: `CRATONVM_ZGC_STARTBITS`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) puts the registry back on
/// the `Mutex<FxHashSet<usize>>` it used before this change, byte for byte, so
/// the A/B is a re-run and not a rebuild.
///
/// Same idiom, and the same two reasons, as [`zgc_tlab_enabled_by_default`]:
/// read through [`cratonvm_types::flags::runtime_var_os`] so it layers with
/// `-XX:` like every other flag, and deliberately NOT declared as a
/// [`cratonvm_types::GcFlags`] field, because a declared flag latches on first
/// read and a mid-run `set_var` then becomes invisible to the very suite that
/// wants to A/B it. Read once per heap, in [`ZgcRealHeap::with_capacity`].
/// Runtime kill switch: `CRATONVM_ZGC_MARKBITS`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) puts this cycle's mark bits
/// back in the object header's [`GC_FLAG_MARKED`], byte for byte, so the A/B is
/// a re-run and not a rebuild. See [`ZgcRealHeap::mark_bits`] for the three
/// costs the side bitmap removes.
///
/// Read once per heap, in [`ZgcRealHeap::with_capacity`], for the same two
/// reasons as [`zgc_start_bits_enabled_by_default`]: it layers with `-XX:` like
/// every other flag, and a declared `GcFlags` field would latch on first read
/// and make a mid-run `set_var` invisible to the suite that wants to A/B it.
pub(crate) fn zgc_mark_bits_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_MARKBITS") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

pub(crate) fn zgc_start_bits_enabled_by_default() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_STARTBITS") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// One bit per 8 bytes of the arena: "is this address an object start?"
///
/// The atomic, removable twin of [`crate::young_mark::ObjectStartBits`] — see
/// the section header above for why neither existing bitmap could be reused and
/// for the three-leg exactness argument.
///
/// # Memory
///
/// One bit per 8 bytes is **1/64th of the arena**, committed up front: ~1 MB for
/// the 64 MB default heap, ~66 MB for a 4.2 GB one. That is deliberate rather
/// than lazily committed. `alloc_zeroed` for a block this size goes straight to
/// the OS (`mmap`/`VirtualAlloc`) and the pages are demand-faulted anyway, so
/// the resident cost already tracks the part of the arena that has been
/// allocated into; a hand-rolled two-level commit would add a dependent load to
/// [`ZgcRealHeap::is_object_address`], which is on the mutator path via
/// `jit_checkcast`. It also compares favourably with what it replaces: the hash
/// set it removes cost ~8 bytes per LIVE OBJECT plus load factor — at bt16's
/// 719 MB of ~72-byte nodes that is well over 100 MB, and it grew without bound
/// because the collection that would prune it essentially never fires.
pub(crate) struct ZObjectStartBits {
    /// `alloc_zeroed` block of [`Self::nwords`] `AtomicU64`s, freed in `Drop`.
    /// Raw rather than `Box<[AtomicU64]>` for the same reason
    /// [`crate::young_mark::YoungMarkBits`] is: `AtomicU64` is not `Clone`, so
    /// `vec![]` cannot build one, and collecting an iterator would MEMSET tens
    /// of megabytes instead of taking a zero-page mapping.
    pub(crate) words: *mut AtomicU64,
    pub(crate) nwords: usize,
    /// The arena base. Bit `i` denotes `base + i * 8`.
    pub(crate) base: usize,
    /// Arena capacity in bytes; `[base, base + span)` is the covered range.
    pub(crate) span: usize,
    /// Bases the 8-byte grid cannot encode — see the section header's "one leg
    /// that is NOT a proof". Expected to stay empty forever.
    pub(crate) overflow: Mutex<FxHashSet<usize>>,
    /// `overflow.len()`, readable without taking the lock. The query path tests
    /// this before it will even consider locking, so the fallback costs a
    /// relaxed load of a line that is never written on a healthy run.
    pub(crate) overflow_len: AtomicUsize,
    /// One-shot latch for the "the grid could not encode a base" warning: one
    /// line per allocation would itself be the hang.
    pub(crate) overflow_warned: AtomicBool,
    /// Which bitmap this is, for the spill warning. Two instances exist over
    /// the SAME geometry -- the object-start registry and the mark bits (see
    /// [`ZgcRealHeap::mark_bits`]) -- and a warning that does not say which one
    /// spilled names the wrong invariant.
    pub(crate) label: &'static str,
}

// SAFETY: identical argument to `crate::young_mark::YoungMarkBits`. Every
// access to `words` is an atomic operation on `AtomicU64`; the allocation is
// owned exclusively by this value and freed exactly once in `Drop`. The
// `overflow` set is behind a `Mutex`.
unsafe impl Send for ZObjectStartBits {}
// SAFETY: as above — all shared access is atomic or mutex-guarded.
unsafe impl Sync for ZObjectStartBits {}

impl ZObjectStartBits {
    /// Cover `[base, base + span)` as the object-start registry.
    pub(crate) fn new(base: usize, span: usize) -> Self {
        Self::labelled(base, span, "object-start")
    }

    /// Cover `[base, base + span)` under a stated role.
    ///
    /// The mark bitmap is a second instance over the identical geometry, and
    /// that identity is the point: both are indexed off the same `arena_base`
    /// on the same 8-byte grid, so a base the registry can encode is a base the
    /// mark bits can encode, and neither can silently disagree with the other
    /// about which addresses are representable.
    pub(crate) fn labelled(base: usize, span: usize, label: &'static str) -> Self {
        let nwords = span.div_ceil(8).div_ceil(64);
        let words = if nwords == 0 {
            std::ptr::NonNull::<AtomicU64>::dangling().as_ptr()
        } else {
            let layout = std::alloc::Layout::array::<AtomicU64>(nwords)
                .expect("zgc object-start bitmap layout overflow");
            // SAFETY: `nwords > 0` so the layout is non-zero-sized, and an
            // all-zero bit pattern is a valid `AtomicU64` (value 0).
            let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut AtomicU64;
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            p
        };
        Self {
            words,
            nwords,
            base,
            span: if nwords == 0 { 0 } else { span },
            overflow: Mutex::new(FxHashSet::default()),
            overflow_len: AtomicUsize::new(0),
            overflow_warned: AtomicBool::new(false),
            label,
        }
    }

    /// Word index and bit mask for `addr`, or `None` when the grid cannot
    /// encode it.
    ///
    /// The `off & 7 != 0` rejection is the whole exactness argument in one
    /// line, and it is what [`crate::young_mark::YoungMarkBits`] omits: without
    /// it an interior address would alias its object's bit and
    /// [`ZgcRealHeap::is_object_address`] would promote a conservative-root
    /// candidate that is not a base.
    #[inline]
    pub(crate) fn locate(&self, addr: usize) -> Option<(usize, u64)> {
        let off = addr.checked_sub(self.base)?;
        if off >= self.span || off & 7 != 0 {
            return None;
        }
        let bit = off >> 3;
        Some((bit >> 6, 1u64 << (bit & 63)))
    }

    /// Record an object start.
    ///
    /// # Why `fetch_or` and not a CAS loop
    ///
    /// The word is shared with the 63 neighbouring 8-byte grid slots, so two
    /// threads allocating adjacent objects write the same word — a
    /// `load`/`or`/`store` would drop one of them. `fetch_or` is a single
    /// atomic read-modify-write, and because this bitmap is *monotone between
    /// collections* (inserts only ever set bits; the only clears happen in the
    /// stop-the-world sweep, with no mutator inserting) there is no value to
    /// re-check and therefore nothing for a retry loop to do. That is exactly
    /// [`crate::young_mark::YoungMarkBits::try_mark`]'s argument.
    ///
    /// `Release`: this is at least as strong as the `Mutex::unlock` it
    /// replaces. A thread that hands a fresh pointer to another thread does so
    /// through a plain field store, which supplies no edge of its own; keeping
    /// the release here means the reader's `Acquire` in [`Self::contains`]
    /// still cannot observe "not an object" for a pointer whose publication it
    /// has already seen. Relaxed would be sufficient under the memory model
    /// only if the publication path itself carried the edge, and it does not.
    #[inline]
    pub(crate) fn insert(&self, addr: usize) {
        match self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            Some((w, mask)) => unsafe {
                (*self.words.add(w)).fetch_or(mask, Ordering::Release);
            },
            None => self.spill(addr),
        }
    }

    /// The grid could not encode `addr`; keep it exactly, in the side set.
    #[cold]
    pub(crate) fn spill(&self, addr: usize) {
        {
            let mut overflow = self.overflow.lock();
            if overflow.insert(addr) {
                let n = overflow.len();
                self.overflow_len.store(n, Ordering::Release);
            }
        }
        if !self.overflow_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "zgc",
                addr = addr,
                base = self.base,
                span = self.span,
                label = self.label,
                "zgc bitmap: an address is off the 8-byte grid (or outside the \
                 arena) — falling back to the exact side set for it; membership \
                 stays correct, but every such address costs a lock and the \
                 bitmap is not carrying it"
            );
        }
    }

    /// Exact membership: is `addr` the base of a registered allocation?
    ///
    /// `Acquire` pairs with [`Self::insert`]'s `Release`; see there. The
    /// overflow probe is guarded by a relaxed-cost load that is zero on every
    /// healthy run, so the common answer is one aligned load and one mask.
    #[inline]
    pub(crate) fn contains(&self, addr: usize) -> bool {
        if let Some((w, mask)) = self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            if unsafe { (*self.words.add(w)).load(Ordering::Acquire) } & mask != 0 {
                return true;
            }
        }
        self.overflow_len.load(Ordering::Acquire) != 0 && self.overflow.lock().contains(&addr)
    }

    /// Clear one start. Called only from the sweep's in-place prune, inside the
    /// stop-the-world region.
    ///
    /// `fetch_and` rather than `load`/`store` for [`Self::insert`]'s reason —
    /// the word is shared with 63 neighbours, and although the sweep is the
    /// only writer *of this word's bits* while the world is stopped, using the
    /// RMW costs nothing and removes the assumption. `Release` keeps the clear
    /// no weaker than the `Mutex::unlock` it replaces, so the preceding
    /// zero-fill of the dead object cannot be observed after it.
    pub(crate) fn remove(&self, addr: usize) {
        if let Some((w, mask)) = self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            let prev = unsafe { (*self.words.add(w)).fetch_and(!mask, Ordering::Release) };
            if prev & mask != 0 {
                return;
            }
        }
        if self.overflow_len.load(Ordering::Acquire) != 0 {
            let mut overflow = self.overflow.lock();
            if overflow.remove(&addr) {
                let n = overflow.len();
                self.overflow_len.store(n, Ordering::Release);
            }
        }
    }

    /// Set the bit for `addr` and report whether **this call** set it.
    ///
    /// The exactly-once primitive the concurrent marker needs, and the reason
    /// the mark bits can leave the object header at all.
    /// [`ObjectHeader::try_add_gc_flags`] is a CAS loop on a byte that shares
    /// its word with `gc_age` and the sticky layout flags; this is one
    /// `fetch_or` on a word that carries nothing but mark bits, so there is no
    /// value to re-check and nothing for a retry loop to do.
    ///
    /// The caller's contract is `try_mark`'s verbatim: **`true` means you now
    /// own the obligation to scan it.** Two `true`s for one object would be a
    /// double scan (wasteful); a lost update would leave an object marked and
    /// never traced, so its children are swept while it is alive and pointing
    /// at them. `fetch_or` returns the previous word, which is exactly the
    /// evidence needed to tell the two apart.
    ///
    /// `AcqRel`: the `Release` half publishes whatever the marker wrote before
    /// claiming (nothing today, but the ordering must not depend on that), and
    /// the `Acquire` half pairs with a peer's claim so a worker that loses the
    /// race sees everything the winner did before it.
    ///
    /// Off-grid addresses go to the exact side set, where "did I insert it"
    /// is `FxHashSet::insert`'s own answer.
    #[inline]
    pub(crate) fn claim(&self, addr: usize) -> bool {
        match self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            Some((w, mask)) => {
                let word = unsafe { &*self.words.add(w) };
                // TEST BEFORE THE READ-MODIFY-WRITE. This is asked of every
                // EDGE, not every object, and most edges point at something
                // already marked -- a shared graph is why marking is a
                // traversal and not a walk. An unconditional `fetch_or` makes
                // each of those an exclusive-state acquisition of a cache line
                // every other worker is also writing; a plain load answers the
                // same question from a shared one.
                //
                // The RMW is still the arbiter, and this only skips the races
                // it would have lost: nothing clears a mark bit during a mark,
                // so "already set" is a stable answer, while "not set yet"
                // falls through and contends for the claim exactly as before.
                if word.load(Ordering::Relaxed) & mask != 0 {
                    return false;
                }
                let prev = word.fetch_or(mask, Ordering::AcqRel);
                prev & mask == 0
            }
            None => {
                let mut overflow = self.overflow.lock();
                let inserted = overflow.insert(addr);
                if inserted {
                    let n = overflow.len();
                    self.overflow_len.store(n, Ordering::Release);
                }
                inserted
            }
        }
    }

    /// Set every bit covering `[lo, hi)`, in one pass over the words.
    ///
    /// The bulk twin of [`Self::insert`], and the reason allocate-black can be
    /// a per-CHUNK operation instead of a per-OBJECT one. A 512 KiB TLAB chunk
    /// of 72-byte objects is ~7 000 objects and therefore ~7 000 `fetch_or`s on
    /// the allocation fast path; the same span here is 1 024 bitmap words, i.e.
    /// one `fetch_or` per 512 arena bytes whatever the object size.
    ///
    /// `fetch_or` and not a plain store even for the interior words: a
    /// concurrent marker may be claiming bits in the same words for objects
    /// that are not in this range, and a read-modify-write is the only thing
    /// that does not drop its claim. Setting a bit that is already set is a
    /// no-op, so there is no value to re-check and nothing for a retry loop to
    /// do -- the same argument as [`Self::insert`]'s.
    ///
    /// Addresses the grid cannot encode are NOT spilled: an interior address of
    /// the range is not an object base, so it can never be queried, and the
    /// only bases inside the range are 8-aligned by construction. The range
    /// itself is clamped to the covered span rather than spilling its ends.
    pub(crate) fn set_range(&self, lo: usize, hi: usize) {
        if self.nwords == 0 || hi <= lo {
            return;
        }
        let lo_off = match lo.checked_sub(self.base) {
            Some(o) if o < self.span => o,
            _ => return,
        };
        let hi_off = hi.saturating_sub(self.base).min(self.span);
        if hi_off <= lo_off {
            return;
        }
        // Bit indices, INCLUSIVE of the first grid slot at or above `lo` and
        // EXCLUSIVE of the one at `hi`. `lo` is 8-aligned at every caller (a
        // chunk base or a TLAB cursor), so the round-up is a no-op there and a
        // conservative narrowing anywhere else -- it can only decline to set a
        // bit, never set one outside the range.
        let first = lo_off.div_ceil(8);
        let last = hi_off / 8; // exclusive
        if last <= first {
            return;
        }
        let (w0, b0) = (first >> 6, first & 63);
        let (w1, b1) = (last >> 6, last & 63);
        if w0 == w1 {
            // `last > first` and both land in one word, so `0 <= b0 < b1 <= 63`
            // and the shift below cannot overflow.
            let mask = (!0u64 << b0) & ((1u64 << b1) - 1);
            // SAFETY: `first < last <= span / 8`, so `w0 < nwords`.
            unsafe { (*self.words.add(w0)).fetch_or(mask, Ordering::Release) };
            return;
        }
        // SAFETY for all three: every index is derived from an offset bounded
        // by `span`, and `nwords` covers `span.div_ceil(8).div_ceil(64)`.
        unsafe {
            (*self.words.add(w0)).fetch_or(!0u64 << b0, Ordering::Release);
            for w in (w0 + 1)..w1.min(self.nwords) {
                (*self.words.add(w)).fetch_or(!0u64, Ordering::Release);
            }
            if b1 != 0 && w1 < self.nwords {
                (*self.words.add(w1)).fetch_or((1u64 << b1) - 1, Ordering::Release);
            }
        }
    }

    /// Clear **every** bit, in one pass over the words.
    ///
    /// # Why this is the whole argument for a side bitmap
    ///
    /// The state it replaces is "walk the registry and clear
    /// [`GC_FLAG_MARKED`] in every object's header". That walk is one
    /// read-modify-write per object, scattered over the whole arena, each one
    /// dirtying a 64-byte line that has to be written back — and the 2026-08-17
    /// pause anatomy measured it at **94-96% of the mark-start pause** (34 ms
    /// of 35 on a 4.6M-entry registry, 66 of 69 on a 10.8M one). Here the same
    /// operation is a sequential store over `span / 512` bytes: 8 MB for a
    /// 4.2 GB heap, touched linearly, with no dependency on how many objects
    /// the heap holds.
    ///
    /// **STOP-THE-WORLD ONLY.** `Relaxed` stores plus one trailing `Release`
    /// fence rather than a released store per word: with every mutator parked
    /// there is no concurrent reader to order against word by word, and the
    /// single fence is what the resumed mutators synchronise with. A caller
    /// that runs this with the world alive would erase claims out from under
    /// the marker, which is a use-after-free, not a torn read -- so the
    /// ordering is not what protects it and the caller's safepoint is.
    pub(crate) fn clear_all(&self) {
        for w in 0..self.nwords {
            // SAFETY: `w < self.nwords`, and `words` holds exactly that many.
            unsafe { (*self.words.add(w)).store(0, Ordering::Relaxed) };
        }
        if self.overflow_len.load(Ordering::Acquire) != 0 {
            self.overflow.lock().clear();
            self.overflow_len.store(0, Ordering::Release);
        }
        std::sync::atomic::fence(Ordering::Release);
    }

    /// Is anything held outside the grid? See [`Self::overflow`].
    #[inline]
    pub(crate) fn has_spill(&self) -> bool {
        self.overflow_len.load(Ordering::Acquire) != 0
    }

    /// The greatest recorded start at or below `addr`, or `None`.
    ///
    /// This is the whole of interior-pointer resolution. Allocations do not
    /// overlap, so the ONLY base whose extent can contain `addr` is the
    /// greatest one `<= addr`: find it, deref one header, compare one extent.
    /// A forward walk reaches the same candidate — after visiting every base
    /// below it and dereferencing every one of their headers.
    ///
    /// WHY THAT MATTERED. `ZgcRealHeap::is_heap_addr` is not a GC-only path:
    /// `VmHeap::is_heap_addr`'s ZGC arm feeds it per-slot conservative root
    /// scanning over ambiguous JVM-long-vs-jobject operand words, so every
    /// interior pointer and every long bit pattern that happens to land inside
    /// the arena envelope bought a full walk of the object-start bitmap up to
    /// the arena's high-water mark. On a heap whose cursor has reached capacity
    /// that is the entire bitmap — ~3M word loads plus a header dereference per
    /// set bit, per probe. Measured with `perf record` on
    /// `type.temporal.InstantTests` (2026-08-11, Azure Linux, default
    /// collector): **80.7% of all CPU samples in `VmHeap::is_heap_addr`**, with
    /// another 11.5% in `object_body_size` — the header deref inside that walk.
    /// The class takes 6.7 s on real HotSpot and had not finished in 1800 s.
    ///
    /// Backwards, the scan stops at the first set bit below `addr`, which in a
    /// populated heap is a handful of words away. It is never worse in shape
    /// than the forward walk it replaces (both are bounded by the bitmap), and
    /// it dereferences exactly one header instead of one per live object.
    ///
    /// The `overflow` set is deliberately NOT consulted here — it carries no
    /// ordering, so "greatest at or below" is not a question it can answer. The
    /// caller keeps the full walk for that arm; see [`Self::has_spill`].
    pub(crate) fn nearest_base_at_or_below(&self, addr: usize) -> Option<usize> {
        if self.span == 0 || self.nwords == 0 {
            return None;
        }
        let off = addr.checked_sub(self.base)?;
        // Past the covered span: the last gridded slot is still the greatest
        // candidate at or below `addr`.
        let off = off.min(self.span - 1);
        let bit = off >> 3; // floor onto the 8-byte grid
        let mut w = bit >> 6;
        debug_assert!(
            w < self.nwords,
            "bit index is derived from a bounded offset"
        );
        // Keep only bits at or below `bit` in the first word. `u64::MAX >> k`
        // has its low `64 - k` bits set, so `k = 63 - (bit & 63)` leaves
        // exactly bits `0..=(bit & 63)`.
        let k = 63 - (bit & 63);
        // SAFETY: `w < self.nwords`, checked above.
        let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) } & (u64::MAX >> k);
        loop {
            if word != 0 {
                let b = 63 - word.leading_zeros() as usize;
                return Some(self.base + (((w << 6) | b) << 3));
            }
            if w == 0 {
                return None;
            }
            w -= 1;
            // SAFETY: `w` only decreases from a value `< self.nwords`.
            word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
        }
    }

    /// Visit every set start, ASCENDING. `f` returns `false` to stop early.
    ///
    /// `end_hint` is a PERFORMANCE BOUND, not a filter: every base strictly
    /// below it is guaranteed to be visited, bases at or above it MAY be. Pass
    /// `usize::MAX` for a full walk. It exists because the walk cost of a
    /// bitmap and of a hash set have opposite shapes — the set was O(live), the
    /// bitmap is O(arena span), so on a mostly-empty multi-gigabyte heap an
    /// unbounded walk would be a *regression* (~8.2M word loads for a 4.2 GB
    /// arena against a few thousand hash steps). The one caller that walks from
    /// the mutator path, [`ZgcRealHeap::is_heap_addr`], has the arena's
    /// high-water mark available and passes it, which restores the O(bytes
    /// actually allocated) shape.
    ///
    /// `Acquire` on each word load, matching [`Self::contains`]. These walks
    /// are off the allocation path, so the per-word ordering is not worth
    /// trading for a fence.
    pub(crate) fn for_each_base(&self, end_hint: usize, f: &mut dyn FnMut(usize) -> bool) {
        // Bytes of the covered span that can hold a base below `end_hint`,
        // rounded UP to a whole word: over-approximating is what makes the
        // parameter a hint rather than a filter.
        let reach = end_hint.saturating_sub(self.base).min(self.span);
        let limit = reach.div_ceil(8).div_ceil(64).min(self.nwords);
        for w in 0..limit {
            // SAFETY: `w < limit <= self.nwords`.
            let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                if !f(self.base + ((w * 64 + b) << 3)) {
                    return;
                }
            }
        }
        // The spill is always walked in full: its members are precisely the
        // ones whose address the grid could not reason about, so no bound
        // derived from the grid may exclude them.
        if self.has_spill() {
            for &addr in self.overflow.lock().iter() {
                if !f(addr) {
                    return;
                }
            }
        }
    }
}

impl Drop for ZObjectStartBits {
    fn drop(&mut self) {
        if self.nwords == 0 {
            return;
        }
        let layout = std::alloc::Layout::array::<AtomicU64>(self.nwords)
            .expect("zgc object-start bitmap layout overflow");
        // SAFETY: `words` came from `alloc_zeroed` with this exact layout and
        // is freed exactly once.
        unsafe { std::alloc::dealloc(self.words as *mut u8, layout) };
    }
}

/// The membership structure behind [`ZgcRealHeap::registry`], plus its kill
/// switch.
///
/// Both arms answer the same questions with the same semantics; `Hash` is the
/// pre-2026-08-07 structure kept verbatim so `CRATONVM_ZGC_STARTBITS=0` is a
/// true A/B and not an approximation of one.
pub(crate) enum ZObjectStartsKind {
    Bits(ZObjectStartBits),
    Hash(Mutex<FxHashSet<usize>>),
}

/// Base address of every live allocation. See the section header for the
/// measurement, the precedent and the exactness argument.
pub(crate) struct ZObjectStarts {
    kind: ZObjectStartsKind,
}

impl ZObjectStarts {
    /// Cover the arena `[base, base + span)`, honouring
    /// [`zgc_start_bits_enabled_by_default`].
    ///
    /// A zero span cannot be gridded at all, so it falls back unconditionally —
    /// which also keeps `ZgcRealHeap`'s constructor total if the envelope is
    /// ever degenerate.
    pub(crate) fn new(base: usize, span: usize) -> Self {
        Self::with_bitmap(base, span, zgc_start_bits_enabled_by_default())
    }

    /// [`Self::new`] with the kill switch supplied rather than read.
    ///
    /// The seam the arm-equivalence tests drive: they must exercise BOTH arms
    /// in one process, and doing that through `CRATONVM_ZGC_STARTBITS` would
    /// mean a `set_var` race against every other test in the binary.
    pub(crate) fn with_bitmap(base: usize, span: usize, bitmap: bool) -> Self {
        let kind = if bitmap && span > 0 {
            ZObjectStartsKind::Bits(ZObjectStartBits::new(base, span))
        } else {
            ZObjectStartsKind::Hash(Mutex::new(FxHashSet::default()))
        };
        Self { kind }
    }

    /// Is the bitmap arm in force? Diagnostics for the kill-switch tests.
    #[cfg(test)]
    pub(crate) fn is_bitmap(&self) -> bool {
        matches!(self.kind, ZObjectStartsKind::Bits(_))
    }

    /// Record one allocation base. The whole point of this file's change: on
    /// the bitmap arm this is one `fetch_or` and no lock at all.
    #[inline]
    pub(crate) fn insert(&self, addr: usize) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.insert(addr),
            ZObjectStartsKind::Hash(set) => {
                set.lock().insert(addr);
            }
        }
    }

    /// Record a batch. Shaped for [`ZTlabHeapHooks::register_allocations`], and
    /// the reason the `Hash` arm keeps its single `reserve` + single lock: the
    /// A/B has to compare against what was actually there.
    #[inline]
    pub(crate) fn insert_all(&self, addrs: &[usize]) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                for &addr in addrs {
                    bits.insert(addr);
                }
            }
            ZObjectStartsKind::Hash(set) => {
                let mut set = set.lock();
                set.reserve(addrs.len());
                for &addr in addrs {
                    set.insert(addr);
                }
            }
        }
    }

    /// Exact-base membership.
    #[inline]
    pub(crate) fn contains(&self, addr: usize) -> bool {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.contains(addr),
            ZObjectStartsKind::Hash(set) => set.lock().contains(&addr),
        }
    }

    /// Drop one base (the sweep's in-place prune).
    pub(crate) fn remove(&self, addr: usize) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.remove(addr),
            ZObjectStartsKind::Hash(set) => {
                set.lock().remove(&addr);
            }
        }
    }

    /// Does this structure hold anything the arena envelope does not bound?
    ///
    /// `true` on the `Hash` arm unconditionally — a hash set carries no
    /// geometry, so nothing may be inferred from an address range about what it
    /// contains. On the bitmap arm it is `true` only if
    /// [`ZObjectStartBits::overflow`] took something, which means: *every base
    /// this structure holds is inside `[base, base + span)`*, because an address
    /// outside it could not have been encoded and would have spilled. That
    /// makes the envelope screen in [`ZgcRealHeap::is_heap_addr`] a proof rather
    /// than an assumption.
    #[inline]
    pub(crate) fn has_spill(&self) -> bool {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.has_spill(),
            ZObjectStartsKind::Hash(_) => true,
        }
    }

    /// The greatest base at or below `addr` — see
    /// [`ZObjectStartBits::nearest_base_at_or_below`] for why interior-pointer
    /// resolution is one backwards bit scan and not a walk.
    ///
    /// `None` on the `Hash` arm, and `None` whenever the bitmap has a spill:
    /// neither can order what it holds, so neither can answer "greatest at or
    /// below". A `None` means "ask the walk", never "no such base".
    #[inline]
    pub(crate) fn nearest_base_at_or_below(&self, addr: usize) -> Option<usize> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) if !bits.has_spill() => {
                bits.nearest_base_at_or_below(addr)
            }
            _ => None,
        }
    }

    /// Visit every base; `f` returns `false` to stop early.
    ///
    /// `end_hint` is a performance bound and never a filter — see
    /// [`ZObjectStartBits::for_each_base`]. The `Hash` arm ignores it, because
    /// its walk cost is already O(live) and it has no ordering to exploit.
    ///
    /// On the bitmap arm this holds NO lock (the words are read atomically),
    /// which is strictly better than the `Hash` arm, where the guard is held
    /// across the callback exactly as the old code held it.
    pub(crate) fn for_each_base(&self, end_hint: usize, f: &mut dyn FnMut(usize) -> bool) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.for_each_base(end_hint, f),
            ZObjectStartsKind::Hash(set) => {
                for &addr in set.lock().iter() {
                    if !f(addr) {
                        return;
                    }
                }
            }
        }
    }

    /// Every base, as a materialised list.
    pub(crate) fn bases(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        self.for_each_base(usize::MAX, &mut |addr| {
            out.push(addr);
            true
        });
        out
    }

    /// A frozen copy, for the collection cycle.
    ///
    /// This is the direct replacement for `self.registry.lock().clone()`. On
    /// the bitmap arm it is a `Vec<u64>` of one bit per 8 arena bytes — 1/64th
    /// of the heap — which is *cheaper* than the `FxHashSet` clone it replaces
    /// (8 bytes per live object plus load factor) for any occupancy above ~1.5%,
    /// and it keeps the O(1) membership the mark phase's wild-child screen
    /// needs.
    pub(crate) fn snapshot(&self) -> ZObjectStartsSnapshot {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                let mut words: Vec<u64> = Vec::with_capacity(bits.nwords);
                for w in 0..bits.nwords {
                    // SAFETY: `w < bits.nwords`.
                    words.push(unsafe { (*bits.words.add(w)).load(Ordering::Acquire) });
                }
                let extra = if bits.overflow_len.load(Ordering::Acquire) != 0 {
                    bits.overflow.lock().clone()
                } else {
                    FxHashSet::default()
                };
                ZObjectStartsSnapshot {
                    words,
                    base: bits.base,
                    extra,
                }
            }
            ZObjectStartsKind::Hash(set) => ZObjectStartsSnapshot {
                words: Vec::new(),
                base: 0,
                extra: set.lock().clone(),
            },
        }
    }
}

/// A point-in-time copy of [`ZObjectStarts`], with the same two operations the
/// mark phase used to get from its `FxHashSet` clone: O(1) membership and a
/// full enumeration.
pub(crate) struct ZObjectStartsSnapshot {
    /// Copied bitmap words; empty on the `Hash` arm.
    pub(crate) words: Vec<u64>,
    /// The arena base the bits are relative to; meaningless when `words` is
    /// empty.
    pub(crate) base: usize,
    /// The `Hash` arm's whole set, or the bitmap arm's (normally empty)
    /// overflow spill.
    pub(crate) extra: FxHashSet<usize>,
}

impl ZObjectStartsSnapshot {
    /// Exact-base membership, identical in answer to `FxHashSet::contains`.
    ///
    /// No `span` field is needed: the trailing bits of the last word cover
    /// addresses past `base + span`, and `ZObjectStartBits::locate` refuses to
    /// set those, so they are always clear.
    #[inline]
    pub(crate) fn contains(&self, addr: usize) -> bool {
        if !self.words.is_empty() {
            if let Some(off) = addr.checked_sub(self.base) {
                if off & 7 == 0 {
                    let bit = off >> 3;
                    if let Some(word) = self.words.get(bit >> 6) {
                        if word & (1u64 << (bit & 63)) != 0 {
                            return true;
                        }
                    }
                }
            }
        }
        !self.extra.is_empty() && self.extra.contains(&addr)
    }

    /// Every base in the snapshot, ASCENDING, **without materialising a
    /// `Vec`**.
    ///
    /// # Why this exists beside `bases`
    ///
    /// The 2026-08-17 pause anatomy put `snapshot_us` at **13% of the
    /// multi-threaded concurrent pause**, and most of that is `bases()`
    /// allocating one `usize` per registered object: on the 10.8M-object arm
    /// that is a **87 MB allocation inside a stop-the-world pause**, followed by
    /// two or three passes over 87 MB of cold memory.
    ///
    /// A bitmap scan is cheaper than that even when it runs three times: the
    /// bitmap is one bit per 8 arena bytes, so at any occupancy above ~1.5% it
    /// is smaller than the base list it would produce, and scanning it is
    /// sequential.
    ///
    /// `bases()` is kept for the callers that genuinely need a slice — the
    /// corpse census wants to index it, and the relocation path hands it to a
    /// selector — and those are per-cycle, not per-phase.
    #[inline]
    pub(crate) fn for_each_base(&self, mut f: impl FnMut(usize)) {
        for (w, &word) in self.words.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                f(self.base + ((w * 64 + b) << 3));
            }
        }
        for addr in self.extra.iter() {
            f(*addr);
        }
    }

    /// Every base at or above `floor`, ASCENDING, without materialising a
    /// `Vec`.
    ///
    /// # Why a floor makes a young sweep O(young)
    ///
    /// The bitmap is one bit per 8 arena bytes indexed from `self.base`, so a
    /// lower bound on the ADDRESS is a lower bound on the WORD index — the scan
    /// simply starts later. A young cycle can therefore skip the whole
    /// old-generation prefix at no cost, which is the one thing that removes the
    /// O(registry) sweep the 2026-08-17 Phase G measurement identified as the
    /// pause (182 ms of a 309 ms mean, unchanged by the generation split because
    /// the sweep walked every registered object regardless).
    ///
    /// `extra` (the large-object region above the grid) is filtered
    /// individually: it is a small set and it is not address-ordered.
    #[inline]
    pub(crate) fn for_each_base_from(&self, floor: usize, mut f: impl FnMut(usize)) {
        // FLOOR division, both times. Rounding the bit index UP would skip the
        // word containing `floor` whenever the bit index landed on a word
        // boundary from below, and the only thing that makes that unreachable
        // today is that object starts are 8-aligned. Starting one word early
        // costs one word of scan and cannot be wrong; the `addr >= floor` test
        // inside the loop is what makes the bound exact.
        let first_word = (floor.saturating_sub(self.base) / 8) / 64;
        for (w, &word) in self.words.iter().enumerate().skip(first_word) {
            let mut word = word;
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                let addr = self.base + ((w * 64 + b) << 3);
                // The first word can straddle the floor.
                if addr >= floor {
                    f(addr);
                }
            }
        }
        for addr in self.extra.iter() {
            if *addr >= floor {
                f(*addr);
            }
        }
    }

    /// Number of `u64` words in the gridded part of the snapshot.
    ///
    /// The unit the parallel sweep splits on. A word is 64 grid slots = 512
    /// arena bytes, and an object START is a single bit, so a split on a word
    /// boundary gives every shard a disjoint set of BASES with no object
    /// straddling two shards -- which is what lets each shard zero its own dead
    /// objects and clear its own registry bits with no coordination.
    pub(crate) fn word_count(&self) -> usize {
        self.words.len()
    }

    /// Every base in words `[w0, w1)` at or above `floor`, ASCENDING.
    ///
    /// [`Self::for_each_base_from`] restricted to a word range, so N of these
    /// over a partition of `0..word_count()` visit exactly the bases
    /// `for_each_base_from` would, once each and in the same global order.
    ///
    /// `extra` -- the off-grid spill set -- belongs to NO word range and is
    /// deliberately not visited here. The caller sweeps it separately, on one
    /// thread; it is empty on every healthy run (see the "one leg that is NOT
    /// a proof" note on `ZObjectStartBits`), and a set that is not
    /// address-ordered cannot be partitioned by address anyway.
    #[inline]
    pub(crate) fn for_each_base_in_words(&self, w0: usize, w1: usize, floor: usize, mut f: impl FnMut(usize)) {
        let w1 = w1.min(self.words.len());
        for w in w0..w1 {
            let mut word = self.words[w];
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                let addr = self.base + ((w * 64 + b) << 3);
                if addr >= floor {
                    f(addr);
                }
            }
        }
    }

    /// Every off-grid base at or above `floor`. See
    /// [`Self::for_each_base_in_words`] for why this is separate.
    #[inline]
    pub(crate) fn for_each_spilled_base(&self, floor: usize, mut f: impl FnMut(usize)) {
        for addr in self.extra.iter() {
            if *addr >= floor {
                f(*addr);
            }
        }
    }

    /// Every base a YOUNG cycle must visit, ASCENDING.
    ///
    /// The union of two things, and the union is the whole point:
    ///
    /// * everything at or above `floor` -- the contiguous nursery
    ///   `gen_young_floor` describes, which is what a bump-dominated workload
    ///   produces and which the floor is right about; and
    /// * everything on a page below the floor that `visit_below` accepts --
    ///   the free-list-served allocation the floor cannot see, and the
    ///   over-retention its own doc admits to.
    ///
    /// Ascending across the join for free: every below-floor page is below the
    /// floor. That matters because the sweep's run-merging hands adjacent spans
    /// to `Arena::add_free_block`, which can only see two spans as adjacent if
    /// they arrive in order.
    ///
    /// One predicate call per WORD, not per base: a word covers 512 arena bytes
    /// and a logical page is 2 MiB, so a word lies wholly inside one page and
    /// the answer is the same for all 64 of its bits.
    #[inline]
    pub(crate) fn for_each_young_base(
        &self,
        floor: usize,
        visit_below: impl Fn(usize) -> bool,
        mut f: impl FnMut(usize),
    ) {
        let floor_word = (floor.saturating_sub(self.base) / 8) / 64;
        for (w, &word) in self.words.iter().enumerate() {
            if word == 0 {
                continue;
            }
            if w < floor_word && !visit_below(w * 64 * 8) {
                continue;
            }
            let mut word = word;
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                let addr = self.base + ((w * 64 + b) << 3);
                // The word straddling the floor is visited either way, so the
                // exact bound is still this test.
                if addr >= floor || visit_below(addr - self.base) {
                    f(addr);
                }
            }
        }
        for addr in self.extra.iter() {
            f(*addr);
        }
    }

    /// How many bases the snapshot holds, counted rather than collected.
    pub(crate) fn base_count(&self) -> usize {
        self.words
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum::<usize>()
            + self.extra.len()
    }

    /// Every base in the snapshot, bitmap portion ASCENDING.
    ///
    /// Ascending order is free here (it was not, from a hash set) and it is the
    /// order the sweep wants: adjacent dead objects hand adjacent spans to
    /// `Arena::add_free_block`, which is what the post-sweep coalescer merges.
    ///
    /// Prefer [`Self::for_each_base`] on a per-phase path; see its note for the
    /// 87 MB this allocates on a large heap.
    pub(crate) fn bases(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(self.extra.len());
        for (w, &word) in self.words.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                out.push(self.base + ((w * 64 + b) << 3));
            }
        }
        out.extend(self.extra.iter().copied());
        out
    }
}

