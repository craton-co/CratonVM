#[allow(unused_imports)]
use super::*;

impl ZgcRealHeap {
    /// After a slide: every reference slot in every survivor must point at a
    /// **registered live base**, or the rewrite pass missed it.
    ///
    /// # Why this exists rather than a test
    ///
    /// A missed slot is a dangling pointer, and a dangling pointer surfaces as
    /// a SIGSEGV somewhere else entirely, later, in whichever subsystem
    /// happens to dereference it first. That is the 2026-08-14 netty/hibernate
    /// cluster: 22 crashes across two unrelated applications, ZGC-only,
    /// spanning WebSocket handshaking, HTTP/2, LZMA, HQL queries and
    /// multitenancy — i.e. no subsystem in common except the collector. No
    /// stack trace from such a crash names the collector, so the crash site is
    /// worthless as evidence and the only useful place to look is here, at the
    /// moment the invariant breaks.
    ///
    /// Off unless `CRATONVM_DBG_ZGC_VERIFY_SLIDE` is set: it is O(live slots)
    /// with a registry probe each, which is far too expensive for every cycle
    /// but trivial next to a debugging session.
    ///
    /// Reports and continues rather than panicking. A panic here would abort
    /// inside a stop-the-world with the heap half-described, and the whole
    /// point is to get the *list* — the first offender is rarely the only one,
    /// and the shape of the set is what names the missing rewrite.
    pub fn verify_no_dangling_slots_after_slide(
        &self,
        live_now: &[usize],
        arena_lo: usize,
        arena_hi: usize,
        fwd: &relocate::ZForwardIndex,
    ) {
        if !zgc_verify_slide_enabled() {
            return;
        }
        // ENGAGEMENT FIRST, and before any early return below it. A release
        // build compiles the clean-case `debug!` away, so these counters are
        // the only thing that can tell "every slot resolved" from "the
        // verifier never ran" — see the field docs on
        // `ZgcCounters::slide_verifications`.
        self.counters
            .slide_verifications
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .slide_verified_survivors
            .fetch_add(live_now.len(), Ordering::Relaxed);
        use census::ZCensusHeapView;
        // Extents of every survivor, sorted, so a dangling target can be asked
        // the question that actually matters: does it land INSIDE a live
        // object? A word that points at nothing is inert; a word that points
        // into a live object of the wrong class is type confusion, and
        // compaction is what converts the first into the second by moving a
        // survivor onto an address some stale word already names.
        let mut extents: Vec<(usize, usize)> = live_now
            .iter()
            .filter_map(|b| {
                // zs s2/rv: `live_extent`, so a slot into a real over-long
                // array is not a false "dangling" (dg residual row 6).
                self.live_extent(*b, self.header_ref(*b as *mut u8))
                    .map(|sz| (*b, *b + sz))
            })
            .collect();
        extents.sort_unstable();
        let lands_inside_a_survivor = |addr: usize| -> bool {
            match extents.binary_search_by(|(lo, _)| lo.cmp(&addr)) {
                Ok(_) => true,
                Err(i) => i > 0 && addr < extents[i - 1].1,
            }
        };
        let mut dangling = 0usize;
        let mut missed = 0usize;
        let mut unregistered = 0usize;
        let mut aliasing = 0usize;
        let mut reported = 0usize;
        for obj in live_now {
            self.reference_slots(*obj as u64, &mut |slot| {
                let raw = slot.raw_word as usize;
                if raw == 0 || raw < arena_lo || raw >= arena_hi {
                    // Off-arena words are not this collector's to judge: a
                    // reference slot can legitimately hold an address from
                    // another space, and `is_in_heap` refuses them everywhere
                    // else too.
                    return;
                }
                if self.registry.contains(raw) {
                    return;
                }
                // CLASSIFY, because "not a live base" has two very different
                // causes and only one of them is a rewrite bug:
                //
                //  * `missed_rewrite` — the target is an address this very
                //    slide moved an object AWAY from. The rewrite pass should
                //    have updated this slot and did not. A real dangling
                //    pointer, and the collector's fault.
                //  * otherwise — the word was never a live base at all. Most
                //    often a PRIMITIVE sitting in a slot the class declares as
                //    a reference (the W7-84 family, which this VM already warns
                //    about separately) that happens to land inside the arena's
                //    address range. Not the slide's doing, and not fixable here.
                let missed_rewrite = fwd.contains_from(raw);
                let aliases_survivor = !missed_rewrite && lands_inside_a_survivor(raw);
                if missed_rewrite {
                    missed += 1;
                } else {
                    unregistered += 1;
                    if aliases_survivor {
                        aliasing += 1;
                    }
                }
                dangling += 1;
                if reported < 16 {
                    reported += 1;
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        holder = *obj,
                        holder_class = self.header_ref(*obj as *mut u8).class_id.as_u32(),
                        slot_addr = slot.slot_addr,
                        points_to = raw,
                        missed_rewrite,
                        aliases_survivor,
                        "zgc slide verify: reference slot does not resolve to a live base"
                    );
                }
            });
        }
        self.counters
            .slide_verify_missed_rewrites
            .fetch_add(missed, Ordering::Relaxed);
        self.counters
            .slide_verify_unregistered
            .fetch_add(unregistered, Ordering::Relaxed);
        if dangling > 0 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                dangling,
                missed_rewrites = missed,
                unregistered_targets = unregistered,
                aliasing_a_survivor = aliasing,
                survivors = live_now.len(),
                "zgc slide verify: {missed} slot(s) still point at an address this slide MOVED AWAY FROM (the rewrite pass missed them); {unregistered} point at something that was never a live base, of which {aliasing} now land INSIDE a live object — those are the dangerous ones: inert before the slide, an alias to the wrong object after it",
            );
        } else {
            tracing::debug!(
                target: "zgc",
                survivors = live_now.len(),
                "zgc slide verify: every reference slot resolves to a live base"
            );
        }
    }

    /// Pin `addr` against relocation — see `ZgcCounters::critical_pins` (no
    /// production caller since the JNI checkouts stopped pinning in gc-common
    /// w2-c). Balanced by [`Self::unpin_critical`].
    pub fn pin_critical(&self, addr: usize) {
        if addr == 0 {
            return;
        }
        *self.counters.critical_pins.lock().entry(addr).or_insert(0) += 1;
    }

    /// Release one pin taken by [`Self::pin_critical`].
    pub fn unpin_critical(&self, addr: usize) {
        let mut pins = self.counters.critical_pins.lock();
        if let Some(count) = pins.get_mut(&addr) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&addr);
            }
        }
    }

    /// Snapshot of the pinned addresses, for the relocation-set filter.
    ///
    /// # THIS IS THE IMMOVABILITY HALF ONLY -- and that is correct
    ///
    /// This is the only reader of [`ZgcCounters::critical_pins`], and its one
    /// caller is inside `relocate_stw`. So a `pin_critical` keeps an object
    /// where it is and does not, by itself, keep it ALIVE. A caller that needs
    /// the object kept alive must also root it; for the JNI checkouts that
    /// root is the per-VM JNI global ref they hold on the array
    /// (`CriticalCopy::array_gref` / `ArrayElemBuffer::array_gref`, rooted by
    /// `vm/src/memory/roots.rs` step 9), and those checkouts no longer pin
    /// here at all (gc-common w2-c). The process-global keep-alive table
    /// (the `pinned` module), which used to be the liveness half, was deleted
    /// in gc-common w4-b.
    ///
    /// Do not "fix" the apparent gap by rooting these addresses here as well.
    /// It would put the keep-alive in two places that can disagree.
    pub fn critical_pin_addrs(&self) -> Vec<usize> {
        let pins = self.counters.critical_pins.lock();
        if pins.is_empty() {
            return Vec::new();
        }
        pins.keys().copied().collect()
    }

    /// Number of live critical pins — diagnostics and tests.
    pub fn critical_pin_count(&self) -> usize {
        self.counters.critical_pins.lock().len()
    }

    /// Free share of the arena in permille — test support for the
    /// fragmentation-gauge fixtures, which have to assert the state they claim
    /// to have built rather than assume it.
    #[cfg(test)]
    pub fn arena_free_permille_for_test(&self) -> usize {
        let arena = self.arena.lock();
        let capacity = arena.capacity();
        if capacity == 0 {
            return 0;
        }
        arena.remaining().saturating_mul(1000) / capacity
    }

    /// Enable GC event logging (`--verbose:gc`).
    ///
    /// Closes the gap recorded on `VmHeap::enable_gc_logging`'s ZGC arm: that
    /// arm had no toggle to flip and no gated statement to flip it for, so a
    /// user running `--verbose:gc -XX:+UseZGC` was told logging was on and then
    /// got nothing for the entire run. Same name/shape as
    /// `G1Collector::enable_gc_logging` (`g1.rs:6770-6777`) so the `VmHeap` arm
    /// is a plain delegation.
    pub fn enable_gc_logging(&self) {
        self.gc_log_enabled.store(true, Ordering::Relaxed);
    }

    /// Disable GC event logging. Mirrors `g1.rs:6775-6777`.
    pub fn disable_gc_logging(&self) {
        self.gc_log_enabled.store(false, Ordering::Relaxed);
    }

    /// Next identity hash code (never zero; wraps avoiding 0).
    pub fn next_hash(&self) -> i32 {
        let h = self.next_hash_code.fetch_add(1, Ordering::Relaxed);
        if h == 0 {
            self.next_hash_code.fetch_add(1, Ordering::Relaxed)
        } else {
            h
        }
    }

    /// Cap on how much the one-shot fragmentation report walks and prints:
    /// walls visited in the winning window, and class rows emitted. A report
    /// that scrolls the log away at the moment the log is least readable is
    /// the failure mode the one-shot guard exists to avoid.
    pub const ZGC_FRAG_REPORT_WALLS: usize = 32;

    /// Report an arena allocation failure, with the occupancy that says
    /// whether the heap was full or merely fragmented.
    ///
    /// Emitted on a doubling schedule (failures 1, 2, 4, 8, ...) rather than
    /// exactly once. One-shot was the original rule and half its reason still
    /// holds -- once the arena is out the failure repeats for every subsequent
    /// request, and a per-failure line buries the run in stderr at the moment
    /// it is least readable. The other half was wrong. A single allocation does
    /// not fail once: its callers run a LADDER -- `gc_alloc_array` and
    /// `jit_newarray` both try, retire the TLAB and force a GC, try again, run
    /// `last_ditch_reclaim`, and try a third time before throwing. One-shot
    /// therefore reported the FIRST rung, whose free list is by construction the
    /// PRE-collection one, and then fell silent for exactly the rungs that
    /// decide whether the `OutOfMemoryError` is honest.
    ///
    /// Failure #2 is the post-GC retry, and that is the number separating "the
    /// heap really has no hole this big" from "the collection freed most of the
    /// heap and the retry still could not see it". Found while diagnosing H2
    /// `TestBenchmark`: the one-shot line reported `used` at 99% of capacity,
    /// while the collection that ran immediately afterwards left the heap 97%
    /// free -- a state this report had no way to show.
    pub fn warn_alloc_failed_once(
        &self,
        size: usize,
        used: usize,
        capacity: usize,
        free_list_bytes: usize,
        largest_free_block: usize,
        // (spans, distinct sizes) -- the SHAPE of the fragmentation. Without it
        // "1.13 GiB free, biggest hole 65528" leaves open whether that is two
        // dozen holes or twenty thousand, and those want different fixes.
        span_shape: (usize, usize),
    ) {
        // 2026-08-27: this message used to open "this heap does not compact,
        // so the bump cursor never rewinds". The first clause stopped being
        // true — `TestMVStoreTool` on that day reported 11 compaction cycles and
        // 1 817 474 objects relocated in the same run that printed this — and it
        // is exactly the sentence that sends a reader looking for a missing
        // compactor instead of at the `zgc frag:` lines below, which had already
        // localised the failure to **2 888 live bytes in 49 runs** standing
        // between 263 216 free bytes and one 266 104-byte contiguous window.
        // The cursor half is still true and is kept; the compaction half is not.
        // zr z3/lt: per heap (was a function-local process `static`).
        let failure_seq = self
            .counters
            .local
            .alloc_failure_warnings
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if !failure_seq.is_power_of_two() {
            return;
        }
        tracing::warn!(
            target: "cratonvm::gc::guard",
            request = size,
            used,
            capacity,
            free_list_bytes,
            largest_free_block,
            free_spans = span_shape.0,
            free_span_sizes = span_shape.1,
            failure_seq,
            "zgc: arena allocation failed — `largest_free_block < request` with \
             a large `free_list_bytes` means fragmentation, not exhaustion: the \
             bump cursor never rewinds, so reclaimed space returns as free-list \
             holes and a big request needs a big CONTIGUOUS one. \
             `failure_seq` is the rung of the caller's try/GC/try/reclaim/try \
             ladder: 1 is pre-collection, 2 is the post-GC retry. \
             Read the `zgc frag:` lines that follow — they name the live \
             objects walling off the cheapest window, which is the question \
             this line cannot answer.",
        );
    }

    /// Gather what is standing between the holes, once, on the allocation
    /// failure that is about to become an `OutOfMemoryError`.
    ///
    /// The one-line guard above says the heap is fragmented rather than full.
    /// That is where every previous investigation of this failure stopped, and
    /// it is one question short: *fragmented by what?* A free list of 3892
    /// half-megabyte spans holding 93% of the heap can mean the spans are
    /// walled by a live/dead mosaic (nothing short of relocation serves the
    /// request) or that a handful of small survivors is holding gigabytes
    /// hostage (a targeted fix does). [`Arena::frag_profile`] separates those,
    /// and this walks the winning window's walls so the occupants can be named.
    ///
    /// Returns `None` on every call after the second: the failure repeats for
    /// every subsequent request once the arena is out, and this report is far
    /// too long to emit per failure.
    ///
    /// TWICE, not once, for the reason spelled out on
    /// [`Self::warn_alloc_failed_once`]: the callers run a try/GC/try ladder, so
    /// report #1 always profiles the PRE-collection arena and report #2 is the
    /// only view of the post-GC one. Naming the walls that survive a collection
    /// is the whole point of this report -- the ones that do not survive it were
    /// never the problem.
    ///
    /// Bounded on purpose: at most [`Self::ZGC_FRAG_REPORT_WALLS`] walls are
    /// walked and that many class rows returned.
    pub fn frag_report_once(&self, request: usize, arena: &Arena) -> Option<ZFragReport> {
        // zr z3/lt: per heap (was a function-local process `static`).
        if self
            .counters
            .local
            .frag_reports
            .fetch_add(1, Ordering::Relaxed)
            >= 2
        {
            return None;
        }
        let profile = arena.frag_profile(request);
        let (high_blocks, high_bytes, high_max) = arena.high_free_shape();
        let mut report = ZFragReport {
            profile,
            high_cursor: arena.high_cursor(),
            high_blocks,
            high_bytes,
            high_max,
            high_reserve_unclaimed: arena.unclaimed_high_reserve(),
            occupants: Vec::new(),
            unregistered_bytes: 0,
        };
        let Some(window) = report.profile.cheapest else {
            return Some(report);
        };
        // Walk the walls inside that window and tally their occupants. Each
        // wall is a run of live objects laid end to end; a byte that is not a
        // registered base is counted rather than skipped, because unregistered
        // bytes below the cursor are themselves a finding — an un-retired TLAB
        // tail is exactly that, and it is invisible to both the allocator and
        // the sweep.
        let base = arena.base_ptr() as usize;
        let blocks = arena.free_blocks_sorted();
        let mut by_class: FxHashMap<u32, (usize, usize)> = FxHashMap::default();
        let mut walls_walked = 0usize;
        for w in blocks.windows(2) {
            let wall_start = w[0].0 + w[0].1;
            let wall_end = w[1].0;
            if wall_end <= wall_start || wall_start < window.start || wall_end > window.end {
                continue;
            }
            walls_walked += 1;
            if walls_walked > Self::ZGC_FRAG_REPORT_WALLS {
                break;
            }
            let mut off = wall_start;
            while off < wall_end {
                let addr = base + off;
                if self.registry.contains(addr) {
                    // SAFETY: the registry holds only live allocation bases.
                    let header = unsafe { &*(addr as *const ObjectHeader) };
                    match Self::alloc_size(header) {
                        Some(size) => {
                            let e = by_class.entry(header.class_id.as_u32()).or_insert((0, 0));
                            e.0 += 1;
                            e.1 += size;
                            off += size.max(8);
                        }
                        None => {
                            // Unsizable header: the sweep refuses these too, so
                            // the rest of this run cannot be strided.
                            report.unregistered_bytes += wall_end - off;
                            break;
                        }
                    }
                } else {
                    report.unregistered_bytes += 8;
                    off += 8;
                }
            }
        }
        let mut rows: Vec<(u32, usize, usize)> =
            by_class.into_iter().map(|(c, (n, b))| (c, n, b)).collect();
        rows.sort_unstable_by_key(|&(c, n, b)| (std::cmp::Reverse(b), std::cmp::Reverse(n), c));
        rows.truncate(Self::ZGC_FRAG_REPORT_WALLS);
        report.occupants = rows;
        Some(report)
    }

    /// zs s5/ta (`CRATONVM_DBG_ZGC_POST_REFUSAL`): an `alloc_raw` refusal
    /// just happened; log it and re-arm the hand-out budget.
    #[cold]
    #[inline(never)]
    pub fn dbg_post_refusal_arm(&self, size: usize) {
        self.counters
            .post_refusal_budget
            .store(self.counters.post_refusal_limit, Ordering::Relaxed);
        eprintln!(
            "[zgc-post-refusal] refused size={size} gc={}",
            self.gc_count.load(Ordering::Relaxed)
        );
    }

    /// zs s5/ta (`CRATONVM_DBG_ZGC_POST_REFUSAL`): one hand-out after a
    /// refusal, while the budget lasts. `first` is the span's first word
    /// before `alloc_raw` zeroes it: `0` for never-written, slide-zeroed or
    /// returned-tail bytes (a returned VM-TLAB tail reads zero); a TLAB filler
    /// class for a tail a filler covered; otherwise what the sweep left there
    /// (`CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` keeps a dead object's header,
    /// whose low half is its class id). Called under the arena lock, after
    /// `hand_out` committed the span.
    #[cold]
    #[inline(never)]
    pub fn dbg_post_refusal_hand_out(&self, ptr: *mut u8, size: usize, large: bool) {
        let left = self.counters.post_refusal_budget.load(Ordering::Relaxed);
        if left == 0 {
            return;
        }
        self.counters
            .post_refusal_budget
            .store(left - 1, Ordering::Relaxed);
        // SAFETY: `ptr` is an 8-aligned span of at least `size` bytes (every
        // object is at least a header) that the arena just handed to this
        // caller and committed; nothing else writes it before `init`.
        let first = unsafe { std::ptr::read_volatile(ptr as *const u64) };
        let cid = first as u32;
        let what = if first == 0 {
            "zero"
        } else if cid == cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID.as_u32()
            || cid == cratonvm_gc_core::tlab::GAP_FILLER_CLASS_ID.as_u32()
        {
            "tlab-filler"
        } else {
            "corpse"
        };
        let since = self
            .gc_count
            .load(Ordering::Relaxed)
            .saturating_sub(self.counters.last_refusal_epoch.load(Ordering::Relaxed));
        eprintln!(
            "[zgc-post-refusal] served size={size} large={large} at={:#x} was={what} \
             first_word={first:#018x} gcs_since_refusal={since}",
            (ptr as usize).saturating_sub(self.arena_base)
        );
    }

    /// Bump-allocate `size` zeroed bytes (8-byte aligned) and register the
    /// base address. Returns `None` on OOM.
    pub fn alloc_raw(&self, size: usize, init: &dyn Fn(*mut u8)) -> Option<*mut u8> {
        // Which end of the arena. See `Arena::high_cursor` for the measurement
        // this exists for; the short version is that one long-lived object
        // inside a thread's private TLAB chunk caps every hole in the heap at
        // one chunk, so an object too big for any TLAB must not be allocated
        // among TLAB chunks.
        let large = size >= ZGC_LARGE_OBJECT_MIN;
        // zr z1/al (`CRATONVM_ZGC_RAW_ZERO_OUTSIDE_LOCK`, default ON): the
        // zeroing of the span moves below the guard's release -- see
        // `zgc_raw_zero_outside_lock_enabled`. `deferred` carries the zero
        // plan out of the block; `None` means the block already zeroed.
        let zero_outside = self.counters.raw_zero_outside_lock.load(Ordering::Relaxed);
        let (ptr, deferred) = {
            let mut arena = self.arena.lock();
            // The high end first for a large object, the low end as the
            // fallback: the two ends share one middle, so a high-end refusal
            // does not mean the arena is out — and serving a large object from
            // the small-object end is merely bad for later fragmentation,
            // while failing is an `OutOfMemoryError`.
            let mut got = if large {
                arena.alloc_high(size, 8).or_else(|| arena.alloc(size, 8))
            } else {
                arena.alloc(size, 8)
            };
            if got.is_none() {
                // Before this can be called a failure: every live TLAB is
                // holding a chunk whose unused tail is arena space no allocator
                // can see. On a non-compacting heap that tail is also the piece
                // most likely to be ADJACENT to a free hole (a chunk is carved
                // from one), so handing it back can both supply the bytes and
                // let `Arena::alloc`'s merge re-form a bigger span.
                //
                // Lock order is cell -> arena, never the reverse, so the arena
                // guard has to go first. Dropping it is safe here: the first
                // attempt already failed, and a peer that allocates in the
                // window can only make the retry fail again — which is exactly
                // the pre-existing answer.
                drop(arena);
                self.retire_all_tlabs();
                arena = self.arena.lock();
                got = if large {
                    arena.alloc_high(size, 8).or_else(|| arena.alloc(size, 8))
                } else {
                    arena.alloc(size, 8)
                };
            }
            // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`): the last cycle's
            // dead objects are on no list until its parked sweep installs them.
            // Finish it (or wait for its runner) and retry once: the allocation
            // stall, instead of a failure that would collect a heap whose
            // reclaim is already computed. Same lock order as the retire above.
            if got.is_none() && self.counters.csweep.pending() {
                drop(arena);
                let _ = self.finish_pending_sweep_for_allocation();
                arena = self.arena.lock();
                got = if large {
                    arena.alloc_high(size, 8).or_else(|| arena.alloc(size, 8))
                } else {
                    arena.alloc(size, 8)
                };
            }
            let ptr = match got {
                Some(p) => p,
                None => {
                    // Allocation failure on a NON-COMPACTING heap is not the
                    // same event as "full of live data", and the two want
                    // different fixes. Say which, once, with the numbers that
                    // separate them:
                    //
                    //   used ~ capacity, free list small  -> genuinely full
                    //   used ~ capacity, free list LARGE  -> fragmented: the
                    //                                        bytes are there,
                    //                                        no hole this big
                    //   largest_free_block < size         -> why THIS request
                    //                                        failed
                    //
                    // Without it an `OutOfMemoryError` raised from here is
                    // undiagnosable after the fact, which is what it was when
                    // `ZipContentTests` began failing at the Spring Boot
                    // suite's default `-Xmx 2g` on the day ZGC became the
                    // default collector.
                    let used = arena.used();
                    let capacity = arena.capacity();
                    let free_list_bytes = arena.free_list_bytes();
                    let largest = arena.largest_free_block();
                    let shape = arena.free_span_shape();
                    // ...and, once, WHAT is standing between the holes. The
                    // guard line says "fragmented, not exhausted"; this says
                    // by what, which is the question every previous
                    // investigation of this failure stopped one step short of.
                    // Gathered under the lock, LOGGED without it — see
                    // `ZFragReport`.
                    let frag = self.frag_report_once(size, &arena);
                    drop(arena);
                    self.warn_alloc_failed_once(
                        size,
                        used,
                        capacity,
                        free_list_bytes,
                        largest,
                        shape,
                    );
                    if let Some(frag) = frag {
                        frag.log(size);
                    }
                    // Ask for a collection at the next safepoint. A native
                    // cannot collect where it stands, but `vm_exec`'s
                    // native-boundary hook acts on this latch — and a request
                    // that just failed is stronger evidence that a cycle is due
                    // than the `allocated >= gc_threshold` predicate, which
                    // counts LIVE bytes and therefore cannot see the bump space
                    // this heap never rewinds.
                    self.native_alloc_pressure.store(true, Ordering::Relaxed);
                    // ...and the HARD latch, which the boundary honours without
                    // re-asking `needs_gc()`. The line above has been here since
                    // the latch existed and was, on its own, inert in exactly
                    // the case it was written for: see the `hard_alloc_failure`
                    // field doc.
                    self.hard_alloc_failure.store(true, Ordering::Relaxed);
                    self.counters
                        .trigger_hard_alloc_fail
                        .fetch_add(1, Ordering::Relaxed);
                    // zr z5/cr: what was refused, and before which collection.
                    self.counters
                        .last_refusal_size
                        .store(size, Ordering::Relaxed);
                    self.counters
                        .last_refusal_epoch
                        .store(self.gc_count.load(Ordering::Relaxed), Ordering::Relaxed);
                    // zs s5/ta (`CRATONVM_DBG_ZGC_POST_REFUSAL`): log what the
                    // next hand-outs are made of. Off: one plain load.
                    if self.counters.post_refusal_limit != 0 {
                        self.dbg_post_refusal_arm(size);
                    }
                    return None;
                }
            };
            // zs s5/ta (`CRATONVM_DBG_ZGC_POST_REFUSAL`): name what a hand-out
            // after a refusal is made of, BEFORE the zeroing below erases it.
            // `hand_out` committed the span, so the read is in mapped memory.
            if self.counters.post_refusal_limit != 0 {
                self.dbg_post_refusal_hand_out(ptr, size, large);
            }
            // Arm the ALLOCATABLE-SPACE trigger (see `headroom_low`). Two
            // reasons this is here rather than in `needs_gc`: the arena lock is
            // already held, and `needs_gc` is polled far more often than
            // allocation happens.
            //
            // Cheap by construction. The un-bumped tail is two field reads, and
            // while it is above the margin — which is the whole of a normal run
            // — nothing else is consulted. Only once the tail is genuinely low
            // does the free-list probe run, and `has_free_block_at_least` is
            // O(1) for exactly the "no" answer that matters here.
            //
            // WIDENING THE MARGIN BY THE UNCLAIMED LARGE-OBJECT RESERVE WAS
            // TRIED HERE, MEASURED, AND REMOVED. The reasoning was sound: a
            // TLAB chunk is reserved space, not allocated space, so `allocated`
            // cannot see the arena filling with chunks, and on
            // `TestNonBlockingAPI` (2026-08-13) this was the ONLY trigger that
            // ever fired — at a bare `margin`, when the un-bumped middle was
            // already down to 16 MB. Collecting earlier does keep the reserve
            // intact.
            //
            // It is still not here, because the arm was priced against what it
            // changed. Once `ZGC_TLAB_RESERVATION_SHARE` bounds what chunks may
            // claim at all, the class passes with the bare margin (`OK (44
            // tests)`, zero OOM warnings) — and the widened margin cost **4.6%
            // on `TestTomcat`**, a class with nowhere near enough threads for
            // the chunk to shrink, i.e. a class that paid for the trigger and
            // got nothing back. Collecting earlier is the wrong lever for a
            // reservation that should not have been that large.
            let margin = zgc_headroom_margin(arena.capacity());
            let tail = arena.capacity().saturating_sub(arena.used());
            if tail < margin && !arena.has_free_block_at_least(margin) {
                self.headroom_low.store(true, Ordering::Relaxed);
            }
            // The arena bump path hands out memory from a zeroed Vec, but a
            // reused free-list block may contain stale bytes — zero it so a
            // fresh header/fields start clean. Except what the arena proves
            // already reads zero: never-written bytes, and the spans the last
            // slide zero-filled (r16 zgcarray ZA1, `Arena`'s `SlideZeroWindow`)
            // -- a mid-size array allocated over a slide's vacated span was
            // zeroed twice.
            let plan = arena.take_last_hand_out_zero_plan(ptr, size);
            if zero_outside {
                // The plan is taken here, under the guard, because the next
                // hand-out overwrites what it is computed from; the `memset`
                // it describes is done after the guard drops.
                (ptr, Some(plan))
            } else {
                // SAFETY: `arena.alloc` guarantees `size` valid bytes at `ptr`,
                // and `plan` is the arena's answer for exactly this hand-out.
                unsafe { cratonvm_gc_core::arena::ZeroPlan::zero(plan.as_ref(), ptr, size) };
                self.note_raw_zero_plan(plan.as_ref());
                (ptr, None)
            }
        };
        if let Some(plan) = deferred {
            // OUTSIDE THE ARENA GUARD. Nothing else can reach these bytes: the
            // span is off the free list (or below the bump cursor), committed
            // by `hand_out`, and not yet in the registry, so no allocator,
            // conservative probe, sweep (no pause can start while this thread
            // is in Rust code without a poll) or uncommit (which touches only
            // free and un-bumped ranges) reads or writes them before `init`.
            //
            // SAFETY: as in the in-guard arm -- `size` valid bytes at `ptr`,
            // exclusively this caller's, and `plan` is this hand-out's answer.
            unsafe { cratonvm_gc_core::arena::ZeroPlan::zero(plan.as_ref(), ptr, size) };
            self.note_raw_zero_plan(plan.as_ref());
        }
        // THE HEADER, BEFORE THE REGISTRY. `registry.insert` is this heap's
        // publication point: it is what `is_object_address` answers from and
        // what `collect_garbage`'s snapshot enumerates, so between the insert
        // and the caller's header write the address is an object nothing has
        // described. The span above is zeroed, so a reader in that window
        // decodes `class_id=0, shape=0, kind=Object` -- a well-formed EMPTY
        // object -- and the sweep would size it at `HEADER_SIZE`, find
        // `GC_FLAG_MARKED` clear (`allocate_black_if_marking` has not run
        // either) and free 16 bytes of an object that is neither dead nor 16
        // bytes long.
        //
        // See `G1Region::bump_alloc_initialized` for the same ordering in the
        // G1 backend and `GenerationalHeap::try_alloc_young_initialized`, whose
        // SAFETY comment has stated the rule since long before either: "`init`
        // writes the valid header before the arena lock is released".
        init(ptr);
        // One `fetch_or` into the object-start bitmap — no lock, no hash, no
        // table that grows with the live set. See the "Object-start membership"
        // section header for the measurement this replaced.
        self.audit_registry_insert(ptr as usize, size, "alloc_raw");
        self.registry.insert(ptr as usize);
        // zr z2/rv: an extent the interior probes' bounded scan cannot cover
        // from its far end is recorded, so a derived pointer deep inside it
        // still resolves (`huge_extent_containing`). After the insert: the
        // table's lookups validate a slot against the registry.
        if size > Self::MAX_ALLOC_EXTENT {
            self.note_huge_extent(ptr as usize);
        }
        // A FRESH ALLOCATION MAKES ITS PAGE YOUNG. The free list serves from
        // below `gen_young_floor`, so without this an object born into a hole
        // a previous sweep left is unreachable by every young cycle -- see
        // `note_young_page`.
        self.note_young_page(ptr as usize);
        // See the same call in `register_allocations`.
        cratonvm_gc_core::gc_quiescence::note_allocated(&[ptr as usize]);
        let after = self.allocated.fetch_add(size, Ordering::Relaxed) + size;
        // Arm the native-allocation-pressure latch on the crossing edge. This
        // is the ZGC analogue of G1's `note_region_consumed_locked`
        // (`g1.rs:1765-1774`): "`needs_gc` went true during allocation".
        //
        // `alloc_raw` is the single chokepoint for EVERY object and array on
        // this backend — `try_alloc_object`, `try_alloc_array` and the
        // infallible `GarbageCollector::alloc_object` / `alloc_array` all route
        // here — so one test covers both shapes.
        //
        // The predicate is `needs_gc`'s LIVE-BYTES clause, inlined rather than
        // called (to keep this off the trait and out of any import ambiguity;
        // see `<Self as GarbageCollector>::needs_gc`). Pairing it with
        // `gc_rearm` — rather than testing `gc_threshold` alone — is what keeps
        // the latch out of the GC-storm shape the `gc_rearm` field exists to
        // prevent: a live set parked above the static threshold leaves
        // `gc_rearm` above `allocated` after every sweep, so the latch stays
        // DOWN until genuinely new allocation clears the re-arm floor. The latch
        // can therefore never request a collection that `needs_gc` would refuse.
        //
        // IT IS A SUBSET, and deliberately so. This comment said "`needs_gc`'s
        // predicate verbatim" and was true when it was written; `needs_gc` has
        // since grown three more clauses (`headroom_low`, the F1 allocation
        // budget, the G2d nursery) and this mirrors none of them. That is the
        // conservative direction — the latch's only job is to make the NEXT
        // native boundary collect without re-asking, and every unmirrored
        // clause is still consulted by `VmHeap::young_spill_pressure`'s own
        // `|| h.needs_gc()` a moment later. Mirroring `headroom_low` here
        // specifically would be redundant twice over: it is raised eleven lines
        // above this one, in this same function, under this same arena guard.
        if after >= self.gc_threshold && after >= self.gc_rearm.load(Ordering::Relaxed) {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
        Some(ptr)
    }

    /// Count what an `alloc_raw` zero plan let the `memset` skip.
    #[inline]
    pub fn note_raw_zero_plan(&self, plan: Option<&cratonvm_gc_core::arena::ZeroPlan>) {
        if let Some(plan) = plan {
            if plan.slide_bytes != 0 {
                self.counters
                    .tlab_slide_zero_bytes_skipped
                    .fetch_add(plan.slide_bytes, Ordering::Relaxed);
            }
        }
    }

    /// Try to allocate an object. Returns `None` on true heap exhaustion.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        // zs s6/no: the opt-in exhausted verdict (one relaxed load when off).
        if self.exhausted_verdict_refuses() {
            // zt t2/al: record the request, sized as below.
            self.note_verdict_refusal(
                cratonvm_types::compact_object_total_size(
                    self.layout_domain(),
                    class_id.as_u32(),
                    num_fields,
                )
                .or_else(|| HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)),
            );
            return None;
        }
        let compact_body = cratonvm_types::compact_object_total_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let total = match compact_body {
            Some(total) => total,
            None => HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)?,
        };
        let num_slots = u32::try_from(num_fields).ok()?;
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                num_slots,
            );
            if let Some(body) = compact_body {
                header.set_compact_shape(num_slots, body);
            }
            // SAFETY: `ptr` is the base of a zeroed, exclusively-owned span.
            unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
        };
        let ptr = self.alloc_raw_tlab(total, init)?;
        // Allocate BLACK while a concurrent cycle is marking. Must follow
        // the header write above; see `allocate_black_if_marking`.
        self.allocate_black_if_marking(ptr);
        self.audit_reserved_vs_header(ptr, total, "object", class_id);
        // SAFETY: `ptr` is a fresh `total`-byte allocation carrying a header.
        unsafe { Some(ObjectRef::from_raw(ptr)) }
    }

    /// The bytes an allocation reserved and the bytes its header describes must
    /// be the same number.
    ///
    /// Everything downstream sizes an object from its header — the sweep zeroes
    /// and free-lists `alloc_size(header)` bytes, the slide memmoves that many,
    /// `is_object_address` decides containment with it — while the allocator
    /// advanced the cursor by what it RESERVED. If those two disagree by even
    /// one slot, the next allocation's base lands inside this object's declared
    /// extent, and the object-start registry then holds two entries that
    /// overlap. That is the shape of every overlap the extent census has
    /// reported on the `ResourceLeakDetectorTest` repro: a registered base
    /// interior to another registered object.
    ///
    /// Checking it here rather than inferring it from the wreckage a cycle
    /// later is the difference between naming the class that does it and
    /// reading tea leaves in a hex dump.
    ///
    /// Diagnostic only, behind `CRATONVM_DBG_ZGC_CORPSE`, capped at 16 lines.
    #[inline]
    pub fn audit_reserved_vs_header(
        &self,
        ptr: *mut u8,
        reserved: usize,
        what: &'static str,
        class_id: ClassId,
    ) {
        if !zgc_corpse_enabled() {
            return;
        }
        // SAFETY: the caller has just written a header at `ptr`.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        let sized = Self::alloc_size(header);
        if sized == Some(reserved) {
            return;
        }
        let n = self.counters.corpse_reports.fetch_add(1, Ordering::Relaxed);
        if n < 16 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                what,
                addr = ptr as usize,
                reserved,
                header_says = ?sized,
                class_id = class_id.as_u32(),
                num_slots = header.num_slots(),
                array_length = header.array_length(),
                kind = ?header.kind(),
                "zgc alloc audit: the bytes reserved for this object and the bytes its \
                 header describes disagree -- the next allocation's base will land inside \
                 its declared extent"
            );
        }
    }

    /// Try to allocate an array. Returns `None` on true heap exhaustion.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if length > ZGC_REAL_MAX_ARRAY_LENGTH {
            return None;
        }
        // zs s6/no: the opt-in exhausted verdict (one relaxed load when off).
        if self.exhausted_verdict_refuses() {
            // zt t2/al: record the request, sized as below.
            self.note_verdict_refusal(
                array_data_size(length, element_type)
                    .ok()
                    .and_then(|data| ARRAY_DATA_OFFSET.checked_add(data)),
            );
            return None;
        }
        let data_size = array_data_size(length, element_type).ok()?;
        let total = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        // The header is written by the INITIALIZER, i.e. before the address
        // enters the object-start registry. See `alloc_raw`.
        let len_u32 = u32::try_from(length).ok()?;
        let init = |ptr: *mut u8| {
            // SAFETY: as in `try_alloc_object`.
            unsafe {
                std::ptr::write(
                    ptr as *mut ObjectHeader,
                    ObjectHeader::new(class_id, ObjectKind::Array, element_type, len_u32, len_u32),
                );
            }
        };
        let ptr = self.alloc_raw_tlab(total, init)?;
        // Allocate BLACK while a concurrent cycle is marking. Must follow
        // the header write above; see `allocate_black_if_marking`.
        self.allocate_black_if_marking(ptr);
        self.audit_reserved_vs_header(ptr, total, "array", class_id);
        // SAFETY: `ptr` is a fresh `total`-byte allocation carrying a header.
        unsafe { Some(ObjectRef::from_raw(ptr)) }
    }

    /// Allocate and pre-initialize primitive-typed slots from JVM descriptors.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        let obj = self.alloc_object(class_id, num_fields);
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| cratonvm_gc_core::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        obj
    }

    /// Fallible descriptor-aware object allocation.
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| cratonvm_gc_core::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        Some(obj)    }

    /// Validate that `addr` is exactly the base of a live object. O(1) — hot
    /// path for conservative-root validation (per candidate qword).
    ///
    /// **TLAB audit.** This deliberately does *not* retire TLABs, and does not
    /// need to: a TLAB-served object is inserted into [`Self::registry`] the
    /// instant it is handed out, inside the owning cell's lock. This predicate
    /// is not GC-only — the JIT calls it on the mutator path (`jit_checkcast`
    /// resolves its receiver through it), and the conservative-root scanner
    /// calls it *before* `collect_garbage` is entered — which is exactly why
    /// `gc::zgc::tlab`'s batched registration could not be adopted here. See
    /// [`ZArenaTlabRegistry`]'s `registry_batch` note.
    #[inline]
    pub fn contains_object_address(&self, addr: usize) -> bool {
        self.registry.contains(addr)
    }

    #[inline]
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        if self.registry.contains(addr) {
            // zr z8/st: the live-TLAB oracle census (one relaxed load when off).
            self.lazy_note_query(addr);
            // SAFETY: the registry contains only live allocation bases.
            Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
        } else {
            None
        }
    }

    /// The primitive inside an auto-box wrapper, or `None` when `candidate` is
    /// an ordinary object.
    ///
    /// The read half of the auto-boxing [`Collector::set_array_element`]
    /// performs for a non-`Object` value stored into a reference array. Without
    /// it the caller gets the *wrapper* back — an object of the synthetic
    /// `AUTOBOX_CLASS_ID`, which has no class name, no methods and no
    /// relationship to the primitive it carries. `GenerationalHeap` and
    /// `G1Collector` both un-box on the read side; this collector did not, so
    /// `Stream.mapToLong(...)`/`mapToDouble(...)` — whose Rust bridge collects
    /// `Value::Long`/`Value::Double` into a reference array — handed the
    /// pipeline a wrapper where a primitive belonged, and every consumer
    /// downstream read either zero or the wrapper's own address.
    ///
    /// Validates the address before dereferencing its header: a reference array
    /// element is a raw word and a stale or garbage one could point anywhere,
    /// so an unchecked read here would be wild. Mirrors
    /// `G1Collector::autobox_payload`.
    pub fn autobox_payload(&self, candidate: ObjectRef) -> Option<Value> {
        self.is_object_address(candidate.as_ptr() as usize)?;
        // SAFETY: `is_object_address` confirmed `candidate` is a registered
        // live allocation base, so its first HEADER_SIZE bytes are a header.
        let header = unsafe { &*(candidate.as_ptr() as *const ObjectHeader) };
        if header.class_id != cratonvm_gc_core::heap::AUTOBOX_CLASS_ID {
            return None;
        }
        Some(<Self as GarbageCollector>::get_field(self, candidate, 0))
    }

    /// Loose containment check returning the base object for any address inside it.
    ///
    /// **TLAB audit.** No retire here either. The interior-pointer fallback
    /// below iterates [`Self::registry`] and decodes a header at each
    /// *registered base* — it never strides raw arena bytes — so an
    /// un-retired chunk tail is invisible to it rather than fatal. Retiring
    /// from a mutator query would also be actively wrong: it would close every
    /// thread's buffer on a conservative-root probe.
    /// Resolve `addr` to the base of the object it points INTO, for PINNING a
    /// frozen peer's conservative roots.
    ///
    /// Deliberately more permissive than [`Self::is_heap_addr`] in exactly two
    /// ways, both of which that method rejects for good reasons that do not
    /// apply here:
    ///
    /// 1. **Misalignment is accepted.** `VmHeap::is_heap_addr`'s ZGC arm drops
    ///    `addr & 0x7 != 0` to converge on the contract Generational and G1
    ///    already had. That is right for deciding whether an ambiguous operand
    ///    word is a reference; it is wrong for deciding whether an object may
    ///    MOVE. A compiled loop's cursor into a `char[]` or `byte[]` is
    ///    routinely misaligned, and if its base is not pinned the array is
    ///    relocated out from under the register holding it.
    ///
    /// 2. **One-past-the-end is accepted** (`addr <= end`, not `addr < end`).
    ///    A loop cursor that has advanced past the last element names no byte
    ///    of the object, so `is_heap_addr` correctly answers `None` -- and the
    ///    object it was walking still must not move.
    ///
    /// Over-approximating here is the SAFE direction: the result is used to
    /// withhold a page from relocation, so a false positive costs one page of
    /// compaction and a false negative costs a use-after-free. That asymmetry
    /// is the whole argument, and it is the one
    /// `coverage_incompleteness_is_page_pinnable` already makes for pinning by
    /// raw address value: "it does not matter whether the address is a base, an
    /// interior pointer or a `long` that happens to look like one, because the
    /// page it falls in is not evacuated either way".
    ///
    /// Measured motivation: with the helper-window discharge on, this workload
    /// SIGSEGVs on a page-ALIGNED fault -- the signature of reading a span
    /// `compact_low_to` vacated and zeroed -- and six repairs aimed at frame
    /// slots, oop maps and remap paths changed nothing, because a derived
    /// pointer has no base anywhere in the frame to find, pin or rewrite.
    ///
    /// # Cost (nio20 wave 6, lane L2)
    ///
    /// `perf` put this function at 22% of `R17NioRows heapcb` (round 19): it
    /// runs once per word of every scanned band at every young collection.
    /// Three changes; the first two change no answer on any arm, the third
    /// changes answers on the unordered arms only, in the safe direction:
    ///
    /// * the envelope screen runs FIRST (most band words -- return addresses,
    ///   stack pointers, small integers -- are outside the arena, and with no
    ///   spill no base is);
    /// * the backwards bit scan stops [`Self::MAX_ALLOC_EXTENT`] below `addr`:
    ///   a base further down would need an extent past that window, and since
    ///   zr z1/rl such extents exist (real arrays over 256 MiB, which
    ///   `alloc_size` sizes and the sweep frees), so a miss is answered from
    ///   the huge-extent table ([`Self::huge_extent_containing`], zr z2/rv)
    ///   rather than taken as definitive. A stale word
    ///   into a long empty stretch (the never-bumped middle of a large-`-Xmx`
    ///   arena) used to scan one bitmap word per 512 bytes down to the next
    ///   live object; now it stops within 256 MiB (`MAX_PLAUSIBLE_BODY` is
    ///   `(1 << 24) * SLOT_SIZE` with 16-byte slots), and empty words are skipped
    ///   four at a time (`HeapBitmap::nearest_base_in`);
    /// * an unordered registry (the `CRATONVM_ZGC_STARTBITS=0` hash arm, or a
    ///   bitmap with a spill) is ANSWERED rather than refused: it used to
    ///   return `None` for every derived pointer, i.e. pin nothing, which is
    ///   the use-after-free direction this function exists to avoid
    ///   ([`ZObjectStarts::nearest_base_within_total`]).
    pub fn resolve_interior_for_pin(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 {
            return None;
        }
        if !self.registry.has_spill() && (addr < self.arena_base || addr > self.arena_end) {
            return None;
        }
        if self.registry.contains(addr) {
            // zr z8/st: the live-TLAB oracle census (one relaxed load when off).
            self.lazy_note_query(addr);
            // SAFETY: the registry contains only live allocation bases.
            return Some(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
        let Some(base) = self
            .registry
            .nearest_base_within_total(addr, addr.saturating_sub(Self::MAX_ALLOC_EXTENT))
        else {
            // No base inside the window: only an extent larger than the
            // window can still contain `addr` (zr z2/rv).
            return self.huge_extent_containing(addr, true);
        };
        // SAFETY: `nearest_base_within_total` yields a live registered base.
        let header = unsafe { &*(base as *const ObjectHeader) };
        // zr z3/rv3: `alloc_size` refuses an array past `MAX_PLAUSIBLE_ARRAY_LEN`
        // (1 << 28 elements) -- e.g. a real `byte[300_000_000]` -- which the
        // sweep still frees when dead (`oversize_array_extent`). A derived
        // pointer in the FIRST 256 MiB of such an array finds its base in the
        // window and used to stop here with `None`: a missed MARK root for a
        // take-over's frozen cursor. The huge-extent table re-validates the
        // base and sizes it from `array_data_size`.
        let Some(size) = Self::alloc_size(header) else {
            return self.huge_extent_containing(addr, true);
        };
        let end = base.checked_add(size)?;
        // `<=`, not `<`: one-past-the-end still names the object for pinning.
        if addr >= base && addr <= end {
            // SAFETY: the registry contains only live bases.
            return Some(unsafe { ObjectRef::from_raw(base as *mut u8) });
        }
        None
    }

    /// zr z2/rv: record a base whose extent exceeds [`Self::MAX_ALLOC_EXTENT`]
    /// (called by `alloc_raw`, the only allocator that can produce one; the
    /// slides never move one).
    ///
    /// # Why the table exists
    ///
    /// The interior probes ([`Self::resolve_interior_for_pin`],
    /// [`Self::is_heap_addr`]) scan back at most `MAX_ALLOC_EXTENT` from an
    /// address. That was exact while `alloc_size` refused every extent past
    /// it -- such an object was never freed (retained unsizable), so failing
    /// to resolve a derived pointer into it cost nothing. Since zr z1 two
    /// default-on switches free such arrays when dead
    /// (`CRATONVM_ZGC_LARGE_ARRAY_SIZING`, `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS`),
    /// and the take-over pass roots a frozen peer's derived pointers through
    /// these probes (`xt_root_scan::takeover_word_probe`: an interior word is a
    /// MARK root, not only a pin). A compiled loop over a `byte[300_000_000]`
    /// that keeps only its cursor in a register past the 256 MiB mark therefore
    /// had its array unmarked and freed under it. The rl lane made such extents
    /// immovable so a missed PIN is harmless; a missed MARK is not.
    pub fn note_huge_extent(&self, base: usize) {
        let c = &self.counters;
        let slots = &c.huge_bases;
        for (i, slot) in slots.iter().enumerate() {
            let cur = slot.load(Ordering::Acquire);
            let free = cur == 0 || !self.registry.contains(cur);
            if free
                && slot
                    .compare_exchange(cur, base, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                c.huge_hwm.fetch_max(i + 1, Ordering::AcqRel);
                return;
            }
        }
        c.huge_overflow.store(true, Ordering::Release);
    }

    /// zr z2/rv: the object of extent past [`Self::MAX_ALLOC_EXTENT`] that
    /// contains `addr` (`inclusive_end`: one-past-the-end counts too, the pin
    /// probe's rule), or `None`. One relaxed load when no such object was
    /// ever allocated. Every slot is re-validated (registered, an array, its
    /// header's own extent past the window), so a stale or re-used slot can
    /// only miss, never answer for the wrong object.
    pub fn huge_extent_containing(&self, addr: usize, inclusive_end: bool) -> Option<ObjectRef> {
        let c = &self.counters;
        let hwm = c.huge_hwm.load(Ordering::Acquire);
        let overflow = c.huge_overflow.load(Ordering::Acquire);
        if hwm == 0 && !overflow {
            return None;
        }
        let contains = |base: usize| -> bool {
            if base == 0 || base > addr || !self.registry.contains(base) {
                return false;
            }
            // zr z4/ab: screened (length bound, extent inside the arena), as
            // the sweep screens an oversize array before it frees one.
            let Some(size) = self.huge_array_extent(base) else {
                return false;
            };
            let end = base + size;
            addr < end || (inclusive_end && addr == end)
        };
        let hit = if overflow {
            // More huge objects than slots: the unbounded scan (correct, and
            // paid only while more than 64 of them are live: since zr z4/ab
            // `prune_huge_extents` re-seeds the slots and drops the flag as
            // soon as the survivors fit again).
            self.registry
                .nearest_base_within_total(addr, self.arena_base)
                .filter(|&b| contains(b))
        } else {
            c.huge_bases[..hwm.min(Z_HUGE_EXTENT_SLOTS)]
                .iter()
                .map(|s| s.load(Ordering::Acquire))
                .find(|&b| contains(b))
        }?;
        c.huge_extent_hits.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `contains` checked that `hit` is a registered base.
        Some(unsafe { ObjectRef::from_raw(hit as *mut u8) })
    }

    /// zr z2/rv: clear the huge-extent slots whose base the sweep unregistered.
    /// Stop-the-world (end of `collect_garbage`), so no `note_huge_extent`
    /// races it; the overflow flag drops once every survivor fits again.
    pub fn prune_huge_extents(&self) {
        let c = &self.counters;
        let hwm = c.huge_hwm.load(Ordering::Acquire).min(Z_HUGE_EXTENT_SLOTS);
        let mut new_hwm = 0usize;
        for (i, slot) in c.huge_bases[..hwm].iter().enumerate() {
            let b = slot.load(Ordering::Acquire);
            if b != 0 && !self.registry.contains(b) {
                slot.store(0, Ordering::Release);
            } else if b != 0 {
                new_hwm = i + 1;
            }
        }
        c.huge_hwm.store(new_hwm, Ordering::Release);
        if c.huge_overflow.load(Ordering::Acquire) && new_hwm < Z_HUGE_EXTENT_SLOTS {
            // Some huge object may never have got a slot. zr z4/ab: the flag
            // used to drop only when NO huge object survived, so one survivor
            // of a past 65-array burst kept every window miss of the interior
            // probes on the unbounded scan for the rest of the run. Re-seed
            // instead: one registry pass, every surviving huge base gets a
            // slot, and the flag drops when they all fit.
            // `CRATONVM_ZGC_HUGE_EXTENT_RESEED=0` restores the old rule.
            if true {
                if self.reseed_huge_extents() {
                    c.huge_overflow.store(false, Ordering::Release);
                }
            } else if new_hwm == 0 && self.registered_huge_extents().is_empty() {
                c.huge_overflow.store(false, Ordering::Release);
            }
        }
    }

    /// zr z4/ab: the extent of the array at the registered base `base` when
    /// it exceeds [`Self::MAX_ALLOC_EXTENT`], screened as the sweep's
    /// `oversize_array_extent` screens one: the length is within
    /// `ZGC_REAL_MAX_ARRAY_LENGTH` and the extent ends inside the arena. In
    /// overflow mode the base is a registry scan result, not a recorded
    /// allocation, so a clobbered length must not claim a huge extent.
    pub fn huge_array_extent(&self, base: usize) -> Option<usize> {
        // SAFETY: callers pass a registered base, a live allocation whose
        // header the allocator wrote before registering it.
        let header = unsafe { &*(base as *const ObjectHeader) };
        huge_array_extent_of(header, base, self.arena_end)
    }

    /// zs s1/dg: the exact extent of the LIVE array at `base` whose length
    /// exceeds [`Self::MAX_PLAUSIBLE_ARRAY_LEN`] -- the arrays `alloc_size`'s
    /// element cap refuses although the allocator admits them -- or `None`
    /// when any check fails (or `CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS=0`).
    ///
    /// The live twin of the sweep's `oversize_array_extent`. Each check is
    /// independent of the length being trusted:
    ///
    /// 1. an array whose length the allocator could have produced and whose
    ///    extent is past [`Self::MAX_ALLOC_EXTENT`] and ends inside the arena
    ///    ([`huge_array_extent_of`], the huge-extent table's screen);
    /// 2. `base` is a registered object start;
    /// 3. no other registered base starts inside `(base, base + size)`: a
    ///    clobbered length that swallows a neighbour is refused. A slide never
    ///    moves such an extent (`MAX_ALLOC_EXTENT`; and a survivor `alloc_size`
    ///    cannot size stops the low slide), so pre- and post-slide registry
    ///    answers are the same for it.
    ///
    /// Cost: one bitmap scan of `size / 8` bits, paid only for the (rare)
    /// arrays past 268M elements, once per pass that meets one.
    pub fn oversize_live_array_extent(&self, base: usize, header: &ObjectHeader) -> Option<usize> {
        if !self
            .counters
            .rewrite_oversize_arrays
            .load(Ordering::Relaxed)
            || header.kind() != ObjectKind::Array
            || (header.array_length() as usize) <= Self::MAX_PLAUSIBLE_ARRAY_LEN
            || base < self.arena_base
        {
            return None;
        }
        // zs s2/rf (`CRATONVM_ZGC_OVERSIZE_SCREEN_MEMO`): one registry scan
        // per array per pause instead of one per consumer.
        let len = header.array_length();
        let c = &self.counters;
        let memo = c.oversize_screen_memo_armed.load(Ordering::Acquire);
        if memo {
            if let Some(&(_, _, answer)) = c
                .oversize_screen_memo
                .lock()
                .iter()
                .find(|&&(b, l, _)| b == base && l == len)
            {
                c.oversize_screen_memo_hits.fetch_add(1, Ordering::Relaxed);
                // zs s3/rv2: a positive is re-validated on the hit. The sweep
                // can remove THIS base inside the pause (the complement frees a
                // dead over-long array whose header it leaves in place), and
                // the key does not carry the element type; both re-checks are
                // O(1) (one registry bit, the extent arithmetic). A refusal is
                // returned as memoised: it is the conservative answer.
                return answer.filter(|&size| {
                    self.registry.contains(base)
                        && huge_array_extent_of(header, base, self.arena_end) == Some(size)
                });
            }
        }
        let answer = self.oversize_live_array_extent_scan(base, header);
        c.oversize_screen_scans.fetch_add(1, Ordering::Relaxed);
        if memo {
            let mut m = c.oversize_screen_memo.lock();
            if !m.iter().any(|&(b, l, _)| b == base && l == len) {
                m.push((base, len, answer));
            }
        }
        answer
    }

    /// The registry half of [`Self::oversize_live_array_extent`] (checks 1-3),
    /// unmemoised.
    pub fn oversize_live_array_extent_scan(&self, base: usize, header: &ObjectHeader) -> Option<usize> {
        let size = huge_array_extent_of(header, base, self.arena_end)?;
        if !self.registry.contains(base) {
            return None;
        }
        if self
            .registry
            .nearest_base_within_total(base + size - 1, base + 8)
            .is_some()
        {
            return None;
        }
        Some(size)
    }

    /// zs s2/rf: `(memo_hits, registry_scans)` of the oversize live-array
    /// screen (`CRATONVM_ZGC_OVERSIZE_SCREEN_MEMO`). With an over-long array
    /// live across a stop-the-world cycle the memo arm shows about one scan
    /// per cycle and the rest as hits; at `=0` every call is a scan (the
    /// positive control).
    pub fn oversize_screen_memo_stats(&self) -> (usize, usize) {
        (
            self.counters
                .oversize_screen_memo_hits
                .load(Ordering::Relaxed),
            self.counters.oversize_screen_scans.load(Ordering::Relaxed),
        )
    }

    /// zs s2/rf: dead oversize arrays the per-object sweep (`sweep_one`)
    /// freed (`CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE`; also in
    /// [`Self::oversize_array_sweep_stats`]'s `swept`).
    pub fn sweep_one_oversize_freed(&self) -> usize {
        self.counters
            .sweep_one_oversize_freed
            .load(Ordering::Relaxed)
    }

    /// zs s1/dg: the size of the survivor at `base` for the slide's census,
    /// partition and both slides: [`Self::alloc_size`], or -- for a real
    /// array past `MAX_PLAUSIBLE_ARRAY_LEN` that it refuses --
    /// [`Self::oversize_live_array_extent`].
    ///
    /// # Why the slide needs it, not only the rewrite
    ///
    /// `size_via_cell` / `logical_pages` read an unsizable survivor as ZERO
    /// bytes. For such an array on an UNSELECTED low page the partition then
    /// recorded no obstacle and a `highest_pinned_end` of its BASE, and the
    /// census booked none of its bytes on the pages its body spans -- so those
    /// pages looked empty (selectable), and the slide's destination cursor
    /// could place survivors from above INTO the live array's payload: silent
    /// corruption of a live array, and space the heap did not have. On a
    /// selected page, or in the large-object end, the unsizable survivor
    /// instead stopped the slide for everything above (below) it, which
    /// starves compaction (`NativeGrowthReclaimProbe toCharArray=3/4` on the
    /// host). Sized, it is an obstacle like any other extent, and past
    /// `MAX_ALLOC_EXTENT` it is never moved.
    #[inline]
    pub fn survivor_size(&self, base: usize, header: &ObjectHeader) -> Option<usize> {
        match Self::alloc_size(header) {
            Some(size) => Some(size),
            None => {
                let size = self.oversize_live_array_extent(base, header);
                if size.is_some() {
                    self.counters
                        .oversize_survivors_sized
                        .fetch_add(1, Ordering::Relaxed);
                }
                size
            }
        }
    }

    /// zs s2/rv: [`Self::survivor_size`] without its engagement count, for the
    /// readers outside the slide that dg's residual page
    /// (`zgc-zs1dg-over-long-array-residual-consumers-20261014.md`, rows 2, 4
    /// and 6) left on `alloc_size` alone: the sweep's live-byte accounting,
    /// `walk_objects` and the debug slide verifier. Same screen, same kill
    /// (`CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS=0` answers `alloc_size`'s
    /// `None`, i.e. the pre-fix behaviour bit for bit).
    #[inline]
    pub fn live_extent(&self, base: usize, header: &ObjectHeader) -> Option<usize> {
        Self::alloc_size(header).or_else(|| self.oversize_live_array_extent(base, header))
    }

    /// zs s1/dg: survivors [`Self::survivor_size`] sized through the oversize
    /// screen (engagement of `CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS` in the
    /// slide's census, partition and slides).
    pub fn oversize_survivors_sized(&self) -> usize {
        self.counters
            .oversize_survivors_sized
            .load(Ordering::Relaxed)
    }

    /// zs s1/dg: rewrite targets the oversize screen sized
    /// (`CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS`, engagement).
    pub fn rewrite_oversize_arrays_walked(&self) -> usize {
        self.counters
            .rewrite_oversize_arrays_walked
            .load(Ordering::Relaxed)
    }

    /// zs s1/dg: reference arrays past `MAX_PLAUSIBLE_ARRAY_LEN` whose
    /// elements a slot walk omitted (should stay `0` on a healthy heap).
    pub fn oversize_ref_arrays_omitted(&self) -> usize {
        self.counters
            .oversize_ref_arrays_omitted
            .load(Ordering::Relaxed)
    }

    /// Survivors the rewrite pass refused as unwalkable, logged or not
    /// (`unwalkable_reports` counts every refusal; only 16 are logged).
    pub fn rewrite_unwalkable_reports(&self) -> usize {
        self.counters.unwalkable_reports.load(Ordering::Relaxed)
    }

    /// Every registered base whose extent passes [`Self::huge_array_extent`].
    /// Stop-the-world callers only (`prune_huge_extents`).
    pub fn registered_huge_extents(&self) -> Vec<usize> {
        let mut found = Vec::new();
        self.registry.for_each_base(self.arena_end, &mut |base| {
            if self.huge_array_extent(base).is_some() {
                found.push(base);
            }
            true
        });
        found
    }

    /// zr z4/ab: give every surviving huge base a slot (the ones already in
    /// the table keep theirs). `true` when all of them fit, i.e. the
    /// unbounded fallback is no longer needed. Stop-the-world (end of
    /// `collect_garbage`), so no `note_huge_extent` races it.
    pub fn reseed_huge_extents(&self) -> bool {
        let c = &self.counters;
        let survivors = self.registered_huge_extents();
        if survivors.len() > Z_HUGE_EXTENT_SLOTS {
            return false;
        }
        let mut hwm = c.huge_hwm.load(Ordering::Acquire).min(Z_HUGE_EXTENT_SLOTS);
        for base in survivors {
            let held = c.huge_bases[..hwm]
                .iter()
                .any(|s| s.load(Ordering::Acquire) == base);
            if held {
                continue;
            }
            let Some(i) = c
                .huge_bases
                .iter()
                .position(|s| s.load(Ordering::Acquire) == 0)
            else {
                return false;
            };
            c.huge_bases[i].store(base, Ordering::Release);
            hwm = hwm.max(i + 1);
        }
        c.huge_hwm.store(hwm, Ordering::Release);
        c.huge_extent_reseeds.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// zr z7/pc: survivors the slide's reference rewrite served from the
    /// size table without a header read (`CRATONVM_ZGC_RELOCATE_SIZE_TABLE`).
    pub fn relocate_size_table_skips(&self) -> usize {
        self.counters
            .relocate_size_table_skips
            .load(Ordering::Relaxed)
    }

    /// zr z7/pc: survivors the slide's page census sized from the sweep's
    /// hints (`CRATONVM_ZGC_SWEEP_SIZE_HINTS`).
    pub fn sweep_size_hint_hits(&self) -> usize {
        self.counters.sweep_size_hint_hits.load(Ordering::Relaxed)
    }

    /// zr z2/rv: interior probes the huge-extent table answered.
    pub fn huge_extent_hits(&self) -> usize {
        self.counters.huge_extent_hits.load(Ordering::Relaxed)
    }

    /// zr z4/ab: collections that re-seeded the huge-extent table out of
    /// overflow mode. See [`Self::reseed_huge_extents`].
    pub fn huge_extent_reseeds(&self) -> usize {
        self.counters.huge_extent_reseeds.load(Ordering::Relaxed)
    }

    /// Is the huge-extent table in overflow mode (every window miss of the
    /// interior probes takes the unbounded scan)? Tests and diagnostics.
    pub fn huge_extent_overflowed(&self) -> bool {
        self.counters.huge_overflow.load(Ordering::Acquire)
    }

    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        // Fast path: an exact object base (the overwhelmingly common probe).
        if self.registry.contains(addr) {
            // zr z8/st: the live-TLAB oracle census (one relaxed load when off).
            self.lazy_note_query(addr);
            // SAFETY: the registry contains only live allocation bases.
            return Some(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
        // ---- Two screens BEFORE the extent walk -------------------------
        //
        // Neither changes the answer; both exist because the fallback's cost
        // shape changed when the registry became a bitmap. The `FxHashSet`
        // iteration was O(live); a bitmap walk is O(arena span), so on a
        // mostly-empty multi-gigabyte heap an unbounded walk here would be a
        // REGRESSION (~8.2M word loads for a 4.2 GB arena against a few
        // thousand hash steps). That matters because this is not a GC-only
        // path: `VmHeap::is_heap_addr`'s ZGC arm (`vm_heap.rs:694-700`) feeds
        // it per-slot conservative root scanning over ambiguous
        // JVM-long-vs-jobject operand words, whose dominant population is
        // zeros, small integers and long bit patterns — every one of which
        // misses the exact-base probe above and lands here.
        //
        // Screen 1 — the arena envelope, which `vm_heap.rs`'s own `TODO(zgc)`
        // on that arm asks for by name ("once `ZgcRealHeap` publishes its arena
        // envelope, the range compare belongs here too"). It cannot lose a
        // base: `has_spill()` is false exactly when every base this registry
        // holds was encodable on the arena grid and is therefore inside the
        // envelope, and no in-arena object's extent can reach outside it
        // either. When it is true (the `Hash` kill-switch arm, or a spill) the
        // screen stands down and the behaviour is the pre-change one, byte for
        // byte.
        if !self.registry.has_spill() && (addr < self.arena_base || addr >= self.arena_end) {
            return None;
        }
        // Screen 3 — the one that removes the walk instead of bounding it.
        //
        // Allocations do not overlap, so the only base whose extent can contain
        // `addr` is the greatest base `<= addr`. On the bitmap arm (no spill)
        // that is one backwards bit scan and ONE header dereference — see
        // `ZObjectStartBits::nearest_base_at_or_below` for the `perf record`
        // that made this the top of the profile (80.7% of all CPU samples on
        // `type.temporal.InstantTests`, a class HotSpot finishes in 6.7 s).
        //
        // Screen 2 below bounded the same walk by the arena high-water mark,
        // which helps only while the cursor is low; once a long-running heap
        // has bumped to capacity the bound is the whole bitmap again. This
        // does not depend on occupancy at all.
        //
        // nio20 wave 6 (L2): bounded at `MAX_ALLOC_EXTENT` below `addr` (no
        // lower base can reach `addr`), and "no base in that window" -- which
        // used to fall through to the O(live) walk below whenever the scan ran
        // off the bottom of the bitmap -- is definitive on this arm too.
        let nearest = match self
            .registry
            .nearest_base_within(addr, addr.saturating_sub(Self::MAX_ALLOC_EXTENT))
        {
            // No base in the window: definitive for every extent the window
            // covers; one past it is the huge-extent table's (zr z2/rv).
            Some(None) => return self.huge_extent_containing(addr, false),
            Some(Some(base)) => Some(base),
            None => None,
        };
        if let Some(base) = nearest {
            let header = unsafe { &*(base as *const ObjectHeader) };
            // Same refusal as the walk below: an unsizable header cannot be
            // said to CONTAIN anything.
            if let Some(size) = Self::alloc_size(header) {
                if let Some(end) = base.checked_add(size) {
                    if addr >= base && addr < end {
                        // SAFETY: the registry contains only live bases.
                        return Some(unsafe { ObjectRef::from_raw(base as *mut u8) });
                    }
                }
            } else {
                // zr z3/rv3: an array past 1 << 28 elements that `alloc_size`
                // refuses but the sweep frees -- see the same arm in
                // `resolve_interior_for_pin`. The table re-validates the base.
                return self.huge_extent_containing(addr, false);
            }
            // The nearest base does not cover `addr`, and no LOWER base can
            // (its extent would have to span across this one). Definitive.
            return None;
        }
        let Some(base) = self
            .registry
            .nearest_base_within_total(addr, addr.saturating_sub(Self::MAX_ALLOC_EXTENT))
        else {
            return self.huge_extent_containing(addr, false);
        };
        // SAFETY: the registry returns a live allocation base.
        let header = unsafe { &*(base as *const ObjectHeader) };
        let Some(size) = Self::alloc_size(header) else {
            return self.huge_extent_containing(addr, false);
        };
        let end = base.checked_add(size)?;
        if addr >= base && addr < end {
            // SAFETY: the registry contains only live allocation bases.
            return Some(unsafe { ObjectRef::from_raw(base as *mut u8) });
        }
        None
    }

    /// Total backing arena capacity.
    pub fn heap_capacity(&self) -> usize {
        // zr z1/tr: lock-free. The arena is created once in
        // `with_capacity_and_initial` and never grown or replaced, and
        // `arena_end - arena_base` is captured from its `capacity()` there --
        // the same property `conservative_addr_span` rests on. This took the
        // arena lock, and `needs_gc`'s warm-up clause called it on every poll
        // before the third collection.
        self.arena_end.saturating_sub(self.arena_base)
    }

    /// zr z1/tr: HotSpot's `soft_max_capacity()` -- `-XX:SoftMaxHeapSize`,
    /// never above the capacity. Defaults to the capacity.
    pub fn soft_max_capacity(&self) -> usize {
        let soft = usize::try_from(self.counters.director.soft_max()).unwrap_or(usize::MAX);
        soft.min(self.heap_capacity())
    }

    /// zr z1/tr: `-XX:SoftMaxHeapSize`. Read by the warm-up clause and the
    /// [`director`] rules; anything at or above the capacity means the
    /// capacity. HotSpot ZGC's soft maximum is a goal, not a limit: the heap
    /// still grows to `-Xmx` when the live set needs it.
    ///
    /// zt t7/x4b (`CRATONVM_ZGC_SOFT_MAX_ZERO_HOTSPOT`, default ON, `=0`
    /// kill): `0` is a soft maximum of 0, as HotSpot 25.0.3 keeps it
    /// (`-XX:SoftMaxHeapSize=0 -Xlog:gc+init` prints `Soft Max Capacity:
    /// 0M`). Under `=0` it means the capacity, as before.
    pub fn set_soft_max_heap_bytes(&self, bytes: usize) {
        self.apply_soft_max_heap_bytes(bytes);
        // zt t6/tc: the JMX row's origin (the VM's only production caller is
        // the `-XX:SoftMaxHeapSize` launch path in `vm_init`).
        self.counters.soft_max_origin.store(1, Ordering::Relaxed);
    }

    /// zt t28/sm (`CRATONVM_ZGC_SOFT_MAX_ERGONOMIC`): HotSpot 25's
    /// ergonomic soft maximum (90 % of an ergonomic `MaxHeapSize`), applied
    /// as [`Self::set_soft_max_heap_bytes`] but keeping the default origin
    /// (`ERGONOMIC`, as HotSpot's `FLAG_SET_ERGO_IF_DEFAULT`).
    ///
    /// zt t29/sm: it paces only under the opt-in
    /// `CRATONVM_ZGC_SOFT_MAX_ERGONOMIC_PACING`; otherwise the heap reports it
    /// ([`Self::reported_soft_max_capacity`]) and paces at the capacity.
    pub fn set_ergonomic_soft_max_heap_bytes(&self, bytes: usize) {
        if zgc_soft_max_ergonomic_pacing_enabled() {
            self.apply_soft_max_heap_bytes(bytes);
        } else {
            self.counters
                .soft_max_report
                .store(bytes.min(self.heap_capacity()), Ordering::Relaxed);
        }
    }

    /// zt t29/sm: the soft maximum Java and the logs see -- the ergonomic
    /// value a non-pacing [`Self::set_ergonomic_soft_max_heap_bytes`] stored,
    /// else [`Self::soft_max_capacity`].
    pub fn reported_soft_max_capacity(&self) -> usize {
        match self.counters.soft_max_report.load(Ordering::Relaxed) {
            0 => self.soft_max_capacity(),
            report => report.min(self.heap_capacity()),
        }
    }

    /// [`Self::set_soft_max_heap_bytes`] without the origin store (zt t7/x4:
    /// the management write stored the command-line origin first, so a
    /// concurrent `getVMOption` could read `COMMAND_LINE` for a value set by
    /// `setVMOption`).
    pub fn apply_soft_max_heap_bytes(&self, bytes: usize) {
        self.counters.soft_max_report.store(0, Ordering::Relaxed);
        let cap = self.heap_capacity();
        let soft = if bytes == 0 && !zgc_soft_max_zero_hotspot_enabled() {
            cap
        } else {
            bytes.min(cap)
        };
        self.counters.director.set_soft_max(soft as u64);
        // zt t1/tp (`CRATONVM_ZGC_SOFT_MAX_PACES`, default ON): the live-bytes
        // clause also fires at the soft maximum's threshold.
        let paces = self.counters.soft_max_paces.load(Ordering::Relaxed);
        let soft_threshold = if paces && soft < cap {
            soft / 100 * ZGC_REAL_GC_THRESHOLD_PERCENT
        } else {
            usize::MAX
        };
        self.counters
            .soft_max_threshold
            .store(soft_threshold, Ordering::Relaxed);
    }

    /// zt t6/tc (zt1tp item 2): `HotSpotDiagnosticMXBean.setVMOption(
    /// "SoftMaxHeapSize", ..)` -- [`Self::set_soft_max_heap_bytes`] at run
    /// time, recorded as a management write. HotSpot's director re-reads the
    /// flag on every tick; every reader here (`needs_gc`'s warm-up clause,
    /// `soft_max_threshold`, the director's rules) reads the stored value per
    /// poll, so the next poll sees it.
    pub fn set_soft_max_heap_bytes_by_management(&self, bytes: usize) {
        self.apply_soft_max_heap_bytes(bytes);
        self.counters.soft_max_origin.store(2, Ordering::Relaxed);
    }

    /// zt t6/tc (zt1tp item 4): `-XX:ZProactive` for this heap's director.
    pub fn set_director_proactive(&self, on: bool) {
        self.counters.director.set_proactive(on);
    }

    /// zt t6/tc (zt1tp item 4): `-XX:ZAllocationSpikeTolerance` for this
    /// heap's director.
    pub fn set_director_allocation_spike_tolerance(&self, tolerance: f64) {
        self.counters
            .director
            .set_allocation_spike_tolerance(tolerance);
    }

    /// zt t7/tg (zt1tp item 3): `-XX:ZCollectionInterval=<seconds>` for this
    /// heap's `Timer` rule; `None` (HotSpot's default 0) turns it off. Per VM.
    pub fn set_collection_interval(&self, interval: Option<std::time::Duration>) {
        self.counters.timer.set_interval(interval);
    }

    /// zt t7/tg: the `Timer` rule's interval, `None` when off.
    pub fn collection_interval(&self) -> Option<std::time::Duration> {
        self.counters.timer.interval()
    }

    /// zt t27/gn5 (the gn page's item 14): `-XX:ZCollectionIntervalMinor=
    /// <seconds>` for this heap's minor `Timer` rule (HotSpot 25's
    /// `rule_minor_timer`; `timer.rs` module doc); `None` (HotSpot's default
    /// `-1`) turns it off. Honoured under `CRATONVM_ZGC_GEN_MINOR_TIMER`
    /// (default ON, `=0` kill: the flag is parsed and ignored, as before), and
    /// acting only in the generational mode ([`Self::minor_timer_armed`]): a
    /// single-generation heap has no young collection to run. Per VM.
    pub fn set_collection_interval_minor(&self, interval: Option<std::time::Duration>) {
        let honoured = zgc_flag_default_on("CRATONVM_ZGC_GEN_MINOR_TIMER");
        self.counters
            .timer
            .set_minor_interval(if honoured { interval } else { None });
    }

    /// zt t27/gn5: the minor `Timer` rule's interval as set, `None` when off.
    pub fn collection_interval_minor(&self) -> Option<std::time::Duration> {
        self.counters.timer.minor_interval()
    }

    /// zt t27/gn5: the minor `Timer` rule's interval when it acts -- set and
    /// the generational mode on -- else `None`.
    pub fn minor_timer_armed(&self) -> Option<std::time::Duration> {
        if self.generational_enabled() {
            self.counters.timer.minor_interval()
        } else {
            None
        }
    }

    /// zt t27/gn5: minor `Timer` collections the director tick asked for
    /// (`[GC]` / tests).
    pub fn director_tick_minor_timer_requests(&self) -> usize {
        self.counters.timer.minor_timer_requests()
    }

    /// zt t10/tg3 (`zgc-zt9tg2-timer-and-launcher-residuals-SUPERSEDED-20261019.md`
    /// item 2): `-XX:+ZCollectionIntervalOnly` for this heap. Honoured under
    /// `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY` (default ON since zt t14/tg6,
    /// rows tg5_i1/i1_c/i2/i3/tg3_O1/O3 SAME; `=0` kill: the flag is parsed
    /// and ignored, as before). Inert without the flag. `needs_gc` then
    /// answers no to every allocation-polled clause (warm-up, threshold,
    /// headroom, allocation budget, nursery, the director's rules), so a
    /// collection starts only from the `Timer` rule's tick or from an
    /// allocation that fails (`Allocation Stall`), as HotSpot 25's director
    /// with `ZCollectionIntervalOnly` starts only `Timer` cycles. The VM's
    /// occupancy-classified forced doors (`allocation_failure_gc_cause`'s
    /// `occupancy` doors) still collect, as allocation stalls.
    /// `CRATONVM_DBG_GC_STRESS` still collects (a diagnostic).
    pub fn set_collection_interval_only(&self, on: bool) {
        let honoured = on && zgc_flag_default_on("CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY");
        self.counters.timer.set_interval_only(honoured);
    }

    /// zt t10/tg3: whether `-XX:+ZCollectionIntervalOnly` is in force here.
    pub fn collection_interval_only(&self) -> bool {
        self.counters.timer.interval_only()
    }

    /// zt t7/tg: whether the VM must run a director tick thread for this heap
    /// -- the `Timer` rule is armed, or the idle proactive tick
    /// (`CRATONVM_ZGC_DIRECTOR_IDLE_TICK`, default ON since zt t20) serves a
    /// deciding director.
    /// `false` on a default run: no thread starts.
    pub fn director_tick_wanted(&self) -> bool {
        self.counters.timer.interval().is_some()
            // zt t27/gn5: the minor `Timer` rule (generational mode only).
            || self.minor_timer_armed().is_some()
            || self.idle_proactive_tick_armed()
            // zt t23/zc6 (`CRATONVM_ZGC_MARK_END_SERVICE`, opt-in): the tick
            // also closes an every-trigger cycle at its fixed point.
            || self.mark_end_service_armed()
    }

    pub fn idle_proactive_tick_armed(&self) -> bool {
        self.counters.timer.idle_proactive() && self.counters.director.is_deciding()
    }

    /// zt t7/tg: one director tick at `now` -- collect for `Timer` (or the
    /// opt-in idle `Proactive`), wait, or stop. Pure arithmetic over the
    /// heap's clock ([`timer::plan`]); the caller runs the collection.
    pub fn director_tick(&self, now: std::time::Instant) -> timer::ZTick {
        // zt t9/tg2: stopped at VM exit (`Self::stop_director_tick`).
        if self.counters.timer.is_stopped() {
            return timer::ZTick::Off;
        }
        // zt t10/tg3: under `-XX:+ZCollectionIntervalOnly` only the timer
        // rule asks (`Self::set_collection_interval_only`).
        let proactive_tick =
            self.idle_proactive_tick_armed() && !self.counters.timer.interval_only();
        let proactive_due = proactive_tick
            && self
                .counters
                .director
                .idle_proactive_due(self.allocated.load(Ordering::Relaxed) as u64, now);
        let base = timer::plan(
            self.counters.timer.interval(),
            self.counters.timer.since_last_major(now),
            proactive_due,
            proactive_tick,
        );
        // zt t27/gn5 (`CRATONVM_ZGC_GEN_MINOR_TIMER`): HotSpot's minor
        // decision, after the major rules; `base` unchanged when the minor
        // timer is not armed.
        let base = timer::plan_minor(
            base,
            self.minor_timer_armed(),
            self.counters.timer.since_last_young(now),
        );
        // zt t23/zc6 (`CRATONVM_ZGC_MARK_END_SERVICE`, opt-in): close an open
        // every-trigger cycle found at its fixed point on two looks
        // (`ZTick::CloseMark`), or look again soon while one is open. `base`
        // unchanged when the service is not armed.
        self.mark_end_tick(base)
    }

    pub fn tick_cause_bit(cause: timer::ZTickCause) -> usize {
        match cause {
            // zt t27/gn5: HotSpot's minor timer reports `GCCause::_z_timer`
            // too (`Minor Collection (Timer)`).
            timer::ZTickCause::Timer | timer::ZTickCause::MinorTimer => TRIGGER_BIT_TIMER,
            timer::ZTickCause::Proactive => TRIGGER_BIT_PROACTIVE,
        }
    }

    /// zt t21/tg10: an allocation poll's director asked for `Proactive` and
    /// found the bit set (see [`ZgcCounters::proactive_poll_co_claim`]). A
    /// load and a compare on the repeat polls of one pending collection; one
    /// relaxed store per collection.
    #[inline]
    pub fn note_proactive_poll_co_claim(&self) {
        let stamp = self.gc_count.load(Ordering::Relaxed).wrapping_add(1).max(1);
        let co_claim = &self.counters.proactive_poll_co_claim;
        if co_claim.load(Ordering::Relaxed) != stamp {
            co_claim.store(stamp, Ordering::Relaxed);
        }
    }

    /// zt t21/tg10 (`zgc-zt12tg5-*` item 7, the stop-withdrawal edge;
    /// `CRATONVM_ZGC_TICK_WITHDRAW_KEEPS_POLL_PROACTIVE`, default ON, `=0`
    /// kill): did an allocation poll ask for `Proactive` for the collection
    /// still pending at `cycles` (the count the tick's claim was checked
    /// against)? Then the bit is the poll's cause as well as the tick's, and
    /// a tick withdrawal must leave it: HotSpot's director, asked by the poll,
    /// would still run that `Proactive` cycle. `Timer` has no other writer.
    pub fn tick_bit_co_claimed_by_poll(&self, cause: timer::ZTickCause, cycles: usize) -> bool {
        matches!(cause, timer::ZTickCause::Proactive)
            && self
                .counters
                .proactive_poll_co_claim
                .load(Ordering::Relaxed)
                == cycles.wrapping_add(1).max(1)
            && zgc_flag_default("CRATONVM_ZGC_TICK_WITHDRAW_KEEPS_POLL_PROACTIVE", true)
    }

    /// zt t7/tg: charge `cause` on the trigger latch for the collection the
    /// tick is about to take through the allocation-threshold door, so the
    /// cycle reports HotSpot's cause (`Timer`, `Proactive`) and paces as a
    /// rule-driven cycle. On a generational heap with an old generation the
    /// cycle is made a major, as HotSpot's `rule_major_timer` /
    /// `rule_major_proactive` start major collections (`gen_force_major_next`
    /// is cleared by the next whole-heap cycle, whoever runs it).
    ///
    /// zt t27/gn5: except [`timer::ZTickCause::MinorTimer`] (`rule_minor_timer`
    /// starts a MINOR collection): its cycle is left to the cycle-kind
    /// decision, which runs it young whenever a young cycle is possible.
    pub fn is_conc_cycle_active(&self) -> bool {
        self.conc_cycle_active.load(Ordering::Relaxed)
    }

    pub fn conc_sweep_pending(&self) -> bool {
        self.counters.csweep.pending()
    }

    pub fn try_arm_tick_concurrent_mark(&self, cause: timer::ZTickCause) -> bool {
        if !self.conc_every_trigger_armed()
            || self.generational_enabled()
            || !matches!(cause, timer::ZTickCause::Proactive | timer::ZTickCause::Timer)
            || self.conc_cycle_active.load(Ordering::Relaxed)
            || self.counters.csweep.pending()
        {
            return false;
        }
        let a = self.allocated.load(Ordering::Relaxed);
        !self.conc_every_trigger_early(a)
    }

    pub fn request_tick_cycle(&self, cause: timer::ZTickCause) {
        let charged = self.claim_trigger(Self::tick_cause_bit(cause));
        // zt t9/tg2: remember whether the bit is the tick's own, for the
        // withdrawal below.
        self.counters
            .timer
            .note_claim(charged, self.gc_count.load(Ordering::Relaxed));
        self.counters.timer.note_request(cause);
        if !matches!(cause, timer::ZTickCause::MinorTimer)
            && self.generational_enabled()
            && self.has_old_objects.load(Ordering::Relaxed)
        {
            self.gen_force_major_next.store(true, Ordering::Relaxed);
        }
    }

    /// zt t7/tg: the tick's pause lost to another thread's, which ran a cycle
    /// that answered the request (the rule no longer says yes): take the
    /// cause bit back off the latch so it does not label a LATER, unrelated
    /// collection. Returns whether the request was withdrawn; `false` means
    /// the rule still says yes and the caller tries again.
    pub fn withdraw_tick_cycle_if_answered(
        &self,
        cause: timer::ZTickCause,
        now: std::time::Instant,
    ) -> bool {
        let still_due = match cause {
            timer::ZTickCause::Timer => self.counters.timer.timer_due(now),
            // zt t27/gn5: every cycle restarts the minor clock, so a lost
            // pause is always an answered one.
            timer::ZTickCause::MinorTimer => self.counters.timer.minor_timer_due(now),
            // A proactive yes is re-evaluated by the next tick, not retried.
            timer::ZTickCause::Proactive => false,
        };
        if still_due {
            return false;
        }
        // zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals` item 8,
        // `CRATONVM_ZGC_TICK_WITHDRAW_OWN_BIT`, default ON, `=0` kill): take
        // the bit back only while it is the tick's own. `Proactive` is also
        // the bit an allocation poll's director charges, so a cleared latch
        // re-charged by a poll after the answering cycle (or a bit the poll
        // had charged before the tick asked) is the NEXT collection's cause,
        // and clearing it dropped that collection's `Proactive`. `Timer` has
        // no other writer, so its withdrawal is unchanged in effect.
        let own_only = zgc_flag_default("CRATONVM_ZGC_TICK_WITHDRAW_OWN_BIT", true);
        let cycles = self.gc_count.load(Ordering::Relaxed);
        // zt t21/tg10: a bit a poll co-claimed stays (see
        // `tick_bit_co_claimed_by_poll`).
        if (!own_only || self.counters.timer.claim_still_owned(cycles))
            && !(own_only && self.tick_bit_co_claimed_by_poll(cause, cycles))
        {
            self.counters
                .trigger_seen
                .fetch_and(!Self::tick_cause_bit(cause), Ordering::Relaxed);
        }
        self.counters.timer.note_withdrawn();
        true
    }

    /// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals` item 9): stop this
    /// heap's director tick for good, as HotSpot's `before_exit` stops its
    /// director: the next tick plans `Off` and the tick thread ends. For the
    /// VM's exit path, after the shutdown hooks; a no-op without a tick.
    pub fn stop_director_tick(&self) {
        self.counters.timer.stop();
    }

    /// zt t10/tg3 (`t10-x7-report.md` R1, `CRATONVM_ZGC_TICK_STOP_ABORTS`,
    /// default ON, `=0` kill): must a tick collection that has not started
    /// its pause yet stand down? `true` once [`Self::stop_director_tick`]
    /// ran: the stop ended the tick loop at its next plan, but a collection
    /// the tick had already handed to its attached thread went on and retried
    /// up to three times, so a `Timer` cycle could still start (and print its
    /// `-Xlog:gc` lines) after the `gc,exit` summary. HotSpot's director
    /// starts no cycle after `before_exit` stopped it. `=0`: the t9 tick,
    /// whose job never reads the stop.
    pub fn director_tick_stopped(&self) -> bool {
        self.counters.timer.is_stopped() && zgc_flag_default("CRATONVM_ZGC_TICK_STOP_ABORTS", true)
    }

    /// zt t10/tg3 (`t10-x7-report.md` R1): a tick collection that stands
    /// down at the stop ([`Self::director_tick_stopped`]) after its request
    /// charged the cause bit takes the bit back, when it is still its own
    /// (the request charged it and no collection completed since,
    /// [`timer::ZTimer::claim_still_owned`]); otherwise the bit would label
    /// a LATER, unrelated collection `Timer` / `Proactive` (and, with
    /// `CRATONVM_ZGC_TIMER_NOT_WARMUP`, clear that collection's warm-up bit).
    /// A bit a poll charged, or one a completed collection already consumed,
    /// is left alone. Called on the tick's own thread only -- at the top of
    /// each attempt (a bit a previous attempt's lost pause left) and between
    /// an attempt's request and its pause -- so it never races its own
    /// collection's read of the latch. Returns whether the bit was taken back.
    ///
    /// zt t11/tg4 (`t11-x8-report.md` T4): `Proactive` is also the bit an
    /// allocation poll's director charges. While the tick's claim stands (it
    /// charged the bit and no collection completed since), a poll that asks
    /// for `Proactive` for the same pending cycle finds the bit already set,
    /// so this withdrawal drops that poll's label too: the next collection
    /// reports its other cause. Reachable only with the idle tick
    /// (`CRATONVM_ZGC_DIRECTOR_IDLE_TICK`, default ON since zt t20, under the
    /// deciding director) and only at the director stop;
    /// `Timer` has no other writer. Recorded on
    /// `zgc-zt11tg4-timer-and-launcher-what-is-left-SUPERSEDED-20261019.md`.
    ///
    /// zt t21/tg10 (`CRATONVM_ZGC_TICK_WITHDRAW_KEEPS_POLL_PROACTIVE`,
    /// default ON, `=0` kill): such a poll marks its co-claim
    /// ([`ZgcCounters::proactive_poll_co_claim`]), and the withdrawal then
    /// spends the tick's claim but leaves the bit (still returning `true`:
    /// the tick's request was withdrawn).
    pub fn withdraw_tick_cycle_on_stop(&self, cause: timer::ZTickCause) -> bool {
        let cycles = self.gc_count.load(Ordering::Relaxed);
        if !self.counters.timer.claim_still_owned(cycles) {
            return false;
        }
        if !self.tick_bit_co_claimed_by_poll(cause, cycles) {
            self.counters
                .trigger_seen
                .fetch_and(!Self::tick_cause_bit(cause), Ordering::Relaxed);
        }
        // The claim is spent: a second withdrawal finds nothing to take.
        self.counters.timer.note_claim(false, cycles);
        self.counters.timer.note_withdrawn();
        true
    }

    /// zt t11/tg4 (`t11-x8-report.md` C12 / T1): a tick collection is about
    /// to make its last stop check ([`Self::director_tick_stopped`]) and run
    /// its pause; [`Self::tick_pause_leave`] on every path out.
    pub fn tick_pause_enter(&self) {
        self.counters.timer.enter_pause();
    }

    /// zt t11/tg4: the tick collection of [`Self::tick_pause_enter`] stood
    /// down or finished its pause.
    pub fn tick_pause_leave(&self) {
        self.counters.timer.leave_pause();
    }

    /// zt t11/tg4: whether a tick collection is past its last stop check and
    /// its pause has not ended. Read after [`Self::stop_director_tick`]: the
    /// stop and the enter are both `SeqCst`, so a tick the stop did not turn
    /// back is seen here.
    pub fn tick_pause_in_flight(&self) -> bool {
        self.counters.timer.pause_in_flight()
    }

    /// zt t12/x9: [`Self::tick_pause_enter`] now and
    /// [`Self::tick_pause_leave`] when the guard drops, on every path out of
    /// the tick collection, an unwinding pause included.
    pub fn tick_pause_guard(&self) -> timer::TickPauseInFlight<'_> {
        self.counters.timer.pause_guard()
    }

    /// zt t7/tg: `(timer requests, proactive tick requests, withdrawn)`.
    pub fn director_tick_counts(&self) -> (usize, usize, usize) {
        self.counters.timer.counts()
    }

    /// zt t6/tc: 0 = ergonomic default, 1 = command line, 2 = management.
    pub fn soft_max_origin_kind(&self) -> u8 {
        self.counters.soft_max_origin.load(Ordering::Relaxed)
    }

    /// zt t1/tp: the live-bytes threshold `needs_gc`'s occupancy clause
    /// compares against: `gc_threshold` (75% of the capacity), or the soft
    /// maximum's when that is lower (`CRATONVM_ZGC_SOFT_MAX_PACES`, default ON;
    /// see [`zgc_soft_max_paces_enabled`]). One relaxed load.
    pub fn pacing_threshold(&self) -> usize {
        let soft = self.counters.soft_max_threshold.load(Ordering::Relaxed);
        self.gc_threshold.min(soft)
    }

    /// zr z1/tr: HotSpot 25 ZGC's `GCCause` for the collection about to run
    /// (`pending == true`, read from the live trigger latch: call it before
    /// the collection) or the one that just ran (`pending == false`).
    /// `None` when `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`, or when no `needs_gc`
    /// clause asked (the caller's door names the cause).
    pub fn hotspot_gc_cause(&self, pending: bool) -> Option<&'static str> {
        if !self.counters.hotspot_gc_cause.load(Ordering::Relaxed) {
            return None;
        }
        let mask = if pending {
            self.counters.trigger_seen.load(Ordering::Relaxed)
        } else {
            self.counters
                .last_cycle_trigger_mask
                .load(Ordering::Relaxed)
        };
        hotspot_cause_for_mask(mask)    }

    /// zr z1/tr: whether HotSpot's ZGC cause spellings are on
    /// (`CRATONVM_ZGC_HOTSPOT_GC_CAUSE`, default on). A door with no mask
    /// still needs to know: an allocation that failed outright is
    /// `Allocation Stall` on HotSpot's ZGC.
    pub fn hotspot_gc_causes_enabled(&self) -> bool {
        self.counters.hotspot_gc_cause.load(Ordering::Relaxed)
    }

    /// zr z2/t2, z3/t2: HotSpot 25 ZGC's cause for a collection taken through
    /// the allocation-FAILURE door, read BEFORE the collection (it reads the
    /// live trigger latch, which the collection clears). `occupancy` is the
    /// DOOR's classification (the VM's forced-door site: the compiled refill
    /// trigger, the native-boundary pressure doors, the compiled box pressure
    /// door -- each initiated by an occupancy verdict, not by a failed
    /// allocation): such a door carries the rule `needs_gc` charged
    /// (`Allocation Rate` when none is pending); every other forced door
    /// follows a failed allocation and is `Allocation Stall` even when a rule
    /// happens to be pending (on a tight heap one almost always is: the z3a
    /// host run printed zero stall causes at `-Xmx48m` while HotSpot printed
    /// 126 under the pending-latch reading this replaces). `None` under
    /// `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`; under
    /// `CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER=0` always `Allocation Stall`
    /// (the z1 reading). See [`zgc_stall_cause_from_trigger_enabled`].
    pub fn allocation_failure_gc_cause(&self, occupancy: bool) -> Option<&'static str> {
        if !self.hotspot_gc_causes_enabled() {
            return None;
        }
        if occupancy && self.occupancy_door_is_not_a_stall() {
            return Some(self.hotspot_gc_cause(true).unwrap_or("Allocation Rate"));
        }
        Some("Allocation Stall")
    }

    /// Whether a forced collection at an occupancy-classified door is NOT an
    /// allocation stall: [`Self::stall_classification_by_door`], except under
    /// `-XX:+ZCollectionIntervalOnly` with
    /// [`zgc_interval_only_occupancy_doors_stall_enabled`] (zt t20/tg9).
    /// HotSpot's director starts nothing but `Timer` cycles there, so a
    /// collection one of this VM's occupancy doors still runs (the
    /// native-boundary pressure door after a refused allocation, the refill
    /// gate's wedge breaker) can only stand for HotSpot's stall: it is
    /// labelled `Allocation Stall` and leaves the `Timer` clock alone, where it
    /// was labelled `Allocation Rate` (no rule is ever pending there) --
    /// t16 row `tg6_i1_c` (`--compatible`, whose natives allocate behind the
    /// pressure door): `causes=[Allocation Rate, Allocation Stall, Timer]`,
    /// HotSpot `rule-cause=false` (Defect D3).
    pub fn occupancy_door_is_not_a_stall(&self) -> bool {
        if !self.stall_classification_by_door() {
            return false;
        }
        !(self.counters.timer.interval_only()
            && zgc_interval_only_occupancy_doors_stall_enabled())
    }

    /// zr z3/t2: `CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER` -- whether forced
    /// collections are classified by their door (occupancy doors are neither
    /// stalls nor `Allocation Stall`), or, at `=0`, by the z1/z2 reading
    /// (every forced collection but the refill trigger's is a stall, every
    /// one's cause `Allocation Stall`).
    pub fn stall_classification_by_door(&self) -> bool {
        self.counters
            .stall_cause_from_trigger
            .load(Ordering::Relaxed)
    }

    /// zt t31/x10 (item 1, `CRATONVM_ZGC_XLOG_EDGE_BOOK`): the occupancy at
    /// the mark start of the last collection, when it closed a concurrent
    /// cycle; `None` after a stop-the-world mark (its mark started inside its
    /// own pause, at the pre-pause occupancy).
    pub fn last_mark_start_used(&self) -> Option<usize> {
        match self.counters.last_mark_start_used.load(Ordering::Relaxed) {
            usize::MAX => None,
            used => Some(used),
        }
    }

    /// zt t31/x10 (item 1): the bytes every slide of this heap has copied
    /// (cumulative; `moved_bytes=`'s running total). A collection's
    /// `Compacted` figure is the difference across it.
    pub fn slide_moved_bytes_total(&self) -> usize {
        self.counters.slide_moved_bytes[1].load(Ordering::Relaxed)
    }

    /// zt t31/x10 (item 5, `CRATONVM_ZGC_STALL_MARK_REACHES_JOINED_PAUSE`):
    /// an allocation stall waits for a collection; the next cycle's end takes
    /// the mark whichever thread runs it (see the `stall_cycle_pending`
    /// counter). Called by the stalled thread before it asks for its pause.
    pub fn note_stall_cycle_pending(&self) {
        if zgc_stall_mark_reaches_joined_pause_enabled() {
            self.counters
                .stall_cycle_pending
                .store(true, Ordering::Release);
        }
    }

    /// Test lever for [`Self::stall_classification_by_door`] (the `=0` arm
    /// without the process environment).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn counters_for_test_set_stall_classification(&self, on: bool) {
        self.counters
            .stall_cause_from_trigger
            .store(on, Ordering::Relaxed);
    }

    /// zr z2/t2: one mutator allocation stall of `nanos` -- a mutator whose
    /// allocation failed and that waited for (or ran) the collection that
    /// serves it. Feeds [`metrics::ZgcMetrics::record_allocation_stall`]
    /// (`CRATONVM_ZGC_METRICS=1` summary, the TSV stall columns), which had no
    /// production caller until this wave
    /// (`zgc-zr1tr-allocation-stalls-are-not-recorded-FIXED-20261012.md`). Atomics
    /// only, no lock.
    pub fn note_allocation_stall(&self, nanos: u64) {
        self.counters.metrics.record_allocation_stall(nanos);
    }

    /// zr z2/t2: `(young, old)` bytes for the JMX pools: allocated since the
    /// last collection, and the post-collection live figure (see
    /// [`zgc_young_pool_since_cycle_enabled`]). `None` under
    /// `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE=0` (the caller then reports the
    /// whole heap in the old pool). Two relaxed loads; the watermark is
    /// clamped to `allocated` (a VM-TLAB tail reclaimed after the reset can
    /// leave `allocated` a little below it).
    pub fn jmx_young_old_split(&self) -> Option<(usize, usize)> {
        if !self.counters.young_pool_since_cycle.load(Ordering::Relaxed) {
            return None;
        }
        let allocated = self.allocated.load(Ordering::Relaxed);
        let mut old = self
            .cycle_alloc_watermark
            .load(Ordering::Relaxed)
            .min(allocated);
        // zt t6/tc (GN-11, `CRATONVM_ZGC_GEN_YOUNG_POOL_LIVE`, default ON
        // inside the generational mode, `=0` kill): on a generational heap
        // with an old generation the post-collection live figure holds the
        // YOUNG survivors too, which HotSpot's `ZGC Young Generation` pool
        // keeps after a minor. They are the live figure minus the old
        // generation's two parts (the last major's old live set and the
        // promotions since), so they move from the old figure to the young.
        //
        // zt t21/pl10 (item 11's second residual, `CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG`,
        // default ON in the mode, `=0` kill): after a WHOLE-HEAP cycle that is
        // not a `System.gc()` (a `Warmup`, `Proactive` or rule major) the old
        // live figure counts every survivor below the new floor, the
        // young-by-age ones included, so `ZGC Young Generation` read ~0.
        // HotSpot 25's major runs a young collection that tenures by age
        // (`major_full_roots` / `major_partial_roots`; only the `System.gc()`
        // pre-clean promotes all), so its young pool keeps them. Leave out the
        // bytes that major's sweep left young (`gen_last_major_young_bytes`,
        // 0 after a young cycle). This also holds before anything was promoted
        // (`has_old_objects` false), when every survivor is young by age.
        let major_young = if self.generational_enabled()
            && zgc_flag_default("CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG", true)
        {
            self.counters
                .gen_last_major_young_bytes
                .load(Ordering::Relaxed)
        } else {
            0
        };
        if self.generational_enabled()
            && (self.has_old_objects.load(Ordering::Relaxed) || major_young != 0)
            && zgc_flag_default("CRATONVM_ZGC_GEN_YOUNG_POOL_LIVE", true)
        {
            let old_gen = self
                .gen_old_live_bytes
                .load(Ordering::Relaxed)
                .saturating_add(self.counters.gen_nursery_old_bytes.load(Ordering::Relaxed))
                .saturating_sub(major_young);
            // zt t16/gn6 (item 11, `CRATONVM_ZGC_GEN_JMX_YOUNG_BELOW_FLOOR`;
            // zt t26/rt2: default ON in the mode, `=0` kill, as
            // `CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG` already is for a major): after
            // a young cycle on the young-aged sweep the young
            // survivors BELOW the floor are still inside `gen_old_live_bytes`
            // (the major that left them there counted every survivor under its
            // cursor old), so they read as `ZGC Old Generation`; HotSpot's
            // minor keeps them in `ZGC Young Generation`. Subtract the figure
            // that cycle measured (`gen_last_young_below_young_bytes`, the
            // `-Xlog` young split's correction) only while the LAST cycle was
            // that young cycle: a major lowers the bit, and its own live figure
            // is the old one again.
            let old_gen = if self
                .counters
                .gen_last_cycle_young_aged
                .load(Ordering::Relaxed)
                && zgc_flag_default("CRATONVM_ZGC_GEN_JMX_YOUNG_BELOW_FLOOR", true)
            {
                old_gen.saturating_sub(
                    self.counters
                        .gen_last_young_below_young_bytes
                        .load(Ordering::Relaxed),
                )
            } else {
                old_gen
            };
            old = old.min(old_gen);
        }
        Some((allocated - old, old))
    }

    /// zt t10/gn2: `(old generation bytes, bytes promoted)` after the last
    /// young cycle of a generational heap, for the `-Xlog` `y:` `Young
    /// Generation Statistics` table: the old generation is the last major's
    /// old live set plus every promotion since (the same two parts
    /// [`Self::jmx_young_old_split`] subtracts), the promotion figure is that
    /// young cycle's alone. `None` off the generational mode. Read by the VM
    /// right after the collection it describes, inside the same collection
    /// event (no other collection runs in between). Two relaxed loads.
    ///
    /// zt t11/gn3 (x8 C8, hand-off G1): the old figure leaves out the young
    /// survivors below the floor that `gen_old_live_bytes` still holds
    /// (`gen_last_young_below_young_bytes`), and the split is `None` after a
    /// young cycle that did not take the young-aged sweep (its old figure
    /// misses that cycle's promotions, so `Live` counted them twice); the
    /// caller then prints the whole-heap table, the pre-t10 shape.
    /// `CRATONVM_ZGC_XLOG_GEN_YOUNG_FIGURES=0` (the caller's switch) still
    /// restores that shape everywhere.
    pub fn gen_last_young_xlog_figures(&self) -> Option<(usize, usize)> {
        if !self.generational_enabled()
            || !self
                .counters
                .gen_last_young_xlog_exact
                .load(Ordering::Relaxed)
        {
            return None;
        }
        let old_gen = self
            .gen_old_live_bytes
            .load(Ordering::Relaxed)
            .saturating_add(self.counters.gen_nursery_old_bytes.load(Ordering::Relaxed))
            .saturating_sub(
                self.counters
                    .gen_last_young_below_young_bytes
                    .load(Ordering::Relaxed),
            );
        let promoted = self
            .counters
            .gen_last_young_promoted_bytes
            .load(Ordering::Relaxed);
        Some((old_gen, promoted))
    }

    /// zt t16/gn6 (item 12): did the last collection begin with an
    /// allocation failure latched (`hard_alloc_failure`, which forces it
    /// whole-heap)? HotSpot 25's `should_preclean_young` pre-cleans the young
    /// generation of a major whose threads are stalled for old
    /// (`is_alloc_stalling_for_old`) whatever its cause; the `-Xlog` logger
    /// reads this as that clause under `CRATONVM_ZGC_XLOG_STALL_PRECLEAN=1`.
    /// Read right after the collection it describes. One relaxed load.
    pub fn last_collection_after_alloc_failure(&self) -> bool {
        self.counters
            .gen_last_major_alloc_stalled
            .load(Ordering::Relaxed)
    }

    /// zr z1/tr: collections a [`director`] rule admitted.
    pub fn trigger_director_count(&self) -> usize {
        self.counters.trigger_director.load(Ordering::Relaxed)
    }

    /// zr zs s1/tp: intervals in which a director rule's yes was held back by
    /// the anti-storm floor (`CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`).
    pub fn trigger_director_deferred_count(&self) -> usize {
        self.counters
            .trigger_director_deferred
            .load(Ordering::Relaxed)
    }

    /// zr zs s1/tp: the warm-up clause's step count -- warm-up CYCLES, as
    /// HotSpot's `rule_warmup` counts them (`ZStatCycle`'s warm-up count), with
    /// `CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS` (default on); every collection
    /// (`gc_count`) at `=0`. HotSpot 25.0.3 (laptop, `Zs1TpWarmupSteps sysgc
    /// -Xmx512m`): `System.gc()` first, then `Warmup` at 10, 20 and 30%; this
    /// collector counted the `System.gc()` as the first step, so its warm-up
    /// cycles came at 20 and 30% and there were two of them.
    pub fn warmup_cycles_done(&self) -> usize {
        if self.counters.warmup_hotspot_steps.load(Ordering::Relaxed) {
            self.counters.warmup_cycles_done.load(Ordering::Relaxed)
        } else {
            self.gc_count.load(Ordering::Relaxed)
        }
    }

    /// zr zs s1/tp: the warm-up clause's progress floor: `gc_rearm`, or with
    /// `CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS` (default on) at most half a
    /// warm-up step past the last collection's live set
    /// ([`director::warmup_floor`]). Shared by `needs_gc` and the
    /// concurrent-start window so the two cannot disagree on the point.
    pub fn warmup_rearm_floor(&self) -> usize {
        director::warmup_floor(
            self.cycle_alloc_watermark.load(Ordering::Relaxed),
            self.gc_rearm.load(Ordering::Relaxed),
            self.soft_max_capacity() / 10,
            self.counters.warmup_hotspot_steps.load(Ordering::Relaxed),
        )
    }

    /// Install the VM's heap-exhaustion unwind (zr z2/cp). First call wins;
    /// a VM installs it once, right after building its heap. See
    /// [`ZgcCounters::exhaustion_unwind`] for the contract the hook must keep.
    pub fn set_exhaustion_unwind(&self, hook: fn(usize, &'static str)) {
        let _ = self.counters.exhaustion_unwind.set(hook);
    }

    /// How many times an infallible arm offered the exhaustion unwind. A
    /// non-zero count on a run that still aborted means the hook declined
    /// (no converting handler was up the stack).
    pub fn exhaustion_unwind_offers(&self) -> usize {
        self.counters
            .exhaustion_unwind_offers
            .load(Ordering::Relaxed)
    }

    /// zs s6/no: this heap's key in [`ZGC_EXHAUSTED_VERDICT`].
    ///
    /// zs s9/no2 (rv6 X1, rv5 residual 3): a serial taken at construction
    /// ([`ZGC_HEAP_SERIAL`], never 0), no longer the heap's address. A
    /// thread-local verdict armed on a heap that was then dropped could match
    /// a new heap allocated at the same address (in a test, or an embedder
    /// that builds VMs one after another) at a `gc_count` that happened to
    /// match. The verdict's other per-heap state (`exhausted_spent`) lives on
    /// the heap's own counters, so the serial covers both.
    pub fn exhausted_verdict_key(&self) -> usize {
        self.counters.exhausted_verdict_key
    }

    /// zs s6/no: the exhausted verdict (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`,
    /// opt-in). Called by the VM at the end of an allocation REFUSAL's
    /// collection (the ladder doors' forced collection and the clear-soft
    /// rung's). When the last collection left under the floor free
    /// ([`director::exhausted_verdict`]), this thread's fallible attempts on
    /// this heap are refused until the next collection or until the ladder's
    /// `OutOfMemoryError` is built ([`Self::lift_exhausted_verdict`]), so the
    /// ladder goes on to its clear-soft rung and then throws: HotSpot's stall,
    /// which fails after a collection that left no whole page free, instead
    /// of one whole-heap collection per sliver of free space down to the last
    /// object. Returns whether the verdict holds. Off (`floor == 0`): one
    /// relaxed load, `false`.
    pub fn arm_exhausted_verdict(&self) -> bool {
        let floor = self.counters.exhausted_free_floor.load(Ordering::Relaxed);
        if floor == 0 {
            return false;
        }
        let live = self.cycle_alloc_watermark.load(Ordering::Relaxed);
        if !director::exhausted_verdict(self.heap_capacity(), live, floor) {
            // The heap recovered (a collection left the floor free): the
            // next fill may end in a verdict again.
            self.counters
                .exhausted_spent
                .store(false, Ordering::Relaxed);
            self.lift_exhausted_verdict();
            return false;
        }
        // zs s9/no2: ONE verdict per fill. Once a verdict has ended the fill
        // in an `OutOfMemoryError`, the rest of it runs the default ladder.
        // Without this the verdict re-armed at every later refusal of a
        // program that keeps allocating after its OOME (a fill that halves
        // its request on each error, `Zs1NdLauncherOome`'s `<clinit>`), and
        // each re-arm refused the retry that would have eaten the garbage the
        // refusal collection had just freed (the failed iteration's node, the
        // previous error). The fill then ended with free holes the default
        // arm consumes, and the launcher's receiver was served from them:
        // s6a L4 `Zs1NdLauncherOome -Xmx32m` exact HotSpot stderr 20/20 on
        // the default arm, 0/20 under the verdict. HotSpot's page model never
        // reuses such holes; the default ladder reaches the same answer by
        // eating them down to object granularity. The tail cut is kept: it
        // comes from the FIRST verdict, which ends the fill's crawl.
        if self.counters.exhausted_spent.load(Ordering::Relaxed) {
            return false;
        }
        let epoch = self.gc_count.load(Ordering::Relaxed);
        let key = self.exhausted_verdict_key();
        ZGC_EXHAUSTED_VERDICT.with(|v| v.set((key, epoch)));
        self.counters
            .exhausted_verdicts_armed
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    /// zs s9/no2: the free space the last collection left (`capacity -` the
    /// live set it published), the input of the near-full soft clear
    /// (`CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`). Before the first
    /// collection: the whole capacity minus nothing (`0` live).
    pub fn free_after_last_cycle(&self) -> usize {
        self.heap_capacity()
            .saturating_sub(self.cycle_alloc_watermark.load(Ordering::Relaxed))
    }

    /// zs s8/rv5: is the exhausted verdict switched on
    /// (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR` set to a floor)? One relaxed load;
    /// lets a caller skip its own pre-checks when the verdict is off.
    pub fn exhausted_verdict_enabled(&self) -> bool {
        self.counters.exhausted_free_floor.load(Ordering::Relaxed) != 0
    }

    /// zs s6/no: drop this thread's exhausted verdict on this heap (the
    /// ladder's `OutOfMemoryError` is being built: its throwable, message and
    /// stack trace allocate as they always did). A no-op when none is held.
    ///
    /// zs s9/no2: a verdict still live when it is lifted (armed since the last
    /// collection: the ladder it refused is throwing) is SPENT for this fill;
    /// see the note in [`Self::arm_exhausted_verdict`].
    pub fn lift_exhausted_verdict(&self) {
        let key = self.exhausted_verdict_key();
        let epoch = self.gc_count.load(Ordering::Relaxed);
        let live = ZGC_EXHAUSTED_VERDICT.with(|v| {
            let (k, e) = v.get();
            if k == key {
                v.set((0, 0));
            }
            k == key && e == epoch
        });
        if live && !self.counters.exhausted_spent.swap(true, Ordering::Relaxed) {
            self.counters
                .exhausted_spent_count
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// zt t2/rv (`zgc-zs10rv7-wave-s9-review-residuals-CLOSED-20261019.md` item 4):
    /// [`Self::lift_exhausted_verdict`] for a VM-raised `OutOfMemoryError`
    /// that is NOT the heap refusal (`Requested array size exceeds VM limit`,
    /// `Metaspace`, ...). The verdict is dropped, so the throwable allocates as
    /// it always did, but not SPENT: that error says nothing about the free
    /// floor, and a spent mark is per heap (it blocks every thread's verdict
    /// until a collection leaves the floor free).
    pub fn drop_exhausted_verdict(&self) {
        let key = self.exhausted_verdict_key();
        ZGC_EXHAUSTED_VERDICT.with(|v| {
            if v.get().0 == key {
                v.set((0, 0));
            }
        });
    }

    /// zs s9/no2: verdicts that ended a fill (spent; `exhausted_spent=`).
    pub fn exhausted_verdict_spent_count(&self) -> usize {
        self.counters.exhausted_spent_count.load(Ordering::Relaxed)
    }

    /// zs s6/no: does this thread hold a live exhausted verdict on this heap
    /// (armed since the last collection)? Read by the fallible entries
    /// (`try_alloc_object` / `try_alloc_array`) only; one relaxed load when
    /// the switch is off.
    #[inline]
    pub fn exhausted_verdict_refuses(&self) -> bool {
        if self.counters.exhausted_free_floor.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let (key, epoch) = ZGC_EXHAUSTED_VERDICT.with(|v| v.get());
        if key != self.exhausted_verdict_key() || epoch != self.gc_count.load(Ordering::Relaxed) {
            return false;
        }
        self.counters
            .exhausted_verdict_refusals
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    /// zt t2/al (`zgc-zt1sp-proposal-verdict-refusals-record-the-request-20261019.md`):
    /// the exhausted verdict just refused a request of `size` bytes (header
    /// plus body, as `alloc_raw` is asked for it; `None` when the size does
    /// not compute, which `alloc_raw` would never have been asked either).
    /// Record it in the zr z5/cr refusal record (the two fields `alloc_raw`'s
    /// failure arm writes; zt t3/x2: NOT that arm's collection latches or its
    /// compaction target, which the verdict's ladder does not need), so the readers of "the request refused since the
    /// latest collection" -- the final-refusal rule (`note_final_refusal`),
    /// the retry-room check (`room_for_last_refusal`) and the stall event's
    /// request size (`jfr_stall_request_bytes`) -- see the verdict's refusal
    /// rather than an older `alloc_raw` one. Before this, a ladder whose last
    /// refusal was the verdict's left `note_final_refusal` reading a refusal
    /// one collection old, so the default on final-refusal rule never armed on the
    /// fills the verdict ends (t1/sp finding 9). Reached only while the default on
    /// verdict (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`) refuses, so the default
    /// configuration never runs it; `CRATONVM_ZGC_VERDICT_RECORDS_REFUSAL=0`
    /// is the A/B arm. Two relaxed stores on a refusal path.
    #[cold]
    pub fn note_verdict_refusal(&self, size: Option<usize>) {
        let Some(size) = size else {
            return;
        };
        if !self.counters.verdict_records_refusal.load(Ordering::Relaxed) {
            return;
        }
        self.counters
            .last_refusal_size
            .store(size, Ordering::Relaxed);
        self.counters
            .last_refusal_epoch
            .store(self.gc_count.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// zs s6/no: `(exhausted_armed, exhausted_refused)` for the
    /// `[GC] zgc-refusal:` line.
    pub fn exhausted_verdict_stats(&self) -> (usize, usize) {
        (
            self.counters
                .exhausted_verdicts_armed
                .load(Ordering::Relaxed),
            self.counters
                .exhausted_verdict_refusals
                .load(Ordering::Relaxed),
        )
    }

    /// Registered objects this heap's sweep refused to size (and therefore
    /// refused to reclaim). See [`Self::alloc_size`]. Non-zero means a class
    /// was unloaded while an instance was still registered, or a header is
    /// corrupt; both leak the object rather than corrupt the arena. zr z3/lt:
    /// per heap (was the process static `ZGC_UNSIZABLE_OBJECTS`).
    pub fn unsizable_registered_objects(&self) -> usize {
        self.counters
            .local
            .unsizable_objects
            .load(Ordering::Relaxed)
    }

    /// Pages admitted to a relocation set because they fell in the window an
    /// allocation failure named -- the ENGAGEMENT counter for targeted
    /// compaction ("the OOM went away" and "no target was ever set" otherwise
    /// produce the same `compaction_cycles`). zr z3/lt: per heap (was the
    /// process static `TARGETED_PAGES_SELECTED` of `forwarding.rs`).
    pub fn targeted_pages_selected(&self) -> usize {
        self.counters
            .local
            .targeted_pages_selected
            .load(Ordering::Relaxed)
    }

    /// Claim this heap's end-of-run metrics row: `true` exactly once per heap
    /// (zr z3/lt), or once per process under
    /// `CRATONVM_ZGC_METRICS_ROW_PER_HEAP=0` (the pre-z3 claim).
    pub fn claim_end_of_run_metrics(&self) -> bool {
        if heap_state::zgc_metrics_row_per_heap() {
            !self
                .counters
                .local
                .end_of_run_metrics_emitted
                .swap(true, Ordering::AcqRel)
        } else {
            !heap_state::PROCESS_METRICS_ROW_EMITTED.swap(true, Ordering::AcqRel)
        }
    }

    /// This heap's `CRATONVM_ZGC_PAR_RELOCATE_BALANCE` seed, for
    /// [`zgc_par_chunks`] (zr z3/lt), and (zs s3/ac) the sink every slide /
    /// rewrite / census pass adds its spawned pullers' thread CPU to
    /// (`relocate_pool_ms=` on `[GC] zgc-cpu:`), `None` under
    /// `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0`. Every pass names it, so no pass
    /// can be left out of the accounting by a call site.
    #[inline]
    pub fn par_bal(&self) -> ZParPass<'_> {
        ZParPass {
            balance: self.counters.local.par_relocate_balance(),
            cpu: self
                .collection_cpu_accounting_on()
                .then_some(&self.counters.relocate_pool_cpu_ns),
        }
    }

    /// The infallible arms' last step before `abort()`: hand the VM a chance
    /// to raise a catchable `OutOfMemoryError` instead. `length` is the
    /// element count for an array and the field count for an object, which is
    /// what the VM's `NativeAllocOom` message wants; `what` is `"object"` or
    /// `"array"`. Returns only if the hook declined or none is installed.
    ///
    /// Called with no lock held: `alloc_raw` has released the arena and the
    /// TLAB cell before it answers `None`, so an unwind from here leaves the
    /// heap as consistent as the refused attempt left it.
    pub fn offer_exhaustion_unwind(&self, length: usize, what: &'static str) {
        if !zgc_infallible_oome_unwind_enabled() {
            return;
        }
        if let Some(hook) = self.counters.exhaustion_unwind.get() {
            self.counters
                .exhaustion_unwind_offers
                .fetch_add(1, Ordering::Relaxed);
            hook(length, what);
        }
    }

    /// **The verdict line printed immediately before
    /// `std::process::abort()`.** Says which of three different failures just
    /// killed the process, with the numbers that separate them.
    ///
    /// # Why this exists rather than the one-line `FATAL:` it follows
    ///
    /// `GarbageCollector::alloc_object` / `alloc_array` cannot report failure
    /// (the trait returns `ObjectRef` and says nothing about exhaustion), so
    /// the terminal answer on this backend is an abort. Until now the whole
    /// record of that event was `FATAL: ZGC(real): out of heap space for
    /// object (N bytes)` — which cannot distinguish:
    ///
    /// 1. **a genuinely full heap.** `used ≈ capacity`, small free list, and a
    ///    collection ran recently and freed nothing. `OutOfMemoryError` is the
    ///    correct answer and the abort is only the wrong *channel*.
    /// 2. **a fragmented heap.** The bytes exist, no hole is big enough.
    ///    `largest_free_block < size` with a large `free_list_bytes` says so.
    ///    `warn_alloc_failed_once` and `ZFragReport` already cover this one and
    ///    have fired by the time this runs.
    /// 3. **a heap that was never collected** — `gc_count` low or zero,
    ///    `allocated` far below `gc_rearm`, `headroom_low` true. This is the
    ///    `gap-e-headroom-trigger-below-rearm` shape: the trigger that exists
    ///    for "the arena cannot serve a request" is gated behind a floor
    ///    expressed in live bytes, and on a non-compacting heap the two can
    ///    diverge. **Unreproduced as of 2026-09-21** — see
    ///    `gap-p-headroom-flip-criteria.md` §0, where every exhaustion shape
    ///    the orchestrator constructed threw `OutOfMemoryError` at near-full
    ///    occupancy instead. This arm exists so that if it ever *does* happen
    ///    the crash says so in its own words rather than being re-derived from
    ///    a byte count; do not read its presence here as evidence that it
    ///    does.
    ///
    /// Those are three different bugs with three different owners, and for a
    /// crash that by construction leaves no other artefact — no unwind, no
    /// `OutOfMemoryError`, no stack — the one line it does leave has to carry
    /// enough to tell them apart. It costs one arena lock on a path that is
    /// about to `abort()`.
    ///
    /// **This does not make the abort correct.** It makes it diagnosable. The
    /// fix is for this entry point to be able to fail; see
    /// `docs/feature-designs/zgc-architecture.md`.
    #[cold]
    #[inline(never)]
    pub fn report_fatal_exhaustion(&self, what: &str, size: usize) {
        let (used, capacity, free_list, largest, spans) = {
            let arena = self.arena.lock();
            (
                arena.used(),
                arena.capacity(),
                arena.free_list_bytes(),
                arena.largest_free_block(),
                arena.free_span_shape(),
            )
        };
        let allocated = self.allocated.load(Ordering::Relaxed);
        let rearm = self.gc_rearm.load(Ordering::Relaxed);
        let (headroom_low, bypass_armed, reclaimed_nothing) = self.headroom_trigger_state();
        let (starved, starved_gap, bypass_fired, bypass_suppressed) = self.headroom_flip_evidence();
        let (stress, threshold, headroom, budget, hard) = self.trigger_tallies();
        let gc_count = self.gc_count.load(Ordering::Relaxed);
        // The verdict FIRST, so it survives a truncated crash log. Only the
        // third arm is a claim about a defect; the other two name the
        // condition and leave the judgement to the reader.
        let verdict = if headroom_low && allocated < rearm {
            "TRIGGER BELOW ITS FLOOR -- the arena went short of contiguous space \
             (headroom_low=true) while live bytes sat under gc_rearm, so needs_gc \
             refused every poll. Compare `allocated` against `arena used` on the \
             next line: far apart means this heap died with room in it. This \
             shape was NOT reproducible on 2026-09-21, so a real sighting is \
             news -- see gap-p-headroom-flip-criteria.md and \
             CRATONVM_ZGC_HEADROOM_BYPASSES_REARM"
        } else if largest < size && free_list > size.saturating_mul(4) {
            "FRAGMENTED -- the bytes are present and no single hole is large enough"
        } else {
            "EXHAUSTED -- used is at capacity with no free list to speak of"
        };
        eprintln!(
            "FATAL: ZGC(real): {what} allocation of {size} bytes cannot be served, and this \
             entry point cannot report failure. VERDICT: {verdict}.\n\
             FATAL: ZGC(real):   arena used={used}/{capacity} free_list_bytes={free_list} \
             largest_free_block={largest} free_spans={spans:?}\n\
             FATAL: ZGC(real):   collections={gc_count} allocated={allocated} gc_rearm={rearm} \
             gc_threshold={} headroom_low={headroom_low} bypass_armed={bypass_armed} \
             last_cycle_reclaimed_nothing={reclaimed_nothing}\n\
             FATAL: ZGC(real):   triggers stress={stress} threshold={threshold} \
             headroom={headroom} alloc_budget={budget} hard_alloc_refusals={hard}\n\
             FATAL: ZGC(real):   headroom starved_intervals={starved} \
             starved_max_gap_bytes={starved_gap} bypass_fired={bypass_fired} \
             bypass_suppressed={bypass_suppressed}",
            self.gc_threshold,
        );
        // zs s2/nd2: name the caller, as G1's fatal arm does (gc-common w2-d).
        // Every VM allocation ladder uses the fallible entry points, so this
        // arm means an infallible caller that cannot report failure; the
        // host's `nd_L1s_co` abort carried no stack (the pc was libc `abort`).
        // Unconditional: the process is about to abort.
        eprintln!(
            "FATAL: ZGC(real) infallible {what} caller:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }

    /// Walk every live object and its allocation size.
    ///
    /// Retires every TLAB first — this is a heap-walking entry point and the
    /// invariant on [`Self::retire_all_tlabs`] applies to it. Idempotent, so
    /// calling it from inside a collection that has already retired costs a
    /// pass over an empty set.
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        self.retire_all_tlabs();
        self.registry
            .bases()
            .into_iter()
            .filter_map(|base| {
                let header = unsafe { &*(base as *const ObjectHeader) };
                // Drop rather than report a sentinel extent: this feeds heap
                // dumps and referrer queries, and `(ptr, 1 TiB)` is worse for
                // every one of them than an omitted row.
                //
                // zs s2/rv: a real array past `MAX_PLAUSIBLE_ARRAY_LEN` is a
                // row too (it was omitted: dg residual row 4).
                self.live_extent(base, header)
                    .map(|size| (base as *mut u8, size))
            })
            .collect()
    }

    /// Header accessor (shared with the trait impl).
    #[inline]
    pub fn header(&self, obj: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj` points to a live allocation whose first HEADER_SIZE
        // bytes are a valid `ObjectHeader` written at allocation time.
        unsafe { &*(obj.as_ptr() as *const ObjectHeader) }
    }

    #[inline]
    pub fn header_mut(&self, base: *mut u8) -> &mut ObjectHeader {
        // SAFETY: `base` is a registered live allocation base; its first
        // HEADER_SIZE bytes are a valid `ObjectHeader`.
        unsafe { &mut *(base as *mut ObjectHeader) }
    }

    /// Shared (`&`) header view — the concurrent twin of [`Self::header_mut`].
    ///
    /// Every mutating operation an [`ObjectHeader`] offers goes through the
    /// atomic mark word, so a `&ObjectHeader` is enough to claim a mark bit.
    /// [`Self::header_mut`] is nonetheless unusable from the concurrent marker:
    /// N workers each minting a `&mut ObjectHeader` for the same object are
    /// aliasing `&mut`s, which is undefined behaviour regardless of what the
    /// fields underneath happen to be — the compiler is entitled to assume the
    /// reference is unique. Every method on [`mark::ZMarkContext`] below uses
    /// this accessor, and none uses `header_mut`.
    #[inline]
    pub fn header_ref(&self, base: *mut u8) -> &ObjectHeader {
        // SAFETY: `base` is a registered live allocation base (the caller has
        // passed it through the registry gate); its first HEADER_SIZE bytes
        // are a valid `ObjectHeader` written at allocation time, and this
        // reference is shared, so concurrent readers are sound.
        unsafe { &*(base as *const ObjectHeader) }
    }

    /// Largest body this collector will believe. Mirrors the `1 << 24` slot cap
    /// `gen_object_total_size` (`gen_heap.rs`) and `object_body_size`'s own
    /// legacy screen already apply — "no real class has 16M fields" — expressed
    /// in bytes so it also bounds a compact body.
    ///
    /// It exists to reject [`cratonvm_types::object_body_size`]'s corrupt-header
    /// sentinel, which is `1 << 40` (1 TiB). That value is deliberately
    /// "impossibly large" rather than `0` so that a caller deriving an extent
    /// from it fails its own arena bounds check and re-syncs — see the constant's
    /// doc in `types/src/field_layout.rs`. This collector had no such check.
    pub const MAX_PLAUSIBLE_BODY: usize = (1usize << 24) * SLOT_SIZE;

    /// The array counterpart of [`Self::MAX_PLAUSIBLE_BODY`], in ELEMENTS.
    ///
    /// `1 << 28` is ~268M elements — larger than any array this VM can hold in
    /// the heaps it runs (a reference array that long is a gigabyte before the
    /// objects it points at), and small enough that a header decoded out of
    /// payload bytes almost always exceeds it. Deliberately generous: this is
    /// a corruption screen, not a policy limit, and refusing a real array would
    /// break a working program in order to catch a broken one.
    ///
    /// # It is BELOW the allocator's own limit, and that gap is reachable
    ///
    /// [`ZGC_REAL_MAX_ARRAY_LENGTH`] is `i32::MAX` (~2.1G elements), so a
    /// reference array between `1 << 28` and that IS allocatable on a large
    /// heap and IS refused here. "larger than any array this VM can hold in the
    /// heaps it runs" is a statement about the heaps it is usually run in, not a
    /// bound anything enforces — a 268M-element `Object[]` is 2.1 GB of payload,
    /// which a 16 GB ergonomic default heap admits.
    ///
    /// What that costs, per consumer of [`ZgcRealHeap::alloc_size`]:
    ///
    /// * **The marker: nothing, now.** `visit_strong_refs_at` deliberately does
    ///   NOT screen array length, because refusing there means an out-edge not
    ///   traced — a live object collected. `enumerate_references` briefly did
    ///   (2026-08-26, same day, caught before it landed on dev) and that was the
    ///   bug this paragraph exists to prevent recurring.
    /// * **The slide: conservative but noisy.** An unsizable object is skipped
    ///   from relocation and logged at `error`, so such an array is permanently
    ///   un-compactable and emits guard spam rather than corrupting anything.
    /// * **`live_bytes` accounting: under-counts** by the array's whole size,
    ///   which makes its page look emptier than it is to the relocation
    ///   selector.
    ///
    /// Neither sibling collector has this gap: `gen_heap`'s and `g1`'s
    /// plausibility screens are both written `if !is_array && num_slots > ...`,
    /// i.e. they cap OBJECT field counts and deliberately leave array length
    /// alone. ZGC is the only one that caps a length, and therefore the only one
    /// where the two constants can disagree.
    ///
    /// Raising this to `ZGC_REAL_MAX_ARRAY_LENGTH` would close the gap and
    /// weaken the screen to nothing (every `u32` length passes), so it is not
    /// the obvious fix; bounding by the ARENA's extent rather than by a constant
    /// probably is. Not attempted here — it wants a fixture that can allocate a
    /// multi-gigabyte array, which this suite has no way to run.
    pub const MAX_PLAUSIBLE_ARRAY_LEN: usize = 1usize << 28;

    /// The largest extent [`Self::alloc_size`] can report: an object total is
    /// capped at [`Self::MAX_PLAUSIBLE_BODY`], an array at `ARRAY_DATA_OFFSET`
    /// plus a payload capped at the same. So a base more than this far below
    /// an address can neither contain it nor end at it, and the interior
    /// probes (`resolve_interior_for_pin`, `is_heap_addr`) stop their
    /// backwards scan there without changing an answer (nio20 wave 6, L2).
    /// Pinned against `alloc_size` by
    /// `max_alloc_extent_bounds_every_size_alloc_size_reports`.
    ///
    /// # Since zr z1/rl it bounds every MOVABLE extent, not every extent
    ///
    /// `alloc_size` now sizes a real array past 256 MiB of payload (see
    /// `zgc_large_array_sizing_enabled`), so an extent larger than this exists.
    /// The interior probes keep this window anyway -- widening it to 2 GiB would
    /// cost the 22 % `R17NioRows` profile item back 16-fold -- and the slide
    /// keeps the answer they can miss irrelevant instead: an object whose extent
    /// exceeds this window is never moved by either end's slide
    /// (`relocate_stw_admitted`, `compact_high_region`), so a derived pointer
    /// into its far interior that `resolve_interior_for_pin` cannot name pins
    /// nothing that needed pinning. HotSpot's ZGC never relocates its large
    /// pages at all; this is the narrower version of the same rule.
    ///
    /// *zr z2/rv:* a missed PIN is harmless that way, a missed MARK is not --
    /// the take-over pass roots derived pointers through the same probes, and
    /// such an array is now freed when the mark misses it. So every allocation
    /// past this window is recorded in the huge-extent table and the probes
    /// consult it on a window miss ([`Self::huge_extent_containing`]).
    pub const MAX_ALLOC_EXTENT: usize = Self::MAX_PLAUSIBLE_BODY + ARRAY_DATA_OFFSET;

    /// Total size in bytes of the allocation rooted at `header`, or `None` when
    /// the header cannot be sized.
    ///
    /// # Why this is fallible
    ///
    /// [`cratonvm_types::object_body_size`] answers `IMPLAUSIBLE_BODY_SIZE`
    /// (1 TiB) — not `0` — for two states: a `GC_FLAG_COMPACT` object whose
    /// `(class_id, num_slots)` no longer resolves to a registered layout (a
    /// class unloaded by `ClassStore::remove` -> `unregister_class_layout`
    /// while an instance is still in this heap's registry), and a legacy header
    /// whose `num_slots` is past the plausibility cap (a desynced walk, a
    /// dangling base). The sentinel is a REFUSAL, and every sibling collector
    /// treats it as one: `gen_heap`'s non-moving sweep stops the walk on
    /// `cursor + total > used`, `g1` refuses to evacuate a sub-header object,
    /// and `gen_object_total_size` returns `0` rather than passing the sentinel
    /// on.
    ///
    /// This function used to return it verbatim, and
    /// [`Self::collect_garbage`]'s sweep fed it straight to
    /// `std::ptr::write_bytes(base, 0, size)` and `Arena::add_free_block`
    /// (whose only bound is a `debug_assert!`, compiled out in release). One
    /// unresolvable compact object therefore memset a terabyte from its base
    /// and handed the allocator a free block far outside the arena. Five other
    /// sites in this same file already guard the identical "compact-flagged,
    /// layout does not resolve" state (`get_field`, `set_field`,
    /// `visit_strong_refs_at`, `census::reference_slots`,
    /// `effectively_compact_header`); the one that decides how many bytes to
    /// ZERO was the only consumer without a screen.
    ///
    /// The array arm is fallible for the matching reason: `array_data_size`
    /// answers `None` on an overflowing length, and the previous
    /// `.unwrap_or(0)` under-reported the extent — the direction that leaves
    /// live bytes on the free list.
    pub fn alloc_size(header: &ObjectHeader) -> Option<usize> {
        match header.kind() {
            ObjectKind::Object | ObjectKind::HumongousFiller => {
                let total = cratonvm_types::object_instance_size(header);
                if total > Self::MAX_PLAUSIBLE_BODY {
                    return None;
                }
                Some(total)
            }
            ObjectKind::Array => {
                // The SAME plausibility bound the object arm applies, and it
                // was missing here. `array_data_size` refuses only integer
                // OVERFLOW, so a clobbered `array_length` of a few hundred
                // million with 4-byte elements sizes cleanly to a couple of
                // gigabytes — and every caller then trusts it, including
                // `reference_slots`, which strides the whole thing and walks
                // out of the arena into an unmapped page.
                //
                // Bounded in ELEMENTS as well as bytes because elements is the
                // number the header actually carries, and so the one a desynced
                // walk corrupts.
                let len = header.array_length() as usize;
                if len > Self::MAX_PLAUSIBLE_ARRAY_LEN {
                    return None;
                }
                let data = array_data_size(len, header.element_type()).ok()?;
                // zr z1/rl: the ELEMENT cap above is the corruption screen for
                // an array; the byte cap below is the OBJECT arm's, and applied
                // here it refused REAL arrays. `MAX_PLAUSIBLE_BODY` is 256 MiB
                // (`(1 << 24) * SLOT_SIZE`, 16-byte slots), so
                // `int[(1 << 26) + 1]`, `long[(1 << 25) + 1]` and an
                // `Object[(1 << 25) + 1]` were legal to allocate and unsizable
                // to the collector: the sweep LEAKED them when dead (`sweep_one`
                // keeps an unsizable base forever), the slide stopped at them,
                // and the rewrite pass SKIPPED a big reference array's slots --
                // dangling references to every object the slide moved. The
                // `RJdkBigNum` guard lines were exactly this: BigInteger's
                // `int[MAX_MAG_LENGTH + 1]` (67 108 865 ints, 256 MiB) allocated
                // and dropped on its overflow paths, header `class_id=0` because
                // `newarray` allocates primitive arrays under class id 0. See
                // `docs/feature-designs/zgc-architecture.md`.
                // `CRATONVM_ZGC_LARGE_ARRAY_SIZING=0` restores the byte cap.
                if data > Self::MAX_PLAUSIBLE_BODY && false {
                    return None;
                }
                ARRAY_DATA_OFFSET.checked_add(data)
            }
        }
    }

    /// zr z7/pc: the slide's per-survivor size cell -- [`Self::alloc_size`]
    /// plus one bit saying the object has NO reference slots -- read once by
    /// [`Self::logical_pages`] and reused by the partition, the screen and
    /// the reference rewrite of the same slide (`CRATONVM_ZGC_RELOCATE_SIZE_TABLE`).
    ///
    /// `0` means "not known here, read the header" and is what an unsizable
    /// header, a size at or past 2 GiB and an address outside the low region
    /// all encode to, so every consumer's fallback is the pre-table code.
    /// The no-reference bit is set for exactly the shapes
    /// [`Self::reference_slots_with`] reports nothing for without reading a
    /// body: a primitive array and a humongous filler.
    #[inline]
    pub fn size_cell(header: &ObjectHeader) -> u32 {
        Self::size_cell_sized(header, Self::alloc_size(header))
    }

    /// [`Self::size_cell`] for a caller that already has `alloc_size(header)`
    /// (the sweep's live arm, zr z7/pc).
    #[inline]
    pub fn size_cell_sized(header: &ObjectHeader, size: Option<usize>) -> u32 {
        match size {
            Some(sz) if sz > 0 && sz < ZSIZE_CELL_NOREF as usize => {
                let noref = match header.kind() {
                    ObjectKind::Array => header.element_type() != ArrayElementType::Reference,
                    ObjectKind::HumongousFiller => true,
                    ObjectKind::Object => false,
                };
                sz as u32 | if noref { ZSIZE_CELL_NOREF } else { 0 }
            }
            _ => 0,
        }
    }

    /// The size a cell recorded, or the header's when the cell is `0` (or
    /// there is no table). Same answer as `alloc_size` on the header the
    /// cell was read from.
    #[inline]
    pub fn size_via_cell(&self, cell: u32, addr: usize) -> Option<usize> {
        if cell != 0 {
            Some((cell & !ZSIZE_CELL_NOREF) as usize)
        } else {
            self.survivor_size(addr, self.header_ref(addr as *mut u8))
        }
    }

    /// Push every reference-typed out-edge of the object at `base` onto
    /// `work`. Mirrors how the semi-space collector enumerates an object's
    /// oops, but reads them in place (non-moving).
    /// `skip_index`: for a registered Weak/Soft/Phantom `Reference` object
    /// (see `ReferenceProcessor::reference_object_addresses`'s INT-8 doc),
    /// the referent slot (index 0) must NOT be traced as a strong edge —
    /// otherwise the referent always appears reachable through its own
    /// Reference object and can never be cleared. Mirrors the G1 marker's
    /// `g1_set_reference_skip_set` mechanism (see
    /// `vm/src/runtime/interpreter.rs`), done internally here since
    /// `ZgcRealHeap::collect_garbage` is self-contained (no interpreter
    /// pre/post-GC cooperation step).
    pub fn enumerate_references(
        &self,
        base: *mut u8,
        work: &mut Vec<usize>,
        skip_index: Option<usize>,
    ) {
        // ONE implementation, not two. Until 2026-08-26 this function and
        // `visit_strong_refs_at` were separate copies of the same walk, and the
        // copies had drifted: this one still transmuted a whole `Value` out of
        // each legacy cell, still had no `num_slots` screen, still ran an array
        // element through `read_prim_element`'s null-degrade, and still dropped
        // an unresolvable compact object's out-edges in silence — four defects
        // its twin had already been fixed for.
        //
        // The fork's stated justification was SHAPE: this one fills a
        // `&mut Vec<usize>` while `mark::ZMarkContext::visit_refs` needs a
        // `&mut dyn FnMut(u64)` so the engine can push into its own striped
        // work-stealing stack, and funnelling that through a `Vec` would
        // allocate per object on every mark worker. That is a real constraint
        // and it is satisfied here: the callback form is the implementation and
        // the `Vec` form is this closure, which allocates nothing.
        //
        // The screens are the same screen. `visit_strong_refs_at` reads
        // `num_slots > (1 << 24)` on the legacy arm, `with_class_layout(..)
        // .is_some()` on the compact arm, and deliberately screens the array arm
        // not at all; this function was brought to exactly those three rather
        // than to the stronger `Self::alloc_size`, because `alloc_size` refuses
        // an array past `MAX_PLAUSIBLE_ARRAY_LEN` (1<<28) while `alloc_array`
        // admits up to `i32::MAX` — and for a MARKER, refusing a legal array is
        // a live object collected. Same screen, same reads, so the merge changes
        // no behaviour on either path.
        self.visit_strong_refs_at(base, skip_index, &mut |raw| work.push(raw as usize));
    }

    /// Bounds-and-sanity check shared by `get_field`/`set_field`. Returns the
    /// in-bounds slot count, or `None` (caller treats as no-op / null) when
    /// the header is suspect or the index is out of range. Mirrors the guards
    /// in `g1::get_field` / `gen_heap`.
    pub fn check_field_index(
        &self,
        header: &ObjectHeader,
        index: usize,
        op: &'static str,
    ) -> Option<usize> {
        // An ARRAY receiver is never a plain-object field access, and the index
        // test below cannot see it: `alloc_array` MIRRORS the length into
        // `num_slots`, so every real element index passes while the byte offset
        // `get_field`/`set_field` computes (`HEADER_SIZE + index * SLOT_SIZE`)
        // names neither that element nor, past the first few, any part of the
        // allocation. See `heap::refuse_array_receiver_field_access`.
        if header.kind() == ObjectKind::Array {
            // SAFETY: an `ObjectHeader` sits at offset 0 of its allocation, so
            // the header reference IS the object base; the caller resolved it
            // from a live `ObjectRef`.
            unsafe {
                cratonvm_gc_core::heap::refuse_array_receiver_field_access(
                    header as *const ObjectHeader as usize,
                    header,
                    index,
                    if op == "set" {
                        "zgc::set_field"
                    } else {
                        "zgc::get_field"
                    },
                )
            };
            return None;
        }
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(target: "zgc", index, num_slots, op, "zgc real: suspect header");
            return None;
        }
        if index >= num_slots {
            tracing::warn!(target: "zgc", index, num_slots, op, "zgc real: field index OOB");
            self.report_corpse_read(header, index, op);
            return None;
        }
        Some(num_slots)
    }

    /// Is the receiver of a field or array access an object at all?
    ///
    /// [`Self::check_field_index`] guards the INDEX and nothing guards the
    /// RECEIVER, so an access through an address that merely happens to carry a
    /// plausible header passes every check and reads or writes wherever it
    /// points. That is the one shape that can explain the reopened
    /// `zgc-rewrite-pass-walks-off-a-reference-array` corruption: a store into
    /// object `O` at index `i` writes at `O + HEADER_SIZE + i * SLOT_SIZE`, so
    /// it can only land on another object's offset 0 -- which is what the
    /// extent census keeps finding -- if `O` is not where an object starts.
    ///
    /// Reports and never refuses. Refusing would turn a wrong write into a
    /// silently dropped one, which is harder to debug and no safer: by the time
    /// this fires the receiver is already wrong, and what is wanted is its
    /// identity, not its suppression.
    ///
    /// Behind `CRATONVM_DBG_ZGC_CORPSE`: one bit test on the object-start
    /// bitmap when it is on, a branch when it is off. Capped, and it names
    /// three things -- whether any registered object CONTAINS the receiver
    /// (an interior pointer, so the caller derived it), how far into that
    /// object it points, and what class that container is.
    /// Where did the object that used to live at `addr` go?
    ///
    /// Chained, because an object can move again in a later cycle and the
    /// table records each hop separately; bounded so a cycle in the table
    /// (which would be a bug in the slide, not in a caller) cannot hang the
    /// barrier. Returns `None` unless the final target is a live object base.
    ///
    /// See [`Self::relocations`] for why this exists and why a re-issued
    /// address cannot reach it.
    /// Leave a real `MARK_FORWARDED` word at every vacated address the slide
    /// emptied and did not immediately refill.
    ///
    /// # Why this collector had none, and what that cost
    ///
    /// `VmHeap::load_and_forward` repairs a possibly-stale reference by reading
    /// a FORWARDING WORD at the old address. Every other moving collector in
    /// this tree leaves one; the ZGC slide left none, because
    /// `Arena::compact_low_to` zeroes the span above the new cursor and the
    /// memmove overwrites everything below it. So the barrier was a silent
    /// no-op at all 46 of its call sites -- every one of which exists because
    /// its caller holds an `ObjectRef` somewhere no root scan can see. That is
    /// what [`Self::relocations`] was added to paper over, at the cost of a
    /// global mutex and a hash probe per miss.
    ///
    /// It also destroyed the evidence: a stale read landed on a well-formed
    /// ALL-ZERO object (`class_id=0`, `num_slots=0`) and the reader walked off
    /// the end of a zero-length object with every identifying byte gone. That
    /// is the whole reason [`Self::corpse_ledger`] exists.
    ///
    /// # Which addresses can carry one, and why only those
    ///
    /// **A sliding compactor's from-space IS its to-space.** Survivors are
    /// packed down into the space lower-addressed survivors vacated, so an old
    /// address below the new cursor now holds a DIFFERENT LIVE OBJECT, and
    /// stamping a word there would corrupt it. Nothing can be done for that
    /// class here -- it is a limit of sliding, not of this function, and it is
    /// what a page-evacuating collector would remove.
    ///
    /// `[new_cursor, old_cursor)` is different: the slide packed every survivor
    /// strictly below `new_cursor` (`live_ceiling` is a check on exactly that
    /// answer) and `compact_low_to` has just zero-filled the whole span. It is
    /// provably empty, so a record stamped in it can overwrite nothing. The
    /// registry membership test below is belt-and-braces on top of that
    /// argument, not a substitute for it.
    ///
    /// # Why a stamped word cannot outlive its truth
    ///
    /// The span is handed back to the allocator -- as un-bumped tail, or on the
    /// free list through `vacated`. Both hand-out paths zero what they serve
    /// (`alloc_raw` per object, `tlab_refill` per chunk), so the first reuse of
    /// those bytes erases the record. A stamped word is therefore readable
    /// exactly as long as nothing has claimed the space, which is precisely as
    /// long as it is true.
    ///
    /// # What a conservative scan sees
    ///
    /// The same thing it saw before: not an object. The registry bit was
    /// cleared by the rebuild above, so `is_object_address` refuses the address
    /// whether it holds zeroes or a forwarding word. The zero-fill's stated
    /// purpose -- "a conservative scanner that met one would resurrect a
    /// corpse" -- is served by the registry, and always was.
    pub fn stamp_forwarding_words(&self, pairs: &[(usize, usize)], tail: &std::ops::Range<usize>) {
        if tail.is_empty() {
            return;
        }
        let mut stamped = 0usize;
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        // READABILITY IS NOT A GIVEN ANY MORE. The backing store commits
        // lazily and gives granules back (`cratonvm_gc_core::reservation`), so a span the
        // allocator has retracted past may be reserved-but-uncommitted -- where
        // a write faults instead of landing. The arena is the only thing that
        // knows; asking it once per stamp is free at a safepoint, and it is the
        // difference between a record and a `STATUS_ACCESS_VIOLATION`.
        let mut arena = self.arena.lock();
        let arena_base = arena.base_ptr() as usize;
        for &(from, to) in pairs {
            // Inside the zeroed, provably-empty tail...
            if from < tail.start || from >= tail.end {
                continue;
            }
            // ...with room for a whole mark word...
            if from.saturating_add(cratonvm_types::MIN_OBJECT_SIZE) > self.arena_end {
                continue;
            }
            // ...and not an address the rebuild has just re-issued to a
            // survivor. Cannot happen given the argument above; asserted by
            // construction rather than trusted, because the cost of being wrong
            // is a live object's mark word replaced by a forwarding pointer,
            // which destroys its lock state.
            if self.registry.contains(from) {
                continue;
            }
            if from < arena_base || !arena.is_readable_at(from - arena_base) {
                continue;
            }
            // SAFETY: `from` is an 8-aligned address inside the arena envelope
            // (it was an allocation base until this slide), `from + 16` is
            // bounded above, and the span is zero-filled and unreferenced by
            // any live object. The write is one aligned `u64`.
            //
            // Since the 8-byte header a forwarding record is two words: a
            // FORWARDED mark word in the header word (class id 0) and the
            // target in the second word -- the same shape a copying collector
            // leaves on a forwarded source.
            unsafe {
                let target = (from + cratonvm_types::FORWARDING_TARGET_OFFSET) as *const AtomicU64;
                (*target).store(to as u64, Ordering::Relaxed);
                let mark = (from + MARK_WORD_OFFSET) as *const std::sync::atomic::AtomicU32;
                (*mark).store(cratonvm_types::MARK_FORWARDED, Ordering::Release);
            }
            // ZA1: the arena's slide window (`SlideZeroWindow`) said these
            // bytes read zero; they do not any more, so whoever hands them out
            // next must zero them.
            arena.note_slide_stamp(from - arena_base);
            stamped += 1;
            lo = lo.min(from);
            hi = hi.max(from + cratonvm_types::MIN_OBJECT_SIZE);
        }
        drop(arena);
        // THE EXACT RANGE THAT WAS STAMPED, so the read side never dereferences
        // an address on the strength of an argument about geometry. Cleared by
        // `Arena::decommit_unbumped_middle`'s caller before the space can go
        // away; see `forwarding_word_at`.
        if stamped > 0 {
            self.stamp_lo.store(lo, Ordering::Release);
            self.stamp_hi.store(hi, Ordering::Release);
        }
        self.counters
            .forwarding_words_stamped
            .fetch_add(stamped, Ordering::Relaxed);
    }

    /// Forget the last slide's forwarding records.
    ///
    /// **Call immediately before anything can decommit the span they live in.**
    /// The records sit in the vacated tail, which is exactly the space
    /// `Arena::decommit_unbumped_middle` releases -- so the read side has to
    /// stop believing in them before, not after.
    pub fn retire_forwarding_words(&self) {
        self.stamp_hi.store(0, Ordering::Release);
        self.stamp_lo.store(usize::MAX, Ordering::Release);
    }

    /// Read a forwarding word stamped by [`Self::stamp_forwarding_words`].
    ///
    /// `None` unless `addr` is an 8-aligned, non-registered address inside this
    /// arena whose mark word is in `MARK_FORWARDED` state and whose target is a
    /// live object base. Every one of those is load-bearing:
    ///
    /// * **8-aligned and in-arena** bounds the read. Callers hand this
    ///   addresses that are already suspect -- that is the point of the barrier
    ///   -- so a wild read here would be the very crash it exists to prevent.
    /// * **not registered** because a LIVE object's mark word is its own lock
    ///   or hash word. Nothing but this function's stamp ever writes
    ///   `MARK_FORWARDED` on this collector, but the screen means a future
    ///   producer cannot turn a locked object into a forwarding record by
    ///   accident.
    /// * **the target is a live base** because a stale record whose target has
    ///   since died must answer `None`, not hand back a freed address. The
    ///   table lookup this parallels ends with the identical check.
    pub fn forwarding_word_at(&self, addr: usize) -> Option<usize> {
        // INSIDE THE RANGE THE LAST SLIDE ACTUALLY STAMPED, first and cheapest.
        //
        // Two relaxed-ish loads and two compares, and they are what makes this
        // read SAFE rather than merely likely to hit: the heap commits lazily
        // and hands granules back, so an arbitrary in-arena address is not
        // necessarily mapped. Every address in `[stamp_lo, stamp_hi)` was
        // verified readable when it was written and is retired before anything
        // can take the space -- see `retire_forwarding_words`.
        //
        // It is also the whole of the "did this cycle stamp anything" test: the
        // range is empty (`hi == 0`) whenever no slide has run since the last
        // retire, so a heap that never compacts pays two loads.
        if addr < self.stamp_lo.load(Ordering::Acquire)
            || addr.saturating_add(cratonvm_types::MIN_OBJECT_SIZE)
                > self.stamp_hi.load(Ordering::Acquire)
        {
            return None;
        }
        if addr < self.arena_base
            || addr.saturating_add(cratonvm_types::MIN_OBJECT_SIZE) > self.arena_end
            || (addr - self.arena_base) & 7 != 0
            || self.registry.contains(addr)
        {
            return None;
        }
        // SAFETY: bounded above, 8-aligned, and inside a `Vec<u8>` that lives
        // for as long as this heap. Reading a `u64` out of initialised arena
        // bytes is well-defined whatever they hold.
        let mark = unsafe {
            (*((addr + MARK_WORD_OFFSET) as *const std::sync::atomic::AtomicU32))
                .load(Ordering::Acquire)
        };
        if !cratonvm_types::ObjectHeader::is_forwarded_mark(mark) {
            return None;
        }
        let target = unsafe {
            (*((addr + cratonvm_types::FORWARDING_TARGET_OFFSET) as *const AtomicU64))
                .load(Ordering::Relaxed)
        } as usize;
        self.registry.contains(target).then_some(target)
    }

    /// `(bytes committed for this heap right now, bytes returned to the OS
    /// over its life)`.
    ///
    /// The two numbers that say whether the reserving backing store is doing
    /// anything. `committed` well below `-Xmx` is lazy commit working;
    /// `returned` above zero is the give-back working. Both are the honest
    /// `capacity` and `0` on the wholly-committed fallback.
    pub fn commit_stats(&self) -> (usize, usize) {
        (
            self.arena.lock().committed_bytes(),
            self.counters.bytes_uncommitted.load(Ordering::Relaxed),
        )
    }

    /// zr z7/rs: the ADDRESS RANGES the arena and the three bitmaps occupy,
    /// for the `[GC] zgc-footprint-spans:` line, so a `/proc/<pid>/smaps`
    /// reader (`tools/rss-by-mapping.sh`) can attribute the mappings that hold
    /// them instead of lumping them with the allocator's. `(lo, hi)` byte
    /// addresses; `(0, 0)` for a bitmap that does not exist (the header arm,
    /// or the old-start bitmap before its first use). Takes no lock.
    pub fn footprint_spans(&self) -> ZFootprintSpans {
        let span = |b: Option<&ZObjectStartBits>| -> (usize, usize) {
            b.filter(|b| b.nwords != 0).map_or((0, 0), |b| {
                let lo = b.words as usize;
                (lo, lo + b.reserved_bytes())
            })
        };
        let on_own_mapping = |b: Option<&ZObjectStartBits>| -> usize {
            usize::from(b.is_some_and(ZObjectStartBits::on_own_mapping))
        };
        ZFootprintSpans {
            arena: (self.arena_base, self.arena_end),
            start_bits: span(self.registry.bitmap()),
            mark_bits: span(self.mark_bits.as_ref()),
            old_bits: span(self.counters.gen_old_bits.get()),
            side_maps: on_own_mapping(self.registry.bitmap())
                + on_own_mapping(self.mark_bits.as_ref())
                + on_own_mapping(self.counters.gen_old_bits.get()),
            arena_no_thp: self.counters.fp.arena_no_thp.load(Ordering::Relaxed),
        }
    }

    /// What this heap holds, per structure (zr z3/fp) -- the figures the
    /// `[GC] zgc-footprint:` summary line prints so a host maxrss can be
    /// attributed. Takes the arena, forwarding-table and scratch locks one at a
    /// time; a diagnostic, not something to call on an allocation path.
    pub fn footprint_stats(&self) -> ZFootprintStats {
        let fp = &self.counters.fp;
        let (
            reserved,
            committed,
            net_committed,
            ever_written,
            reset_total,
            bookkeeping,
            commit_screen,
        ) = {
            let arena = self.arena.lock();
            (
                arena.capacity(),
                arena.committed_bytes(),
                arena.net_committed_bytes(),
                arena.capacity().saturating_sub(arena.pristine_bytes()),
                arena.bytes_reset(),
                arena.bookkeeping_bytes(),
                arena
                    .commit_bits()
                    .map_or(0, |b| b.len() * std::mem::size_of::<AtomicU64>()),
            )
        };
        fp.committed_peak.fetch_max(committed, Ordering::Relaxed);
        let bitmap = |b: Option<&ZObjectStartBits>| -> (usize, usize) {
            b.map_or((0, 0), |b| (b.reserved_bytes(), b.noted_page_bytes()))
        };
        let (start_r, start_n) = bitmap(self.registry.bitmap());
        let (mark_r, mark_n) = bitmap(self.mark_bits.as_ref());
        let (old_r, old_n) = bitmap(self.counters.gen_old_bits.get());
        let (reloc_len, reloc_bytes) = {
            let reloc = self.counters.relocations.lock();
            (
                reloc.len(),
                zgc_fp_hash_table_bytes(
                    reloc.capacity(),
                    std::mem::size_of::<(usize, (usize, u32))>(),
                ),
            )
        };
        fp.relocations_peak
            .fetch_max(reloc_bytes, Ordering::Relaxed);
        let scratch = self.counters.dead_scratch.lock().capacity() * std::mem::size_of::<usize>();
        fp.dead_scratch_peak.fetch_max(scratch, Ordering::Relaxed);
        ZFootprintStats {
            arena_reserved: reserved,
            arena_committed: committed,
            arena_committed_peak: fp.committed_peak.load(Ordering::Relaxed),
            arena_ever_written: ever_written,
            arena_uncommitted_total: self.counters.bytes_uncommitted.load(Ordering::Relaxed),
            arena_reset_total: reset_total,
            arena_bookkeeping: bookkeeping,
            start_bits_reserved: start_r,
            start_bits_noted: start_n,
            mark_bits_reserved: mark_r,
            mark_bits_noted: mark_n,
            old_bits_reserved: old_r,
            old_bits_noted: old_n,
            grids: self.young_pages.len() * std::mem::size_of::<AtomicU64>() + commit_screen,
            relocations_entries: reloc_len,
            relocations_bytes: reloc_bytes,
            relocations_peak: fp.relocations_peak.load(Ordering::Relaxed),
            dead_scratch_bytes: scratch,
            dead_scratch_peak: fp.dead_scratch_peak.load(Ordering::Relaxed),
            snapshot_peak: fp.snapshot_peak.load(Ordering::Relaxed),
            remset_bytes: self.remembered.total_retained_bytes(),
            trims: fp.trims.load(Ordering::Relaxed),
            trimmed_bytes: fp.trimmed_bytes.load(Ordering::Relaxed),
            trim_regrows: fp.trim_regrows.load(Ordering::Relaxed),
            idle_stamps: fp.idle_stamps.load(Ordering::Relaxed),
            sparse_words_skipped: fp.sparse_words_skipped.load(Ordering::Relaxed),
            arena_net_committed: net_committed,
            slide_pairs_peak: fp.slide_pairs_peak.load(Ordering::Relaxed),
            slide_index_peak: fp.slide_index_peak.load(Ordering::Relaxed),
            slide_map_peak: fp.slide_map_peak.load(Ordering::Relaxed),
            slide_transient_peak: fp.slide_transient_peak.load(Ordering::Relaxed),
            uncommit_passes: fp.uncommit_passes.load(Ordering::Relaxed),
            uncommit_passes_released: fp.uncommit_passes_released.load(Ordering::Relaxed),
            uncommit_reset_bytes: fp.uncommit_reset_bytes.load(Ordering::Relaxed),
            uncommit_decommitted_bytes: fp.uncommit_decommitted_bytes.load(Ordering::Relaxed),
            uncommit_busy: fp.uncommit_busy.load(Ordering::Relaxed),
            bitmap_pages_released: fp.bitmap_pages_released.load(Ordering::Relaxed),
        }
    }

    /// Per-heap overrides of the three zr z3/fp switches, for tests (the
    /// process defaults are `CRATONVM_ZGC_FOOTPRINT_TRIM`,
    /// `CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP` and
    /// `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`).
    pub fn set_footprint_switches(&self, trim: bool, stamp_at_sweep: bool, sparse_clear: bool) {
        let fp = &self.counters.fp;
        fp.trim_on.store(trim, Ordering::Relaxed);
        fp.stamp_at_sweep_on
            .store(stamp_at_sweep, Ordering::Relaxed);
        fp.sparse_clear_on.store(sparse_clear, Ordering::Relaxed);
        self.arena.lock().set_keep_refused_idle_stamps(trim);
    }

    /// How many forwarding words the slides have stamped, and how many
    /// `forwarded_after_slide` answers came from one rather than from the
    /// table.
    ///
    /// The engagement counter for the pair. `stamped` at zero on a run with
    /// relocations means every vacated address was below the new cursor, i.e.
    /// the slide refilled everything it emptied; `from_word` at zero with
    /// `stamped` high means the barrier is never asked about the tail, and the
    /// table is carrying the whole load after all.
    ///
    /// (zs s2/sl2: this doc sat above `commit_stats`, glued to its own.)
    pub fn forwarding_word_engagement(&self) -> (usize, usize) {
        (
            self.counters
                .forwarding_words_stamped
                .load(Ordering::Relaxed),
            self.counters.forwarding_words_read.load(Ordering::Relaxed),
        )
    }

    pub fn forwarded_after_slide(&self, addr: usize) -> Option<usize> {
        // zt t23/zc7 (R3, `CRATONVM_ZGC_REGION_QUARANTINE`): the region cycle
        // kept since the last slide, first. During its mutator interval it is
        // the only record of the low end's moves (the slide did not publish
        // them below), and its answer needs no chain walk: no slide runs
        // before its release, so its `to` is the object's current base. One
        // acquire load when no cycle is kept.
        if let Some(to) = self.region_cycle_forwarded(addr) {
            return Some(to);
        }
        // THE STAMPED WORD FIRST. It is a bounds check, an aligned load and a
        // tag test -- no lock, no hash, no allocation -- and it is the answer
        // for every address the slide emptied and did not refill. The table
        // below covers the rest: an old address BELOW the new cursor, where a
        // survivor now sits and no record can be stamped. See
        // `stamp_forwarding_words` for why that split is a property of sliding
        // compaction rather than of this function.
        if let Some(target) = self.forwarding_word_at(addr) {
            self.counters
                .forwarding_words_read
                .fetch_add(1, Ordering::Relaxed);
            return Some(target);
        }
        let reloc = self.counters.relocations.lock();
        if reloc.is_empty() {
            return None;
        }
        let first = *reloc.get(&addr)?;
        let cur = forward_chain_end(&reloc, first, zgc_forward_chain_by_epoch());
        drop(reloc);
        if self.registry.contains(cur) {
            return Some(cur);
        }
        // zt t23/zc7 (R3): an older slide's chain can end at a base the live
        // cycle moved since (its pair is in the cycle, not in the table).
        self.region_cycle_forwarded(cur)
    }

    /// Publish one slide's `from -> to` pairs into `counters.relocations`,
    /// stamped with a fresh slide epoch (zr z2/r2; see the field doc for why
    /// the chain walk needs it).
    pub fn publish_relocations(&self, pairs: &[(usize, usize)]) {
        if pairs.is_empty() {
            return;
        }
        let epoch = self
            .counters
            .slide_epoch
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut reloc = self.counters.relocations.lock();
        reloc.reserve(pairs.len());
        for (from, to) in pairs {
            reloc.insert(*from, (*to, epoch));
        }
        // zt t8/rl2: the demand the trim hysteresis reads, and the regrow
        // census.
        self.counters
            .fp
            .after_relocations_publish(pairs.len(), reloc.capacity());
    }

    /// Drop entries whose vacated address has been handed out again — the
    /// lookup can no longer reach them, so they are pure memory. Called once
    /// per collection, after the sweep has published the live registry.
    ///
    /// AMORTIZED since zr z2/r2 (`CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED`,
    /// default ON, `=0` restores the walk on every collection). The walk used
    /// to run on every collection -- young cycles, refused and cost-gated
    /// ones, none of which add an entry -- over a table that sits near its
    /// 4 M ceiling on a long compacting run, inside the pause and charged to
    /// the sweep's share. It now runs only once the table has DOUBLED since
    /// the last walk (floored at `ZGC_RELOCATIONS_PRUNE_MIN`, capped at the
    /// ceiling so the ceiling still binds), so the total walked is at most
    /// twice the entries ever inserted. What the skipped walks would have
    /// dropped is an entry whose vacated address is a live base again: a
    /// lookup never starts there (the barrier asks only about addresses that
    /// are NOT live bases), and the chain walk stops at it unless a LATER
    /// slide recorded it (`forward_chain_end`), so keeping it longer changes
    /// no answer the epoch rule gives. Counted in `relocation_prunes_run` /
    /// `relocation_prunes_skipped` / `relocation_prune_entries_walked`.
    pub fn prune_relocations(&self) {
        let mut reloc = self.counters.relocations.lock();
        if reloc.is_empty() {
            // zr z3/fp: an empty table can still hold its peak allocation
            // (`clear()` below keeps it); give that back too.
            self.counters.fp.after_relocations_prune(&mut *reloc, true);
            return;
        }
        // zt t1/rl (`CRATONVM_ZGC_RELOCATIONS_CEILING_SCALED`, default ON since zt t12): the
        // ceiling follows the heap, never above the absolute one.
        let ceiling = zgc_relocations_ceiling_for(
            self.heap_capacity(),
            true,
        );
        if zgc_relocations_prune_amortized()
            && !relocations_prune_due_with(
                reloc.len(),
                self.counters
                    .relocations_prune_due_at
                    .load(Ordering::Relaxed),
                ceiling,
            )
        {
            self.counters
                .relocation_prunes_skipped
                .fetch_add(1, Ordering::Relaxed);
            // zr z3/fp: peak only -- a skipped prune has not looked at which
            // entries are dead, so the table is not trimmed here.
            self.counters.fp.after_relocations_prune(&mut *reloc, false);
            return;
        }
        self.counters
            .relocation_prunes_run
            .fetch_add(1, Ordering::Relaxed);
        let walked = reloc.len();
        self.counters
            .relocation_prune_entries_walked
            .fetch_add(walked as u64, Ordering::Relaxed);
        let started = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
        reloc.retain(|from, _| !self.registry.contains(*from));
        self.counters
            .relocations_prune_due_at
            .store(reloc.len().saturating_mul(2), Ordering::Relaxed);
        // `CRATONVM_DBG_ZGC_RELOCATE_PHASES` (zr z2/r2): the prune is charged
        // to the sweep's share of the pause, so it gets its own line.
        if let Some(started) = started {
            eprintln!(
                "[zgc-relocations-prune] walked={walked} kept={} us={} runs={} skips={}",
                reloc.len(),
                started.elapsed().as_micros(),
                self.counters.relocation_prunes_run.load(Ordering::Relaxed),
                self.counters
                    .relocation_prunes_skipped
                    .load(Ordering::Relaxed),
            );
        }
        // A hard ceiling as well: this table is bounded by the arena in
        // principle, but "in principle" is not a bound anyone can point at in a
        // heap dump. Clearing degrades the barrier to what it did before this
        // table existed, which is the safe direction, and says so.
        if reloc.len() > ceiling {
            tracing::warn!(
                target: "cratonvm::gc",
                entries = reloc.len(),
                ceiling,
                "zgc relocation table exceeded its ceiling and was cleared — the forwarding \
                 barrier degrades to a no-op for older moves until the next slide"
            );
            reloc.clear();
            self.counters
                .relocations_prune_due_at
                .store(0, Ordering::Relaxed);
        }
        // zr z3/fp (`CRATONVM_ZGC_FOOTPRINT_TRIM`): `retain` and `clear` keep
        // the table's PEAK allocation. Near the 4 M ceiling that is ~200 MB of
        // buckets (`zgc_fp_hash_table_bytes`), held for the rest of the run by
        // a table that may now hold a fraction of it. Released once capacity
        // exceeds four times what survived, keeping twice that.
        self.counters.fp.after_relocations_prune(&mut *reloc, true);
    }

    /// The flag-free "was this receiver RECLAIMED?" verdict, for THIS
    /// collector -- the port of `GenerationalHeap::reclaimed_hole_at`.
    ///
    /// ZGC has been the DEFAULT collector since 2026-08-10, and until this
    /// existed the whole H2-CID0 apparatus in `vm::memory::reclaim_guard`
    /// answered `None` on it: a receiver that reads back as `java.lang.Object`
    /// (`ClassId(0)` -- which is also the all-zero header the sweep leaves over
    /// a span it freed, and also an ordinary `new Object()`) reached the reader
    /// with no way to tell those three apart, and the `NoSuchMethodError
    /// method="java/lang/Object.<anything>"` terminal printed nothing else at
    /// all. That is exactly the state the H2 `TestMultiThread` MVStore-writer
    /// write-up is stuck in.
    ///
    /// Free-list membership is not ambiguous here either: a live object is
    /// never inside a free block and never in the un-bumped middle between the
    /// two cursors, so the arena can answer outright with no flag set in
    /// advance. Costs both the arena lock and a scan of the free list, so it is
    /// asked only from terminal error paths.
    ///
    /// Deliberately does NOT report an interior address of a LIVE object: that
    /// is a derived pointer, not reclaimed memory, and reporting it under a
    /// message that says "RECLAIMED" would be worse than silence.
    /// `audit_access_receiver` covers that case.
    pub fn reclaimed_hole_at(&self, addr: usize) -> Option<(&'static str, usize, usize)> {
        // A registered base is a live object, whatever its header says.
        if self.registry.contains(addr) {
            return None;
        }
        let arena = self.arena.lock();
        let base = arena.base_ptr() as usize;
        let cap = arena.capacity();
        if addr < base || addr >= base + cap {
            return None;
        }
        let off = addr - base;
        for (o, size) in arena.free_blocks_sorted() {
            if off >= o && off < o.saturating_add(size) {
                return Some(("zgc arena free list (swept and coalesced)", base + o, size));
            }
        }
        // The un-bumped middle. `used()` is both regions together, so the low
        // cursor is recovered from it and the high one rather than read
        // directly (the field is private, and deriving it here keeps the two
        // definitions from drifting).
        let high = arena.high_cursor();
        let low = arena.used().saturating_sub(cap.saturating_sub(high));
        if off >= low && off < high {
            return Some((
                "zgc arena un-bumped middle (never allocated, or retracted into)",
                base + low,
                high - low,
            ));
        }
        None
    }

    /// Live objects that still name `addr` in a decoded reference slot.
    ///
    /// The companion half of [`Self::reclaimed_hole_at`], and the one fact the
    /// "survived un-rewritten" face has never had on this collector: the
    /// verdict says the address is reclaimed, this says WHO still points at it.
    /// Decoded through `reference_slots` -- each object's own slot enumerator,
    /// which knows the compact and legacy cell shapes apart -- rather than a
    /// raw word scan, which on this heap reports ~1M stale `Value`-cell padding
    /// words per compaction.
    ///
    /// The third element is the ordinal of the slot within the object's OWN
    /// reference-slot enumeration, not a field index: this walker sees words,
    /// not fields, and an invented field index would be a wrong number rather
    /// than a missing one.
    ///
    /// O(live) under the registry lock, so terminal error paths only, and
    /// `cap`ped.
    pub fn live_holders_of(&self, addr: usize, cap: usize) -> Vec<(usize, u32, usize)> {
        use census::ZCensusHeapView;
        let mut out: Vec<(usize, u32, usize)> = Vec::new();
        if cap == 0 {
            return out;
        }
        let target = addr as u64;
        for holder in self.registry.snapshot().bases() {
            let mut ordinal = 0usize;
            let mut hit: Option<usize> = None;
            self.reference_slots(holder as u64, &mut |slot| {
                if hit.is_none() && slot.raw_word == target {
                    hit = Some(ordinal);
                }
                ordinal += 1;
            });
            if let Some(slot) = hit {
                let cid = self.header_ref(holder as *mut u8).class_id.as_u32();
                out.push((holder, cid, slot));
                if out.len() >= cap {
                    break;
                }
            }
        }
        out
    }

    /// `CRATONVM_DBG_ZGC_CORPSE` -- what the slide moved AWAY from `addr`, if
    /// the ledger was armed for this run.
    ///
    /// Returns `(from, moved_to, class_id, size, still_live_at_target)`. The
    /// exact base first, then an interior hit, matching `report_corpse_read`.
    /// `None` when the flag is off (the ledger costs a map insert per relocated
    /// object, and a cycle relocates hundreds of thousands) or when no record
    /// covers the address.
    pub fn corpse_lookup(&self, addr: usize) -> Option<(usize, usize, u32, usize, bool)> {
        if !zgc_corpse_enabled() {
            return None;
        }
        let hit = {
            let ledger = self.counters.corpse_ledger.lock();
            ledger.get(&addr).copied().map(|v| (addr, v)).or_else(|| {
                ledger
                    .iter()
                    .find(|(from, (_, _, size, _))| addr > **from && addr < **from + *size)
                    .map(|(from, v)| (*from, *v))
            })
        };
        hit.map(|(from, (to, class_id, size, _cycle))| {
            (from, to, class_id, size, self.registry.contains(to))
        })
    }

    pub fn audit_access_receiver(&self, base: usize, index: usize, op: &'static str) {
        if !zgc_corpse_enabled() || self.registry.contains(base) {
            return;
        }
        let n = self.counters.corpse_reports.fetch_add(1, Ordering::Relaxed);
        if n >= 24 {
            return;
        }
        // The registered object this address falls inside, if any. An interior
        // hit means the caller HELD a derived pointer; a miss means the address
        // is not in any live object at all, which is a different bug.
        let container = self.registry.nearest_base_at_or_below(base);
        let (container_base, container_class, container_slots, interior_off) = match container {
            Some(c) if c < base => {
                let ch = self.header_ref(c as *mut u8);
                (c, ch.class_id.as_u32(), ch.num_slots(), base - c)
            }
            _ => (0, 0, 0, 0),
        };
        let h = self.header_ref(base as *mut u8);
        tracing::error!(
            target: "cratonvm::gc::guard",
            op,
            base,
            index,
            receiver_class_id = h.class_id.as_u32(),
            receiver_num_slots = h.num_slots(),
            receiver_kind = ?h.kind(),
            container_base,
            container_class,
            container_slots,
            interior_off,
            "zgc access audit: the receiver of this field/array access is not a \
             registered object base -- the access will land wherever it points"
        );
    }

    /// `CRATONVM_DBG_ZGC_CORPSE` -- name the object that USED to be at the
    /// address a stale reference just read.
    ///
    /// # Why the crash cannot be read without this
    ///
    /// The `zgc real: field index OOB index=N num_slots=0` signature is what a
    /// stale pointer into a compacted-away object looks like, and it is
    /// deliberately anonymous: `compact_low_to` zeroes the vacated span so a
    /// conservative scan cannot resurrect a corpse, which also erases the
    /// class id, the slot count and every field. So the warning reports the
    /// only two numbers that survive, and neither of them identifies anything.
    ///
    /// Three questions decide what to fix, and the zeroed header answers none:
    ///
    ///  * **What moved away?** The class of the vacated object names the
    ///    family of holder. A `Thread`, a `ThreadLocal` map or a JNI handle
    ///    points somewhere very different from an ordinary Java object.
    ///  * **Where did it go?** If the survivor is still alive at `moved_to`,
    ///    the holder simply was not rewritten -- a missing remap. If it is not,
    ///    the object died later and this is a lifetime bug instead.
    ///  * **Who is reading?** The Rust caller chain separates an interpreter
    ///    `getfield` (a Java slot the rewrite missed -- but the slide verifier
    ///    already reports `missed_rewrites=0`, so this would be a contradiction
    ///    worth having) from a native accessor holding a raw address across a
    ///    safepoint (which the slot graph cannot see, and which is where the
    ///    evidence currently points).
    ///
    /// Capped at eight full reports: the count of these warnings measures how
    /// long a run SURVIVED, not how broken it is, and thousands of backtraces
    /// would bury the first one -- which is the only one whose heap state is
    /// still close to the fault.
    #[cold]
    pub fn report_corpse_read(&self, header: &ObjectHeader, index: usize, op: &'static str) {
        if !zgc_corpse_enabled() {
            return;
        }
        let n = self.counters.corpse_reports.fetch_add(1, Ordering::Relaxed);
        if n >= 8 {
            return;
        }
        let addr = header as *const ObjectHeader as usize;
        // Exact base first; then an interior hit, because a stale pointer is
        // not always to the object's base.
        let hit = {
            let ledger = self.counters.corpse_ledger.lock();
            ledger.get(&addr).copied().map(|v| (addr, v)).or_else(|| {
                ledger
                    .iter()
                    .find(|(from, (_, _, size, _))| addr > **from && addr < **from + *size)
                    .map(|(from, v)| (*from, *v))
            })
        };
        // `exe+RVA` when the hook is installed; `Backtrace::force_capture`
        // otherwise. Paste an RVA list into `CRATONVM_SYMBOLIZE` with the SAME
        // binary (and its PDB in place, i.e. `target/release/cratonvm.exe`, not
        // a renamed copy -- the debug directory records the original PDB path).
        //
        // ON LINUX THE FALLBACK IS THE PATH, AND IT WORKS. No caller installs
        // the hook, and this comment used to say the fallback "yields nothing
        // but `<unknown>`". Measured 2026-08-24 on a fat-LTO release build:
        // **40 symbolized frames with file:line**, which named the defect four
        // frames up. `Display` is MULTI-LINE, so a one-line grep of the log
        // shows frame 0 and nothing else -- read the lines after `backtrace=`
        // before concluding the capture is broken, as one session did.
        let backtrace = {
            let rvas = cratonvm_gc_core::gc_quiescence::native_rvas();
            if rvas.is_empty() {
                std::backtrace::Backtrace::force_capture().to_string()
            } else {
                rvas.iter()
                    .map(|r| format!("0x{r:X}"))
                    .collect::<Vec<_>>()
                    .join(",")
            }
        };
        match hit {
            Some((from, (to, class_id, size, cycle))) => {
                let still_live = self.registry.contains(to);
                let cycles_ago = self
                    .counters
                    .corpse_cycle
                    .load(Ordering::Relaxed)
                    .saturating_sub(cycle + 1);
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    read_addr = addr,
                    vacated_base = from,
                    interior_offset = addr - from,
                    moved_to = to,
                    size,
                    class_id,
                    class = %self.diag_class_name(class_id),
                    survivor_still_registered = still_live,
                    root_source = cratonvm_gc_core::gc_quiescence::root_source_of(addr)
                        .unwrap_or("<none: not handed to the marker as a root>"),
                    vacated_in_cycle = cycle,
                    cycles_ago,
                    index,
                    // READ or WRITE decides who is holding it. A WRITE at
                    // index 0 of a `Reference` is somebody clearing a
                    // referent -- the collector or the reference processor. A
                    // READ is Java code, or the VM reading `referent` on
                    // behalf of `Reference.get`. The two want entirely
                    // different searches, and the zeroed header cannot tell
                    // them apart.
                    op,
                    %backtrace,
                    "zgc corpse read: this address was vacated by the LAST slide -- something still holds the pre-move address"
                );
            }
            None => {
                // Not from the last slide. Either an older cycle vacated it
                // (the ledger keeps one cycle only) or the address was never a
                // relocation source -- which would make this a different bug
                // and is worth distinguishing rather than assuming.
                //
                // `in_registry=true` settles which, and it is the case the
                // Tomcat `num_slots=0` signature actually hits (2026-08-24):
                // the address is a LIVE REGISTERED object that has no slots,
                // not a corpse at all. The report used to stop one field short
                // of a diagnosis -- "some live object has zero slots" names no
                // suspect. A corpse's header is zeroed by `compact_low_to`, but
                // a live object's is not, so its class id is still readable and
                // is the only thing that can point at the allocation site.
                let live = self.registry.contains(addr);
                let class_id = if live {
                    header.class_id.as_u32()
                } else {
                    u32::MAX
                };
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    read_addr = addr,
                    index,
                    root_source = cratonvm_gc_core::gc_quiescence::root_source_of(addr)
                        .unwrap_or("<none: not handed to the marker as a root>"),
                    slides_so_far = self.counters.corpse_cycle.load(Ordering::Relaxed),
                    op,
                    in_registry = live,
                    class_id,
                    class = %self.diag_class_name(class_id),
                    header_num_slots = header.num_slots(),
                    %backtrace,
                    "zgc corpse read: OOB read at an address the LAST slide did not vacate -- older cycle, or never a relocation source"
                );
            }
        }
    }

    // ---- THE MARK-BIT FUNNEL ----------------------------------------------
    //
    // Every read and write of this cycle's liveness bit goes through the four
    // methods below, and nothing outside them names [`GC_FLAG_MARKED`] on this
    // collector's path. That is what makes [`Self::mark_bits`] a kill switch
    // rather than a fork: with it `None` the four are the header operations
    // they replaced, byte for byte.
    //
    // They are `#[inline]` and branch on an `Option` that is fixed at heap
    // construction, so the branch predicts perfectly and the header arm costs
    // exactly what it did before.

    /// Is the object at `base` marked live for this cycle? **Query only.**
    ///
    /// An implementation that marked as a side effect would answer "live,
    /// because I just made it live" for every referent handed to
    /// [`ReferenceProcessor`], and no weak, soft, phantom or cleaner reference
    /// would ever be cleared again.
    #[inline]
    pub fn mark_is_set(&self, base: usize) -> bool {
        match &self.mark_bits {
            Some(bits) => bits.contains(base),
            // SAFETY: every address reachable here is either a registered live
            // allocation base (whose first HEADER_SIZE bytes are a valid
            // header) or null, which every caller screens.
            None => self.header_ref(base as *mut u8).gc_flags() & GC_FLAG_MARKED != 0,
        }
    }

    /// Mark the object at `base`, reporting whether **this call** marked it.
    ///
    /// The exactly-once claim: `true` means the caller now owns the obligation
    /// to trace the object's out-edges. See [`ZObjectStartBits::claim`] and
    /// [`ObjectHeader::try_add_gc_flags`] for why a lost update here is fatal
    /// rather than merely wasteful.
    #[inline]
    pub fn mark_claim(&self, base: usize) -> bool {
        let won = match &self.mark_bits {
            Some(bits) => bits.claim(base),
            // SAFETY: as `mark_is_set`.
            None => self
                .header_ref(base as *mut u8)
                .try_add_gc_flags(GC_FLAG_MARKED),
        };
        // zt t34: the R6 plan's region liveness and legacy holders.
        if won && self.counters.crl.live.armed() {
            self.crl_liveness_note_marked(base);
        }
        won
    }

    /// Mark the object at `base`, unconditionally.
    ///
    /// For the callers that already know the bit is clear, or do not care —
    /// the young cycle's old-generation pre-mark and the single-threaded
    /// trace, both of which have just tested it.
    #[inline]
    pub fn mark_set(&self, base: usize) {
        match &self.mark_bits {
            Some(bits) => bits.insert(base),
            // SAFETY: as `mark_is_set`.
            None => self
                .header_ref(base as *mut u8)
                .add_gc_flags(GC_FLAG_MARKED),
        }
    }

    /// The arena address ranges that can hold an object start: the low bump
    /// region and the large-object end, with the never-bumped middle between
    /// them excluded.
    ///
    /// This is what makes the per-cycle bitmap passes cost what the heap USES
    /// rather than what `-Xmx` reserves. The object-start registry and the mark
    /// bits are both sized by capacity -- one bit per 8 arena bytes -- and the
    /// snapshot the mark phase takes was copying all of it every cycle. At
    /// `-Xmx4g` that is 8.4 million atomic loads into a fresh 64 MiB `Vec`,
    /// paid whether the heap holds ten objects or ten million.
    ///
    /// Measured on `G1ChurnPauseProbe 50 1800`, comparing cycles at the SAME
    /// registered-object count so the only variable is the heap flag:
    /// `snapshot_us` 11-13 ms at `-Xmx2048m` against 21-23 ms at `-Xmx4096m`.
    /// Exactly the ratio of the capacities. That is a pause floor no allocation
    /// budget can lower, and it is why a pause target that holds at 2 GiB was
    /// unreachable at 4 GiB (see `refresh_pause_target_budget`).
    ///
    /// # The middle is provably empty
    ///
    /// `Arena` is two-ended: small objects and TLAB chunks bump UP from offset
    /// 0, allocations at or above `ZGC_LARGE_OBJECT_MIN` bump DOWN from
    /// capacity, and the span between the two cursors has never been handed
    /// out. No allocation starts there, so no start bit in it is set, so
    /// copying it transfers zeroes.
    ///
    /// This is also what pays for `Arena::retract_cursor_to`. Retraction lowers
    /// the low cursor onto the last survivor after every sweep; without bounded
    /// passes that only helped the allocator find contiguous space, because the
    /// bitmap work stayed sized by capacity either way. With them, retraction
    /// SHRINKS THE PAUSE: the tail of garbage a burst allocated is handed back,
    /// and the next cycle's snapshot, clear and complement sweep all stop at
    /// the new cursor.
    ///
    /// Takes the arena lock. The caller is at a safepoint, so the two cursors
    /// cannot move while the ranges are in use.
    pub fn live_bitmap_bounds(&self) -> [(usize, usize); 2] {
        let base = self.arena_base;
        let (low_end, high_start, end) = {
            let arena = self.arena.lock();
            (
                // HIGH-WATER, not the cursor. `Arena::retract_cursor_to`
                // lowers the cursor onto the last survivor after every sweep,
                // and the MARK bitmap has bits above the new cursor -- set
                // during the cycle, when it was still high. A clear bounded by
                // the cursor leaves them, and the next cycle then reads a mark
                // set carrying a previous cycle's bits. `Arena::low_high_water`
                // is the bound both bitmaps need; the debug verifier in
                // `ZObjectStartBits::debug_assert_clear_outside` is what caught
                // the cursor being the wrong one.
                base + arena.low_high_water(),
                // WATER MARK on this side too, and for the mirrored reason.
                // `retract_high_cursor_into_free_head` RAISES `high_cursor`,
                // which shrinks `[high_cursor, capacity)` and would leave this
                // cycle's mark bits below the new cursor uncleared -- read back
                // as "already marked" once the cursor bumps down over them
                // again, so the marker never traces those objects. The low end
                // got `low_high_water` when the bounds landed; this side did not.
                base + arena.high_low_water(),
                base + arena.capacity(),
            )
        };
        [(base, low_end), (high_start, end)]    }

    /// May this pause clear registry summary bits? (See
    /// [`Self::forget_empty_registry_blocks`] for the conditions.)
    pub fn registry_forget_allowed(&self) -> bool {
        // zt t2/al (X1): the carve half of the requirement is the switch this
        // heap's carves actually ran with (seeded on the registry), not a
        // fresh environment read.
        self.tlabs.carve_notes_blocks.load(Ordering::Relaxed)
            && vm_tlab::zgc_forget_empty_blocks_enabled()
            && !cratonvm_gc_core::gc_quiescence::unrewritable_peer_state()
    }

    /// Clear **every** mark bit in the heap.
    ///
    /// One linear pass over `capacity / 512` bytes on the bitmap arm; on the
    /// header arm the caller must still walk the registry, which is why this
    /// answers `false` there instead of pretending to have done it.
    ///
    /// **Stop-the-world only** — see [`ZObjectStartBits::clear_all`].
    /// Clear the object-start registry's summary bit for every block the sweep
    /// left empty (nio21 wave 9 L3, `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS`).
    /// Returns the number of blocks forgotten; `0` when declined.
    ///
    /// The summary bits were set-only, so a swept nursery stretch stayed
    /// "noted" for the life of the heap and every pin probe above it walked
    /// its words (`HeapBitmap::forget_empty_blocks` has the cost argument).
    ///
    /// # When it is sound, and why it declines otherwise
    ///
    /// "A clear block bit proves the block is empty" survives the pass only if
    /// no registry word is written while it runs and every later writer notes
    /// the block before its word. Called from `collect_garbage` after the sweep,
    /// inside the `StopTheWorldToken`:
    ///
    /// * every `insert` path (`HeapBitmap::insert`, `insert_in_owned_chunk`,
    ///   `set_range`, the slide's re-registration) notes its block first;
    /// * the JIT's inline store does not, so the VM-TLAB carve notes the whole
    ///   chunk (`CRATONVM_ZGC_CARVE_NOTES_BLOCKS`, required by this switch),
    ///   and `collect_garbage` has already bumped the ownership epoch, so no
    ///   chunk carved before this pause can be inline-stored into again;
    /// * the one writer neither covers is a JIT thread OS-SUSPENDED (taken
    ///   over) between its inline epoch check and its store: it resumes and
    ///   completes the store into a block this pass may have just cleared.
    ///   Every cycle that took a peer over sets
    ///   `gc_quiescence::unrewritable_peer_state()` (the takeover is never
    ///   discharged), so the pass declines that cycle; the blocks are
    ///   forgotten at the next clean one.
    ///
    /// zr z1/al: counted (`forget_block_stats`), because the count used to be
    /// discarded at the call site and a pass that never runs (the switch off,
    /// or a take-over in every cycle) was indistinguishable from one that
    /// finds nothing to forget.
    pub fn forget_empty_registry_blocks(&self) -> usize {
        if !self.registry_forget_allowed() {
            self.counters
                .forget_block_declined
                .fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let forgotten = self.registry.forget_empty_blocks();
        self.counters
            .forget_block_passes
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .forget_blocks_total
            .fetch_add(forgotten, Ordering::Relaxed);
        forgotten
    }

    /// `(passes, declined, blocks_forgotten)` of
    /// [`Self::forget_empty_registry_blocks`] since the heap was built (zr
    /// z1/al). `passes + declined` is the collection count; `declined ==
    /// collections` means `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS` (or
    /// `CRATONVM_ZGC_CARVE_NOTES_BLOCKS`) is off or every cycle took a peer
    /// over, and no pin-probe saving can be claimed for the run.
    pub fn forget_block_stats(&self) -> (usize, usize, usize) {
        (
            self.counters.forget_block_passes.load(Ordering::Relaxed),
            self.counters.forget_block_declined.load(Ordering::Relaxed),
            self.counters.forget_blocks_total.load(Ordering::Relaxed),
        )
    }

    /// `(swept, bytes_swept, refused)` for the large-object sweep's oversize
    /// arrays (zr z1/al, `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS`; see
    /// `ZgcRealHeap::oversize_array_extent`). `swept == 0` on a run that drops
    /// arrays over 256 MiB means the switch is off; `refused > 0` means an
    /// extent check failed and that base is still retained.
    pub fn oversize_array_sweep_stats(&self) -> (usize, usize, usize) {
        (
            self.counters.oversize_arrays_swept.load(Ordering::Relaxed),
            self.counters
                .oversize_array_bytes_swept
                .load(Ordering::Relaxed),
            self.counters
                .oversize_arrays_refused
                .load(Ordering::Relaxed),
        )
    }

    /// `(tails_lowered, bytes_added)` by `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT`
    /// since the heap was built (zr z1/al): how many published VM-TLAB tails
    /// had their start lowered over unregistered bytes, and how many bytes
    /// that withheld from the sweeps beyond the tails as published. The
    /// cover's whole cost; zero on a run whose STW never published a tail.
    pub fn inflight_cover_stats(&self) -> (usize, usize) {
        (
            self.counters
                .inflight_covers_lowered
                .load(Ordering::Relaxed),
            self.counters.inflight_cover_bytes.load(Ordering::Relaxed),
        )
    }

    /// Clear the words of `bits` covering `ranges`: sparsely (only non-zero
    /// words are stored into) under `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`, the
    /// default, or with `clear_within`'s store-every-word pass at `=0`. Same
    /// final state; STOP-THE-WORLD ONLY, like both (zr z3/fp).
    pub fn clear_bitmap_within(&self, bits: &ZObjectStartBits, ranges: &[(usize, usize)]) {
        if self.counters.fp.sparse_clear_on.load(Ordering::Relaxed) {
            let skipped = bits.clear_within_sparse(ranges);
            self.counters
                .fp
                .sparse_words_skipped
                .fetch_add(skipped, Ordering::Relaxed);
        } else {
            bits.clear_within(ranges);
        }
    }

    #[must_use = "the header arm cannot clear in bulk and the caller must walk"]
    pub fn mark_clear_all(&self) -> bool {
        // THIS IS ALSO WHAT INVALIDATES THE PER-CHUNK BLACKENING, and it needs
        // no help to do it: a chunk blackened for the cycle just ended stops
        // being black because its bits are now zero, and
        // `allocate_black_if_marking`'s test reads the bits rather than a flag.
        // A watermark on the TLAB would have needed explicit invalidation
        // HERE -- at the end of a sweep, with every chunk still live and owned
        // by its thread -- and a missed one is an object swept while live.
        match &self.mark_bits {
            Some(bits) => {
                // BOUNDED, for the reason `live_bitmap_bounds` gives: a word
                // in the never-bumped middle is already zero and storing a
                // zero into it is work that buys nothing. Cheaper than the
                // snapshot's saving (a linear store, not an atomic load) but
                // paid on the same per-cycle schedule.
                if zgc_bitmap_bounds_enabled() {
                    // zr z3/fp (`CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`): store only
                    // into words a mark set, so the pages over garbage and
                    // holes stay unbacked instead of all being faulted in.
                    let bounds = self.live_bitmap_bounds();
                    self.clear_bitmap_within(bits, &bounds);
                    // zr z5/sw: reset the per-region liveness summary with
                    // the words it summarises, so the next sweep's region step
                    // sees only this coming cycle's claims. Same ranges, just
                    // cleared, still stop-the-world: the one place
                    // `forget_blocks_within` is sound. At `=0` the bits stay
                    // set-only (conservative) and the region step never fires.
                    if sweep::zgc_sweep_live_proportional_enabled()
                        && !self.generational_enabled.load(Ordering::Relaxed)
                    {
                        let n = bits.forget_blocks_within(Some(&bounds));
                        self.counters
                            .mark_summary_blocks_forgotten
                            .fetch_add(n, Ordering::Relaxed);
                    }
                    // Both bitmaps over this arena are clear now, so the
                    // retraction history has done its job: the next cycle's
                    // bound starts from the retracted cursor rather than from
                    // wherever this one peaked. Reset AFTER the clear -- doing
                    // it earlier hands the clear a bound that does not cover
                    // the bits it has to reach.
                    {
                        let mut arena = self.arena.lock();
                        arena.reset_low_high_water();
                        // BOTH ends, or the half that is not reset keeps a
                        // widened bound forever and the saving this file exists
                        // for decays back to a capacity-sized pass.
                        arena.reset_high_low_water();
                    }
                } else {
                    bits.clear_all();
                    if sweep::zgc_sweep_live_proportional_enabled()
                        && !self.generational_enabled.load(Ordering::Relaxed)
                    {
                        let n = bits.forget_blocks_within(None);
                        self.counters
                            .mark_summary_blocks_forgotten
                            .fetch_add(n, Ordering::Relaxed);
                    }
                }
                true
            }
            None => false,
        }
    }

    /// True iff the object at `base` is marked live by the current cycle's mark
    /// phase. `base == 0` (null) is treated as not live. Used as the
    /// `is_marked` predicate handed to the shared [`ReferenceProcessor`] so
    /// weak/soft/phantom clearing observes the exact liveness the trace
    /// computed.
    pub fn is_marked_addr(&self, base: usize) -> bool {
        if base == 0 {
            return false;
        }
        self.mark_is_set(base)
    }

    /// Arm the HIB-CV-24 reconciliation watch (see `zgc_reconcile_watch_map`)
    /// for the given addresses. Call before `collect_garbage`; an empty list
    /// is a no-op for `zgc_capture_reconcile_watch`, so callers that never
    /// watch anything pay nothing.
    pub fn zgc_set_reconcile_watch(&self, addrs: Vec<usize>) {
        let key = self as *const Self as usize;
        zgc_reconcile_watch_map().lock().insert(key, addrs);
    }

    /// Take back this cycle's captured exact verdicts (mark-bitmap-true,
    /// pre-slide) for the addresses armed via `zgc_set_reconcile_watch`.
    /// `None` if nothing was armed, or the watch was already consumed.
    pub fn zgc_take_reconcile_results(&self) -> Option<HashMap<usize, bool>> {
        let key = self as *const Self as usize;
        zgc_reconcile_watch_map().lock().remove(&key);
        zgc_reconcile_results_map().lock().remove(&key)
    }

    /// Record the exact `is_marked_addr` verdict for every watched address,
    /// at the same safe pre-slide point `process_references` reads from.
    /// Called from `collect_garbage` right alongside `process_references`.
    pub fn zgc_capture_reconcile_watch(&self) {
        let key = self as *const Self as usize;
        let Some(addrs) = zgc_reconcile_watch_map().lock().get(&key).cloned() else {
            return;
        };
        if addrs.is_empty() {
            return;
        }
        let results: HashMap<usize, bool> = addrs
            .into_iter()
            .map(|addr| (addr, self.is_marked_addr(addr)))
            .collect();
        zgc_reconcile_results_map().lock().insert(key, results);
    }

    /// This heap's [`census::ZSlotCensus`] — the reference-slot instrument.
    ///
    /// The handle a driver uses to arm the diagnostic
    /// (`heap.counters.slot_census().enable()`), label the run, and read the result back
    /// after a collection. It is disabled until something calls `enable()`, so
    /// exposing it costs a build nothing.
    pub fn slot_census(&self) -> &census::ZSlotCensus {
        &self.counters.slot_census
    }

    /// Print the reference-slot census summary, if the census ran.
    ///
    /// Called from [`ZgcRealHeap`]'s `Drop`, which is this file's end-of-run
    /// point; the collector's other `[GC] zgc-*:` summary lines are emitted by
    /// `VmHeap::print_gc_summary` over in `vm_heap.rs`. **If that function ever
    /// calls this one, delete the `Drop` call** — there is deliberately no
    /// one-shot latch here, because a latch would be per-process state in a
    /// per-VM object and `census.rs`'s header forbids exactly that.
    ///
    /// Gated on [`census::ZSlotCensus::is_enabled`] rather than printing zeroes:
    /// `format_summary` distinguishes "no walk completed" (`last_walk: NONE`)
    /// from "no slots found" (`VERDICT=no_data`), so an *enabled* run that
    /// stayed inert reports that, while a disabled run says nothing at all.
    /// Read `legacy_share` off the `last_walk` line, not the cumulative `walk`
    /// block — the cumulative one is lifetime-weighted and reads high on legacy.
    pub fn log_slot_census_summary(&self) {
        if self.counters.slot_census.is_enabled() {
            self.counters.slot_census.log_summary();
        }
    }

}
