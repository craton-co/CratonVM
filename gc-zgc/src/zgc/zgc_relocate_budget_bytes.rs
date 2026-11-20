#[allow(unused_imports)]
use super::*;

impl ZgcRealHeap {
    /// This heap's [`metrics::ZgcMetrics`] — the OpenJDK-shaped pause, phase
    /// and allocation-stall accounting.
    ///
    /// Fed on **every** collection since 2026-09-20 (see
    /// [`Self::record_metrics_for_cycle`]), so this is a live reading and not
    /// a zeroed struct. Three things a caller wants from it that nothing else
    /// in this file exposes:
    ///
    /// * [`metrics::ZgcMetrics::stw_share`] — the fraction of collector time
    ///   the mutators were stopped for. `1.0` here, honestly, and that is the
    ///   number `proposal-a-concurrent-mark-by-default` has to move.
    /// * [`metrics::ZgcMetrics::last_allocation_rate_bytes_per_sec`] — the
    ///   inter-cycle mutator allocation rate, derived from the occupancy
    ///   gauges so it cannot disagree with them. This is the `R` in
    ///   `proposal-e-allocation-rate-driven-trigger`'s predicate, and the one
    ///   input that proposal's stage 1 has to characterise before anything is
    ///   built on it.
    /// * [`metrics::ZgcMetrics::to_tsv_row`] with
    ///   [`metrics::ZgcMetrics::tsv_header`] — 58 columns that join against
    ///   the census's row for suite-wide aggregation.
    ///
    /// The allocation-stall columns are fed since zr z2/t2 through
    /// [`Self::note_allocation_stall`] (the VM's forced-collection door times
    /// each stall); `ZgcStallGuard` itself still has no caller.
    pub fn metrics(&self) -> &metrics::ZgcMetrics {
        &self.counters.metrics
    }

    /// Whether the VM's own read path will take its **compact** arm for this
    /// object — the census's "effectively compact" predicate.
    ///
    /// Why this is not `is_compact_object(header)`: `compact_object_field_storage`
    /// (`types/src/field_layout.rs:953-965`) refuses in *two* steps —
    /// `is_compact_object`, and then `class_layout_for_fields(class_id,
    /// num_slots)?`. An object carrying [`cratonvm_types::GC_FLAG_COMPACT`] whose
    /// class has no layout registered for its exact field count (a redefine that
    /// changed the count, a foreign layout domain) falls through that `?`, and
    /// [`Self::get_field`] then reads it as a **16-byte tagged cell**. For an
    /// instrument whose entire purpose is barrier cost, what the read path does
    /// is the truth and the header bit is not.
    ///
    /// `with_class_layout(.., |_| ())`, not `class_layout_for_fields(..)`: the
    /// two resolve through the same `registry_layout_for_fields`, and the
    /// borrowing form does not pay the `Arc` clone/drop for a handle that is
    /// discarded immediately — the same reason `enumerate_references` uses it.
    pub fn effectively_compact_header(header: &ObjectHeader) -> bool {
        cratonvm_types::is_compact_object(header)
            && cratonvm_types::with_class_layout(
                header.class_id.as_u32(),
                header.num_slots(),
                |_layout| (),
            )
            .is_some()
    }

    /// Run the shared [`ReferenceProcessor`] against the just-completed mark.
    ///
    /// Called by [`Self::collect_garbage`] AFTER the mark phase has set the
    /// [`GC_FLAG_MARKED`] bits but BEFORE the sweep clears them and reclaims
    /// dead objects, so the `is_marked` snapshot is exactly the live set the
    /// trace computed and any still-live `Reference` object can have its
    /// referent field nulled in place.
    ///
    /// This is the SAME `gc::reference` path the generational and G1 backends
    /// drive (HotSpot ordering: soft → weak → final → phantom); it is invoked
    /// here, not reimplemented. Clearing nulls field 0 (the `referent`) of each
    /// soft/weak `Reference` whose referent died, mirroring the VM-level
    /// `process_references_after_gc` writer.
    ///
    /// Residual (documented, out of scope for this non-moving STW backend):
    /// (a) discovery is caller-driven via [`Self::discover_reference`]; this
    /// method does not itself scan the heap for `Reference` subclasses — that
    /// classification needs class-layout knowledge the GC layer intentionally
    /// does not own. (b) enqueue/finalize actions are surfaced in the returned
    /// [`ReferenceProcessingResult`] for the runtime to drain; the GC does not
    /// run finalizers. (c) `free_heap_mb` for the soft-ref LRU policy is
    /// approximated from outstanding allocation against the threshold.
    pub fn process_references(&self) -> ReferenceProcessingResult {
        // Approximate free heap (MB) for the SoftReference LRU policy: bytes
        // below the GC trigger threshold that are not currently outstanding.
        let free_bytes = self
            .gc_threshold
            .saturating_sub(self.allocated.load(Ordering::Relaxed));
        let free_mb = free_bytes / (1024 * 1024);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut rp = self.counters.ref_processor.lock();
        let is_marked = |addr: usize| self.is_marked_addr(addr);
        // zr z1/rf (lane mk's cross-lane item,
        // `zgc-zr1mk-weak-ref-through-kept-soft-referent-is-cleared-NOT-A-DEFECT-20261012`):
        // a tracer, as G1's remark passes one. Without it the processor kept
        // only the DIRECT referents of the policy-kept soft references (and of
        // the finalizer rows) live for weak clearing, while this heap's mark
        // hid every registered `Reference`'s slot 0 (`ref_skip_objs`), so a
        // weak reference to an object two hops behind a kept soft referent
        // was cleared and the soft remark below then kept the object.
        //
        // Scope (zr z2/rv): `rp` is this heap's PRIVATE processor, empty in a
        // real run (see the `ref_processor` field), so this tracer -- like the
        // defect it closes -- is reached by this file's tests only. The VM's
        // references are cleared by `weakref_null_referents_pre_gc` /
        // `process_references_after_gc`, which the page's probe row exercises.
        //
        // The walk does NOT mark (the processor asks `is_marked` afterwards);
        // it stops at marked objects (already live, and their own children
        // were marked by the same mark) and honours the same referent hole,
        // taken here under the processor lock it already holds, so the closure
        // never needs the lock itself.
        let skip: FxHashSet<usize> = rp.reference_object_addresses().into_iter().collect();
        let trace = |roots: &[usize]| -> Vec<usize> {
            let mut seen: FxHashSet<usize> = FxHashSet::default();
            let mut work: Vec<usize> = roots.to_vec();
            let mut children: Vec<usize> = Vec::new();
            while let Some(addr) = work.pop() {
                if addr == 0 || !self.registry.contains(addr) || !seen.insert(addr) {
                    continue;
                }
                if self.is_marked_addr(addr) {
                    continue;
                }
                children.clear();
                let hole = skip.contains(&addr).then_some(0);
                self.enumerate_references(addr as *mut u8, &mut children, hole);
                work.extend(children.iter().copied().filter(|c| !seen.contains(c)));
            }
            seen.into_iter().collect()
        };
        let result =
            rp.process_references_with_finalizer_trace(&is_marked, Some(&trace), free_mb, now_ms);

        // Null the referent (field 0) of every soft/weak Reference whose
        // referent was cleared this cycle. Each is emitted EXACTLY ONCE (the
        // processor flags `clear_emitted`), matching the VM-level writer and
        // avoiding the recycled-address corruption documented in
        // `gc::reference`.
        for ref_obj in rp.take_newly_cleared() {
            // The Reference object must itself be live (marked) to write into;
            // a dead Reference is about to be swept, so skip it.
            if self.is_marked_addr(ref_obj) {
                self.set_field(
                    // SAFETY: `ref_obj` is a registered live allocation base.
                    unsafe { ObjectRef::from_raw(ref_obj as *mut u8) },
                    0,
                    Value::Object(None),
                );
            }
        }

        // Drop bookkeeping for Reference objects that did not survive this
        // cycle so the registry does not grow without bound and stale indices
        // are rebuilt.
        //
        // Runs BEFORE the slide, so every address here is still the one the
        // mark bits describe -- which is what makes this screen correct and
        // what keeps it from colliding with the relocation. The relocation
        // half is `update_after_gc`, driven from `collect_garbage` once the
        // pointer map exists; this comment used to claim it was unnecessary
        // because the collector never moved anything.
        let is_live = |addr: usize| self.is_marked_addr(addr);
        rp.remove_collected(&is_live);

        result
    }

    // -- Concurrent-mark seam ---------------------------------------------
    //
    // The three methods below exist only to serve the
    // [`mark::ZMarkContext`] implementation further down this file. Nothing
    // in `collect_garbage` calls them and nothing in `collect_garbage`
    // changed to accommodate them; see that impl's header for why adopting
    // the concurrent engine is a separate step.

    /// Open a concurrent mark cycle: snapshot the weak-referent skip set and
    /// publish it for [`mark::ZMarkContext::visit_refs`].
    ///
    /// Returns the snapshot so a caller can log or assert on its size.
    ///
    /// # When to call it
    ///
    /// In the mark-start safepoint, **after** every survivor's stale
    /// [`GC_FLAG_MARKED`] has been cleared and **before** any worker thread is
    /// started. Both halves of that ordering are load-bearing:
    ///
    /// * Clearing after a worker has started loses that worker's claim — the
    ///   object reads as unmarked again and can be pushed a second time, or
    ///   worse, the clear lands between another worker's winning
    ///   [`mark::ZMarkContext::try_mark`] and its scan, so the object is
    ///   scanned once and then re-claimed by a third worker as if it were
    ///   fresh. Exactly-once is a property of the whole cycle, not of one CAS.
    /// * Snapshotting after workers start means some edges were traced under
    ///   an empty skip set — see the `mark_ref_skip` field docs for why a
    ///   live view makes referent immortality depend on thread timing.
    ///
    /// This method deliberately does **not** clear the mark bits itself. The
    /// clear is a full walk of the registry that must happen inside the
    /// safepoint, and folding it in here would hide a stop-the-world pass
    /// behind a name that reads like bookkeeping.
    pub fn begin_concurrent_mark_cycle(&self) -> std::sync::Arc<ZSkipSnapshot> {
        self.begin_concurrent_mark_cycle_withdrawing(&[])
    }

    /// [`Self::begin_concurrent_mark_cycle`] for the mark-end refresh, which
    /// takes the soft rows (zr z2/cc) out of the set: `withdrawn` names them,
    /// so the member bitmap (`CRATONVM_ZGC_MARK_SKIP_BITS`) clears their bits
    /// as the hash-set path's rebuilt set drops them (zt t31/zc11; see
    /// `skip_bits`'s module doc for the defect a kept bit was).
    pub fn begin_concurrent_mark_cycle_withdrawing(
        &self,
        withdrawn: &[usize],
    ) -> std::sync::Arc<ZSkipSnapshot> {
        // Take the `ref_processor` guard, build the list, and DROP the guard
        // before touching `mark_ref_skip`. Holding two collector locks at once
        // is how a lock cycle is built; there is no reason to here.
        let mut snapshot: Vec<usize> = {
            let rp = self.counters.ref_processor.lock();
            rp.reference_object_addresses()
        };
        // zr z1/rf: the VM processor's active Weak / Phantom rows, when the
        // VM handed them in for this concurrent cycle (empty otherwise; see
        // `vm_conc_ref_skip`). Its own lock, taken and released alone.
        snapshot.extend(self.counters.vm_conc_ref_skip.lock().iter().copied());
        // zr z2/cc: the soft rows, while the concurrent phase runs (empty at
        // the mark-end refresh: `finish_concurrent_mark` takes them first).
        snapshot.extend(self.counters.vm_conc_soft_skip.lock().iter().copied());
        // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`): the rows the VM's
        // pre-collection pass hid instead of nulling, for the collection about
        // to run (the parallel stop-the-world mark, and the concurrent mark
        // end's refresh). Empty at every mark start (the VM door clears it).
        self.extend_with_stw_vm_reference_skip(&mut snapshot);
        // zt t31/zc11 (`CRATONVM_ZGC_MARK_SKIP_BITS`, default ON): the member
        // bitmap answers for this cycle -- one bit per member, set before the
        // arm below -- and no hash set, Bloom filter or class table is built.
        // A refresh while armed only adds bits (`skip_bits`).
        let refresh = self.mark_ref_skip_armed.load(Ordering::Acquire);
        let by_bits = self
            .counters
            .skipbits
            .fill(snapshot.iter().copied(), refresh, withdrawn, || {
                let span = self.arena_end.saturating_sub(self.arena_base);
                (span > 0).then(|| {
                    ZObjectStartBits::labelled_side_table(
                        self.arena_base,
                        span,
                        "skip",
                        zgc_side_table_no_thp_enabled(),
                    )
                })
            });
        if let Some(members) = by_bits {
            let shared = if self.counters.skipbits.persistent_enabled() {
                std::sync::Arc::new(ZSkipSnapshot::from_members_with_persistent(
                    members,
                    std::sync::Arc::clone(&self.counters.skipbits),
                ))
            } else {
                std::sync::Arc::new(ZSkipSnapshot::from_members(members))
            };
            *self.counters.mark_ref_skip.write() = Some(std::sync::Arc::clone(&shared));
            self.mark_ref_skip_armed.store(true, Ordering::Release);
            return shared;
        }
        let shared = std::sync::Arc::new(ZSkipSnapshot::from_set(snapshot.into_iter().collect()));
        // FILL THE FILTER BEFORE ARMING IT. See `mark_ref_skip_bloom`: the
        // `Release` store below is what makes these bits visible to a worker
        // that `Acquire`-loads the flag, and a worker that saw the flag set over
        // a half-filled filter would get a false NEGATIVE -- the one direction
        // that is a use-after-free rather than a wasted lock.
        for word in self.counters.mark_ref_skip_bloom.iter() {
            word.store(0, Ordering::Relaxed);
        }
        for addr in shared.iter() {
            let (w1, b1, w2, b2) = Self::skip_bloom_slots(addr);
            self.counters.mark_ref_skip_bloom[w1].fetch_or(1u64 << b1, Ordering::Relaxed);
            self.counters.mark_ref_skip_bloom[w2].fetch_or(1u64 << b2, Ordering::Relaxed);
        }
        // zt t27/zc10 (`CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`, default ON): and
        // the member-class table the mark asks before the filter, also before
        // the arm below. A REFRESH (the cycle already armed: the concurrent mark
        // end) only adds to it, so a worker still draining never reads a
        // cleared word; see `skip_class_filter`. A member that is not a
        // registered base has no header to read and switches the table off.
        self.counters
            .skipcls
            .fill(shared.iter(), refresh, |addr| {
                self.registry
                    .contains(addr)
                    .then(|| self.header_ref(addr as *mut u8).class_id.as_u32())
            });
        *self.counters.mark_ref_skip.write() = Some(std::sync::Arc::clone(&shared));
        self.mark_ref_skip_armed.store(true, Ordering::Release);
        shared
    }

    /// The two `(word, bit)` pairs an address occupies in the skip-set filter.
    ///
    /// Two independent hashes out of one 64-bit mix (the splitmix64 finaliser):
    /// the low half indexes the first bit, the high half the second. One mix
    /// rather than two because the finaliser's halves are independent enough for
    /// a filter whose only failure mode is an occasional extra lock acquisition.
    ///
    /// Addresses are 8-aligned, so the low three bits carry no information and
    /// are shifted out before mixing -- without that, a third of the filter's
    /// bits would be unreachable.
    #[inline]
    pub fn skip_bloom_slots(addr: usize) -> (usize, u32, usize, u32) {
        let mut x = (addr >> 3) as u64;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        x ^= x >> 33;
        let bits = (Z_SKIP_BLOOM_WORDS * 64) as u64;
        let h1 = x % bits;
        let h2 = (x >> 32) % bits;
        (
            (h1 / 64) as usize,
            (h1 % 64) as u32,
            (h2 / 64) as usize,
            (h2 % 64) as u32,
        )
    }

    /// Could `addr` be in this cycle's skip set?
    ///
    /// `false` is definitive: `addr` is certainly not in it, and the caller may
    /// skip the exact check and its lock. `true` means "take the lock and ask".
    /// Returns `false` with no cycle open, which the caller must NOT read as
    /// "not a reference" -- it checks `mark_ref_skip_armed` itself first.
    #[inline]
    pub fn skip_bloom_may_contain(&self, addr: usize) -> bool {
        let (w1, b1, w2, b2) = Self::skip_bloom_slots(addr);
        self.counters.mark_ref_skip_bloom[w1].load(Ordering::Relaxed) & (1u64 << b1) != 0
            && self.counters.mark_ref_skip_bloom[w2].load(Ordering::Relaxed) & (1u64 << b2) != 0
    }

    /// Close the concurrent mark cycle opened by
    /// [`Self::begin_concurrent_mark_cycle`], dropping the skip-set snapshot.
    ///
    /// Call it after the last worker has joined and after the non-strong
    /// reference phase has run — the reference phase asks
    /// [`mark::ZMarkContext::is_marked`] about referents, which does not
    /// consult the skip set, but a `keep_alive` re-drain does re-enter
    /// `visit_refs` and must still see the cycle's snapshot rather than an
    /// empty set.
    pub fn end_concurrent_mark_cycle(&self) {
        // The flag first: from here `visit_refs` takes the no-cycle arm and does
        // not consult the filter at all, so the bits may be left as they are
        // until the next `begin` clears them.
        self.mark_ref_skip_armed.store(false, Ordering::Release);
        *self.counters.mark_ref_skip.write() = None;
        // zt t31/zc11: the member bits go with the arm (no worker that read
        // the arm is still visiting: the caller joined them).
        self.counters.skipbits.clear();
        // zr z1/rf: the VM's rows are this concurrent cycle's only.
        self.counters.vm_conc_ref_skip.lock().clear();
        self.counters.vm_conc_soft_skip.lock().clear();
    }

    /// zr z1/rf (`CRATONVM_ZGC_CONC_REF_SKIP`): hand the VM reference
    /// processor's ACTIVE Weak / Phantom `Reference` objects (screened by the
    /// VM: each address is a live `java.lang.ref.Reference` now) to the
    /// concurrent mark cycle about to open. **Call in the mark-start pause,
    /// before [`Self::start_concurrent_mark`]**; ignored (and cleared) when no
    /// cycle opens, because `start_concurrent_mark`'s refusal arms leave it to
    /// [`Self::end_concurrent_mark_cycle`] or the next call to replace.
    ///
    /// Sound only with the VM's keep-alive on referent reads (`Reference.get`
    /// routes the referent through `satb_barrier`, which feeds this heap's
    /// mark ingress while a cycle marks) and the VM's pre-collection pass at
    /// the collection that ends the cycle (which nulls exactly these slots
    /// and judges them against the finished mark).
    pub fn set_concurrent_vm_reference_skip(&self, addrs: Vec<usize>) {
        *self.counters.vm_conc_ref_skip.lock() = addrs;
    }

    /// zr z1/rf: how many VM rows the next / current concurrent cycle hides.
    pub fn concurrent_vm_reference_skip_len(&self) -> usize {
        self.counters.vm_conc_ref_skip.lock().len()
    }

    /// zr z2/cc (`CRATONVM_ZGC_CONC_REF_SKIP_SOFT`): hand the VM's ACTIVE Soft
    /// `Reference` objects (screened by the VM like the weak rows) to the
    /// concurrent cycle about to open. Their referent slots are hidden from
    /// the concurrent PHASE only. Same call discipline as
    /// [`Self::set_concurrent_vm_reference_skip`].
    ///
    /// Why the mark end differs from the weak rows: the LRU policy KEEPS most
    /// soft referents, and nothing re-marks a kept referent whose slot the
    /// marker hid. So at the mark end (`finish_concurrent_mark`), after the
    /// VM's pre-collection pass has nulled the condemned slots, the soft rows
    /// leave the skip set and the strong edges of every soft `Reference`
    /// already marked are pushed as roots (`kept_soft_referent_roots`): a
    /// kept slot is non-null and its referent is traced; a condemned one is
    /// null and its referent dies unless something else holds it. A soft
    /// `Reference` first reached at the mark end is scanned with its slot
    /// visible.
    pub fn set_concurrent_vm_soft_reference_skip(&self, addrs: Vec<usize>) {
        *self.counters.vm_conc_soft_skip.lock() = addrs;
    }

    /// zr z2/cc: how many soft rows the next / current concurrent phase hides.
    pub fn concurrent_vm_soft_reference_skip_len(&self) -> usize {
        self.counters.vm_conc_soft_skip.lock().len()
    }

    /// Test hook: block until the open concurrent cycle's pool reaches a fixed
    /// point (its concurrent phase has traced everything it can see).
    #[cfg(test)]
    pub fn conc_wait_for_fixed_point_for_test(&self) {
        let pool = self
            .counters
            .conc_pool
            .lock()
            .as_ref()
            .map(std::sync::Arc::clone);
        if let Some(pool) = pool {
            pool.wait_for_fixed_point();
        }
    }

    /// zr z2/cc: the mark-end roots for the soft rows the concurrent phase hid
    /// -- every strong edge (the referent slot included, now that the skip set
    /// no longer names it) of each such `Reference` that is a registered,
    /// already-MARKED object. A marked `Reference` was (or will be) scanned
    /// with its slot hidden, so its referent needs this push; its other
    /// fields were traced by that scan and are dropped by `push_roots`'s
    /// claim. Mutators are stopped (mark-end pause).
    /// zt t31/zc11 (`CRATONVM_DBG_ZGC_SOFT_ROWS`, debug): after the closing
    /// drain, every soft row the mark reached whose referent it did not -- a
    /// referent the sweep is about to free under a live `SoftReference`
    /// unless the pause's own clear nulls the slot (a row the pre-collection
    /// pass hid, `in_stw`). One line per such row, with whether the phase had
    /// marked the row before the drain and whether its skip bit still hides
    /// the slot; a summary line per closing pause.
    pub fn dbg_soft_rows_after_drain(&self, soft_rows: &[usize], marked_before: &[bool]) {
        use cratonvm_gc_core::collector::GarbageCollector;
        let stw = self.stw_vm_reference_skip_set();
        let (mut live_rows, mut lost) = (0usize, 0usize);
        for (i, &r) in soft_rows.iter().enumerate() {
            if r == 0 || !self.registry.contains(r) || !self.mark_is_set(r) {
                continue;
            }
            live_rows += 1;
            // SAFETY: a registered, marked object start inside the pause.
            let obj = unsafe { ObjectRef::from_raw(r as *mut u8) };
            let Value::Object(Some(l)) = <Self as GarbageCollector>::get_field(self, obj, 0) else {
                continue;
            };
            let l = l.as_ptr() as usize;
            if !self.registry.contains(l) || self.mark_is_set(l) {
                continue;
            }
            lost += 1;
            eprintln!(
                "[GC] zgc-soft-row-dbg: lost row={r:#x} referent={l:#x} marked_before={} \
                 bit={:?} in_stw={}",
                marked_before.get(i).copied().unwrap_or(false),
                self.counters.skipbits.answer(r),
                stw.contains(&r),
            );
        }
        eprintln!(
            "[GC] zgc-soft-row-dbg: rows={} live_rows={live_rows} lost={lost}",
            soft_rows.len()
        );
    }

    pub fn kept_soft_referent_roots(&self, soft_rows: &[usize]) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`): a CONDEMNED soft row
        // keeps its referent in the slot under the switch (the pre-collection
        // pass hid it instead of nulling it), so its hole stays: the referent
        // is not a root. Empty set off the switch.
        let hidden = self.stw_vm_reference_skip_set();
        for &addr in soft_rows {
            if addr == 0 || !self.registry.contains(addr) || !self.mark_is_set(addr) {
                continue;
            }
            let hole = hidden.contains(&addr).then_some(0usize);
            self.visit_strong_refs_at(addr as *mut u8, hole, &mut |child| out.push(child));
        }
        out
    }

    /// The current cycle's skip-set snapshot, or `None` outside a cycle.
    ///
    /// # What `visit_refs` does when this is `None`
    ///
    /// It reports **every** reference slot, including a `Reference`'s
    /// referent, and logs one warning per heap. That is the wrong answer, and
    /// it is the wrong answer in the survivable direction: an over-reported
    /// edge makes a weak referent immortal (a leak, and `Cleaner` stops
    /// firing), while an under-reported edge drops a live object's out-edges
    /// and is a use-after-free. Faced with a caller that skipped
    /// [`Self::begin_concurrent_mark_cycle`], leak.
    ///
    /// The alternative — take the snapshot lazily on the first `visit_refs` —
    /// was rejected twice over: it would block a mark worker on
    /// `ref_processor`'s mutex, which
    /// [`mark::ZMarkContext`]'s threading contract forbids (a blocking
    /// implementation turns `pause_for_safepoint` into a hang), and a snapshot
    /// taken at the first *visit* is not a snapshot taken at mark *start*.
    pub fn concurrent_mark_skip_set(&self) -> Option<std::sync::Arc<ZSkipSnapshot>> {
        // Clone the handle under the read lock and return; the guard is gone
        // by the time the caller walks anything.
        self.counters
            .mark_ref_skip
            .read()
            .as_ref()
            .map(std::sync::Arc::clone)
    }

    /// Report every **strong** reference out-edge of the object at `base`.
    ///
    /// # This WAS a fork of [`Self::enumerate_references`]; it is now the shared
    /// implementation of both
    ///
    /// It was not a wrapper for two reasons, one of shape and one historical:
    ///
    /// * **Shape.** `enumerate_references` pushes into a `&mut Vec<usize>`;
    ///   [`mark::ZMarkContext::visit_refs`] hands each child to a
    ///   `&mut dyn FnMut(u64)` so the engine can push into the *striped*
    ///   work-stealing stack it owns. Funnelling through a `Vec` first would
    ///   allocate per object on every mark worker. This reason still stands.
    /// * **Defect (since FIXED at the source).** `enumerate_references`'s
    ///   compact arm used to read its field with
    ///   `std::ptr::read(slot as *const u64)`, an unconditional 8-byte load.
    ///   The real read path (`read_compact_field`,
    ///   `types/src/field_layout.rs:978-993`) goes through
    ///   [`cratonvm_types::narrow_oop::read_ref_slot`], which is a 4-byte
    ///   narrow load plus base+shift when compressed oops are on. Under narrow
    ///   oops the raw 8-byte load reads one field and half of the next and
    ///   calls the result a pointer: every compact reference field is then
    ///   traced to garbage (refused by `is_in_heap`, so it looks like nothing
    ///   is wrong) and the real referent is never traced at all — a live
    ///   object collected, silently, only under compressed oops. It was latent
    ///   rather than live only because `vm/src/vm/vm_init.rs` refuses
    ///   compressed oops for any backend but `Generational`.
    ///   `enumerate_references` now uses the same
    ///   `narrow_oop::read_ref_slot` load this fork does, so the two arms agree
    ///   and this bullet records why the fork exists, not a live divergence.
    ///
    /// # The fork is GONE (2026-08-26). This body is the only copy.
    ///
    /// Only the shape bullet above ever survived scrutiny, and it is one closure
    /// wide: [`ZgcRealHeap::enumerate_references`] is now
    /// `self.visit_strong_refs_at(base, skip_index, &mut |raw| work.push(raw as usize))`,
    /// which satisfies the `Vec`-versus-`FnMut` constraint without allocating.
    ///
    /// **The screens are the same screen, and that is what made the merge safe.**
    /// An earlier draft had `enumerate_references` screening with
    /// [`ZgcRealHeap::alloc_size`], which is stronger, and unifying onto it would
    /// have been a REGRESSION rather than a hardening: `alloc_size` refuses an
    /// array past `MAX_PLAUSIBLE_ARRAY_LEN` (1<<28 elements) while `alloc_array`
    /// admits up to `ZGC_REAL_MAX_ARRAY_LENGTH` (`i32::MAX`), so a legal
    /// reference array between those bounds would have had its elements left
    /// untraced — a live object collected. `MAX_PLAUSIBLE_ARRAY_LEN`'s own doc
    /// states the trade; it is right for the SWEEP that reads it, where refusing
    /// means "do not free", and wrong for a MARKER, where it means "do not
    /// trace". So `enumerate_references` was brought DOWN to this function's
    /// three screens — `num_slots > (1 << 24)` on the legacy arm,
    /// `with_class_layout(..).is_some()` on the compact arm, and nothing at all
    /// on the array arm — rather than this function being brought up to
    /// `alloc_size`. `both_reference_enumerators_report_the_same_edges_for_every
    /// _header_shape` is the test that keeps them one walk.
    ///
    /// The third fork, `census::reference_slots`, is a different matter and
    /// stays: it keeps nulls, reports every slot with its tag whatever the tag
    /// is, and has no `skip_index`, because it counts SLOTS rather than edges.
    /// Merging it would make the marker carry census metadata per slot.
    ///
    /// The legacy arm also differs deliberately: it reads the cell's `u32` tag
    /// and `u64` payload directly instead of `std::ptr::read`ing a whole
    /// [`Value`]. A concurrent marker races a mutator's field store, and
    /// materialising a `Value` out of a half-written cell can produce an
    /// invalid enum discriminant, which is instant undefined behaviour rather
    /// than a wrong answer. The raw tag/payload read cannot be poisoned that
    /// way — a torn cell yields an implausible address, which
    /// [`mark::ZMarkContext::is_in_heap`] then refuses like any other wild
    /// child. This is the same reason the census reads slots raw.
    ///
    /// # What the merge cost, measured
    ///
    /// The concern that deferred this work for two days was that hardening
    /// `enumerate_references` would mean `heap::read_value_cell_checked`'s
    /// ATOMIC 16-byte load on every legacy slot of the default collector's mark
    /// path. It does not: this read is a 4-byte tag load and a compare, and it
    /// skips the 8-byte payload load entirely when the tag is not
    /// `VTAG_OBJECT`, so it is strictly less work than the
    /// `std::ptr::read::<Value>` it replaced.
    ///
    /// Measured anyway, because a structural argument is not a number.
    /// `RMapGcStress` under ZGC at `--Xmx 64m` (34-35 real collections per run,
    /// so the walk is engaged), 7 interleaved pairs, priced in CPU time:
    ///
    /// * median paired difference **-0.58 s on ~21.8 s (-2.6%)**, after faster
    ///   in **4 of 7 pairs** — a coin flip.
    /// * within-arm spread **1.57x** (20.88 s to 32.67 s on the BEFORE arm).
    ///
    /// So: **no measurable difference.** The spread means this resolves about
    /// +/-10%, which is enough to rule out the per-slot atomic that was feared
    /// and NOT enough to claim the change is faster. Do not quote the -2.6% as a
    /// speedup; it is noise with a sign.
    ///
    /// The BEFORE arm was built at the same base with only this function
    /// reverted, because a cross-base A/B is not an A/B.
    ///
    /// `skip_index` is the referent-slot hole: `Some(0)` for a registered
    /// `Weak`/`Soft`/`Phantom`/`Cleaner` `Reference` object, exactly as
    /// `collect_garbage`'s `skip_for` computes it.
    pub fn visit_strong_refs_at(
        &self,
        base: *mut u8,
        skip_index: Option<usize>,
        f: &mut dyn FnMut(u64),
    ) {
        let header = self.header_ref(base);
        // Read once per object, not per edge: `CRATONVM_DBG_ZGC_WILD`.
        let dbg_wild = dbg_zgc_wild();
        match header.kind() {
            ObjectKind::Object => {
                let class_id = header.class_id.as_u32();
                let num_slots = header.num_slots();
                // The RAW header bit, following the precedent set by this
                // file's census impl: the bit decides the object's BODY
                // LAYOUT (`alloc_object` sizes a flagged object with
                // `layout.body_size`, not `num_slots * SLOT_SIZE`), while a
                // successful layout lookup only decides how that body is
                // *interpreted*. Deciding with `effectively_compact_header`
                // would send a flagged object whose layout cannot be resolved
                // down the legacy arm, which strides `num_slots` 16-byte cells
                // through a compact-sized body and reads past the allocation.
                if cratonvm_types::is_compact_object(header) {
                    // zr z3/mt: walk the layout's OOP-MAP (`ref_disps`, the
                    // reference displacements every other collector here
                    // scans) instead of zipping every field with its `is_ref`
                    // bit: the primitive fields cost nothing. Same edges; the
                    // referent hole is the skipped field's displacement. The
                    // indexed walk stays for `CRATONVM_DBG_ZGC_WILD` (it names
                    // the field index) and `CRATONVM_ZGC_MARK_REF_DISPS=0`.
                    let oop_map =
                        !dbg_wild && self.counters.mark_loop.ref_disps.load(Ordering::Relaxed);
                    let laid_out =
                        cratonvm_types::with_class_layout(class_id, num_slots, |layout| {
                            if oop_map {
                                let skip_disp = skip_index.and_then(|i| layout.field_disp(i));
                                for &offset in &layout.ref_disps {
                                    // SAFETY: as the indexed walk below --
                                    // `ref_disps` is this object's own layout.
                                    let slot = unsafe { base.add(offset as usize) };
                                    if skip_disp == Some(offset) {
                                        // zt t31/zc11 (R4): the referent hole
                                        // is healed, never traced.
                                        self.mark_slot_hole(slot);
                                        continue;
                                    }
                                    let raw =
                                        unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                                    if raw != 0 {
                                        f(self.mark_slot_word(slot, raw));
                                    }
                                }
                                return;
                            }
                            for (index, (&offset, &is_ref)) in layout
                                .field_disps
                                .iter()
                                .zip(layout.is_ref.iter())
                                .enumerate()
                            {
                                if !is_ref {
                                    continue;
                                }
                                // SAFETY: `offset` comes from this object's own
                                // registered layout — the layout its body was
                                // sized with at allocation — so the field lies
                                // inside the allocation, and `read_ref_slot`
                                // reads exactly `ref_field_size()` bytes of it.
                                let slot = unsafe { base.add(offset as usize) };
                                if skip_index == Some(index) {
                                    // zt t31/zc11 (R4): see the oop-map arm.
                                    self.mark_slot_hole(slot);
                                    continue;
                                }
                                let raw =
                                    unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                                if raw != 0 {
                                    if dbg_wild {
                                        self.dbg_wild_edge(base as usize, "field", index, raw);
                                    }
                                    f(self.mark_slot_word(slot, raw));
                                }
                            }
                        })
                        .is_some();
                    if !laid_out {
                        // Compact-flagged with no layout for
                        // `(class_id, num_slots)`: its cells cannot be walked
                        // without reading out of bounds, so report NO edges and
                        // say so. Under-reporting edges is the dangerous
                        // direction, which is exactly why this is a loud
                        // warning and not a silent `return`.
                        tracing::warn!(
                            target: "zgc",
                            class_id,
                            num_slots,
                            "zgc concurrent mark: compact-flagged object has no registered \
                             layout; its out-edges cannot be enumerated and are NOT traced"
                        );
                    }
                } else {
                    let n = num_slots as usize;
                    // The same "no real class has 1<<24 fields" screen
                    // `check_field_index` applies: a walk that decoded payload
                    // bytes as a header would otherwise stride millions of
                    // cells straight out of the arena.
                    if n > (1 << 24) {
                        tracing::debug!(
                            target: "zgc",
                            num_slots = n,
                            "zgc reference walk: suspect header, out-edges omitted"
                        );
                        return;
                    }
                    for i in 0..n {
                        if skip_index == Some(i) {
                            continue;
                        }
                        // SAFETY: a non-compact object's body is exactly
                        // `num_slots * SLOT_SIZE` bytes (`alloc_object`'s
                        // `fields_size` fallback), so cell `i` and both words
                        // read out of it lie inside the allocation. Both reads
                        // are naturally aligned because `SLOT_SIZE` is 16 and
                        // `HEADER_SIZE` is 8-byte aligned.
                        let cell = unsafe { base.add(HEADER_SIZE + i * SLOT_SIZE) };
                        let tag = unsafe {
                            std::ptr::read(
                                cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32
                            )
                        };
                        if tag != cratonvm_types::VTAG_OBJECT as u32 {
                            continue;
                        }
                        // A RELAXED ATOMIC load, not `std::ptr::read`. This
                        // marker runs CONCURRENTLY with mutators by design, and
                        // this payload word is the word the ZGC load barrier
                        // CAS-heals (`zgc/barrier.rs`'s `load_barrier_slow`).
                        // A plain read racing that `compare_exchange` is a data
                        // race and therefore UB in the Rust abstract machine
                        // regardless of what an aligned qword does on x86-64.
                        // `Relaxed` is the whole of the fix and changes no
                        // codegen -- the same single `mov`. See the atomicity
                        // note over `cratonvm_types::narrow_oop::read_ref_slot`
                        // (`types/src/narrow_oop.rs:353`) for why nothing
                        // stronger is warranted; this is that note's spelling.
                        //
                        // The 4-byte TAG load above stays PLAIN, deliberately:
                        // the barrier CASes the PAYLOAD word, never the tag,
                        // so the tag is not on the arming path. Its own
                        // (separate, tracked) item is on
                        // `census::ZSlotShape::word_is_atomically_accessed_today`.
                        let raw = unsafe {
                            (&*(cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET)
                                as *const AtomicU64))
                                .load(Ordering::Relaxed)
                        };
                        if raw != 0 {
                            if dbg_wild {
                                self.dbg_wild_edge(base as usize, "cell", i, raw);
                            }
                            f(raw);
                        }
                    }
                }
            }
            ObjectKind::Array => {
                if header.element_type() == ArrayElementType::Reference {
                    let len = header.array_length() as usize;
                    // `ref_element_size()`, not the wide `REF_ELEMENT_SIZE`
                    // constant: the stride is 4 under narrow oops. Same reason
                    // the compact arm above avoids the raw 8-byte load.
                    let stride = cratonvm_types::ref_element_size();
                    // SAFETY: the data area begins at `base + ARRAY_DATA_OFFSET`
                    // and holds `array_length()` elements of `stride` bytes;
                    // `i < len`.
                    let data = unsafe { base.add(ARRAY_DATA_OFFSET) };
                    for i in 0..len {
                        let slot = unsafe { data.add(i * stride) };
                        let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                        if raw != 0 {
                            if dbg_wild {
                                self.dbg_wild_edge(base as usize, "elem", i, raw);                            }
                            f(self.mark_slot_word(slot, raw));                        }                    }                }
                // Primitive arrays have no out-edges.            
}
            ObjectKind::HumongousFiller => {}        }    }
}
impl Default for ZgcRealHeap {
    fn default() -> Self {
        Self::new()
    }
}


/// Alignment of every TLAB-served allocation.
///
/// Fixed at 8 rather than taken from the caller: every production allocation
/// path on this heap already asks [`Arena::alloc`] for 8, and
/// [`Tlab::reserved_tail`]'s own note records that a cursor which is not
/// 8-aligned is silently dropped by the consumer of the tail — which turns the
/// tripwire into a no-op instead of a warning. Pinning the alignment here means
/// the cursor can never leave the object grid in the first place.
pub const ZGC_TLAB_ALIGN: usize = 8;
/// `CRATONVM_ZGC_RELOCATE_BUDGET_MB=<n>` -- the slide's evacuation budget, in
/// MiB of **live** bytes, for the one production
/// [`forwarding::ZRelocationSet::select`] call.
///
/// **The default is unchanged and is exactly
/// [`forwarding::ZFWD_DEFAULT_MAX_EVACUATION_BYTES`] (64 MiB).** Unset, the
/// selector sees byte-for-byte the value it saw before this knob existed, so no
/// current run moves. That is the whole point of it: a knob before a default
/// change, for the reason `docs/GC.md` records against
/// `CRATONVM_ZGC_ALLOC_TRIGGER` -- whose default lived one day -- and so the
/// A/B can be a same-binary pair of arms.
///
/// `0` means **unlimited**, not "select nothing". A zero-budget arm is already
/// reachable as `CRATONVM_ZGC_RELOCATE=0`, which refuses the slide outright and
/// says so in the skip census; spelling it a second way here would produce a
/// silent whole-heap decline attributed to nothing.
///
/// **Read [`ZgcRealHeap::relocation_budget_engagement`] before touching this.**
/// If `cycles_truncated` is zero on the workload the budget is not binding and
/// moving it measures nothing.
///
/// A value that does not parse is ignored with the default kept, rather than
/// panicking a collector mid-cycle; the `[GC] zgc-features:` line then shows
/// the unchanged engagement and the operator sees no effect, which is the same
/// signal a typo in any other `CRATONVM_*` integer gives here.
pub fn zgc_relocate_budget_bytes() -> usize {
    static G: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        match None::<std::ffi::OsString>
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
        {
            Some(0) => usize::MAX,
            Some(mb) => mb.saturating_mul(1024 * 1024),
            None => forwarding::ZFWD_DEFAULT_MAX_EVACUATION_BYTES,
        }
    })
}

/// `CRATONVM_ZGC_RELOCATE_COST_GATE` -- let `relocate_stw` decline a LOW
/// slide that cannot pay for its whole-live-set rewrite. **Default ON since
/// round 9 wave 9 (`zgc9`)**, `=0` restores the ungated slide. It landed OFF
/// in wave 6 (`zgc6`) as a behaviour change (less defragmentation) that the
/// wave writing it could not build or run. Read per cycle, not latched, so a
/// test can A/B it: it is consulted once per compacting collection.
///
/// The flip (wave 9, `cratonvm-jitr9-w8b.exe`, where the parallel passes are
/// already on):
/// * `ZgcTlabStress 8 200`, 5 interleaved rounds, every output `SAME`:
///   summed `relocate_us` 734 ms mean without the gate, 364 with it; wall clock
///   14.5 s against 13.8 s (`CRATONVM_ZGC_RELOCATE=0`: 13.3 s).
/// * `regression-suite/run.sh` with `=1`: 95/95.
/// * The fragmentation regressions wave 6 named, with `=1` against `=0`, same
///   verdict in both arms and `cost_gated=0` throughout (the gate does not fire
///   on a tight heap or on garbage-rich pages, which is their regime): H2
///   `TestKillProcessWhileWriting` and `TestMVStoreTool` at `--Xmx 1g` pass
///   with no OOM; netty `ResourceLeakDetectorTest` is 2/3 in both arms (a
///   known pre-existing failure).
///
/// Measured in round 9 wave 7 (`zgc7`, w6 binary, `ZgcTlabStress 8 200`, 6
/// interleaved rounds): summed `relocate_us` 1 510 ms mean without it, 891 with
/// it; it declines the last 3-4 cycles of a run (`cost_gated=3`/`4`), each then
/// costing 50-120 ms instead of ~600. The default is held in
/// [`ZGC_RELOCATE_COST_GATE_DEFAULT_ON`] so the flip -- recommended once the
/// fragmentation regressions pass with it on -- is one token. A set value
/// decides exactly as `runtime_flag_on` did: empty or an off-word is off,
/// anything else on.
pub fn zgc_relocate_cost_gate_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => ZGC_RELOCATE_COST_GATE_DEFAULT_ON,
    }
}

/// What an unset `CRATONVM_ZGC_RELOCATE_COST_GATE` means. See
/// [`zgc_relocate_cost_gate_enabled`] for the measurement. Flipped to `true`
/// in round 9 wave 9 (`zgc9`).
pub const ZGC_RELOCATE_COST_GATE_DEFAULT_ON: bool = true;

/// `CRATONVM_ZGC_READABLE_GIVE_BACK=1` -- give pages back to the OS WITHOUT
/// unmapping them, so the JIT's published raw-load bound survives the give-back
/// (round 9 wave 9, `zgc9`). Read once per heap, in `with_capacity`, and only
/// consulted when that heap publishes the bound at all.
///
/// # Why (measured on `cratonvm-jitr9-w8b.exe`)
///
/// `with_capacity` publishes `[arena_base, arena_end)` as `JIT_READ_BOUNDS`
/// slot 0, and every guarded inline `getfield` loads raw through it. The first
/// collection that gives back a single granule then withdraws that bound FOR
/// THE LIFE OF THE PROCESS (the one-way latch in `collect_garbage`'s give-back
/// block), because a decommitted granule faults on touch. From then on every
/// guarded `getfield` takes the checked helper. On `CratonBench` the
/// collections between phases have given pages back long before its last
/// phase, `bintrees`, runs. Interleaved, 2 rounds each:
///
/// | run | default | `CRATONVM_GC_RESERVE=0` (nothing is ever given back) |
/// |---|---:|---:|
/// | `CratonBench` total | 28 012 / 28 352 ms | 24 789 / 24 626 ms |
/// | its `bintrees` phase | 5 846 / 6 277 ms | 2 139 / 2 457 ms |
/// | `IrAllocLoop 100000`, round 2 | 6 349 / 6 466 ms | 2 737 / 2 652 ms |
/// | same, plus `CRATONVM_ZGC_NO_JIT_READ_BOUNDS=1` | 6 372 / 6 295 ms | 6 148 / 6 300 ms |
/// | `CratonBench` `bintrees`, plus `..._NO_JIT_READ_BOUNDS=1` (1 round) | 5 802 ms | 5 819 ms |
///
/// The `NO_JIT_READ_BOUNDS` rows are the attribution: with the bound never
/// published the two stores are equally slow, so the whole gap is the withdrawn
/// bound and none of it is page faults.
///
/// # What it changes
///
/// The heap's arena resets instead of decommitting (`Arena::readable_give_back`,
/// `HeapStore::reset_range`): the physical pages still go back to the OS, the
/// granules stay committed and read as zero, so no give-back leaves a hole and
/// the latch never fires. What it gives up is the Windows COMMIT CHARGE of the
/// peak (the resident set is still returned): `-Xmx` stays a ceiling, but a
/// process that peaked keeps its peak's commit. **Default ON** since the round 9
/// wave 9 integration (`=0` is the kill switch; the flip is
/// [`ZGC_READABLE_GIVE_BACK_DEFAULT_ON`]). Measured on the wave-9 binary, 3
/// interleaved rounds: `CratonBench` bintrees 5228/5242/5562 -> 1973/2338/1974 ms,
/// total 25399/25211/26098 -> 21987/22532/22203 ms, checksums identical; all JIT
/// probes, `ZgcTlabStress 8 200` (SAME) and the 95-vector regression suite green
/// with it on.
pub fn zgc_readable_give_back_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => ZGC_READABLE_GIVE_BACK_DEFAULT_ON,
    }
}

/// `CRATONVM_ZGC_MOVABLE_COMMIT_SCREEN=0` -- do not publish the arena's commit
/// bitmap beside the movable envelope, so `gen_heap::movable_range_committed`
/// answers `None` under ZGC again and the frame-band verifier's object screen
/// reads a band word's header without knowing whether its granule is mapped
/// (the pre-round-13 behaviour, which faults on a stale word in the reserved
/// middle). **Default ON** (a crash fix; `=0` is the kill switch).
pub fn zgc_movable_commit_screen_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => true,
    }
}

/// What an unset `CRATONVM_ZGC_READABLE_GIVE_BACK` means. See
/// [`zgc_readable_give_back_enabled`].
pub const ZGC_READABLE_GIVE_BACK_DEFAULT_ON: bool = true;

/// `CRATONVM_ZGC_PAR_RELOCATE` unset: how many workers the compaction's
/// whole-live-set passes use (round 9 wave 7, `zgc7`). `0` = serial, the
/// behaviour before this wave. Held in one constant so the flip, once the
/// integrated build has measured it, is one token plus the INVENTORY row.
///
/// Why these passes, and why threads rather than fewer passes: on
/// `ZgcTlabStress 8 200` (w6 binary, line-level sampler
/// `C:\craton\jitr9-probes\zgc7\lsampler.py`) the collector thread's samples
/// under `relocate_stw_admitted` split roughly 45 % reference rewrite, 20 %
/// `logical_pages`, 20 % the `live_ceiling` screen and 12 % the partition --
/// every one of them a header read (and for the rewrite, a slot walk) per
/// LIVE object, with ~60 K moved of 10 M live. Each is a pure function of
/// disjoint objects, so it splits by contiguous chunks of the live set with no
/// shared writes (the rewrite's stores land in the chunk's own objects).
/// Bounding the rewrite to the objects that can hold a pointer into a moved
/// page needs a remembered set fed by a barrier on EVERY reference store, and
/// several store paths here bypass the accessor barrier (see
/// `perf-zgc-relocate-stw-...` "Status after wave 7"); that is not a change
/// this collector can make safely from this crate alone.
///
/// **FLIPPED to 8 (round 9 wave 8, `zgc8`)**, measured on
/// `cratonvm-jitr9-w7b.exe`, `ZgcTlabStress 8 200`, 8 interleaved rounds per
/// arm, every output identical to HotSpot's (`C:\craton\jitr9-probes\zgc8\`):
/// summed `relocate_us` per million survivors walked, over the cycles that
/// actually relocated, was 22-32 ms with `=8` against 59-97 ms serial (about
/// 2.9x less stop-the-world); mean wall clock 16.2 s against 17.0 s. Three
/// `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1` runs under `=8`: 14 slides verified over
/// 76.7 M survivors, `missed_rewrites=0 unregistered_targets=0`. `=0` (or an
/// off-word) is the serial kill switch.
pub const ZGC_PAR_RELOCATE_DEFAULT_WORKERS: usize = 8;

/// `CRATONVM_ZGC_PAR_RELOCATE=on` (a word rather than a count): half of a
/// typical desktop's cores, the same restraint `parallel_mark_workers`
/// documents for a stop-the-world phase on a shared box.
pub const ZGC_PAR_RELOCATE_AUTO_WORKERS: usize = 8;

/// Run `f` over `items` in at most `workers` contiguous chunks and return each
/// chunk's answer IN CHUNK ORDER. With `workers <= 1` (or fewer than two
/// items) it is exactly one call `f(items)` on the calling thread, so the
/// serial arm and the parallel arm share one body -- the rule the sweep's
/// `sweep_bitmap` follows, so a divergence between the arms cannot hide in
/// code only one of them runs.
///
/// A worker panic is re-raised on the caller with its original payload: the
/// serial arm would have panicked the same way, and swallowing it would leave
/// a pass half done.
///
/// # Balanced, since zr z1/rl (`CRATONVM_ZGC_PAR_RELOCATE_BALANCE`)
///
/// The input is the live set in ADDRESS order, and the arena keeps large
/// objects at its high end, so equal-COUNT chunks put every large object --
/// every big reference array the rewrite must walk slot by slot -- into the
/// LAST chunk, i.e. onto one worker while the other seven wait at the join.
/// The measured 2.9x from 8 workers (`ZGC_PAR_RELOCATE_DEFAULT_WORKERS`) is
/// the shape of that tail. So the items are cut into
/// `workers * ZGC_PAR_CHUNKS_PER_WORKER` chunks and `workers` threads (the
/// caller is one of them, so `workers - 1` spawns) pull chunk indices from one
/// counter. Answers are still returned one per chunk IN CHUNK ORDER, and the
/// chunks still tile the input exactly once, so every caller's fold is
/// unchanged; only the chunk COUNT grew, which every caller already treats as
/// arbitrary. `=0` restores one chunk per worker.
///
/// `balance` is the heap's `CRATONVM_ZGC_PAR_RELOCATE_BALANCE` seed
/// ([`ZgcRealHeap::par_bal`]; zr z3/lt: per heap, no longer a process latch).
///
/// `pass` is the heap's [`ZParPass`] ([`ZgcRealHeap::par_bal`]: the balance
/// seed and, zs s3/ac, the CPU sink of `relocate_pool_ms=`), or a plain
/// `bool` balance with no CPU accounting.
pub fn zgc_par_chunks<'a, T, F>(
    pass: impl Into<ZParPass<'a>>,
    items: &[usize],
    workers: usize,
    f: F,
) -> Vec<T>
where
    T: Send,
    F: Fn(&[usize]) -> T + Sync,
{
    let pass = pass.into();
    let per_worker = if pass.balance {
        ZGC_PAR_CHUNKS_PER_WORKER
    } else {
        1
    };
    zgc_par_chunks_with(items, workers, per_worker, pass.cpu, f)
}

/// How one [`zgc_par_chunks`] pass runs (zs s3/ac): the
/// `CRATONVM_ZGC_PAR_RELOCATE_BALANCE` seed, and where the spawned pullers'
/// thread CPU is added (`None`: not accounted). A `bool` converts to a pass
/// with no sink, so the tests' `zgc_par_chunks(true, ..)` calls read as before.
#[derive(Clone, Copy)]
pub struct ZParPass<'a> {
    pub balance: bool,
    pub cpu: Option<&'a AtomicU64>,
}

impl From<bool> for ZParPass<'_> {
    fn from(balance: bool) -> Self {
        ZParPass { balance, cpu: None }
    }
}

/// Hard ceiling on `counters.relocations` (see `prune_relocations`): past it
/// the table is cleared, which degrades the forwarding barrier to what it did
/// before the table existed.
pub const ZGC_RELOCATIONS_CEILING: usize = 4_000_000;

/// Smallest table the amortized prune bothers to walk (zr z2/r2). 64 K
/// entries is about 1.5 MB of map, against a walk of that many registry
/// probes on every collection.
pub const ZGC_RELOCATIONS_PRUNE_MIN: usize = 64 * 1024;

/// Is an amortized prune of a table of `len` entries due, given the
/// `due_at` the last prune recorded (twice what it left)? Floored at
/// `ZGC_RELOCATIONS_PRUNE_MIN`; capped just past `ZGC_RELOCATIONS_CEILING`,
/// so a table that crosses the ceiling is always walked (and, if still past
/// it, cleared) on the next collection. Pure, for the tests.
pub fn relocations_prune_due(len: usize, due_at: usize) -> bool {
    relocations_prune_due_with(len, due_at, ZGC_RELOCATIONS_CEILING)
}

/// [`relocations_prune_due`] against an explicit `ceiling` (zt t1/rl: the
/// scaled one, [`zgc_relocations_ceiling_for`]). Pure, for the tests.
pub fn relocations_prune_due_with(len: usize, due_at: usize, ceiling: usize) -> bool {
    len >= due_at
        .max(ZGC_RELOCATIONS_PRUNE_MIN)
        .min(ceiling.saturating_add(1))
}

/// Floor of the heap-scaled forwarding-table ceiling (zt t1/rl): 256 Ki
/// entries, about 12 MB of map.
pub const ZGC_RELOCATIONS_CEILING_MIN: usize = 256 * 1024;

/// The forwarding-table ceiling for a heap of `capacity` bytes (zt t1/rl,
/// `zgc-zr3fp-slide-transients-scale-with-objects-moved` item 2). Unscaled:
/// the absolute [`ZGC_RELOCATIONS_CEILING`]. Scaled: one entry per 256 bytes
/// of heap, floored at [`ZGC_RELOCATIONS_CEILING_MIN`] and never above the
/// absolute ceiling -- so it can only LOWER the bound (1 M entries at
/// `-Xmx256m`, the absolute 4 M from 1 GiB up). A lower ceiling clears the
/// table sooner, which degrades the barrier for older moves to the
/// stamped-word path: the safe direction the clear already documents. Pure.
pub fn zgc_relocations_ceiling_for(capacity: usize, scaled: bool) -> usize {
    if !scaled {
        return ZGC_RELOCATIONS_CEILING;
    }
    (capacity / 256)
        .max(ZGC_RELOCATIONS_CEILING_MIN)
        .min(ZGC_RELOCATIONS_CEILING)
}



/// `CRATONVM_ZGC_RELOCATIONS_TRIM_HYSTERESIS` (zt t8/rl2): opt-in `=1`, the
/// forwarding table's footprint trim is sized by its demand and waits out a
/// run of over-provisioned prunes (`ZFootprintCounters::after_relocations_prune`).
/// Read by value at heap construction (per heap).
pub fn zgc_relocations_trim_hysteresis_requested() -> bool {
    false
}

/// `CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED` (zr z2/r2): default ON, `=0`
/// walks the forwarding table on every collection as before. Read once
/// (zt t8/rl2: per call in this crate's unit tests, so a
/// `with_thread_overrides` test sees its own value and not whatever the first
/// reader latched).
pub fn zgc_relocations_prune_amortized() -> bool {
    const KEY: &str = "CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED";
    if cfg!(test) {
        return zgc_default_on_switch(KEY);
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| zgc_default_on_switch(KEY))
}

/// `CRATONVM_ZGC_FORWARD_CHAIN_BY_EPOCH` (zr z2/r2): default ON, `=0`
/// restores the unconditional chain walk in `forwarded_after_slide`. Read
/// once (zt t8/rl2: per call in this crate's unit tests).
pub fn zgc_forward_chain_by_epoch() -> bool {
    const KEY: &str = "CRATONVM_ZGC_FORWARD_CHAIN_BY_EPOCH";
    if cfg!(test) {
        return zgc_default_on_switch(KEY);
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| zgc_default_on_switch(KEY))
}



pub fn zgc_default_on_switch(key: &str) -> bool {
    match cratonvm_types::flags::runtime_var_os(key) {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => true,
    }
}

// ---------------------------------------------------------------------------
// zr z3/fp: footprint -- what this collector holds beside the heap bytes
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/zgc-architecture.md`
// split the peak RSS into a non-heap boot baseline, the arena the cursors
// walked, and the side structures. This section is the side structures' half:
// three structures that kept their PEAK allocation for the life of the process
// (the forwarding table, the sweep's dead-address scratch, and the pages of the
// mark bitmap a per-cycle clear stored zeroes into), the uncommit clock that
// started a collection late, and the `[GC] zgc-footprint:` instrument that
// attributes what is left. Three switches:
//
// * `CRATONVM_ZGC_FOOTPRINT_TRIM` (default ON, `=0` kill): release retained
//   capacity of the forwarding table and the dead scratch once it is mostly
//   unused, and keep a granule's idle stamp when the `-Xms` floor refused its
//   release.
// * `CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP` (default ON, `=0` kill): start the
//   `ZUncommitDelay` clock when the sweep frees a granule, not at the next
//   collection's start.
// * `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR` (default ON, `=0` kill): the per-cycle
//   mark-bitmap clear stores only into non-zero words.

/// Below this many entries a table or scratch is not worth trimming: 16 Ki
/// entries is a few hundred KB, and regrowing it costs a pause a rehash.
pub const ZGC_FP_TRIM_FLOOR: usize = 16 * 1024;

/// A trimmed container keeps twice what it holds, so the next cycle of the
/// same size does not regrow it.
pub const ZGC_FP_TRIM_KEEP_FACTOR: usize = 2;

/// Trim only when capacity exceeds this many times what is held.
pub const ZGC_FP_TRIM_SLACK_FACTOR: usize = 4;

/// Consecutive collections the dead scratch must stay mostly unused before it
/// is trimmed. Keeps the reuse `the_dead_scratch_is_cleared_and_its_capacity_
/// reused` pins: ONE small cycle after a large one keeps the large buffer.
pub const ZGC_FP_SCRATCH_IDLE_CYCLES: usize = 8;

/// Per-heap footprint state (zr z3/fp). Boxed inside `ZgcCounters`, never on
/// the allocation or barrier path: every writer is a collection or the
/// summary.
pub struct ZFootprintCounters {
    /// `CRATONVM_ZGC_FOOTPRINT_TRIM`, seeded per heap.
    pub trim_on: AtomicBool,
    /// `CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP`, seeded per heap.
    pub stamp_at_sweep_on: AtomicBool,
    /// `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`, seeded per heap.
    pub sparse_clear_on: AtomicBool,
    /// Highest arena commitment seen at a collection start (before the
    /// give-back) or at a summary.
    pub committed_peak: AtomicUsize,
    /// Largest per-cycle registry snapshot, in bytes of copied words.
    pub snapshot_peak: AtomicUsize,
    /// Largest estimated forwarding-table allocation, in bytes.
    pub relocations_peak: AtomicUsize,
    /// Largest dead-scratch allocation, in bytes.
    pub dead_scratch_peak: AtomicUsize,
    /// Consecutive collections that used under a quarter of the scratch.
    pub dead_scratch_idle: AtomicUsize,
    /// Trims performed, and the bytes of capacity they released (estimated).
    pub trims: AtomicUsize,
    pub trimmed_bytes: AtomicUsize,
    /// zt t8/rl2 (`CRATONVM_ZGC_RELOCATIONS_TRIM_HYSTERESIS`, opt-in), seeded
    /// per heap: size the forwarding table's trim by its demand and wait out
    /// a run of over-provisioned prunes first.
    pub reloc_trim_hysteresis_on: AtomicBool,
    /// zt t8/rl2: the most forwarding entries one slide published since the
    /// last prune (the table's demand), consumed by each prune.
    pub reloc_published_since_prune: AtomicUsize,
    /// zt t8/rl2: consecutive prunes that found the table over-provisioned
    /// for its demand (the hysteresis arm only).
    pub reloc_overprovisioned_prunes: AtomicUsize,
    /// zt t8/rl2: the table's capacity right after its last trim, or 0 once
    /// the next publish has been seen.
    pub reloc_trimmed_to: AtomicUsize,
    /// zt t8/rl2: publishes that had to regrow the table past what the trim
    /// just before them left (`fp_trim_regrows=`): the trim-then-regrow
    /// oscillation's census, counted on every arm.
    pub trim_regrows: AtomicUsize,
    /// Granules the end-of-sweep pass started the uncommit clock on.
    pub idle_stamps: AtomicUsize,
    /// Bitmap words the sparse clear did not store into (each a word whose
    /// page may stay unbacked).
    pub sparse_words_skipped: AtomicU64,
    /// The startup commit (`-Xms`, or the ergonomic initial heap): the JMX
    /// `init` figure. Kept here because the arena's uncommit FLOOR is no
    /// longer the same number once [`ZgcRealHeap::set_uncommit_floor`] lowers
    /// it to HotSpot's `MinHeapSize`.
    pub initial_commit: AtomicUsize,
    /// zr z4/un: `CRATONVM_ZGC_SLIDE_BUFFER_TRIM`, seeded per heap (the
    /// slide's bookkeeping buffer lifetimes; see `relocate_stw_admitted`).
    pub slide_trim_on: AtomicBool,
    /// zr z4/un: the slide's transient bookkeeping, per cycle maxima (bytes):
    /// the `pairs` list, the sorted forward indexes alive at once, the cycle's
    /// `PointerMap`, and the largest sum of the three alive together.
    pub slide_pairs_peak: AtomicUsize,
    pub slide_index_peak: AtomicUsize,
    pub slide_map_peak: AtomicUsize,
    pub slide_transient_peak: AtomicUsize,
    /// zr z4/un: the uncommitter. `uncommit_gate` excludes its passes from
    /// collections (`collect_garbage` holds it for the whole pause; a pass
    /// only `try_lock`s it). `uncommitter` holds the thread's stop handle.
    pub uncommit_gate: Mutex<()>,
    pub uncommitter: Mutex<Option<ZUncommitterHandle>>,
    /// jr21 gc: the pre-fault worker's state (`zgc::prefault`). Its populate
    /// steps take `uncommit_gate` too, so no give-back interleaves with them.
    pub prefault: prefault::ZPrefault,
    /// zr z4/un: uncommitter passes run, passes that released something,
    /// bytes reset / decommitted by them, and passes skipped because a
    /// collection or a concurrent mark was in progress.
    pub uncommit_passes: AtomicUsize,
    pub uncommit_passes_released: AtomicUsize,
    pub uncommit_reset_bytes: AtomicUsize,
    pub uncommit_decommitted_bytes: AtomicUsize,
    pub uncommit_busy: AtomicUsize,
    /// zt t3/rl (`CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`): side-table
    /// bytes (registry and mark bitmap pages) given back with the arena
    /// granules under them. Cumulative.
    pub bitmap_pages_released: AtomicUsize,
    /// zr z7/rs: `CRATONVM_ZGC_ARENA_NO_THP`'s outcome (`ZGC_NO_THP_*`):
    /// unset, applied, refused by the kernel / owned store, or yielded to
    /// `CRATONVM_HEAP_THP`.
    pub arena_no_thp: AtomicUsize,
    /// zki (page `zgc-zt23hunt4-*` item 4): the startup commit on explicit
    /// hugetlbfs pages ([`ZgcRealHeap::with_capacity_initial_and_large_pages`]):
    /// the pool's refusals while committing it (HotSpot's `Failed to commit
    /// memory` lines) and whether it fell short of the initial heap (HotSpot
    /// then fails VM creation). Both `0` / `false` otherwise.
    pub hugetlb_initial_refusals: AtomicUsize,
    pub hugetlb_initial_short: AtomicBool,
    /// zki: the hugetlbfs pages HotSpot's code cache would hold, held for the
    /// heap's life on explicit pages (`None` otherwise).
    pub code_heap_hold: Mutex<Option<cratonvm_gc_core::reservation::HugetlbHold>>,
    /// zki: the arena's hugetlbfs refusals already reported
    /// ([`ZgcRealHeap::take_hugetlb_refusal_report`]), and the capacity the
    /// first report lowered the maximum to (`0` = not lowered).
    pub hugetlb_refusals_reported: AtomicUsize,
    pub hugetlb_lowered_max: AtomicUsize,
    /// zki: the arena is backed by explicit hugetlbfs pages (set once at
    /// construction), so the report above costs one load elsewhere.
    pub hugetlb_backed: AtomicBool,
    /// zki: HotSpot's uncommit probe was refused by the pool
    /// ([`ZgcExplicitPages::uncommit_probe`]).
    pub hugetlb_uncommit_unsupported: AtomicBool,
}

/// zr z4/un: where the uncommitter thread reports each pass that released
/// memory, as `(bytes, start_epoch_nanos, duration_nanos)` -- the VM turns it
/// into `jdk.ZUncommit`.
pub type ZUncommitJfrSink = std::sync::Arc<dyn Fn(usize, u64, u64) + Send + Sync>;

/// zr z4/un: what one [`ZgcRealHeap::uncommit_pass_at`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZUncommitPass {
    /// Bytes reset (physical pages returned, mapping kept).
    pub reset: usize,
    /// Bytes that became real holes (only a refused Windows re-commit).
    pub decommitted: usize,
    /// A collection or a concurrent mark was in progress; retried shortly.
    pub busy: bool,
    /// How long the uncommitter sleeps before the next pass.
    pub next_wait: std::time::Duration,
}

/// zr z4/un: the stop handle of a heap's uncommitter thread.
pub struct ZUncommitterHandle {
    pub stop: std::sync::Arc<(Mutex<bool>, parking_lot::Condvar)>,
    pub thread: Option<cratonvm_types::latched_thread::LatchedJoinHandle<()>>,
}

impl ZUncommitterHandle {
    pub fn stop_and_join(mut self) {
        {
            let (flag, cv) = &*self.stop;
            *flag.lock() = true;
            cv.notify_all();
        }
        if let Some(t) = self.thread.take() {
            // The thread may be the one dropping the heap (it upgraded the
            // last strong reference); never join yourself.
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }
}

/// zr z4/un: granules one uncommitter batch releases under one arena-lock
/// hold (64 MiB). HotSpot uncommits in bounded steps too; the bound keeps an
/// allocation that wants the lock from waiting behind a long `madvise` run.
pub const ZGC_UNCOMMIT_BATCH_GRANULES: usize = 32;
/// zr z4/un: the uncommitter's shortest and longest sleep. It sleeps until the
/// next granule is due, but at most `MAX` (so memory freed while it sleeps is
/// seen within that) and at least `MIN`.
pub const ZGC_UNCOMMIT_MIN_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
pub const ZGC_UNCOMMIT_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// zr z4/un: HotSpot 25's `[info][gc,heap]` uncommit line (`zUncommitter.cpp`,
/// observed on 25.0.3 `-XX:ZUncommitDelay=2 -Xlog:gc*`):
/// `Uncommitter (0) Uncommitted: 136M(27%) in 36.786ms`.
pub fn zgc_uncommitter_log_line(
    bytes: usize,
    max_capacity: usize,
    elapsed: std::time::Duration,
) -> String {
    let pct = if max_capacity == 0 {
        0.0
    } else {
        bytes as f64 * 100.0 / max_capacity as f64
    };
    format!(
        "Uncommitter (0) Uncommitted: {}M({:.0}%) in {:.3}ms",
        bytes / (1024 * 1024),
        pct,
        elapsed.as_secs_f64() * 1000.0
    )
}

/// zki (page `zgc-zt23hunt4-*` item 4): how the VM asks for a heap on
/// explicit hugetlbfs pages ([`ZgcRealHeap::with_capacity_initial_and_large_pages`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZgcExplicitPages {
    /// Bytes HotSpot's code cache takes from the pool first (`0` for none).
    pub code_heap_bytes: usize,
    /// Run HotSpot's uncommit-support probe (`ZUncommit` on and the minimum
    /// heap below the maximum, as `try_enable_uncommit` asks it only then):
    /// commit one granule and give it back; a refusal is one more `Failed to
    /// commit memory` line and `Uncommit: Implicitly Disabled (Not supported
    /// by operating system)` (host row `zki_h3`).
    pub uncommit_probe: bool,
}

/// zki (page `zgc-zt23hunt4-*` item 4): what
/// [`ZgcRealHeap::take_hugetlb_refusal_report`] hands the VM to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZgcHugetlbRefusalReport {
    /// `Failed to commit memory (Not enough space)` lines.
    pub failed_commit_lines: usize,
    /// The heap's maximum capacity (`-Xmx`).
    pub max_capacity: usize,
    /// The committed capacity the maximum was lowered to.
    pub lowered_to: usize,
}


/// zki: how many commits HotSpot's `ZPhysicalMemoryBacking::commit` tries and
/// fails for a request of `granules` granules against a pool that has none
/// left -- the whole request, then half of what is left each step until less
/// than one granule remains (`Arena::commit_initial_explicit`'s loop, every
/// step refused). Pure.
pub fn zgc_explicit_commit_refusal_lines(granules: usize) -> usize {
    if granules == 0 {
        return 0;
    }
    let mut lines = 1;
    let mut end = granules;
    loop {
        let len = end / 2;
        if len == 0 {
            return lines;
        }
        lines += 1;
        end -= len;
    }
}

impl ZFootprintCounters {
    pub fn from_env() -> Self {
        Self::from_env_with_initial(0)
    }

    pub fn from_env_with_initial(initial_commit: usize) -> Self {
        Self {
            initial_commit: AtomicUsize::new(initial_commit),
            trim_on: AtomicBool::new(zgc_default_on_switch("CRATONVM_ZGC_FOOTPRINT_TRIM")),
            stamp_at_sweep_on: AtomicBool::new(true),
            sparse_clear_on: AtomicBool::new(true),
            committed_peak: AtomicUsize::new(0),
            snapshot_peak: AtomicUsize::new(0),
            relocations_peak: AtomicUsize::new(0),
            dead_scratch_peak: AtomicUsize::new(0),
            dead_scratch_idle: AtomicUsize::new(0),
            trims: AtomicUsize::new(0),
            trimmed_bytes: AtomicUsize::new(0),
            reloc_trim_hysteresis_on: AtomicBool::new(zgc_relocations_trim_hysteresis_requested()),
            reloc_published_since_prune: AtomicUsize::new(0),
            reloc_overprovisioned_prunes: AtomicUsize::new(0),
            reloc_trimmed_to: AtomicUsize::new(0),
            trim_regrows: AtomicUsize::new(0),
            idle_stamps: AtomicUsize::new(0),
            sparse_words_skipped: AtomicU64::new(0),
            slide_trim_on: AtomicBool::new(true),
            slide_pairs_peak: AtomicUsize::new(0),
            slide_index_peak: AtomicUsize::new(0),
            slide_map_peak: AtomicUsize::new(0),
            slide_transient_peak: AtomicUsize::new(0),
            uncommit_gate: Mutex::new(()),
            uncommitter: Mutex::new(None),
            prefault: prefault::ZPrefault::new(),
            uncommit_passes: AtomicUsize::new(0),
            uncommit_passes_released: AtomicUsize::new(0),
            uncommit_reset_bytes: AtomicUsize::new(0),
            uncommit_decommitted_bytes: AtomicUsize::new(0),
            uncommit_busy: AtomicUsize::new(0),
            bitmap_pages_released: AtomicUsize::new(0),
            arena_no_thp: AtomicUsize::new(ZGC_NO_THP_UNSET),
            hugetlb_initial_refusals: AtomicUsize::new(0),
            hugetlb_initial_short: AtomicBool::new(false),
            code_heap_hold: Mutex::new(None),
            hugetlb_refusals_reported: AtomicUsize::new(0),
            hugetlb_lowered_max: AtomicUsize::new(0),
            hugetlb_backed: AtomicBool::new(false),
            hugetlb_uncommit_unsupported: AtomicBool::new(false),
        }
    }

    /// zr z4/un: record the slide's bookkeeping alive at one point (bytes).
    pub fn note_slide_buffers(&self, pairs: usize, index: usize, map: usize) {
        self.slide_pairs_peak.fetch_max(pairs, Ordering::Relaxed);
        self.slide_index_peak.fetch_max(index, Ordering::Relaxed);
        self.slide_map_peak.fetch_max(map, Ordering::Relaxed);
        self.slide_transient_peak
            .fetch_max(pairs + index + map, Ordering::Relaxed);
    }

    pub fn trim_on(&self) -> bool {
        self.trim_on.load(Ordering::Relaxed)
    }

    pub fn note_trim(&self, released: usize) {
        self.trims.fetch_add(1, Ordering::Relaxed);
        self.trimmed_bytes.fetch_add(released, Ordering::Relaxed);
    }

    /// The dead scratch after its one consumer (`MonitorCleanup::prune_dead`)
    /// has read it: record the peak, and trim it once it has stayed under a
    /// quarter full for [`ZGC_FP_SCRATCH_IDLE_CYCLES`] collections.
    pub fn after_dead_scratch(&self, dead: &mut Vec<usize>) {
        let word = std::mem::size_of::<usize>();
        self.dead_scratch_peak
            .fetch_max(dead.capacity() * word, Ordering::Relaxed);
        if dead.len().saturating_mul(ZGC_FP_TRIM_SLACK_FACTOR) >= dead.capacity() {
            self.dead_scratch_idle.store(0, Ordering::Relaxed);
            return;
        }
        let idle = self.dead_scratch_idle.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.trim_on() || idle < ZGC_FP_SCRATCH_IDLE_CYCLES {
            return;
        }
        if let Some(keep) = zgc_fp_trim_target(dead.len(), dead.capacity()) {
            let before = dead.capacity();
            dead.shrink_to(keep);
            self.note_trim(before.saturating_sub(dead.capacity()) * word);
            self.dead_scratch_idle.store(0, Ordering::Relaxed);
        }
    }

    /// zt t8/rl2: a slide published `pairs` forwarding entries and the table
    /// now has `capacity` usable slots. Records the demand the hysteresis
    /// reads, and counts a REGROW: the first publish after a trim that needed
    /// more room than the trim left (`fp_trim_regrows=`, every arm).
    pub fn after_relocations_publish(&self, pairs: usize, capacity: usize) {
        self.reloc_published_since_prune
            .fetch_max(pairs, Ordering::Relaxed);
        let trimmed_to = self.reloc_trimmed_to.swap(0, Ordering::Relaxed);
        if trimmed_to != 0 && capacity > trimmed_to {
            self.trim_regrows.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The forwarding table at a collection's prune: record the peak, and (when
    /// `trim` says the prune may) release capacity it no longer needs.
    ///
    /// zt t8/rl2 (`zgc-zr3fp-side-structures-kept-their-peak` review): the
    /// default trim sizes the table by what SURVIVED the walk. On a steady
    /// compacting run most entries die at every walk (their vacated address
    /// is a live base again) while every slide publishes about as many new
    /// ones, so each walk can trim the table to a fraction and the next
    /// slide's `reserve` regrow it: a rehash plus a fresh allocation per
    /// cycle, inside the pause, for a footprint the run needs again at once.
    /// `fp_trim_regrows=` counts that on every arm. Under
    /// `CRATONVM_ZGC_RELOCATIONS_TRIM_HYSTERESIS=1` (opt-in) the trim is sized
    /// by the DEMAND -- the larger of what is held and the most one slide
    /// published since the last prune -- and taken only after
    /// [`ZGC_FP_SCRATCH_IDLE_CYCLES`] consecutive over-provisioned prunes, the
    /// dead scratch's rule; skipped prunes count and may trim too (a trim
    /// keeps every entry, so it needs no deadness verdict), which also gives
    /// back a table whose slides stopped and whose amortized walk never
    /// comes due again.
    pub fn after_relocations_prune<V>(&self, table: &mut FxHashMap<usize, V>, trim: bool) {
        let entry = std::mem::size_of::<(usize, V)>();
        self.relocations_peak.fetch_max(
            zgc_fp_hash_table_bytes(table.capacity(), entry),
            Ordering::Relaxed,
        );
        let published = self.reloc_published_since_prune.swap(0, Ordering::Relaxed);
        if !self.trim_on() {
            return;
        }
        let keep = if self.reloc_trim_hysteresis_on.load(Ordering::Relaxed) {
            let demand = table.len().max(published);
            let Some(keep) = zgc_fp_trim_target(demand, table.capacity()) else {
                self.reloc_overprovisioned_prunes
                    .store(0, Ordering::Relaxed);
                return;
            };
            let run = self
                .reloc_overprovisioned_prunes
                .fetch_add(1, Ordering::Relaxed);
            if run + 1 < ZGC_FP_SCRATCH_IDLE_CYCLES {
                return;
            }
            self.reloc_overprovisioned_prunes
                .store(0, Ordering::Relaxed);
            keep
        } else {
            if !trim {
                return;
            }
            let Some(keep) = zgc_fp_trim_target(table.len(), table.capacity()) else {
                return;
            };
            keep        };
        let before = zgc_fp_hash_table_bytes(table.capacity(), entry);
        table.shrink_to(keep);
        let after = zgc_fp_hash_table_bytes(table.capacity(), entry);
        self.note_trim(before.saturating_sub(after));
        self.reloc_trimmed_to
            .store(table.capacity(), Ordering::Relaxed);
    }
}

/// The capacity to shrink a container of `len` live entries and `cap`
/// capacity to, or `None` when it is not worth it: only past
/// [`ZGC_FP_TRIM_FLOOR`] and only when capacity is more than
/// [`ZGC_FP_TRIM_SLACK_FACTOR`] times what is held, keeping
/// [`ZGC_FP_TRIM_KEEP_FACTOR`] times that (floored), and only when that at
/// least halves it. Pure, for the tests.
pub fn zgc_fp_trim_target(len: usize, cap: usize) -> Option<usize> {
    if cap <= ZGC_FP_TRIM_FLOOR {
        return None;
    }
    if len.saturating_mul(ZGC_FP_TRIM_SLACK_FACTOR) >= cap {
        return None;
    }
    let keep = len
        .saturating_mul(ZGC_FP_TRIM_KEEP_FACTOR)
        .max(ZGC_FP_TRIM_FLOOR);
    // A shrink that would not at least halve the allocation is a rehash for
    // nothing.
    (keep.saturating_mul(2) <= cap).then_some(keep)
}

/// zr z4/un: the bytes a slide's forward index holds (its sorted entries plus
/// the bucket directory, at most one `u32` per entry): an estimate for the
/// `zgc-footprint` line.
pub fn zgc_fwd_index_bytes(index: &relocate::ZForwardIndex) -> usize {
    index.entries().len() * (std::mem::size_of::<(usize, usize)>() + std::mem::size_of::<u32>())
}

/// Estimated bytes of a hashbrown table that reports `capacity` usable slots
/// of `entry` bytes: buckets are the next power of two at or above `8/7` of
/// the capacity, each costing the entry plus one control byte. Zero for an
/// unallocated table. (zs s1/fp: this doc had drifted onto
/// `zgc_fwd_index_bytes` when that function was inserted above it.)
pub fn zgc_fp_hash_table_bytes(capacity: usize, entry: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = capacity
        .saturating_mul(8)
        .div_ceil(7)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    buckets.saturating_mul(entry.saturating_add(1))
}

/// zr z7/rs: where the arena and the bitmaps live, for the
/// `[GC] zgc-footprint-spans:` line ([`ZgcRealHeap::footprint_spans`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZFootprintSpans {
    /// The arena reservation `[base, end)`.
    pub arena: (usize, usize),
    /// The words of the object-start, mark and old-start bitmaps; `(0, 0)`
    /// when absent.
    pub start_bits: (usize, usize),
    pub mark_bits: (usize, usize),
    pub old_bits: (usize, usize),
    /// zr z10/mt: how many of the three bitmaps sit on a private
    /// `MADV_NOHUGEPAGE` mapping (`CRATONVM_ZGC_SIDE_TABLE_NO_THP`) rather than
    /// in the process allocator.
    pub side_maps: usize,
    /// `CRATONVM_ZGC_ARENA_NO_THP`'s outcome: 0 unset, 1 applied, 2 refused,
    /// 3 yielded to `CRATONVM_HEAP_THP`.
    pub arena_no_thp: usize,
}

impl ZFootprintSpans {
    /// The line's body: `arena=0x..-0x.. start_bits=.. mark_bits=..
    /// old_bits=.. side_maps=N arena_no_thp=N`.
    pub fn render(&self) -> String {
        let r = |(lo, hi): (usize, usize)| format!("{lo:#x}-{hi:#x}");
        format!(
            "arena={} start_bits={} mark_bits={} old_bits={} side_maps={} arena_no_thp={}",
            r(self.arena),
            r(self.start_bits),
            r(self.mark_bits),
            r(self.old_bits),
            self.side_maps,
            self.arena_no_thp
        )
    }
}

/// What this heap holds, per structure, for the `[GC] zgc-footprint:` summary
/// line (zr z3/fp). Every figure is bytes unless named otherwise. `*_reserved`
/// is address space a structure owns (it costs nothing until written);
/// `*_noted` is the estimated resident part; the rest are allocation sizes of
/// structures that are written in full.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZFootprintStats {
    /// `-Xmx`: the arena reservation.
    pub arena_reserved: usize,
    /// Arena bytes the OS is asked to back now (`Runtime.totalMemory()`).
    pub arena_committed: usize,
    /// Highest commitment seen at a collection start or at this call.
    pub arena_committed_peak: usize,
    /// Arena bytes ever handed to a writer: an upper bound on the arena's
    /// resident set before give-backs.
    pub arena_ever_written: usize,
    /// Arena bytes decommitted, summed.
    pub arena_uncommitted_total: usize,
    /// Arena bytes reset (released while staying mapped), summed.
    pub arena_reset_total: usize,
    /// Free lists, anchors, uncommit clock, known-zero sets.
    pub arena_bookkeeping: usize,
    /// The object-start registry bitmap.
    pub start_bits_reserved: usize,
    pub start_bits_noted: usize,
    /// The mark bitmap (0 when the header arm is in force).
    pub mark_bits_reserved: usize,
    pub mark_bits_noted: usize,
    /// The generational old-start bitmap (0 until first used).
    pub old_bits_reserved: usize,
    pub old_bits_noted: usize,
    /// The young-grain grid and the commit-bit screen.
    pub grids: usize,
    /// The forwarding table: entries, estimated allocation, peak allocation.
    pub relocations_entries: usize,
    pub relocations_bytes: usize,
    pub relocations_peak: usize,
    /// The sweep's dead-address scratch: allocation and peak.
    pub dead_scratch_bytes: usize,
    pub dead_scratch_peak: usize,
    /// Largest per-cycle registry snapshot (transient, inside the pause).
    pub snapshot_peak: usize,
    /// Generational remembered sets (0 unless generational mode ran).
    pub remset_bytes: usize,
    /// `CRATONVM_ZGC_FOOTPRINT_TRIM`'s engagement.
    pub trims: usize,
    pub trimmed_bytes: usize,
    /// zt t8/rl2: publishes that regrew the forwarding table right after a
    /// trim (the oscillation `CRATONVM_ZGC_RELOCATIONS_TRIM_HYSTERESIS`
    /// removes).
    pub trim_regrows: usize,
    /// `CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP`'s engagement (granules).
    pub idle_stamps: usize,
    /// `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`'s engagement (words).
    pub sparse_words_skipped: u64,
    /// zr z4/un: the commitment less the granules a readable give-back reset
    /// and nothing reused since (`Runtime.totalMemory()` under
    /// `CRATONVM_ZGC_RESET_ACCOUNTING`).
    pub arena_net_committed: usize,
    /// zr z4/un: the slide's transient bookkeeping peaks (see
    /// `ZFootprintCounters::slide_pairs_peak`).
    pub slide_pairs_peak: usize,
    pub slide_index_peak: usize,
    pub slide_map_peak: usize,
    pub slide_transient_peak: usize,
    /// zr z4/un: the uncommitter's engagement.
    pub uncommit_passes: usize,
    pub uncommit_passes_released: usize,
    pub uncommit_reset_bytes: usize,
    pub uncommit_decommitted_bytes: usize,
    pub uncommit_busy: usize,
    /// zt t3/rl: side-table bytes given back with their arena granules
    /// (`CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`).
    pub bitmap_pages_released: usize,
}

impl ZFootprintStats {
    /// The side structures' resident estimate, summed: everything but the
    /// arena bytes themselves and the transient snapshot.
    pub fn side_resident_estimate(&self) -> usize {
        self.arena_bookkeeping
            + self.start_bits_noted
            + self.mark_bits_noted
            + self.old_bits_noted
            + self.grids
            + self.relocations_bytes
            + self.dead_scratch_bytes
            + self.remset_bytes
    }
}

/// Where a stale address's forwarding chain ends (zr z2/r2), starting from
/// its first hop `first = (to, epoch)`.
///
/// With `by_epoch`, a hop is followed only when the next entry was recorded
/// by a LATER slide than the one that led to it: X moved A -> B in slide 1
/// and B -> C in slide 2 is a chain (answer C), while Y moving B -> C and X
/// moving A -> B in the SAME slide is not (answer B -- following it handed
/// back Y). Without `by_epoch` it is the unconditional walk this replaced.
/// At most 8 hops either way.
pub fn forward_chain_end(
    reloc: &FxHashMap<usize, (usize, u32)>,
    first: (usize, u32),
    by_epoch: bool,
) -> usize {
    let (mut cur, mut epoch) = first;
    for _ in 0..8 {
        match reloc.get(&cur) {
            Some(&(next, next_epoch))
                if next != cur && (!by_epoch || (next_epoch.wrapping_sub(epoch) as i32) > 0) =>
            {
                cur = next;
                epoch = next_epoch;
            }
            _ => break,
        }
    }
    cur
}

/// The rewrite's per-page source filter (round 9 wave 6; a type since zr
/// z2/r2, it was a `&dyn Fn` closure): one bit per logical page of the arena,
/// set where some slide source lies, so "cannot be a source" is one load. Exact:
/// every source marks its page, and if any source lies outside the arena the
/// filter stands down (`exact == false` answers `true` for everything).
pub struct ZSourcePages {
    pub lo: usize,
    pub span: usize,
    pub page_bytes: usize,
    pub pages: Vec<bool>,
    pub exact: bool,
}

impl ZSourcePages {
    pub fn new(entries: &[(usize, usize)], lo: usize, hi: usize, page_bytes: usize) -> Self {
        let page_bytes = page_bytes.max(1);
        let span = hi.saturating_sub(lo);
        let mut pages = vec![false; span.div_ceil(page_bytes)];
        let mut exact = true;
        for &(from, _) in entries {
            let off = from.wrapping_sub(lo);
            if from >= lo && off < span {
                pages[off / page_bytes] = true;
            } else {
                exact = false;
            }
        }
        Self {
            lo,
            span,
            page_bytes,
            pages,
            exact,
        }
    }

    /// zt t23/zc7: also mark every page `[start, end)` of `spans` touches (the
    /// region cycle's source regions, which the forward index answers without
    /// listing them in its entries). A span reaching outside the filter's
    /// range stands the filter down, as an outside entry does. Marking more
    /// pages only sends more words to the index, never fewer.
    pub fn with_spans(mut self, spans: &[(usize, usize)]) -> Self {
        for &(s, e) in spans {
            if e <= s {
                continue;
            }
            let (first, last) = (s.wrapping_sub(self.lo), (e - 1).wrapping_sub(self.lo));
            if s < self.lo || last >= self.span {
                self.exact = false;
                continue;
            }
            for page in first / self.page_bytes..=last / self.page_bytes {
                self.pages[page] = true;
            }
        }
        self
    }

    /// Could `addr` be a slide source? `false` is exact; `true` means "ask the
    /// forward index".
    #[inline]
    pub fn may_be_source(&self, addr: usize) -> bool {
        if !self.exact {
            return true;
        }
        let off = addr.wrapping_sub(self.lo);
        addr >= self.lo && off < self.span && self.pages[off / self.page_bytes]
    }
}

/// The low slide's `live_ceiling` / vacated-span screen, handed to the fused
/// rewrite (zr z2/r2, `CRATONVM_ZGC_FUSED_SLIDE_SCREEN`). `rewrite == false`
/// runs the screen alone (nothing moved at either end).
pub struct ZSlideScreen<'a> {
    pub base: usize,
    pub low_end: usize,
    pub vacate_floor: usize,
    pub candidates: &'a [(usize, usize, bool)],
    pub rewrite: bool,
}

/// One chunk's part of [`ZSlideScreen`]: its highest live end and its copy of
/// the candidate spans, `blocked` set where one of its extents overlaps. Folded
/// exactly as the separate pass folds its chunks (max, and OR).
pub struct ZScreenPart {
    pub live_ceiling: usize,
    pub spans: Vec<(usize, usize, bool)>,
}

/// `CRATONVM_DBG_ZGC_RELOCATE_PHASES=1` (zr z2/r2): one `[zgc-relocate-phases]`
/// line per slide naming the microseconds each phase of
/// `relocate_stw_admitted` took, in the order they ran. Reads no clock when
/// off.
pub struct ZSlidePhaseClock {
    pub start: Option<std::time::Instant>,
    pub last: Option<std::time::Instant>,
    pub marks: Vec<(&'static str, u128)>,
}

impl ZSlidePhaseClock {
    pub fn new(on: bool) -> Self {
        let now = on.then(std::time::Instant::now);
        Self {
            start: now,
            last: now,
            marks: Vec::new(),
        }
    }

    /// Close the phase that ended now, under `name`.
    pub fn mark(&mut self, name: &'static str) {
        if let Some(last) = self.last {
            let now = std::time::Instant::now();
            self.marks
                .push((name, now.duration_since(last).as_micros()));
            self.last = Some(now);
        }
    }

    pub fn line(&self, live: usize, moved: usize, fused: bool) -> Option<String> {
        use std::fmt::Write as _;
        let start = self.start?;
        let mut line = format!("[zgc-relocate-phases] live={live} moved={moved} fused={fused}");
        for (name, us) in &self.marks {
            let _ = write!(line, " {name}_us={us}");
        }
        let _ = write!(line, " total_us={}", start.elapsed().as_micros());
        Some(line)
    }

    pub fn print(&self, live: usize, moved: usize, fused: bool) {
        if let Some(line) = self.line(live, moved, fused) {
            eprintln!("{line}");
        }
    }
}

pub fn zgc_dbg_relocate_phases_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(zgc_dbg_relocate_phases_enabled_requested)
}

/// Uncached half of [`zgc_dbg_relocate_phases_enabled`] (zt t6/rv3):
/// `CRATONVM_DBG_ZGC_RELOCATE_PHASES` read by value. It read presence, so `=0`
/// turned it on.
pub fn zgc_dbg_relocate_phases_enabled_requested() -> bool {
    false}

/// The no-reference bit of a [`ZgcRealHeap::size_cell`]. Sizes at or above it
/// (2 GiB) are not tabled.
pub const ZSIZE_CELL_NOREF: u32 = 1 << 31;

/// `CRATONVM_ZGC_RELOCATE_SIZE_TABLE` (zr z7/pc): default ON -- the slide reads
/// each low-region survivor's header ONCE (in `logical_pages`) and keeps its
/// size and a no-reference bit in a side table aligned with the live set; the
/// partition reads sizes from it, and the post-slide rewrite / screen skip a
/// primitive array without touching its header. `=0` restores a header read
/// per survivor per pass. Read once per process (tests:
/// `RELOCATE_SIZE_TABLE_OVERRIDE`).
pub fn zgc_relocate_size_table_enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = RELOCATE_SIZE_TABLE_OVERRIDE.with(|c| c.get()) {
        return on;
    }
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    zgc_default_on_switch("CRATONVM_ZGC_RELOCATE_SIZE_TABLE")
}

/// `CRATONVM_ZGC_SWEEP_SIZE_HINTS` (zr z7/pc): default ON, and only effective
/// under `CRATONVM_ZGC_RELOCATE_SIZE_TABLE` -- the bitmap sweep records each
/// low-region survivor's size cell (it reads the header anyway) and the slide's
/// page census takes it from there instead of reading the header a second
/// time. `=0` restores the census's own header read.
pub fn zgc_sweep_size_hints_enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = SWEEP_SIZE_HINTS_OVERRIDE.with(|c| c.get()) {
        return on;
    }
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    zgc_default_on_switch("CRATONVM_ZGC_SWEEP_SIZE_HINTS")
}





/// `CRATONVM_ZGC_RETAIN_PLAIN_STORES` (zgc pc3, default ON, `=0` kill): the
/// sweep's registry prune (`starts &= marks`) writes a changed word with a
/// plain store instead of a locked `fetch_and`
/// (`HeapBitmap::retain_marked_within_plain`). Stop-the-world only, like the
/// prune itself; same result. ~4 ms of a ~20 ms pause on the zgc-r21 lean
/// row, where nearly every word holds a dead start.
pub fn zgc_retain_plain_stores_enabled() -> bool {
    true
}



/// The sweep's survivor size cells handed to the slide (zr z7/pc): ascending
/// bases and their [`ZgcRealHeap::size_cell`], paired by index. A hint is used
/// only for the exact base it names, so a hint list that misses survivors (an
/// arm that recorded nothing, a spilled base) or carries extra bases costs a
/// header read, never a wrong size.
#[derive(Default)]
pub struct ZSweepSizeHints {
    pub addrs: Vec<usize>,
    pub cells: Vec<u32>,
}

impl ZSweepSizeHints {
    /// Non-empty, paired, strictly ascending. Checked once per slide (a linear
    /// pass over a list the census walks anyway).
    pub fn is_usable(&self) -> bool {
        !self.addrs.is_empty()
            && self.addrs.len() == self.cells.len()
            && self.addrs.windows(2).all(|w| w[0] < w[1])
    }
}





/// Latched "no chunk" answers before the carve re-tests for real. Bounds what
/// a free block the latch cannot see (a concurrent sweep's install, a coalesce
/// inside another thread's failing `alloc`) costs: at most this many objects
/// on the per-object path that a chunk could have served.
pub const ZGC_CARVE_FUTILE_SKIPS: usize = 64;

/// A carve whose `need` is at least this many bytes uses the latch's second
/// slot. The two callers ask in different units -- the VM thread's refill
/// for at least a minimum buffer (8 KiB by default), the heap's own cell
/// refill for one object's footprint -- so one slot would be overwritten by
/// each in turn and answer neither.
pub const ZGC_CARVE_FUTILE_SPLIT_NEED: usize = 4096;

/// The carve-futile latch (zt t27/pc2, `CRATONVM_ZGC_TLAB_CARVE_FUTILE_LATCH`).
///
/// # Why "it failed at (want, need)" may answer a later (want', need')
///
/// At a fixed arena state, `carve_tlab_chunk` fails iff the recycled rung
/// (`recycled_chunk_size`, then `alloc` of the largest low block) and
/// `alloc(want)` both fail. `alloc(want)` serves from a free block of at least
/// `want`, the bump headroom, the high free list or the unclaimed reserve:
/// each harder for a larger request. The recycled rung needs
/// `need <= largest < want` and `largest >= max(want / 8, need)` (or its
/// starved arm: `largest >= max(want / 64, need)`, two spare blocks of
/// `want / 8`, and a bump headroom below `want`): every one of those is harder
/// for a larger `need`, and for a larger `want` all but the headroom test are
/// harder too -- and that one already held at `want`, because `alloc(want)`
/// failed on the bump (up to the chunk alignment's padding, the one edge where
/// a latched answer can be wrong; it costs a chunk, never an object). So a
/// failure at `(want, need)` is a failure at every `want' >= want,
/// need' >= need` -- while the arena has not GAINED space.
///
/// Allocation only takes space (smaller blocks, a higher cursor). What adds
/// it between two collections: a TLAB tail returned to the free list (the
/// two return sites bump `carve_space_gen`), a coalesce inside a failing
/// `alloc` (another thread's per-object path), a concurrent sweep's install
/// (`CRATONVM_ZGC_CONCURRENT_SWEEP`). The first moves the generation; the
/// other two are covered by the skip budget ([`ZGC_CARVE_FUTILE_SKIPS`]):
/// after that many latched answers one real carve re-tests. A collection
/// moves `gc_count`, the generation's other half.
///
/// None of this is a correctness argument: a latched "no chunk" sends the
/// object to the per-object path (`alloc_raw`), which serves it from the same
/// free list and bump the chunk would have come from. The latch can only
/// change which path serves an object, never whether it is served.
#[derive(Default)]
pub struct ZCarveFutileLatch {
    pub slots: [[AtomicUsize; 4]; 2],
}

impl ZCarveFutileLatch {
    pub fn slot(&self, need: usize) -> &[AtomicUsize; 4] {
        &self.slots[usize::from(need >= ZGC_CARVE_FUTILE_SPLIT_NEED)]
    }

    /// Is any slot set? The tail-return sites read this before they bump the
    /// generation, so a healthy run never writes the shared counter.
    pub fn armed(&self) -> bool {
        self.slots.iter().any(|s| s[0].load(Ordering::Relaxed) != 0)
    }

    /// Does a latch set at `space_gen` answer this carve? Consumes one skip
    /// when it does.
    pub fn answers(&self, space_gen: usize, want: usize, need: usize) -> bool {
        let s = self.slot(need);
        let stamp = s[0].load(Ordering::Acquire);
        if stamp == 0 || stamp != space_gen.wrapping_add(1) {
            return false;
        }
        if want < s[1].load(Ordering::Relaxed) || need < s[2].load(Ordering::Relaxed) {
            return false;
        }
        s[3].try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
            left.checked_sub(1)
        })
        .is_ok()
    }

    /// A real carve at `space_gen` failed for `(want, need)`: latch it, with a
    /// fresh skip budget. The stamp is stored last.
    pub fn note_failure(&self, space_gen: usize, want: usize, need: usize) -> bool {
        let stamp = space_gen.wrapping_add(1);
        if stamp == 0 {
            return false;
        }
        let s = self.slot(need);
        s[1].store(want, Ordering::Relaxed);
        s[2].store(need, Ordering::Relaxed);
        s[3].store(ZGC_CARVE_FUTILE_SKIPS, Ordering::Relaxed);
        s[0].store(stamp, Ordering::Release);
        true
    }

    /// A real carve succeeded: the arena can serve chunks again. Read before
    /// write, so the healthy path leaves the line shared.
    pub fn note_success(&self) {
        for s in &self.slots {
            if s[0].load(Ordering::Relaxed) != 0 {
                s[0].store(0, Ordering::Relaxed);
            }
        }
    }
}

/// `CRATONVM_ZGC_SWEEP_SIZE_MEMO` (zt t27/pc2, default ON, `=0` kill): the
/// bitmap sweep sizes a survivor from the previous sweep's size cell for the
/// same base (no header read) when that sweep's cells were kept as a
/// [`ZSweepSizeMemo`]. Read once per collection.
pub fn zgc_sweep_size_memo_enabled() -> bool {
    zgc_flag_default_on("CRATONVM_ZGC_SWEEP_SIZE_MEMO")
}

/// The survivor size cells one bitmap sweep recorded (the slide's hints), kept
/// for the NEXT sweep of the same heap (zt t27/pc2, `CRATONVM_ZGC_SWEEP_SIZE_MEMO`).
///
/// # Why a cell from the last sweep is this sweep's answer
///
/// A cell `(b, c)` was recorded for the object live at base `b` when sweep N
/// read its header: `c` is `size_cell(header)`. Sweep N+1 asks it only for a
/// base it finds MARKED, i.e. an object live at `b` now. That object is the
/// one sweep N sized, with the same header size bits, because between the two
/// sweeps nothing can put a different object at `b`:
///
/// * only a sweep frees a low object that was live (`registry.remove` has two
///   production callers, the sweep and the slide), and N+1's sweep is the one
///   asking;
/// * the slide of cycle N moves objects: every base it moved AWAY from is
///   removed from the memo here ([`Self::from_hints`] takes the slide's
///   sources), and a destination is never the base of an object that stayed
///   (the slide's own invariant, checked by its screen), so no remaining cell
///   names an address a different object was moved onto;
/// * a live object's size bits never change in place (no production writer
///   of `array_length` / `num_slots` on a live object); the one global input
///   of `alloc_size`, a compact class layout, is pinned by
///   `cratonvm_types::layout_replace_epoch()`, recorded here and compared;
/// * any other collection between the two (a deferred-sweep cycle, a young
///   cycle) moves `gc_count`, and the memo is valid for one `gc_count` only;
///   every collection takes the memo out, so a stale one is never kept.
///
/// Unsizable and oversize survivors carry no cell (`size_cell` is `0` for
/// them) and are sized from their header as before; so is every base the memo
/// does not name (an object allocated since sweep N, a moved one). The
/// accounting (`bytes_copied`, `objects_copied`, the next memo's cells) is the
/// same number the header read would give.
pub struct ZSweepSizeMemo {
    /// The `gc_count()` the next collection's sweep reads (before its own
    /// increment); any other value means a collection ran in between.
    pub valid_at: usize,
    /// `cratonvm_types::layout_replace_epoch()` when the cells were read.
    pub layout_epoch: u32,
    /// Ascending low-region bases, paired with `cells` by index.
    pub addrs: Vec<usize>,
    pub cells: Vec<u32>,
}

impl ZSweepSizeMemo {
    /// The memo of `hints` (ascending, paired) minus every base in `moved`
    /// (any order: the slide's `from` addresses, both ends). `None` when the
    /// hints are not usable.
    pub fn from_hints(
        hints: ZSweepSizeHints,
        mut moved: Vec<usize>,
        valid_at: usize,
        layout_epoch: u32,
    ) -> Option<Self> {
        if !hints.is_usable() {
            return None;
        }
        let ZSweepSizeHints {
            mut addrs,
            mut cells,
        } = hints;
        if !moved.is_empty() {
            moved.sort_unstable();
            // One merge over two ascending lists, compacting in place.
            let mut m = 0usize;
            let mut keep = 0usize;
            let mut i = 0usize;
            while i < addrs.len() {
                let a = addrs[i];
                while m < moved.len() && moved[m] < a {
                    m += 1;
                }
                if m >= moved.len() || moved[m] != a {
                    addrs[keep] = a;
                    cells[keep] = cells[i];
                    keep += 1;
                }
                i += 1;
            }
            addrs.truncate(keep);
            cells.truncate(keep);
        }
        Some(Self {
            valid_at,
            layout_epoch,
            addrs,
            cells,
        })
    }

    /// Is this memo the answer for a sweep at `gc_count` under `layout_epoch`?
    pub fn valid_for(&self, gc_count: usize, layout_epoch: u32) -> bool {
        self.valid_at == gc_count && self.layout_epoch == layout_epoch
    }

    /// The cell recorded for `addr`, or `0`. `cursor` is a merge cursor over
    /// `addrs`, advanced past every base below `addr`; callers ask in
    /// ascending order and start it with [`Self::cursor_for`].
    #[inline]
    pub fn cell_at(&self, cursor: &mut usize, addr: usize) -> u32 {
        while *cursor < self.addrs.len() && self.addrs[*cursor] < addr {
            *cursor += 1;
        }
        if *cursor < self.addrs.len() && self.addrs[*cursor] == addr {
            let c = self.cells[*cursor];
            *cursor += 1;
            c
        } else {
            0
        }
    }

    /// The merge cursor's start for a walk beginning at `addr`.
    pub fn cursor_for(&self, addr: usize) -> usize {
        self.addrs.partition_point(|a| *a < addr)
    }
}

/// Can the low slide of this selection move nothing? (zt t27/pc2,
/// `CRATONVM_ZGC_SLIDE_NO_MOVE_DECLINE`; the moved-nothing slides of
/// `zgc-zt25hunt7b-*` B2: `h7_OOM1_KV`, 62 of 65 cycles.)
///
/// `true` only when the slide's probe could place no survivor of a selected
/// page. That probe moves a survivor `s` (base `from`) to `cand =
/// base + round_up(probe - base, 8)` with `cand < from` and every byte of
/// `[cand, cand + size(s))` on a selected page and clear of obstacles, where
/// the probe starts at the slide cursor `dest`. The window may overlap `s`'s
/// own bytes (the copy is a memmove), so ONE free 8-byte grid cell below
/// `from` can be enough; the size of `s` does not enter. Until the first
/// move, `dest` is the end of the previous SELECTED survivor (or the slide
/// floor, or an obstacle's end, all no lower), and every byte between `dest`
/// and `from` that lies in no live object is a byte of a gap between two
/// consecutive live extents. So the first move needs a gap, after the
/// previous selected survivor, that touches a selected page and holds a grid
/// cell (`round_up(gap start) < next base`).
///
/// One walk of the live set in address order: `pending` = "such a gap has
/// been seen since the last selected survivor"; a selected survivor with
/// `pending` answers `false`, otherwise resets it. Every approximation is on
/// the safe side: a gap counts when ANY of its pages is selected (the window
/// must lie wholly on selected pages), obstacles and the extent cap
/// (`MAX_ALLOC_EXTENT`) are ignored, an unsizable survivor (cell `0`) on a
/// selected page answers `false` outright, and one elsewhere gets no extent
/// (so the gap after it reads longer than it is).
///
/// `cells` is the slide's size table (one cell per entry of `live`, `0`
/// outside `[base, low_end)`); `selected[p]` is page `p`'s membership.
/// Answering `true` lets the caller decline: the slide would copy nothing.
pub fn slide_moves_nothing(
    live: &[usize],
    cells: &[u32],
    base: usize,
    low_end: usize,
    page_bytes: usize,
    selected: &[bool],
) -> bool {
    if cells.len() != live.len() || page_bytes == 0 {
        return false; // no size table: cannot prove anything
    }
    let page_of = |a: usize| (a - base) / page_bytes;
    let is_selected = |p: usize| selected.get(p).copied().unwrap_or(false);
    let mut prev_end = base;
    let mut pending = false;
    for (&b, &cell) in live.iter().zip(cells) {
        if b < base || b >= low_end {
            continue;
        }
        // The probe's own rounding: the first grid cell at or above the
        // previous extent's end.
        let cell_start = base + ((prev_end - base + 7) & !7);
        if !pending && cell_start < b && (page_of(cell_start)..=page_of(b - 1)).any(is_selected) {
            pending = true;
        }
        if is_selected(page_of(b)) {
            if cell == 0 || pending {
                return false;
            }
            // `pending` is false here: it stays so until a gap after this
            // survivor sets it.
        }
        let size = (cell & !ZSIZE_CELL_NOREF) as usize;
        prev_end = prev_end.max(b.saturating_add(size));
    }
    true
}

/// Where `chunk` starts inside `items`, for a chunk [`zgc_par_chunks`] cut out
/// of `items` (its chunks are subslices, so this is pointer arithmetic).
#[inline]
pub fn zgc_chunk_offset(items: &[usize], chunk: &[usize]) -> usize {
    (chunk.as_ptr() as usize).wrapping_sub(items.as_ptr() as usize) / std::mem::size_of::<usize>()
}

/// `CRATONVM_ZGC_FUSED_SLIDE_SCREEN` (zr z2/r2): default ON -- the low slide's
/// `live_ceiling` screen runs inside the reference rewrite, and the
/// large-object end is compacted before the low cursor drops. `=0` restores
/// the separate screen pass and the old order. Read once per process (tests:
/// `FUSED_SLIDE_SCREEN_OVERRIDE`).
pub fn zgc_fused_slide_screen_enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = FUSED_SLIDE_SCREEN_OVERRIDE.with(|c| c.get()) {
        return on;
    }
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    true
}

/// `CRATONVM_DBG_ZGC_SLIDE_PANIC=1` (zr z2/r2): inject a panic right after the
/// low slide's first memmove -- the positive control for [`ZSlideFailStop`].
/// Diagnostic only.
pub fn zgc_dbg_slide_panic_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(zgc_dbg_slide_panic_enabled_requested)
}

/// Uncached half of [`zgc_dbg_slide_panic_enabled`] (zt t6/rv3):
/// `CRATONVM_DBG_ZGC_SLIDE_PANIC` read by value. It read presence, so `=0`
/// turned it on.
pub fn zgc_dbg_slide_panic_enabled_requested() -> bool {
    false}



/// Fail-stop for a panic inside the stop-the-world slide (zr z2/r2; the
/// previous round's open item "`catch_unwind` around the slide",
/// `gap-b-two-relocators-one-of-them-unreachable.md`).
///
/// A `catch_unwind` cannot RECOVER a slide: once the first survivor has been
/// memmoved, the object-start registry, the forwarding table and every
/// reference slot describe different heaps until the function returns, and
/// nothing can roll the copies back. What a panic there used to do is unwind
/// through the collector into whatever caught it -- the JIT helper guards and
/// the interpreter's `catch_unwind`s do -- and resume mutators on that heap.
/// The asserts that can fire in the slide (`compact_low_to`'s "must not raise
/// the cursor", `compact_high_to`'s span checks, a re-raised worker panic) are
/// all corruption detectors, so the honest answer is the one HotSpot gives a
/// failed `guarantee` in a pause: stop the process, saying why.
///
/// Armed just before the first memmove of each end; a panic BEFORE that (the
/// selection, the partition) has written nothing and still unwinds as it did.
/// The drop does nothing on a normal return. `CRATONVM_ZGC_SLIDE_PANIC_ABORT=0`
/// never arms it.
#[derive(Default)]
pub struct ZSlideFailStop {
    pub armed: bool,
}

impl ZSlideFailStop {
    pub fn arm(&mut self) {
        {
            self.armed = true;
        }
    }
}

impl Drop for ZSlideFailStop {
    fn drop(&mut self) {
        if self.armed && std::thread::panicking() {
            eprintln!(
                "[cratonvm] FATAL: zgc relocate: a panic unwound out of the stop-the-world \
                 slide after it had started moving objects. The heap is half-moved (the \
                 object-start registry, the forwarding table and the reference slots \
                 disagree), so no mutator may run on it: aborting. \
                 CRATONVM_ZGC_SLIDE_PANIC_ABORT=0 unwinds instead."
            );
            std::process::abort();
        }
    }
}

/// What one chunk of the slide's reference-rewrite pass did (zr z1/rl).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZRewriteTally {
    /// Survivors `rewrite_target_walkable_sized` refused.
    pub unwalkable: usize,
    /// Reference slots pointed at a destination.
    pub rewritten: usize,
    /// Primitive legacy cells whose bits equalled a moved base and were left
    /// alone (they used to be rewritten).
    pub primitive_cells_skipped: usize,
    /// Non-null reference slots the pass READ (zr z2/r2), rewritten or not:
    /// the walk's own cost unit, which the cost-gate proposal
    /// (`zgc-zr1rl-proposal-cost-gate-by-slot-density-and-fragmentation`)
    /// wants to price the slide by instead of live bytes.
    pub slots_walked: usize,
    /// Survivors served from the size table with no header read (zr z7/pc,
    /// `CRATONVM_ZGC_RELOCATE_SIZE_TABLE`): a primitive array or filler whose
    /// cell said it has no reference slots. `0` on the kill arm.
    pub table_skipped: usize,
    /// zs s2/sl2 (`CRATONVM_ZGC_REWRITE_SIZE_FROM_CELL=1`): survivors WITH
    /// reference slots sized from their pre-slide cell instead of
    /// `survivor_size` on the header. `0` unless the opt-in is on.
    pub cell_sized: usize,
}

impl ZRewriteTally {
    pub fn add(self, o: Self) -> Self {
        Self {
            unwalkable: self.unwalkable + o.unwalkable,
            rewritten: self.rewritten + o.rewritten,
            primitive_cells_skipped: self.primitive_cells_skipped + o.primitive_cells_skipped,
            slots_walked: self.slots_walked + o.slots_walked,
            table_skipped: self.table_skipped + o.table_skipped,
            cell_sized: self.cell_sized + o.cell_sized,
        }
    }
}

/// `CRATONVM_ZGC_REWRITE_SIZE_FROM_CELL` (zs s2/sl2): the rewrite sizes a
/// survivor WITH reference slots from its pre-slide size cell instead of
/// calling `survivor_size` on its post-slide header (the relocation half of
/// `zgc-zr7pc-a-collection-reads-every-live-header-four-times`). Default OFF
/// (opt-in `=1`): it gives up the walkability screen's independent size read,
/// so it ships only on a host A/B that pays for it. Read once per rewrite
/// chunk.
pub fn zgc_rewrite_size_from_cell_enabled() -> bool {
    false
}

// `CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY` (zr z1/rl): default ON -- the
// slide's rewrite pass leaves a legacy cell whose tag is not `Value::Object`
// alone even when its bits equal a moved base. `=0` restores rewriting every
// matching word. zr z3/lt: seeded per heap
// (`heap_state::ZHeapLocalState::rewrite_reference_cells_only`), no longer
// latched once per process.

/// `CRATONVM_DBG_ZGC_REWRITE=1` (zr z1/rl): print one `[zgc-rewrite]` line per
/// slide on stderr. Diagnostic only.
pub fn zgc_dbg_rewrite_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(zgc_dbg_rewrite_enabled_requested)
}

/// Uncached half of [`zgc_dbg_rewrite_enabled`] (zt t6/rv3):
/// `CRATONVM_DBG_ZGC_REWRITE` read by value. It read presence, so `=0`
/// turned it on.
pub fn zgc_dbg_rewrite_enabled_requested() -> bool {
    false}

/// Chunks each worker gets on average under the balanced arm of
/// [`zgc_par_chunks`]. Eight keeps a chunk at ~4 K survivors at the smallest
/// worker share `relocate_workers_for` admits (32 K), which is still far above
/// one counter `fetch_add` per chunk.
pub const ZGC_PAR_CHUNKS_PER_WORKER: usize = 8;

// `CRATONVM_ZGC_PAR_RELOCATE_BALANCE` (zr z1/rl): default ON, `=0` is the
// one-chunk-per-worker split of round 9 wave 7. zr z3/lt: seeded per heap
// (`heap_state::ZHeapLocalState::par_relocate_balance`), read by
// `ZgcRealHeap::par_chunks`; no longer latched once per process.

/// zt t8/rl2: the worker CPU a failed stop-the-world pool charged after the
/// fixed-point snapshot was booked -- the retire's same-cycle total less the
/// snapshot, never negative. Pure, for the test.
pub fn zgc_mark_straggler_cpu(booked_snapshot: u64, retired_total: u64) -> u64 {
    retired_total.saturating_sub(booked_snapshot)
}

/// zt t8/rl2 (`zgc-zs3ac-collection-cpu-residual-shares` items 1 and 3): a
/// spawned or pooled puller's thread-CPU span that adds itself to `sink` when
/// it is dropped -- at the end of the pull, or while a panicking chunk
/// unwinds the puller -- so a pass whose helper panicked still charges the
/// CPU that helper burnt (the pass re-raises the panic either way). One clock
/// pair per thread per pass, as before; `None` reads no clock.
pub struct ZCpuSinkOnExit<'a> {
    pub span: mark::ZThreadCpuSpan,
    pub sink: Option<&'a AtomicU64>,
}

impl<'a> ZCpuSinkOnExit<'a> {
    pub fn start(sink: Option<&'a AtomicU64>) -> Self {
        Self {
            span: mark::ZThreadCpuSpan::start(sink.is_some()),
            sink,
        }
    }
}

impl Drop for ZCpuSinkOnExit<'_> {
    fn drop(&mut self) {
        if let Some(sink) = self.sink {
            sink.fetch_add(self.span.elapsed_ns(), Ordering::Relaxed);
        }
    }
}

/// [`zgc_par_chunks`] with the chunks-per-worker factor explicit, so the tests
/// can drive both arms in one process.
///
/// `cpu` (zs s3/ac): when `Some`, each SPAWNED puller adds its thread CPU for
/// the pass to it -- one clock pair per thread per pass, never per chunk. The
/// caller's own pull is the collecting thread, whose whole pause is booked
/// separately (`pause_thread_ms=`), so it is not added here.
pub fn zgc_par_chunks_with<T, F>(
    items: &[usize],
    workers: usize,
    per_worker: usize,
    cpu: Option<&AtomicU64>,
    f: F,
) -> Vec<T>
where
    T: Send,
    F: Fn(&[usize]) -> T + Sync,
{
    if workers <= 1 || items.len() < 2 {
        return vec![f(items)];
    }
    let n_chunks = workers.saturating_mul(per_worker.max(1)).min(items.len());
    let per = items.len().div_ceil(n_chunks).max(1);
    let chunks: Vec<&[usize]> = items.chunks(per).collect();
    let threads = workers.min(chunks.len());
    let next = AtomicUsize::new(0);
    let f = &f;
    let chunks_ref = &chunks;
    let next_ref = &next;
    // One puller: claim the next chunk index until none is left, and keep
    // `(index, answer)` so the caller can put the answers back in order.
    let pull = move || -> Vec<(usize, T)> {
        let mut out = Vec::new();
        loop {
            let i = next_ref.fetch_add(1, Ordering::Relaxed);
            let Some(chunk) = chunks_ref.get(i) else {
                break;
            };
            out.push((i, f(chunk)));        }
        out    };
    let mut slots: Vec<Option<T>> = Vec::with_capacity(chunks.len());
    slots.resize_with(chunks.len(), || None);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (1..threads)
            .map(|_| {
                scope.spawn(move || {
                    // Booked on drop, so a panicking chunk still charges.
                    let _cpu = ZCpuSinkOnExit::start(cpu);
                    pull()
                })
            })
            .collect();
        // The caller pulls too: one spawn fewer per pass, and it would
        // otherwise only wait at the join.
        let mine = pull();
        let mut parts = vec![mine];
        for h in handles {
            match h.join() {
                Ok(v) => parts.push(v),
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
        for (i, v) in parts.into_iter().flatten() {
            slots[i] = Some(v);
        }
    });
    slots
        .into_iter()
        .map(|s| s.expect("every chunk index is claimed exactly once"))
        .collect()
}

/// zs s3/cr: the threads [`zgc_par_chunks_with`] runs a pass on (the caller
/// included), for `len` items. Pure; the count `reloc_par_passes[1]` books.
pub fn zgc_par_threads(len: usize, workers: usize, per_worker: usize) -> usize {
    if workers <= 1 || len < 2 {
        return 1;
    }
    let n_chunks = workers.saturating_mul(per_worker.max(1)).min(len);
    let per = len.div_ceil(n_chunks).max(1);
    workers.min(len.div_ceil(per))
}

/// `CRATONVM_ZGC_RELOCATE_POOL` (zs s3/cr): run the relocation passes on the
/// heap's persistent pool instead of per-pass scoped threads. Default OFF
/// (opt-in `=1`); read once per pass.
pub fn zgc_relocate_pool_enabled() -> bool {
    false
}

/// [`zgc_par_chunks_with`] on a persistent [`cratonvm_gc_core::evac_pool::EvacPool`]
/// (zs s3/cr): the same chunks, the same pulling, the same answers in chunk
/// order, the caller pulling too. `pool.scope` is the barrier `thread::scope`
/// was: it returns only after every helper's call has returned (including by
/// unwinding), and re-raises a helper's panic payload on the caller, as the
/// scoped arm's join does. The pool is grown to `threads - 1` helpers first;
/// a helper the OS refuses narrows the pass (more chunks pulled by fewer
/// threads), never changes its answer.
pub fn zgc_par_chunks_pooled<T, F>(
    pool: &cratonvm_gc_core::evac_pool::EvacPool,
    items: &[usize],
    workers: usize,
    per_worker: usize,
    cpu: Option<&AtomicU64>,
    f: F,
) -> Vec<T>
where
    T: Send,
    F: Fn(&[usize]) -> T + Sync,
{
    if workers <= 1 || items.len() < 2 {
        return vec![f(items)];
    }
    let n_chunks = workers.saturating_mul(per_worker.max(1)).min(items.len());
    let per = items.len().div_ceil(n_chunks).max(1);
    let chunks: Vec<&[usize]> = items.chunks(per).collect();
    let threads = workers.min(chunks.len());
    let next = AtomicUsize::new(0);
    let pull = || -> Vec<(usize, T)> {
        let mut out = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(chunk) = chunks.get(i) else {
                break;
            };
            out.push((i, f(chunk)));        }
        out    };
    let helper_parts: Mutex<Vec<Vec<(usize, T)>>> = Mutex::new(Vec::new());
    let mut mine: Vec<(usize, T)> = Vec::new();
    pool.ensure_helpers(threads - 1);
    let body = |_helper: usize| {
        // zt t1/mk (X-rl1, zs4rv3 item 1): book the pooled helpers' CPU.
        // zt t8/rl2: on drop, so a helper whose chunk panics still charges.
        let part = {
            let _cpu = ZCpuSinkOnExit::start(cpu);
            pull()
        };
        helper_parts.lock().push(part);
    };
    pool.scope(threads - 1, &body, || mine = pull());
    let mut slots: Vec<Option<T>> = Vec::with_capacity(chunks.len());
    slots.resize_with(chunks.len(), || None);
    for (i, v) in mine
        .into_iter()
        .chain(helper_parts.into_inner().into_iter().flatten())
    {
        slots[i] = Some(v);
    }
    slots
        .into_iter()
        .map(|s| s.expect("every chunk index is claimed exactly once"))
        .collect()
}

/// The cost gate's ratio: decline when the selected pages' garbage is less
/// than `1 / 32` (~3 %) of the low region's live bytes. At that ratio the
/// rewrite pass walks at least 32 live bytes per byte it can make contiguous.
pub const ZGC_RELOCATE_COST_GATE_RATIO: usize = 32;

/// The cost gate's decision, pure so it can be tested without a heap.
///
/// Declines only when BOTH hold:
/// * the heap is not tight -- at least `ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE` of
///   capacity is free, so no request is waiting on the contiguity the slide
///   would buy;
/// * the garbage the slide can gather is under `1 / RATIO` of the live bytes
///   it must rewrite around.
///
/// `low_live == 0` never declines: an empty low region costs nothing to walk.
pub fn relocate_cost_gate_declines(
    selected_garbage: usize,
    low_live: usize,
    free_permille: usize,
) -> bool {
    free_permille >= ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE
        && low_live > 0
        && selected_garbage.saturating_mul(ZGC_RELOCATE_COST_GATE_RATIO) < low_live
}

/// Why a low slide's selector runs with a widened occupancy cutoff (zr z2/cp,
/// `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT`). The discriminant indexes
/// `ZgcCounters::pressure_select`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocatePressure {
    /// The cycle began with [`ZgcRealHeap::hard_alloc_failure`] raised: an
    /// allocation was refused, and this collection is the try/GC/try ladder's
    /// GC (or the native boundary's relief for it).
    AllocFailure = 0,
    /// No refusal, but the low end's bump headroom (`Arena::low_bump_headroom`)
    /// is under `ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE` of capacity, so most
    /// requests are served from free-list holes. Opt-in (`=2`).
    Tight = 1,
}

/// The occupancy cutoff after an allocation failure: evacuate any page more
/// than 5 % garbage.
///
/// HotSpot's figure, RECALLED and not cited (no OpenJDK source was read for
/// this, the same disclosure `ZRelocationPolicy::relocate_large_pages`
/// makes): generational ZGC selects an OLD page for relocation when its
/// garbage exceeds `ZFragmentationLimit` (5 %) of the page, a YOUNG page when
/// it exceeds `ZYoungCompactionLimit` (25 %). This collector's default
/// (`ZFWD_DEFAULT_MAX_LIVE_OCCUPANCY`, 0.25 live = 75 % garbage) is the
/// inverse of the young figure, so HotSpot compacts pages this selector
/// never looks at, and the measured cost was capacity: `Zr2CpCapacity grow`
/// fit 360 145 retained `int[64]` against HotSpot's 922 640 at -Xmx256m.
/// After a refused request nothing but evacuation can make the contiguity it
/// lacked, so this tier takes the old-page figure. The 64 MiB evacuation
/// budget still bounds the copy.
pub const ZGC_PRESSURE_FAILURE_MAX_LIVE_OCCUPANCY: f64 = 0.95;

/// The occupancy cutoff on a tight heap with no failure (opt-in tier): the
/// young-page figure above, garbage over 25 %.
pub const ZGC_PRESSURE_TIGHT_MAX_LIVE_OCCUPANCY: f64 = 0.75;

/// `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT` (zr z2/cp): `0` / off = never
/// widen (the kill switch, the pre-z2 selector); `1` = widen after a hard
/// allocation failure only; unset / `2` = also widen on a tight heap with no
/// failure (DEFAULT since the z2/cp flip, 2026-10-13: host z2b, `Zr2CpCapacity
/// mosaic -Xmx256m`, 6 interleaved pairs, CPU 9.1-10.6 s against 18.2-22.4 s
/// with `=1`, retained 920 337-920 947 against 923 362-923 386, HotSpot
/// 900 237). Any other word reads as `2`. Read per cycle so a test can A/B
/// it.
///
/// zt t21/ca7: an EMPTY value reads as the default `2`, as an empty
/// `CRATONVM_ZGC_SELECTIVE_RELOCATE` does and as every default-ON key does
/// (`runtime_flag_default_on`: only `0`/`off`/`false`/`no` turn one off). It
/// read as the `=0` kill until then.
pub fn zgc_relocate_pressure_select_mode() -> u8 {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            match v.as_str() {
                "0" | "false" | "off" | "no" => 0,
                "1" | "failure" => 1,
                _ => 2,
            }
        }
        None => 2,
    }
}


/// zr z9/sl, `CRATONVM_ZGC_SELECTIVE_RELOCATE`: the selective arm's knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZSelectiveRelocate {
    /// A free gap at least this long is already usable by the allocator's
    /// contiguous requests, so it is not fragmented garbage.
    pub hole_min: usize,
    /// A page with survivors is evacuated only when at least this percentage
    /// of its extent is fragmented garbage. `0` = any fragmented byte.
    /// [`ZSelectiveMode::PerPage`] only.
    pub frag_pct: usize,
    /// Which decision the arm makes (zs s1/sl).
    pub mode: ZSelectiveMode,
}

/// zs s1/sl: the two keep/evacuate decisions of
/// `CRATONVM_ZGC_SELECTIVE_RELOCATE`.
///
/// The z9 arm (`=1`, [`Self::PerPage`]) filtered PAGES: a sparse page whose
/// holes were already long stayed in place. Host z9a measured it -11 % on
/// `Zr7PcCollectionCpu prim` and +12 % CPU on `MtChurnProbe 8 2500 384`,
/// where it kept ~13 k pages per run and took the failure fallback in one
/// run in two. Two things in that shape are wrong by reading:
///
/// * the slide's fixed cost (the rewrite over every live slot) is paid ONCE
///   per slide, whatever the number of pages: once any page is worth
///   evacuating, leaving the other sparse pages in place saves only their
///   copy (their live bytes, small by selection) and gives up their
///   contiguity. The per-page filter is the wrong granularity for the cost
///   it tries to avoid;
/// * a page kept in place keeps its holes on the free list, so the next
///   cycle's allocation runs from recycled holes rather than the bump tail
///   (every refill a free-list carve), and a heap that fills that way reaches
///   the failure fallback, the most expensive collection there is.
///
/// [`Self::CycleGate`] (`=2`) decides per CYCLE instead: slide with the
/// default selector, unfiltered, when the candidate pages hold enough
/// fragmented garbage to pay the rewrite (the cost gate's 1/32 of the low
/// live bytes) or when the low bump headroom is under the fragmentation
/// gauge's quarter (the allocator already lives on holes); otherwise decline
/// the slide outright. Its input is the same per-page fragmented garbage,
/// taken in the fused `logical_pages` pass (`CRATONVM_ZGC_SELECTIVE_FUSED_FRAG`),
/// so the decision itself reads no header and walks nothing extra.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZSelectiveMode {
    /// zr z9/sl: drop sparse pages whose holes are long (`=1`, any on-word).
    PerPage,
    /// zs s1/sl: slide unfiltered or not at all (`=2` / `cycle`).
    CycleGate,
}

/// `CRATONVM_ZGC_SELECTIVE_RELOCATE_HOLE_KIB`'s default: `ZGC_LARGE_OBJECT_MIN`
/// (64 KiB), the size from which a request leaves the small-object end, and
/// `ZGC_TLAB_MAX_CHUNK / 8`, the smallest recycled chunk the TLAB carve takes
/// (`recycled_chunk_size`).
pub const ZGC_SELECTIVE_HOLE_MIN_DEFAULT: usize = 64 * 1024;

/// `CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT`'s default: HotSpot's
/// `ZFragmentationLimit` (25, non-generational ZGC), RECALLED and not cited,
/// the same disclosure `ZGC_PRESSURE_FAILURE_MAX_LIVE_OCCUPANCY` makes.
pub const ZGC_SELECTIVE_FRAG_PCT_DEFAULT: usize = 25;

/// `CRATONVM_ZGC_SELECTIVE_RELOCATE` (zr z9/sl): `0` / an off-word = `None`
/// (the default selector, unfiltered: the kill switch); anything else = the selective
/// arm with `CRATONVM_ZGC_SELECTIVE_RELOCATE_HOLE_KIB` (default 64, `0` or
/// unparsable = default) and `CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT`
/// (default 25, clamped to 100). Read per slide so a test can A/B it.
///
/// zs s1/sl: `2` / `cycle` selects [`ZSelectiveMode::CycleGate`]; every
/// other on-word (`1`, `true`, ...) keeps the z9 [`ZSelectiveMode::PerPage`]
/// arm byte for byte.
///
/// zs s2: unset (or empty) = [`ZSelectiveMode::CycleGate`], default ON. Host
/// A/B on `cvm-s1b` (7 interleaved rounds, `ab-s1bf`): `Zr3MtMarkShapes hubs`
/// -9 % CPU, `Zr7PcCollectionCpu prim` -2 %, `MtChurnProbe` +0.7 %,
/// `Zr2CpCapacity grow` +1.3 %, `Zr3JaRefArrayAlloc` equal; every stdout
/// equal, the sl SL4 / SL6 rows SAME, SL5 retained equal to OFF's.
/// `CRATONVM_ZGC_SELECTIVE_RELOCATE=0` restores the unfiltered selector.
pub fn zgc_selective_relocate_config() -> Option<ZSelectiveRelocate> {
    let mode = match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            match v.as_str() {
                "" => Some(ZSelectiveMode::CycleGate),
                "0" | "false" | "off" | "no" => None,
                "2" | "cycle" => Some(ZSelectiveMode::CycleGate),
                _ => Some(ZSelectiveMode::PerPage),
            }
        }
        // zs s2 (orchestrator): default ON as the cycle gate (`=2`).
        None => Some(ZSelectiveMode::CycleGate),
    };
    let mode = mode?;
    let num = |key: &str| -> Option<usize> {
        cratonvm_types::flags::runtime_var_os(key)
            .and_then(|raw| raw.to_string_lossy().trim().parse::<usize>().ok())
    };
    let hole_min = num("CRATONVM_ZGC_SELECTIVE_RELOCATE_HOLE_KIB")
        .filter(|k| *k > 0)
        .map_or(ZGC_SELECTIVE_HOLE_MIN_DEFAULT, |k| k.saturating_mul(1024));
    let frag_pct = num("CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT")
        .map_or(ZGC_SELECTIVE_FRAG_PCT_DEFAULT, |p| p.min(100));
    Some(ZSelectiveRelocate {
        hole_min,
        frag_pct,
        mode,
    })
}

/// `CRATONVM_ZGC_SELECTIVE_FUSED_FRAG` (zs s1/sl): the selective arm's
/// fragmented-garbage figure is booked inside `logical_pages`' own pass over
/// the live set, from the size it already reads, instead of a second
/// whole-live-set walk (`fragmented_garbage_per_page`). Default ON; `=0`
/// restores the separate walk. Same answer on an ascending live list (test
/// `the_fused_fragmented_census_matches_the_separate_walk`). Only consulted
/// when `CRATONVM_ZGC_SELECTIVE_RELOCATE` is on, so the default selector is
/// untouched either way.
pub fn zgc_selective_fused_frag_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => true,
    }
}

/// `CRATONVM_ZGC_SLIDE_SIZE_FROM_TABLE` (zs s1/sl): the low slide's loop
/// sizes each selected survivor from the size cell the page census took
/// (`CRATONVM_ZGC_RELOCATE_SIZE_TABLE`) instead of reading its header again.
/// Default ON; `=0` restores the header read. Read once per slide.
pub fn zgc_slide_size_from_table_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => true,
    }
}

/// `CRATONVM_ZGC_SLIDE_UNSIZABLE_OBSTACLE` (zs s1/sl): an unsizable survivor
/// on an unselected page is an obstacle up to `low_end` in the low slide's
/// placement probe, so nothing is copied over the body it may have. Default
/// ON; `=0` restores the pre-zs partition (no obstacle; the post-slide
/// ceiling screen alone). Read once per slide.
pub fn zgc_slide_unsizable_obstacle_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => true,
    }
}

/// A default-ON switch: unset, empty, or any word but an off-word, is ON.
///
/// zt t2/rv (`zgc-zs3rv2-wave-s2-review-residuals-CLOSED-20261019.md` §6): these
/// are safety screens, so `KEY=` (empty) must not turn one off; it did, by a
/// private copy of [`zgc_default_on_switch`]'s parse. Now the shared rule
/// (`runtime_flag_default_on`), whose off-words are the same four.
pub fn zgc_sl2_default_on(key: &str) -> bool {
    cratonvm_types::flags::runtime_flag_default_on(key)
}

/// `CRATONVM_ZGC_SLIDE_PIN_BY_BASE` (zs s2/sl2): the low slide's pin screen
/// also drops the page of the OBJECT each pin lies in
/// (`resolve_interior_for_pin`), not only the page of the pin word. Default
/// ON; `=0` restores the address-page screen alone. Read once per slide.
pub fn zgc_slide_pin_by_base_enabled() -> bool {
    zgc_sl2_default_on("CRATONVM_ZGC_SLIDE_PIN_BY_BASE")
}

/// `CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL` (zs s2/sl2): the relocation-set
/// telemetry (JFR `jdk.ZRelocationSet`, `targeted_pages_selected`, the
/// pressure tier's live bytes) books the set the slide relocates, after the
/// pin screen and the gates. Default ON; `=0` books the selector's answer as
/// before. Read once per slide.
pub fn zgc_relocation_set_books_final_enabled() -> bool {
    zgc_sl2_default_on("CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL")
}

/// `CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS` (zs s2/sl2): the cost gate and
/// the selective arm's empty-set decline exempt a targeted cycle only while a
/// targeted page survived the pin screen. Default ON; `=0` tests the policy's
/// window as before. Read once per slide.
pub fn zgc_target_exemption_after_pins_enabled() -> bool {
    zgc_sl2_default_on("CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS")
}

/// Is this a TARGETED cycle for the low slide's two gates (zs s2/sl2)? With
/// `after_pins` (the default), only while some page of the window `target`
/// is still in `selected`; on the kill arm whenever a window was named.
pub fn targeted_cycle_after_pins(
    target: Option<(u64, u64)>,
    selected: &FxHashSet<u64>,
    after_pins: bool,
) -> bool {
    match target {
        None => false,
        Some(_) if !after_pins => true,
        Some((lo, hi)) => selected.iter().any(|&id| id >= lo && id <= hi),
    }
}

/// [`ZSelectiveMode::CycleGate`]'s answer for one cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectiveCycle {
    /// Enough fragmented garbage in reach: run the default selector unchanged.
    Admit,
    /// The low bump headroom is tight: run the default selector unchanged.
    Tight,
    /// Nothing the slide would make contiguous pays the rewrite: no slide.
    Decline,
}

/// zs s3/cr: the `selective_relocate` slot a [`SelectiveCycle`] verdict books
/// (`[4]` admit, `[5]` decline, `[6]` tight).
pub fn selective_cycle_slot(verdict: SelectiveCycle) -> usize {
    match verdict {
        SelectiveCycle::Admit => 4,
        SelectiveCycle::Decline => 5,
        SelectiveCycle::Tight => 6,
    }
}

/// zs s3/cr (`CRATONVM_ZGC_SELECTIVE_GATE_ON_SELECTION`): the fragmented
/// garbage the `=2` gate prices when it is asked on the SELECTION -- the sum of
/// `frag[page]` over the candidate pages with survivors that `selected` still
/// holds (the budget prefix after the pin screen). An empty page has nothing
/// to copy and is a destination only, as in the old input. Pure, for the tests.
pub fn cycle_gate_frag_on_selection(
    candidates: &[forwarding::PageCandidate],
    selected: &FxHashSet<u64>,
    frag: &[usize],
) -> usize {
    candidates
        .iter()
        .filter(|c| c.live_bytes > 0 && selected.contains(&c.page_id))
        .map(|c| frag.get(c.page_id as usize).copied().unwrap_or(0))
        .sum()
}



/// The cycle gate, pure so it can be tested without a heap.
///
/// * `frag_in_reach`: fragmented garbage on the pages the default selector
///   could pick (survivors, garbage, occupancy under the cycle's cutoff);
/// * `low_live`: the low region's live bytes, the rewrite's size;
/// * `bump_free_permille`: the low bump headroom per mille of capacity.
///
/// Tight first: below [`ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE`] the next requests
/// are served from holes and contiguity is what they lack, which is the
/// default selector's case whatever the holes' length. Then the cost gate's
/// own ratio ([`ZGC_RELOCATE_COST_GATE_RATIO`]) over the fragmented bytes. An
/// empty low region admits (the default path; nothing to walk).
pub fn selective_cycle_gate(
    frag_in_reach: usize,
    low_live: usize,
    bump_free_permille: usize,
) -> SelectiveCycle {
    if bump_free_permille < ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE {
        return SelectiveCycle::Tight;
    }
    if low_live == 0 || frag_in_reach.saturating_mul(ZGC_RELOCATE_COST_GATE_RATIO) >= low_live {
        return SelectiveCycle::Admit;
    }
    SelectiveCycle::Decline
}

/// zr z11/rv9: parallel-mark cycles marked serially after a cycle on which
/// no mark pool could be spawned (`CRATONVM_ZGC_MARK_SPAWN_BACKOFF`). A
/// spawn failure is an OS thread limit or memory, neither of which clears
/// in one collection; 16 cycles bounds the retry cost to one failed spawn
/// per 17 collections and still recovers within a few seconds of churn.
pub const ZGC_MARK_SPAWN_BACKOFF_CYCLES: usize = 16;



/// The mark end's bound on [`zgc_satb_flush_pause_safe_enabled`]'s wait. A
/// flusher in its window is running, so it is normally microseconds; past
/// this the cycle falls back to a stop-the-world mark.
pub const Z_SATB_FLUSH_WAIT_MS: u64 = 100;





/// `CRATONVM_ZGC_SELECTIVE_BYPASS_ON_LATCH` (zr z11/rv9): the selective arm's
/// failure bypass reads the cycle's allocation-failure latch itself rather
/// than the pressure tier (which is `None` under
/// `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT=0`). Default ON; `=0` restores the
/// z9/sl read. Only consulted when `CRATONVM_ZGC_SELECTIVE_RELOCATE` is on.
pub fn zgc_selective_bypass_on_latch_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => true,
    }
}

/// Does the selective arm keep page `(live, capacity, frag)` as a relocation
/// candidate? Pure so it can be tested without a heap. A targeted page and a
/// page with no survivor (a destination: nothing to copy) always stay; a page
/// with survivors stays when its fragmented garbage is at least `frag_pct` %
/// of its extent (and is non-zero).
pub fn selective_page_admits(
    live_bytes: usize,
    capacity_bytes: usize,
    frag_bytes: usize,
    frag_pct: usize,
    targeted: bool,
) -> bool {
    if targeted || live_bytes == 0 {
        return true;
    }
    frag_bytes > 0 && frag_bytes.saturating_mul(100) >= frag_pct.saturating_mul(capacity_bytes)
}

/// Book the free gap `[lo, hi)` (absolute addresses, `base`-relative pages)
/// into `frag` when it is shorter than `hole_min`, split at page boundaries.
/// Empty, inverted or long gaps add nothing; a page past the vector's end is
/// ignored.
pub fn add_fragmented_gap(frag: &mut [usize], base: usize, lo: usize, hi: usize, hole_min: usize) {
    if hi <= lo || hi - lo >= hole_min || lo < base {
        return;
    }
    let page = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    let mut at = lo;
    while at < hi {
        let idx = (at - base) / page;
        let page_end = base + (idx + 1) * page;
        let upto = page_end.min(hi);
        if let Some(slot) = frag.get_mut(idx) {
            *slot += upto - at;
        }
        at = upto;
    }
}

/// `CRATONVM_ZGC_INFALLIBLE_OOME_UNWIND` (zr z2/cp): the infallible trait arms
/// offer the VM's exhaustion unwind before aborting. Default ON, `=0` restores
/// the unconditional abort. Read on the failure path only.
pub fn zgc_infallible_oome_unwind_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => true,
    }
}

/// The tier a cycle's selector runs under, pure so it can be tested without a
/// heap. `mode` is [`zgc_relocate_pressure_select_mode`]'s answer.
pub fn relocate_pressure_tier(
    mode: u8,
    after_alloc_failure: bool,
    free_permille: usize,
) -> Option<RelocatePressure> {
    if mode == 0 {
        return None;
    }
    if after_alloc_failure {
        return Some(RelocatePressure::AllocFailure);
    }
    if mode >= 2 && free_permille < ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE {
        return Some(RelocatePressure::Tight);
    }
    None
}

/// The widened `max_live_occupancy` for `tier`.
pub fn relocate_pressure_occupancy(tier: RelocatePressure) -> f64 {
    match tier {
        RelocatePressure::AllocFailure => ZGC_PRESSURE_FAILURE_MAX_LIVE_OCCUPANCY,
        RelocatePressure::Tight => ZGC_PRESSURE_TIGHT_MAX_LIVE_OCCUPANCY,
    }
}

/// zs s5/ta: `CRATONVM_DBG_ZGC_POST_REFUSAL` -- after each `alloc_raw`
/// refusal, log the next N hand-outs (`[zgc-post-refusal]` lines on stderr):
/// size, arena offset, and what the span held before it was zeroed (`zero`,
/// `tlab-filler`, or `corpse` with the dead object's first header word). It
/// names the bytes that serve a request after a heap fill's last refusal
/// (`zgc-zs5ta-bytes-served-after-a-final-refusal-are-unattributed-SUPERSEDED-20261019.md`).
/// Unset, empty or `0`/`off`/`false`/`no`: off. A number of at least 2: that
/// many hand-outs per refusal. Any other value (`1` included): 16. Read once
/// per heap.
pub fn zgc_dbg_post_refusal_limit() -> usize {
    match None::<std::ffi::OsString> {
        None => 0,
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            if v.is_empty() || matches!(v.as_str(), "0" | "off" | "false" | "no") {
                0
            } else {
                // `1` is the on-word the `CRATONVM_DBG=zgc-post-refusal`
                // token spelling sets, not a count.
                v.parse::<usize>().ok().filter(|&n| n > 1).unwrap_or(16)
            }
        }
    }
}

/// zt t2/al: `CRATONVM_ZGC_CHUNK_ZERO_DESCENDING` -- zero each TLAB chunk in
/// pieces from the top down (`ZeroPlan::zero_descending`), so the bytes the
/// bump hands out first are the last written and the likeliest to be cached
/// when the first headers land in them. The zr7ca chunk-zeroing-frontier
/// proposal's cache argument, tested with no soft `Tlab::end` (the cost that
/// retired `CRATONVM_ZGC_LAZY_TLAB_ZERO`, 00274a14c): the chunk is still
/// zeroed whole at the carve, the same bytes, so nothing a reader of the chunk
/// can see changes. **default on** (16 KiB pieces); `=0`/`off`/`false`/`no` kills
/// (the ascending whole-chunk `memset`); `1`/`on`/`true`/`yes`/empty = 16 KiB
/// pieces; a number = that many KiB, clamped to `[4, 256]`. Returns the piece
/// size in BYTES (`0` = off). Read once per heap.
pub fn zgc_chunk_zero_descending_block() -> usize {
    match None::<std::ffi::OsString> {
        None => 16 * 1024,
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            match v.as_str() {
                "0" | "off" | "false" | "no" => 0,
                "" | "1" | "on" | "true" | "yes" => 16 * 1024,
                other => other
                    .parse::<usize>()
                    .ok()
                    .map_or(16 * 1024, |kib| kib.clamp(4, 256) * 1024),
            }
        }
    }
}

/// zs s10/sp: `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS` -- the floor of the
/// final-refusal threshold ([`ZgcRealHeap::note_final_refusal`]).
/// `0`/`off`/`false`/`no`: `0` (off; the `=0` kill since zt t12). Unset (the
/// default since zt t12), empty, and `1`/`on`/`true`/`yes`:
/// 2048, HotSpot's `MinTLABSize` (after a failed refill HotSpot serves a small
/// request only from a new TLAB or fresh page space, never from a hole). A
/// number of at least 16: that floor (`16` is the literal rule of the
/// proposal: blocks smaller than the refused request), capped at
/// `ZGC_LARGE_OBJECT_MIN`. Any other value -- a number from 2 to 15, a word
/// that is not one of the above -- arms the `on` floor, 2048 (zt t2/x1: the
/// doc did not say so). Read once per heap.
///
/// zt t12 (orchestrator): default ON (`=0` kill), decided by lane t10/pl2's
/// rule over its 212-row grid (`t10-zs9la-grid-verdict.txt`): the launcher
/// OOME cells EXACT 20/20 in both modes with the rule (the default served the
/// receiver in 3 of 4 cells), SP-2/3/4 unchanged, no abort, CPU 0.918x
/// (cpucap) and 0.709x (cpuproxy) over interleaved runs.
///
/// zt t21/ca7 (`zgc-zt11pr-opt-in-flip-decisions` section 0b item 4): an EMPTY
/// value is ON, the module rule for a default-ON key (`zgc_flag_default_on`,
/// `runtime_flag_default_on`: only `0`/`off`/`false`/`no` turn one off). It
/// read as the kill since the zt t12 flip, the one ZGC default-ON key whose
/// `KEY=` disagreed with the rule.
pub fn zgc_retire_sub_request_spans_floor() -> usize {
    match None::<std::ffi::OsString> {
        None => 2048,
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            match v.as_str() {
                "0" | "off" | "false" | "no" => 0,
                "" | "1" | "on" | "true" | "yes" => 2048,
                other => other
                    .parse::<usize>()
                    .ok()
                    .filter(|&n| n >= 16)
                    .map(|n| n.min(ZGC_LARGE_OBJECT_MIN))
                    .unwrap_or(2048),
            }
        }
    }
}

/// gkr (`T1W13BigAfterChurn big-then-chain`): `CRATONVM_ZGC_MOVABLE_PINS`
/// (default ON, `=0` kills) -- the relocating pauses drop from their pin set
/// the words this thread's band scan claimed movable, when this cycle's
/// coverage proof passed
/// (`gc_quiescence::pinned_jit_roots_snapshot_honouring_movable`). A pinned
/// live argument in `main`'s single-pass frame held a 16-byte lambda in the
/// middle of the arena and split the free space below a 52 MiB request.
/// Read per pause, no static.
pub fn zgc_movable_pins_honoured() -> bool {
    !cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete()
}

/// `CRATONVM_DBG_ZGC_HIGH`, read by value on every call (zt t6/rv3).
pub fn dbg_zgc_high() -> bool {
    false}

/// `CRATONVM_ZGC_ASSUME_REWRITABLE`, read by value on every call (zt t6/rv3).
pub fn zgc_assume_rewritable() -> bool {
    false
}

/// `CRATONVM_ZGC_HIGH_COMPACTION=0` — the kill switch for the LARGE-OBJECT
/// end's compactor. Default ON.
///
/// **Default-ON where `CRATONVM_ZGC_TARGETED_COMPACTION` was default-OFF (and
/// retired in zt t12/rt), and the difference is engagement.** That flag
/// engaged zero times on every workload measured; this one exists because the measurement
/// which proved that also named what would engage — `in_low_region=false` on
/// the very window it was selecting — and repairs precisely it. Shipping it off
/// would leave the H2 fragmentation family with no repair in any default
/// configuration, which is the state this replaces.
///
/// `=0` is the bisect, and it is a re-run rather than a rebuild: with it the
/// high end is left exactly as it was before 2026-08-29, so a same-binary A/B
/// is possible. The counters on the `[GC] zgc-features:` line
/// (`high_compaction_cycles`, `high_compaction_declined`) say which arm ran,
/// because a kill switch nobody can see the effect of is not a bisect lever.
///
/// Read once at heap construction into [`ZgcRealHeap::high_compaction_enabled`]:
/// it decides what a collection does and must not change mid-cycle.
/// `CRATONVM_ZGC_PUBLISH_VACATED=0` — the kill switch for handing the slide's
/// vacated spans back to the free list. Default ON.
///
/// With it off, reclaim is the cursor drop and nothing else, which is what it
/// was before 2026-08-29 — so the A/B is a re-run and not a rebuild. It is a
/// switch rather than an unconditional change because this is the one repair on
/// this branch that ADDS memory to the allocator's free list on a claim about
/// what is dead, and a wrong claim there is a double hand-out rather than a
/// missed optimisation. The screen that backs the claim is a check on the
/// answer (see the call site), and this is what a future reader bisects with if
/// it is ever wrong.
/// Read once at heap construction into
/// [`ZgcRealHeap::publish_vacated_enabled`], per heap rather than as a
/// process-wide `OnceLock`, so the A/B is runnable inside one test binary.
pub fn publish_vacated_requested_by_default() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// Kill switch: `CRATONVM_ZGC_PRISTINE_CHUNKS`. **Default on.**
///
/// On, `carve_tlab_chunk` does not `memset` the part of a TLAB chunk the arena
/// proves has never been written since the heap was created (its pristine
/// window -- `Arena::pristine_lo`): fresh reservation or `alloc_zeroed` memory
/// that already reads as zero. Standalone `CratonBench bintrees` never
/// collects, so every one of its chunks is pristine and it gains about 8 %.
///
/// # This is as far as the chunk zeroing goes, and that is a measurement
///
/// After the first collection the bump cursor is retracted onto memory that has
/// been written, so nothing is pristine and the whole chunk is memset again:
/// 1.79 GB of the 2.19 GB `CratonBench bintrees -Xmx512m` carves. Deleting even
/// THAT outright -- the ceiling of every "zero per object" or "zero off the
/// allocation path" design, measured with a throwaway dial on 2026-09-20 -- is
/// worth 2.1 % of that run and 0.8 % of `IrAllocLoop`, against a coordinated
/// change to four allocation paths whose failure mode is silent heap
/// corruption. So the retired ZGC TLAB chunk-zeroing write-up (round 9
/// waves 9-11, closed 2026-09-20) is CLOSED rather than fixed, and this
/// switch is the part of it that shipped. `--verbose:gc`'s `zgc-tlab-zero` line is how
/// those numbers are read back out of a run.
///
/// `0` / `off` / `false` / `no` restore the unconditional chunk `memset`, which
/// is what a reader bisects with if a chunk ever shows non-zero bytes. Read
/// once per heap at construction, like [`publish_vacated_requested_by_default`].
/// Should `collect_garbage` adopt the sweep's dead-address buffer (a swap) or
/// copy into the one `ZgcCounters::dead_scratch` already holds?
///
/// # Why this is not `swept_cap >= scratch_cap`
///
/// The scratch exists so the 96 MB `Vec<usize>` a whole-heap cycle produces is
/// allocated once rather than per cycle, and
/// `the_dead_scratch_is_cleared_and_its_capacity_reused` pins that: a SMALL
/// cycle following a large one must not leave the heap holding the small
/// buffer, because the next large cycle would then grow from scratch.
///
/// An exact `>=` honours that test and still loses in the steady state. The
/// sweep sizes its list exactly, so on a repeating workload the incoming
/// capacity is the previous cycle's dead count give or take a fraction of a
/// percent -- and every cycle that reclaims slightly LESS than the one before
/// takes the copy branch. Three of `CratonBench bintrees -Xmx512m`'s five
/// cycles do exactly that.
///
/// Half is the hysteresis: a buffer within 2x is the same buffer for reuse
/// purposes, and a cycle that reclaims less than half of the previous one is
/// the case the test describes.
pub fn swap_is_worth_it(swept_cap: usize, scratch_cap: usize) -> bool {
    swept_cap.saturating_mul(2) >= scratch_cap
}

/// zr z7/rs: `CRATONVM_ZGC_ARENA_NO_THP=1` (default on): advise the arena
/// reservation `MADV_NOHUGEPAGE` at heap construction (Linux; a no-op
/// elsewhere, and skipped when `CRATONVM_HEAP_THP` asks for huge pages). On a
/// host whose THP mode is `always` the first touch of a granule otherwise
/// faults a whole 2 MiB page in, so the arena's RSS rounds every TLAB chunk
/// and large-object start up to 2 MiB; HotSpot's ZGC heap is a `memfd` mapping
/// that mode does not reach. Opt-in until the host's
/// `tools/rss-by-mapping.sh` rows show `AnonHugePages` in the `gc:java-heap`
/// class (only an `always` host can); see
/// `docs/feature-designs/zgc-architecture.md`.
pub fn zgc_arena_no_thp_requested() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "on" | "true" | "yes")
        }
        None => false,
    }
}

/// zr z10/mt: put the object-start, mark and old-start bitmaps (each 1/64 of
/// `-Xmx`, written sparsely) on private `MADV_NOHUGEPAGE` mappings of their
/// own instead of `alloc_zeroed` blocks in the process allocator, whose arena
/// the launcher's mimalloc commits eagerly and advises `MADV_HUGEPAGE` (see
/// `cratonvm_gc_core::reservation::map_side_table`). Linux only; a bitmap under
/// `heap_bitmap::SIDE_TABLE_MAP_MIN_BYTES` (a 64 MiB arena) stays where it
/// was. Default ON, `=0` kill. Engagement: `side_maps=` on
/// `[GC] zgc-footprint-spans:`.
pub fn zgc_side_table_no_thp_enabled() -> bool {
    true
}

/// zr z7/rs: what `CRATONVM_ZGC_ARENA_NO_THP` did to this heap's arena, for
/// the `[GC] zgc-footprint-spans:` line (`arena_no_thp=`).
pub const ZGC_NO_THP_UNSET: usize = 0;
pub const ZGC_NO_THP_APPLIED: usize = 1;
pub const ZGC_NO_THP_REFUSED: usize = 2;
pub const ZGC_NO_THP_YIELDED: usize = 3;

pub fn zgc_pristine_chunks_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// Mark every candidate span that `[now, end)` overlaps.
///
/// `spans` are `(lo, hi, blocked)`, ascending and disjoint (so `hi` is
/// ascending too). A span is marked when `now < hi && end > lo` -- exactly the
/// "something starting below `hi` reaches past `lo`" question the old
/// materialised screen asked with a prefix maximum over every survivor's
/// extent. One binary search, then only the spans actually overlapped.
pub fn block_overlapping_spans(spans: &mut [(usize, usize, bool)], now: usize, end: usize) {
    let mut i = spans.partition_point(|s| s.1 <= now);
    while i < spans.len() && spans[i].0 < end {
        spans[i].2 = true;
        i += 1;
    }
}

/// `CRATONVM_ZGC_SLIDE_EVACUATE` (zs s2/sl2): the low slide evacuates the
/// selected pages' survivors into the bump tail and frees the pages whole,
/// instead of sliding them down in place. Default OFF (opt-in `=1`). Read once
/// per slide. See the arm in `relocate_stw_admitted_hinted`.
pub fn zgc_slide_evacuate_enabled() -> bool {
    false
}

/// The evacuating arm's gate (`CRATONVM_ZGC_SLIDE_EVACUATE`, the region
/// relocation): the tail must take the whole set (headroom at least the
/// selected live bytes plus an eighth plus one large-object minimum), never
/// after an allocation failure, never on a pressure tier, only with the
/// vacated-span publication on. Read twice per slide (zt t35/zrr): by the
/// no-move decline, which must not skip a cycle the arm will refuse, and by
/// the arm itself.
///
/// zt t3/rl (zs3rv2 §6): two more refusals, both of which send the cycle to
/// the slide instead.
/// * `CRATONVM_ZGC_SLIDE_UNSIZABLE_OBSTACLE=0`: an unsizable UNSELECTED
///   survivor is then no obstacle to the partition, but the screen still fails
///   closed at it, so the blocked-span free could release old extents the
///   screen kept. The evacuation needs the obstacle rule.
/// * a to-space window already open (a panicked slide under
///   `CRATONVM_ZGC_SLIDE_PANIC_ABORT=0` left it): the open would be refused and
///   the arm would move nothing, on every later cycle, while the slide was
///   filtered out.
pub fn slide_evacuate_gate_admits(
    selected_live: usize,
    cycle_followed_failure: bool,
    no_pressure_tier: bool,
    publish_vacated: bool,
    bump_headroom: usize,
    to_space_open: bool,
) -> bool {
    let need = selected_live
        .saturating_add(selected_live / 8)
        .saturating_add(ZGC_LARGE_OBJECT_MIN);
    !cycle_followed_failure
        && no_pressure_tier
        && publish_vacated
        && bump_headroom >= need
        && zgc_slide_unsizable_obstacle_enabled()
        && !to_space_open
}

/// `CRATONVM_ZGC_EVAC_QUARANTINE` (zs s3/cr): the evacuating slide withholds
/// the spans it vacated from the free list until the next collection. Default
/// OFF (opt-in `=1`), and inert without `CRATONVM_ZGC_SLIDE_EVACUATE=1`. Read
/// once per slide.
pub fn zgc_evac_quarantine_enabled() -> bool {
    false
}

/// zt t1/rl: does a pin in ascending `pins` lie in the extent `[from, from +
/// size)`, or at `from + size` when `end_inclusive` (the one-past-the-end
/// rule)? The smallest pin at or above `from` decides. The high slide's pin
/// test; pure, for the tests.
pub fn sorted_pin_in_extent(pins: &[usize], from: usize, size: usize, end_inclusive: bool) -> bool {
    let end = from + size;
    pins.get(pins.partition_point(|&p| p < from))
        .is_some_and(|&p| p < end || (end_inclusive && p == end))
}



/// `CRATONVM_ZGC_UNCOMMITTER_WINDOWS_PAUSE_ONLY` (zt t1/rl): opt-in `=1`. On
/// Windows, `start_uncommitter` starts no thread, so memory goes back only at
/// a collection's give-back, with the world stopped (option 1 of
/// `zgc-zr5rv5-uncommitter-resets-beside-mutators-on-windows`). Read once per
/// heap start.
pub fn zgc_uncommitter_windows_pause_only_enabled() -> bool {
    false
}

/// Does `start_uncommitter` refuse for the platform? Only on Windows under
/// the switch. Pure, for the tests.
pub fn zgc_uncommitter_refused_on_this_platform(windows: bool, pause_only: bool) -> bool {
    windows && pause_only
}



/// `CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK` (zt t1/rl, debug; zs3cr item 6):
/// `release_evac_quarantine` first walks every registered object's reference
/// slots and counts the words that still name a held span (`qdead=` on
/// `[GC] zgc-relocate:`). O(registered slots) per release; inert without
/// `CRATONVM_ZGC_EVAC_QUARANTINE=1`. Read once per release.
pub fn zgc_dbg_quarantine_dead_check_enabled() -> bool {
    false}

/// `CRATONVM_DBG_ZGC_SOFT_ROWS` (zt t31/zc11, debug): after a concurrent
/// cycle's closing drain, name every soft row the mark reached whose referent
/// it did not (`ZgcRealHeap::dbg_soft_rows_after_drain`). Read once per
/// closing pause.
pub fn zgc_dbg_soft_rows_enabled() -> bool {
    false}

/// `CRATONVM_DBG_ZGC_EVAC_BLOCK_SPANS` (zs s3/cr, debug): every span the
/// evacuating slide would free is treated as blocked by the screen, so the
/// per-object free of moved survivors' old extents runs. The positive control
/// of that net. Read once per evacuating slide.
pub fn zgc_dbg_evac_block_spans_enabled() -> bool {
    false}

/// zs s3/cr: `[lo, hi)` minus `holes` (absolute, ascending, disjoint), as
/// ascending disjoint pieces. Pure, for the tests.
pub fn subtract_sorted_spans(lo: usize, hi: usize, holes: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut cur = lo;
    let mut j = holes.partition_point(|h| h.1 <= lo);
    while j < holes.len() && holes[j].0 < hi {
        if holes[j].0 > cur {
            out.push((cur, holes[j].0));
        }
        cur = cur.max(holes[j].1);
        j += 1;
    }
    if cur < hi {
        out.push((cur, hi));
    }
    out
}

/// zs s2/sl2: the spans an evacuating low slide may free, as unscreened
/// `(lo, hi, false)` triples, ascending and disjoint.
///
/// Each page of `pages` (ascending ids) contributes `[page_lo, min(page_hi,
/// low_end))` minus the union of `blockers` (absolute extents that stay:
/// straddlers from unselected pages, survivors the evacuation left; any
/// order, may overlap). Spans that touch are merged, then each is widened
/// over every free hole in `holes` (absolute, ascending, disjoint) that
/// overlaps or touches it: those bytes are dead already, and
/// `Arena::compact_low_to` drops a kept block that overlaps a published span,
/// so a hole only half inside one would lose its other half. A widened span
/// never crosses a blocker (a hole holds nothing live). Pure, for the tests.
pub fn evac_vacate_spans(
    pages: &[u64],
    base: usize,
    page_bytes: usize,
    low_end: usize,
    blockers: &[(usize, usize)],
    holes: &[(usize, usize)],
) -> Vec<(usize, usize, bool)> {
    // The blockers as a disjoint union.
    let mut bl: Vec<(usize, usize)> = blockers.iter().copied().filter(|(s, e)| e > s).collect();
    bl.sort_unstable();
    let mut union: Vec<(usize, usize)> = Vec::with_capacity(bl.len());
    for (s, e) in bl {
        match union.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => union.push((s, e)),
        }
    }
    let mut raw: Vec<(usize, usize)> = Vec::new();
    for &pg in pages {
        let lo = base + pg as usize * page_bytes;
        let hi = (lo + page_bytes).min(low_end);
        if hi <= lo {
            continue;
        }
        let mut cur = lo;
        let mut i = union.partition_point(|u| u.1 <= lo);
        while i < union.len() && union[i].0 < hi {
            if union[i].0 > cur {
                raw.push((cur, union[i].0));
            }
            cur = cur.max(union[i].1);
            i += 1;
        }
        if cur < hi {
            raw.push((cur, hi));
        }
    }
    // Widen over the holes that overlap or touch each span, then merge.
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(raw.len());
    for (mut s, mut e) in raw {
        let mut j = holes.partition_point(|h| h.1 < s);
        while j < holes.len() && holes[j].0 <= e {
            s = s.min(holes[j].0);
            e = e.max(holes[j].1);
            j += 1;
        }
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out.into_iter().map(|(s, e)| (s, e, false)).collect()
}


pub fn zgc_high_compaction_requested_by_default() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

pub const ZGC_TLAB_MAX_CHUNK: usize = 512 * 1024;

/// Size at or above which an allocation is served from the arena's HIGH end
/// (`Arena::alloc_high`) instead of being bump-allocated among TLAB chunks.
///
/// 64 KiB, and the number is derived rather than picked: it is
/// `ZGC_TLAB_MAX_CHUNK / 8`, which is exactly `ZTlabConfig::max_tlab_alloc` —
/// the size above which no TLAB will ever serve an object. So the rule is not
/// "big objects go up there", it is **"an object that can only come from the
/// shared arena must not be placed among the thread-private chunks"**, and the
/// threshold is the definition of that set rather than a tuning knob.
///
/// # Why the two populations must not share space
///
/// This heap does not compact, so the largest servable request is the largest
/// gap between two survivors. Measured on `TestNonBlockingAPI` under ZGC
/// (2026-08-13, `-Xmx 2g`): a 2,101,264-byte `char[]` raised
/// `OutOfMemoryError` with **1.99 GB of the 2 GB heap free**, because the
/// largest hole was 524,192 bytes — one `ZGC_TLAB_MAX_CHUNK` minus 96. The
/// fragmentation report named the walls exactly: **544 live bytes in four
/// runs — four `AbstractQueuedSynchronizer$ConditionNode`s and two
/// `ExclusiveNode`s — standing inside 2,621,264 bytes of otherwise contiguous
/// arena**. A thread that parks leaves one small, long-lived AQS node inside
/// its own 512 KiB private chunk; one survivor per chunk caps every hole in
/// the heap at one chunk, for the rest of the process.
///
/// Raising the chunk size only relocates that ceiling — it had already been
/// raised once (64 KiB -> 512 KiB) when a 65,552-byte `DFAState[8192]` hit the
/// same wall in Hibernate's `sql.exec.SmokeTests`. Splitting the populations
/// removes it: at the high end a large object's only possible neighbours are
/// other large objects, which are rarer by orders of magnitude, so the holes
/// they leave stay large.
pub const ZGC_LARGE_OBJECT_MIN: usize = ZGC_TLAB_MAX_CHUNK / 8;

/// The size of a RECYCLED TLAB chunk worth taking, given the full chunk size
/// `want`, the request `need` that forced this refill, and the largest block on
/// the low end's free list.
///
/// `None` means "ask for a full chunk" — either the free list has nothing worth
/// having, or it already has a full-size block, in which case the ordinary
/// `alloc(want)` will find it and there is nothing to decide.
///
/// Split out of [`ZgcRealHeap::tlab_refill`] so the decision can be tested
/// against the exact numbers that produced the defect, which is not reachable
/// through the heap's public API: it takes ~4,000 threads and a full 2 GB arena
/// to reproduce the shape, and the shape is one comparison.
///
/// The PREFERRED floor is `want / 8`, which is `ZTlabConfig::max_tlab_alloc` —
/// the bound that decides what a TLAB will serve at all. A chunk at that floor
/// still holds at least eight of the largest object it can ever be asked for.
/// Below it a buffer is churning rather than buffering.
///
/// # …and a STARVED floor of `want / 64`, because the alternative changes
///
/// "Is a short chunk worth taking?" has two answers and they depend on what
/// happens if it is refused. With bump headroom left, the refusal costs one
/// clean full-size bump and the short chunk is pure churn. With none, the
/// refusal walks straight down `Arena::alloc` — free list, un-reserved bump,
/// merge — to the arm that **eats the large-object reserve rather than fail**.
/// The chunk is taken either way; the only question is whether it comes out of
/// recycled space or out of the space the large-object end was promised.
///
/// MEASURED on `org.h2.test.store.TestMVStoreTool` at `--Xmx 1g`, 2026-08-29,
/// at the failing 262 160-byte request:
///
/// ```text
/// used=1073540152 capacity=1073741824 free_list_bytes=757199136
/// largest_free_block=102128 high_cursor=1069219280 high_bytes=102128
/// high_reserve_unclaimed=129695184
/// span_hist=8:72610 16:28157 32:12819 … 1K:45234 2K:15565 8K:65060 32K:53 64K:1
/// ```
///
/// 757 MB free and the largest LOW block under 64 KiB — one notch below a
/// `want / 8` floor of exactly 64 KiB, with 65 060 spans of 8–16 KiB sitting
/// unusable beneath it. Every refill in the process therefore bumped, the
/// cursor reached capacity, the 128 MiB reserve was spent on TLAB churn, and a
/// 262 160-byte request that the reserve existed to serve had nowhere to go.
///
/// `need` is still the hard bound in both regimes — a chunk that cannot serve
/// the request that forced the refill is not a chunk — so the starved floor
/// cannot produce a refill that immediately fails.
#[inline]
pub fn recycled_chunk_size(
    want: usize,
    need: usize,
    largest_low_free: usize,
    bump_headroom: usize,
    publish_vacated: bool,
    // Is there a free block of at least `ZGC_LARGE_OBJECT_MIN` BESIDES the one
    // `largest_low_free` names? See the starved rung below.
    direct_reserve_spare: bool,
) -> Option<usize> {
    let size = largest_low_free & !(ZGC_TLAB_ALIGN - 1);
    if size >= want || size < need {
        return None;
    }
    if size >= (want / 8).max(need) {
        return Some(size);
    }
    // Below the preferred floor: worth it only when a full chunk can no longer
    // be bumped without spending the large-object reserve, AND only while the
    // shared arena can still serve a direct allocation without this block.
    //
    // THE SECOND CONDITION IS THE ONE THAT WAS MISSING, AND IT COST A WORKLOAD.
    //
    // This rung takes `largest_low_free` -- the arena's LARGEST low block --
    // for a THREAD-PRIVATE buffer. Once the bump is gone every refill in the
    // process qualifies, so the large end of the free list is consumed
    // continuously and, on a heap whose live objects wall every span, nothing
    // replenishes it. `largest_free_block` then walks down to the rung's own
    // floor and stays there.
    //
    // Measured on
    // `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`
    // at `--Xmx 1500m` (2026-08-30). Four `OutOfMemoryError`s on a **16 400**
    // byte request -- a `char[8192]`, TLAB-eligible, so it reached the arena
    // only because its own buffer could not serve it -- with 905 MB free and
    // `largest_free_block=14232..16224`, i.e. sitting inside the 8-64 KiB band
    // this rung had been grinding. The same class, same binary:
    //
    // ```text
    //   default                                4x OOM, no @@RESULT
    //   CRATONVM_ZGC_RELOCATE=0                4x OOM, largest_free_block identical
    //   CRATONVM_ZGC_TARGETED_COMPACTION=1     4x OOM
    //   CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0    ok=132/132
    //   CRATONVM_ZGC_TLAB=0                    ok=132/132
    //   --Xmx 3000m                            ok=132/132
    // ```
    //
    // The reserve is DERIVED, not tuned, and the derivation matters: it is
    // `max_tlab_alloc`, the largest object a TLAB will ever serve, and
    // therefore the largest allocation that can fall back to a DIRECT arena
    // request when its own buffer cannot take it. That is precisely the
    // request this rung competes with.
    //
    // `ZGC_LARGE_OBJECT_MIN` was the first cut and is wrong: the starved
    // regime is by definition `largest_low_free < want / 8`, which is that
    // constant, so a spare block that big can never exist while the rung is
    // being asked and the gate degenerates into a kill switch. Measured --
    // it took `TestKillProcessWhileWriting` from PASS to FAIL, which is what
    // `CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` does.
    //
    // `direct_reserve_spare` is true when a block that big exists BESIDES the
    // one on offer, which is the question `has_free_block_at_least` cannot
    // answer for a caller about to consume the largest.
    //
    // The preferred rung above is untouched: at `want / 8` or better the block
    // is a retired chunk coming back one survivor short, which is the shape
    // this whole function was written for.
    (starved_recycle_permitted(publish_vacated)
        && bump_headroom < want
        && direct_reserve_spare
        && size >= (want / 64).max(need))
    .then_some(size)
}

/// Is the starved floor allowed to fire at all?
///
/// **The two switches are not independent, and the measurement that says so is
/// on the page this work belongs to.** `TestMVStoreTool` at `--Xmx 1g`, one
/// binary, arms interleaved: with `CRATONVM_ZGC_PUBLISH_VACATED=0` alone the
/// class fails 2/2 (`oom=6` at 60 s and 81 s, on a **9 888-byte** request with
/// 797 MB free and `largest_free_block=8184`), while the arm with that switch
/// off AND this one off passes 2/2.
///
/// The floor takes 8–64 KiB blocks off the free list when the bump is out of
/// headroom. While the slide is publishing page-granular spans back, that is
/// recycling. With the publication off, nothing replenishes the large end of
/// the free list and the floor grinds the last of it into TLAB chunks — a
/// bisect arm strictly worse than either endpoint, which is a footgun rather
/// than a lever.
///
/// So the floor is gated on the publication. Both default ON, so the shipped
/// configuration is byte-for-byte the one that was measured; this only affects
/// somebody turning the publication off to bisect.
pub fn starved_recycle_permitted(publish_vacated: bool) -> bool {
    publish_vacated && starved_recycle_enabled()
}

/// `CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` — the kill switch for the starved
/// floor in [`recycled_chunk_size`]. Default ON.
///
/// It exists so the change is a RE-RUN and not a rebuild: with it off, the
/// refill floor is `want / 8` in every regime, byte for byte what it was before
/// 2026-08-29. `CRATONVM_ZGC_TLAB=0` is not a substitute — it turns the whole
/// thread-local buffer off and changes the allocation path, the free-list
/// shape and the run time (561 s against 103 s on `TestMVStoreTool`), so an arm
/// that differs by it differs by far more than this decision.
pub fn starved_recycle_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        match None::<std::ffi::OsString> {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    })
}

/// Share of the arena that TLAB chunks may hold in RESERVATION at one time.
///
/// A chunk is not allocated memory, it is *claimed* memory: the part of it not
/// yet handed to an object belongs to no object and to no free list, and no
/// collection can reclaim it while its owning thread is alive. The chunk size
/// therefore has to be a function of how many threads are claiming one, and
/// until this constant existed it was not — it was `capacity / 1024` clamped to
/// [`ZGC_TLAB_MAX_CHUNK`], i.e. a flat 512 KiB on any heap above 512 MiB
/// however many threads the workload ran.
///
/// Measured on Tomcat's `TestNonBlockingAPI` (2026-08-13, `-Xmx 2g`): the class
/// runs ~4,000 threads, and `4,000 x 512 KiB` is **2 GB — the entire heap**.
/// The arena filled with chunk reservations, the un-bumped middle closed, and
/// a 2 MB `char[]` had nowhere left to go while 1.99 GB sat on the free list.
/// No collection could help: the chunks were legitimately claimed by live
/// threads.
///
/// `capacity / 16` (134 MiB at `-Xmx 2g`) divided by the live buffer count is
/// the budget each thread gets, clamped into
/// `[min_tlab_size(), ZGC_TLAB_MAX_CHUNK]`. The clamp is what keeps ordinary
/// workloads unchanged: below 256 threads the per-thread share still exceeds
/// 512 KiB at `-Xmx 2g`, so the chunk is the same 512 KiB it was.
pub const ZGC_TLAB_RESERVATION_SHARE: usize = 16;

/// What one [`ZgcRealHeap::mark_closure_serial`] call did. zr z1/mk.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZSerialMarkStats {
    /// Candidate addresses refused as not a registered base.
    pub wild_skipped: usize,
    /// Objects popped and traced (claim-on-push arm only; 0 on the `=0` arm).
    pub objects: usize,
    /// Reference-array continuations resumed (claim-on-push arm only). Zero
    /// on a heap with no reference array longer than
    /// `mark::ref_chunk_budget()`: read it as "nothing needed splitting".
    pub chunks: usize,
    /// Capacity the claim-on-push work stack grew to, in entries (0 on the
    /// `=0` arm, whose stack is the caller's).
    pub stack_peak: usize,
    /// zr z3/mt: objects whose oop-map came from the one-entry memo
    /// (`CRATONVM_ZGC_MARK_LAYOUT_MEMO`), and lookups that refilled it. Both 0
    /// with the memo off, and on a heap with no compact objects.
    pub memo_hits: usize,
    pub memo_misses: usize,
    /// zr z3/mt: edges dropped by the mark-bit screen before the registry
    /// screen (`CRATONVM_ZGC_MARK_TEST_FIRST`); 0 with it off.
    pub mark_first_skips: usize,
    /// zr zs s1/mc: thread CPU nanoseconds of the call
    /// (`CRATONVM_ZGC_MARK_CPU_ACCOUNTING`; 0 with it off or with no thread
    /// clock).
    pub cpu_ns: u64,
}

/// `CRATONVM_ZGC_MARK_CLAIM_ON_PUSH` -- the serial stop-the-world marker
/// claims a child when it is PUSHED and chunks long reference arrays (zr
/// z1/mk). **Default on**; `0` / `off` / `false` / `no` restores the pop-time
/// loop. Read on every call, like `CRATONVM_ZGC_MARK_POOL_PERSISTENT`, so an
/// in-process A/B is a re-run. See [`ZgcRealHeap::mark_closure_serial`].
pub fn zgc_mark_claim_on_push() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// `CRATONVM_ZGC_MARK_INLINE_DRIVER` -- the parallel stop-the-world mark runs
/// its `zgc_concurrent` driver on the collecting thread rather than on a
/// thread spawned and joined per cycle (zr z1/mk). **Default on**; `0` / `off`
/// / `false` / `no` restores the spawn. Inert unless `CRATONVM_ZGC_PARMARK`
/// selects the driven path. Read per cycle.
pub fn zgc_mark_inline_driver() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// `CRATONVM_ZGC_CONC_OVERLAY_REPAIR` -- at a concurrent cycle's mark end,
/// re-read the collection-overlay rows mutators wrote during the phase and
/// push their references (zr z1/mk; see
/// [`ZgcRealHeap::overlay_rows_written_during_mark`]). **Default on**; inert
/// unless `CRATONVM_ZGC_CONC_START` opens a cycle and a provider has rows. `0`
/// / `off` / `false` / `no` turns it off. Read per cycle.
/// `CRATONVM_ZGC_SATB_THREAD_BUFFERS` -- the concurrent mark's SATB pre-write
/// barrier records into a per-thread buffer and publishes to the ingress once
/// per [`mark::Z_MARK_MUTATOR_BUFFER_CAPACITY`] stores (zr z2/cc; step 2.2 of
/// `proposal-a-concurrent-mark-by-default.md`; see
/// [`mark::ZSatbThreadBuffers`]). **Default on**; inert unless a concurrent
/// cycle arms the barrier. `0` restores one ingress-bucket push per store.
/// Latched: read on the barrier's slow path.
/// `CRATONVM_ZGC_CONC_START_NEAREST` -- the concurrent-start window ends at the
/// nearest predictable collection point (warm-up, allocation budget, threshold)
/// rather than at `gc_threshold` alone (zr z2/cc; see
/// [`ZgcRealHeap::conc_start_window`]). **Default on**; inert unless
/// `CRATONVM_ZGC_CONC_START` configures concurrent marking. `0` restores the
/// threshold-only window. Latched (read on the allocation poll).
pub fn zgc_conc_start_nearest() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        match None::<std::ffi::OsString> {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    })
}

/// `CRATONVM_ZGC_CONC_EVERY_TRIGGER` (zr z3/cs, **opt-in**): every non-urgent
/// `needs_gc` yes (warm-up, allocation budget, live-bytes threshold, headroom,
/// the director's rules) first opens a concurrent mark when the heap has the
/// headroom to finish one, and the collection is taken when the mark reaches
/// its fixed point -- HotSpot ZGC's model (`Warmup`, `Allocation Rate`,
/// `Proactive`, `High Usage` start concurrent cycles; a stop-the-world finish
/// happens only when allocation runs out). Configures concurrent marking by
/// itself (no `CRATONVM_ZGC_CONC_START` needed). Inert in ZGC's generational
/// mode. Read once per heap at construction. See [`ZConcTriggerState`].
///
/// zr z7b/cm: with the variable UNSET, an explicit `CRATONVM_ZGC_CONC_START`
/// decides -- `0` is the stop-the-world kill, a percentage or `auto` selects
/// the occupancy-window mode -- and only with neither set does
/// [`Z_CONC_EVERY_TRIGGER_DEFAULT`] apply. So `CRATONVM_ZGC_CONC_START=0` (or
/// `CRATONVM_ZGC_CONC_EVERY_TRIGGER=0`) is the kill of a default-on mode.
///
/// zt t18/zc1: the concurrent profile (`CRATONVM_ZGC_CONCURRENT=1`,
/// [`zgc_concurrent_profile_enabled`]) is a third way to turn it on, under the
/// same precedence: an explicit `CRATONVM_ZGC_CONC_EVERY_TRIGGER` or
/// `CRATONVM_ZGC_CONC_START` still decides.
pub fn zgc_conc_every_trigger_enabled() -> bool {
    match None::<String> {
        Some(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => {
            if None::<std::ffi::OsString>.is_some() {
                false
            } else {
                Z_CONC_EVERY_TRIGGER_DEFAULT || zgc_concurrent_profile_enabled()
            }
        }
    }
}

/// Whether a ZGC heap with neither `CRATONVM_ZGC_CONC_EVERY_TRIGGER` nor
/// `CRATONVM_ZGC_CONC_START` set runs HotSpot's trigger model (every trigger
/// opens a concurrent mark; the collection follows its fixed point).
pub const Z_CONC_EVERY_TRIGGER_DEFAULT: bool = false;





/// `CRATONVM_ZGC_SLIDE_TAIL_ZERO_AT_ALLOCATION` (zt t31/zc11, default ON,
/// `=0` kill; the concurrent-collector program page's item carried in from the
/// r21 cost page): a slide leaves the tail above its new cursor unzeroed and
/// the hand-out that next takes those bytes zeroes them, as HotSpot zeroes at
/// allocation (`Arena::compact_low_to`'s note says why that is sound). `=0`:
/// the slide zeroes the tail inside the pause, as before. Read once per heap,
/// at construction.
pub fn zgc_slide_tail_zero_at_allocation_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_CONCURRENT` (zt t18/zc1, **opt-in**, `=1`): the concurrent
/// ZGC profile of `docs/feature-designs/zgc-architecture.md`,
/// stage 1. One switch for every stage-1 piece that is built, so a row can
/// measure the profile as a whole against the default and against HotSpot:
///
/// * every trigger opens a concurrent mark (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`,
///   [`zgc_conc_every_trigger_enabled`]);
/// * a `System.gc()` / `jcmd GC.run` request runs a concurrent cycle that
///   starts after the call and waits for it
///   (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`, [`zgc_system_gc_concurrent_enabled`]);
/// * the sweep leaves the pause and runs after the release (zt t21/zc2,
///   `CRATONVM_ZGC_CONCURRENT_SWEEP`, `concurrent_sweep.rs`);
/// * the referent slot keeps its referent through the collection, hidden from
///   the markers and cleared in the pause when the referent dies (zt t22/zc3,
///   `CRATONVM_ZGC_CONCURRENT_REFS`, `concurrent_refs.rs`);
/// * the director tick closes an every-trigger cycle at its fixed point, so
///   a program that stops allocating does not keep the SATB barrier armed
///   (zt t23/zc6, `CRATONVM_ZGC_MARK_END_SERVICE`, `mark_end.rs`);
/// * a closing allocation-door collection first tries a Pause Mark End and
///   releases the world when mark work is left, the pool continuing (zt
///   t24/zc8, `CRATONVM_ZGC_MARK_CONTINUE`, `mark_end.rs`);
/// * the every-trigger cycle is asked for at HotSpot's allocation-rate point,
///   while the heap can still finish a mark, instead of at a clause that fires
///   too late for one (zt t26/zc9, `CRATONVM_ZGC_CONCURRENT_RATE_START`,
///   `rate_start.rs`).
/// * on a cycle that defers its sweep, the VM reference round runs after the
///   release, before the sweep's install (zt t31/zc11,
///   `CRATONVM_ZGC_CONCURRENT_REF_ROUND`, `concurrent_ref_round.rs`).
///
/// Each piece keeps its own switch, and an explicit value of it wins over the
/// profile (`=0` is that piece's kill inside the profile). Read at heap
/// construction, by the piece readers. Unset, empty or an off word: off.
pub fn zgc_concurrent_profile_enabled() -> bool {
    match None::<String> {
        Some(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => zgc_concurrent_profile_default(),
    }
}

/// The profile's default: ON (the flip of this change; `=0` kills it).
pub const ZGC_CONCURRENT_PROFILE_DEFAULT_ON: bool = true;

/// [`ZGC_CONCURRENT_PROFILE_DEFAULT_ON`], except in this crate's own unit
/// tests: the ~2000 of them pin the classic stop-the-world engine (slide,
/// trigger, warm-up) on heaps built with the environment unset, and a test of
/// the profile sets `CRATONVM_ZGC_CONCURRENT=1` itself. The VM, the CLI and
/// every other crate's tests link this crate without `cfg(test)`, so they run
/// the default.
pub fn zgc_concurrent_profile_default() -> bool {
    ZGC_CONCURRENT_PROFILE_DEFAULT_ON && !cfg!(test)
}

/// `CRATONVM_ZGC_CONCURRENT_RELOCATE` (opt-in, `=1`; needs the concurrent
/// profile; default ON 2026-11-10, back to opt-in 2026-11-11: armed, the
/// colored load barrier cost 1.5-4x on the JDK string/regex rows and 30-40%
/// mutator throughput on `ZgcRelocHiccup` without bringing pauses near
/// HotSpot's -- WARNING-zgc-concurrent-relocate-opt-in): the concurrent-relocation stack as one switch -- the
/// colored load barrier, the color flip, region relocation, the kept cycle's
/// no-hand-over quarantine, remap-in-mark and R6's relocate-after-release
/// ([`zgc_reloc_stack_flag`]). Each piece keeps its own switch, and an
/// explicit value of it wins over this profile.
pub fn zgc_concurrent_relocation_profile_enabled() -> bool {
    // A generational heap refuses concurrent relocation (`ZConcRelocRefusal::
    // Generational`), so the implicit default does not arm the barrier there.
    zgc_concurrent_profile_enabled()
        && !zgc_generational_enabled()
        && false
}

/// One piece of the concurrent-relocation stack: its own switch when set,
/// else [`zgc_concurrent_relocation_profile_enabled`].
pub fn zgc_reloc_stack_flag(key: &str) -> bool {
    match cratonvm_types::flags::runtime_var(key) {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => zgc_concurrent_relocation_profile_enabled(),
    }
}

/// `CRATONVM_ZGC_SYSTEM_GC_CONCURRENT` (zt t18/zc1, **opt-in**; on under the
/// concurrent profile, `=0` kills it there): a full request on the
/// `System.gc()` / `jcmd GC.run` doors opens a concurrent mark that starts
/// AFTER the call, waits (taking part in safepoints) for its fixed point, and
/// then runs the requested collection, which closes that cycle with its mark
/// set instead of abandoning it (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`).
/// HotSpot 25's ZGC has no stop-the-world collection: `System.gc()` is a
/// synchronous request for one whole concurrent cycle (`ZDriverPort::send_sync`),
/// so its pauses are Pause Mark Start / Pause Mark End / Pause Relocate Start
/// and the caller waits for the cycle. Needs the every-trigger mode on (the
/// request rides its `requested_at`); inert otherwise and in generational
/// mode. Read once per heap, at construction ([`ZConcTriggerState::new`]).
/// zt t32 (2f, `CRATONVM_ZGC_LOAD_BARRIER_MARKING`; opt-in): mark through the
/// load barrier -- the store side logs only the deletion of a plain word. Armed
/// with the color flip (`arm_load_barrier_color_flip`); nothing without it.
pub fn zgc_load_barrier_marking_requested() -> bool {
    false
}

pub fn zgc_system_gc_concurrent_enabled() -> bool {
    match None::<String> {
        Some(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => zgc_concurrent_profile_enabled(),
    }
}

/// zt t18/zc1: how long the `System.gc()` caller waits for its concurrent
/// cycle's mark to reach a fixed point before it runs the closing collection
/// anyway (which then finishes the mark inside its pause, as every collection
/// that closes an unfinished cycle does). A bound, not a target: the wait
/// normally ends at the fixed point. Generous, because a pause that finishes a
/// large mark is exactly what the concurrent cycle is for avoiding.
pub const ZGC_SYSTEM_GC_CONC_WAIT_MS: u64 = 10_000;

/// zt t18/zc1: how long the cycle a `System.gc()` request opened keeps its
/// mark end for that request ([`ZgcRealHeap::system_gc_holds_mark_end`]):
/// twice the caller's wait, so a caller that is slow to come back from its
/// wait still finds its own cycle open, while a caller that never comes back
/// (it unwound) holds nothing for long.
pub const ZGC_SYSTEM_GC_CONC_HOLD_NANOS: u64 = 2 * ZGC_SYSTEM_GC_CONC_WAIT_MS * 1_000_000;

/// `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS` (zr z3/cs, **default on**, `=0`
/// kill): `-XX:MaxGCPauseMillis` does not reach ZGC's pause-target budget.
/// HotSpot's ZGC does not read the flag (a G1 / Parallel ergonomic), so a
/// program passing it collects exactly as without it; here it used to resize
/// `alloc_trigger_bytes` and therefore the collection frequency. The
/// CratonVM-only knob `CRATONVM_ZGC_PAUSE_TARGET_MS` (default 200) is
/// untouched. Page `zgc-zr1tr-maxgcpausemillis-sizes-zgc-collection-frequency-CLOSED-20261019.md`.
pub fn zgc_ignore_max_gc_pause_millis() -> bool {
    true
}

/// zr z3/cs: per-heap state of `CRATONVM_ZGC_CONC_EVERY_TRIGGER`.
///
/// # The life of one cycle under the switch
///
/// 1. An early `needs_gc` clause says yes. With the headroom to finish a mark
///    ([`director::conc_cycle_has_headroom`]) `needs_gc` answers NO and
///    records a request (`requested_at`); without it, YES (a stop-the-world
///    collection, `no_headroom`). The clause's `trigger_seen` bit is charged
///    as before, so the collection that ends the cycle carries its cause.
/// 2. The next poll site that can take a pause (`maybe_gc`, the TLAB refill
///    trigger, the JIT allocation helpers) sees
///    [`ZgcRealHeap::should_start_concurrent_mark`] true and runs the
///    mark-start pause; the VM reports the outcome
///    ([`ZgcRealHeap::note_conc_start_outcome`]: `opened`, or a refusal --
///    the take-over cover -- which lapses the request at once).
/// 3. While the cycle traces, `needs_gc` answers NO. It answers YES when the
///    marker reached its fixed point (`finished`), or when free space falls
///    to the urgent reserve (`abandoned_stw` + `urgent`); any other door
///    (allocation failure, `System.gc()`, a forced pause) closing the cycle
///    before the fixed point is `abandoned_stw` too.
/// 4. A request no site serves lapses after [`director::conc_request_grace`]
///    bytes, or at the urgent reserve (`lapsed`): the mode never defers a
///    collection without limit.
///
/// Soundness is the existing concurrent cycle's: the mode changes only WHEN a
/// cycle opens and when the collection that closes it is taken.
pub struct ZConcTriggerState {
    pub enabled: AtomicBool,
    /// `allocated` when the pending request was made (`0`: none).
    pub requested_at: AtomicUsize,
    /// The VM reported that the requested mark start did not open.
    pub refused: AtomicBool,
    /// The gate answered `UrgentWhileOpen` in this interval.
    pub urgent_seen: AtomicBool,
    /// `no_headroom` was charged in this interval.
    pub no_headroom_seen: AtomicBool,
    /// Mark start to this collection, nanoseconds, of the last cycle that
    /// opened (fed to the director as the concurrent cycle's duration).
    pub last_phase_nanos: AtomicU64,
    pub requests: AtomicUsize,
    pub opened: AtomicUsize,
    pub start_refusals: AtomicUsize,
    pub finished: AtomicUsize,
    pub abandoned: AtomicUsize,
    pub urgent: AtomicUsize,
    pub lapsed: AtomicUsize,
    pub no_headroom: AtomicUsize,
    /// zr zs s1/tp: mark-start pauses that found a cycle already open (the
    /// loser of the open race paid a safepoint for nothing). Counted in every
    /// concurrent mode, not only under the switch.
    pub lost_races: AtomicUsize,
    /// zr zs s1/tp: mark starts the VM dropped before requesting the pause
    /// because the start was no longer due (`CRATONVM_ZGC_CONC_START_RECHECK`).
    pub start_rechecks: AtomicUsize,
    /// zt t18/zc1: `CRATONVM_ZGC_SYSTEM_GC_CONCURRENT` on this heap
    /// ([`zgc_system_gc_concurrent_enabled`], read at construction).
    pub sysgc_enabled: AtomicBool,
    /// zt t18/zc1: the monotonic deadline (nanoseconds) until which the open
    /// cycle's mark end belongs to the `System.gc()` request that opened it
    /// (`0`: no such request). Set by
    /// [`ZgcRealHeap::request_system_gc_concurrent_cycle`] BEFORE the mark
    /// start, consumed by every collection
    /// ([`ZgcRealHeap::take_system_gc_cycle_owner`]).
    pub sysgc_owner_deadline: AtomicU64,
    /// zt t18/zc1 engagement, cumulative: requests that asked for a cycle,
    /// requests whose cycle opened, cycles the request's own collection closed
    /// with their mark set (`served`), owned cycles another door closed
    /// first (`lost`), requests whose mark start did not open (`not_opened`:
    /// another pause in flight, the take-over cover), and `needs_gc` polls that
    /// found the owned mark finished and held it for its owner (`held`).
    pub sysgc_requests: AtomicUsize,
    pub sysgc_opened: AtomicUsize,
    pub sysgc_served: AtomicUsize,
    pub sysgc_lost: AtomicUsize,
    pub sysgc_not_opened: AtomicUsize,
    pub sysgc_held: AtomicUsize,
}

/// zr z3/cs: the `[GC] zgc-conc-trigger:` summary figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZConcTriggerReport {
    pub enabled: bool,
    pub requests: usize,
    pub opened: usize,
    pub start_refusals: usize,
    pub finished: usize,
    pub abandoned: usize,
    pub urgent: usize,
    pub lapsed: usize,
    pub no_headroom: usize,
    /// zr zs s1/tp: see [`ZConcTriggerState`]'s fields of the same names.
    pub lost_races: usize,
    pub start_rechecks: usize,
    /// zt t18/zc1: `CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`; the `sysgc_*` fields
    /// of [`ZConcTriggerState`].
    pub sysgc_enabled: bool,
    pub sysgc_requests: usize,
    pub sysgc_opened: usize,
    pub sysgc_served: usize,
    pub sysgc_lost: usize,
    pub sysgc_not_opened: usize,
    pub sysgc_held: usize,
}

impl ZConcTriggerState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            requested_at: AtomicUsize::new(0),
            refused: AtomicBool::new(false),
            urgent_seen: AtomicBool::new(false),
            no_headroom_seen: AtomicBool::new(false),
            last_phase_nanos: AtomicU64::new(0),
            requests: AtomicUsize::new(0),
            opened: AtomicUsize::new(0),
            start_refusals: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            abandoned: AtomicUsize::new(0),
            urgent: AtomicUsize::new(0),
            lapsed: AtomicUsize::new(0),
            no_headroom: AtomicUsize::new(0),
            lost_races: AtomicUsize::new(0),
            start_rechecks: AtomicUsize::new(0),
            sysgc_enabled: AtomicBool::new(zgc_system_gc_concurrent_enabled()),
            sysgc_owner_deadline: AtomicU64::new(0),
            sysgc_requests: AtomicUsize::new(0),
            sysgc_opened: AtomicUsize::new(0),
            sysgc_served: AtomicUsize::new(0),
            sysgc_lost: AtomicUsize::new(0),
            sysgc_not_opened: AtomicUsize::new(0),
            sysgc_held: AtomicUsize::new(0),
        }
    }
}

