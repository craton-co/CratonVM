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
// MOVED, 2026-09-05, to `crate::heap_bitmap::HeapBitmap`. Nothing about it was
// specific to this collector — it references no `zgc` item — and living inside a
// `#[cfg(feature = "zgc")]` module was the only reason the generational and G1
// collectors could not use the crate's one atomic, alignment-exact, removable
// bitmap and were left with three weaker variants instead. The measurement, the
// `gen_heap` precedent it generalises and the exactness argument all travelled
// with it; read them there.
//
// Aliased rather than renamed at the call sites so this collector's ~200 uses,
// its two instances (the object-start registry and the mark bits) and every
// warning that names the type read exactly as they did.
pub(crate) use crate::heap_bitmap::HeapBitmap as ZObjectStartBits;

// The two switches below stay HERE: they are this collector's A/B levers
// over its own use of the bitmap, not properties of the bitmap.
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
/// wants to A/B it. Read once per heap, in `ZgcRealHeap::with_capacity`].
/// Runtime kill switch: `CRATONVM_ZGC_MARKBITS`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) puts this cycle's mark bits
/// back in the object header's [`GC_FLAG_MARKED`], byte for byte, so the A/B is
/// a re-run and not a rebuild. See `ZgcRealHeap::mark_bits` for the three
/// costs the side bitmap removes.
///
/// Read once per heap, in `ZgcRealHeap::with_capacity`], for the same two
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

    /// Drop every base `marks` does not have set: the whole prune, in one
    /// `fetch_and` per 512 arena bytes, for a sweep that already holds the
    /// live set as a bitmap.
    ///
    /// `None` on the hash arm, which has no word structure to `and` against
    /// and whose caller must keep removing one base at a time.
    pub(crate) fn retain_marked(&self, marks: &ZObjectStartBits) -> Option<()> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                bits.retain_marked(marks);
                Some(())
            }
            ZObjectStartsKind::Hash(_) => None,
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
    /// [`Self::snapshot`], copying only the words that can hold a bit.
    ///
    /// `ranges` are arena address ranges the caller knows bound every live
    /// allocation -- on this heap, the low bump region and the large-object
    /// end, with the never-bumped middle between them. See
    /// [`ZObjectStartBits::snapshot_words_within`] for the measurement that
    /// motivates it and the invariant it rests on.
    ///
    /// Falls back to the full copy on the `Hash` arm, which has no geometry to
    /// bound.
    pub(crate) fn snapshot_within(&self, ranges: &[(usize, usize)]) -> ZObjectStartsSnapshot {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                let words = bits.snapshot_words_within(ranges);
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
            ZObjectStartsKind::Hash(_) => self.snapshot(),
        }
    }

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

