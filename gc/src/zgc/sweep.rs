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
// PARALLEL MARKING WAS MEASURED AS A LOSS in this tree (+31% at one worker,
// +153% at four; reverted 2026-08-14) and the reason does not carry over. A
// mark is a dependent pointer chase: it is memory-LATENCY bound, and more
// workers buy nothing while costing coherence traffic on shared queues. A sweep
// is a linear scan with an independent, mostly-store body: it is bandwidth
// bound. That is an argument, not a measurement, which is why this ships
// default-off behind `CRATONVM_ZGC_PARSWEEP` -- see that function.

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

