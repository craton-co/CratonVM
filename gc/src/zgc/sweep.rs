// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The sweep, as a shardable unit.
//!
//! Split out of `zgc.rs` on 2026-09-02. The sweep is the largest single term
//! in this collector's pause -- 170 ms of a 265 ms mean on the
//! 13.0M-dead-object cycle the 2026-08-17 anatomy measured -- and it is the
//! phase most likely to be worked on, so it is worth being able to read it
//! without a 24,000-line file around it.
//!
//! A CHILD module of `zgc`: `impl super::ZgcRealHeap` here reaches the heap's
//! private fields exactly as the parent does, so nothing needed widening.
//!
//! The per-object body ([`super::ZgcRealHeap::sweep_one`]) is written once and
//! called by both drivers, which is what stops the serial and sharded paths
//! drifting on the only question that matters -- which objects are reclaimed.

use std::sync::atomic::Ordering;

use crate::heap::{GC_FLAG_MARKED, HEADER_SIZE};

use super::{ZObjectStartsSnapshot, ZgcRealHeap, Z_PARMARK_MAX_WORKERS};

// The sweep, as a shardable unit
// ---------------------------------------------------------------------------
//
// The whole-heap sweep is the largest single term in this collector's pause:
// 170 ms of a 265 ms mean on the 13.0M-dead-object cycle the 2026-08-17 anatomy
// measured, against 6.3 ms of 112 ms for a young cycle's bounded one. It is
// also, unlike the mark, embarrassingly parallel -- a linear scan over a bitmap
// whose only shared output is the arena free list.
//
// The two types below are what make that true rather than merely plausible.
// `ZSweepShard` is everything one worker produces, and nothing in it is shared;
// `ZgcRealHeap::sweep_one` is the per-object body, written once and called by
// both the serial and the parallel driver, so the two cannot drift on the
// question that matters -- which objects are reclaimed.
//
// PARALLEL MARKING IS STILL A LOSS, but not for the reason recorded here
// until 2026-09-02, and the difference is worth carrying because it says
// where to look next.
//
// The 2026-08-14 measurement was +31% at one worker and +153% at four --
// RISING with worker count, which is the signature of contention, and the
// note here read it as "coherence traffic on shared queues". Re-measured on
// BinTreesClassic 16 at -Xmx192m, release, after the per-edge shared counters
// and the unconditional claim RMW came out of the drain loop (`mark_roots`,
// `ZObjectStartBits::claim`), mark_us per cycle:
//
//     workers   0 (serial)      1        2        4
//               4107 5277    8720 13842  8169 9820  6691 7040
//
// Serial still wins by 2x, but the cost now FALLS as workers are added
// instead of rising. That inverts the diagnosis: what is left is not
// contention, it is a fixed per-cycle charge that more workers amortise --
// and `ZMarkCoordinator::new` spawns N OS threads on every cycle, with
// `mark_with_controller_stw` spawning a controller on top. A persistent pool
// parked between cycles is the next thing to try; chasing locks is not.
//
// None of it carries over to the SWEEP either way. A mark is a dependent
// pointer chase, memory-LATENCY bound; a sweep is a linear scan with an
// independent, mostly-store body, bandwidth bound. That is an argument rather
// than a measurement, which is why sharding ships default-off behind
// `CRATONVM_ZGC_PARSWEEP` -- and see that function for the further reason it
// is currently unreachable.

/// Everything one sweep worker produces. No shared state, by construction.
///
/// The free spans are the only output with an ordering obligation: the arena's
/// post-sweep coalescer merges spans that are ADJACENT, and it can only see two
/// spans as adjacent if they arrive in ascending order. A shard collects its own
/// spans ascending (the bitmap scan is ascending), and the merge concatenates
/// shards in ascending address order, so the global sequence is ascending too --
/// including across a shard boundary, where the merge additionally JOINS a
/// shard's last run to the next shard's first when they touch.
#[derive(Default)]
pub(crate) struct ZSweepShard {
    /// Survivor bytes and count.
    pub(crate) bytes_copied: usize,
    pub(crate) objects_copied: usize,
    /// Reclaimed bytes and count.
    pub(crate) bytes_freed: usize,
    pub(crate) dead_count: usize,
    /// Registered bases whose header could not be sized this cycle.
    pub(crate) unsizable: usize,
    /// Survivors this shard promoted out of the young generation.
    pub(crate) gen_promoted: usize,
    /// Bases this shard actually visited (the nursery's engagement counter).
    pub(crate) swept: usize,
    /// Bytes NOT memset because only the header was zeroed.
    pub(crate) zero_bytes_skipped: usize,
    /// Dead objects that joined a merged run.
    pub(crate) dead_in_runs: usize,
    /// Dead bases, when `MonitorCleanup::wants_dead_addresses` asked for them.
    pub(crate) dead: Vec<usize>,
    /// Identity hashes of the dead, for the native side tables keyed by them.
    pub(crate) dead_hashes: Vec<i32>,
    /// Free spans, ARENA-RELATIVE and ascending, already run-merged within this
    /// shard. Handed to `Arena::add_free_block` by the merge.
    pub(crate) spans: Vec<(usize, usize)>,
    /// The run currently being accumulated; `None` between runs.
    pub(crate) run: Option<(usize, usize)>,
}

impl ZSweepShard {
    /// Close the run in flight. Every shard must be flushed before its spans
    /// are read: the last run has no successor to flush it, and missing it
    /// leaks the topmost run of garbage in the shard -- invisibly, because the
    /// cursor retraction cannot reclaim a span that is not on the list.
    pub(crate) fn flush(&mut self) {
        if let Some(run) = self.run.take() {
            self.spans.push(run);
        }
    }

    /// Fold `other` in. Address order is the caller's obligation: `other` must
    /// cover strictly higher addresses than `self`.
    pub(crate) fn absorb(&mut self, other: &mut ZSweepShard) {
        other.flush();
        self.bytes_copied += other.bytes_copied;
        self.objects_copied += other.objects_copied;
        self.bytes_freed += other.bytes_freed;
        self.dead_count += other.dead_count;
        self.unsizable += other.unsizable;
        self.gen_promoted += other.gen_promoted;
        self.swept += other.swept;
        self.zero_bytes_skipped += other.zero_bytes_skipped;
        self.dead_in_runs += other.dead_in_runs;
        self.dead.append(&mut other.dead);
        self.dead_hashes.append(&mut other.dead_hashes);
        // CONCATENATED, NOT JOINED, at the seam.
        //
        // A first version merged a shard's last span with the next shard's
        // first when they touched. It was wrong twice over and worth nothing
        // either time:
        //
        //  * `sweep_one` never merges a span that reaches the LARGE-OBJECT end
        //    (see `ZSweepCfg::high_floor`), and a seam join has no way to know
        //    that -- so it could hand `Arena::add_free_block` a span straddling
        //    the line, which routes by offset and would then serve the same
        //    bytes from the low free list and the high cursor both.
        //  * it saved at most `workers - 1` calls per cycle, and
        //    `coalesce_free_list` merges adjacent spans immediately afterwards
        //    regardless.
        //
        // Ascending order -- which is the property the coalescer actually needs
        // -- is preserved by concatenation alone.
        self.spans.append(&mut other.spans);
    }
}

/// One word range's share of the complement sweep, plus the two edges the
/// join needs to stitch it to its neighbours.
///
/// Separate from [`ZSweepShard`] because the complement is a CHAIN and a shard
/// cannot close its own ends: the free span below its first survivor reaches
/// back into the previous shard, which is still running. `ZSweepShard` carries
/// what is genuinely per-shard (counters, dead lists, the large-object end's
/// individual holes) and this wraps it with what is not.
#[derive(Default)]
pub(crate) struct ZComplementShard {
    /// Counters, dead lists, and the HIGH-end per-object holes.
    pub(crate) sh: ZSweepShard,
    /// Free spans strictly BETWEEN two survivors this shard saw itself,
    /// arena-relative and ascending.
    pub(crate) spans: Vec<(usize, usize)>,
    /// The base of the first LOW survivor, or `None` if the shard saw none.
    /// The span below it belongs to the join.
    pub(crate) first_live: Option<usize>,
    /// Absolute end of the last live extent. Meaningful only when
    /// [`Self::first_live`] is set.
    pub(crate) last_end: usize,
    /// The last survivor's header could not be sized, so its extent runs to
    /// the next live base -- possibly in the next shard.
    pub(crate) ends_unsizable: bool,
}

/// The per-cycle constants `sweep_one` reads. Gathered once, outside the loop,
/// so no worker re-reads an atomic or an environment-derived flag per object.
pub(crate) struct ZSweepCfg {
    pub(crate) arena_base: usize,
    /// Where the large-object end begins. A merged run must never grow across
    /// it: `Arena::add_free_block` routes a span by its offset and bounds a LOW
    /// span by the low cursor, so a run spanning the line would be pushed onto
    /// the low tier while covering high-region bytes, and the arena would serve
    /// the same memory from the free list and the high cursor both.
    pub(crate) high_floor: usize,
    /// The low bump cursor as an arena offset: the end of the region
    /// [`ZgcRealHeap::sweep_bitmap`] computes a complement over.
    pub(crate) low_cursor: usize,
    pub(crate) zero_header_only: bool,
    pub(crate) merge_dead_runs: bool,
    /// Are the mark bits cleared in bulk after the sweep? When they are, the
    /// survivor arm does not touch the header at all on a non-generational run.
    pub(crate) bulk_clearable: bool,
    pub(crate) want_dead: bool,
    pub(crate) collect_dead_hashes: bool,
    pub(crate) gen_on: bool,
    pub(crate) promo_age: u8,
}

impl ZgcRealHeap {
    /// Sweep one registered base into `sh`.
    ///
    /// THE WHOLE PER-OBJECT BODY, written once. The serial and parallel
    /// drivers both call it and neither adds anything, so the two cannot drift
    /// on the only question that matters -- which objects are reclaimed. The
    /// arm-equivalence test asserts that as an outcome rather than trusting it.
    ///
    /// Takes `&self` and is called concurrently from
    /// [`Self::sweep_parallel`]'s workers. That is sound because every shard
    /// owns a disjoint set of bases (the split is on bitmap WORD boundaries and
    /// an object start is one bit), so the `header_mut` reborrows below never
    /// alias, the `write_bytes` spans never overlap, and the only shared writes
    /// are `registry.remove` (one `fetch_and`) and, on a generational cycle,
    /// the remembered set -- which is why [`Self::sweep_workers`] refuses to
    /// shard one.
    #[inline]
    pub(crate) fn sweep_one(&self, base: usize, cfg: &ZSweepCfg, sh: &mut ZSweepShard) {
        sh.swept += 1;
        let header = self.header_mut(base as *mut u8);
        // A header this collector cannot size must not be swept. The dead arm
        // below `write_bytes`es `size` bytes and hands the same span to
        // `Arena::add_free_block`, whose only bound is a `debug_assert!` — so
        // passing `object_body_size`'s 1 TiB corrupt-header sentinel through
        // here memsets a terabyte from `base` and free-lists memory that is not
        // in the arena.
        //
        // Retaining it instead leaks one object until its layout resolves again
        // (or forever, if its class really is gone), which is the fail-safe
        // direction: it stays registered, stays rooted conservatively, and is
        // never handed out twice. The one-shot `tracing::warn!` at the call
        // site makes the leak visible rather than silent.
        let Some(size) = Self::alloc_size(header) else {
            sh.unsizable += 1;
            if !cfg.bulk_clearable {
                header.clear_gc_flags(GC_FLAG_MARKED);
            }
            return;
        };
        if self.mark_is_set(base) {
            // Survivor: keep it, and clear the mark bit for the next cycle.
            //
            // ON THE BITMAP ARM THERE IS NOTHING TO CLEAR HERE. One `clear_all`
            // after the walk does the whole heap, which is the difference
            // between a linear store over `capacity / 512` bytes and a
            // read-modify-write of every survivor's 64-byte header line -- i.e.
            // between touching 8 MB and dirtying the entire live set, on a
            // phase that is otherwise read-only on a non-generational run.
            if !cfg.bulk_clearable {
                header.clear_gc_flags(GC_FLAG_MARKED);
            }
            sh.bytes_copied += size;
            sh.objects_copied += 1;
            // PHASE G: one more collection survived. `age_survivor` returns
            // true on the collection that promotes it, and cards it -- see
            // there for why the card at promotion is what makes a young cycle
            // sound. Free on a non-generational run: one relaxed load and a
            // branch.
            if cfg.gen_on && self.age_survivor(base, header, cfg.promo_age) {
                sh.gen_promoted += 1;
            }
            return;
        }
        // Dead: zero it (so a later scan can't see a stale header) and return
        // the span to the arena free list.
        //
        // THE HEADER IS THE WHOLE REASON -- see `zgc_sweep_header_zero`.
        // `HEADER_SIZE` is the entire `ObjectHeader`, so this leaves
        // `class_id=0, num_slots=0`, which is the identical corpse a reader of
        // a vacated span met before; and the body is only reachable through
        // that header, which is why the bytes below it need not be touched.
        // `.min(size)` is belt-and-braces: no allocation is shorter than its
        // header, and a `write_bytes` past the end of one would be exactly the
        // bug `alloc_size` refuses above.
        let zero = if cfg.zero_header_only {
            let n = HEADER_SIZE.min(size);
            sh.zero_bytes_skipped += size - n;
            n
        } else {
            size
        };
        // BEFORE the zeroing: the identity hash lives in the mark word, so
        // `write_bytes` below is what destroys it. A `0` means "never hashed"
        // (or hashed then monitor-inflated, which displaces it) and such an
        // object cannot be a key in an identity-hash-keyed table, so skipping
        // it is exact rather than approximate. See `identity_side_tables`.
        if cfg.collect_dead_hashes {
            let h = cratonvm_types::ObjectHeader::neutral_hash(
                header.mark_word.load(Ordering::Relaxed),
            );
            if h != 0 {
                sh.dead_hashes.push(h);
            }
        }
        // SAFETY: `base` is a registered allocation of `size` bytes inside the
        // arena, and both arms write at most `size` of them.
        unsafe { std::ptr::write_bytes(base as *mut u8, 0, zero) };
        if base >= cfg.arena_base {
            let off = base - cfg.arena_base;
            // A large object never joins a run -- see `ZSweepCfg::high_floor`.
            let mergeable =
                cfg.merge_dead_runs && off < cfg.high_floor && off + size <= cfg.high_floor;
            if mergeable {
                // ONE SPAN PER RUN. The walk is ascending, so a dead object
                // adjacent to the previous one extends it; anything else
                // flushes and starts a new run. See `zgc_sweep_dead_runs` for
                // why the post-coalesce result is the same either way.
                sh.dead_in_runs += 1;
                match sh.run {
                    Some((ro, rl)) if ro + rl == off => sh.run = Some((ro, rl + size)),
                    Some(run) => {
                        sh.spans.push(run);
                        sh.run = Some((off, size));
                    }
                    None => sh.run = Some((off, size)),
                }
            } else {
                // FLUSH FIRST. The coalescer only sees adjacent spans as
                // adjacent because they arrive in ascending order; emitting
                // this one ahead of the run below it would break that ordering
                // for the rest of the cycle.
                sh.flush();
                sh.spans.push((off, size));
            }
        }
        sh.bytes_freed += size;
        sh.dead_count += 1;
        // THE REGISTRY PRUNE, IN PLACE.
        //
        // It used to run after the loop, over a collected `dead` slice -- which
        // is what made that slice unconditional. The base is already in hand
        // here, so the second pass over 104 MB of cold `usize`s bought nothing
        // but the ordering note below, which still holds:
        //
        // the prune is a REMOVAL, never a wholesale replacement of the
        // structure by the mark snapshot's survivors. An allocation registered
        // by another path between the mark snapshot and here would be erased by
        // a replacement -- leaking its memory forever (unsweepable) and, worse,
        // making `is_object_address` deny it so conservative rooting drops it
        // while reachable.
        //
        // No mutator is running (`retire_all_tlabs` ran before the snapshot and
        // this is inside the stop-the-world pause), so nothing can allocate into
        // the span this call is freeing.
        self.registry.remove(base);
        if cfg.want_dead {
            sh.dead.push(base);
        }
    }

    /// The sweep as a **streaming pass over two bitmaps**, computing free
    /// space as the complement of the live set instead of as the union of the
    /// dead objects.
    ///
    /// # What it stops doing
    ///
    /// [`Self::sweep_one`] reads a header for every registered object -- live
    /// or dead -- to size it. On a heap where a few percent survive that is
    /// one cache miss per DEAD object to learn something the two bitmaps
    /// already imply: a start bit with no mark bit is garbage, and the bytes
    /// between one live object's end and the next live object's start are free
    /// whatever used to be in them. So this reads headers for LIVE objects
    /// only.
    ///
    /// It also stops removing dead bases one at a time: `starts &= marks`,
    /// word by word, is the same prune at one `fetch_and` per 512 arena bytes
    /// ([`super::ZObjectStarts::retain_marked`]).
    ///
    /// # Why the complement is exact rather than optimistic
    ///
    /// Every byte below the low cursor is covered by a live object, covered by
    /// a dead one, or already on the free list, and the last two are both
    /// reclaimable. "Not covered by a live object" is therefore precisely the
    /// reclaimable set -- and it is *more* precise than the per-object sweep,
    /// which can only free what it can size and so leaks the bytes of every
    /// object whose header it refuses (`unsizable`).
    ///
    /// That is also why the free list is REPLACED rather than added to: this
    /// recomputes what was already free along with what has just become free,
    /// in address order, already merged
    /// ([`crate::arena::Arena::rebuild_low_free_list`]).
    ///
    /// # The four things it must not free, and how each is kept
    ///
    /// * **The large-object end.** Objects above the high cursor are packed
    ///   downward from the top of the arena and have their own free list; they
    ///   take the per-object path here, as before.
    /// * **A live object whose header cannot be sized.** Its extent is
    ///   unknown, so the pass claims everything up to the next live base and
    ///   frees nothing it cannot account for.
    /// * **An un-retired TLAB tail** published by the stop-the-world protocol
    ///   for a peer frozen in compiled code: it holds no live object, so the
    ///   complement would hand it out from under its owner.
    /// * **The bytes above the last live object.** Those are not free-listed
    ///   at all -- the cursor is lowered onto them instead, which is what
    ///   `retract_cursor_into_free_tail` did with a full sort.
    ///
    /// Returns `(shard, free spans as arena offsets, new low cursor)`, or
    /// `None` when the shape is unavailable -- no mark bitmap (the header
    /// arm) or a hash registry with no words to walk -- and the caller takes
    /// the per-object sweep.
    pub(crate) fn sweep_bitmap(
        &self,
        registered: &ZObjectStartsSnapshot,
        cfg: &ZSweepCfg,
        workers: usize,
    ) -> Option<(ZSweepShard, Vec<(usize, usize)>, usize)> {
        let marks = self.mark_bits.as_ref()?;
        if registered.words.is_empty() {
            return None; // the hash arm has no word structure to stream
        }
        let base = cfg.arena_base;
        let low_end = base.checked_add(cfg.low_cursor)?;
        let high_floor = base.saturating_add(cfg.high_floor);
        let words = registered.words.len().min(marks.word_count());

        // ONE BODY, TWO ARMS -- and the serial arm goes through the same join,
        // so it is not a shortcut past it. `sweep_one` established the rule for
        // the header-walk sweep (see this module's header) and the complement
        // pass needs it more, not less: the seam logic is the part a divergence
        // would hide, and a serial arm that skipped it would test nothing.
        let mut shards: Vec<ZComplementShard> = if workers > 1 {
            let per = words.div_ceil(workers);
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(workers);
                for i in 0..workers {
                    let w0 = i * per;
                    if w0 >= words {
                        break;
                    }
                    let w1 = (w0 + per).min(words);
                    handles.push(scope.spawn(move || {
                        self.sweep_bitmap_range(registered, cfg, marks, w0, w1, base, high_floor)
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| h.join().expect("zgc complement sweep worker panicked"))
                    .collect()
            })
        } else {
            vec![self.sweep_bitmap_range(registered, cfg, marks, 0, words, base, high_floor)]
        };

        let (mut sh, mut spans, mut new_cursor) = Self::join_complement(&mut shards, base, low_end);

        let skips = self.jit_tlab_skip_regions();
        if !skips.is_empty() {
            Self::withhold_skip_regions(&mut spans, &skips, base);
            // ...AND THE CURSOR, not just the free list.
            //
            // Clipping the spans keeps a published tail off the free list, and
            // that is only half of it: this pass also LOWERS the bump cursor
            // onto the bytes above the last survivor, and a tail is above the
            // last survivor by definition -- it holds no live object. Retracting
            // through one hands its owner's next bump straight to the bump
            // path as fresh space. The floor is the same one the slide applies
            // (`jit_tlab_skip_floor`), for the same reason.
            new_cursor = new_cursor
                .max(self.jit_tlab_skip_floor(base, low_end))
                .min(low_end);
        }
        // THE PRUNE, in one `fetch_and` per 512 arena bytes rather than one
        // `remove` per dead object. It has to run against the LIVE registry
        // rather than the snapshot the walk above read, because an object
        // allocated black after that snapshot was taken is registered, marked,
        // and must stay registered -- `starts &= marks` keeps exactly those.
        //
        // Serial, and left that way deliberately: it is one `fetch_and` per 512
        // arena bytes over a span the bounded passes have already narrowed to
        // the bumped ends, which is nothing beside the per-object work above.
        self.registry.retain_marked(marks)?;
        sh.dead_in_runs = sh.dead_count;
        Some((sh, spans, new_cursor - base))
    }

    /// The complement pass over ONE word range. The shared body of the serial
    /// and sharded arms.
    ///
    /// Emits only the free spans strictly BETWEEN two live objects it saw
    /// itself. The gap that reaches back past `w0` belongs to no shard --
    /// resolving it needs the previous shard's last live extent, which is not
    /// known until that shard finishes -- so it is described rather than
    /// emitted, in [`ZComplementShard::first_live`], and
    /// [`ZgcRealHeap::join_complement`] closes it.
    #[allow(clippy::too_many_arguments)]
    fn sweep_bitmap_range(
        &self,
        registered: &ZObjectStartsSnapshot,
        cfg: &ZSweepCfg,
        marks: &super::starts::ZObjectStartBits,
        w0: usize,
        w1: usize,
        base: usize,
        high_floor: usize,
    ) -> ZComplementShard {
        let mut out = ZComplementShard::default();
        let sh = &mut out.sh;
        // Valid only once `first_live` is set; the join supplies everything
        // below it.
        let mut prev_end = 0usize;
        let mut unsizable_live = false;

        for w in w0..w1 {
            let start_w = registered.words[w];
            if start_w == 0 {
                continue; // 512 arena bytes holding no allocation base
            }
            let word_base = registered.base + (w << 9);
            let mark_w = marks.word_at(w);

            // --- the dead: addresses only, no header read -----------------
            let mut dead_w = start_w & !mark_w;
            while dead_w != 0 {
                let bit = dead_w.trailing_zeros() as usize;
                dead_w &= dead_w - 1;
                let addr = word_base + (bit << 3);
                sh.swept += 1;
                if addr >= high_floor {
                    // The large-object end keeps the per-object protocol: its
                    // free list is not the one this pass rebuilds.
                    self.sweep_one_high(addr, cfg, sh);
                    continue;
                }
                sh.dead_count += 1;
                if cfg.collect_dead_hashes {
                    let h = cratonvm_types::ObjectHeader::neutral_hash(
                        self.header_ref(addr as *mut u8)
                            .mark_word
                            .load(Ordering::Relaxed),
                    );
                    if h != 0 {
                        sh.dead_hashes.push(h);
                    }
                }
                if cfg.want_dead {
                    sh.dead.push(addr);
                }
                // A pure write, and only of the header -- the body is left for
                // the next allocation to zero. Same contract as `sweep_one`'s
                // `zero_header_only`, which is default-on; the full-body arm
                // cannot be served here because the size is exactly what this
                // pass declines to read.
                unsafe { std::ptr::write_bytes(addr as *mut u8, 0, HEADER_SIZE) };
            }

            // --- the live: one header read, extent, and the complement ----
            let mut live_w = start_w & mark_w;
            while live_w != 0 {
                let bit = live_w.trailing_zeros() as usize;
                live_w &= live_w - 1;
                let addr = word_base + (bit << 3);
                sh.swept += 1;
                let header = self.header_mut(addr as *mut u8);
                let size = match Self::alloc_size(header) {
                    Some(size) => {
                        sh.bytes_copied += size;
                        sh.objects_copied += 1;
                        Some(size)
                    }
                    None => {
                        sh.unsizable += 1;
                        None
                    }
                };
                if cfg.gen_on && self.age_survivor(addr, header, cfg.promo_age) {
                    sh.gen_promoted += 1;
                }
                if addr >= high_floor {
                    continue; // accounted, but not part of the low complement
                }
                if out.first_live.is_none() {
                    // The gap below this one crosses `w0`. The join owns it.
                    out.first_live = Some(addr);
                } else {
                    if unsizable_live {
                        // The previous survivor ends at or before this base.
                        unsizable_live = false;
                        prev_end = prev_end.max(addr);
                    }
                    if addr > prev_end {
                        out.spans.push((prev_end - base, addr - prev_end));
                    }
                }
                match size {
                    Some(size) => prev_end = prev_end.max(addr.saturating_add(size)),
                    None => {
                        unsizable_live = true;
                        prev_end = prev_end.max(addr);
                    }
                }
            }
        }
        out.last_end = prev_end;
        out.ends_unsizable = unsizable_live;
        out
    }

    /// Stitch the shards' complements into the one ascending, merged sequence a
    /// serial walk would have produced, and return the bump cursor with it.
    ///
    /// # What a shard cannot know
    ///
    /// The complement is a CHAIN: a free span runs from the end of one live
    /// extent to the base of the next, and at `w0` the previous live extent is
    /// in another shard, still running. So a shard emits its interior spans and
    /// describes its two edges -- the base of its first survivor and the end of
    /// its last -- and this pass walks the shards in address order closing each
    /// seam with the span `[chain, first_live)`.
    ///
    /// Two details carry across a seam and neither is optional:
    ///
    ///  * **An unsizable survivor.** A live object whose header could not be
    ///    sized has no known extent, so it runs to the next live base -- which
    ///    may be in the next shard. `ends_unsizable` carries the pending
    ///    resolution over, and `chain` is raised to the next `first_live`
    ///    exactly as the serial walk raises `prev_end`.
    ///  * **A shard with no survivor at all** contributes nothing and must not
    ///    advance `chain`. Its whole range is free, and the next shard's seam
    ///    span covers it, because that span runs from `chain` to wherever the
    ///    next survivor actually is.
    ///
    /// A shard's HIGH-end spans travel in `ZSweepShard::spans` instead and are
    /// concatenated by `absorb`, which is correct for them: they are individual
    /// per-object holes routed by offset, not a chain.
    fn join_complement(
        shards: &mut [ZComplementShard],
        base: usize,
        low_end: usize,
    ) -> (ZSweepShard, Vec<(usize, usize)>, usize) {
        let mut out = ZSweepShard::default();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut chain = base;
        let mut pending_unsizable = false;
        for shard in shards.iter_mut() {
            if let Some(first) = shard.first_live {
                if pending_unsizable {
                    pending_unsizable = false;
                    chain = chain.max(first);
                }
                if first > chain {
                    spans.push((chain - base, first - chain));
                }
                spans.append(&mut shard.spans);
                chain = shard.last_end;
                pending_unsizable = shard.ends_unsizable;
            }
            // ASCENDING, because the coalescer requires it: `shards` is in
            // range order and the high-end spans inside each are too.
            out.absorb(&mut shard.sh);
        }
        if pending_unsizable {
            // Nothing past the last survivor can be justified, so nothing past
            // it is reclaimed this cycle.
            chain = low_end;
        }
        (out, spans, chain.min(low_end))
    }

    // `jit_tlab_skip_regions` USED TO BE STUBBED HERE, returning empty, with a
    // note saying the VM-TLAB branch would have to fill it in when the two
    // met. They met on 2026-09-02 and it did: the real one lives in
    // `zgc/vm_tlab.rs` and returns the reserved tails the stop-the-world
    // protocol published for peers frozen in compiled code. The complement
    // sweep calls that one now, which is what `withhold_skip_regions` below
    // exists to consume -- a tail its owner will resume bumping into holds no
    // live object, so the complement would otherwise hand those bytes out
    // from under it.

    /// Clip published TLAB tails out of the free spans. A frozen peer resumes
    /// bumping into its tail, so the complement must not offer those bytes to
    /// anyone else. The list is empty unless a peer was frozen mid-allocation,
    /// which is what makes an O(spans x skips) pass the right shape.
    fn withhold_skip_regions(
        spans: &mut Vec<(usize, usize)>,
        skips: &[(usize, usize)],
        base: usize,
    ) {
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for &(off, len) in spans.iter() {
            let mut pieces = vec![(off, off + len)];
            for &(s, e) in skips {
                let (s, e) = (s.saturating_sub(base), e.saturating_sub(base));
                let mut next = Vec::with_capacity(pieces.len() + 1);
                for (lo, hi) in pieces {
                    if e <= lo || s >= hi {
                        next.push((lo, hi));
                        continue;
                    }
                    if lo < s {
                        next.push((lo, s));
                    }
                    if e < hi {
                        next.push((e, hi));
                    }
                }
                pieces = next;
            }
            out.extend(pieces.into_iter().map(|(lo, hi)| (lo, hi - lo)));
        }
        *spans = out;
    }

    /// The dead half of [`Self::sweep_one`] for an object in the large-object
    /// end, whose free list this pass does not rebuild.
    fn sweep_one_high(&self, base: usize, cfg: &ZSweepCfg, sh: &mut ZSweepShard) {
        let header = self.header_mut(base as *mut u8);
        let Some(size) = Self::alloc_size(header) else {
            sh.unsizable += 1;
            return;
        };
        sh.dead_count += 1;
        if cfg.collect_dead_hashes {
            let h = cratonvm_types::ObjectHeader::neutral_hash(
                header.mark_word.load(Ordering::Relaxed),
            );
            if h != 0 {
                sh.dead_hashes.push(h);
            }
        }
        let zero = if cfg.zero_header_only {
            HEADER_SIZE.min(size)
        } else {
            size
        };
        unsafe { std::ptr::write_bytes(base as *mut u8, 0, zero) };
        if base >= cfg.arena_base {
            sh.flush();
            sh.spans.push((base - cfg.arena_base, size));
        }
        sh.bytes_freed += size;
        if cfg.want_dead {
            sh.dead.push(base);
        }
        self.registry.remove(base);
    }

    /// The sweep, on this thread.
    pub(crate) fn sweep_serial(
        &self,
        registered: &ZObjectStartsSnapshot,
        floor: usize,
        cfg: &ZSweepCfg,
    ) -> ZSweepShard {
        let mut sh = ZSweepShard::default();
        if floor == 0 {
            registered.for_each_base_in_words(0, registered.word_count(), 0, |base| {
                self.sweep_one(base, cfg, &mut sh)
            });
        } else {
            // A YOUNG CYCLE SWEEPS THE UNION, not the range. See
            // `note_young_page`: the free list serves from below the floor, so
            // an object born into a hole a previous sweep left is invisible to
            // a floor-bounded cycle and is retained until the next major.
            registered.for_each_young_base(
                floor,
                |off| self.young_page_at_offset(off),
                |base| self.sweep_one(base, cfg, &mut sh),
            );
        }
        sh.flush();
        sh
    }

    /// The sweep, split over `workers` threads at bitmap word boundaries.
    ///
    /// # Why the split is sound
    ///
    /// A bitmap word covers 64 grid slots = 512 arena bytes, and an object
    /// START is a single bit. Splitting on a word boundary therefore gives each
    /// worker a disjoint set of BASES -- no object is visited twice and none is
    /// missed, whatever its size. A large object may span many words, but it is
    /// swept by whichever worker owns the word its BASE sits in, and no other
    /// worker touches those bytes because no other base lies inside a live
    /// allocation.
    ///
    /// Each worker writes only into its own [`ZSweepShard`]. The two writes
    /// that leave a shard are `registry.remove` (one `fetch_and` on a word this
    /// worker's range owns) and the per-object `write_bytes` (disjoint spans).
    /// The arena free list is NOT touched: spans are handed over afterwards in
    /// one ordered batch, which is also why the arena guard is not held for the
    /// walk.
    ///
    /// # Why the shards are merged in address order
    ///
    /// `Arena::coalesce_free_list` merges spans that are adjacent, and it can
    /// only see two spans as adjacent if they arrive ascending.
    /// [`ZSweepShard::absorb`] concatenates in range order and joins the seam
    /// where one shard's last run touches the next shard's first, so the merged
    /// sequence is exactly what a serial walk would have produced.
    pub(crate) fn sweep_parallel(
        &self,
        registered: &ZObjectStartsSnapshot,
        floor: usize,
        cfg: &ZSweepCfg,
        workers: usize,
    ) -> ZSweepShard {
        let total_words = registered.word_count();
        let per = total_words.div_ceil(workers);
        let mut shards: Vec<ZSweepShard> = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for i in 0..workers {
                let w0 = i * per;
                if w0 >= total_words {
                    break;
                }
                let w1 = (w0 + per).min(total_words);
                handles.push(scope.spawn(move || {
                    let mut sh = ZSweepShard::default();
                    registered.for_each_base_in_words(w0, w1, floor, |base| {
                        self.sweep_one(base, cfg, &mut sh)
                    });
                    sh.flush();
                    sh
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().expect("zgc sweep worker panicked"))
                .collect()
        });
        // ASCENDING, because `absorb` requires it and the coalescer requires
        // `absorb`. `shards` is already in range order; draining forwards keeps
        // it.
        let mut out = ZSweepShard::default();
        for sh in shards.iter_mut() {
            out.absorb(sh);
        }
        out
    }

    /// How many threads the sweep should use. `1` is serial.
    ///
    /// # It reaches the complement sweep since 2026-09-04
    ///
    /// It did not before. [`Self::sweep_bitmap`] is default-on and wins the arm
    /// selection in `collect_garbage`, and the worker count was consulted only
    /// by the arms that lost -- so `CRATONVM_ZGC_PARSWEEP=<n>` parsed, clamped,
    /// reported, and did nothing. A switch that answers is worse than one that
    /// is absent: it made "I measured the sharded sweep" a sentence someone
    /// could say about a run that never sharded, and it made
    /// `a_sharded_sweep_leaves_the_arena_exactly_as_the_serial_one_does` green
    /// and vacuous, because both of its arms ran the same serial code.
    ///
    /// The complement was not trivially shardable, which is why it stayed that
    /// way: it chains `prev_end` across the whole address space to produce the
    /// free list as one merged, ordered sequence, and a shard boundary falls in
    /// the middle of that chain. Each shard now emits only the spans strictly
    /// between survivors it saw itself and describes its two edges, and
    /// `ZgcRealHeap::join_complement` walks the shards in address order closing
    /// each seam. `the_worker_count_reaches_the_complement_sweep` is what stops
    /// the switch going quietly inert again.
    ///
    /// # Why this is default-off, on a phase that should parallelise
    ///
    /// The sweep is the largest single term in a whole-heap pause -- 170 ms of
    /// 265 ms on the 13.0M-dead-object cycle the 2026-08-17 anatomy measured --
    /// and unlike the mark it is a linear scan with an independent body, so it
    /// is bandwidth bound rather than latency bound and should scale.
    ///
    /// That is an argument, not a measurement, and this tree has shipped a ZGC
    /// marking feature default-ON on an argument before: parallel STW marking,
    /// 2026-08-14, which cost +31% at one worker and +153% at four and was
    /// reverted. `Z_CONC_START_PERCENT_DEFAULT` records the lesson in as many
    /// words. `CRATONVM_ZGC_PARSWEEP=<n>` is the opt-in, and
    /// `gen_sweep_cost_stats`' `swept` counter beside `--verbose:gc`'s
    /// `sweep_us` is how the arm is compared.
    ///
    /// # The one configuration it refuses
    ///
    /// A GENERATIONAL cycle, whatever the setting says. Its survivor arm calls
    /// `age_survivor`, which on the promoting collection calls `card_object`
    /// and writes the remembered set -- shared state whose concurrency this
    /// change has not audited. A young cycle also has nothing to gain: its
    /// sweep is 6.3 ms of a 112 ms pause, because the floor already bounds it.
    /// `CRATONVM_ZGC_BITMAP_SWEEP`: compute free space as the complement of
    /// the live set in one streaming pass over the two bitmaps
    /// ([`Self::sweep_bitmap`]) rather than as the union of the dead objects,
    /// one header read at a time. Default ON; `0`/`off`/`false`/`no` restores
    /// the per-object sweep and the per-object free-list pushes exactly.
    /// # The measurement
    ///
    /// `BinTreesClassic 16` at `-Xmx192m`, release, interleaved on a quiet
    /// host. Both arms performed 5 collections over the same live set and
    /// reported the same `registered=3,125,050 dead=2,940,700`, so they
    /// reclaimed identically and only the cost differs:
    ///
    /// ```text
    ///   sweep_us (mean per cycle)      wall clock
    ///   on    31,253  31,796  30,297  29,883      2839  3106  3157  2888 ms
    ///   off   84,645  61,867  67,357  61,591      4122  3472  3614  3386 ms
    /// ```
    ///
    /// A 2.1-2.7x faster sweep and ~11% off the whole run, 4/4 rounds. Read
    /// `sweep_us` on the `[GC] zgc-pause:` line rather than the wall clock
    /// when re-measuring: at a heap size where the run performs one or two
    /// collections the sweep is a rounding error in the total and the arms
    /// are indistinguishable, which is exactly what an earlier attempt at
    /// `-Xmx512m` showed.
    ///
    /// The regression suite is 87/0 on BOTH arms, release binary, quiet host.
    /// Runs against the DEBUG binary while the box was building something else
    /// showed 5-7 failures per arm whose sets differed in both directions --
    /// three vectors failed only with this feature OFF, which it cannot cause --
    /// and every one of them passed in isolation afterwards. That is the
    /// host, not the sweep, and it is worth knowing before reading a red one.
    ///
    /// Read per COLLECTION rather than latched in a `OnceLock`: this is
    /// consulted once a cycle, so caching it buys nothing and costs the
    /// ability to A/B the two sweeps against one another in one process --
    /// which is the only way to assert they agree.
    pub(crate) fn bitmap_sweep_enabled(&self) -> bool {
        match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_BITMAP_SWEEP") {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    }

    pub(crate) fn sweep_workers(&self, gen_on: bool) -> usize {
        if gen_on {
            return 1;
        }
        self.sweep_worker_count.load(Ordering::Relaxed).max(1)
    }

    /// Set the sweep's worker count for THIS heap. `0` or `1` is serial.
    ///
    /// Per heap rather than a process-wide latch, for the reason
    /// `generational_enabled` and `tlab_enabled` are: the flag readers cache in
    /// a `OnceLock`, so the first test to touch `CRATONVM_ZGC_PARSWEEP` would
    /// decide the arm for every other test in the binary -- and the arm
    /// equivalence this feature rests on can only be asserted by a test that
    /// runs both in one process.
    pub fn set_sweep_workers(&self, n: usize) {
        self.sweep_worker_count
            .store(Self::clamp_sweep_workers(n), Ordering::Relaxed);
    }

    /// Never more workers than cores: this is a stop-the-world phase, so
    /// oversubscription buys nothing and costs context switches inside the
    /// pause it is meant to shorten.
    pub(crate) fn clamp_sweep_workers(n: usize) -> usize {
        if n <= 1 {
            return 1;
        }
        let cores = std::thread::available_parallelism()
            .map(|c| c.get())
            .unwrap_or(1);
        n.min(cores).min(Z_PARMARK_MAX_WORKERS)
    }

    /// `CRATONVM_ZGC_PARSWEEP` -- the seed for [`Self::sweep_worker_count`].
    pub(crate) fn sweep_workers_requested() -> usize {
        match cratonvm_types::flags::runtime_var("CRATONVM_ZGC_PARSWEEP") {
            Ok(v) => Self::clamp_sweep_workers(v.trim().parse::<usize>().unwrap_or(1)),
            Err(_) => 1,
        }
    }
}
