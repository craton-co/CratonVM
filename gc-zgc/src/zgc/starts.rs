// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! `pub` markers below. Nothing outside this file changed visibility.
//!
//! Two instances exist over the identical geometry -- the object-start
//! registry and the mark bits (`ZgcRealHeap::mark_bits`) -- built through the
//! same constructor so they cannot disagree about which addresses are
//! representable. The `label` field is what tells their warnings apart.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashSet;

// ---------------------------------------------------------------------------
// Object-start membership for `ZgcRealHeap` — the allocation-path bitmap
// ---------------------------------------------------------------------------
//
// MOVED, 2026-09-05, to `cratonvm_gc_core::heap_bitmap::HeapBitmap`. Nothing about it was
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
pub use cratonvm_gc_core::heap_bitmap::HeapBitmap as ZObjectStartBits;

// The two switches below stay HERE: they are this collector's A/B levers
// over its own use of the bitmap, not properties of the bitmap.
/// Runtime kill switch: `CRATONVM_ZGC_MARKBITS`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) puts this cycle's mark bits
/// back in the object header's [`GC_FLAG_MARKED`], byte for byte, so the A/B is
/// a re-run and not a rebuild. See `ZgcRealHeap::mark_bits` for the three
/// costs the side bitmap removes.
///
/// Read once per heap, in `ZgcRealHeap::with_capacity`, for the same two
/// reasons as [`zgc_start_bits_enabled_by_default`]: it layers with `-XX:` like
/// every other flag, and a declared `GcFlags` field would latch on first read
/// and make a mid-run `set_var` invisible to the suite that wants to A/B it.
pub fn zgc_mark_bits_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// Runtime kill switch: `CRATONVM_ZGC_STARTBITS`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) puts the registry back on
/// the `Mutex<FxHashSet<usize>>` it used before this change, byte for byte, so
/// the A/B is a re-run and not a rebuild.
///
/// Same idiom, and the same two reasons, as [`zgc_mark_bits_enabled`]: read
/// through [`cratonvm_types::flags::runtime_var_os`] so it layers with `-XX:`
/// like every other flag, and deliberately NOT declared as a
/// [`cratonvm_types::GcFlags`] field, because a declared flag latches on first
/// read and a mid-run `set_var` then becomes invisible to the very suite that
/// wants to A/B it. Read once per heap, in `ZgcRealHeap::with_capacity`.
///
/// **This doc block used to sit above `zgc_mark_bits_enabled`**, concatenated
/// with that function's own — two `///` runs with no item between them attach
/// to the next item, so `STARTBITS` was documented on the `MARKBITS` reader and
/// this function had no doc at all. It also referred to a
/// `zgc_tlab_enabled_by_default` that is not in this module.
pub fn zgc_start_bits_enabled_by_default() -> bool {
    match None::<std::ffi::OsString> {
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
pub enum ZObjectStartsKind {
    Bits(ZObjectStartBits),
    Hash(Mutex<FxHashSet<usize>>),
}

/// Base address of every live allocation. See the section header for the
/// measurement, the precedent and the exactness argument.
pub struct ZObjectStarts {
    kind: ZObjectStartsKind,
    snap_pool: std::sync::Arc<Mutex<Vec<Vec<u64>>>>,
}

impl ZObjectStarts {
    /// Cover the arena `[base, base + span)`, honouring
    /// [`zgc_start_bits_enabled_by_default`].
    ///
    /// A zero span cannot be gridded at all, so it falls back unconditionally —
    /// which also keeps `ZgcRealHeap`'s constructor total if the envelope is
    /// ever degenerate.
    pub fn new(base: usize, span: usize) -> Self {
        Self::with_bitmap(base, span, zgc_start_bits_enabled_by_default())
    }

    /// [`Self::new`] with the kill switch supplied rather than read.
    ///
    /// The seam the arm-equivalence tests drive: they must exercise BOTH arms
    /// in one process, and doing that through `CRATONVM_ZGC_STARTBITS` would
    /// mean a `set_var` race against every other test in the binary.
    pub fn with_bitmap(base: usize, span: usize, bitmap: bool) -> Self {
        let kind = if bitmap && span > 0 {
            // zr z10/mt: on a mapping of its own (`CRATONVM_ZGC_SIDE_TABLE_NO_THP`).
            ZObjectStartsKind::Bits(ZObjectStartBits::labelled_side_table(
                base,
                span,
                "object-start",
                super::zgc_side_table_no_thp_enabled(),
            ))
        } else {
            ZObjectStartsKind::Hash(Mutex::new(FxHashSet::default()))
        };
        Self {
            kind,
            snap_pool: std::sync::Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The bitmap, when the bitmap arm is in force (zr z3/fp: footprint
    /// diagnostics).
    pub fn bitmap(&self) -> Option<&ZObjectStartBits> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => Some(bits),
            ZObjectStartsKind::Hash(_) => None,
        }
    }

    /// Is the bitmap arm in force? Diagnostics for the kill-switch tests.
    #[cfg(test)]
    pub fn is_bitmap(&self) -> bool {
        matches!(self.kind, ZObjectStartsKind::Bits(_))
    }

    /// Record one allocation base. The whole point of this file's change: on
    /// the bitmap arm this is one `fetch_or` and no lock at all.
    #[inline]
    pub fn insert(&self, addr: usize) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.insert(addr),
            ZObjectStartsKind::Hash(set) => {
                set.lock().insert(addr);
            }
        }
    }

    /// [`Self::insert`] for a base inside a VM-TLAB chunk `[lo, hi)` that the
    /// calling thread OWNS. `vm_tlab`'s `OwnedVmTlabChunk` says when that holds;
    /// callers must not pass any other range.
    ///
    /// A bitmap word lying wholly inside `[lo, hi)` gets a plain load, `or` and
    /// `Release` store instead of `fetch_or`. The locked RMW was ~85 % of
    /// `note_tlab_object` in the round 9 wave 4 instruction profile (bintrees,
    /// 375 of 440 samples on the instruction after it): it drains the store
    /// buffer, so every allocation waits for its own just-written header. A
    /// `Release` store on x86-64 is a plain `mov`, which TSO already orders
    /// after those header stores, so the publication [`Self::contains`]'s
    /// `Acquire` pairs with is unchanged.
    ///
    /// Why dropping the atomicity is sound for those words, and only those.
    /// The RMW exists because a word is SHARED with 63 neighbouring grid slots
    /// and a concurrent writer's bit could be lost. A word wholly inside an
    /// owned chunk can only hold bases of objects bumped into that chunk, and
    /// the only mutator that registers them is the one running the chunk's
    /// buffer: this thread. The collector's writers (`remove`,
    /// `retain_marked`, the relocation's re-registration) run with every
    /// mutator stopped at a safepoint, and this is straight-line code with no
    /// poll, so none of them can land between the load and the store. The
    /// words at the chunk's two ends may also hold a neighbour's bases, so
    /// they keep the `fetch_or`.
    #[inline]
    pub fn insert_in_owned_chunk(&self, addr: usize, lo: usize, hi: usize) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => match bits.locate(addr) {
                Some((w, mask)) => {
                    let word_lo = bits.word_base(w);
                    // SAFETY: `locate` bounds-checked `addr`, so `w < bits.nwords`.
                    let word = unsafe { &*bits.words.add(w) };
                    bits.note_word(w);
                    if word_lo >= lo && hi.saturating_sub(word_lo) >= 512 {
                        let cur = word.load(Ordering::Relaxed);
                        word.store(cur | mask, Ordering::Release);
                    } else {
                        word.fetch_or(mask, Ordering::Release);
                    }
                }
                None => bits.spill(addr),
            },
            ZObjectStartsKind::Hash(set) => {
                set.lock().insert(addr);
            }
        }
    }

    /// Note the summary blocks covering `[lo, hi)` on the bitmap arm (no-op on
    /// the `Hash` arm). See `HeapBitmap::note_blocks_covering`: the VM-TLAB
    /// carve calls it so the JIT's inline start-bit store, which writes the
    /// words directly, never lands in a block whose summary bit is clear.
    #[inline]
    pub fn note_blocks_covering(&self, lo: usize, hi: usize) {
        if let ZObjectStartsKind::Bits(bits) = &self.kind {
            bits.note_blocks_covering(lo, hi);
        }
    }

    /// The bitmap arm, for tests that inspect its summary blocks.
    #[cfg(test)]
    pub fn bits_for_test(&self) -> Option<&ZObjectStartBits> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => Some(bits),
            ZObjectStartsKind::Hash(_) => None,
        }
    }

    /// Clear the summary bit of every empty block on the bitmap arm; `0` on
    /// the `Hash` arm. STOP-THE-WORLD ONLY, with no JIT thread frozen
    /// mid-store -- see `HeapBitmap::forget_empty_blocks` and its one caller,
    /// `ZgcRealHeap::forget_empty_registry_blocks`.
    pub fn forget_empty_blocks(&self) -> usize {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.forget_empty_blocks(),
            ZObjectStartsKind::Hash(_) => 0,
        }
    }

    /// `(words, base, span)` of the bitmap arm, for the JIT's inline start-bit
    /// store (`cratonvm_gc_core::tlab::JitZgcAnnounceTable`); `None` on the `Hash` arm
    /// or an empty grid. Bit `(addr - base) / 8` of word array `words` is
    /// `addr`'s start bit, for `addr` in `[base, base + span)` on the 8-byte
    /// grid -- exactly `HeapBitmap::locate`'s answer.
    pub fn jit_bitmap_geometry(&self) -> Option<(usize, usize, usize)> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) if bits.nwords > 0 && bits.span > 0 => {
                Some((bits.words as usize, bits.base, bits.span))
            }
            _ => None,
        }
    }

    /// Record a batch. Shaped for [`ZTlabHeapHooks::register_allocations`], and
    /// the reason the `Hash` arm keeps its single `reserve` + single lock: the
    /// A/B has to compare against what was actually there.
    #[inline]
    pub fn insert_all(&self, addrs: &[usize]) {
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
    pub fn contains(&self, addr: usize) -> bool {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.contains(addr),
            ZObjectStartsKind::Hash(set) => set.lock().contains(&addr),
        }
    }

    /// Drop the bases `mask` names in bitmap word `w` (zr z2/g2, the young
    /// complement sweep). `false` on the `Hash` arm, which has no words: the
    /// caller must not have selected a word-streaming sweep there.
    pub fn clear_word_bits(&self, w: usize, mask: u64) -> bool {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                bits.clear_word_bits(w, mask);
                true
            }
            ZObjectStartsKind::Hash(_) => false,
        }
    }

    /// Drop one base (the sweep's in-place prune).
    pub fn remove(&self, addr: usize) {
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
    /// # TOTAL. It used to answer `Option<()>`, and that was a hazard
    ///
    /// *Changed 2026-09-21 (wave 4, lane Q).* This returned `None` on the hash
    /// arm — "no word structure to `and` against" — and its one caller,
    /// [`ZgcRealHeap::sweep_bitmap`], spelled that `?`. So a `None` here
    /// **abandoned the complement sweep after it had already zeroed every dead
    /// object's header and computed the cycle's free spans**, and the caller
    /// then ran the per-object sweep over headers that were no longer there:
    /// the second pass cannot size what the first one blanked, so it frees
    /// nothing it cannot account for and the two passes disagree about the
    /// free list for the rest of the cycle.
    ///
    /// It could not fire, for a reason that lives **in a different function
    /// and is not stated as a coupling**: `sweep_bitmap` screens the hash arm
    /// out at the top with `registered.words.is_empty()`. That is the kind of
    /// safety this round exists to remove — a mid-mutation bail-out that is
    /// unreachable only because of a screen someone else maintains.
    ///
    /// So the hash arm answers the question instead of declining it: retain
    /// exactly the bases `marks` holds, one probe each. It is O(live) with a
    /// lock, against the bitmap arm's one `fetch_and` per 512 arena bytes —
    /// which is why the bitmap arm exists — and it is still unreachable from
    /// `sweep_bitmap` today. That is the point: it is now unreachable
    /// *and* correct if it is ever reached.
    ///
    /// One deliberate asymmetry, because the two structures are not the same
    /// shape: [`ZObjectStartBits::retain_marked`] ands whole words and leaves
    /// the off-grid **spill** set alone, so a spilled base survives whatever
    /// `marks` says; the hash arm has no spill/grid distinction and prunes an
    /// unmarked base wherever it lives. That direction is the safe one (a
    /// *marked* off-grid base is in `marks`'s own overflow set, and
    /// [`ZObjectStartBits::contains`] consults it), and the sweep re-registers
    /// the bases its high arm could not size regardless.
    /// [`Self::retain_marked`] on the bitmap arm bounded to `ranges` (arena
    /// address ranges that hold every start, as for
    /// [`Self::snapshot_within`]); the `Hash` arm has no geometry and prunes
    /// as [`Self::retain_marked`] does.
    pub fn retain_marked_within(&self, marks: &ZObjectStartBits, ranges: &[(usize, usize)]) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                if super::zgc_retain_plain_stores_enabled() {
                    bits.retain_marked_within_plain(marks, ranges)
                } else {
                    bits.retain_marked_within(marks, ranges)
                }
            }
            ZObjectStartsKind::Hash(_) => self.retain_marked(marks),
        }
    }

    pub fn retain_marked(&self, marks: &ZObjectStartBits) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.retain_marked(marks),
            ZObjectStartsKind::Hash(set) => {
                let mut g = set.lock();
                g.retain(|&addr| marks.contains(addr));
                drop(g);
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
    pub fn has_spill(&self) -> bool {
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
    pub fn nearest_base_at_or_below(&self, addr: usize) -> Option<usize> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) if !bits.has_spill() => {
                bits.nearest_base_at_or_below(addr)
            }
            _ => None,
        }
    }

    /// [`Self::nearest_base_at_or_below`] restricted to bases `>= floor`, with
    /// the two "no" answers kept APART (nio20 wave 6, lane L2):
    ///
    /// * `None` -- this structure cannot order its bases (the `Hash` arm, or
    ///   a bitmap with a spill): ask the walk, exactly as a `None` from
    ///   [`Self::nearest_base_at_or_below`] means;
    /// * `Some(None)` -- the bitmap is authoritative and holds no base in
    ///   `[floor, addr]`;
    /// * `Some(Some(b))` -- the greatest base `<= addr`, which is `>= floor`.
    ///
    /// The split matters to a caller that bounds the scan: "nothing in the
    /// window" is a definitive answer once the window is wide enough
    /// (see `ZgcRealHeap::MAX_ALLOC_EXTENT`), and must not fall into the
    /// O(live) walk the unordered arms need.
    #[inline]
    pub fn nearest_base_within(&self, addr: usize, floor: usize) -> Option<Option<usize>> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) if !bits.has_spill() => {
                Some(bits.nearest_base_in(addr, floor))
            }
            _ => None,
        }
    }

    /// The greatest base in `[floor, addr]`, on EVERY arm (nio20 wave 6, lane
    /// L2) -- the total form of [`Self::nearest_base_within`], for the pin
    /// probe, which must not answer "unknown":
    ///
    /// * bitmap, no spill: the bounded bit scan;
    /// * bitmap with a spill: the greater of the bit scan and the greatest
    ///   in-window SPILLED base (the spill is the only unordered part, and it
    ///   is empty on every healthy run);
    /// * `Hash` (`CRATONVM_ZGC_STARTBITS=0`): the maximum over the set, one
    ///   pass under its lock -- O(live), as every interior query on that arm.
    ///
    /// Before this, `ZgcRealHeap::resolve_interior_for_pin` answered `None`
    /// for every derived pointer on the last two arms (its `?` on
    /// [`Self::nearest_base_at_or_below`]), so a cursor into an array was
    /// never pinned there.
    pub fn nearest_base_within_total(&self, addr: usize, floor: usize) -> Option<usize> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                let grid = bits.nearest_base_in(addr, floor);
                if !bits.has_spill() {
                    return grid;
                }
                let spilled = bits
                    .overflow
                    .lock()
                    .iter()
                    .copied()
                    .filter(|&b| b <= addr && b >= floor)
                    .max();
                // `None < Some(_)`, so this is the greater of the two answers.
                grid.max(spilled)
            }
            ZObjectStartsKind::Hash(set) => set
                .lock()
                .iter()
                .copied()
                .filter(|&b| b <= addr && b >= floor)
                .max(),
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
    pub fn for_each_base(&self, end_hint: usize, f: &mut dyn FnMut(usize) -> bool) {
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

    /// Every base, as a materialised list, ascending.
    ///
    /// zs s2 (sl2 X1): the hash arm iterates in hash order and the bitmap arm
    /// appends its spill set after the bits, so the list is sorted here when it
    /// is not already: every consumer of the slide's `live` list may assume
    /// ascending order. A healthy bitmap run pays one `is_sorted` pass.
    pub fn bases(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        self.for_each_base(usize::MAX, &mut |addr| {
            out.push(addr);
            true
        });
        if !out.is_sorted() {
            out.sort_unstable();
        }
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
    /// [`ZObjectStartBits::snapshot_word_runs`] for the measurement that
    /// motivates it and the invariant it rests on.
    ///
    /// Falls back to the full copy on the `Hash` arm, which has no geometry to
    /// bound.
    pub fn snapshot_within(&self, ranges: &[(usize, usize)]) -> ZObjectStartsSnapshot {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                let (runs, word_ones) = {
                    let mut pool = self.snap_pool.lock();
                    bits.snapshot_word_runs_pooled_with_ones(ranges, &mut pool)
                };
                let extra = if bits.overflow_len.load(Ordering::Acquire) != 0 {
                    bits.overflow.lock().clone()
                } else {
                    FxHashSet::default()
                };
                let merged: Vec<(usize, usize)> = runs
                    .iter()
                    .map(|(w0, words)| (*w0, w0 + words.len()))
                    .collect();
                let cached_base_count = Some(word_ones + extra.len());
                ZObjectStartsSnapshot {
                    words: ZSnapWords::from_runs_pooled(
                        bits.nwords,
                        runs,
                        std::sync::Arc::clone(&self.snap_pool),
                    ),
                    base: bits.base,
                    extra,
                    covered: Some(merged),
                    cached_base_count,
                }
            }
            ZObjectStartsKind::Hash(_) => self.snapshot(),
        }
    }

    /// `self.snapshot_within(ranges).bases()`, list for list, without the
    /// snapshot (zt t26/pc1, `CRATONVM_ZGC_RELOCATE_LIVE_DIRECT`): the bitmap
    /// arm walks the live words in place and allocates the list once at its
    /// exact length ([`ZObjectStartBits::bases_within`]); the `Hash` arm is the
    /// old expression. **STOP-THE-WORLD ONLY**, like the snapshot it replaces.
    pub fn bases_within(&self, ranges: &[(usize, usize)]) -> Vec<usize> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.bases_within(ranges),
            ZObjectStartsKind::Hash(_) => self.snapshot_within(ranges).bases(),
        }
    }

    /// The bases in `[lo, hi)`, ascending: a query of one span, not a bounded
    /// pass (`HeapBitmap::bases_in_span`). **STOP-THE-WORLD ONLY.**
    pub fn bases_in_span(&self, lo: usize, hi: usize) -> Vec<usize> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.bases_in_span(lo, hi),
            ZObjectStartsKind::Hash(_) => {
                let mut v: Vec<usize> = self
                    .snapshot()
                    .bases()
                    .into_iter()
                    .filter(|a| (lo..hi).contains(a))
                    .collect();
                v.sort_unstable();
                v
            }
        }
    }

    /// zgc pc3 (`CRATONVM_ZGC_RELOCATE_LIVE_SUMMARY`): [`Self::bases_within`]
    /// with the bitmap's empty summary blocks skipped
    /// (`HeapBitmap::bases_within_summarised`). Same list.
    pub fn bases_within_summarised(&self, ranges: &[(usize, usize)]) -> Vec<usize> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.bases_within_summarised(ranges),
            ZObjectStartsKind::Hash(_) => self.snapshot_within(ranges).bases(),
        }
    }

    pub fn snapshot(&self) -> ZObjectStartsSnapshot {
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
                    words: words.into(),
                    base: bits.base,
                    extra,
                    covered: None,
                    cached_base_count: None,
                }
            }
            ZObjectStartsKind::Hash(set) => ZObjectStartsSnapshot {
                words: ZSnapWords::default(),
                base: 0,
                extra: set.lock().clone(),
                covered: None,
                cached_base_count: None,
            },
        }
    }
}

/// The words of a [`ZObjectStartsSnapshot`]: a bitmap `len` words long of
/// which only some runs are stored, every other word reading as zero.
///
/// # Why not a `Vec<u64>` of the full length (2026-10-07)
///
/// The bitmap is one bit per 8 arena bytes, so it is CAPACITY / 64 bytes: 267
/// MB at the default `-Xmx` of a 64 GiB Windows machine. The bounded snapshot
/// copied only the bumped ends but still returned a full-length zeroed `Vec`,
/// on the reasoning that a zeroed allocation is a free zero-page mapping. Under
/// the launcher's mimalloc on Windows it was not: about 30 ms, twice per
/// collection (mark start and the relocation's live set), on a heap with 1 MB
/// live. A thread calling `System.gc()` back to back therefore held every other
/// thread in a ~120 ms-per-call pause loop where HotSpot's pause is ~2 ms, and
/// a first `MemorySegment.get` under `--real-jdk` -- a few hundred milliseconds
/// of LambdaForm spinning -- took minutes
/// (`docs/feature-designs/zgc-architecture.md`).
///
/// Indexing keeps the absolute word index every consumer already used, so the
/// passes over [`ZObjectStartsSnapshot::segments`] read exactly what they read
/// before. A run lookup is a scan of at most a handful of runs (two on the
/// two-ended arena).
#[derive(Default)]
pub struct ZSnapWords {
    /// The logical length: the bitmap's word count.
    len: usize,
    /// `(first word, words)`, ascending and disjoint.
    runs: Vec<(usize, Vec<u64>)>,
    /// Optional pool to recycle `Vec<u64>` allocations on drop.
    pool: Option<std::sync::Arc<Mutex<Vec<Vec<u64>>>>>,
}

impl Drop for ZSnapWords {
    fn drop(&mut self) {
        let Some(pool) = self.pool.take() else {
            return;
        };
        let mut p = pool.lock();
        for (_, mut words) in self.runs.drain(..) {
            if words.capacity() > 0 && p.len() < 4 {
                words.clear();
                p.push(words);
            }
        }
    }
}

/// What an uncovered word reads as.
static ZSNAP_ZERO: u64 = 0;

impl ZSnapWords {
    /// `runs` must be ascending, disjoint and inside `[0, len)`.
    pub fn from_runs(len: usize, runs: Vec<(usize, Vec<u64>)>) -> Self {
        debug_assert!(runs.windows(2).all(|p| p[0].0 + p[0].1.len() <= p[1].0));
        debug_assert!(runs.last().is_none_or(|(w0, v)| w0 + v.len() <= len));
        Self {
            len,
            runs,
            pool: None,
        }
    }

    pub fn from_runs_pooled(
        len: usize,
        runs: Vec<(usize, Vec<u64>)>,
        pool: std::sync::Arc<Mutex<Vec<Vec<u64>>>>,
    ) -> Self {
        debug_assert!(runs.windows(2).all(|p| p[0].0 + p[0].1.len() <= p[1].0));
        debug_assert!(runs.last().is_none_or(|(w0, v)| w0 + v.len() <= len));
        Self {
            len,
            runs,
            pool: Some(pool),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Words actually stored (the runs), as opposed to [`Self::len`], the
    /// logical length (zr z3/fp: the snapshot's real allocation).
    pub fn stored_words(&self) -> usize {
        self.runs.iter().map(|(_, words)| words.len()).sum()
    }

    #[inline]
    pub fn get(&self, w: usize) -> Option<&u64> {
        if w >= self.len {
            return None;
        }
        for (w0, words) in &self.runs {
            if w < *w0 {
                break;
            }
            if let Some(word) = words.get(w - w0) {
                return Some(word);
            }
        }
        Some(&ZSNAP_ZERO)
    }

    /// The stored run holding all of `[w0, w1)`; the empty slice for an empty
    /// range. Panics on a range no single run holds -- every caller slices a
    /// [`ZObjectStartsSnapshot::segments`] entry, which one run always holds.
    fn slice(&self, w0: usize, w1: usize) -> &[u64] {
        if w0 >= w1 {
            return &[];
        }
        for (r0, words) in &self.runs {
            if w0 >= *r0 && w1 <= r0 + words.len() {
                return &words[w0 - r0..w1 - r0];
            }
        }
        panic!("ZSnapWords: words {w0}..{w1} are not one stored run")
    }
}

impl From<Vec<u64>> for ZSnapWords {
    fn from(words: Vec<u64>) -> Self {
        let len = words.len();
        let runs = if len == 0 {
            Vec::new()
        } else {
            vec![(0, words)]
        };
        Self {
            len,
            runs,
            pool: None,
        }
    }
}

impl std::ops::Index<usize> for ZSnapWords {
    type Output = u64;
    #[inline]
    fn index(&self, w: usize) -> &u64 {
        match self.get(w) {
            Some(word) => word,
            None => panic!("ZSnapWords: word {w} out of range (len {})", self.len),
        }
    }
}

impl std::ops::Index<std::ops::Range<usize>> for ZSnapWords {
    type Output = [u64];
    fn index(&self, r: std::ops::Range<usize>) -> &[u64] {
        self.slice(r.start, r.end)
    }
}

/// A point-in-time copy of [`ZObjectStarts`], with the same two operations the
/// mark phase used to get from its `FxHashSet` clone: O(1) membership and a
/// full enumeration.
pub struct ZObjectStartsSnapshot {
    /// Copied bitmap words, indexed by the bitmap's own word index; empty on
    /// the `Hash` arm. Only the covered runs are stored (see [`ZSnapWords`]).
    pub words: ZSnapWords,
    /// The arena base the bits are relative to; meaningless when `words` is
    /// empty.
    pub base: usize,
    /// The `Hash` arm's whole set, or the bitmap arm's (normally empty)
    /// overflow spill.
    pub extra: FxHashSet<usize>,
    /// The word ranges [`ZObjectStarts::snapshot_within`] copied, ascending
    /// and disjoint; every word outside them is zero by that function's
    /// invariant (verified in debug builds by
    /// `ZObjectStartBits::debug_assert_clear_outside`). `None` = every word.
    ///
    /// The per-cycle passes over the snapshot iterate only these
    /// ([`Self::segments`]). Before they did, `base_count` and the complement
    /// sweep's two passes walked every word of the full-capacity `Vec` -- 8.4
    /// million words at `-Xmx4g` -- whatever the bounds had saved the copy:
    /// 21% and 59% of a ~1 MiB-live, 400 ms ZGC cycle
    /// (`docs/feature-designs/zgc-architecture.md`).
    pub covered: Option<Vec<(usize, usize)>>,
    pub cached_base_count: Option<usize>,
}

impl ZObjectStartsSnapshot {
    /// Bytes this snapshot holds: the stored bitmap words plus the side set
    /// (zr z3/fp; an estimate for the set, 16 bytes a slot).
    pub fn resident_bytes(&self) -> usize {
        self.words.stored_words() * std::mem::size_of::<u64>() + self.extra.capacity() * 16
    }

    /// The parts of words `[w0, w1)` that can hold a bit, ascending: the
    /// intersection with [`Self::covered`] (all of it when `None`), clamped to
    /// the snapshot.
    pub fn segments(&self, w0: usize, w1: usize) -> Vec<(usize, usize)> {
        let w1 = w1.min(self.words.len());
        match &self.covered {
            None if w0 < w1 => vec![(w0, w1)],
            None => Vec::new(),
            Some(ranges) => ranges
                .iter()
                .filter_map(|&(a, b)| {
                    let (a, b) = (a.max(w0), b.min(w1));
                    (a < b).then_some((a, b))
                })
                .collect(),
        }
    }

    /// How many words [`Self::segments`] yields over `[0, word_count)`.
    pub fn covered_word_count(&self) -> usize {
        self.segments(0, self.words.len())
            .iter()
            .map(|&(a, b)| b - a)
            .sum()
    }

    /// Split the covered words into at most `parts` contiguous word ranges of
    /// about equal COVERED size, ascending, each `[w0, w1)` in snapshot word
    /// indices (a range may span the uncovered gap between two segments).
    /// A split on the raw `0..word_count()` would hand nearly every worker
    /// the never-bumped middle and one worker all the work.
    pub fn split_covered(&self, parts: usize) -> Vec<(usize, usize)> {
        let segs = self.segments(0, self.words.len());
        let total: usize = segs.iter().map(|&(a, b)| b - a).sum();
        if total == 0 || parts == 0 {
            return Vec::new();
        }
        let per = total.div_ceil(parts.max(1));
        let mut out = Vec::with_capacity(parts);
        let mut start: Option<usize> = None;
        let mut taken = 0usize;
        for &(a, b) in &segs {
            let mut w = a;
            while w < b {
                let s0 = *start.get_or_insert(w);
                let take = (per - taken).min(b - w);
                w += take;
                taken += take;
                if taken == per {
                    out.push((s0, w));
                    start = None;
                    taken = 0;
                }
            }
        }
        if let Some(s0) = start {
            out.push((s0, segs.last().map_or(s0, |&(_, b)| b)));
        }
        out
    }

    /// Does the snapshot hold any base in the address range `[lo, hi)`?
    ///
    /// zr z1/al: the "no object starts inside this extent" screen the
    /// large-object sweep applies before it trusts an oversize array's header
    /// (`ZgcRealHeap::oversize_array_extent`). Streams the covered words of
    /// the range (one load per 512 arena bytes, uncovered words skipped) and
    /// then the spill set; the snapshot is immutable for the cycle, so a
    /// parallel sweep shard removing bases from the live registry cannot hide
    /// one from it.
    pub fn any_start_in(&self, lo: usize, hi: usize) -> bool {
        if hi <= lo {
            return false;
        }
        if !self.words.is_empty() {
            // Grid slot `b` names `base + 8b`; the slots in `[lo, hi)` are
            // `ceil((lo - base) / 8) .. ceil((hi - base) / 8)`.
            let b_lo = lo.saturating_sub(self.base).div_ceil(8);
            let b_hi = hi.saturating_sub(self.base).div_ceil(8);
            if b_hi > b_lo {
                let (w_lo, w_hi) = (b_lo >> 6, (b_hi - 1) >> 6);
                for (a, b) in self.segments(w_lo, w_hi + 1) {
                    for w in a..b {
                        let mut word = self.words[w];
                        if word == 0 {
                            continue;
                        }
                        if w == w_lo {
                            word &= u64::MAX << (b_lo & 63);
                        }
                        if w == w_hi {
                            let top = (b_hi - 1) & 63;
                            word &= u64::MAX >> (63 - top);
                        }
                        if word != 0 {
                            return true;
                        }
                    }
                }
            }
        }
        !self.extra.is_empty() && self.extra.iter().any(|&a| a >= lo && a < hi)
    }

    /// Exact-base membership, identical in answer to `FxHashSet::contains`.
    ///
    /// No `span` field is needed: the trailing bits of the last word cover
    /// addresses past `base + span`, and `ZObjectStartBits::locate` refuses to
    /// set those, so they are always clear.
    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
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
    pub fn for_each_base(&self, mut f: impl FnMut(usize)) {
        for (w0, w1) in self.segments(0, self.words.len()) {
            for w in w0..w1 {
                let mut word = self.words[w];
                while word != 0 {
                    let b = word.trailing_zeros() as usize;
                    word &= word - 1;
                    f(self.base + ((w * 64 + b) << 3));
                }
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
    pub fn for_each_base_from(&self, floor: usize, mut f: impl FnMut(usize)) {
        // FLOOR division, both times. Rounding the bit index UP would skip the
        // word containing `floor` whenever the bit index landed on a word
        // boundary from below, and the only thing that makes that unreachable
        // today is that object starts are 8-aligned. Starting one word early
        // costs one word of scan and cannot be wrong; the `addr >= floor` test
        // inside the loop is what makes the bound exact.
        let first_word = (floor.saturating_sub(self.base) / 8) / 64;
        for (w0, w1) in self.segments(first_word, self.words.len()) {
            for w in w0..w1 {
                let mut word = self.words[w];
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
    pub fn word_count(&self) -> usize {
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
    pub fn for_each_base_in_words(
        &self,
        w0: usize,
        w1: usize,
        floor: usize,
        mut f: impl FnMut(usize),
    ) {
        for (w0, w1) in self.segments(w0, w1) {
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
    }

    /// Every off-grid base at or above `floor`. See
    /// [`Self::for_each_base_in_words`] for why this is separate.
    #[inline]
    pub fn for_each_spilled_base(&self, floor: usize, mut f: impl FnMut(usize)) {
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
    ///
    /// # The spill set belongs to the caller, not to this walk
    ///
    /// *Corrected 2026-09-20.* This used to end with an unconditional
    /// `for addr in self.extra.iter() { f(*addr) }`, which made it the only
    /// walker on this type that visits the off-grid spill set — and the spill
    /// set is ALSO swept separately, on one thread, by
    /// [`Self::for_each_spilled_base`] (`ZgcRealHeap::collect_garbage` calls it
    /// straight after whichever sweep arm ran, precisely because a set with no
    /// address order can neither be partitioned nor merged into a run).
    ///
    /// On a young cycle — the only cycle that reaches this function — every
    /// spilled base was therefore handed to `sweep_one` **twice**. The second
    /// visit is not a double count, it is a double free: the first one zeroed
    /// the header, pushed `(off, size)` onto the shard's span list and
    /// `registry.remove`d the base; the second one re-sizes the corpse it just
    /// created and pushes a SECOND span covering the same bytes, after which
    /// `Arena::add_free_block` holds one address twice and the allocator hands
    /// it out twice. `extra` is empty on every healthy run, which is the only
    /// reason this has not been seen; "unreachable today" is not a property a
    /// double free gets to keep.
    ///
    /// So the rule here is the one [`Self::for_each_base_in_words`] already
    /// states: a base outside the grid belongs to no word range and is not this
    /// walk's to visit. Both young arms of the sweep now agree with the
    /// whole-heap arms about who owns the spill set.
    #[inline]
    pub fn for_each_young_base(
        &self,
        floor: usize,
        visit_below: impl Fn(usize) -> bool,
        mut f: impl FnMut(usize),
    ) {
        let floor_word = (floor.saturating_sub(self.base) / 8) / 64;
        for (w0, w1) in self.segments(0, self.words.len()) {
            for w in w0..w1 {
                let word = self.words[w];
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
                    // The word straddling the floor is visited either way, so
                    // the exact bound is still this test.
                    if addr >= floor || visit_below(addr - self.base) {
                        f(addr);
                    }
                }
            }
        }
        // NO `extra` LOOP HERE. See the doc comment: the spill set is swept
        // once, separately, by the caller.
    }

    /// How many bases the snapshot holds, counted rather than collected.
    pub fn base_count(&self) -> usize {
        if let Some(count) = self.cached_base_count {
            return count;
        }
        self.segments(0, self.words.len())
            .into_iter()
            .map(|(w0, w1)| {
                self.words[w0..w1]
                    .iter()
                    .map(|w| w.count_ones() as usize)
                    .sum::<usize>()
            })
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
    pub fn bases(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(self.extra.len());
        for (w0, w1) in self.segments(0, self.words.len()) {
            for w in w0..w1 {
                let mut word = self.words[w];
                while word != 0 {
                    let b = word.trailing_zeros() as usize;
                    word &= word - 1;
                    out.push(self.base + ((w * 64 + b) << 3));
                }
            }
        }
        out.extend(self.extra.iter().copied());
        // zs s2 (sl2 X1): the spill set lands after the bits; keep the list
        // ascending, as the doc above promises. Empty spill on healthy runs.
        if !self.extra.is_empty() {
            out.sort_unstable();
        }
        out
    }
}

#[cfg(test)]
mod covered_snapshot_tests;

#[cfg(test)]
mod young_walk_tests;

#[cfg(test)]
mod owned_chunk_tests;
