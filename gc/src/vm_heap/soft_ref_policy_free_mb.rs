#[allow(unused_imports)]
use super::*;

impl VmHeap {
    /// Free-heap estimate in **megabytes** for the `SoftReference` LRU policy,
    /// deliberately keyed on *allocatable* rather than merely *unused* bytes.
    ///
    /// HotSpot's `LRUMaxHeapPolicy` clears a soft reference when it has been
    /// idle longer than `SoftRefLRUPolicyMSPerMB * free_heap_MB`, so this number
    /// is the entire pressure dimension of the policy: return a large value and
    /// nothing is ever cleared, return zero and everything is.
    ///
    /// **Why not simply `heap_capacity() - live_bytes_estimate()` on
    /// Generational.** Its young generation can run non-moving (a live JIT
    /// frame diverts the cycle to the in-place sweep — see `docs/GC.md`'s
    /// `divert_non_moving` table), and under a non-moving, fragmenting young
    /// generation "unused bytes" and "bytes an allocation can actually obtain"
    /// diverge without bound: a heap can be 60% unused and still fail a modest
    /// allocation because no single free run is large enough. A policy keyed
    /// on unused bytes then refuses to clear soft references precisely while
    /// allocation is failing — the worst possible time — and `OutOfMemoryError`
    /// is thrown with a heap full of reclaimable soft-reachable objects.
    ///
    /// So the Generational arm reports the free space of the generation that
    /// must satisfy the next allocation (young), floored at the old
    /// generation's headroom. It is intentionally *pessimistic*:
    /// under-reporting free space makes the policy clear soft references
    /// sooner, which costs cache hit rate; over-reporting makes it clear them
    /// never, which costs the process. Prefer the recoverable failure.
    ///
    /// # G1 and ZGC: whole-heap headroom, which is HotSpot's own figure
    ///
    /// Both have ONE allocation pool, so "the generation that satisfies the
    /// next allocation" is the whole heap and the answer is the maximum
    /// capacity less `allocated_bytes()` — exactly HotSpot's
    /// `LRUMaxHeapPolicy` (`MaxHeapSize - used_at_last_gc`). For G1 that is
    /// the `-Xmx` region grid, not the sizing policy's soft capacity.
    ///
    /// **The G1 arm was wrong until 2026-09-23, in the direction that clears
    /// every soft reference at every collection.** It went through the
    /// generational formula above with G1's `eden_stats` / `old_gen_stats`,
    /// whose CAPACITIES are "regions currently of that type × region size" —
    /// so Free regions, which ARE G1's headroom, were counted nowhere. After a
    /// young pause Eden is empty (capacity 0), the formula fell into its
    /// "no young space" arm and answered the unused tails of the Old regions:
    /// under a megabyte on a fresh 256 MiB heap, i.e. `0` MB, i.e. maximum
    /// pressure — `SoftRefLRUPolicyMSPerMB × 0` makes every soft reference idle
    /// for longer than the threshold, so every `SoftReference` cache under
    /// `-XX:+UseG1GC` was emptied by every pause whatever the heap occupancy.
    /// Pinned by `soft_ref_policy_free_mb_on_g1_counts_free_regions`.
    ///
    /// The ZGC arm already produced the whole-heap figure (its
    /// `young_gen_stats` is `(0, 0)` and its `old_gen_stats` is the whole
    /// arena); it is now stated directly rather than reached by accident.
    ///
    /// Consumers: `process_references_after_gc` and
    /// `g1_remark_process_references` (`vm/src/runtime/interpreter/gc_and_alloc.rs`)
    /// and the interpreter's soft-reference touch path. (This doc carried a
    /// "HANDOFF — no caller yet" section until 2026-09-23; the handoff had
    /// landed.)
    pub fn soft_ref_policy_free_mb(&self) -> usize {
        const MB: usize = 1024 * 1024;
        match &self.inner {
            VmHeapInner::Generational(_) => {}
            VmHeapInner::G1(h) => {
                // g1sz: the `-Xmx` grid, not `heap_capacity()`. Under the
                // adaptive sizing policy (default ON since 2026-10-26) the
                // latter is the soft capacity, which starts at `-Xms`, and
                // HotSpot's `LRUMaxHeapPolicy` measures against
                // `MaxHeapSize` whatever is committed: on a 64 MiB heap
                // that soft capacity left a few-KiB live set reporting 3 MB
                // free, and the policy clearing SoftReferences HotSpot keeps.
                let max = h.collector.heap_target_bytes_within(false);
                return max.saturating_sub(self.allocated_bytes()) / MB;
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => {
                return self.heap_capacity().saturating_sub(self.allocated_bytes()) / MB;
            }
        }
        let (young_used, young_cap) = self.young_gen_stats();
        // gcd d4/n (`gcd-d4n-soft-policy-free-mb-ignores-the-young-free-list`):
        // a non-moving young sweep frees onto the from-space free list and
        // never retracts the bump cursor, so `young_from_used()` after it
        // still reads "young is full" and the soft LRU saw ~0 MB free. The
        // free list is empty after a moving cycle, so that case is unchanged.
        let young_used = match &self.inner {
            VmHeapInner::Generational(h) => young_used.saturating_sub(h.young_from_free_bytes()),
            _ => young_used,
        };
        let (old_used, old_cap) = self.old_gen_stats();

        // Whole-heap headroom, as an upper bound.
        let total_free = (young_cap + old_cap).saturating_sub(young_used + old_used);

        // Allocation-relevant headroom. An allocation lands in young/eden; when
        // that space is exhausted a collection runs and the survivors must fit
        // in the old generation. Both must have room, so the binding constraint
        // is the smaller of the two.
        let young_free = young_cap.saturating_sub(young_used);
        let old_free = old_cap.saturating_sub(old_used);
        let allocatable = if young_cap == 0 {
            // Only Generational reaches this formula now (G1 and ZGC returned
            // above); a zero young capacity is not a state it produces, and
            // the old-gen pair is the right answer if it ever does.
            old_free
        } else {
            young_free.min(old_free)
        };

        // Never report more than the whole-heap headroom, and round DOWN: a
        // sub-megabyte remainder must read as 0 MB (maximum pressure), not 1.
        allocatable.min(total_free) / MB
    }

    // =====================================================================
    // GenerationalHeap-specific methods (no-ops for G1)
    // =====================================================================

    /// Could [`Self::satb_barrier`] retain a value right now? `false` only when
    /// the collector's SATB queue is known closed (G1, generational); ZGC
    /// answers `true`, its own gate being inside the barrier. For a caller that
    /// must do work BEFORE it can name the value to log (SBR4 satb: the
    /// `Reference.referent` keep-alive in `jit_getfield`), so the common
    /// no-cycle case pays one acquire load instead of that work.
    #[inline]
    pub fn satb_armed(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.satb_queue_ref().is_some_and(|q| q.is_active()),
            VmHeapInner::G1(h) => h.satb_queue().is_active(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => true,
        }
    }

    /// SATB barrier for concurrent marking.
    /// For G1, logs the old reference to the global SATB queue when marking is active.
    pub fn satb_barrier(&self, old_value: Value) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.satb_barrier(old_value),
            VmHeapInner::G1(h) => {
                if let Value::Object(Some(obj_ref)) = old_value {
                    h.satb_pre_barrier(obj_ref.as_ptr() as usize);
                }
            }
            // Phase 3 (2026-08-13): this arm was `{}`, and that empty body was
            // the whole of ZGC's missing mutator ingress. Every reference store
            // in this VM already reaches here for G1's sake, so the barrier
            // `zgc_concurrent.rs` describes as unwired needed no new call site
            // — it needed this arm. Inert while no cycle is marking: the ZGC
            // side is one relaxed load and a return.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                if let Value::Object(Some(obj_ref)) = old_value {
                    h.satb_pre_barrier(obj_ref.as_ptr() as usize);
                }
            }
        }
    }

    /// zt t23/zb7 (stage 2f blocker (c)): the `Reference.referent`
    /// keep-alive -- a referent the mutator just read out of a `Reference`
    /// (`Reference.get()` interpreted, compiled or native; JNI
    /// `GetObjectField` of the referent slot; `NativeContext::gc_reference_keep_alive`)
    /// stays alive for the running mark. G1 / generational: the SATB barrier,
    /// as before. ZGC: [`ZgcRealHeap::referent_keep_alive`], which under the
    /// color flip marks through the load barrier's mark step instead of the
    /// SATB pre-barrier and is otherwise exactly [`Self::satb_barrier`].
    ///
    /// zt t23/zb8: also the keep-alive of every other weakly held object the
    /// VM resolves and hands on -- weak JNI globals (`jobject_to_obj`,
    /// `resolve_global_handle_keeping_weak_alive`), the weak door's survivors
    /// (`WeakDoorArgs::repin`), weak layer-module handles, JDWP weak handles and
    /// interned-String pool hits -- so none of them publishes only through SATB.
    pub fn referent_keep_alive(&self, referent: Value) {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                if let Value::Object(Some(obj_ref)) = referent {
                    h.referent_keep_alive(obj_ref.as_ptr() as usize);
                }
            }
            _ => self.satb_barrier(referent),
        }
    }

    /// Round-5 fix (CRIT — UAF): drain THIS thread's per-thread SATB
    /// buffer into the global SATB queue.
    ///
    /// Must be called on every mutator thread immediately before it
    /// blocks at a GC safepoint, and on the GC-initiating thread before
    /// it runs initial-mark / remark. Without this drain, up to
    /// `DEFAULT_SATB_CAPACITY` (256) overwritten references per thread
    /// remain in the thread-local buffer and never reach the marker —
    /// causing the classic SATB lost-object scenario (A→B replaced by
    /// A→null after A is scanned but before B is scanned), which the
    /// next evacuation turns into a use-after-free on B.
    ///
    /// The `thread_local!` storage means each thread must call this
    /// itself; the GC cannot reach into another thread's buffer.
    pub fn flush_thread_satb(&self) {
        match &self.inner {
            VmHeapInner::G1(h) => {
                if h.satb_queue().is_active() {
                    cratonvm_gc_core::satb::flush_thread_satb_buffer(h.satb_queue());
                }
            }
            VmHeapInner::Generational(h) => {
                // Clone-free: this runs on EVERY JIT helper entry, and the
                // common case (no concurrent old-gen mark running) is a
                // single Acquire load — don't pay an Arc refcount round
                // trip just to check `is_active`.
                if let Some(q) = h.satb_queue_ref() {
                    if q.is_active() {
                        cratonvm_gc_core::satb::flush_thread_satb_buffer(q);
                    }
                }
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => {}
        }
    }

    /// Check if old generation needs GC (generational only).
    pub fn old_gen_needs_gc(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.old_gen_needs_gc(),
            VmHeapInner::G1(_) => false, // G1 manages its own concurrent marking
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// Get old generation base pointer and capacity (generational only).
    pub fn old_gen_info(&self) -> (usize, usize) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.old_gen_info(),
            VmHeapInner::G1(_) => (0, 0),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => (0, 0),
        }
    }

    /// gcj j2/og2 — the span a Generational concurrent cycle's mark bitmap
    /// covers (`GenerationalHeap::old_gen_mark_span`: base and growth limit).
    /// `(0, 0)` on G1 and ZGC, exactly as [`Self::old_gen_info`].
    pub fn old_gen_mark_span(&self) -> (usize, usize) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.old_gen_mark_span(),
            VmHeapInner::G1(_) => (0, 0),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => (0, 0),
        }
    }

    /// Lock the old generation for direct access (generational only).
    /// Returns None for G1.
    pub fn old_gen_lock(&self) -> Option<parking_lot::MutexGuard<'_, OldGen>> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.old_gen_lock()),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// Collect every young-gen object's reference to an old-gen object
    /// (mandatory concurrent old-gen marking roots — see
    /// `GenerationalHeap::collect_young_to_old_roots`). Generational only;
    /// empty for G1 (its concurrent marking has its own remembered sets).
    /// Must be called during a GC safepoint.
    pub fn collect_young_to_old_roots(&self) -> Vec<usize> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.collect_young_to_old_roots(),
            VmHeapInner::G1(_) => Vec::new(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => Vec::new(),
        }
    }

    /// Enable concurrent GC support (generational only).
    pub fn enable_concurrent_gc(
        &mut self,
        satb_queue: Arc<SatbQueue>,
        gc_state: Arc<ConcurrentGcState>,
    ) {
        match &mut self.inner {
            VmHeapInner::Generational(h) => {
                // gcj j2/cm2 (`CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`, opt-in;
                // `gcj-j1cm-proposal-dirty-thread-satb-walk`, Generational
                // half): the remark / cleanup walks of this heap's queue visit
                // only the threads that logged during the cycle. Before the
                // queue's first activation; G1's queue is never switched.
                {
                    satb_queue.enable_dirty_thread_walk();
                }
                h.enable_concurrent_gc(satb_queue, gc_state)
            }
            VmHeapInner::G1(_) => {} // G1 has built-in concurrent marking
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => {}
        }
    }

    /// gen r4w5/concmark5 (2026-09-24) — should the VM start the generational
    /// concurrent-cycle SERVICE thread (`ConcurrentGcState::serve`)? `true`
    /// only on the Generational backend with `CRATONVM_GEN_CONC_SERVICE_THREAD`
    /// set. G1 has its own controller (`G1State::concurrent_mark`); ZGC's
    /// marker threads are its own.
    pub fn wants_concurrent_service_thread(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) => ConcurrentGcState::service_enabled(),
            VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// gen r4w5/concmark5 — the generational concurrent cycle's trigger, for
    /// the service thread's periodic check (`ConcurrentServiceHooks::cycle_due`).
    /// On the service thread this is the policy's verdict; on any other thread
    /// with a service attached a due cycle is handed off and this is `false`
    /// (`GenerationalHeap::concurrent_cycle_due`). `false` off Generational.
    pub fn concurrent_cycle_due(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.concurrent_cycle_due(),
            VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// Probe whether a young-gen allocation would succeed, OR whether a young
    /// GC should be forced first to keep an evacuation reserve.
    ///
    /// Returning `None` routes the caller (the JIT alloc helpers'
    /// `if try_alloc_young_probe(..).is_none() { maybe_gc_forced }` path) into a
    /// young GC before the allocation is attempted.
    pub fn try_alloc_young_probe(&self, size: usize) -> Option<()> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.try_alloc_young_probe(size),
            // G1: JIT-compiled code never reaches the interpreter's `maybe_gc`
            // safepoint poll (which consults `needs_gc()`), so without a probe
            // here a JIT-heavy mutator allocates Eden right up to a 100%-full
            // heap before *any* young GC fires. At that point every region is
            // Eden (in the collection set) and ZERO Free regions remain for
            // to-space, so `evacuate_object` finds no destination — without the
            // self-forward net the whole CSet (incl. the live set) is reclaimed
            // (the SteadyChurn `-XX:+UseG1GC` + JIT OOM; the generational
            // collector hides it via its non-moving JIT-active sweep, which needs
            // no to-space). Mirror the interpreter's threshold: signal "collect
            // now" (`None`) while the evacuation reserve (Free regions) is still
            // intact, so the forced young GC has somewhere to evacuate. `size`
            // is unused for G1 — the reserve is region-granular (`needs_gc()` =
            // Free regions < 25%), not a byte-level bump check.
            VmHeapInner::G1(_) => {
                if self.needs_gc() {
                    None
                } else {
                    Some(())
                }
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => {
                if self.needs_gc() {
                    None
                } else {
                    Some(())
                }
            }
        }
    }

    /// O(1) probe: can the young-gen BUMP TAIL supply `size` bytes right now?
    ///
    /// Unlike [`Self::try_alloc_young_probe`], this never consults the young
    /// free list (`largest_free_block` is an O(free-blocks) scan — calling it
    /// per allocation from the JIT TLAB-refill gate was ~55% of a
    /// binarytrees-18 run once young fragmented). Used to decide whether a
    /// TLAB refill is worth attempting: a fragmented-but-full young (the
    /// non-moving-sweep steady state, whose cursor can never retreat) answers
    /// `false`, sending the caller to the old-gen spill path instead of
    /// scanning the free list on every allocation.
    pub fn young_bump_headroom(&self, size: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.young_bump_headroom(size),
            // G1's Eden is region-granular; reuse the reserve signal.
            VmHeapInner::G1(_) => !self.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => !self.needs_gc(),
        }
    }

    /// gcd d9/c: see `GenerationalHeap::young_starved_behind_blocked_drain`.
    /// G1 and ZGC have no promotion gate of this kind: `false`.
    /// (Lane d9/b applied d9/c's cross-lane request 2a; BUILD DEPENDENCY on
    /// d9/c's `02847aedd`, which defines the Generational method.)
    pub fn young_starved_behind_blocked_drain(&self, min_free: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.young_starved_behind_blocked_drain(min_free),
            VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// Amortized-O(1) probe: could a young TLAB refill of `size` bytes be
    /// served from RECLAIMED young space right now? See
    /// `GenerationalHeap::young_has_free_block` — early-exit scan behind the
    /// cached largest-block upper bound, safe to consult per allocation.
    /// Together with [`Self::young_bump_headroom`] this forms the JIT
    /// TLAB-refill gate.
    pub fn young_has_free_block(&self, size: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.young_has_free_block(size),
            // G1 refills carve whole-region chunks from Eden; the reserve
            // signal is the applicable "room without forcing a GC" answer.
            VmHeapInner::G1(_) => !self.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => !self.needs_gc(),
        }
    }

    /// zr z5/cr: ZGC's bytes allocated since the last collection
    /// (`ZgcRealHeap::bytes_allocated_since_cycle`: objects plus carved TLAB
    /// chunks in full, never an under-read). `None` on Generational and G1,
    /// so every caller is a no-op there.
    pub fn zgc_bytes_allocated_since_cycle(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some(h.bytes_allocated_since_cycle()),
        }
    }

    /// zr z5/cr: ZGC only -- after exactly one collection since the arena last
    /// refused a request, does a single span now serve that request
    /// (`ZgcRealHeap::room_for_last_refusal`)? `None` on Generational and G1,
    /// and on ZGC when there is no such record.
    pub fn zgc_room_for_last_refusal(&self) -> Option<bool> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.room_for_last_refusal(),
        }
    }

    /// zs s6/no: arm the calling thread's exhausted verdict after a ZGC
    /// allocation refusal's collection (`ZgcRealHeap::arm_exhausted_verdict`,
    /// opt-in `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`). `false` (and nothing
    /// armed) on Generational and G1, and on ZGC with the switch off.
    pub fn zgc_arm_exhausted_verdict(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.arm_exhausted_verdict(),
        }
    }

    /// zs s8/rv5: is the ZGC exhausted verdict switched on
    /// (`ZgcRealHeap::exhausted_verdict_enabled`)? `false` off ZGC.
    pub fn zgc_exhausted_verdict_enabled(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.exhausted_verdict_enabled(),
        }
    }

    /// zs s9/no2: exhausted verdicts spent (one per fill that a verdict
    /// ended; `ZgcRealHeap::exhausted_verdict_spent_count`); `None` off ZGC.
    pub fn zgc_exhausted_spent_count(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some(h.exhausted_verdict_spent_count()),
        }
    }

    /// zs s9/no2: `(capacity, free space the last collection left)` on ZGC
    /// (`ZgcRealHeap::free_after_last_cycle`); `None` on Generational and G1.
    pub fn zgc_free_after_last_cycle(&self) -> Option<(usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some((h.heap_capacity(), h.free_after_last_cycle())),
        }
    }

    /// zt t5/gn: is this a ZGC heap running its generational mode
    /// (`CRATONVM_ZGC_GENERATIONAL`)? `false` on Generational and G1, which
    /// are other collectors. One relaxed load.
    pub fn zgc_generational_enabled(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.generational_enabled(),
        }
    }

    /// zt t10/gn2: `(old generation bytes, bytes promoted)` after the last
    /// young cycle of a ZGC generational heap
    /// (`ZgcRealHeap::gen_last_young_xlog_figures`), for the `-Xlog` young
    /// table of a minor. `None` on Generational and G1 and off the mode.
    pub fn zgc_gen_last_young_xlog_figures(&self) -> Option<(usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.gen_last_young_xlog_figures(),
        }
    }

    /// zt t16/gn6 (item 12): did the last ZGC collection begin with an
    /// allocation failure latched -- HotSpot 25's `is_alloc_stalling_for_old`
    /// clause of `should_preclean_young` (`ZgcRealHeap::
    /// last_collection_after_alloc_failure`)? `false` on Generational and G1.
    pub fn zgc_last_collection_after_alloc_failure(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.last_collection_after_alloc_failure(),
        }
    }

    /// zt t10/gn2: did the last ZGC collection run whole-heap after the
    /// generational young arm of the pre-collection pass (consumed; see
    /// `ZgcRealHeap::take_armed_major_soft_clock_due`)? `false` on
    /// Generational and G1, so the caller's clock note is a no-op there.
    pub fn zgc_take_armed_major_soft_clock_due(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.take_armed_major_soft_clock_due(),
        }
    }

    /// gcj j4/rf: the Generational heap's
    /// `GenerationalHeap::next_pause_certainly_minor`; `false` on every other
    /// collector.
    pub fn gen_next_pause_certainly_minor(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.decide_next_pause_minor(),
            _ => false,
        }
    }

    /// zt t7/cc: `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` is on and the heap
    /// is ZGC in its generational mode. `false` on every other collector, so
    /// the reference pass's young-cycle arm is a no-op there.
    pub fn zgc_young_skip_references_on(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.young_skip_references_on(),
        }
    }

    /// zt t7/cc: the VM's pre-collection reference pass asks whether it may
    /// leave every referent in place for the collection about to run (a
    /// predicted young cycle; `ZgcRealHeap::arm_young_refs_strong`). `false`
    /// on every other collector.
    pub fn zgc_arm_young_refs_strong(&self, want: bool) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.arm_young_refs_strong(want),
        }
    }

    /// zt t11/gn3: is the ZGC collection about to run armed as a young cycle
    /// that keeps its referents (`ZgcRealHeap::young_refs_strong_armed`)?
    /// `false` on every other collector.
    pub fn zgc_young_refs_strong_armed(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.young_refs_strong_armed(),
        }
    }

    /// zs s6/no: drop the calling thread's exhausted verdict (an
    /// `OutOfMemoryError` is being built). No-op off ZGC.
    pub fn zgc_lift_exhausted_verdict(&self) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.lift_exhausted_verdict(),
        }
    }

    /// zt t2/rv: drop the calling thread's exhausted verdict WITHOUT spending
    /// it (`ZgcRealHeap::drop_exhausted_verdict`), for a VM-raised
    /// `OutOfMemoryError` that is not the heap refusal the verdict judged.
    /// No-op off ZGC.
    pub fn zgc_drop_exhausted_verdict(&self) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.drop_exhausted_verdict(),
        }
    }

    /// zs s10/sp: an `OutOfMemoryError` is being built for a refused
    /// allocation: arm the opt-in final-refusal rule
    /// (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`, `ZgcRealHeap::note_final_refusal`).
    /// No-op off ZGC and with the switch unset.
    pub fn zgc_note_final_refusal(&self) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_final_refusal(),
        }
    }

    /// zs s10/sp: `(threshold now, arms, disarms)` of that rule; `None` off ZGC.
    pub fn zgc_sub_request_span_stats(&self) -> Option<(usize, usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some(h.sub_request_span_stats()),
        }
    }

    /// zs s6/no: `(exhausted_armed, exhausted_refused)`; `None` off ZGC.
    pub fn zgc_exhausted_verdict_stats(&self) -> Option<(usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some(h.exhausted_verdict_stats()),
        }
    }

    /// Returns whether this heap is using the G1 collector.
    pub fn is_g1(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(_))
    }

    /// Returns whether this heap is the Generational collector — the only
    /// backend with a MOVING YOUNG generation.
    ///
    /// Exists because "does moving-young apply here?" was being answered by
    /// `conservative_roots::moving_young_enabled()`, which ANDs a JIT-side gate
    /// with `flags().gc.moving_young` and consults the collector in neither. G1
    /// evacuates by region and ZGC compacts by a whole-heap slide with its own
    /// per-cycle coverage proof — neither has a moving YOUNG generation — so on
    /// both of them the precise-moving-young question, and the unmemoised
    /// full-stack probe that answers it, is inert work. See the two
    /// `moving_young_precise_only` sites. (This said "ZGC never moves anything"
    /// until 2026-09-23; ZGC has compacted by default since 2026-08-13.)
    pub fn is_generational(&self) -> bool {
        matches!(&self.inner, VmHeapInner::Generational(_))
    }

    /// Is [`Self::is_object_address`] this backend's own marker root screen?
    ///
    /// ZGC only. Its marker skips any root or child that is not a registered
    /// allocation base (`ZgcRealHeap::collect_garbage`'s `registered.contains`,
    /// counted as `wild_skipped`), and `is_object_address` asks that same
    /// registry, so a candidate it refuses is never marked through: dropping it
    /// before the collection changes no mark. Generational and G1 answer with a
    /// header probe that can refuse a genuine young or mid-initialisation object
    /// (see `Frame::scan_locals_in`), so a refusal there proves nothing.
    pub fn object_start_is_the_mark_screen(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => true,
        }
    }

    /// Does this backend keep a conservatively-PINNED object at its address for
    /// the rest of the collection?
    ///
    /// # What asks, and why the answer is a capability rather than a policy
    ///
    /// The cross-thread JIT coverage handshake
    /// (`conservative_roots::refresh_moving_young_coverage_for_collection`)
    /// lets a moving cycle proceed while a peer thread holds compiled frames
    /// nobody could prove rewritable, provided those frames' conservative roots
    /// were PINNED instead. That discharge is sound exactly when the collector
    /// about to run honours the pin, and it is a property of the collector, not
    /// of the frames.
    ///
    /// * **G1** — `true`. It evacuates by region and withholds the regions named
    ///   by `gc_quiescence::pinned_jit_roots_snapshot()` from the collection set.
    /// * **ZGC** — `true`. Same shape at page granularity; `relocate_stw`
    ///   consumes the same snapshot.
    /// * **Generational** — **`false`, and it cannot be otherwise.** The young
    ///   collector is Cheney copying: from-space is reclaimed WHOLESALE, so
    ///   every live object in it moves by construction and there is no
    ///   "withhold this one" to implement. Consistently, `gen_heap.rs` and
    ///   `gen_evac.rs` contain no reader of the pin snapshot at all — the
    ///   pinned addresses reach that backend only as ROOTS, which keeps them
    ///   ALIVE and says nothing about keeping them PUT.
    ///
    /// That last distinction is the whole of this method. A peer's frame whose
    /// roots were "pinned" under the generational collector is a frame whose
    /// objects were faithfully kept alive at NEW addresses, with nothing having
    /// rewritten the frame — which is a stale compiled-frame reference, and was
    /// reproducible in ten seconds on the H2 JDBC corpus.
    pub fn honours_conservative_pins(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) => false,
            VmHeapInner::G1(_) => true,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => true,
        }
    }

    /// Check if G1 should start concurrent marking (IHOP threshold crossed).
    ///
    /// # LANE W3-C — this is the ONLY production consult of the IHOP policy
    ///
    /// Every caller of this function is a mark-cycle trigger
    /// (`interpreter::maybe_concurrent_gc`, `jit_drive_g1_concurrent_mark`),
    /// and it used to be the whole policy: two loads and a comparison,
    /// re-implemented here. `G1Collector::check_ihop` — which carries the
    /// mark-cycle back-off that wave 2 landed behind
    /// `CRATONVM_G1_IHOP_BACKOFF` — had **no caller outside tests**. A
    /// workspace grep for `check_ihop(` returned its definition, its doc
    /// citations, and `gc/src/g1.rs`'s own `mod tests` plus
    /// `gc/tests/g1_w2c_ihop_model.rs`. The flag was inert on every real run,
    /// so `backoff_declined_polls` read zero by construction and that zero was
    /// indistinguishable from "the back-off never needed to fire".
    ///
    /// That is `orchestrator-wave-1-measurements.md` §4 verbatim — a lever
    /// read on a path the workload does not take — arriving inside the wave
    /// that wrote §4 down.
    ///
    /// **The conjunction below is ordered so the DEFAULT build is unchanged.**
    /// With `CRATONVM_G1_IHOP_BACKOFF` off, `check_ihop` returns
    /// `old_bytes >= threshold`, which the second clause already implies, so
    /// the whole expression evaluates exactly as it did before. The only
    /// behaviour that changes is with the flag ON, which is what a flag is
    /// for. `check_ihop` is called last for the same reason: it is the only
    /// clause with a side effect (it counts the consult) and the cheap loads
    /// should short-circuit ahead of it.
    pub fn g1_should_start_marking(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(g1) => {
                // gci i22/sz: a humongous request that crossed IHOP asked for
                // this cycle (default-ON `CRATONVM_G1_HUMONGOUS_IHOP_START`; one
                // relaxed load otherwise). HotSpot's `G1 Humongous Allocation`
                // collection is the concurrent start by its cause, whatever
                // the old occupancy reads without the request.
                g1.humongous_conc_start_requested()
                    // Check if old gen bytes exceed the G1 marking threshold (IHOP)
                    || (g1.marking_threshold_bytes() > 0
                        && g1.old_gen_bytes() > g1.marking_threshold_bytes()
                        && g1.check_ihop())
            }
            VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    // =====================================================================
    // ZGC concurrent marking (2026-08-16)
    // =====================================================================
    //
    // Deliberately NOT folded into the `g1_*` predicates above. The two
    // collectors reach the same shape (open at a brief STW, trace with
    // mutators running, close at the next collection's STW) from opposite
    // sides -- G1 opens on an old-gen occupancy that only a young collection
    // updates, ZGC on total allocation, and G1 closes on a quiescence poll
    // while ZGC closes when the collection itself arrives. A shared predicate
    // would have to be a union of both, and the arm that did not apply would
    // be dead code that reads as coverage.

    /// Should a ZGC concurrent mark cycle open now?
    ///
    /// On the allocation path (`maybe_gc`), so the ZGC arm is two relaxed
    /// loads and the others are a compile-time-known `false`.
    #[inline]
    pub fn zgc_should_start_concurrent_mark(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.should_start_concurrent_mark(),
        }
    }

    /// zr z3/cs: is a ZGC concurrent cycle REQUESTED by `needs_gc` and waiting
    /// for a poll site to open it (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`)? `false`
    /// on the other backends and with the switch off.
    #[inline]
    pub fn zgc_conc_start_requested(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.conc_start_requested(),
        }
    }

    /// zr z3/cs: the VM ran a ZGC mark-start pause (`opened`) or refused one.
    /// A no-op on the other backends and with the switch off.
    pub fn zgc_note_conc_start_outcome(&self, opened: bool) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_conc_start_outcome(opened),
        }
    }

    /// zr zs s1/tp: the VM dropped a ZGC mark start before requesting its
    /// pause because the start was no longer due. A no-op on the other
    /// backends.
    pub fn zgc_note_conc_start_skipped(&self) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_conc_start_skipped(),
        }
    }

    /// zt t24/hunt7 (`CRATONVM_ZGC_OOM_LADDER_FRESH_MARK`): did the last ZGC
    /// collection close a concurrent cycle with that cycle's mark set? `false`
    /// on the other backends ([`ZgcRealHeap::last_collection_marked_concurrently`]).
    pub fn zgc_last_collection_marked_concurrently(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.last_collection_marked_concurrently(),
        }
    }

    /// Is a ZGC concurrent mark cycle in flight?
    #[inline]
    pub fn zgc_concurrent_mark_active(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.concurrent_mark_active(),
        }
    }

    /// zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`): has the open ZGC
    /// concurrent cycle's marker reached its fixed point? `false` with no
    /// cycle open and on the other backends.
    pub fn zgc_concurrent_mark_caught_up(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.concurrent_mark_caught_up(),
        }
    }

    /// zt t18/zc1: a ZGC `System.gc()` request asks for a concurrent cycle of
    /// its own (`ZgcRealHeap::request_system_gc_concurrent_cycle`). `false`:
    /// nothing was recorded (switch off, not the every-trigger mode, a cycle
    /// already open, another backend) and the request collects as before.
    pub fn zgc_request_system_gc_concurrent_cycle(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.request_system_gc_concurrent_cycle(),
        }
    }

    /// zt t18/zc1: the mark start of a ZGC `System.gc()` request ran; `opened`
    /// = a cycle is open now. A no-op on the other backends.
    pub fn zgc_note_system_gc_concurrent_cycle(&self, opened: bool) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_system_gc_concurrent_cycle(opened),
        }
    }

    /// zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): the collecting
    /// door will run a deferred sweep after it releases the world, so the
    /// collection it is about to run may park its sweep. Call inside the
    /// pause, right before the collection. A no-op off ZGC and with the switch
    /// off.
    pub fn zgc_arm_concurrent_sweep_runner(&self) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.arm_concurrent_sweep_runner(),
        }
    }

    /// zt t21/zc2: run the sweep the collection just parked, on this (the
    /// collecting) thread, with the world released. `true` when the
    /// collection deferred its sweep (zt t22/x18: whoever ran it -- this call
    /// or a runner that took it first, `ZgcRealHeap::run_pending_sweep_after_release`):
    /// the heap's after-collection figures changed, and the caller re-samples
    /// them before it reports the collection.
    pub fn zgc_run_pending_sweep_after_release(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.run_pending_sweep_after_release(),
        }
    }

    /// zt t33: the collection's pause ends: heals and colored stores write the
    /// good color again (`ZgcRealHeap::end_closing_heal_color`). Called by the
    /// VM right before it releases the mutators. A no-op off ZGC.
    pub fn zgc_thread_cpu_nanos(&self) -> Option<u64> {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => cratonvm_gc_zgc::zgc::mark::thread_cpu_nanos(),
        }
    }

    pub fn zgc_end_closing_heal_color(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.end_closing_heal_color();
        }
    }

    /// zt t32 (R6, `CRATONVM_ZGC_CONCURRENT_RELOCATE`): the collecting door's
    /// drain after its release -- copy every planned object no barrier copied
    /// yet (`ZgcRealHeap::run_concurrent_relocate_drain`). `false` (one lock)
    /// when nothing is planned, and off ZGC.
    pub fn zgc_run_concurrent_relocate_drain(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.run_concurrent_relocate_drain(),
        }
    }

    /// zt t31/zc11 (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`): may this
    /// collection's VM reference round run after the release, before the
    /// parked sweep's install? Asked inside the pause; `false` off ZGC.
    pub fn zgc_reference_round_may_follow_release(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.reference_round_may_follow_release(),
        }
    }

    /// zt t31/zc11: the VM kept a round the heap allowed in the pause.
    pub fn zgc_note_reference_round_kept_by_vm(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.note_reference_round_refusal(cratonvm_gc_zgc::zgc::ZRefRoundRefusal::Vm);
        }
    }

    /// zt t31/zc11: hold the parked sweep's install for the round (inside the
    /// pause).
    pub fn zgc_hold_sweep_install(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.hold_sweep_install();
        }
    }

    /// zt t31/zc11: the round ended (`round_us`, `0` on an unwind).
    pub fn zgc_release_sweep_install_hold(&self, round_us: u64) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.release_sweep_install_hold(round_us);
        }
    }

    /// zt t21/zc2: a `System.gc()` caller returns only after the whole cycle
    /// (HotSpot's synchronous request): finish a pending sweep, or wait for
    /// the thread running it. One load when nothing is pending.
    pub fn zgc_finish_pending_sweep_for_waiter(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.finish_pending_sweep(cratonvm_gc_zgc::zgc::ZSweepRunner::Waiter),
        }
    }

    /// zt t23/x19 (page item 28d; `CRATONVM_ZGC_EXIT_FINISHES_SWEEP`, default
    /// ON, `=0` kill): the VM's exit finishes a pending deferred sweep (or
    /// waits for its runner) before it reads the heap it reports
    /// (`ZgcRealHeap::finish_pending_sweep_at_exit`). Called by the exit
    /// readers themselves -- the `-Xlog:gc+exit` heap lines, the `[GC]`
    /// summary, the end-of-run metrics row -- so every launcher arm gets it.
    /// One load when nothing is pending; `false` off ZGC.
    pub fn zgc_finish_pending_sweep_at_exit(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.finish_pending_sweep_at_exit(),
        }
    }

    /// zt t21/zc2: the live bytes the last installed deferred sweep left,
    /// without what the mutators allocated after its pause (`None` off ZGC,
    /// with the switch off, or before the first install).
    pub fn zgc_concurrent_sweep_last_live(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.concurrent_sweep_last_live(),
        }
    }

    /// zr z1/rf (`CRATONVM_ZGC_CONC_REF_SKIP`): hand the VM's active Weak /
    /// Phantom `Reference` objects to the ZGC concurrent mark cycle about to
    /// open (`ZgcRealHeap::set_concurrent_vm_reference_skip`). A no-op on G1
    /// and Generational, which have their own skip sets.
    pub fn zgc_set_concurrent_reference_skip(&self, addrs: Vec<usize>) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.set_concurrent_vm_reference_skip(addrs),
        }
    }

    /// zr z2/cc (`CRATONVM_ZGC_CONC_REF_SKIP_SOFT`): hand the VM's active Soft
    /// `Reference` objects to the ZGC concurrent mark cycle about to open
    /// (`ZgcRealHeap::set_concurrent_vm_soft_reference_skip`: hidden for the
    /// concurrent phase, kept referents re-rooted at the mark end). A no-op on
    /// G1 and Generational.
    pub fn zgc_set_concurrent_soft_reference_skip(&self, addrs: Vec<usize>) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.set_concurrent_vm_soft_reference_skip(addrs),
        }
    }

    /// zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in): may the
    /// pre-collection pass HIDE the referent slots of this collection instead
    /// of nulling them (`ZgcRealHeap::concurrent_refs_skip_mode`)? `false`
    /// off ZGC, with the switch off and in generational mode. Ask once per
    /// collection, inside the pause.
    pub fn zgc_concurrent_refs_skip_mode(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.concurrent_refs_skip_mode(),
        }
    }

    pub fn zgc_persistent_skip_bits_enabled(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.persistent_skip_bits_enabled(),
        }
    }

    #[inline]
    pub fn record_zgc_discovered_weak_phantom(&self, base: usize, class_id: u32, kind_code: u8) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.record_discovered_weak_phantom(base, class_id, kind_code);
        }
        #[cfg(not(feature = "zgc"))]
        let _ = (base, class_id, kind_code);
    }

    #[inline]
    pub fn note_zgc_weak_phantom_class(&self, class_id: u32, kind_code: u8) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.note_weak_phantom_class(class_id, kind_code);
        }
        #[cfg(not(feature = "zgc"))]
        let _ = (class_id, kind_code);
    }

    pub fn take_zgc_discovered_weak_phantom(&self) -> Vec<(usize, u32, u8, Option<usize>, bool)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            return h.take_discovered_weak_phantom();
        }
        Vec::new()
    }

    pub fn zgc_attach_persistent_skip_bits(&self, rp: &mut crate::ReferenceProcessor) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            rp.attach_persistent_skip_bits(h.persistent_skip_bits_arc());
        }
        #[cfg(not(feature = "zgc"))]
        let _ = rp;
    }

    /// zt t22/zc3: hand the rows the pre-collection pass hid to the collection
    /// about to run (`ZgcRealHeap::set_stw_vm_reference_skip`). A no-op off
    /// ZGC.
    pub fn zgc_set_stw_reference_skip(&self, addrs: Vec<usize>) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.set_stw_vm_reference_skip(addrs),
        }
    }

    /// zt t22/zc3: forget the collection's hidden rows (the door, after the
    /// collection). A no-op off ZGC.
    pub fn zgc_clear_stw_reference_skip(&self) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.clear_stw_vm_reference_skip(),
        }
    }

    /// zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): run the referent clear
    /// the collection staged (`ZgcRealHeap::run_staged_referent_clear`). A
    /// no-op off ZGC and when nothing is staged.
    pub fn zgc_run_staged_referent_clear(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.run_staged_referent_clear();
        }
    }

    pub fn zgc_is_marked_addr(&self, addr: usize) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            return h.is_marked_addr(addr);
        }
        let _ = addr;
        false
    }

    /// zt t32: the referent read gate's state (`ZgcRealHeap::referent_gate_state`);
    /// `ZGC_REFERENT_GATE_NEVER` (0) off ZGC.
    #[inline]
    pub fn zgc_referent_gate_state(&self) -> u8 {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.referent_gate_state(),
            _ => 0,
        }
    }

    pub fn zgc_referent_gate_state_ptr(&self) -> usize {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.referent_gate_state_ptr(),
            _ => 0,
        }
    }

    /// j27 wave 5 (lane REFS): can the referent read gate ever arm on this
    /// heap (`ZgcRealHeap::referent_gate_may_arm`)? `false` off ZGC.
    pub fn zgc_referent_gate_may_arm(&self) -> bool {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.referent_gate_may_arm(),
            _ => false,
        }
    }

    /// zt t32: is a referent read blocked by the armed gate
    /// (`ZgcRealHeap::referent_read_blocked`)? `false` off ZGC.
    pub fn zgc_referent_read_blocked(&self, receiver: usize, referent: usize) -> bool {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.referent_read_blocked(receiver, referent),
            _ => false,
        }
    }

    /// zt t22/zc3 (the census): the door's laps of this collection's VM
    /// reference round, in microseconds (`ZgcRealHeap::note_vm_reference_round`).
    /// A no-op off ZGC.
    pub fn zgc_note_vm_reference_round(&self, pre_us: u64, post_us: u64, jni_us: u64) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {
                let _ = (pre_us, post_us, jni_us);
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_vm_reference_round(pre_us, post_us, jni_us),
        }
    }

    /// zt t31/zc11 (1c step 2c): the census note of a round whose body ran
    /// after the release (`ZgcRealHeap::note_vm_reference_round_after_release`).
    /// A no-op off ZGC.
    pub fn zgc_note_vm_reference_round_after_release(
        &self,
        pre_us: u64,
        post_us: u64,
        jni_us: u64,
        after_us: u64,
    ) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {
                let _ = (pre_us, post_us, jni_us, after_us);
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                h.note_vm_reference_round_after_release(pre_us, post_us, jni_us, after_us)
            }
        }
    }

    /// zr z1/rf: can the interned-String table be a WEAK root on this heap?
    /// `true` only on a ZGC heap whose every reclamation is a stop-the-world
    /// whole-heap collection followed, in the same pause, by the VM's
    /// `update_all_roots` epilogue: concurrent marking is not configured
    /// (`CRATONVM_ZGC_CONC_START` unset / `0`: the trigger is latched at
    /// construction) and generational mode is off. A concurrent mark would
    /// need a keep-alive barrier on pool hits (a mutator could fetch an
    /// unmarked pooled String and hide it in a scanned object), and a minor
    /// cycle's young sweep was not reviewed for the table's verdicts. `false`
    /// on G1 and Generational (their gate is the VM's own).
    #[inline]
    pub fn zgc_weak_string_table_sound(&self) -> bool {
        self.zgc_weak_string_table_sound_for(false)
    }

    /// zt t27/gn5 (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`, the gn page's item
    /// 13): [`Self::zgc_weak_string_table_sound`], with the generational mode
    /// admitted when `generational_ok` (the VM's switch). Sound there because
    /// every collection of the mode is stop-the-world and ends in the same
    /// `update_all_roots` sweep, whose ZGC verdict is the exact start registry
    /// (`addr_keyed::survived_in_place` -> `ZgcRealHeap::is_object_address`):
    /// a minor sweeps only NON-old bases (`sweep_young_aged_words`: `registered
    /// & !old`), so an old pooled String the minor did not trace stays
    /// registered and its entry stays (only a major, which traces the old
    /// generation, can prove it dead), while a dead young one is unregistered
    /// by the minor that freed it and its entry is dropped.
    #[inline]
    pub fn zgc_weak_string_table_sound_for(&self, generational_ok: bool) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {
                let _ = generational_ok;
                false
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                h.adaptive_conc_start_state().2 == 0
                    && !h.concurrent_mark_active()
                    && (generational_ok || !h.generational_enabled())
            }
        }
    }

    /// zt t23/st1 (`CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT`): the
    /// interned-String table's soundness test once the VM supplies the three
    /// pieces a concurrent mark needs (`vm/src/memory/string_table.rs`,
    /// module doc "Concurrent ZGC"): the mark start's and the closing
    /// collection's root scans leave the pool out, every pool hit while a
    /// cycle marks goes through the SATB keep-alive, and the closing
    /// collection's `update_all_roots` sweeps the pool under the mark's
    /// verdict. Then only generational mode is refused (a minor cycle's young
    /// sweep was not reviewed for the table's verdicts). `false` on G1 and
    /// Generational, as [`Self::zgc_weak_string_table_sound`].
    #[inline]
    pub fn zgc_weak_string_table_sound_under_concurrent_mark(&self) -> bool {
        self.zgc_weak_string_table_sound_under_concurrent_mark_for(false)
    }

    /// zt t27/gn5: [`Self::zgc_weak_string_table_sound_under_concurrent_mark`]
    /// with the generational mode admitted when `generational_ok`
    /// (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`; see
    /// [`Self::zgc_weak_string_table_sound_for`]). A young cycle is never
    /// predicted or run while a concurrent cycle is open
    /// (`ZgcRealHeap::young_cycle_predicted` refuses on `conc_cycle_active`),
    /// so a concurrent major of the mode is the non-generational case: its two
    /// scans leave the pool out, its hits are kept alive, its closing pause
    /// sweeps.
    #[inline]
    pub fn zgc_weak_string_table_sound_under_concurrent_mark_for(
        &self,
        generational_ok: bool,
    ) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {
                let _ = generational_ok;
                false
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => generational_ok || !h.generational_enabled(),
        }
    }

    /// Open a ZGC concurrent mark cycle at a brief stop-the-world pause.
    ///
    /// `roots` must be the COMPLETE root set -- this thread, every parked
    /// peer's snapshot, and the conservative roots of any forcibly-stopped
    /// in-JIT peer. A root missed here is an object the concurrent phase never
    /// traces, and the mark-end re-scan only covers roots that still exist
    /// then. Returns `true` iff a cycle opened.
    pub fn zgc_start_concurrent_mark(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &[ObjectRef],
    ) -> bool {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                let addrs: Vec<u64> = roots.iter().map(|r| r.as_ptr() as u64).collect();
                h.start_concurrent_mark(stw, &addrs)
            }
        }
    }

    /// Abandon an open ZGC concurrent cycle, discarding its mark bits.
    pub fn zgc_abandon_concurrent_mark(&self) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.abandon_concurrent_mark(),
        }
    }

    /// `(started, completed, black_allocations, ingress_replayed, phase_nanos)`
    /// for the ZGC concurrent marker; all zeros on the other backends.
    pub fn zgc_concurrent_mark_stats(&self) -> (usize, usize, usize, usize, u64) {
        match &self.inner {
            VmHeapInner::G1(_) | VmHeapInner::Generational(_) => (0, 0, 0, 0, 0),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.concurrent_mark_stats(),
        }
    }

    /// Check if G1 concurrent marking is currently active.
    pub fn g1_is_marking_active(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.gc_state.is_marking_active(),
            VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_FUTILE_YOUNG_ESCALATION`):
    /// has G1 taken enough consecutive young pauses on a starved Free pool,
    /// with a mark cycle open, that the mark driver should finish the cycle
    /// now? Consumes the streak. See `G1Collector::take_futile_young_escalation`.
    /// The other backends have no G1 mark cycle to finish.
    pub fn g1_take_futile_young_escalation(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.take_futile_young_escalation(),
            VmHeapInner::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// r23 w6 (`CRATONVM_G1_STARVED_FULL`): should the mark driver abort the
    /// open cycle and run G1's starved full now? Consumes the streak. See
    /// `G1Collector::take_starved_full`. G1 only.
    pub fn g1_take_starved_full(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(g1) if g1.take_starved_full())
    }

    /// r23 w6: arm the starved full's pause (`G1Collector::arm_starved_full`).
    /// G1 only; `false` when nothing was armed.
    pub fn g1_arm_starved_full(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(g1) if g1.arm_starved_full())
    }

    /// The G1 near-full proxy row: an in-place-only full compaction for the
    /// next pause (`G1Collector::arm_in_place_compaction`), for the
    /// allocation-failure ladder when [`Self::g1_arm_full_compaction`]
    /// refused. `false` off G1.
    pub fn g1_arm_in_place_compaction(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(g1) if g1.arm_in_place_compaction())
    }

    /// The R22 aastore-humongous page: see
    /// `G1Collector::arm_fragmented_humongous_slide`. `None` off G1.
    pub fn g1_arm_fragmented_humongous_slide(&self) -> Option<(usize, usize, usize)> {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.arm_fragmented_humongous_slide(),
            _ => None,
        }
    }

    /// gcki-ngr: see `G1Collector::failed_humongous_request_fits`. `false`
    /// off G1.
    pub fn g1_failed_humongous_request_fits(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(g1) if g1.failed_humongous_request_fits())
    }

    /// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): the address ranges
    /// this heap's NEXT collection treats as young, for the VM's split root
    /// gather (`ThreadRegistry::arm_parked_split_gather`), or `None` when the
    /// heap cannot take an old-only root section for that collection. Asked
    /// inside the pause, right before the gather.
    ///
    /// The contract a `Some` answer makes: the next collection either follows
    /// no root that lies outside these ranges (a young-only phase: old -> young
    /// edges come from the card table / remembered sets), or adds the section
    /// handed to [`Self::stage_parked_old_only_roots`] back to its root set
    /// first.
    ///
    /// * Generational: both semispaces, unless a concurrent old mark is open
    ///   (SATB armed: its initial mark and remark are separate doors, but the
    ///   split is not argued against an open snapshot). A same-pause major
    ///   (moving Phase 5, the non-moving old sweep) consumes the staged
    ///   section (`GenerationalHeap::parked_old_only_young_ranges`).
    /// * G1: every region that is not `Old` (eden, survivor, humongous, free:
    ///   the eager humongous reclaim reads the root set), and only when the
    ///   pause is provably young-only and no mark is open; a G1 pause never
    ///   consumes a staged section (`G1Collector::parked_old_only_young_ranges`).
    /// * ZGC: never (out of scope).
    pub fn parked_old_only_young_ranges(&self) -> Option<Vec<(usize, usize)>> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.parked_old_only_young_ranges(),
            VmHeapInner::G1(g1) => g1.parked_old_only_young_ranges(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// P3 item 1: on G1, the `Old` regions' address ranges when the next pause is
    /// provably young-only ([`G1Collector::young_pause_old_ranges`]); `None`
    /// otherwise and on every other collector.
    pub fn g1_young_pause_old_ranges(&self) -> Option<Vec<(usize, usize)>> {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.young_pause_old_ranges(),
            _ => None,
        }
    }

    /// gci i13/vg: the old-only root section of this pause's split gather
    /// (see [`Self::parked_old_only_young_ranges`]). Generational keeps it for
    /// the collection that follows and clears it when that collection ends;
    /// G1 answered the ranges only for a young-only pause, so it has no phase
    /// that needs the section and drops it; ZGC never arms a split.
    pub fn stage_parked_old_only_roots(&self, old_only: Vec<ObjectRef>) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.stage_parked_old_only_roots(&old_only),
            VmHeapInner::G1(_) => drop(old_only),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => drop(old_only),
        }
    }

    /// Preserve a proved fragmented humongous request for the very next
    /// allocation-failure compaction pause.  The caller invokes this only
    /// after a synchronous final remark + cleanup; normal G1 pauses never
    /// acquire a full-compaction goal.
    pub fn g1_arm_fragmented_humongous_compaction(&self) {
        if let VmHeapInner::G1(g1) = &self.inner {
            g1.arm_fragmented_humongous_compaction();
        }
    }

    /// The G1 near-full proxy-define page: the allocation ladder's final
    /// attempt may take one reserve region (`G1Collector::arm_reserve_last_chance`).
    /// A no-op off G1.
    pub fn g1_arm_reserve_last_chance(&self) {
        if let VmHeapInner::G1(g1) = &self.inner {
            g1.arm_reserve_last_chance();
        }
    }

    /// The G1 near-full proxy-define page: the address ranges the last G1
    /// pause slid in place (`G1Collector::last_pause_slid_ranges`); empty off
    /// G1 and after any pause that did not slide.
    pub fn g1_last_pause_slid_ranges(&self) -> Vec<(usize, usize)> {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.last_pause_slid_ranges(),
            _ => Vec::new(),
        }
    }

    /// gcd d10/o: arm one G1 full-compaction pause for the next collection
    /// (see `G1Collector::arm_full_compaction`). The caller has just run a
    /// synchronous final remark + cleanup. `false` off G1, or when that
    /// cleanup did not trust its closure.
    pub fn g1_arm_full_compaction(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(g1) if g1.arm_full_compaction())
    }

    /// JIT round 22 wave 11 (lane memory): [`Self::g1_arm_full_compaction`] as
    /// the OOME path's maximal pass (`G1Collector::arm_full_compaction_maximal`).
    pub fn g1_arm_full_compaction_maximal(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(g1) if g1.arm_full_compaction_maximal())
    }

    /// Whether G1 holds an allocation-failure compaction request no pause has
    /// consumed yet: a staging pause re-arms it for one more recovery pause.
    pub fn g1_humongous_compaction_pending(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.humongous_compaction_pending(),
            _ => false,
        }
    }

    /// True only after G1 has observed a humongous allocation refusal caused
    /// by fragmentation rather than aggregate exhaustion.
    #[inline]
    pub fn g1_has_fragmented_humongous_request(&self) -> bool {
        matches!(&self.inner, VmHeapInner::G1(g1) if g1.has_fragmented_humongous_request())
    }

    /// Start a G1 concurrent mark cycle: activate SATB, clear bitmaps,
    /// and spawn the background [`ConcurrentMarkController`].
    ///
    /// Task #56: the controller is parked in `G1State::concurrent_mark`
    /// and joined later by [`Self::g1_signal_marking_complete`].
    ///
    /// Edge case (re-entry): if a controller is already parked from a
    /// previous unfinished cycle, we log a warning and join the stale
    /// worker before restarting. Skipping the new cycle would leave a
    /// running worker racing with the bitmap clear below.
    pub fn g1_start_concurrent_mark(&self, stw: &cratonvm_gc_core::collector::StopTheWorldToken) {
        self.g1_start_concurrent_mark_with_reference_skip(stw, &[]);
    }

    /// [`Self::g1_start_concurrent_mark`] with the cycle's referent-slot skip
    /// set published inside `G1Collector::start_concurrent_mark_with_reference_skip`,
    /// i.e. before the overlay snapshot grays anything and before the marker
    /// is spawned (gci i14/gq G-4). `reference_objs` empty = the plain door
    /// (the caller then publishes with [`Self::g1_set_reference_skip_set`]).
    pub fn g1_start_concurrent_mark_with_reference_skip(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        reference_objs: &[usize],
    ) {
        self.g1_start_concurrent_mark_inner(stw, reference_objs, false);
    }

    /// g1ep7: [`Self::g1_start_concurrent_mark_with_reference_skip`] for the
    /// concurrent cycle: its young regions are root regions
    /// (`G1Collector::start_concurrent_mark_with_root_regions`).
    pub fn g1_start_concurrent_mark_with_root_regions(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        reference_objs: &[usize],
    ) {
        self.g1_start_concurrent_mark_inner(stw, reference_objs, true);
    }

    pub(in crate::vm_heap) fn g1_start_concurrent_mark_inner(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        reference_objs: &[usize],
        root_regions: bool,
    ) {
        if let VmHeapInner::G1(state) = &self.inner {
            let mut slot = state.concurrent_mark.lock();
            if let Some(stale) = slot.take() {
                tracing::warn!(
                    "g1_start_concurrent_mark: prior controller still parked \
                     ({} steps) — joining before restart",
                    stale.steps_performed(),
                );
                drop(slot); // release before potentially-blocking join
                let _ = stale.request_stop_and_join();
                slot = state.concurrent_mark.lock();
            }
            // Order matters: phase must be ConcurrentMark before the
            // worker starts stepping.
            if root_regions {
                state
                    .collector
                    .start_concurrent_mark_with_root_regions(stw, reference_objs);
            } else {
                state
                    .collector
                    .start_concurrent_mark_with_reference_skip(stw, reference_objs);
            }
            *slot = Some(ConcurrentMarkController::spawn(Arc::clone(
                &state.collector,
            )));
        }
    }

    /// INT-8: publish the referent-slot skip set for the cycle that
    /// [`Self::g1_start_concurrent_mark`] just opened — the Weak/Soft/Phantom
    /// `Reference` OBJECT addresses from the VM's reference registry,
    /// snapshotted inside the same initial-mark STW. Must run BEFORE
    /// [`Self::g1_mark_roots`] seeds the gray set (the skip set gates how
    /// Reference objects are scanned). No-op on other backends.
    pub fn g1_set_reference_skip_set(&self, addrs: &[usize]) {
        if let VmHeapInner::G1(state) = &self.inner {
            state.collector.set_reference_skip_set(addrs);
        }
    }

    /// Mark roots into G1's mark bitmap.
    ///
    /// I-17: `remark` is an STW phase, so this wrapper takes the witness too
    /// rather than fabricating one — the caller is inside the initial-mark
    /// pause and already holds it.
    pub fn g1_mark_roots(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &[cratonvm_types::ObjectRef],
    ) {
        if let VmHeapInner::G1(g1) = &self.inner {
            g1.remark(stw, roots); // remark marks roots + drains SATB
                                   // The worker (spawned by `g1_start_concurrent_mark` just before
                                   // this) may have already drained the initially-empty worklist and
                                   // parked with `quiesced=true`. These roots seed real work, so wake
                                   // it and clear the premature quiescence — otherwise the completion
                                   // poll could fire before the seeded graph is marked.
            if let Some(ctrl) = g1.concurrent_mark.lock().as_ref() {
                ctrl.notify_work_available();
            }
        }
    }

    /// gci i12/wb: inside the concurrent-start pause, after
    /// [`Self::g1_mark_roots`], scan every object the takeover's frozen peers
    /// hold (`G1Collector::scan_frozen_held_objects`, which has the argument),
    /// and wake the worker for the grays it pushed. `0` off G1.
    pub fn g1_scan_frozen_held_objects(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        frozen: &[cratonvm_types::ObjectRef],
    ) -> usize {
        if let VmHeapInner::G1(g1) = &self.inner {
            let scanned = g1.scan_frozen_held_objects(stw, frozen);
            if scanned > 0 {
                if let Some(ctrl) = g1.concurrent_mark.lock().as_ref() {
                    ctrl.notify_work_available();
                }
            }
            return scanned;
        }
        0
    }

    /// Perform one step of G1 concurrent marking. Returns true when done.
    pub fn g1_concurrent_mark_step(&self, work_amount: usize) -> bool {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.concurrent_mark_step(work_amount),
            VmHeapInner::Generational(_) => true,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => true,
        }
    }

    /// Probe whether the G1 concurrent-mark controller (if any) has
    /// exhausted its worklist and the worker thread has exited. Used
    /// by the coordinator to schedule `g1_signal_marking_complete`
    /// once natural completion is reached, without holding the
    /// controller in a blocking join. Returns `true` when there is no
    /// active controller (vacuously finished).
    pub fn g1_concurrent_mark_finished(&self) -> bool {
        if let VmHeapInner::G1(state) = &self.inner {
            let slot = state.concurrent_mark.lock();
            // Completion = the worker has marked to a FIXED POINT (`is_quiesced`),
            // NOT that the worker thread exited (`!is_running`). The worker parks
            // (stays alive) at a fixed point and only exits on `request_stop`,
            // which the coordinator issues *after* observing completion — so
            // polling `!is_running` here deadlocked, leaving marking permanently
            // "active" and mixed GC never firing (old-gen never reclaimed).
            return slot.as_ref().map_or(true, |c| c.is_quiesced());
        }
        true
    }

    /// G1 STW final remark + cleanup — the cycle-ending twin of
    /// [`Self::g1_mark_roots`].
    ///
    /// MUST be called from inside a stop-the-world pause (the caller holds
    /// the GC barrier; every mutator is parked at its safepoint), with
    /// `roots` covering ALL threads. Performs, in order:
    ///
    /// 1. joins the background marker (its worklist is at a fixed point —
    ///    the caller gates on [`Self::g1_concurrent_mark_finished`], and a
    ///    stopped worker rules out interleaving with the drain below);
    /// 2. `remark(roots)` — re-scans roots and drains the SATB log (every
    ///    parked mutator's local buffer + the global shards) into the gray
    ///    set. Without this the SATB write barrier's entire output was
    ///    DISCARDED at cleanup: the cycle was a one-shot closure from the
    ///    initial roots, racing mutator edge deletions, and any live object
    ///    whose only marker-visible path was rewritten mid-trace stayed
    ///    unmarked (the SteadyChurn freed-live-Old-region defect);
    /// 3. drains the gray set to a fixed point (including the overflow
    ///    rescans — `concurrent_mark_step` returns `false` until both the
    ///    worklist is empty and no overflow recovery is pending);
    /// 4. INT-8 — when `process_refs` is supplied, invokes it with the
    ///    post-remark liveness predicate (`G1Collector::is_live_after_mark`:
    ///    bitmap + TAMS snapshot; conservative LIVE without positive
    ///    evidence). The callback runs the VM's reference processing
    ///    (clears, queue links, finalizer/cleaner submissions) and returns
    ///    the addresses that must be RESURRECTED — dead finalizables about
    ///    to run `finalize()`, pending cleaner chains, policy-retained soft
    ///    referents. Those are marked gray and the closure re-drained, so
    ///    step 5 cannot free them. (Since gch h4/g1 the VM's callback marks
    ///    that list itself, inside the callback -- `g1_remark_mark_kept_closure`,
    ///    the same `resurrect_after_remark` -- so that its class, loader and
    ///    weak verdicts read the final bitmap; the heap's call here is then a
    ///    no-op kept for any caller that does not.) The predicate is
    ///    [`G1Collector::remark_liveness`]'s view: `is_live_after_mark` with the
    ///    snapshot copied once (gci i22/g1rk). This window — bitmap complete, nothing
    ///    freed yet — is the only point in the cycle where weak/soft refs
    ///    to dead OLD-region referents can be cleared (evacuation pauses
    ///    only ever see CSet deaths);
    /// 5. `cleanup()` — per-region liveness, in-place free of wholly-dead
    ///    Old regions, humongous reclaim, SATB deactivation — while the
    ///    world is still stopped.
    ///
    /// Returns `false` (and does nothing) when no cycle is active, so a
    /// second initiator that lost the STW race cannot re-run remark against
    /// an already-completed cycle.
    pub fn g1_final_remark_and_cleanup(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &[cratonvm_types::ObjectRef],
        process_refs: Option<&mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize>>,
    ) -> bool {
        if let VmHeapInner::G1(state) = &self.inner {
            let Some(ctrl) = state.concurrent_mark.lock().take() else {
                // Defensive: a marking-active phase with no controller is an
                // orphaned cycle nobody is driving — abort it (no bitmap
                // verdicts) rather than letting the completion gate spin on
                // it forever. We are inside an STW here, so this cannot race
                // `g1_start_concurrent_mark` (also STW-only).
                if state.collector.gc_state.is_marking_active() {
                    tracing::warn!(
                        "g1_final_remark_and_cleanup: marking active with no \
                         controller — aborting orphaned cycle"
                    );
                    state.collector.abort_concurrent_mark();
                } else {
                    tracing::trace!("g1_final_remark_and_cleanup: no active controller");
                }
                return false;
            };
            let steps = ctrl.steps_performed();
            let outcome = ctrl.request_stop_and_join();
            // r11w19 census: count the orphans only a rejected band word
            // keeps alive, against the drained closure (see
            // `G1Collector::finish_deferred_pin_seeds`).
            let census = cratonvm_gc_g1::g1::g1_jit_pin_need_census();
            if census {
                state.collector.defer_next_remark_pin_seeds();
            }
            state.collector.remark(stw, roots);
            // g1ep7: on the evacuation pool (`drain_marking_to_fixed_point`).
            state.collector.drain_marking_to_fixed_point();
            if census && state.collector.finish_deferred_pin_seeds() {
                state.collector.drain_marking_to_fixed_point();
            }
            tracing::debug!(
                "g1_final_remark_and_cleanup: remark+drain done (worker steps={}, joined_ok={})",
                steps,
                outcome.is_ok(),
            );
            // INT-8 (step 4): reference processing against the completed
            // bitmap, then resurrection of everything the processor handed
            // out, BEFORE cleanup can free it.
            if let Some(cb) = process_refs {
                let collector = &state.collector;
                // gci i22/g1rk (item 7 of `gch-h4g1-g1-review-findings-FIXED-20261017`):
                // `is_live_after_mark`'s verdict with the mark-start snapshot
                // copied once for the whole callback, instead of its mutex per
                // question (`G1Collector::remark_liveness`; the snapshot cannot
                // change inside this pause). No lock is held between calls.
                let view = collector.remark_liveness();
                let is_live = |addr: usize| view.is_live(addr);
                let resurrect = cb(&is_live);
                drop(view);
                collector.resurrect_after_remark(&resurrect);
            }
            // The collection-overlay rows of every owner this completed mark
            // proves dead go BEFORE cleanup frees their elements (after the
            // resurrection above, whose objects may own rows). G1 marks overlay
            // contents only on behalf of a live owner (`g1_overlay.rs`).
            state.collector.prune_overlays_after_mark();
            state
                .gc_state
                .set_phase(ConcurrentGcPhase::ConcurrentSweep);
            state.collector.cleanup(stw);
            state.collector.gc_state.set_phase(ConcurrentGcPhase::Idle);
            return true;
        }
        false
    }

    /// Signal that G1 concurrent marking is complete: join the worker,
    /// run cleanup, return phase to `Idle`.
    ///
    /// NOTE: prefer [`Self::g1_final_remark_and_cleanup`] — this variant
    /// skips the final remark (no root re-scan, no SATB drain), so any
    /// reference the mutators overwrote during the concurrent phase never
    /// reaches the bitmap. Retained for tests and as the abort/teardown
    /// path; the runtime cycle driver no longer calls it.
    ///
    /// **This path therefore reclaims NOTHING, by design** (audit G1-3 /
    /// §8.2). Skipping the remark means the gray set is not drained to a fixed
    /// point, and `G1Collector::cleanup` no longer trusts its caller about
    /// that: it re-checks the worklist and, finding it non-empty, takes the
    /// retain-everything fail-safe — no in-place free of a zero-live Old
    /// region, no humongous reclaim — because on an incomplete closure
    /// `live_bytes == 0` does not mean "unreachable", and acting on it frees
    /// live objects. The pause is recorded with
    /// `g1_degraded::CLEANUP_CLOSURE_INCOMPLETE` so the declined reclamation
    /// is visible in `collector_decision_report()` rather than silent.
    ///
    /// So the useful reading of this method is "stop the marker and return the
    /// phase machine to `Idle`", not "finish the cycle". A caller that wants
    /// the cycle's reclamation must run [`Self::g1_final_remark_and_cleanup`],
    /// which drives `concurrent_mark_step` to a fixed point first. Covered by
    /// `g1::tests::cleanup_with_an_undrained_gray_set_retains_every_region`.
    ///
    /// Task #56: drains the [`ConcurrentMarkController`] slot and joins
    /// the background worker (blocking). The STW remark the caller runs
    /// next requires a quiescent worklist, so the join is mandatory.
    ///
    /// Edge case (no active controller): defensive no-op — happens when
    /// called twice, or before any cycle started. Skipping cleanup keeps
    /// the phase machine clean (cleanup itself is idempotent, but
    /// running it from Idle would flip `marking_complete` spuriously).
    pub fn g1_signal_marking_complete(&self, stw: &cratonvm_gc_core::collector::StopTheWorldToken) {
        if let VmHeapInner::G1(state) = &self.inner {
            let ctrl = state.concurrent_mark.lock().take();
            match ctrl {
                Some(ctrl) => {
                    let steps = ctrl.steps_performed();
                    let outcome = ctrl.request_stop_and_join();
                    tracing::debug!(
                        "g1_signal_marking_complete: joined (steps={}, joined_ok={})",
                        steps,
                        outcome.is_ok(),
                    );
                    // As in `g1_final_remark_and_cleanup`: overlay rows of
                    // owners the mark proves dead go before cleanup frees.
                    state.collector.prune_overlays_after_mark();
                    state
                        .collector
                        .gc_state
                        .set_phase(ConcurrentGcPhase::ConcurrentSweep);
                    state.collector.cleanup(stw);
                    state.collector.gc_state.set_phase(ConcurrentGcPhase::Idle);
                }
                None => {
                    tracing::trace!("g1_signal_marking_complete: no active controller");
                }
            }
        }
    }

    /// gci i15/ca (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
    /// item 7's cost half): ABORT the open G1 marking cycle, as HotSpot's full
    /// collection does (`G1ConcurrentMark::concurrent_cycle_abort`): stop and
    /// join the background marker, then
    /// [`G1Collector::abort_concurrent_mark`](cratonvm_gc_g1::g1::G1Collector::abort_concurrent_mark)
    /// -- gray set, overlay snapshot, referent-slot skip set, TAMS rows and
    /// mark-start snapshot dropped, the cycle's trust withdrawn
    /// (`last_mark_trusted`), the phase back to `Idle`, then the SATB queue
    /// closed with every per-thread bucket and shard discarded. No remark, no
    /// reference processing, no cleanup: nothing is freed, and the next cycle
    /// re-marks from its own roots (`start_concurrent_mark_with_reference_skip`
    /// clears every bitmap and bumps the SATB epoch).
    ///
    /// MUST be called inside a stop-the-world pause (the token): every mutator
    /// is parked, so no store can be mid-SATB-log while the queue closes, and no
    /// young pause can run between the join and the abort. The caller is the
    /// VM's full-collection door (`vm/src/runtime/interpreter/gc_and_alloc.rs`
    /// `g1_abort_open_cycle_pause`), which then starts a fresh cycle.
    ///
    /// `true` = a cycle was marking and is now aborted; `false` (nothing done)
    /// off G1 or with no cycle marking (another thread's remark closed it first).
    pub fn g1_abort_concurrent_mark(&self, _stw: &cratonvm_gc_core::collector::StopTheWorldToken) -> bool {
        if let VmHeapInner::G1(state) = &self.inner {
            if !state.collector.gc_state.is_marking_active() {
                return false;
            }
            // Joined BEFORE the abort: a marker step racing `clear_gray_set` /
            // the TAMS reset could re-gray or account into the state the abort
            // is discarding. Released before the blocking join, as
            // `g1_start_concurrent_mark_with_reference_skip` does.
            let ctrl = state.concurrent_mark.lock().take();
            if let Some(ctrl) = ctrl {
                let steps = ctrl.steps_performed();
                let outcome = ctrl.request_stop_and_join();
                tracing::debug!(
                    "g1_abort_concurrent_mark: marker joined (steps={}, joined_ok={})",
                    steps,
                    outcome.is_ok(),
                );
            }
            state.collector.abort_concurrent_mark();
            return true;
        }
        false
    }

    /// Apply the HotSpot tenuring flags (`-XX:MaxTenuringThreshold`,
    /// `-XX:InitialTenuringThreshold`, `-XX:TargetSurvivorRatio`,
    /// `-XX:+PrintTenuringDistribution`) — gen r4w6/young6.
    ///
    /// Honoured in full by the generational backend (adaptive tenuring, see
    /// `gen_heap_tenuring.rs`); G1 takes `MaxTenuringThreshold` and
    /// `TargetSurvivorRatio`, as HotSpot G1 does (gci i23/tn,
    /// `G1Collector::set_tenuring_flags`); ZGC has no tenuring, so for it
    /// this is a no-op and the launcher, which knows the flags were typed,
    /// prints the one-time "ignored on this collector" note. A default
    /// config is a no-op on every backend.
    pub fn set_tenuring_config(&self, cfg: &cratonvm_gc_gen::gen_heap::TenuringConfig) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.set_tenuring_config(cfg);
        } else if let VmHeapInner::G1(h) = &self.inner {
            // gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018`
            // item 5): HotSpot G1 honours `-XX:MaxTenuringThreshold` and
            // `-XX:TargetSurvivorRatio`; `InitialTenuringThreshold` is
            // Parallel's only, and `PrintTenuringDistribution` is not G1's
            // (the launcher still notes that one). Both `None` = no change.
            h.set_tenuring_flags(cfg.max_tenuring_threshold, cfg.target_survivor_ratio);
        }
    }

    /// gcg g3/x: `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio`; Generational,
    /// and G1's sizing policy (gci i13/ef finding 6; read only under
    /// `CRATONVM_G1_HEAP_RESIZE`). ZGC does not read them.
    pub fn set_heap_free_ratios(&self, min: Option<u8>, max: Option<u8>) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.set_heap_free_ratios(min, max);
        } else if let VmHeapInner::G1(h) = &self.inner {
            h.set_heap_free_ratios(min, max);
        }
    }

    /// Enable GC logging (verbose:gc).
    pub fn enable_gc_logging(&self) {
        match &self.inner {
            VmHeapInner::G1(g1) => g1.enable_gc_logging(),
            // 2026-09-20 (round 2): a plain delegation, exactly like its two
            // neighbours. Until this date this arm turned NOTHING on —
            // `GenerationalHeap` had no logging flag and no gated per-collection
            // statement to flip it for — while logging an affirmative at `info`,
            // which survives a release build (`release_max_level_info`). So
            // `--verbose:gc -XX:+UseGenerationalGC` printed a green light and
            // then produced no per-collection output for the rest of the run.
            // (The *shutdown* census below was always emitted; the missing half
            // was the PER-COLLECTION line, which is what the flag means on
            // HotSpot and what G1 and ZGC emit. The wave-1 write-up overstated
            // this; the measured correction is in
            // `docs/feature-designs/generative-gc-architecture.md`.)
            //
            // `GenerationalHeap::set_verbose_gc` now owns the flag and the
            // per-collection emitter reads it, mirroring `zgc.rs:8007` and
            // `g1.rs:18073`. Gap:
            // `docs/feature-designs/generative-gc-architecture.md`.
            VmHeapInner::Generational(h) => h.set_verbose_gc(true),
            // 2026-08-07: this arm used to answer with an honest "unavailable"
            // `tracing::info!` instead of enabling anything, because
            // `ZgcRealHeap` had no toggle to flip and no gated statement to flip
            // it for — so a user running `--verbose:gc -XX:+UseZGC` got nothing
            // for the whole run. `zgc.rs` has since grown the `gc_log_enabled:
            // AtomicBool` field, the `enable_gc_logging` / `disable_gc_logging`
            // pair mirroring G1's, and the per-collection `eprintln!` in
            // `collect_garbage` that reads the flag — so the honest answer is
            // now a plain delegation, exactly like G1's arm above.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.enable_gc_logging(),
        }
    }

    /// Switch GC logging (verbose:gc) on or off at run time -- the VM side of
    /// `MemoryMXBean.setVerbose` (gc-common w18-g). Each backend's own flag,
    /// the same ones `enable_gc_logging` sets.
    pub fn set_gc_logging(&self, on: bool) {
        match &self.inner {
            VmHeapInner::G1(g1) => {
                if on {
                    g1.enable_gc_logging()
                } else {
                    g1.disable_gc_logging()
                }
            }
            VmHeapInner::Generational(h) => h.set_verbose_gc(on),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                if on {
                    h.enable_gc_logging()
                } else {
                    h.disable_gc_logging()
                }
            }
        }
    }

    /// Emit ZGC's end-of-run metrics summary and TSV row, once per process.
    ///
    /// **Deliberately NOT inside [`VmHeap::print_gc_summary`]**, which the
    /// callers gate on `gc_stats_requested` (`--verbose:gc` /
    /// `CRATONVM_GC_STATS`). The whole point of `CRATONVM_ZGC_METRICS_TSV` is
    /// a machine-readable row a harness can `join`, and requiring a log flag
    /// to get it would reintroduce the log-parsing this replaces.
    ///
    /// It also cannot live where it was: `impl Drop for ZgcRealHeap` does not
    /// run in the shipped VM (the heap is held in an `Arc` until the process
    /// exits), which is why a run with `CRATONVM_ZGC_METRICS=1` printed no
    /// metrics summary at all before 2026-09-21. `ZgcRealHeap::
    /// emit_end_of_run_metrics` claims once per process, so calling this from
    /// both of `vm-cli`'s exit arms is safe.
    ///
    /// A no-op on every other backend and in a build without the `zgc`
    /// feature.
    pub fn emit_zgc_end_of_run_metrics(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            // zt t23/x19 (page item 28d): the cycle's end first.
            let _ = h.finish_pending_sweep_at_exit();
            h.emit_end_of_run_metrics();
        }
    }

    /// Print the end-of-run `[GC] …` census to stderr: G1's pause summary
    /// (p50/p99/max young + mixed) and its per-subsystem lines, ZGC's
    /// `[GC] zgc-*` block, the Generational counts and sweep-health lines, and
    /// the cross-backend card/remembered-set report. Called at VM shutdown when
    /// GC stats are requested (`--verbose:gc` or `CRATONVM_GC_STATS`).
    ///
    /// (This doc used to sit on [`Self::emit_zgc_end_of_run_metrics`], merged
    /// into that function's own doc, and claimed the Generational arm was a
    /// no-op — it prints more lines than any other arm.)
    pub fn print_gc_summary(&self) {
        // zt t23/x19 (page item 28d): a sweep still pending at exit is
        // finished first, so the summary (`allocated`, `csw_pending=`) reports
        // the cycle's end. One load when nothing is pending.
        let _ = self.zgc_finish_pending_sweep_at_exit();
        // gcf f4/r (`gengc-r5w4-defaults8-proposal-print-the-effective-heap-policy`):
        // which allocation / heap policies actually ran, each with where its
        // value came from, first, so two runs' logs can be compared after
        // the fact. Rendered from the flag snapshot (`GcFlags::policy_line`).
        let backend = match &self.inner {
            VmHeapInner::Generational(_) => "generational",
            VmHeapInner::G1(_) => "g1",
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => "zgc",
        };
        let env = |key: &str| {
            cratonvm_types::flags::runtime_var_os(key).map(|v| v.to_string_lossy().into_owned())
        };
        eprintln!(
            "{}",
            crate::gc_flags().policy_line(backend, self.is_generational(), &env)
        );
        if let VmHeapInner::G1(g1) = &self.inner {
            g1.print_gc_summary();
            // Independent of GC stats being requested: this census answers
            // "how many accessor calls still take the global regions lock?",
            // which is a question about the MUTATOR, not about collections.
            // Gated by its own flag (`CRATONVM_DBG_G1ACCESSOR`).
            g1.dbg_report_accessor_census();
        }
        // ZGC: the same unconditional-counts treatment the generational branch
        // below gets, and for the same reason — without it a `--verbose:gc` run
        // on this backend printed NOTHING at all (there is no ZGC branch in any
        // logging path; `enable_gc_logging` above only ever emitted a claim),
        // so "did this configuration collect more?" — the first question to ask
        // about the open ZGC HANG/FAIL classes — could not be answered from a
        // log. `occupancy` is the post-sweep live figure, because the sweep
        // stores retained bytes back into `allocated` (`zgc.rs:2472`); it is
        // therefore directly comparable across runs, unlike a bump cursor.
        // Cheap: two relaxed loads and one arena lock at shutdown.
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            eprintln!(
                "[GC] zgc-real: collections={} occupancy={}/{} bytes",
                h.gc_count(),
                h.allocated_bytes(),
                h.heap_capacity(),
            );
            // zs s3/ac: the collection's CPU by owner, and the process CPU it
            // is a share of, so one run's stderr answers "collection CPU /
            // process CPU" (`ZCollectionCpuTotals` says which shares nest).
            // Outside every `cycles_started > 0` block: a run with no
            // concurrent cycle still has a STW collection CPU to report.
            eprintln!(
                "{}",
                h.collection_cpu_totals().summary_line(
                    cratonvm_gc_zgc::zgc::mark::process_cpu_nanos(),
                    h.collection_cpu_accounting_on(),
                )
            );
            // zs s3/ac: the SATB direct hand-off's engagement
            // (`CRATONVM_ZGC_SATB_DIRECT_HANDOFF`); 0 on a run with no
            // concurrent cycle.
            let (satb_direct_handed, satb_direct_on) = h.satb_direct_handoff_report();
            eprintln!(
                "[GC] zgc-satb-direct: handed={satb_direct_handed} switch={}",
                satb_direct_on as u8
            );
            // WHY those collections happened. `needs_gc` has four
            // independent reasons and the count alone cannot separate
            // them, which is what made "13 collections against 2" on the
            // same workload unattributable. A cycle can satisfy more than
            // one, so these do not have to sum to `collections`.
            {
                {
                    // WHICH door started each cycle. Printed beside the
                    // trigger tallies because the two answer different
                    // halves of the same question, and on the run that
                    // motivated both, the tallies were all zero.
                    let (needs, requested, forced, native) =
                        cratonvm_types::gc_entry_census::totals();
                    eprintln!(
                        "[GC] zgc-entry: maybe_gc_needs={needs} \
                         maybe_gc_requested={requested} forced={forced} \
                         from_native={native}"
                    );
                    {
                        let (att, ok, total) = cratonvm_types::gc_entry_census::refill_totals();
                        eprintln!(
                            "[GC] zgc-entry:   tlab refills attempted={att} refill_succeeded={ok}; bytes_allocated_total={total} (the wedge break's re-arm, one break per 64 MB)"
                        );
                        let (rt, rok) = cratonvm_types::gc_entry_census::refill_retry_totals();
                        eprintln!(
                            "[GC] zgc-entry:   post-break refill retries={rt} retry_succeeded={rok} (a success seeds the TLAB, whose allocations re-arm the breaker)"
                        );
                        // WHO ZEROES TLAB CHUNKS (round 9 wave 11, `zgc11`).
                        // `pristine_bytes_skipped` had no reader outside a unit
                        // test, so whether the pristine skip did anything in a
                        // run could not be told from its log. Read it against
                        // `vm_tlab_refill_bytes`: a GC-heavy run skips only
                        // up to the arena's first high-water mark, and pays the
                        // `memset` for every chunk carved below it.
                        let (_, refill_bytes, _, _) = h.vm_tlab_engagement();
                        eprintln!(
                            "[GC] zgc-tlab-zero: pristine_bytes_skipped={} slide_zero_bytes_skipped={} tlab_zero_refill_bytes={refill_bytes}",
                            h.tlab_pristine_bytes_skipped(),
                            h.tlab_slide_zero_bytes_skipped(),
                        );
                        // zt t2/al: the opt-in descending chunk zeroing's
                        // engagement, printed only while it is on (the
                        // default line above is unchanged).
                        let (piece, chunks) = h.chunk_zero_descending_stats();
                        if piece != 0 {
                            eprintln!(
                                "[GC] zgc-tlab-zero-descending: piece_bytes={piece} chunks={chunks}"
                            );
                        }
                        // THE TLAB RESERVATION, AGAINST THE BUDGET IT IS
                        // SUPPOSED TO OBEY (applies
                        // `handoff-s-tlab-reserved-peak.md`).
                        //
                        // `reserved` at shutdown is ~0 -- every thread has
                        // retired. The peak is monotone and is still there.
                        //
                        // The two rules do not disagree about the current
                        // reservation; they disagree about the SUM OUTSTANDING
                        // AT THE WORST INSTANT. `ZGC_TLAB_RESERVATION_SHARE` is
                        // a budget in bytes, while the count rule enforces a
                        // per-thread chunk SIZE, so with N threads the sum can
                        // exceed the budget by roughly a factor of N -- and the
                        // window re-opens at every collection, because the count
                        // is rebuilt there.
                        //
                        // Read `peak/budget`: far above 1 is the unbounded
                        // shape the share exists to prevent. (zt t12/rt: the
                        // byte arm, `CRATONVM_ZGC_TLAB_RESERVED_BYTES`, and its
                        // `byte_sizing=` field were retired.)
                        eprintln!(
                            "[GC] zgc-tlab-reserve: reserved={} peak={} budget={}",
                            h.tlab_reserved_bytes(),
                            h.tlab_reserved_bytes_peak(),
                            h.tlab_reservation_budget_bytes(),
                        );
                        // THE PAGE SURVEY -- roadmap C0 stage 0, applying
                        // `handoff-s-page-survey-stage-0.md`. Without this line
                        // `CRATONVM_ZGC_PAGE_SURVEY` is UNFALSIFIABLE, which is
                        // the condition `page.rs` has been in since it was
                        // written: 2,000 lines of allocator whose engagement
                        // could not be told from its absence.
                        //
                        // The whole flag changes no behaviour -- a survey
                        // cannot allocate, free or write a byte -- so the only
                        // thing it can be judged on is whether it saw the same
                        // heap the collector did. Two readings say that:
                        // `grid_refreshes` must equal the collection count, and
                        // `views_rejected` must be ZERO. A non-zero rejection
                        // count is a WIRING DEFECT, not a measurement: it means
                        // the grid was built against a different base than the
                        // survey was, so the survey describes the wrong heap.
                        //
                        // `base_aligned` is recorded rather than enforced.
                        // Stage 0 does not need it; stage 1 does, because a
                        // real page must start on a granule.
                        if let Some(survey) = h.page_survey_stats() {
                            eprintln!(
                                "[GC] zgc-page-survey: grid_refreshes={} \
                                 views_installed={} views_rejected={} \
                                 granules_indexed={} base_aligned={:?} \
                                 survey_collections={}",
                                survey.grid_refreshes,
                                survey.views_installed,
                                survey.views_rejected,
                                survey.granules_indexed,
                                h.page_survey_base_aligned(),
                                h.gc_count(),
                            );
                        }
                    }
                    for (site, n) in cratonvm_types::gc_entry_census::forced_sites() {
                        eprintln!("[GC] zgc-entry:   forced by {site}: {n}");
                    }
                }
                let (stress, threshold, headroom, budget, hard) = h.trigger_tallies();
                eprintln!(
                    "[GC] zgc-trigger: stress={stress} live_bytes_threshold={threshold} headroom_low={headroom} alloc_budget={budget} hard_alloc_refusals={hard} warmup={} director={} director_deferred={}",
                    h.trigger_warmup_count(),
                    // zr zs s1/tp: the director's tally had no reader -- the
                    // rate-trigger rows' "director tally > 0" positive
                    // control could not be read -- and its anti-storm
                    // deferrals (`CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`).
                    h.trigger_director_count(),
                    h.trigger_director_deferred_count(),
                );
                // zr z6/tp: trigger pacing near a full heap -- which rule asked
                // for each collection taken with the live set above the
                // threshold, and the allocation window before it.
                eprintln!("{}", h.pacing_line());
                // WHY THE COLLECTOR DID **NOT** RUN, which the five tallies
                // above cannot say: `headroom_low` is about allocatable space,
                // `gc_rearm` is about live bytes, and on a non-compacting heap
                // those can diverge — so the arena can be unable to serve an
                // allocation while the trigger that exists for exactly that
                // case is held below its floor.
                //
                // What that COSTS is open, and smaller than this round's
                // summary claimed. Measured 2026-09-21 at the wave-4 base
                // commit: every exhaustion shape the orchestrator could
                // construct — including the non-compacting arm, where the
                // divergence is supposed to be unbounded — threw a catchable
                // `OutOfMemoryError` at ~63 MiB of a 64 MiB heap. Near-full,
                // no abort. `starved_intervals` is what settles whether the
                // state occurs at all; see `gap-p-headroom-flip-criteria.md`.
                //
                // `starved` is the counterfactual and is readable on the
                // DEFAULT configuration: it does not need
                // `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM=1` to be set, which is
                // the point — the flip criterion it replaces asked for a
                // reading that only exists in the arm being justified.
                //
                // Printed here with the zeroes included, because in a
                // stats-requested run "this workload never reached the shape"
                // is itself the answer and a suppressed line reads identically
                // to a line nobody looked at. `ZgcRealHeap`'s `Drop` prints the
                // same figures under NO flag when they are non-zero, which is
                // the reading for a run with logging off — the run this counter
                // is actually about.
                let (starved, starved_gap, bypass, suppressed) = h.headroom_flip_evidence();
                eprintln!(
                    "[GC] zgc-headroom: starved_intervals={starved} \
                     starved_max_gap_bytes={starved_gap} bypass_fired={bypass} \
                     bypass_suppressed={suppressed} bypass_enabled={}",
                    h.headroom_trigger_state().1,
                );
                // Every count here is a dropped old->young edge, i.e. a live
                // object freed. It must read 0; see
                // `ZgcCounters::remset_pages_without_base`.
                eprintln!(
                    "[GC] zgc-remset: pages_without_a_base={}",
                    h.remset_pages_without_base(),
                );
                // WHAT THE PAUSE TARGET IS ACTUALLY MISSING BY, which decides
                // `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE`.
                //
                // The p99 here is over the WHOLE cycle -- every phase guard,
                // including the relocation slide -- and it is measured the same
                // way in both arms of that flag, because it comes from the
                // phase guards and not from the `gc_started` clock the flag
                // moves. So it is the honest pause in a run that believes the
                // pre-slide figure, which is exactly what an A/B needs and what
                // no line printed before: `zgc-relocate` gives one cycle's
                // `pause_with_relocate_us` and a percentile over it needed the
                // whole log post-processed.
                //
                // Read it against `target_ms`: a p99 above the target in the
                // `=0` arm is the default-on control loop failing to deliver
                // what it promises while reporting success, because the number
                // it was fed excluded about two thirds of the pause. See
                // `ZgcMetrics::cycle_pause_distribution` and
                // `gap-p-pause-target-includes-relocate-default.md`.
                let (target_ms, _span, budget, unreachable) = h.pause_target_state();
                let (sampled, p50_ns, p99_ns, max_ns) = h.metrics().cycle_pause_distribution();
                eprintln!(
                    "[GC] zgc-pause-dist: target_ms={target_ms} cycles_sampled={sampled} \
                     p50_us={} p99_us={} max_us={} alloc_trigger_bytes={budget} \
                     target_unreachable_cycles={unreachable}",
                    p50_ns / 1_000,
                    p99_ns / 1_000,
                    max_ns / 1_000,
                );
            }
            // Phase 2.2's tracked number, on its own line so a suite runner can
            // extract it per class with one grep.
            //
            // `frag_samples=0` is printed rather than suppressed, and it does
            // NOT mean "no fragmentation": it means no collection ever left a
            // quarter of the heap free, so this run says nothing about the
            // subject. Reporting that as a clean score is exactly how a gauge
            // becomes a vacuous green, so the two cases are spelled
            // differently and the reader is told which they have.
            // Did the 2026-08-13 default-on features engage? A gauntlet run
            // that silently took the serial, non-moving path would otherwise
            // look identical to one that exercised both.
            let (par_cycles, compactions, relocated) = h.feature_engagement();
            // `driver_passes` is the one field that separates "the worker pool
            // marked" from "`zgc_concurrent`'s controller drove the cycle":
            // the pool-only path this replaced produced identical mark bits,
            // identical stats and an identical `parallel_mark_cycles`.
            let (driver_passes, mark_fallbacks) = h.driver_engagement();
            // `relocation_skipped_jit` belongs NEXT TO `compaction_cycles`, not
            // on a line of its own: alone, `compaction_cycles=0` reads as a
            // broken collector; beside a large skip count it reads as a
            // workload that is never JIT-quiet. ZGC declines to relocate while
            // a compiled frame is live (its registers and spill slots cannot be
            // rewritten), so a JIT-saturated run legitimately compacts rarely
            // -- and compaction is this collector's only defragmentation, so
            // that is a number somebody needs to see.
            let skipped_jit = h.relocation_skipped_jit();
            // ...and its counterpart: cycles that compacted WITH a compiled
            // frame live, on the collection's per-cycle coverage proof.
            // `skipped_jit` alone cannot tell "this workload is never
            // JIT-quiet, so it never defragments" from "it is never
            // JIT-quiet and defragments anyway, on the proof", and those
            // are the before and after of the H2
            // `TestKillProcessWhileWriting` OutOfMemoryError.
            let proven_jit = h.relocation_on_proven_jit();
            // A retained TLAB chunk would be arena the collector cannot see,
            // and on a compacting heap that is a correctness problem rather
            // than a bookkeeping one. It reads zero on every workload measured
            // so far, which is the point of printing it: the hypothesis it
            // rules out is a good one, and the next reader should not have to
            // re-derive it.
            let tlab_skipped = h.tlab_retire_skipped();
            // zt t12/rt: `targets_recorded=` / `targets_consumed=` left this
            // line with the retired `CRATONVM_ZGC_TARGETED_COMPACTION`; nothing
            // names a target page any more, so `targeted_pages=` reads 0.
            // The starved TLAB rung takes the arena's LARGEST low free block, so
            // every firing lowers the very number a direct allocation is about
            // to fail against. Its switch shipped with no engagement counter,
            // which made "never reached" and "reached constantly" the same run.
            let (recycled_refills, starved_refills, starved_bytes) = h.tlab_recycle_engagement();
            // The VM thread's own TLAB (the JIT inline allocator's buffer) on
            // this backend. `vm_tlab_refills=0` on a run that allocated means
            // the arm never engaged -- switch off, or every allocation took
            // the helper -- and no throughput claim about it can stand.
            let (vm_tlab_refills, vm_tlab_refill_bytes, vm_tlab_tails, vm_tlab_tail_bytes) =
                h.vm_tlab_engagement();
            // IS THE EVACUATION BUDGET BINDING ON THIS RUN?
            //
            // `ZRelocationSet::select` is a prefix rule over
            // `max_evacuation_bytes` (64 MiB of live bytes by default,
            // `CRATONVM_ZGC_RELOCATE_BUDGET_MB` to change it), so a cycle that
            // compacted a prefix of the heap and stopped reports one
            // `compaction_cycles`, a large `objects_relocated` and no skip
            // reason -- identical to a cycle that finished. On this collector
            // compaction is the only defragmentation there is, so the two
            // readings point at opposite repairs for a fragmentation
            // `OutOfMemoryError`: "the slide never ran" wants the coverage
            // proof, "the slide ran and stopped early" wants the constant.
            //
            // `reloc_budget_truncated_cycles=0` is the finding that the
            // constant is NOT binding here, and no A/B of its value can
            // measure anything on this workload.
            let (reloc_pages_deferred, reloc_budget_truncated_cycles) =
                h.relocation_budget_engagement();
            eprintln!(
                "[GC] zgc-features: parallel_mark_cycles={par_cycles} \
                 driver_passes={driver_passes} mark_fallbacks={mark_fallbacks} \
                 compaction_cycles={compactions} objects_relocated={relocated} \
                 relocation_skipped_jit={skipped_jit} \
                 relocation_on_proven_jit={proven_jit} \
                 tlab_retire_skipped={tlab_skipped} tlab_retire_skipped_at_safepoint={tlab_retire_skipped_at_safepoint} targeted_pages={targeted_pages} tlab_recycled_refills={recycled_refills} tlab_starved_refills={starved_refills} tlab_starved_bytes={starved_bytes} \
                 vm_tlab_refills={vm_tlab_refills} vm_tlab_refill_bytes={vm_tlab_refill_bytes} \
                 vm_tlab_tails_returned={vm_tlab_tails} vm_tlab_tail_bytes_returned={vm_tlab_tail_bytes} \
                 vm_tlab_tail_recharge_bytes={tail_recharge_bytes} \
                 reloc_pages_deferred={reloc_pages_deferred} \
                 reloc_budget_truncated_cycles={reloc_budget_truncated_cycles}",
                targeted_pages = h.targeted_pages_selected(),
                // zr z1/al residual: published tails a collection kept charged
                // (`ZgcRealHeap::recharge_published_tails`); 0 on a run whose
                // collections froze nobody holding a VM TLAB.
                tail_recharge_bytes = h.published_tail_recharge_bytes(),
                tlab_retire_skipped_at_safepoint = h.tlab_retire_skipped_at_safepoint(),
                recycled_refills = recycled_refills,
                starved_refills = starved_refills,
                starved_bytes = starved_bytes,
            );
            // WHICH of the five terms refused, and — when it was the coverage
            // proof — which obligation. `relocation_skipped_jit` is a count of
            // a conjunction; on its own it names nothing, and the generational
            // collector's `moving_young_fallback_reason` census cannot fill the
            // gap because its only writer is that collector's own per-cycle
            // accounting (`gc_quiescence::moving_young_incomplete_reason_mask`
            // says so). Printed only when something actually declined, so a
            // clean run does not grow two empty sections.
            let skip_reasons = h.relocation_skip_reason_counts();
            for (reason, n) in skip_reasons.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] zgc-relocation-skip-reason: {}={}",
                        cratonvm_gc_zgc::zgc::relocation_skip_reason::label(reason),
                        n
                    );
                }
            }
            // WHICH OBLIGATION FAILED, independently of whether it stopped the
            // slide. The loop above counts refusals that took effect; this one
            // counts the verdicts, including the four terms the
            // `compiled_frames_live` gate computes and discards. A term that
            // appears here with no matching `zgc-relocation-skip-reason:` line
            // fired on every cycle it was asked about and stopped none of them.
            //
            // One code is structurally absent here and that is not a gap:
            // `tlab-retire-incomplete-at-safepoint` is an earlier unconditional
            // refusal that returns before this census.
            let refusal_terms = h.relocation_refusal_term_counts();
            for (reason, n) in refusal_terms.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] zgc-relocation-refusal-term: {}={} (stopped_a_slide={})",
                        cratonvm_gc_zgc::zgc::relocation_skip_reason::label(reason),
                        n,
                        skip_reasons.get(reason).copied().unwrap_or(0),
                    );
                }
            }
            let cov_reasons = h.relocation_coverage_reason_counts();
            for (reason, n) in cov_reasons.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] zgc-relocation-coverage-reason: {}={}",
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::label(reason),
                        n
                    );
                }
            }
            // WHAT THE UNREGISTERED-FRAME PROBE ACTUALLY SAW, because the
            // coverage reason above cannot say. `unregistered-jit-frame-on-stack`
            // counts cycles refused; these two count the HITS behind them, split
            // by the only question that decides whether a refusal was earned: was
            // the stack word at or above this thread's returned-JIT-frame residue
            // mark (a band no returned frame can have written -- a live guardless
            // frame) or below it (the leftovers of a frame that has returned)?
            // Printed only when the probe fired at all.
            let (residue_explained, residue_live) =
                cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_residue_census();
            if residue_explained > 0 || residue_live > 0 {
                eprintln!(
                    "[GC] zgc-unregistered-jit-frame: hits_above_residue_mark={residue_live} hits_explained_by_residue={residue_explained} (the second kind marks and pins the band but no longer refuses relocation; CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0 restores the refusal)"
                );
            }
            // THE OTHER END OF THE ARENA, on its own line.
            //
            // Every number above describes the LOW end. A heap can compact that
            // one on every cycle while the allocation actually failing is
            // served from the large-object end, which until 2026-08-29 nothing
            // could relocate at all -- the H2
            // `TestMVStoreTool`/`TestCachedQueryResults` failures, and the
            // reason `targeted_pages` reads 0 on them. `declined` is printed
            // beside `cycles` for the reason `relocation_skipped_jit` is
            // printed beside `compaction_cycles`: `cycles=0` alone reads as a
            // broken compactor, `cycles=0 declined=812` reads as a workload
            // that never fragmented its large-object end, and only one of those
            // is a defect.
            let (hi_cycles, hi_declined, hi_moved, hi_bytes) = h.high_compaction_engagement();
            // ...and what the LOW slide handed back rather than losing. Reclaim
            // used to be the cursor drop alone, so every byte a slide emptied
            // below a cursor it could not move was invisible to the allocator
            // for the rest of the process -- and to the sweep too, which walks
            // the object-start registry the slide has just rewritten. A large
            // `vacated_bytes` is the measure of what that cost.
            let (vac_spans, vac_bytes) = h.vacated_publication();
            eprintln!(
                // KEYS PREFIXED `high_`, and that is not cosmetic. This line
                // used to print `cycles=` and `objects_relocated=`, which are
                // the SAME KEYS the whole-heap compaction line above emits for
                // an unrelated population -- the large-object end, whose counts
                // are tiny beside it (12 against 601233 on a measured H2 run).
                // A reader grepping the summary for `objects_relocated=` gets
                // two matches with no way to tell which is which, and the
                // obvious `| tail -1` picks THIS one. That is not a
                // hypothetical: it produced a wrong figure in an H2 analysis on
                // 2026-09-05, and the sibling collision on `cycles=` made a GC
                // control read a vacuous zero in the same session.
                //
                // A summary is an interface. Its keys have to be unique across
                // the whole summary or it is not greppable, which is the only
                // way anyone consumes it.
                "[GC] zgc-high-compaction: high_cycles={hi_cycles} high_declined={hi_declined} high_objects_relocated={hi_moved} high_bytes_copied={hi_bytes} high_vacated_spans={vac_spans} high_vacated_bytes={vac_bytes}"
            );
            // THE SLIDE VERIFIER'S OWN ENGAGEMENT, so that a clean run under
            // `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1` is a READING rather than an
            // absence of output.
            //
            // `verify_no_dangling_slots_after_slide` reports a finding at
            // `error!` and a pass at `debug!`, and `release_max_level_info`
            // deletes the `debug!` from a release build. So on the binary
            // anybody actually reproduces with, "it printed nothing" covered
            // both "every reference slot resolved to a live base" and "the flag
            // was misspelled / no slide ran / the gate returned early" — and
            // only the first is evidence. `slides_verified` and
            // `survivors_walked` are the denominator that separates them.
            let (sv_runs, sv_survivors, sv_missed, sv_unreg) = h.slide_verification_stats();
            eprintln!(
                "[GC] zgc-slide-verify: slides_verified={sv_runs} survivors_walked={sv_survivors} missed_rewrites={sv_missed} unregistered_targets={sv_unreg} slide_verify_enabled={}",
                cratonvm_gc_zgc::zgc::zgc_verify_slide_enabled(),
            );
            // CONCURRENT marking, on its own line and with five fields rather
            // than one, because four different runs look identical in any
            // smaller summary:
            //
            //   started=0                 the threshold was never crossed --
            //                             this run says NOTHING about
            //                             concurrent marking
            //   started>0, completed=0    every cycle opened and then failed to
            //                             certify; the collector fell back to a
            //                             stop-the-world mark each time
            //   started>0, replayed=0     the barrier saw no reference
            //                             overwrites, so the SATB half is
            //                             untested by this workload
            //   started>0, phase_ms~0     the cycle opened and the collection
            //                             arrived immediately, so there was no
            //                             concurrent phase to speak of
            //
            // `black` is the allocate-black count: objects born marked because
            // a cycle was in flight. Zero of those with a non-zero `phase_ms`
            // means the mutators allocated nothing while the marker ran.
            let (started, completed, black, replayed, phase_nanos) =
                self.zgc_concurrent_mark_stats();
            // ...and the WINDOW FIT, which is the one number
            // `proposal-a-concurrent-mark-by-default.md` is gated on.
            //
            // `scanned_at_safepoint / scanned_concurrently` below **0.1** is
            // that proposal's step-3 acceptance bar: the fraction of the mark
            // that the concurrent window was too short to absorb, and which
            // therefore landed inside the stop-the-world pause concurrent
            // marking exists to shorten. Per-cycle figures are on
            // `[GC] zgc-markend:`; these are the sums, so the question is a
            // single run rather than a log-parsing session.
            //
            // Read `scanned_concurrently=0` as "no concurrent cycle completed
            // -- this run says nothing about the bar", not as a pass.
            // `window_target_ms=0` means the adaptive sizing had no opinion (a
            // fixed `CRATONVM_ZGC_CONC_START`, or no allocation rate sampled).
            //
            // Printing it is NOT a claim that the bar is met, and the default
            // is unchanged: `CRATONVM_ZGC_CONC_START=0`.
            let (scanned_conc, scanned_stw, window_target_ms) = h.concurrent_window_fit();
            // zr z7/cm: `satb_filtered` = SATB records dropped because the old
            // value was already marked (`CRATONVM_ZGC_SATB_FILTER_MARKED`);
            // `satb_filter` / `root_filter_conc` = the two concurrent-cost
            // switches as this heap runs them.
            let (satb_filtered, satb_filter_on, root_filter_conc) = h.concurrent_cost_report();
            // zt t7/cc: the concurrent overlay repair's engagement
            // (`CRATONVM_ZGC_CONC_OVERLAY_REPAIR`), summed over cycles.
            let (ov_rows, ov_refs) = h.concurrent_overlay_repair_totals();
            eprintln!(
                "[GC] zgc-concurrent: cycles_started={started} cycles_completed={completed} \
                 black_allocations={black} satb_replayed={replayed} \
                 concurrent_phase_ms={} scanned_concurrently={scanned_conc} \
                 scanned_at_safepoint={scanned_stw} window_target_ms={window_target_ms} \
                 satb_filtered={satb_filtered} satb_filter={} root_filter_conc={} \
                 overlay_repair_rows_total={ov_rows} overlay_repair_refs_total={ov_refs}",
                phase_nanos / 1_000_000,
                satb_filter_on as u8,
                root_filter_conc as u8,
            );
            // zr z3/cs: `CRATONVM_ZGC_CONC_EVERY_TRIGGER`. `ct_opened` cycles
            // were opened on a trigger's request; each ends `ct_finished` (the
            // collection waited for the mark's fixed point) or
            // `ct_abandoned_stw` (closed before it: `ct_urgent` of those at
            // the urgent reserve, the rest by another door -- allocation
            // failure, `System.gc()`). `ct_lapsed` requests never opened
            // (`ct_start_refusals` of them refused by the take-over cover);
            // `ct_no_headroom` early triggers collected stop-the-world because
            // the heap could not finish a mark. All zero with the switch off.
            let ct = h.conc_trigger_report();
            eprintln!(
                "[GC] zgc-conc-trigger: every_trigger={} ct_requests={} ct_opened={} \
                 ct_finished={} ct_abandoned_stw={} ct_urgent={} ct_lapsed={} \
                 ct_start_refusals={} ct_no_headroom={} ct_lost_races={} \
                 ct_start_rechecks={} ct_sysgc={} ct_sysgc_requests={} \
                 ct_sysgc_opened={} ct_sysgc_served={} ct_sysgc_lost={} \
                 ct_sysgc_not_opened={} ct_sysgc_held={}",
                ct.enabled as u8,
                ct.requests,
                ct.opened,
                ct.finished,
                ct.abandoned,
                ct.urgent,
                ct.lapsed,
                ct.start_refusals,
                ct.no_headroom,
                // zr zs s1/tp: open-race losers that paid a mark-start pause
                // anyway, and the starts dropped before requesting one.
                ct.lost_races,
                ct.start_rechecks,
                // zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`): requests
                // that asked for a cycle of their own, cycles that opened, the
                // ones the request's own collection closed with their mark set
                // (`served`) or another door closed first (`lost`), mark
                // starts that did not open, and mark-end polls held for the
                // waiting caller.
                ct.sysgc_enabled as u8,
                ct.sysgc_requests,
                ct.sysgc_opened,
                ct.sysgc_served,
                ct.sysgc_lost,
                ct.sysgc_not_opened,
                ct.sysgc_held,
            );
            // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): cycles
            // whose sweep left the pause (`csw_deferred`), cycles the screen
            // kept in it (`csw_refused`, the last reason's code), who ran each
            // parked sweep, the allocations that waited for one, and the
            // cumulative microseconds of the deferred pauses, the walks, the
            // installs and the release-to-walk waits. All zero with the switch
            // off.
            let cs = h.concurrent_sweep_report();
            eprintln!(
                "[GC] zgc-concurrent-sweep: csw_enabled={} csw_deferred={} csw_refused={} \
                 csw_last_refusal={} csw_by_initiator={} csw_by_allocation={} \
                 csw_by_cycle_start={} csw_by_mark_start={} csw_by_waiter={} \
                 csw_alloc_retries={} csw_pause_us={} csw_walk_us={} csw_install_us={} \
                 csw_wait_us={} csw_pending={} csw_by_exit={}",
                cs.enabled as u8,
                cs.deferred,
                cs.refused,
                cs.last_refusal,
                cs.by_initiator,
                cs.by_allocation,
                cs.by_cycle_start,
                cs.by_mark_start,
                cs.by_waiter,
                cs.alloc_retries,
                cs.pause_us,
                cs.walk_us,
                cs.install_us,
                cs.wait_us,
                cs.pending as u8,
                // zt t23/x19 (page item 28d): sweeps the VM's exit ran.
                cs.by_exit,
            );
            // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in): collections
            // whose referent slots were hidden instead of nulled
            // (`crf_skip_cycles`, rows `crf_skip_rows`, slots the pause cleared
            // `crf_stw_cleared`), collections generational mode refused, and the
            // census of the VM's reference round (recorded with the switch off
            // too): rounds, cumulative microseconds of the pre-collection pass,
            // `process_references_after_gc` and the JNI weak sweep, and the
            // longest round.
            let cr = h.concurrent_refs_report();
            eprintln!(
                "[GC] zgc-concurrent-refs: crf_enabled={} crf_skip_cycles={} crf_skip_rows={} \
                 crf_stw_cleared={} crf_refused={} crf_rounds={} crf_pre_us={} crf_post_us={} \
                 crf_jni_us={} crf_max_round_us={} crf_deferred_clears={} \
                 crf_after_release_cleared={} crf_gate_blocked={}",
                cr.enabled as u8,
                cr.skip_cycles,
                cr.skip_rows,
                cr.stw_cleared,
                cr.refused,
                cr.rounds,
                cr.pre_us,
                cr.post_us,
                cr.jni_us,
                cr.max_round_us,
                // zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): collections
                // that cleared after the release, the slots nulled there, and
                // mutator referent reads the gate answered `null`.
                cr.deferred_clears,
                cr.after_release_cleared,
                cr.gate_blocked,
            );
            // zt t23/zc6 (stage 1f): the concurrent cycles' closing pauses --
            // certified closes, closes whose pool had already reached its
            // fixed point, closes that fell back to a stop-the-world mark, the
            // addresses those pauses pushed and newly claimed, what the phase
            // marked / scanned against what the pauses did, the pauses' drive
            // time -- and the mark-end service (`CRATONVM_ZGC_MARK_END_SERVICE`,
            // opt-in): its looks at an open cycle and its closing attempts.
            let me = h.mark_end_report();
            eprintln!(
                "[GC] zgc-markend-census: mec_service={} mec_closed={} mec_caught_up_at_entry={} \
                 mec_fallbacks={} mec_pushed={} mec_claimed={} mec_conc_marked={} \
                 mec_end_marked={} mec_conc_scanned={} mec_end_scanned={} mec_end_us={} \
                 mec_end_max_us={} mec_svc_ticks={} mec_svc_closes={} mec_svc_lost={} \
                 mec_svc_stale={} mec_svc_extra={}",
                me.service as u8,
                me.closed,
                me.caught_up_at_entry,
                me.fallbacks,
                me.pushed,
                me.claimed,
                me.conc_marked,
                me.end_marked,
                me.conc_scanned,
                me.end_scanned,
                me.end_us,
                me.end_max_us,
                me.svc_ticks,
                me.svc_closes,
                me.svc_lost,
                me.svc_stale,
                me.svc_extra,
            );
            // zt t24/zc8 (stage 1f, `CRATONVM_ZGC_MARK_CONTINUE`, opt-in):
            // Concurrent Mark Continue -- the Pause Mark End attempts, their
            // verdicts, the closes that drained in the pause instead (the
            // cycle's attempts used up, the urgent reserve), what the attempts
            // handed the pool and newly claimed, the failed pauses' and the
            // continue phases' time, and the service attempts that released.
            let mc = h.mark_continue_report();
            eprintln!(
                "[GC] zgc-markcontinue: mcc_enabled={} mcc_attempts={} mcc_continues={} \
                 mcc_completes={} mcc_exhausted={} mcc_urgent={} mcc_pushed={} mcc_claimed={} \
                 mcc_pause_us={} mcc_continue_us={} mcc_max_cycle_continues={} \
                 mcc_svc_continued={} cpb_forget={} cpb_forgets={}",
                mc.enabled as u8,
                mc.attempts,
                mc.continues,
                mc.completes,
                mc.exhausted,
                mc.urgent,
                mc.pushed,
                mc.claimed,
                mc.pause_us,
                mc.continue_us,
                mc.max_cycle_continues,
                mc.svc_continued,
                // Stage 0 CPU: concurrent closes that dropped the pause-target
                // allocation budget (`CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET`).
                mc.budget_forget as u8,
                mc.budget_forgets,
            );
            // zt t26/zc9 (stage 1a, `CRATONVM_ZGC_CONCURRENT_RATE_START`,
            // opt-in): every-trigger requests made at HotSpot's
            // allocation-rate point, the cycles that opened on one, and the
            // rule's lapsed requests dropped instead of collected.
            let rs = h.rate_start_report();
            eprintln!(
                "[GC] zgc-rate-start: crs_enabled={} crs_requests={} crs_opened={} crs_dropped={}",
                rs.enabled as u8, rs.requests, rs.opened, rs.dropped,
            );
            // zt t27/zc10 (`CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`, default ON):
            // the referent-skip snapshots the member-class table was filled
            // for, the ones an unregistered member switched it off, and the
            // last fill's members and distinct class bits.
            let sk = h.skip_class_filter_report();
            eprintln!(
                "[GC] zgc-skip-class-filter: skc_enabled={} skc_fills={} skc_off={} \
                 skc_members={} skc_classes={}",
                sk.enabled as u8, sk.fills, sk.off, sk.members, sk.classes,
            );
            // zt t31/zc11 (`CRATONVM_ZGC_MARK_SKIP_BITS`, default ON): the
            // snapshots the member bitmap answered for, the members it set
            // (each once per cycle), the last cycle's members and the bits the
            // cycles' ends cleared.
            // zt t31/zc11 (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`): the reference
            // rounds run after the release, the deferring cycles that kept
            // theirs in the pause (and the last reason), the rounds' time and
            // the sweep runners that waited for one.
            let rr = h.concurrent_ref_round_report();
            eprintln!(
                "[GC] zgc-concurrent-ref-round: crr_enabled={} crr_rounds={} crr_refused={} \
                 crr_last_refusal={} crr_round_us={} crr_max_round_us={} crr_hold_waits={}",
                rr.enabled as u8,
                rr.rounds,
                rr.refused,
                rr.last_refusal,
                rr.round_us,
                rr.max_round_us,
                rr.hold_waits,
            );
            // zt t31/zc11 (`CRATONVM_ZGC_REMAP_IN_MARK`, R4): the slides that
            // left healed slots stale, the refusals (and the last reason), the
            // slots skipped, the markers' heals (and those that forwarded), the
            // load barrier's and the SATB pre-barrier's forwards.
            let rm = h.remap_in_mark_report();
            eprintln!(
                "[GC] zgc-remap-in-mark: rim_enabled={} rim_engaged_slides={} rim_refused={} \
                 rim_last_refusal={} rim_skipped={} rim_healed={} rim_forwarded={} \
                 rim_barrier_forwarded={} rim_satb_forwarded={} rim_gen_engaged={} \
                 rim_kept_past_young={}",
                rm.enabled as u8,
                rm.engaged_slides,
                rm.refused,
                rm.last_refusal,
                rm.skipped,
                rm.healed,
                rm.forwarded,
                rm.barrier_forwarded,
                rm.satb_forwarded,
                rm.gen_engaged,
                rm.kept_past_young,
            );
            // zt t32 (`CRATONVM_ZGC_CONCURRENT_RELOCATE`, R6): the closing pauses
            // that planned instead of sliding, the refusals (and the last
            // reason), the regions and objects planned, and who copied them:
            // the pause (eager), a pointer-map lookup after the release, the
            // barrier / marker / SATB forward, the drain; the copiers that
            // waited for another's claim; the bytes the releases freed. zt t34:
            // the plans made from the mark's liveness and those that sized the
            // live set instead, the selected regions the exact re-read took
            // out, the born-black objects scanned for legacy holders (and
            // found), and the liveness check (plans, under-estimated regions,
            // missed holders; both 0).
            let cr = h.concurrent_relocate_report();
            eprintln!(
                "[GC] zgc-concurrent-relocate: crl_enabled={} crl_cycles={} crl_refused={} \
                 crl_last_refusal={} crl_regions={} crl_trimmed={} crl_planned={} crl_planned_bytes={} \
                 crl_eager={} crl_hit={} crl_barrier={} crl_drain={} crl_waits={} \
                 crl_released_bytes={} crl_release_kept={} crl_deferred={} \
                 crl_legacy_holders={} crl_walk_checked={} crl_walk_missed={}/{}/{} \
                 crl_mark_plans={} crl_mark_fallbacks={} crl_exact_dropped={} \
                 crl_black_scanned={} crl_black_legacy={} crl_liveness_check={}/{}/{} \
                 crl_dead={}/{}/{} crl_from_writes={}",
                cr.enabled as u8,
                cr.cycles,
                cr.refused,
                cr.last_refusal,
                cr.regions,
                cr.trimmed,
                cr.planned,
                cr.planned_bytes,
                cr.eager,
                cr.hit,
                cr.barrier,
                cr.drain,
                cr.waits,
                cr.released_bytes,
                cr.release_kept,
                cr.deferred,
                cr.legacy_holders,
                cr.walk_checked,
                cr.walk_missed[0],
                cr.walk_missed[1],
                cr.walk_missed[2],
                cr.mark_plans,
                cr.mark_fallbacks,
                cr.exact_dropped,
                cr.black_scanned,
                cr.black_legacy,
                cr.check[0],
                cr.check[1],
                cr.check[2],
                cr.dead_check[0],
                cr.dead_check[1],
                cr.dead_check[2],
                cr.from_writes,
            );
            // zt t32 (`CRATONVM_ZGC_LOAD_BARRIER_MARKING`, 2f): armed, and the
            // store side's deletions it did not log (colored) and did (plain).
            let (lbm, lbm_skipped, lbm_logged) = h.load_barrier_marking_report();
            eprintln!(
                "[GC] zgc-lbm: lbm_armed={} lbm_colored_skipped={} lbm_plain_logged={}",
                lbm as u8, lbm_skipped, lbm_logged,
            );
            let sb = h.skip_bits_report();
            eprintln!(
                "[GC] zgc-skip-bits: skb_enabled={} skb_fills={} skb_members={} \
                 skb_last_members={} skb_cleared={} skb_withdrawn={}",
                sb.enabled as u8,
                sb.fills,
                sb.members,
                sb.last_members,
                sb.cleared,
                sb.withdrawn,
            );
            // PHASE G. `old_retained` is the one that says whether the phase
            // did anything: it counts the objects a young cycle kept WITHOUT
            // tracing, i.e. the tracing it did not do. A run with
            // `young_cycles>0` and `old_retained=0` did full-heap work under a
            // generational name -- which is precisely the vacuous green a
            // "generational is on" claim would otherwise be built on. Printed
            // unconditionally, so a run that never engaged the phase says so
            // instead of printing nothing.
            let (young, since_major, retained, remembered, promoted, recards) =
                h.generational_stats();
            eprintln!(
                "[GC] zgc-generational: enabled={} young_cycles={young} \
                 minors_since_major={since_major} old_retained={retained} \
                 remembered_roots={remembered} promotions={promoted} \
                 recards_after_relocation={recards}",
                h.generational_enabled(),
            );
            // THE NURSERY, beside the split it bounds. `sweep_skipped` is the
            // engagement counter: a run with `young_cycles>0` and
            // `sweep_skipped=0` swept the whole registry on every young cycle, so
            // the floor never moved and the O(young) sweep is not happening.
            let (skipped, floor, old_live) = h.nursery_stats();
            let (nursery_fired, nursery_budget) = h.nursery_trigger_stats();
            eprintln!(
                "[GC] zgc-nursery-trigger: fired={nursery_fired} budget_bytes={nursery_budget} promotions_by_slide={} \
                 overshoot_max={} nursery_warmup_held={}",
                h.promotions_by_slide(),
                // BESIDE THE BUDGET, because it is meaningless without it: this
                // is how far past `budget_bytes` the nursery got before a
                // safepoint arrived, and it prices "a hard ceiling rather than a
                // trigger" -- an open item that has had no number attached. See
                // `ZgcRealHeap::gen_nursery_overshoot_max`.
                h.nursery_overshoot_max(),
                // zt t20/gn7 (D7): `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP`'s
                // engagement.
                h.nursery_warmup_held(),
            );
            // ...AND THE ONE NUMBER THAT DECIDES THE YOUNG-SPACE PROGRAMME.
            //
            // `survivor_page_pct` is the verdict on
            // `docs/feature-designs/zgc-architecture.md`,
            // and the rule was written down before the run: **90 or above
            // refutes it** (nine nursery pages in ten hold a survivor, so a
            // page-granular young generation has almost nothing to release),
            // 60 or below supports it, in between is inconclusive.
            //
            // READ THE TWO ENGAGEMENT FIELDS BEFORE THE VERDICT. This is the
            // difference between refuting the young-space programme and
            // refuting nothing, and `survivor_cycles` alone cannot tell them
            // apart -- it reads `0` for three different situations.
            //
            //   survivor_cycles_observed=0      the census never ran. A fact
            //                                   about the WIRING; this run says
            //                                   nothing about the heap.
            //   survivor_geometry_rejected>0    a young cycle handed the census
            //                                   a frame that does not describe
            //                                   a nursery (an unset young
            //                                   floor). Not evidence; there is
            //                                   a `cratonvm::gc::guard` error
            //                                   naming the frame.
            //   survivor_cycles<3               not enough evidence yet -- the
            //                                   first young cycle after a
            //                                   whole-heap collection sees a
            //                                   nursery that has only just
            //                                   started filling.
            //
            // Past those, `survivor_page_pct >= 90` refutes and `<= 60`
            // supports. `survivor_pages=0/0` means the nursery was empty on
            // every cycle, not that every page was releasable.
            //
            // It is a bit per page rather than a survival RATE on purpose: a
            // nursery can be 1% live and still have a survivor on every page,
            // which is exactly the case that defeats page-granular reclamation
            // and exactly the case a byte-ratio calls healthy.
            //   survivor_pct_last - survivor_pct_first large
            //                                   the census RATCHETED. The
            //                                   nursery floor moves only at a
            //                                   major, and promotion on this
            //                                   backend is a header label
            //                                   rather than a copy, so a
            //                                   promoted object stays inside
            //                                   the nursery's address range
            //                                   and a page that held a
            //                                   survivor holds it for every
            //                                   later young cycle of the same
            //                                   major epoch. Later cycles also
            //                                   examine more pages, so they
            //                                   dominate the sum. A
            //                                   `survivor_page_pct >= 90`
            //                                   built on a climbing series
            //                                   refutes NOTHING. Read
            //                                   `survivor_pct_first` -- the
            //                                   one cycle whose nursery holds
            //                                   only what was allocated since
            //                                   the floor last moved.
            //
            // That fourth rule is not hypothetical and it is not a caveat on a
            // number nobody used: `proposal-d-real-young-space.md` was marked
            // REFUTED on this aggregate and the refutation was propagated to
            // the roadmap and the round summary.
            // `gc/tests/zgc_s_nursery_census_ratchet.rs` exhibits a workload
            // with a constant 1-in-5 MARGINAL survivor rate -- which would
            // strongly SUPPORT the proposal -- that this census scores
            // `Refuted`. See `handoff-s-nursery-census-per-cycle.md`.
            let surv = h.nursery_survivor_report();
            eprintln!(
                "[GC] zgc-nursery: sweep_skipped={skipped} floor={floor} \
                 old_live_bytes={old_live} \
                 survivor_pages={}/{} \
                 survivor_page_pct={} survivor_cycles={} \
                 survivor_cycles_observed={} \
                 survivor_geometry_rejected={} \
                 survivor_pct_first={} survivor_pct_last={} \
                 survivor_pct_range={}-{} survivor_ratchet_margin={}",
                surv.pages_with_survivor,
                surv.pages,
                surv.survivor_page_percent(),
                surv.cycles,
                surv.cycles_observed,
                surv.geometry_rejected,
                surv.first_cycle_percent,
                surv.last_cycle_percent,
                surv.min_cycle_percent,
                surv.max_cycle_percent,
                surv.ratchet_margin(),
            );
            // zr z1/gn: the old-start bitmap (`CRATONVM_ZGC_GEN_OLD_BITMAP`) and
            // the young-aged sweep it enables. `premarks=0` with
            // `young_cycles>0` and `enabled=1` means every young cycle fell
            // back to the header walk (`fallbacks`); `verify_unsound` must be 0
            // under `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY=1`.
            let [ob_on, ob_valid, ob_pre, ob_fb, ya_sweeps, bf_visited, bf_freed, v_unsound, v_imprecise] =
                h.gen_old_bits_stats();
            // zr z3/rv3: `young_refusals` = would-be young cycles run whole-heap
            // because the bitmap was on but not usable
            // (`CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR`); with it on,
            // `fallbacks` stays 0.
            let ob_refused = h.gen_old_bits_young_refusals();
            eprintln!(
                "[GC] zgc-gen-old-bits: old_bits_enabled={ob_on} valid={ob_valid} premarks={ob_pre} \
                 fallbacks={ob_fb} young_refusals={ob_refused} young_aged_sweeps={ya_sweeps} \
                 below_floor_visited={bf_visited} below_floor_freed_bytes={bf_freed} \
                 verify_unsound={v_unsound} verify_imprecise={v_imprecise}"
            );
            // zr z2/g2: WHERE A GENERATIONAL RUN'S COLLECTOR TIME WENT, by cycle
            // kind (`minor_*` / `major_*`), summed over the run. Filled only on
            // a generational heap when the pause clock ran (`--verbose:gc`), so
            // a quiet or non-generational run prints zeroes. `remset_us` and
            // `census_us` are parts of `mark_us` and `sweep_us`. Read
            // `minor_cycles` and `major_cycles` against the default collector's
            // cycle count first: more cycles and dearer cycles are different
            // fixes. `inline_announce_blocked` is the positive control for
            // `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE` (`-` = table unpublished).
            {
                let (
                    y,
                    m,
                    [
                        yc_sweeps,
                        yc_refusals,
                        yc_last,
                        majors_growth,
                        granules,
                        converted,
                        majors_request,
                    ],
                ) = h.gen_cost_report();
                let row = |p: &str, r: &[u64; cratonvm_gc_zgc::zgc::GEN_COST_COLUMNS]| {
                    use cratonvm_gc_zgc::zgc::{GEN_COST_CENSUS_US, GEN_COST_CYCLES, GEN_COST_DEAD, GEN_COST_MARK_US, GEN_COST_MOVED, GEN_COST_PAUSE_US, GEN_COST_PRE_US, GEN_COST_REFS_US, GEN_COST_RELOCATE_US, GEN_COST_REMSET_US, GEN_COST_STREAM_US, GEN_COST_SWEPT, GEN_COST_SWEEP_US, GEN_COST_SWEEP_WALK_US};
                    format!(
                        "{p}_cycles={} {p}_pause_us={} {p}_pre_us={} {p}_mark_us={} \
                         {p}_remset_us={} {p}_refs_us={} {p}_sweep_us={} {p}_census_us={} \
                         {p}_relocate_us={} {p}_swept={} {p}_dead={} {p}_moved={} \
                         {p}_stream_us={} {p}_sweep_walk_us={}",
                        r[GEN_COST_CYCLES],
                        r[GEN_COST_PAUSE_US],
                        r[GEN_COST_PRE_US],
                        r[GEN_COST_MARK_US],
                        r[GEN_COST_REMSET_US],
                        r[GEN_COST_REFS_US],
                        r[GEN_COST_SWEEP_US],
                        r[GEN_COST_CENSUS_US],
                        r[GEN_COST_RELOCATE_US],
                        r[GEN_COST_SWEPT],
                        r[GEN_COST_DEAD],
                        r[GEN_COST_MOVED],
                        r[GEN_COST_STREAM_US],
                        r[GEN_COST_SWEEP_WALK_US],
                    )
                };
                let blocked = match h.jit_inline_announce_blocked_report() {
                    Some(b) => (b as u8).to_string(),
                    None => "-".to_string(),
                };
                // zt t6/tc (GN-17): `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS`'s
                // engagement counters.
                let (ys_sharded, ys_shards) = h.gen_young_sweep_shard_report();
                // zt t6/tc (GN-5): `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES`'s.
                let tenures = h.gen_tenure_all_sweeps();
                // zt t7/tg (GN-12): the weak-minor census and the opt-in
                // `CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR`'s engagement.
                let (weak_minors, weak_run, weak_escalations) = h.gen_weak_minor_report();
                // zt t7/cc: `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`'s
                // engagement (`young_refs_strong_cycles`), its wrong
                // predictions and the finalizables it deferred to a major.
                let (yr_cycles, yr_mispredicted, yr_fin) = h.gen_young_refs_strong_report();
                // zt t11/gn3: `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES`'s engagement.
                let majors_rule_cause = h.gen_majors_by_rule_cause();
                // zt t14/gn5: `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION`'s.
                let empty_skipped = h.gen_empty_minor_escalations_skipped();
                eprintln!(
                    "[GC] zgc-gen-cost: {} {} complement_sweeps={yc_sweeps} \
                     complement_refusals={yc_refusals} complement_last_refusal={yc_last} \
                     majors_by_old_growth={majors_growth} element_granules_scanned={granules} \
                     element_granules_carded={converted} inline_announce_blocked={blocked} \
                     majors_by_request={majors_request} \
                     young_sweep_sharded={ys_sharded} young_sweep_shards={ys_shards} \
                     system_gc_tenures={tenures} weak_minors={weak_minors} \
                     weak_minor_max_run={weak_run} weak_minor_escalations={weak_escalations} \
                     young_refs_strong_cycles={yr_cycles} \
                     young_refs_mispredicted={yr_mispredicted} young_fin_deferred={yr_fin} \
                     majors_by_rule_cause={majors_rule_cause} \
                     empty_minor_escalations_skipped={empty_skipped}",
                    row("minor", &y),
                    row("major", &m),
                );
            }
            // zr z1/gn: the AGE-FILTERED nursery census
            // (`CRATONVM_ZGC_NURSERY_CENSUS_AGE=1`): the same instrument as the
            // line above, counting only survivors that were young when their
            // cycle began. Read `observed` first, then `first`.
            //
            // zr z2/g2: every key carries a `young_` prefix (and `enabled` is
            // `age_census_enabled`) -- the z1 line reused `[GC] zgc-nursery:`'s
            // keys verbatim, which `every_gc_summary_key_is_unique_across_the_
            // whole_summary` refuses (a `grep survivor_pct_first | tail -1`
            // picked whichever line printed last).
            let ys = h.nursery_survivor_young_report();
            eprintln!(
                "[GC] zgc-nursery-young: age_census_enabled={} young_survivor_pages={}/{} \
                 young_survivor_page_pct={} young_survivor_cycles={} \
                 young_survivor_cycles_observed={} young_survivor_geometry_rejected={} \
                 young_survivor_pct_first={} young_survivor_pct_last={} \
                 young_survivor_pct_range={}-{} young_survivor_ratchet_margin={}",
                h.nursery_census_age_enabled() as u8,
                ys.pages_with_survivor,
                ys.pages,
                ys.survivor_page_percent(),
                ys.cycles,
                ys.cycles_observed,
                ys.geometry_rejected,
                ys.first_cycle_percent,
                ys.last_cycle_percent,
                ys.min_cycle_percent,
                ys.max_cycle_percent,
                ys.ratchet_margin(),
            );
            // WHAT THE YOUNG SWEEP STOPPED DOING PER DEAD OBJECT.
            //
            // Read the two together and against `young_cycles` above.
            // `zero_bytes_skipped=0` with `young_cycles>0` means every dead
            // object was still memset in full, so `CRATONVM_ZGC_SWEEP_HEADER_ZERO`
            // is on and inert; `dead_runs == dead_objects` means no two dead
            // objects were ever adjacent, so the run merge is. Neither number
            // says it alone -- a small `dead_runs` is equally consistent with a
            // cycle that found almost no garbage.
            let (zero_skipped, dead_runs, dead_objects) = h.gen_sweep_cost_stats();
            eprintln!(
                "[GC] zgc-sweep-cost: zero_bytes_skipped={zero_skipped} \
                 dead_runs={dead_runs} dead_objects={dead_objects}",
            );
            // zr z1/al: the engagement counters of the allocation / sweep
            // switches, so a host row has a positive control and a kill-switch
            // control to read. `forget_*`: `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS`
            // (passes + declined = collections; all declined = the switch is
            // off or every cycle took a peer over). `inflight_*`:
            // `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT` (tails lowered, bytes added
            // to the withheld set -- its whole cost). `oversize_*`:
            // `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS` (dead > 256 MiB arrays
            // reclaimed from the large-object end; `refused` = still retained).
            let (fp, fd, fb) = h.forget_block_stats();
            let (il, ib) = h.inflight_cover_stats();
            let (os, osb, orf) = h.oversize_array_sweep_stats();
            // `dead_headers_skipped`: `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS`
            // (default ON since zr z8/zb); 0 on a collecting run means `=0`
            // or the corpse forensics turned the store back on.
            let dhs = h.sweep_dead_headers_skipped();
            // `huge_extent_hits` (zr z2/rv): interior probes the huge-extent
            // table answered -- derived pointers more than 256 MiB into a live
            // array, which the bounded scan alone resolved to nothing.
            let hx = h.huge_extent_hits();
            // `huge_extent_reseeds` (zr z4/ab, `CRATONVM_ZGC_HUGE_EXTENT_RESEED`;
            // zt t2/rv: it was on no line, so the reseed row had no visible
            // engagement counter): prunes that rebuilt an overflowed table.
            let hxr = h.huge_extent_reseeds();
            eprintln!(
                "[GC] zgc-alloc-sweep: forget_passes={fp} forget_declined={fd} \
                 blocks_forgotten={fb} inflight_tails_lowered={il} \
                 inflight_cover_bytes={ib} oversize_arrays_swept={os} \
                 oversize_bytes_swept={osb} oversize_refused={orf} \
                 dead_headers_skipped={dhs} huge_extent_hits={hx} \
                 huge_extent_reseeds={hxr}",
            );
            // zr z8/st: `CRATONVM_ZGC_LAZY_TLAB_STARTS` (default off; every
            // figure 0 and `lz_mode=off` then). `lz_unregistered_found` is the
            // walk's whole disagreement with the per-allocation bit (0 expected
            // apart from dead objects whose announce failed); `lz_q_own` /
            // `lz_q_peer` are oracle hits inside a live chunk (the proposal's
            // refutation census); `lz_q_mismatch` must read 0.
            let lz = h.lazy_tlab_starts_report();
            eprintln!(
                "[GC] zgc-lazy-starts: lz_mode={} lz_chunks_live={} lz_retire_walks={} \
                 lz_stw_walks={} lz_objects_walked={} lz_already_registered={} \
                 lz_unregistered_found={} lz_walk_stops={} \
                 lz_chunks_dropped={} lz_q_own={} lz_q_peer={} lz_q_checked={} \
                 lz_q_mismatch={} lz_q_steps={} lz_q_capped={} lz_chunks_unpublished_dropped={}",
                lz.mode,
                lz.chunks_live,
                lz.retire_walks,
                lz.stw_walks,
                lz.objects_walked,
                lz.already_registered,
                lz.unregistered_found,
                lz.walk_stops,
                lz.chunks_dropped,
                lz.q_own,
                lz.q_peer,
                lz.q_checked,
                lz.q_mismatch,
                lz.q_steps,
                lz.q_capped,
                lz.chunks_unpublished_dropped,
            );
            // zr z5/sw: `CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`'s engagement.
            // `prune_key_cycles` > 0 on any run with an inflated monitor (the
            // key probe replaced the dead list); `region_words_skipped` > 0
            // only with `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` on as well
            // (the default since zr z8/zb);
            // all five read 0 at `=0`.
            let (kc, kp, kd, rw, mf) = h.sweep_live_proportional_stats();
            eprintln!(
                "[GC] zgc-sweep-live: prune_key_cycles={kc} keys_probed={kp} \
                 keys_dead={kd} region_words_skipped={rw} mark_blocks_forgotten={mf}",
            );
            // zr z2/cp: `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT`'s engagement.
            // `after_failure_slides` is the positive control of any row that
            // raised (or used to raise) an OOME on fragmentation; 0 there with
            // the switch unset means the widened selector never ran.
            let (pf, pt, pb) = h.relocation_pressure_select_stats();
            eprintln!(
                "[GC] zgc-capacity: after_failure_slides={pf} tight_slides={pt} \
                 widened_live_bytes={pb} refusals_seen={} \
                 infallible_unwind_offers={}",
                h.trigger_tallies().4,
                // `CRATONVM_ZGC_INFALLIBLE_OOME_UNWIND`: infallible arms that
                // offered the VM's OOME unwind instead of aborting outright.
                h.exhaustion_unwind_offers(),
            );
            // zr z9/sl: `CRATONVM_ZGC_SELECTIVE_RELOCATE`'s engagement.
            // `sel_slides` > 0 only with the switch on (the positive control);
            // `sel_pages_kept` = sparse pages left in place because their holes
            // were already usable; `sel_emptied` = slides it declined outright;
            // `sel_failure_bypass` = after-failure slides that ran the full
            // selector (the fallback). `sel_bytes_moved` is the slides' copy
            // volume on EVERY arm (both ends), the number `bytes_copied=` on
            // `[GC] zgc-real:` is not.
            let (ss, sk, se, sb, sm) = h.selective_relocate_stats();
            // zs s1/sl: the cycle gate (`=2`) and the fused census.
            // `sel_cycle_*` > 0 only on `=2`; `sel_fused` = selective slides
            // whose census rode `logical_pages` (0 under
            // `CRATONVM_ZGC_SELECTIVE_FUSED_FRAG=0`, the positive control).
            let (ca, cd, ct, cf) = h.selective_cycle_stats();
            eprintln!(
                "[GC] zgc-selective: sel_slides={ss} sel_pages_kept={sk} \
                 sel_emptied={se} sel_failure_bypass={sb} sel_bytes_moved={sm} \
                 sel_cycle_admit={ca} sel_cycle_decline={cd} sel_cycle_tight={ct} \
                 sel_fused={cf}",
            );
            // zr z3/fp: what the collector holds, per structure, so a host
            // maxrss can be attributed (`docs/feature-designs/zgc-architecture.md`). Bytes throughout.
            // `*_reserved` is address space (free until written), `*_noted`
            // its estimated resident part; `fp_arena_written` bounds the arena's
            // resident set from above; `fp_side_resident` sums the side
            // structures. `fp_trims` / `fp_idle_stamps` / `fp_sparse_skipped`
            // are the engagement counters of `CRATONVM_ZGC_FOOTPRINT_TRIM`,
            // `CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP` and
            // `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`.
            let fp = h.footprint_stats();
            eprintln!(
                "[GC] zgc-footprint: fp_arena_reserved={} fp_arena_committed={} \
                 fp_arena_committed_peak={} fp_arena_written={} fp_arena_uncommitted={} \
                 fp_arena_reset={} fp_arena_bookkeeping={} fp_start_bits_reserved={} \
                 fp_start_bits_noted={} fp_mark_bits_reserved={} fp_mark_bits_noted={} \
                 fp_old_bits_reserved={} fp_old_bits_noted={} fp_grids={} \
                 fp_relocations_entries={} fp_relocations_bytes={} fp_relocations_peak={} \
                 fp_dead_scratch_bytes={} fp_dead_scratch_peak={} fp_snapshot_peak={} \
                 fp_remset_bytes={} fp_side_resident={} fp_trims={} fp_trimmed_bytes={} \
                 fp_idle_stamps={} fp_sparse_skipped={} fp_trim_regrows={}",
                fp.arena_reserved,
                fp.arena_committed,
                fp.arena_committed_peak,
                fp.arena_ever_written,
                fp.arena_uncommitted_total,
                fp.arena_reset_total,
                fp.arena_bookkeeping,
                fp.start_bits_reserved,
                fp.start_bits_noted,
                fp.mark_bits_reserved,
                fp.mark_bits_noted,
                fp.old_bits_reserved,
                fp.old_bits_noted,
                fp.grids,
                fp.relocations_entries,
                fp.relocations_bytes,
                fp.relocations_peak,
                fp.dead_scratch_bytes,
                fp.dead_scratch_peak,
                fp.snapshot_peak,
                fp.remset_bytes,
                fp.side_resident_estimate(),
                fp.trims,
                fp.trimmed_bytes,
                fp.idle_stamps,
                fp.sparse_words_skipped,
                // zt t8/rl2: trims the next slide undid (see
                // `CRATONVM_ZGC_RELOCATIONS_TRIM_HYSTERESIS`).
                fp.trim_regrows,
            );
            // zr z7/rs: where the arena and the bitmaps LIVE, so
            // `tools/rss-by-mapping.sh` can attribute the smaps mappings that
            // hold them (`gc:java-heap`, `gc:bitmaps`) instead of counting
            // them as the allocator's. `arena_no_thp` is
            // `CRATONVM_ZGC_ARENA_NO_THP`'s outcome (0 unset, 1 applied,
            // 2 refused, 3 yielded to `CRATONVM_HEAP_THP`).
            eprintln!(
                "[GC] zgc-footprint-spans: {} at=exit",
                h.footprint_spans().render()
            );
            // zr z4/un: the uncommitter (`CRATONVM_ZGC_UNCOMMITTER`), the net
            // commitment (`CRATONVM_ZGC_RESET_ACCOUNTING`, what
            // `Runtime.totalMemory()` reads) and the slide's transient
            // bookkeeping peaks (`CRATONVM_ZGC_SLIDE_BUFFER_TRIM`). Bytes,
            // except the pass counts.
            eprintln!(
                "[GC] zgc-uncommitter: un_running={} un_passes={} un_passes_released={} \
                 un_reset={} un_decommitted={} un_busy={} un_net_committed={} \
                 un_slide_pairs_peak={} un_slide_index_peak={} un_slide_map_peak={} \
                 un_slide_transient_peak={} un_bitmap_released={} un_prefaulted={}",
                h.uncommitter_running() as u8,
                fp.uncommit_passes,
                fp.uncommit_passes_released,
                fp.uncommit_reset_bytes,
                fp.uncommit_decommitted_bytes,
                fp.uncommit_busy,
                fp.arena_net_committed,
                fp.slide_pairs_peak,
                fp.slide_index_peak,
                fp.slide_map_peak,
                fp.slide_transient_peak,
                // zt t3/rl: `CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`.
                fp.bitmap_pages_released,
                // jr21 gc: bytes the pre-fault worker populated ahead of the
                // low cursor (`CRATONVM_ZGC_PREFAULT_AHEAD`).
                h.prefaulted_bytes(),
            );
            // `ZGC_UNSIZABLE_OBJECTS` had no reader anywhere but a unit test.
            // It is the sweep's own count of registered objects whose header it
            // could not size -- i.e. of heap corruption the collector has
            // already met and silently worked around, one warning per process.
            // A run that ends with a nonzero here has corrupt headers whatever
            // else it reports.
            // A NOTIFICATION THAT IS MISSING IS A SLOWDOWN, NOT A FAILURE.
            // The mark driver's fixed-point wait is a `wait_for`, so a lost
            // notification costs a poll interval and is otherwise
            // indistinguishable from a working one -- which is how the driver
            // came to poll a never-notified condvar on a 5 ms grid for months.
            // Nonzero here means it is back. A count, so it reads the same on a
            // loaded host as on a quiet one.
            // THE OVERLAY GATE, asked of the provider rather than measured here
            // -- see `ExternalRootProvider::gate_stats`. `roots_for_owner` runs
            // once per marked object and was 20-29% of mark samples on both the
            // serial and the parallel arm (2026-08-17 `perf record`). A gate that
            // works and a gate that is inert return the same empty `Vec`, and the
            // previous attempt at this optimisation WAS inert, so `hits` is the
            // only thing that separates them. `disabled=true` means an owner was
            // registered with no class id and the gate has failed safe.
            for (name, hits, misses, disabled) in cratonvm_gc_core::external_roots::provider_gate_stats() {
                eprintln!(
                    "[GC] zgc-overlay-gate: provider={name} hits={hits} misses={misses} disabled={disabled}",
                );
            }
            eprintln!(
                "[GC] zgc-mark-wait: park_timeouts={}",
                h.mark_park_timeouts(),
            );
            eprintln!(
                "[GC] zgc-integrity: unsizable_registered_objects={}",
                h.unsizable_registered_objects(),
            );
            let g = h.frag_gauge();
            match g.worst_permille {
                Some(worst) => eprintln!(
                    "[GC] zgc-frag: frag_samples={} worst_largest_free_permille={} free_permille_at_worst={} at_cycle={}",
                    g.samples, worst, g.free_permille, g.worst_cycle,
                ),
                None => eprintln!(
                    "[GC] zgc-frag: frag_samples=0 worst_largest_free_permille=n/a (no collection left >=25% of the heap free; this run is not evidence either way)"
                ),
            }
        }
        // gen r4w4/alloc4: the TLAB census on G1 and ZGC too — residue (b) of
        // the write-only-instruments page. The census is process-wide and every
        // backend's TLABs retire through `tlab.rs`, and `FILLER_OVER_OBJECT` can
        // fire on any of them, but the two lines were printed only in the
        // Generational arm below, so a G1 or ZGC summary said nothing about its
        // own TLAB waste, sizer or tripwires. Same two lines, same renderer;
        // `vm-cli`'s fired-tripwire print now defers to every backend's summary.
        if !matches!(&self.inner, VmHeapInner::Generational(_)) {
            // gci i48/zl3: `sizer_on=` resolves G1's default on a G1 heap.
            let [tlab_waste, tlab_guard] =
                cratonvm_gc_core::tlab::tlab_census_lines_for_backend(false, self.is_g1());
            eprintln!("{tlab_waste}");
            eprintln!(
                "{}",
                render_tlab_guard_line(&tlab_guard, cratonvm_gc_core::tlab::tlab_guard_line_is_urgent()),
            );
        }
        // Collection COUNTS, unconditionally. Without these the summary is not
        // comparable across configurations: the moving-young line below only
        // prints when moving-young is requested, so a `CRATONVM_NO_MOVING_YOUNG`
        // run printed nothing at all and "did this config collect more?" — the
        // first question to ask about an allocation-heavy regression — could not
        // be answered from a log. Cheap: two relaxed loads at shutdown.
        if let VmHeapInner::Generational(h) = &self.inner {
            let s = h.stats().snapshot();
            eprintln!(
                "[GC] generational: minor={} major={}",
                s.minor_gc_count, s.major_gc_count,
            );
            // gen r4w2/obs (2026-09-23): three instruments wave 1 added and
            // nothing printed.
            //
            // The young trigger (lane E): the live threshold, what set it and
            // what the pause-goal loop did to it. Formatted in `gen_heap.rs`
            // (`YoungTriggerCensus::line`); its keys are prefixed
            // (`young_trigger_*`, `adapt_*`) and are in this file's
            // key-uniqueness corpus.
            eprintln!("{}", h.young_trigger_census_line());
            // gcd d9/b: the young trigger's floor on a wedged old generation
            // (`gen_heap::young_trigger_floor_judged`, keys `ytw_*`) and the
            // allocation-failure ladder's work per heap `OutOfMemoryError`
            // (`GenerationalHeap::oome_ladder_census_line`, keys `ladder_*`).
            // Both formatted in `gen_heap.rs` and in the uniqueness corpus.
            eprintln!("{}", h.young_trigger_wedged_census_line());
            eprintln!("{}", h.oome_ladder_census_line());
            // gen r4w4/young4 (2026-09-24): the young cycles the conservative
            // JIT-root term (`nonmoving-unrewritable-conservative-jit-roots`)
            // diverted, and what a pin-aware copying cycle would have had to
            // leave in place on them. See
            // `GenerationalHeap::conservative_divert_census`.
            let [cj_div, cj_zero, cj_ysum, cj_ymax, cj_psum, cj_noveto] =
                h.conservative_divert_census();
            // gen r4w5/pinwords5: the young pin-word ledger on the same cycles
            // (option B's price; process-wide, see
            // `gc_quiescence::young_pin_ledger_census`). `cjdiv_ledger_empty` is
            // how many term-4 cycles option B alone would have let copy;
            // `cjdiv_ledger_cleared` how many it did
            // (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`).
            // gcj j1pn ledger item 2: `cjdiv_ledger_screened_out_sum` is the
            // to-space / above-top words the from-space screen dropped.
            let [
                lg_evals,
                lg_incomplete,
                lg_empty,
                lg_wsum,
                lg_wmax,
                lg_native,
                lg_cleared,
                lg_screened,
            ] = cratonvm_gc_core::gc_quiescence::young_pin_ledger_census();
            // gcj j1pn ledger item 3: initiator deposits that skipped the
            // pause door's own frames, and the bytes skipped.
            let (lg_cuts, lg_cut_bytes) = cratonvm_gc_core::gc_quiescence::young_pin_collector_cut_census();
            eprintln!(
                "[GC] young_conservative_divert: cjdiv_diverts={cj_div} \
                 cjdiv_no_young_pin={cj_zero} cjdiv_young_pins_sum={cj_ysum} \
                 cjdiv_young_pins_max={cj_ymax} cjdiv_pins_sum={cj_psum} \
                 cjdiv_no_initiator_veto={cj_noveto} cjdiv_ledger_evals={lg_evals} \
                 cjdiv_ledger_incomplete={lg_incomplete} cjdiv_ledger_empty={lg_empty} \
                 cjdiv_ledger_words_sum={lg_wsum} cjdiv_ledger_words_max={lg_wmax} \
                 cjdiv_ledger_native_sum={lg_native} cjdiv_ledger_cleared={lg_cleared} \
                 cjdiv_ledger_screened_out_sum={lg_screened} \
                 cjdiv_ledger_collector_cuts={lg_cuts} \
                 cjdiv_ledger_collector_cut_bytes={lg_cut_bytes}"
            );
            // gen r4w5/pinned5 (2026-09-24): the pinned in-place young copy
            // (`CRATONVM_GEN_PINNED_YOUNG_COPY`) — the term-4 cycles it took,
            // the ones it declined, and what it left in place. See
            // `GenerationalHeap::pinned_young_census`.
            let [py_cycles, py_nowords, py_bound, py_ledger, py_psum, py_pmax, py_osum, py_bsum, py_copies, py_overflow, py_kept] =
                h.pinned_young_census();
            // gch h2/yg: of `pycopy_ledger_incomplete`, the declines with raw
            // JNI locals open (`GenerationalHeap::pinned_ledger_incomplete_raw_jni`).
            let py_raw_jni = h.pinned_ledger_incomplete_raw_jni();
            eprintln!(
                "[GC] young_pinned_copy: pycopy_cycles={py_cycles} \
                 pycopy_no_young_words={py_nowords} pycopy_over_bound={py_bound} \
                 pycopy_ledger_incomplete={py_ledger} pycopy_pages_sum={py_psum} \
                 pycopy_pages_max={py_pmax} pycopy_objects_sum={py_osum} \
                 pycopy_bytes_sum={py_bsum} pycopy_inplace_copies={py_copies} \
                 pycopy_overflow_promotions={py_overflow} pycopy_kept={py_kept} \
                 pycopy_incomplete_raw_jni={py_raw_jni}"
            );
            // gch h2/yg (gcd d1e proposal): relocating cycles whose blocked-peer
            // capture held a young non-start word, and how many of them pinned it
            // (`GenerationalHeap::blocked_peer_interior_census`).
            let [bpw_cycles, bpw_with, bpw_words, bpw_pinned] = h.blocked_peer_interior_census();
            eprintln!(
                "[GC] young_blocked_peer_words: bpw_relocating_cycles={bpw_cycles} bpw_cycles_with_words={bpw_with} bpw_words_sum={bpw_words} bpw_cycles_pinned={bpw_pinned}"
            );
            // gcf f2/y: the pinned copy's defragmentation gate
            // (`CRATONVM_GEN_PINNED_COPY_DEFRAG`), formatted in `gen_heap.rs`.
            eprintln!("{}", h.pinned_copy_defrag_census_line());
            // gcj j1/yg: which term kept each term-4 cycle from the pinned
            // copy (`CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK`).
            eprintln!("{}", h.term4_offer_census_line());
            // gcj yg3-residuals R3.
            eprintln!("{}", h.young_geometry_census_line());
            // gcg g1/y: `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`'s engagement
            // (`[GC] pinned_exact_roots:`), formatted in `gen_heap.rs`.
            eprintln!("{}", h.pinned_exact_roots_census_line());
            // gcf f3/q: the young-premium cuts of the non-moving sweep
            // (`CRATONVM_GEN_WALK_SIZE_MEMO` and friends), formatted in
            // `gen_heap.rs`.
            eprintln!("{}", h.young_premium_census_line());
            // gcd d3/m: the opt-in pinned-copy take-over arm
            // (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`).
            let [pt_cand, pt_taken, pt_incomplete, pt_over] = h.pinned_takeover_census();
            eprintln!(
                "[GC] young_pinned_takeover: ptko_candidates={pt_cand} ptko_taken={pt_taken} ptko_ledger_incomplete={pt_incomplete} ptko_over_bound={pt_over}"
            );
            // gcj j1/yg item 1: the opt-in unproven-frame arm
            // (`CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES`).
            let [pu_cand, pu_taken, pu_incomplete, pu_over] = h.pinned_unproven_census();
            eprintln!(
                "[GC] young_pinned_unproven: pyun_candidates={pu_cand} pyun_taken={pu_taken} pyun_ledger_incomplete={pu_incomplete} pyun_over_bound={pu_over}"
            );
            let [ur_reads, ur_dep, ur_depth, ur_orphan, ur_bounds, ur_jni, ur_tko, ur_cov, ur_peer, ur_ovf, ur_marked] =
                cratonvm_gc_core::gc_quiescence::young_pin_unproven_read_census();
            eprintln!(
                "[GC] young_pinned_unproven_reads: pyur_reads={ur_reads} pyur_deposit_incomplete={ur_dep} pyur_depth_short={ur_depth} pyur_orphaned={ur_orphan} pyur_bounds={ur_bounds} pyur_raw_jni={ur_jni} pyur_takeover={ur_tko} pyur_coverage={ur_cov} pyur_peer_stacks={ur_peer} pyur_deposits_overflowed={ur_ovf} pyur_deposits_marked={ur_marked}"
            );
            // gcd d5/s: why the take-over arm declined
            // (`GenerationalHeap::pinned_takeover_declines`).
            let [tk_reached, tk_none, tk_frozen, tk_unread, tk_refused, tk_derived, tk_cov, tk_div] =
                h.pinned_takeover_declines();
            eprintln!(
                "[GC] young_pinned_takeover_declines: ptkod_reached={tk_reached} ptkod_no_takeover_peer={tk_none} ptkod_frozen={tk_frozen} ptkod_unread={tk_unread} ptkod_window_refused={tk_refused} ptkod_derived_unresolved={tk_derived} ptkod_other_coverage={tk_cov} ptkod_other_divert={tk_div}"
            );
            // gen r5w6/pin10: header screens that refused a forward chain or a
            // BUSY mark (`gen_heap::header_screen_counts`).
            let (hs_fwd, hs_busy, hs_abandoned) = cratonvm_gc_gen::gen_heap::header_screen_counts();
            eprintln!(
                "[GC] header_screens: hscr_forward_declined={hs_fwd} hscr_busy_declined={hs_busy} hscr_busy_wait_abandoned={hs_abandoned}"
            );
            // gcd d2/h: precise young roots the object-start walk did not
            // record (`GenerationalHeap::unrecorded_young_root_census`).
            let [ur_refused, ur_open, ur_roots] = h.unrecorded_young_root_census();
            let ur_ledger = h.unrecorded_young_root_ledger_guarded_cycles();
            // gch h1/yp: offenders the guard's exclusion set absorbed
            // (`GenerationalHeap::unrecorded_young_root_exclusion_census`).
            let [ur_ex_cycles, ur_ex_roots] = h.unrecorded_young_root_exclusion_census();
            // gch h2/yg: the longest run of consecutive refused cycles
            // (`GenerationalHeap::unrecorded_young_root_max_streak`).
            let ur_streak = h.unrecorded_young_root_max_streak();
            // gci i10/gr: cycles the dirty-card / overlay value guard ran on
            // (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`; 0 with it off).
            let ur_cards = h.unrecorded_young_root_cards_guarded_cycles();
            eprintln!(
                "[GC] young_unrecorded_roots: urr_cycles_refused={ur_refused} urr_failed_open={ur_open} urr_roots_sum={ur_roots} urr_ledger_guarded={ur_ledger} urr_excluded_cycles={ur_ex_cycles} urr_excluded_roots={ur_ex_roots} urr_max_streak={ur_streak} urr_cards_guarded={ur_cards}"
            );
            // gen r4w4/alloc4: which allocation door this heap's objects took
            // (TLAB misses, old-gen direct), the refill outcomes, and the
            // zero-once / humongous-unlocked memset census. Formatted in
            // `gen_heap.rs` (`GenerationalHeap::alloc_path_census_line`, wave 3
            // left it unprinted); its keys are in the uniqueness corpus below.
            eprintln!("{}", h.alloc_path_census_line());
            // The collections as the two JMX collector beans report them
            // (`Copy` / `MarkSweepCompact`, `gc_metrics::serial_collectors`), so
            // a probe reading `GarbageCollectorMXBean` counts has the VM-side
            // figure to compare against. `minor=` above is NOT the `Copy` count:
            // it is also bumped by cycles that ran old gen.
            let [young_bean, full_bean] = h.jmx_collectors();
            eprintln!(
                "[GC] jmx_collectors: copy_collections={} copy_time_ms={} \
                 marksweepcompact_collections={} marksweepcompact_time_ms={}",
                young_bean.count, young_bean.time_ms, full_bean.count, full_bean.time_ms,
            );
            // The card table's self-maintained scan bound (lane D). A zero
            // `cards_skipped_by_scan_bound` on a run that collected means the
            // bound is inert; `raw_card_address_escaped=true` says why (the JIT
            // was handed the raw map, which disarms the bound for good).
            let (bound, num_cards, skipped, escaped) = h.card_scan_bound_census();
            eprintln!(
                "[GC] cards/scan_bound: scan_bound_cards={bound} num_cards={num_cards} \
                 cards_skipped_by_scan_bound={skipped} raw_card_address_escaped={escaped}"
            );
            // gen r4w3/obs2 (2026-09-23): the wave-2 instruments nobody printed
            // (cross-lane requests of the youngpolicy, oldgen2 and concmark
            // reviews). Every key carries its line's prefix, so it stays unique
            // across the summary.
            //
            // Promote-on-pressure arms (lane youngpolicy): armed, consumed by a
            // moving cycle, consumed after being carried across a non-moving
            // cycle, dropped at a divert (`CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES`).
            let (pp_armed, pp_consumed, pp_stale, pp_dropped) = h.promote_pressure_census();
            eprintln!(
                "[GC] promote_pressure: promote_pressure_armed={pp_armed} \
                 promote_pressure_consumed={pp_consumed} \
                 promote_pressure_consumed_stale={pp_stale} \
                 promote_pressure_dropped_on_divert={pp_dropped}"
            );
            // The old-gen trigger's verdicts and the old generation's backing
            // store (lane oldgen2). One old-gen lock at shutdown.
            let (trig, oldgen_committed, oldgen_given_back) = h.old_gen_trigger_stats();
            eprintln!(
                "[GC] oldgen_trigger: trig_collections={} trig_low_yield={} trig_requested={} \
                 trig_occupancy={} trig_alloc_failure={} trig_suppressed={} \
                 trig_would_suppress={} oldgen_alloc_failures={} oldgen_commit_refusals={} \
                 oldgen_allocs={} oldgen_nonfit_probes={} oldgen_committed_bytes={oldgen_committed} \
                 oldgen_given_back_bytes={oldgen_given_back}",
                trig.collections,
                trig.low_yield,
                trig.requested,
                trig.occupancy,
                trig.allocation_failure,
                trig.suppressed,
                trig.would_suppress,
                trig.alloc_failures,
                trig.commit_refusals,
                trig.allocs,
                trig.nonfit_probes,
            );
            // gen r5w3/obs7 (cross-lane request from oldgen7,
            // `gengc-r5w3-oldgen7-old-gen-sizing-stats-are-never-printed`): the
            // old generation's sizing counters, which no production code
            // printed, so no flag-on/flag-off A/B could read them. Keys are
            // `oldsz_`-prefixed (unique across the summary). One more old-gen
            // lock at shutdown.
            let s = h.old_gen_sizing_stats();
            eprintln!(
                "[GC] oldgen_sizing: oldsz_reserved={} oldsz_committed={} oldsz_committed_peak={} \
                 oldsz_commit_floor={} oldsz_shrinks={} oldsz_shrunk_bytes={} oldsz_shrinks_damped={} \
                 oldsz_conc_resizes={} oldsz_conc_shrinks={} oldsz_interior_decommits={} \
                 oldsz_interior_decommitted_bytes={} oldsz_interior_damped={} \
                 oldsz_hole_recommits={} oldsz_frag_refusals={} \
                 oldsz_frag_compactions_requested={} oldsz_compactions={} \
                 oldsz_pinned_compactions={} oldsz_pinned_moved_bytes={} oldsz_pinned_refusals={} oldsz_humongous_requests={} \
                 oldsz_walk_gap_recoveries={} oldsz_borrow_growths={} oldsz_borrowed_bytes={} \
                 oldsz_growth_max={} oldsz_humongous_top_allocs={} \
                 oldsz_humongous_top_fallbacks={} oldsz_walk_breaks_planted={} \
                 oldsz_moving_major_vetoes={} oldsz_true_root_majors={} \
                 oldsz_true_root_fallbacks={} oldsz_true_root_young_excluded={} \
                 oldsz_true_root_fb_walk_gap={} oldsz_true_root_fb_promotion_off_grid={} \
                 oldsz_true_root_fb_promoted={} oldsz_true_root_fb_pinned_plan={} \
                 oldsz_true_root_fb_moving_cycle={} oldsz_true_root_fb_compacting={} \
                 oldsz_true_root_promotions={} \
                 oldsz_true_root_promotions_dead={} oldsz_requested_major_compactions={} \
                 oldsz_requested_major_compaction_declines={} oldsz_mark_admission_bitmaps={} \
                 oldsz_pinned_compaction_movable_words_unpinned={} \
                 oldsz_moving_legacy_seed_majors={}",
                s.reserved_bytes,
                s.committed_bytes,
                s.committed_peak,
                s.commit_floor,
                s.shrinks,
                s.shrunk_bytes,
                s.shrinks_damped,
                s.concurrent_sweep_resizes,
                s.concurrent_sweep_shrinks,
                s.interior_decommits,
                s.interior_decommitted_bytes,
                s.interior_decommits_damped,
                s.hole_recommits,
                s.fragmentation_refusals,
                s.fragmentation_compactions_requested,
                s.compactions,
                s.pinned_compactions,
                s.pinned_compaction_moved_bytes,
                s.pinned_compaction_refusals,
                s.humongous_compaction_requests,
                s.walk_gap_recoveries,
                s.borrow_growths,
                s.borrowed_bytes,
                s.growth_max_bytes,
                s.humongous_top_allocs,
                s.humongous_top_fallbacks,
                s.walk_breaks_planted,
                s.moving_major_compaction_vetoes,
                s.true_root_majors,
                s.true_root_fallbacks,
                s.true_root_young_excluded,
                s.true_root_fallbacks_for(cratonvm_gc_gen::old_gen::TrueRootFallback::WalkGap),
                s.true_root_fallbacks_for(cratonvm_gc_gen::old_gen::TrueRootFallback::PromotionOffGrid),
                s.true_root_fallbacks_for(cratonvm_gc_gen::old_gen::TrueRootFallback::Promoted),
                s.true_root_fallbacks_for(cratonvm_gc_gen::old_gen::TrueRootFallback::PinnedPlan),
                s.true_root_fallbacks_for(cratonvm_gc_gen::old_gen::TrueRootFallback::MovingCycle),
                s.true_root_fallbacks_for(cratonvm_gc_gen::old_gen::TrueRootFallback::Compacting),
                s.true_root_promotions,
                s.true_root_promotions_dead,
                s.requested_major_compactions,
                s.requested_major_compaction_declines,
                s.mark_admission_bitmaps,
                s.pinned_compaction_movable_words_unpinned,
                s.moving_legacy_seed_majors,
            );
            // gcj j1/og (cross-lane X1): engagement of the old-gen compaction
            // forward index and of the free-ratio growth.
            eprintln!(
                "[GC] oldgen_j1og: oldsz_forward_index_compactions={} oldsz_free_ratio_growths={} \
                 oldsz_free_ratio_grown_bytes={} oldsz_par_adjust_compactions={} \
                 oldsz_free_ratio_growths_after_concurrent={}",
                s.forward_index_compactions, s.free_ratio_growths, s.free_ratio_grown_bytes,
                s.par_adjust_compactions, s.free_ratio_growths_after_concurrent,
            );
            // gcj j3/og3 (cross-lane X1): the Phase 0 drop, the Serial-shaped
            // trigger and capacity-shrink counters.
            eprintln!(
                "[GC] oldgen_j3og3: oldsz_phase0_skips={} oldsz_trust_mark_disagreements={} oldsz_trust_mark_verifications={} oldsz_compact_escape_hits={} \
                 oldsz_serial_trigger_due={} oldsz_serial_trigger_over_floor_skips={} \
                 oldsz_capacity_shrinks={} oldsz_capacity_shrunk_bytes={} \
                 oldsz_frag_window_compactions={} oldsz_frag_window_not_needed={}",
                s.phase0_skips, s.trust_mark_disagreements, s.trust_mark_verifications,
                cratonvm_gc_gen::old_gen::COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed),
                s.serial_trigger_due,
                s.serial_trigger_over_floor_skips, s.capacity_shrinks, s.capacity_shrunk_bytes,
                s.frag_window_compactions, s.frag_window_not_needed,
            );
            eprintln!(
                "[GC] serial_pool_accounting: snapshot_fallbacks={}",
                h.serial_pool_snapshot_fallbacks(),
            );
            // gch h4/wr: the engagement counter of
            // `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD` (watched young
            // survivors the true-root majors judged dead), on a line of its own
            // so the sizing line's key list stays the old generation's.
            eprintln!(
                "[GC] true_root_watched: oldsz_true_root_watched_dead={}",
                h.true_root_watched_left_out_total(),
            );
            // The sliced concurrent old-gen cycle (lane concmark). Process-wide
            // counters (`concurrent_mark.rs` statics), printed on this arm
            // because the generational driver is the one that bumps them.
            {
                use cratonvm_gc_gen::concurrent_mark as cm;
                use std::sync::atomic::Ordering as O;
                eprintln!(
                    "[GC] conc_mark: conc_mark_slices={} conc_remark_retries={} \
                     conc_remark_abandons={} conc_cycle_stale_abandons={} \
                     conc_cycle_open_refused={} conc_mark_walks_built={} \
                     conc_mark_walks_reused={} conc_mark_walk_desync_aborts={} conc_mark_walk_gap_recoveries={}",
                    cm::CONC_MARK_SLICES.load(O::Relaxed),
                    cm::CONC_REMARK_RETRIES.load(O::Relaxed),
                    cm::CONC_REMARK_ABANDONS.load(O::Relaxed),
                    cm::CONC_CYCLE_STALE_ABANDONS.load(O::Relaxed),
                    cm::CONC_CYCLE_OPEN_REFUSED.load(O::Relaxed),
                    cm::CONC_MARK_WALKS_BUILT.load(O::Relaxed),
                    cm::CONC_MARK_WALKS_REUSED.load(O::Relaxed),
                    cm::CONC_MARK_WALK_DESYNC_ABORTS.load(O::Relaxed),
                    // gen r5w6/sizer10 (old9's cross-lane request 2): remarks
                    // that recovered an unwalked gap conservatively instead of
                    // refusing the sweep (`concurrent_mark.rs`).
                    cm::CONC_MARK_WALK_GAP_RECOVERIES.load(O::Relaxed),
                );
            }
            // gen r5w6/sizer10 (pin9's request): candidates
            // `GenerationalHeap::is_object_address` declined because their
            // header reads FORWARDED and the forward leaves this heap (the
            // netty `0x100000004` SIGSEGV family). Process-wide, like the counters
            // above; printed only when non-zero, so a clean run's summary is
            // unchanged.
            {
                let declined = cratonvm_gc_gen::gen_heap::FORWARD_SCREEN_DECLINED
                    .load(std::sync::atomic::Ordering::Relaxed);
                if declined != 0 {
                    eprintln!("[GC] forward_screen: fwd_screen_declined={declined}");
                }
            }
            // gen r4w4/concmark4: the old-gen policy (concurrent-first vs
            // legacy), its start threshold, and the per-VM cycle census —
            // including the initial-mark takeover numbers wave 3 asked for.
            if let Some(line) = h.concurrent_policy_census_line() {
                eprintln!("{line}");
            }
            // gen r4w5/concmark5: the concurrent-cycle driver (service thread,
            // remark reference-processing hook) and the STW old-gen trigger's
            // count-based cadence (majors per 100 young decisions, back-to-back
            // majors, majors under 5 % yield). Keys `concdrv_` / `majcad_`.
            if let Some(line) = h.concurrent_driver_census_line() {
                eprintln!("{line}");
            }
            if let Some(line) = h.major_cadence_census_line() {
                eprintln!("{line}");
            }
            // The shared SATB queue's two gate-ordering detectors
            // (`gengc-r4-mark-counters-and-comments-that-lie`): a log that
            // arrived after the queue was deactivated, and a deactivation
            // that timed out waiting for in-flight barriers. Non-zero on
            // either is a finding. Absent until the concurrent collector is
            // wired (`enable_concurrent_gc`).
            if let Some(q) = h.satb_queue_ref() {
                eprintln!(
                    "[GC] satb_queue: satb_late_log_drops={} satb_quiescence_timeouts={}",
                    q.late_log_drops(),
                    q.quiescence_timeouts(),
                );
            }
            // GC notifications (this lane): printed once any bean has had a
            // listener, so a run that never used them keeps its summary.
            let notif = h.gc_notifications().census();
            if notif.queued != 0 || notif.deferred != 0 {
                eprintln!("{}", notif.summary_line());
            }
            // Re-publish the normalization denominators so the card-cost report
            // below divides by the CURRENT heap rather than by whatever the
            // last collection saw. Cheap: two arena locks at shutdown.
            h.publish_gc_metrics_occupancy();
        }
        // gc-common w7-f: the same GC-notification census for a backend whose
        // beans the event plumbing books (G1 here; ZGC's state lives in the VM
        // and is not reachable from this summary), under the same rule.
        if let Some(b) = self.backend_gc_beans() {
            // The collections as G1's three JMX collector beans report them,
            // unconditionally (the VM-side figure a `GarbageCollectorMXBean`
            // probe compares against, like Generational's `jmx_collectors:`).
            // Not `collections=` of the G1 collector's own counters: those
            // also count evacuation-failure drain passes inside one pause.
            if let [young, old, conc] = b.collectors().as_slice() {
                eprintln!(
                    "[GC] g1_jmx_collectors: g1_young_collections={} g1_young_time_ms={} \
                     g1_old_collections={} g1_concurrent_collections={} \
                     g1_concurrent_time_ms={}",
                    young.count, young.time_ms, old.count, conc.count, conc.time_ms,
                );
            }
            let notif = b.notifications().census();
            if notif.queued != 0 || notif.deferred != 0 {
                eprintln!("{}", notif.summary_line());
            }
        }
        // Young non-moving-sweep health, UNCONDITIONALLY (the H2-CID0 rule: a
        // line printed only when non-zero cannot tell "clean" from "never
        // ran", and here that is the whole question).
        //
        // `par_accepts` far below `par_attempts` means the parallel sweep
        // prefix is being discarded and the entire arena is re-swept
        // sequentially. Until 2026-08-12 that was the state on EVERY JIT-warm
        // workload — `attempts=5 accepts=0` on the hibernate repro,
        // every abort the benign empty-object zero run — and these counters
        // said so the whole time with nobody to read them.
        //
        // `zero_empty_runs` is that benign shape, now stepped over on-grid: it
        // is normal and often large, and is deliberately NOT summed with
        // `zero_spans`, the residue that still forces an unwind.
        // `phantom_extents` and `live_in_dead` are the two corruption guards —
        // non-zero on either is a finding, not tuning.
        if let VmHeapInner::Generational(_) = &self.inner {
            use std::sync::atomic::Ordering as O;
            // The selective-promotion census, read ONCE for both lines that
            // need it — the `young_sweep:` denominator immediately below and
            // the `young_walk_entries:` line further down. One read, so the two
            // lines of a single summary can never disagree.
            let (sw, sel, defrag, cand, pin, unaged, evac, ofull) =
                cratonvm_gc_gen::gen_heap::selective_promotion_census();
            // `non_moving_sweeps` is `par_attempts`' DENOMINATOR, printed on
            // the same line as the numerator (gengc-round2-core2).
            //
            // `par_attempts` lives inside `sweep_young_non_moving`, so its
            // population is "non-moving young cycles" and NOT "collections".
            // Read without that denominator, `par_attempts=0` says "the
            // parallel-sweep predicate is inert" when the truth may be "this
            // workload took the MOVING path on every cycle and never reached
            // the predicate at all". That misreading is exactly what round 1's
            // probe results recorded and round 2 had to correct
            // (`docs/feature-designs/generative-gc-architecture.md`);
            // the 2026-09-20 `BinT 14` soak had ~1500 collections and
            // `non_moving_cycles_total=2`.
            //
            // The key is deliberately NOT `sp_sweeps=`, which is the same
            // counter under its selective-promotion name on the
            // `young_walk_entries:` line below (and, per cycle, on
            // `[GC_OVERHEAD]` in `vm/`). Minting a second `sp_sweeps=` token
            // would give one run two lines carrying that key, which is the
            // duplicate-key defect this round kept finding; a distinct name
            // keeps a scraper's `sp_sweeps=` grep single-valued while still
            // putting the number where the reader of `par_attempts` needs it.
            eprintln!(
                "[GC] young_sweep: non_moving_sweeps={sw} par_attempts={} par_accepts={} zero_spans={} \
                 zero_empty_runs={} phantom_extents={} phantom_nonbase_marks={} \
                 raw_interior_cleared={} live_in_dead={} walk_overshoot={} \
                 anchor_not_a_base={}",
                cratonvm_gc_gen::gen_heap::PAR_SWEEP_ATTEMPTS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::PAR_SWEEP_ACCEPTS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::SWEEP_ZERO_SPAN_HITS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::SWEEP_ZERO_SPAN_EMPTY_RUNS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::SWEEP_PHANTOM_EXTENTS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::SWEEP_PHANTOM_INTERIOR_MARKS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::LATE_RESOLVE_RAW_INTERIOR_CLEARED.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::LIVE_IN_DEAD_SPANS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::SWEEP_WALK_OVERSHOOT_HITS.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::SWEEP_ANCHOR_NOT_A_BASE.load(O::Relaxed),
            );
            eprintln!(
                "[GC] young_sweep_empty_runs: last_cycle_bytes={} young_used={} no_header_flag={}",
                cratonvm_gc_gen::gen_heap::EMPTY_RUN_BYTES_LAST.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::EMPTY_RUN_YOUNG_USED_LAST.load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::SWEEP_NO_HEADER_FLAG.load(O::Relaxed),
            );
            let l = &cratonvm_gc_gen::gen_heap::LATE_WALK_ZERO_RUNS;
            eprintln!(
                "[GC] late_walk_zero_runs: zr_mark_y2o={} zr_fixup_yo={} zr_walk_young={}",
                l[0].load(O::Relaxed),
                l[1].load(O::Relaxed),
                l[2].load(O::Relaxed),
            );
            // Which check abandoned a chunk. `par_accepts` alone cannot say,
            // and one `None` from any chunk discards the whole cycle's
            // attempt. Legend is on `PAR_CHUNK_BAILS`; printed as a bare array
            // so a soak log can be diffed without parsing seven key=value
            // pairs, and unconditionally for the same reason as the line above.
            let b = &cratonvm_gc_gen::gen_heap::PAR_CHUNK_BAILS;
            eprintln!(
                "[GC] young_sweep_chunk_bails: overshoot={} gap_filler={} zero_span={} \
                 bad_size={} hole_crossing={} phantom={} anchor_miss={}",
                b[0].load(O::Relaxed),
                b[1].load(O::Relaxed),
                b[2].load(O::Relaxed),
                b[3].load(O::Relaxed),
                b[4].load(O::Relaxed),
                b[5].load(O::Relaxed),
                b[6].load(O::Relaxed),
            );
            // The moving path's twin: which check refused a chunk of the
            // PARALLEL object-start walk (gen r4w2/youngmark). Legend on
            // `OBJSTART_CHUNK_BAILS`; `objstart_` prefix per the key test below.
            let osb = &cratonvm_gc_gen::gen_heap::OBJSTART_CHUNK_BAILS;
            eprintln!(
                "[GC] objstart_chunk_bails: objstart_anchor_in_skip={} objstart_skip_overflow={} \
                 objstart_cursor_in_skip={} objstart_gap_filler={} objstart_bad_size={} \
                 objstart_skip_crossing={} objstart_misaligned={} objstart_anchor_miss={}",
                osb[0].load(O::Relaxed),
                osb[1].load(O::Relaxed),
                osb[2].load(O::Relaxed),
                osb[3].load(O::Relaxed),
                osb[4].load(O::Relaxed),
                osb[5].load(O::Relaxed),
                osb[6].load(O::Relaxed),
                osb[7].load(O::Relaxed),
            );
            // …and of the zero-span bails, which of the predicate's three
            // conditions did the refusing. See `ZERO_RUN_REFUSALS`.
            let z = &cratonvm_gc_gen::gen_heap::ZERO_RUN_REFUSALS;
            eprintln!(
                "[GC] young_sweep_zero_refusals: misaligned={} live_inside={} \
                 implausible_next={} live_resumes={}",
                z[0].load(O::Relaxed),
                z[1].load(O::Relaxed),
                z[2].load(O::Relaxed),
                // NOT a refusal: runs stepped over by resuming at a PROVED live
                // base inside them (`zero_run_verdict`). `live_inside` beside it
                // stays the genuine refusals — an UNRESOLVED mark, which may be
                // an object interior rather than a base, or a caller with no
                // unresolved set to judge against. Both nonzero is the expected
                // reading: the gate is meant to accept only what it can prove.
                cratonvm_gc_gen::gen_heap::ZERO_RUN_LIVE_RESUMES.load(O::Relaxed),
            );
            // Did the five walks that still carry the old rule even RUN? A
            // zero anomaly count above means nothing without this. Legend on
            // `YOUNG_WALK_ENTRIES`; `sp_*` is the selective-promotion census,
            // which says whether the two passes inside it were reachable at
            // all (`sp_selective` is the gate).
            // `sw`/`sel`/… come from the single `selective_promotion_census()`
            // read at the top of this block, which also feeds
            // `non_moving_sweeps=` on the `young_sweep:` line.
            let w = &cratonvm_gc_gen::gen_heap::YOUNG_WALK_ENTRIES;
            // `sp_sweeps_without_selective` (gen r4w2/obs): the non-moving
            // sweeps that did not run the selective-promotion walk. Since gen
            // r4w2/youngmark those sweeps still AGE their survivors (the walk
            // runs age-only), so this counts non-promoting sweeps, not
            // non-aging ones (only `CRATONVM_NO_SELECTIVE_PROMOTE` skips the
            // walk) — see
            // `docs/feature-designs/generative-gc-architecture.md`.
            eprintln!(
                "[GC] young_walk_entries: evac_prepass={} fixup_3a={} mark_y2o={} \
                 fixup_yo={} walk_young={} | sp_sweeps={sw} sp_selective={sel} \
                 sp_sweeps_without_selective={} \
                 sp_defrag={defrag} sp_candidates={cand} sp_pinned={pin} \
                 sp_unaged={unaged} sp_evacuated={evac} sp_old_full={ofull}",
                w[0].load(O::Relaxed),
                w[1].load(O::Relaxed),
                w[2].load(O::Relaxed),
                w[3].load(O::Relaxed),
                w[4].load(O::Relaxed),
                sw.saturating_sub(sel),
            );
            // …and when the evacuation pre-pass DID run, what stopped it.
            let e = &cratonvm_gc_gen::gen_heap::EVAC_UNWIND_REASONS;
            eprintln!(
                "[GC] evac_unwind: unwind_overshoot={} unwind_zero_span={} unwind_bad_size={} \
                 unwind_hole_crossing={} candidates_dropped={}",
                e[0].load(O::Relaxed),
                e[1].load(O::Relaxed),
                e[2].load(O::Relaxed),
                e[3].load(O::Relaxed),
                cratonvm_gc_gen::gen_heap::EVAC_UNWIND_CANDIDATES.load(O::Relaxed),
            );
            // THE TLAB CENSUS AND THE TWO TLAB TRIPWIRES.
            //
            // `gengc-alloc-tlab-instruments-are-write-only-FIXED-20260924.md`:
            // `FILLER_OVER_OBJECT` and `REFILL_OVER_OBJECT` were incremented
            // and loaded by NOTHING in the workspace. `FILLER_OVER_OBJECT`'s
            // own doc calls a non-zero count "a use-after-free in waiting, not
            // a diagnostic curiosity" — the write site names the live lambda
            // capture at `0x200868400d8`, 261 336 bytes inside a 261 792-byte
            // filler, that reached `invokevirtual` as an all-zero header — and
            // the only trace a run left was a rate-limited `tracing::error!`
            // on `cratonvm::gc::guard` (the first eight occurrences and then
            // powers of two — `tlab.rs:1562`), a target a release run with the
            // default subscriber never prints and nothing in CI greps. A run
            // could trip it on every collection and finish looking clean.
            // Round 1 added the readers, round 2 the formatter; this is the
            // printer, and it is the half that makes the instrument an
            // instrument.
            //
            // A PULL, like `tlab_lazy_zero_stats()` on the ZGC block above:
            // `tlab.rs` holds no heap handle (that is the stated reason
            // `WATCH_COLLECTION` is a process static there) and has no output
            // policy, so the formatting lives there and the decision to print
            // lives here. Cheap: nine relaxed loads at shutdown.
            //
            // G1 and ZGC print the same two lines near the top of this
            // function (gen r4w4/alloc4; they used to be Generational-only, so
            // the other two backends' share of these process-wide numbers was
            // reported nowhere). The tuning element carries the
            // `[GC] tlab-sizer:` line after a newline (`tlab_sizer_line`).
            // gen r5w4/defaults8: `_for(true)` — this is the Generational arm,
            // where the share sizer's default is ON, so `sizer_on=` must be
            // this heap's answer, not the heap-less one.
            let [tlab_waste, tlab_guard] = cratonvm_gc_core::tlab::tlab_census_lines_for(true);
            eprintln!("{tlab_waste}");
            // The guard line is CORRECTNESS, not tuning, so it is printed
            // whether or not it fired — a zero-valued guard line is what
            // distinguishes "the tripwire says no" from "nobody asked", which
            // is the whole complaint the gap page was filed over. When it DID
            // fire it must not read like another gauge in a wall of gauges, so
            // the urgent case gets the `compact_oop_map_missing` treatment: the
            // same key=value prefix a grep wants, plus a suffix that says what
            // the number means. `tlab_guard_line_is_urgent()` is the predicate.
            eprintln!(
                "{}",
                render_tlab_guard_line(&tlab_guard, cratonvm_gc_core::tlab::tlab_guard_line_is_urgent()),
            );
            // THE EVACUATION POOL'S WIDTH, step (4) of
            // `gengc-alloc-evac-pool-width-frozen-FIXED-20260923.md`: "`EvacPool::
            // helpers()` should be published on the `[GC]` shutdown line …  so
            // 'the pool was narrower than the ask' stops needing a temporary
            // `eprintln!` to see."
            //
            // The ASK is not reachable from here — both callers clamp to
            // `pool.helpers()` before dispatching, so `scope` is never told it
            // was asked for more, and a counter of that inside the pool would
            // be unfireable. What the line carries instead is the pool's own
            // history, which shows the same defect from the other side:
            // `pool_built_with == pool_max_dispatched` with `pool_grew_by=0`
            // over a large `pool_dispatches` IS the frozen pool. `pool_grew_by`
            // is also the engagement counter for `EvacPool::ensure_helpers`,
            // whose only production caller is the one line of part (b) that is
            // still open in `gen_heap.rs` — so `pool_grew_by=0` says "part (b)
            // has not landed" rather than "it landed and did nothing".
            //
            // Generational arm (G1 prints its own pool's as `[GC] g1
            // evac-pool:`). g1gen/c6: this heap's own pool, not process-wide
            // counters every heap's pool fed.
            if let VmHeapInner::Generational(h) = &self.inner {
                eprintln!(
                    "{}",
                    cratonvm_gc_core::evac_pool::evac_pool_census_line(h.evac_pool_census())
                );
            }
        }
        // Old-gen free-list coalescing (the counterpart of the young sweep's
        // post-sweep coalescer). A large `merged` with compaction never having
        // run is the fragmentation regime this exists for.
        //
        // gen r4w2/obs (2026-09-23): `coalesce_skipped_maximal` is the passes
        // that returned at once because nothing had been freed since the last
        // full merge (`OldGen::free_list_maximal`, round 4 lane C). This line
        // used to print only when `calls > 0`, and the comment here read
        // `calls>0 merged=0` as "already maximally coalesced" — the skip
        // counter is now that reading, so the line prints when EITHER moved.
        {
            use std::sync::atomic::Ordering as O;
            let calls = cratonvm_gc_gen::old_gen::COALESCE_CALLS.load(O::Relaxed);
            let merged = cratonvm_gc_gen::old_gen::BLOCKS_MERGED.load(O::Relaxed);
            let skipped_maximal = cratonvm_gc_gen::old_gen::COALESCE_SKIPPED_MAXIMAL.load(O::Relaxed);
            if calls > 0 || skipped_maximal > 0 {
                eprintln!(
                    "[GC] oldgen_coalesce: calls={calls} blocks_merged={merged} \
                     coalesce_skipped_maximal={skipped_maximal}"
                );
            }
            // The old-gen live-set closure and the compaction's stale-grid
            // re-walk (lane C, design item 5): readable only from a debugger
            // until this line. `compact_stale_grid_rewalks` is expected to stay
            // 0. `close_live_set_multi_pass_calls` / `_deep_calls` are the calls
            // on which the pre-round-4 fixpoint ran >= 2 / >= 3 full passes, so
            // the two against `close_live_set_calls` price the change.
            let cl_calls = cratonvm_gc_gen::old_gen::CLOSE_LIVE_SET_CALLS.load(O::Relaxed);
            let cl_rescued = cratonvm_gc_gen::old_gen::CLOSE_LIVE_SET_RESCUED.load(O::Relaxed);
            let cl_multi = cratonvm_gc_gen::old_gen::CLOSE_LIVE_SET_MULTI_PASS_CALLS.load(O::Relaxed);
            let cl_deep = cratonvm_gc_gen::old_gen::CLOSE_LIVE_SET_DEEP_CALLS.load(O::Relaxed);
            let cl_scans = cratonvm_gc_gen::old_gen::CLOSE_LIVE_SET_WORKLIST_SCANS.load(O::Relaxed);
            let stale_rewalks = cratonvm_gc_gen::old_gen::COMPACT_STALE_GRID_REWALKS.load(O::Relaxed);
            if cl_calls | stale_rewalks != 0 {
                eprintln!(
                    "[GC] oldgen_closure: close_live_set_calls={cl_calls} \
                     close_live_set_rescued={cl_rescued} \
                     close_live_set_multi_pass_calls={cl_multi} \
                     close_live_set_deep_calls={cl_deep} \
                     close_live_set_worklist_scans={cl_scans} \
                     compact_stale_grid_rewalks={stale_rewalks}"
                );
            }
        }
        // H2-CID0 — the conservative-root and free-list invariants. Printed
        // unconditionally when non-zero so a soak log answers "did the workload
        // actually enter the regime this fix is about?" without a debug flag.
        //
        // `interior_root_pins` non-zero means conservative roots really are
        // interior words of live old-gen objects in this workload, i.e. the hole
        // the pin closes was live. `freed_interior_pinned` is ZERO BY
        // CONSTRUCTION unless `CRATONVM_GC_NO_OLD_INTERIOR_PINS` disabled the
        // pin — it is the negative control, and a non-zero value on an ordinary
        // run would mean the pin has regressed. `free_list_overlaps` non-zero
        // means an old-gen span was freed twice.
        {
            use std::sync::atomic::Ordering as O;
            let pins = cratonvm_gc_gen::gen_heap::OLDMARK_INTERIOR_ROOT_PINS.load(O::Relaxed);
            let freed = cratonvm_gc_gen::gen_heap::OLD_SWEEP_FREED_INTERIOR_PINNED.load(O::Relaxed);
            let overlaps = cratonvm_gc_gen::gen_heap::OLD_FREE_LIST_OVERLAPS.load(O::Relaxed);
            if pins | freed | overlaps != 0 {
                eprintln!(
                    "[GC] oldgen_conservative: interior_root_pins={pins} \
                     freed_interior_pinned={freed} free_list_overlaps={overlaps}"
                );
            }
            // The compacting arm's half of the same question, and the cost of
            // the answer. `dropped_interior_root` is zero by construction once
            // the downgrade is in; `downgraded` against `major=N` above says how
            // often compaction had to give way to the in-place sweep.
            let c_watched = cratonvm_gc_gen::gen_heap::COMPACT_DROPPED_WATCHED.load(O::Relaxed);
            let c_interior = cratonvm_gc_gen::gen_heap::COMPACT_DROPPED_INTERIOR_ROOT.load(O::Relaxed);
            let c_down = cratonvm_gc_gen::gen_heap::COMPACT_DOWNGRADED_INTERIOR_ROOT.load(O::Relaxed);
            if c_watched | c_interior | c_down != 0 {
                eprintln!(
                    "[GC] oldgen_compact: dropped_watched_referents={c_watched} \
                     dropped_interior_root={c_interior} downgraded_to_inplace={c_down}"
                );
            }
        }
        // H2-CID0 — the marking fail-open in `compact_oop_scan`. Printed only
        // when non-zero, because zero is the expected reading and a line that
        // is always there stops being read.
        {
            let n = cratonvm_gc_core::heap::COMPACT_OOP_MAP_MISSING.load(std::sync::atomic::Ordering::Relaxed);
            if n != 0 {
                eprintln!(
                    "[GC] compact_oop_map_missing={n} — MARKING FAIL-OPEN: that many \
                     GC_FLAG_COMPACT objects were scanned as legacy `Value` cells, so \
                     every reference they hold was invisible to the collector"
                );
            }
        }
        // The read-side companion to the line above: that one is a marking
        // FAIL-OPEN, this one is a read FAIL-SILENT. Every path that hands Java
        // a `null` for a slot that held bits — the interpreter's tagged decodes,
        // the JIT read helpers, `read_prim_element`'s reference arm — feeds one
        // process-wide per-source table in `cratonvm_types::compact_value`, and
        // this prints the breakdown rather than the total because the sources do
        // not mean the same thing:
        //
        //   * `interpreter` reads a TAGGED slot, so a count there can be a
        //     primitive `long` whose verbatim bits collided into the object
        //     sub-tag. That is benign and is exactly what the degrade exists for.
        //   * `jit` and `array-element` read UNTAGGED reference words — the JVM
        //     type system already says the slot is a reference — so there is no
        //     long/object ambiguity to absorb. A non-zero count there means a
        //     word that should have been a live pointer was handed to Java as
        //     `null`, i.e. the GC root-coverage gap of commit `6a04b0e3c1` is
        //     live in THIS run. `jit` is the most diagnostic of the three: a
        //     JIT'd frame is the frame a deposited root snapshot misses.
        //
        // Printed only when non-zero, for the same reason as the line above.
        //
        // The total is summed from THIS snapshot rather than read via
        // `object_degradation_count()`: that accessor is defined as the same sum
        // but takes its own set of relaxed loads, so under concurrency the two
        // could disagree and the printed line would not add up.
        {
            let breakdown = cratonvm_types::compact_value::object_degradation_breakdown();
            let total: u64 = breakdown.iter().sum();
            if total != 0 {
                let rendered = cratonvm_types::compact_value::DegradationSource::ALL
                    .iter()
                    .map(|s| format!("{}={}", s.name(), breakdown[s.index()]))
                    .collect::<Vec<_>>()
                    .join(" ");
                eprintln!(
                    "[GC] object_degradations={total} ({rendered}) — READ FAIL-SILENT: \
                     that many reference-shaped slots decoded to `null` instead of the \
                     object they named; a non-zero `jit` or `array-element` count is a \
                     live root-coverage failure, not a long/object collision"
                );
            }
        }
        // What the collector actually did on the last cycle and why. This is
        // the line that settles the `docs/GC.md` ("young collections run
        // non-moving whenever any JIT frame is active") vs `ARCHITECTURE.md`
        // ("per-cycle coverage proof, moving is possible") disagreement for
        // THIS run — see `tlab-and-card-audit.md` §3.
        //
        // Under G1 this used to be uninformative by construction:
        // `G1Collector::collect_garbage` passed the constant
        // `incomplete_reason::NONE`, so the record said "the backend always
        // evacuates" and nothing else, whatever the root scan had found. It now
        // carries the obligation that actually failed, and
        // `collector_decision_report` appends the `[GC] g1 root coverage:` rate
        // — which is what makes "was this pause's root set complete?" a
        // question a log answers instead of a crash dump.
        //
        // NOT PRINTED HERE, and that is the fix rather than an omission.
        // `vm-cli`'s `maybe_dump_shutdown_reports` emits the decision report on
        // BOTH exit arms and this function is now reached from that same hook,
        // so printing it here as well put the whole report on stderr TWICE on
        // the normal-return arm. The report is the one census that must survive
        // a `System.exit`, so the hook keeps it and this function does not.
        // Card / remembered-set costs, raw and normalized per allocated object
        // and per live byte.
        eprintln!("{}", cratonvm_gc_core::gc_metrics::gc_metrics_report());
        // Parallel young evacuation. Printed UNCONDITIONALLY, including the
        // all-zero line: the path is gated four ways over (moving cycle,
        // `CRATONVM_GC_PAR_EVAC`, the worker policy, and `ParEvac::plan`'s
        // to-space slack), so "never engaged" is the common outcome and a
        // counter that only appears when non-zero would make it
        // indistinguishable from "the report is missing".
        //
        // `helper_scans` is the one to read second. `cycles > 0` only says the
        // copy phase dispatched; `helper_scans == 0` beside it says the driver
        // did all of it, which is a load-balancing regression every
        // correctness test in the suite passes (see `gen_evac`).
        {
            let c = cratonvm_gc_gen::gen_evac::par_evac_census();
            eprintln!(
                "[GC] par_evac: par_evac_cycles={} helper_scans={} cas_losses={} \
                 declined_for_slack={} filler_bytes={} par_evac_promotions={} \
                 deferred_cards={} abandoned_old_bytes={} par_evac_overflow_promotions={} \
                 cheney_overflow_promotions={}",
                c.cycles,
                c.helper_scans,
                c.cas_losses,
                c.declined_for_slack,
                c.filler_bytes,
                c.promotions,
                c.deferred_cards,
                // gen r4w2/obs: old gen filled over a CAS loser's abandoned
                // promotion (kept since round 1, never printed), and survivors
                // tenured early because the reserved to-space region ran out
                // (lane A, round 4).
                c.abandoned_old_bytes,
                c.overflow_promotions,
                // gcj j3/yg3: the serial Cheney copy's twin of the last one
                // (`CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES`).
                cratonvm_gc_gen::gen_heap::CHENEY_OVERFLOW_PROMOTIONS
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
        }
        let fallbacks = cratonvm_gc_core::gc_quiescence::moving_young_coverage_fallback_count();
        // GENERATIONAL ONLY (2026-09-23). The line describes the moving YOUNG
        // generation, which only this backend has, and its totals come from the
        // decision histogram — which G1 has always fed
        // (`moving-backend-always-evacuates`, so every G1 pause was counted in
        // `moving_cycles_total`) and which ZGC feeds since 2026-09-23. The gate
        // below is `moving_young_enabled()`, a FLAG that defaults on for every
        // backend, so on a G1 or ZGC run this line printed that backend's
        // pauses under the generational young collector's name. The histogram
        // itself, with per-backend reason labels, is in
        // `collector_decision_report()`.
        let generational = matches!(&self.inner, VmHeapInner::Generational(_));
        if generational && (cratonvm_gc_core::gc_quiescence::moving_young_enabled() || fallbacks > 0) {
            // Both numbers, always. A correct answer while the moving count is
            // zero means the young generation never actually copied anything,
            // which is the exact way the 2026-07-01 validation declared
            // moving-young working while it was inert (see
            // `moving-young-corruption-rootcause.md` section 6). The histogram
            // then names what stopped it.
            //
            // THE KEY WAS `cycles=`, AND IT WAS NOT THE NUMBER OF MOVING CYCLES
            // (renamed 2026-09-20). `gc_quiescence::record_moving_young_cycle`
            // has one caller, `gen_heap::collect_garbage_inner`, and it sits
            // behind `if moving_young && has_conservative_roots` -- so it counts
            // only the moving cycles taken WHILE A JIT FRAME WAS LIVE. A run
            // with no compiled frames can relocate on every single collection
            // and still print `cycles=0 coverage_fallbacks=0`, which reads as
            // "the young generation never moved" and is the precise inverse of
            // the truth. `gc_metrics::collector_decision_report` has always
            // called the same number `moving_cycles_under_live_jit`; this line
            // did not, and `docs/GC.md` told operators to read this line.
            //
            // The honest totals come from the decision histogram, which is
            // bumped by `record_collector_decision` on BOTH arms of the young
            // path -- once per generational collection, moving or not.
            let under_jit = cratonvm_gc_core::gc_quiescence::moving_young_cycle_count();
            // `refused_cycles_total` is a THIRD term, added 2026-09-21. A
            // young cycle that refuses to run at all (an unparseable
            // from-space object-start layout, or a live mutator's reserved
            // TLAB tail inside the arena the cycle was about to reset) ran
            // NEITHER collector, and folding it into
            // `non_moving_cycles_total` claims the non-moving sweep
            // reclaimed something. A heap wedged in a refusal loop and an
            // idle heap used to print identical lines here; now they do
            // not, and `collector_decision_report()`'s histogram names
            // WHICH of the two refusal causes fired.
            let (moving_total, non_moving_total, refused_total, _) =
                cratonvm_gc_core::gc_metrics::decision_histogram();
            eprintln!(
                "[GC] moving_young: moving_cycles_total={moving_total} \
                 non_moving_cycles_total={non_moving_total} \
                 refused_cycles_total={refused_total} \
                 moving_cycles_under_live_jit={under_jit} \
                 coverage_fallbacks={fallbacks}"
            );
            let counts = cratonvm_gc_core::gc_quiescence::moving_young_fallback_reason_counts();
            for (reason, n) in counts.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] moving_young_fallback_reason: {}={}",
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::label(reason),
                        n
                    );
                }
            }
        }
    }

    /// Get the number of fields (slots) in an object.
    pub fn num_fields(&self, obj: ObjectRef) -> usize {
        dispatch!(&self.inner, get_header(obj)).num_slots() as usize
    }

    /// Carve out a TLAB from the young generation (generational), the Eden
    /// region (G1), or the low arena (ZGC, since 2026-09-02 -- `zgc/vm_tlab.rs`,
    /// kill switch `CRATONVM_ZGC_JIT_TLAB=0`). Returns `Some((ptr, size))` on
    /// success; the chunk is zeroed.
    ///
    /// Every object the VM lays out in the chunk must be reported through
    /// [`Self::note_tlab_object`] the moment its header is complete: on ZGC
    /// that is how the object enters the start registry the sweep, the SATB
    /// barrier and the conservative scans all consult.
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        self.refill_tlab_at_least(requested_size, 0)
    }

    /// [`Self::refill_tlab`], carving at least `min_size` bytes or nothing
    /// (gcd d1/d, `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object-FIXED-20261012.md`
    /// clamp 2): the VM's refill site passes the footprint of the object that
    /// missed, so a chunk that could not hold it is never carved. `0` is
    /// [`Self::refill_tlab`].
    ///
    /// Every backend: Generational (gcd d1/d), G1 (gch h4/g1) and ZGC (wave 9,
    /// lane gcI, `ZgcRealHeap::refill_tlab_at_least`).
    pub fn refill_tlab_at_least(
        &self,
        requested_size: usize,
        min_size: usize,
    ) -> Option<(*mut u8, usize)> {
        let chunk = match &self.inner {
            VmHeapInner::Generational(h) => h.refill_tlab_at_least(requested_size, min_size),
            // gch h4/g1: G1 takes the floor too (`G1Collector::refill_tlab_at_least`).
            VmHeapInner::G1(h) => h.refill_tlab_at_least(requested_size, min_size),
            // Wave 9, lane gcI: and ZGC (`ZgcRealHeap::refill_tlab_at_least`).
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.refill_tlab_at_least(requested_size, min_size),
        };
        // `CRATONVM_DBG_VACATED_FRAMES`: the chunk is bump-allocated from
        // without any further call into the heap, so this is the only door that
        // can tell the vacated ledger those addresses are being re-issued.
        if let Some((ptr, size)) = chunk {
            let lo = ptr as usize;
            cratonvm_gc_core::gc_quiescence::note_allocated_range(lo, lo.saturating_add(size));
            cratonvm_types::record_object_ref_range(ptr, size);
        }
        chunk
    }

    /// An object the VM just finished laying out at `ptr` inside a chunk from
    /// [`Self::refill_tlab`]; `footprint` is what the buffer's cursor advanced
    /// by. Registers it with the backend that needs to know (ZGC's start
    /// registry, plus allocate-black and the young grain); a no-op on the
    /// backends whose sweeps parse the chunk linearly.
    #[inline]
    pub fn note_tlab_object(&self, ptr: *mut u8, footprint: usize) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = (ptr, footprint);
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_tlab_object(ptr, footprint),
        }
    }

    /// Check if a raw address is within a live (non-Free) region of the heap.
    /// For generational GC, always returns false (not applicable).
    /// For G1, checks whether the address falls within an allocated portion
    /// of a non-Free region — used by reference processing to distinguish
    /// live objects in non-collected regions from dead objects.
    pub fn is_addr_live(&self, addr: usize) -> bool {
        match &self.inner {
            // An old-gen address is live if it is inside an ALLOCATED span.
            // Reference processing uses this to avoid clearing weak/soft refs
            // whose referent was tenured in an earlier cycle; returning `false`
            // here unconditionally — the original behavior — cleared every weak
            // reference to a promoted object on the next young GC.
            //
            // This used to be the bare range check `is_old_gen_addr`, on the
            // premise that "a minor GC never collects the old generation". That
            // premise is false: `sweep_old_gen_non_moving` reclaims dead old-gen
            // blocks IN PLACE, during a young collection, whenever a live JIT
            // frame blocks the moving young collector — so a freed block kept
            // answering "live" and no consumer of this predicate ever pruned a
            // dangling old-gen entry. See
            // `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
            //
            // Young-GC live-reclaim ROOT FIX (2026-07-07): also recognize
            // kept-in-place young survivors of the NON-MOVING sweep (which
            // produces no pointer_map entries), or reference processing
            // judges every live young Reference object/referent dead —
            // skipping the post-GC referent restore and pruning live
            // processor entries (the RRWL hold-count IMSE/hang family). See
            // `GenerationalHeap::is_live_young_survivor` for the soundness
            // argument (STW-window-only, zeroed-span discriminator,
            // moving-collection compatibility).
            VmHeapInner::Generational(h) => {
                h.is_live_old_gen_addr(addr) || h.is_live_young_survivor(addr)
            }
            VmHeapInner::G1(h) => h.is_addr_in_live_region(addr),
            // ZGC: the EXACT registry-base test (`zgc.rs:1715` — one hash
            // probe), deliberately NOT the loose `is_heap_addr` this arm used
            // to call. The two differ only in `is_heap_addr`'s interior
            // fallback (`zgc.rs:1731-1743`), and that fallback is exactly what
            // made this arm unsound.
            //
            // The ZGC sweep zeroes each dead object, returns its span to the
            // arena free list, and then COALESCES adjacent free spans into
            // maximal blocks. A later allocation carved from the head of a
            // coalesced block therefore covers the interior of what used to be
            // several dead objects — so a *dead* object's pre-GC base becomes
            // an interior address of an innocent LIVE object, and the extent
            // walk answered `true` for it. Reference processing then read that
            // as "the referent survived", and because ZGC's `pointer_map`
            // was always empty when this arm was written — the collector was
            // non-moving until 2026-08-13, and `relocate_stw` now returns a
            // NON-EMPTY map on a default run — the consumer at
            // `interpreter/gc_and_alloc.rs:2287` falls back to the stale
            // address and does `set_field(obj, 0, Value::Object(None))` on it
            // — a null written into the middle of a live object, and at the
            // weak/phantom restore site a non-null reference written there.
            // That is precisely the HIB-CV-32 stale-referent-write corruption
            // shape the guard chain around `watched_pre_gc_addr_survived`
            // exists to prevent, reproduced on this backend through a
            // predicate whose own consumer doc (below, "registry lookup")
            // already believed it was exact. It is exact now.
            //
            // Not merely a correctness fix: `is_heap_addr`'s fallback is
            // O(live) *under the registry mutex*, and this predicate runs once
            // per tracked reference per collection. `is_object_address` is one
            // locked hash probe.
            //
            // Contrast G1's arm above, which IS deliberately loose
            // (region-granular): G1 emits identity `pointer_map` entries for
            // every self-forwarded live object, so the map hit fires first and
            // the loose predicate is only a fallback. ZGC has no such map, so
            // its predicate is load-bearing alone and must be exact.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.is_object_address(addr).is_some(),
        }
    }

    /// HIB-CV-24 (ZGC manifestation): arm a class-unload reconciliation watch
    /// (see `zgc::zgc_reconcile_watch_map`) for the given pre-collection
    /// addresses, so their EXACT pre-slide mark-bitmap verdict can be read
    /// back after the cycle via [`Self::zgc_take_reconcile_results`] instead
    /// of the post-slide, compaction-aliasable `is_addr_live`. No-op on
    /// Generational/G1: the historical `gen_heap.rs` fix already covers
    /// Generational, and G1's own remark-time `is_marked` (see
    /// `g1_remark_process_references`) is already exact for this consumer.
    pub fn zgc_set_reconcile_watch(&self, addrs: Vec<usize>) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.zgc_set_reconcile_watch(addrs),
        }
    }

    /// Take back this cycle's captured exact verdicts armed via
    /// [`Self::zgc_set_reconcile_watch`]. `None` on Generational/G1 (nothing
    /// to take) or if nothing was armed for ZGC.
    pub fn zgc_take_reconcile_results(&self) -> Option<std::collections::HashMap<usize, bool>> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.zgc_take_reconcile_results(),
        }
    }

    /// gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9): is
    /// `addr` (a PRE-collection address) a watched young survivor that the
    /// last Generational true-root major left out of its trace, i.e. dead
    /// although still in place? See
    /// `GenerationalHeap::true_root_watched_left_out`. Always `false` on G1 and
    /// ZGC (no true-root young seed), and with the switch off. Since gci i3/og
    /// also installed by a MOVING true-root major (P-C) under
    /// `CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD` (opt-in).
    pub fn true_root_watched_left_out(&self, addr: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.true_root_watched_left_out(addr),
            VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// gch h4/wr: [`Self::true_root_watched_left_out`]'s whole verdict,
    /// ascending (empty on G1 / ZGC and on almost every pause), for a caller
    /// that judges many addresses in one window.
    pub fn true_root_watched_left_out_snapshot(&self) -> Vec<usize> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.true_root_watched_left_out_snapshot(),
            VmHeapInner::G1(_) => Vec::new(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => Vec::new(),
        }
    }

    /// gci i27/gr (`jr20-g1-young-pause-keeps-every-referent-in-a-jit-pinned-region-FIXED-20261016`):
    /// is the last G1 collection an evacuation pause that kept a young region
    /// in place, with no collection since? One load; `false` off G1. The VM
    /// asks before it gathers candidates for
    /// [`Self::g1_pinned_young_referents_unheld`].
    pub fn g1_pinned_referent_verdict_armed(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.pinned_referent_verdict_armed(),
            _ => false,
        }
    }

    /// gci i27/gr: G1's per-object verdict for addresses in the young regions
    /// the last pause kept in place: which `candidates` (PRE-collection
    /// addresses the pause neither moved nor identity-mapped) nothing holds.
    /// `roots` are the pause's root words as the collection left them. The
    /// region-granular arm of [`Self::watched_pre_gc_addr_survived`] answers
    /// "live" for every one of them; see
    /// `G1Collector::pinned_young_referents_unheld` for what "holds" covers and
    /// why every doubt declines. `Err(NotArmed)` off G1.
    pub fn g1_pinned_young_referents_unheld(
        &self,
        candidates: &[usize],
        roots: &[usize],
    ) -> Result<cratonvm_gc_g1::g1::G1PinnedReferentVerdict, cratonvm_gc_g1::g1::G1PinnedReferentDecline> {
        match &self.inner {
            VmHeapInner::G1(h) => h.pinned_young_referents_unheld(candidates, roots),
            _ => {
                let _ = (candidates, roots);
                Err(cratonvm_gc_g1::g1::G1PinnedReferentDecline::NotArmed)
            }
        }
    }

}
