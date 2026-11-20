#[allow(unused_imports)]
use super::*;

impl G1Collector {
    /// g1pin: pin the objects blocked peers' captured stack words lie in
    /// ([`g1_pins_peer_stack_objects`]), publish them as this collection's
    /// object pins, and add to `set` the region of every word that cannot be
    /// resolved to an object start. Returns the words the above-cursor screen
    /// declined, as [`Self::jit_pinned_region_set_screened`] counts them.
    pub fn pin_peer_stack_objects(
        &self,
        regions: &[G1Region],
        skip_above_cursor: bool,
        set: &mut RegionSet,
    ) -> u64 {
        let (by_region, declined) = self.peer_words_by_region(regions, skip_above_cursor);
        let jit_skips = if by_region.is_empty() {
            Vec::new()
        } else {
            self.jit_tlab_skip_spans()
        };
        let mut pins = Vec::new();
        let mut n_words = 0usize;
        let mut fallbacks = [0usize; PeerWordFallback::COUNT];
        for (idx, words) in by_region {
            n_words += words.len();
            match self.peer_words_to_object_starts(regions, idx, &words, &jit_skips) {
                Ok(starts) => pins.extend(starts),
                Err(why) => {
                    fallbacks[why as usize] += 1;
                    set.insert(idx);
                }
            }
        }
        if gc_flags().g1_dbg_pins && n_words > 0 {
            eprintln!(
                "[g1][PEERPIN] words={n_words} objects={} region_pins(not_walkable/above_cursor/unparseable)={fallbacks:?}",
                pins.len()
            );
        }
        self.publish_peer_object_pins(pins);
        declined
    }

    /// g1pin: this pause's captured peer-stack words, sorted, by the region
    /// they lie in, and the count the above-cursor screen declined.
    pub fn peer_words_by_region(
        &self,
        regions: &[G1Region],
        skip_above_cursor: bool,
    ) -> (std::collections::BTreeMap<usize, Vec<usize>>, u64) {
        let mut by_region: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
        let mut declined = 0u64;
        let mut words = Vec::new();
        cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut words, |_| true);
        for addr in words {
            if skip_above_cursor && self.pin_word_above_cursor(regions, addr) {
                declined += 1;
            } else if let Some(idx) = self.lookup_region_for_addr(addr) {
                by_region.entry(idx).or_default().push(addr);
            }
        }
        for words in by_region.values_mut() {
            words.sort_unstable();
        }
        (by_region, declined)
    }

    /// g1pin: the start of the object each of `words` (sorted, all in region
    /// `idx`) lies in, by a walk of the region from its base to its cursor.
    /// The first word that cannot be placed answers why ([`PeerWordFallback`]),
    /// and the caller then pins the whole region.
    pub fn peer_words_to_object_starts(
        &self,
        regions: &[G1Region],
        idx: usize,
        words: &[usize],
        jit_skips: &[(usize, usize)],
    ) -> Result<Vec<usize>, PeerWordFallback> {
        let (base, cursor) = regions
            .get(idx)
            .and_then(peer_walkable_extent)
            .ok_or(PeerWordFallback::NotWalkable)?;
        let mut starts = Vec::with_capacity(words.len());
        let (mut next, mut offset) = (0usize, 0usize);
        while next < words.len() {
            if offset >= cursor {
                return Err(PeerWordFallback::AboveCursor);
            }
            let addr = base + offset;
            let (len, placeable) = peer_walk_step(addr, cursor - offset, jit_skips)
                .extent()
                .ok_or(PeerWordFallback::Unparseable)?;
            let named = words[next..].partition_point(|&w| w < addr + len);
            // A word in a filler or a frozen peer's TLAB tail names no object:
            // nothing there moves, so the pointer map has no key equal to it
            // and the wake stores nothing over it, and no object needs it to
            // stay alive. It pins nothing. (A frozen tail's region is pinned
            // by `jit_tlab_skip_regions` already.)
            if named > 0 && placeable {
                starts.push(addr);
            }
            next += named;
            offset += len;
        }
        Ok(starts)
    }

    /// g1pin: replace this collection's object pins with `pins`.
    pub fn publish_peer_object_pins(&self, mut pins: Vec<usize>) {
        pins.sort_unstable();
        pins.dedup();
        self.peer_object_pin_count.store(pins.len(), Ordering::Release);
        *self.peer_object_pins.write() = pins;
    }

    /// g1pin: is `addr` an object start this collection pinned in place
    /// ([`Self::pin_peer_stack_objects`])?
    #[inline]
    pub fn peer_object_pinned(&self, addr: usize) -> bool {
        self.peer_object_pin_count.load(Ordering::Acquire) != 0
            && self.peer_object_pins.read().binary_search(&addr).is_ok()
    }

    /// g1pin: this collection's object pins, sorted.
    pub fn peer_object_pins_snapshot(&self) -> Vec<usize> {
        if self.peer_object_pin_count.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        self.peer_object_pins.read().clone()
    }

    /// g1pin: drop the object pins (both ends of `collect_garbage`).
    pub fn clear_peer_object_pins(&self) {
        self.peer_object_pin_count.store(0, Ordering::Release);
        self.peer_object_pins.write().clear();
    }

    /// g1pin: the object pins inside `cset_set`, appended to a pause's
    /// keep-alive roots. A pinned object is live by assumption, as its region
    /// was under the region pin: the word naming it may be the only
    /// reference, and the evacuators keep it where it is
    /// ([`Self::peer_object_pinned`]) and scan it in place.
    pub fn append_peer_object_pin_roots(&self, roots: &mut Vec<usize>, cset_set: &RegionSet) {
        for addr in self.peer_object_pins_snapshot() {
            if self
                .lookup_region_for_addr(addr)
                .is_some_and(|idx| cset_set.contains(&idx))
            {
                roots.push(addr);
            }
        }
    }

    /// [`g1_peer_pins_skip_post_block_regions`]: remove this pause's blocked-peer
    /// captures whose word names a region handed out after the peer blocked.
    pub fn drop_post_block_peer_stack_words(&self, regions: &[G1Region]) {
        let mut stamps: std::collections::HashMap<u32, Option<u64>> =
            std::collections::HashMap::new();
        let dropped = cratonvm_gc_core::gc_quiescence::drop_peer_stack_slots_where(|tid, addr| {
            let Some(region) = self
                .lookup_region_for_addr(addr)
                .and_then(|idx| regions.get(idx))
            else {
                return false;
            };
            let block = *stamps
                .entry(tid)
                .or_insert_with(|| cratonvm_gc_core::gc_quiescence::block_stamp_of_tid(tid));
            peer_word_names_post_block_region(region.handout_stamp(), block)
        });
        if dropped > 0 && gc_flags().g1_dbg_pins {
            tracing::warn!("[g1][PEERPIN] post-block captures dropped={dropped}");
        }
    }

    /// gci i10/pp -- [`pin_word_names_no_allocation`] for `addr` against the
    /// region table. `false` for an address outside every region.
    pub fn pin_word_above_cursor(&self, regions: &[G1Region], addr: usize) -> bool {
        let Some(idx) = self.lookup_region_for_addr(addr) else {
            return false;
        };
        let Some(r) = regions.get(idx) else {
            return false;
        };
        pin_word_names_no_allocation(r.region_type(), r.data.addr(), r.cursor(), addr)    }

    /// The regions a live JIT frame forces out of the collection set.
    ///
    /// The VM's root gatherer publishes these addresses (only under G1) via
    /// [`cratonvm_gc_core::gc_quiescence::add_pinned_jit_root`]; a conservative JIT root
    /// lives in a register/spill slot the collector cannot rewrite, so its
    /// object must not move. Excluding its region from the CSet is G1's analog
    /// of the generational collector's non-moving-while-in-JIT sweep. That half
    /// contributes nothing unless a compiled frame is live, because
    /// `jit_frames_live_for_pins()` gates the snapshot read.
    ///
    /// INT-3 — the set ALSO includes every region holding a published
    /// un-retired TLAB tail (see [`Self::set_jit_tlab_skip_regions`]): the
    /// tail's owner resumes bump-allocating into `[cursor, end)` after the
    /// pause, so evacuating + freeing that region would hand the same memory
    /// out twice. That half is deliberately NOT gated on
    /// `gc_quiescence::is_active()` — a blocked thread's un-retired tail can be
    /// published while no thread is in JIT at all — so "no thread is in JIT"
    /// does NOT mean this returns empty.
    ///
    /// # The movable partition, which this used to ignore
    ///
    /// Not every conservative JIT root has to be pinned. A reference the shadow
    /// stack published is PRECISE and REWRITABLE -- `shadow_stack.remap`
    /// rewrites it after a move and the JIT's shadow reload
    /// (`emit_shadow_reload`, for a `ShadowHome::Reg` home) refreshes the
    /// register -- so its object may be evacuated like any other.
    ///
    /// gci i9/ng (i8/nt review item 2): this used to say the register is
    /// refreshed "from the (rewritten) frame slot". That is not true of every
    /// movable word. The register-image remap also rewrites the per-safepoint
    /// blind GPR spill image, and compiled code never loads that image back
    /// (`emit_pre_safepoint_spill`: no post-call reload), so a spill-image
    /// word is only safe to move when the shadow reload refreshes the live
    /// register. The publisher enforces that, not this filter:
    /// `conservative_roots::register_image_word_publishes_movable` publishes a
    /// spill-image word movable only with shadow emission on (and
    /// `CRATONVM_GC_SPILL_IMAGE_PINS` unset); otherwise it is an unrewritable
    /// veto and pins its region here. With shadow emission on, a register the
    /// shadow push did not publish is still refreshed by nothing (i8/nt
    /// item 5, the netty page's candidate). `gc_quiescence` carries that partition, and
    /// `gen_heap::collect_garbage_inner` has consulted it since the moving
    /// young generation shipped:
    ///
    /// ```text
    /// let movable = honour_movable
    ///     && is_movable_jit_root(a)
    ///     && !is_unrewritable_jit_root(a);
    /// if is_y(a) && !movable { pin_base_of(a, &mut pinned); }
    /// ```
    ///
    /// G1 applied NO such filter: every address in the snapshot pinned its
    /// region. On a region-granular collector that is the expensive way to be
    /// wrong -- a pinned region leaves the collection set WHOLESALE, so one
    /// rewritable reference costs a whole megabyte at `-Xmx2g`. On H2's
    /// `TestValueMemory` Type 3 it pinned 14 regions for 12474 KB from 38
    /// addresses of which only 10 were actually unrewritable, and G1 read
    /// ~11000 where the generational collector read 1149 and ZGC 1205 on the
    /// same row.
    ///
    /// The three conjuncts are the generational path's, unchanged and for its
    /// reasons: `honour_movable` is the whole-cycle proof that precise coverage
    /// held, `is_movable_jit_root` is the per-address claim that a rewritable
    /// channel names it, and `is_unrewritable_jit_root` is the VETO -- the pin
    /// set is keyed by OBJECT, so one rewritable channel naming an address must
    /// not license moving it out from under every other word that also holds
    /// it, such as a compiled frame's callee-saved register image, which
    /// `band_slot_is_verifiable` refuses to inspect and no channel rewrites.
    ///
    /// Gated on `CRATONVM_GC_G1_MOVABLE_PINS`, which defaults ON (`=0` turns
    /// the filter off) — see `g1_movable_pins_enabled` for the measurement
    /// that flipped it. (This said "gated OFF by default" until round 12.)
    ///
    /// The snapshot half is read when a compiled frame is live by either
    /// detector, the guarded depth or the root scan's unregistered-frame flag
    /// (`jit_frames_live_for_pins`, round 12 wave 1).
    #[cfg(test)]
    pub fn jit_pinned_region_set(&self) -> RegionSet {
        let regions = self.regions.read();
        self.jit_pinned_region_set_screened(None, &regions, true).0
    }

    /// [`Self::jit_pinned_region_set_screened`], optionally screened (gci i10/pp,
    /// `CRATONVM_G1_PIN_SKIP_ABOVE_CURSOR`): with `Some(regions)` a kept
    /// snapshot word that lies strictly above its region's cursor
    /// ([`pin_word_names_no_allocation`]) pins nothing, and the second value
    /// counts those words. The frozen-peer TLAB half is never screened: a
    /// tail's owner resumes allocating into it after the pause. `None` is the
    /// unscreened set, byte for byte the function this used to be.
    ///
    /// `regions` serves the blocked-peer post-block screen
    /// ([`g1_peer_pins_skip_post_block_regions`]), which reads hand-out stamps.
    /// `peer_words` adds the blocked peers' captured stack words
    /// ([`g1_pins_peer_stack_words`]); a caller that only FREES passes `false`
    /// ([`Self::cleanup_jit_pinned_regions`]).
    pub fn jit_pinned_region_set_screened(
        &self,
        above_cursor: Option<&[G1Region]>,
        regions: &[G1Region],
        peer_words: bool,
    ) -> (RegionSet, u64) {
        let mut declined = 0u64;
        // NOT gated on `moving_young_coverage_incomplete`, and the difference
        // from the generational path is the whole point.
        //
        // That flag is a WHOLE-CYCLE proof, and the generational collector needs
        // one because its question is "may I move the young generation at all"
        // -- one unproven frame and the entire cycle must fall back to the
        // non-moving sweep. G1's question is per-region, and the claim it rests
        // on is per-ADDRESS: a movable publication says this object's every band
        // sighting is either rewritten by `remap_one_jit_frame` or dead, and
        // that is true or false about one object regardless of what some other
        // frame could not prove about itself.
        //
        // Keeping the whole-cycle gate here was measured, and it made the filter
        // inert: `coverage_incomplete=true` on the very pause this exists for,
        // so `honour_movable=false` and all 38 pins survived a filter that had
        // nothing wrong with it.
        //
        // The fail-closed direction is preserved by the PUBLISHER, not by this
        // gate: `publish_unrewritable_band_roots` publishes movable only for
        // words it can argue about, and vetoes the address outright when any
        // unverifiable word also names it.
        let honour_movable = g1_movable_pins_enabled();
        let mut set: RegionSet = if jit_frames_live_for_pins() {
            // JIT round 21 lane parkcredit: the movable claim and its veto are
            // the CALLING thread's thread-locals (its own collection's band
            // scan), so they may unpin only the calling thread's own words.
            // A word another thread's deposit published, or a blocked peer's
            // helper window (its register file, its Rust frames), is in no
            // channel this collection rewrites -- a register is rewritten by
            // nobody -- and one oop-map slot of the initiator naming the same
            // object must not move it out from under that word. Same rule as
            // the generational old compaction's
            // (`pinned_jit_roots_snapshot_split`, gcg g3/e hunk H2).
            let (own, others) = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot_split();
            let snap_len = own.len() + others.len();
            let n_others = others.len();
            // `rescued=`: own words the claim drops whose address another
            // publisher also names -- the pins the old whole-snapshot filter
            // used to cancel. Diagnostic only.
            let rescued = if gc_flags().g1_dbg_pins && honour_movable {
                let foreign: std::collections::HashSet<usize> = others.iter().copied().collect();
                own.iter()
                    .filter(|&&a| {
                        foreign.contains(&a)
                            && cratonvm_gc_core::gc_quiescence::is_movable_jit_root(a)
                            && !cratonvm_gc_core::gc_quiescence::is_unrewritable_jit_root(a)
                    })
                    .count()
            } else {
                0
            };
            let (mut kept, n_mov, n_unrew) = g1_kept_pin_words(
                own,
                others,
                honour_movable,
                cratonvm_gc_core::gc_quiescence::is_movable_jit_root,
                cratonvm_gc_core::gc_quiescence::is_unrewritable_jit_root,
            );
            if let Some(regions) = above_cursor {
                let before = kept.len();
                kept.retain(|&addr| !self.pin_word_above_cursor(regions, addr));
                declined = (before - kept.len()) as u64;
            }
            if gc_flags().g1_dbg_pins {
                tracing::warn!(
                    "[g1][MOVPIN] snapshot={} kept={} movable_claimed={} unrew_veto={} honour_movable={} coverage_incomplete={} movable_set={} others={} rescued={}",
                    snap_len,
                    kept.len(),
                    n_mov,
                    n_unrew,
                    honour_movable,
                    cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete(),
                    cratonvm_gc_core::gc_quiescence::movable_jit_root_count(),
                    n_others,
                    rescued,
                );
            }
            kept.into_iter()
                .filter_map(|addr| self.lookup_region_for_addr(addr))
                .collect()
        } else {
            RegionSet::new()
        };
        // Not gated on `jit_frames_live_for_pins`: a blocked peer's Rust
        // frames hold these words whether or not a compiled frame is live.
        // See `g1_pins_peer_stack_words`. With `peer_words` false the caller
        // pins them itself (`g1_pins_peer_stack_objects`), or, in cleanup, not
        // at all (`g1_cleanup_skips_peer_stack_words`). Either way the
        // post-block screen runs first, so no consumer of this pause's
        // captures, the fold included, sees a word it removes.
        if g1_peer_pins_skip_post_block_regions() {
            self.drop_post_block_peer_stack_words(regions);
        }
        if peer_words {
            let mut words = Vec::new();
            cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut words, |_| true);
            let mut by_region: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
            for addr in words {
                if let Some(regions) = above_cursor {
                    if self.pin_word_above_cursor(regions, addr) {
                        declined += 1;
                        continue;
                    }
                }
                if let Some(idx) = self.lookup_region_for_addr(addr) {
                    by_region.entry(idx).or_default().push(addr);
                }
            }
            let starts_only = !by_region.is_empty();
            let jit_skips = if starts_only {
                self.jit_tlab_skip_spans()
            } else {
                Vec::new()
            };
            for (idx, mut words) in by_region {
                if starts_only {
                    words.sort_unstable();
                    if !peer_words_name_an_object_start(regions, idx, &words, &jit_skips) {
                        continue;
                    }
                }
                set.insert(idx);
            }
        }
        for &(start, end) in self.jit_tlab_skip_regions.lock().iter() {
            // A mutator TLAB is carved from a single Eden region
            // (`refill_tlab`), so one lookup suffices; the `end - 1` probe is
            // defense-in-depth should that invariant ever change.
            if let Some(idx) = self.lookup_region_for_addr(start) {
                set.insert(idx);
            }
            if end > start {
                if let Some(idx) = self.lookup_region_for_addr(end - 1) {
                    set.insert(idx);
                }
            }
        }
        (set, declined)
    }

    /// gkr (`CRATONVM_G1_DBG_PINS=1`, diagnostic): for each region of `held`,
    /// every pin word that lies in it and where the word came from -- the
    /// calling thread's JIT band (`own`, with its movable / unrewritable
    /// claim), another publisher's (`other`), a blocked peer's stack
    /// (`peer`), a TLAB skip span (`tlab`), or a non-object root (`root`),
    /// with the frame provenance the scan recorded. One string, for the
    /// in-place pass's `[g1][IN-PLACE]` report.
    pub fn dbg_held_region_pin_sources(&self, roots: &[ObjectRef], held: &RegionSet) -> String {
        let mut rows: Vec<(usize, String)> = Vec::new();
        let mut note = |addr: usize, src: &str| {
            if let Some(idx) = self.lookup_region_for_addr(addr) {
                if held.contains(&idx) {
                    let prov = cratonvm_gc_core::gc_quiescence::lookup_jit_root_provenance(addr)
                        .unwrap_or_else(|| "-".into());
                    rows.push((idx, format!("0x{addr:x}:{src}:[{prov}]")));
                }
            }
        };
        let (own, others) = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot_split();
        for a in own {
            let claim = match (
                cratonvm_gc_core::gc_quiescence::is_movable_jit_root(a),
                cratonvm_gc_core::gc_quiescence::is_unrewritable_jit_root(a),
            ) {
                (true, false) => "own-movable",
                (_, true) => "own-unrew",
                (false, false) => "own",
            };
            note(a, claim);
        }
        for a in others {
            note(a, "other");
        }
        let mut words = Vec::new();
        cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut words, |_| true);
        for a in words {
            note(a, "peer");
        }
        for &(start, _) in self.jit_tlab_skip_regions.lock().iter() {
            note(start, "tlab");
        }
        for r in roots {
            note(r.as_ptr() as usize, "root");
        }
        rows.sort_by_key(|(i, _)| *i);
        let mut out = String::new();
        for (i, s) in rows {
            out.push_str(&format!(" r{i}={s}"));
        }
        out
    }

    /// INT-3 — publish the reserved (un-retired) TLAB tails of frozen in-JIT
    /// peers / blocked threads as absolute `(cursor, end)` address ranges so
    /// this collection's region walkers skip them and their regions stay out
    /// of the CSet (see [`Self::jit_tlab_skip_regions`]). Replaces any
    /// previously-set list. Must be set under STW immediately before the
    /// collection and cleared immediately after.
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        let mut g = self.jit_tlab_skip_regions.lock();
        g.clear();
        g.extend_from_slice(regions);
    }

    /// INT-3 — clear the published TLAB skip regions (see
    /// [`Self::set_jit_tlab_skip_regions`]).
    pub fn clear_jit_tlab_skip_regions(&self) {
        self.jit_tlab_skip_regions.lock().clear();
    }

    /// INT-3 — snapshot of the published frozen-peer TLAB tails, taken once
    /// per walk loop (not per object). Empty on every normal cycle.
    pub fn jit_tlab_skip_spans(&self) -> Vec<(usize, usize)> {
        self.jit_tlab_skip_regions.lock().clone()
    }

    /// Find which region contains the given address (by raw address).
    ///
    /// O(log R) via the cached `region_lookup` table. See
    /// [`Self::region_for_ptr`] for the rationale; this variant is the
    /// hot path for the write barrier in
    /// [`Self::post_write_barrier_rset`], where it is called twice per
    /// reference store.
    pub fn region_for_ptr_with_regions(&self, _regions: &[G1Region], addr: usize) -> Option<usize> {
        self.lookup_region_for_addr(addr)
    }

    /// JIT round 23 wave 3, lane L4 (`CRATONVM_G1_YOUNG_SOURCE_FILTER`,
    /// HotSpot's young-card filter): may COMPILED code's store with `holder`
    /// skip the post-write barrier because the holder lives in an Eden /
    /// Survivor region? Asked by the VM's lean `jit_g1_post_write_barrier`
    /// (and answered inline by the compiled sequence through
    /// [`crate::gen_heap::JIT_G1_YOUNG`]). Sound only with the prerequisite
    /// the switch implies ([`g1_young_source_prereq_enabled`]): a young region
    /// is either evacuated by the next pause (its live objects scanned) or,
    /// when that pause leaves it out, re-walked for its outgoing edges before
    /// any remembered set is read; cleanup scrubs young regions unscreened.
    ///
    /// Deliberately NOT inside [`Self::post_write_barrier_rset`]: the
    /// interpreter's, the natives' and the collector's own stores keep
    /// recording (an extra young-source edge is only over-approximation), so
    /// the barrier's contract for every other caller is unchanged.
    pub fn young_holder_skips_post_barrier(&self, holder: ObjectRef) -> bool {
        if !self.young_source_filter_on() {
            return false;
        }
        self.lookup_region_for_addr(holder.as_ptr() as usize)
            .is_some_and(|idx| self.region_is_young_unlocked(idx))
    }

    /// JIT round 23 wave 3, lane L4: is the young-holder filter (and so its
    /// prerequisite's pause work) on for THIS heap? The process switches
    /// ([`g1_young_source_filter_enabled`]) AND this heap's cleanup
    /// freed-target scrub (`CRATONVM_G1_CLEANUP_SCRUB_FREED_TARGETS`), because
    /// the prerequisite's soundness argument leans on that scrub: without it a
    /// dead young object keeps a slot into a region `cleanup` freed, and the
    /// pause walk of a pinned young region would record (and Phase 2 follow)
    /// that stale slot on every pause (r23 wave 3 review).
    #[inline]
    pub fn young_source_filter_on(&self) -> bool {
        self.cleanup_scrub_freed_targets && g1_young_source_filter_enabled()
    }

    /// JIT round 23 wave 3, lane L4: is region `idx` Eden or Survivor, read
    /// WITHOUT the regions lock (the young-source barrier filter's question).
    /// Sound because the `Vec<G1Region>` is never resized after `new` (see
    /// `LAST_RSET_TARGET`'s SAFETY invariant) and `region_type` is an
    /// `AtomicU8`; the answer is stable outside a pause.
    #[inline]
    pub fn region_is_young_unlocked(&self, idx: usize) -> bool {
        // SAFETY: the vector's buffer and length never change after `new`;
        // only the region's atomic type byte is read.
        let regions: &Vec<G1Region> = unsafe { &*self.regions.data_ptr() };
        regions.get(idx).is_some_and(|r| {
            matches!(r.region_type(), RegionType::Eden | RegionType::Survivor)
        })
    }

    /// JIT round 23 wave 3, lane L4 -- the young-source PREREQUISITE
    /// ([`g1_young_source_prereq_enabled`]): walk every Eden / Survivor region
    /// that is NOT in this pause's collection set (`in_cset`) and record each
    /// object's outgoing cross-region edges and card
    /// ([`Self::record_outgoing_rset_edges`]), so a pinned young holder whose
    /// stores the young filter skipped is a remembered-set source exactly as if
    /// the barrier had run. Called right after the pause chooses its young set,
    /// before Phase 2, eager humongous reclaim or any fix-up reads a remembered
    /// set or a card. A no-op (zero walked) unless the filter is on. Answers the
    /// number of regions walked.
    ///
    /// Dead objects are walked too (their slots then name referents the pause
    /// keeps alive through the source walk), which is the "undead" cost the
    /// barrier itself always paid for a pinned region's dead holders; slots
    /// into a region `cleanup` freed cannot exist, because the cleanup scrub
    /// walks every young region unscreened while the filter is on.
    pub fn record_excluded_young_sources(
        &self,
        regions: &mut [G1Region],
        in_cset: &dyn Fn(usize) -> bool,
    ) -> usize {
        if !self.young_source_filter_on() {
            return 0;
        }
        let jit_skips = self.jit_tlab_skip_spans();
        let mut walked = 0usize;
        for idx in 0..regions.len() {
            if !matches!(
                regions[idx].region_type(),
                RegionType::Eden | RegionType::Survivor
            ) || in_cset(idx)
            {
                continue;
            }
            // SAFETY: a stop-the-world pause holds the regions write guard
            // `regions` was borrowed from; the walk stays below the cursor.
            let starts = unsafe { young_region_object_starts(&regions[idx], &jit_skips) };
            for addr in starts {
                let _ = self.record_outgoing_rset_edges(regions, addr);
            }
            walked += 1;
        }
        if walked > 0 {
            G1_YOUNG_SOURCES_REWALKED.fetch_add(walked as u64, Ordering::Relaxed);
        }
        walked
    }

    /// Binary-search the cached `(base_addr, region_idx)` table to find
    /// which region (if any) owns `addr`.
    ///
    /// Complexity: O(log R) — independent of the number of live regions.
    ///
    /// Returns `Some(idx)` iff `addr` falls within `[base, base + region_size)`
    /// for some region. The check uses `self.config.region_size` instead of
    /// the per-region `data.len()` because every region's backing buffer is
    /// allocated at exactly `region_size` bytes (see [`G1Region::from_arena`]).
    ///
    /// O(1) ARITHMETIC, not the binary search this used to be. Region `i`'s
    /// base is exactly `arena_base + i * region_size` — `new` carves one
    /// contiguous `arena` into adjacent equal slices and never reallocates it —
    /// so the index is a subtraction and a divide, and the same form is already
    /// used by `header_verdict_at` and the walkers.
    ///
    /// It matters because this is the hottest read in the collector: the
    /// reference write barrier calls it TWICE per store. A `partition_point`
    /// over 1500 entries touches ~11 scattered cache lines each time, and eight
    /// mutators doing that 4M times apiece thrash the shared cache — which is
    /// not a lock, so it does not show up as one, but it scaled just like one.
    /// Isolated with a store whose referent is its own array, so the barrier
    /// returns immediately after these two lookups and nothing else runs
    /// (`BarrierProbe refself`, 2M stores): **1051 ms on one thread, 4923 ms on
    /// eight**, matching `refstore`'s scaling exactly and proving the residue
    /// was here rather than in the remembered set (whose mutex the census
    /// recorded being taken 27 times in that entire run).
    #[inline]
    pub fn lookup_region_for_addr(&self, addr: usize) -> Option<usize> {
        if addr < self.arena_base || addr >= self.arena_end {
            return None;
        }
        let region_size = self.config.region_size;
        if region_size == 0 {
            return None;
        }
        // F-09 — a SHIFT, not the 64-bit `div` this used to be. The divisor is
        // a runtime value, so `/` compiled to a real `div`: tens of cycles,
        // unpipelined, twice per compiled reference store (the write barrier
        // looks up both ends) and once per reference slot in every collector
        // walk. `normalize_region_size` in `new` is what licenses the shift.
        let idx = (addr - self.arena_base) >> self.region_shift;
        debug_assert_eq!(
            Some(idx),
            {
                let pp = self
                    .region_lookup
                    .partition_point(|(base, _)| *base <= addr);
                if pp == 0 {
                    None
                } else {
                    let (base, i) = self.region_lookup[pp - 1];
                    if addr < base.wrapping_add(region_size) {
                        Some(i)
                    } else {
                        None
                    }
                }
            },
            "arithmetic region index disagrees with the region_lookup table at addr=0x{addr:x} — the arena is no longer a run of equal adjacent slices and every caller of this function is now wrong"
        );
        Some(idx)
    }

    // -----------------------------------------------------------------------
    // Humongous object addressing
    // -----------------------------------------------------------------------
    //
    // A humongous object spans a contiguous run of region indices, which —
    // because all regions are adjacent slices of one `arena` — is one
    // physically-contiguous block. The real `ObjectHeader` sits at offset 0 of
    // the start region and the payload flows straight through, so logical
    // payload byte `P` lives at `start_addr + HEADER_SIZE + P`. The accessors
    // below could read/write that flat range directly; `humongous_copy` is
    // retained so the existing field/array accessor call sites are unchanged.

    /// Could an object of `total_object_size` bytes possibly be humongous?
    ///
    /// Lock-free, and exact in the direction that matters. `alloc_in_region`
    /// routes a request to `claim_humongous_span_locked` iff
    /// `size > region_size / 2`, and `refill_tlab` never hands out a chunk
    /// larger than that same bound (since 2026-09-20 it CLAMPS an over-large
    /// request to it rather than refusing — see `refill_tlab` — which keeps
    /// the invariant exact: a chunk of `region_size / 2` cannot hold an object
    /// that exceeds `region_size / 2`). So anything at or below the bound was
    /// allocated inside ONE region, and its [`Self::humongous_span`] is
    /// necessarily `None`.
    ///
    /// Answering `false` therefore licenses skipping the `regions` lock
    /// altogether, which is the entire point of this predicate. Every accessor
    /// on this collector — `get_field`, `set_field`, `get_array_element`,
    /// `set_array_element` — took that one global mutex on EVERY call, and the
    /// only thing it protected was the `region_type` read inside
    /// `humongous_span`. So a `putfield` on one thread serialized against a
    /// `getfield` on every other, and an `int[]` store paid for a humongous
    /// check that could not possibly apply to it. Measured with a
    /// constant-total-work store loop (`BarrierProbe primstore` — an `int[]`
    /// store, no GC barrier of any kind): **768 ms on one thread, 3172 ms on
    /// eight**, against ZGC flat at ~790/850 ms.
    ///
    /// Only the SIZE is consulted, never the object's address, so this is safe
    /// to call before any region state is stable.
    #[inline]
    pub fn may_be_humongous(&self, total_object_size: usize) -> bool {
        total_object_size > self.config.region_size / 2
    }

    /// Record one accessor call for the `CRATONVM_DBG_G1ACCESSOR` census, and
    /// answer the same thing [`Self::may_be_humongous`] does.
    ///
    /// Wrapping the predicate rather than counting at each of the four call
    /// sites keeps the census and the decision impossible to disagree: whatever
    /// routed the call is what gets counted.
    #[inline]
    pub fn accessor_needs_regions_lock(&self, total_object_size: usize) -> bool {
        let locked = self.may_be_humongous(total_object_size);
        if gc_flags().g1_dbg_accessor {
            if locked {
                self.accessor_calls_locked.fetch_add(1, Ordering::Relaxed);
            } else {
                self.accessor_calls_lockfree.fetch_add(1, Ordering::Relaxed);
            }
        }
        locked
    }

    /// Print the `CRATONVM_DBG_G1ACCESSOR` census. Called once, at VM exit.
    pub fn dbg_report_accessor_census(&self) {
        if !gc_flags().g1_dbg_accessor {
            return;
        }
        let free = self.accessor_calls_lockfree.load(Ordering::Relaxed);
        let locked = self.accessor_calls_locked.load(Ordering::Relaxed);
        let total = free + locked;
        let memo_hit = self.rset_edge_memo_hit.load(Ordering::Relaxed);
        let recorded = self.rset_edge_recorded.load(Ordering::Relaxed);
        let same_region = self.rset_edge_same_region.load(Ordering::Relaxed);
        // W3-B — the fail-safe's own frequency, beside the fast path it guards.
        // `epoch_raced=0` over a large `cross_region_stores` is the evidence
        // that the barrier's lock-free path is not in fact racing
        // reclassification on this workload; a non-zero value says it is, and
        // that the page's option-4 fix is worth its atomic field.
        let epoch_raced = self.rset_fast_path_epoch_raced.load(Ordering::Relaxed);
        eprintln!("[g1-accessor] rset fast_path_epoch_raced={epoch_raced}");
        eprintln!(
            "[g1-accessor] rset cross_region_stores={} memo_hit={memo_hit} took_rset_mutex={recorded} same_region_skipped={same_region} memo_hit_pct={:.2}",
            memo_hit + recorded,
            if memo_hit + recorded == 0 {
                0.0
            } else {
                memo_hit as f64 * 100.0 / (memo_hit + recorded) as f64
            }
        );
        // vhc21 (VHCAE21-7): the compiled edge filter keeps ONE source region per
        // destination region. `alternating` is the number of times a barrier
        // call replaced a live-epoch entry naming another source; widen the
        // entry to two ways only if it is a large share of the calls that
        // reach the filter (`unchanged + first + alternating`).
        let (jit_same, jit_first, jit_alt) = self.jit_edge_filter.census();
        let jit_total = jit_same + jit_first + jit_alt;
        eprintln!(
            "[g1-accessor] jit edge filter note_edge calls={jit_total} unchanged={jit_same} first_or_new_epoch={jit_first} alternating_source={jit_alt} alternating_pct={:.2}",
            if jit_total == 0 {
                0.0
            } else {
                jit_alt as f64 * 100.0 / jit_total as f64
            }
        );
        eprintln!(
            "[g1-accessor] field/array accessor calls={total} lock_free={free} took_regions_lock={locked} lock_free_pct={:.2}",
            if total == 0 {
                0.0
            } else {
                free as f64 * 100.0 / total as f64
            }
        );
    }

    /// If `obj`'s start address names a `HumongousStart` region, return the
    /// start region index and the total payload byte count (object size minus
    /// the single ObjectHeader). Returns `None` for ordinary (non-humongous)
    /// objects, whose access uses the plain flat-offset path.
    ///
    /// Takes the already-held `regions` slice to avoid re-locking.
    ///
    /// Gate every call on [`Self::may_be_humongous`] — it answers the same
    /// question for the common case without the lock this one needs.
    pub fn humongous_span(
        &self,
        regions: &[G1Region],
        obj: ObjectRef,
        total_object_size: usize,
    ) -> Option<(usize, usize)> {
        let idx = self.lookup_region_for_addr(obj.as_ptr() as usize)?;
        if regions[idx].region_type() != RegionType::HumongousStart {
            return None;
        }
        let payload_bytes = total_object_size.saturating_sub(ARRAY_DATA_OFFSET);
        Some((idx, payload_bytes))
    }

    /// `true` iff `obj`'s start address names a `HumongousStart` region.
    ///
    /// The object's payload is one contiguous block (the regions are adjacent
    /// arena slices), so a flat `obj.as_ptr() + HEADER_SIZE + i*stride` access
    /// is valid for callers — this predicate is retained for the GC-internal
    /// accessor routing and for callers that must distinguish humongous (e.g.
    /// to skip evacuation of an in-place, non-relocated object).
    ///
    /// This is the size-independent sibling of `humongous_span` (which also
    /// needs the object's total size to compute the payload byte count).
    /// Takes the `regions` lock briefly to inspect the region type — mirrors
    /// the `lookup_region_for_addr` + `RegionType::HumongousStart` check in
    /// `humongous_span`.
    pub fn is_humongous(&self, obj: ObjectRef) -> bool {
        let regions = self.regions.read();
        match self.lookup_region_for_addr(obj.as_ptr() as usize) {
            Some(idx) => regions[idx].region_type() == RegionType::HumongousStart,
            None => false,
        }
    }

    /// Copy `len` bytes of a humongous object's payload, starting at logical
    /// payload offset `payload_off`, between the heap backing store and the
    /// caller-provided `buf`.
    ///
    /// `write == true` copies `buf -> heap`; otherwise `heap -> buf`. The
    /// humongous object is one contiguous block (its regions are adjacent
    /// arena slices), so this is a single bounds-checked `memcpy` at
    /// `start_addr + HEADER_SIZE + payload_off`. Returns `false` if the access
    /// would exceed the object's payload (checked up front, before any copy) —
    /// the caller turns that into a dropped access, exactly as the `index >=
    /// len` bounds checks do.
    ///
    /// SAFETY: callers hold the `regions` lock for the duration, so the region
    /// classification (and thus the reserved arena span) is stable. `start`
    /// must be a `HumongousStart` index and `total_payload` the object's
    /// payload byte count (object size minus the single `ObjectHeader`).
    pub fn humongous_copy(
        &self,
        regions: &[G1Region],
        start: usize,
        total_payload: usize,
        payload_off: usize,
        buf: *mut u8,
        len: usize,
        write: bool,
    ) -> bool {
        // Reject any access whose end exceeds the object's payload.
        let end = match payload_off.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        if end > total_payload {
            return false;
        }
        if start >= regions.len() {
            return false;
        }

        // The payload is contiguous from `start_addr + HEADER_SIZE`. Use the
        // region's integer base address (not the `Deref` slice, whose `len` is
        // only `region_size`) so the pointer carries arena provenance across
        // region boundaries.
        let phys = (regions[start].data.addr() + ARRAY_DATA_OFFSET + payload_off) as *mut u8;
        // SAFETY: `payload_off + len <= total_payload`, and the humongous span
        // reserved `ceil(size/region_size)` contiguous arena regions covering
        // `HEADER_SIZE + total_payload` bytes from `start_addr`, so
        // `[phys, phys+len)` is inside the object's reserved, contiguous,
        // arena-backed memory. `buf` is a caller-owned buffer of >= `len` bytes.
        unsafe {
            if write {
                std::ptr::copy_nonoverlapping(buf, phys, len);
            } else {
                std::ptr::copy_nonoverlapping(phys, buf, len);
            }
        }
        true
    }

    /// Lock-free `[base, end)` envelope of the single contiguous G1 arena.
    /// See [`crate::gen_heap::GenerationalHeap::conservative_addr_span`] for
    /// the contract; here the bounds are immutable for the collector's
    /// lifetime.
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        if self.arena_end > self.arena_base {
            Some((self.arena_base, self.arena_end))
        } else {
            None
        }
    }

    /// Conservative validity check for a *raw address* — see
    /// [`crate::gen_heap::GenerationalHeap::is_object_address`] for the
    /// contract. NEW-1.5 JIT frame root scanning calls this through
    /// [`crate::vm_heap::VmHeap::is_object_address`].
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        // SAFETY: `is_object_address_inner` accepts only an address inside a
        // live region whose header tags and sanity bits were validated.
        self.object_start_counted(addr)
            .map(|raw| unsafe { ObjectRef::from_raw(raw) })
    }

    /// [`Self::is_object_address`]'s yes/no without building the `ObjectRef`
    /// (see `GenerationalHeap::is_object_start`; proposal
    /// `interpreter-L7-proposal-the-header-guards-validate-without-building-an-objectref-FIXED-20261013`,
    /// interpreter round i1 wave 49, lane L7). The census counts it as a call
    /// of `is_object_address`.
    #[inline]
    pub fn is_object_start(&self, addr: usize) -> bool {
        self.object_start_counted(addr).is_some()
    }

    /// The census wrapper of [`Self::is_object_address_inner`].
    #[inline]
    pub fn object_start_counted(&self, addr: usize) -> Option<*mut u8> {
        let __t0 = g1_objaddr_census_on().then(std::time::Instant::now);
        let __r = self.is_object_address_inner(addr);
        if let Some(t0) = __t0 {
            G1_OBJADDR_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if __r.is_some() {
                G1_OBJADDR_ACCEPTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            G1_OBJADDR_NANOS.fetch_add(
                t0.elapsed().as_nanos() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        __r
    }

    /// Publish `cset` for the duration of a parallel evacuation. The set is
    /// installed before the release-store that opens the fast gate, so a root
    /// screen that observes the gate cannot miss a member.
    pub fn activate_evacuation_cset(&self, cset: &RegionSet) -> ActiveEvacuationCsetGuard<'_> {
        *self.active_evacuation_cset.write() = cset.clone();
        self.active_evacuation.store(true, Ordering::Release);
        ActiveEvacuationCsetGuard { collector: self }
    }

    /// Is `addr` a CSet source whose forwarding claim is still being
    /// published? This deliberately reads only the mark word: resolving its
    /// shape can call `forwarding_address()` and used to turn this conservative
    /// screen into a busy/yield loop while the parallel evacuator owned it.
    #[inline]
    pub fn is_busy_forwarding_claim_in_active_cset(&self, addr: usize) -> bool {
        if !self.active_evacuation.load(Ordering::Acquire)
            || addr < self.arena_base
            || addr >= self.arena_end
            || addr & 0x7 != 0
        {
            return false;
        }
        let region_index = (addr - self.arena_base) >> self.region_shift;
        if !self.active_evacuation_cset.read().contains(&region_index) {
            return false;
        }
        // SAFETY: the arena bounds and alignment above put the atomic mark
        // word fully inside the fixed backing allocation. This is a screen,
        // not an object proof; false positives take the conservative reject.
        let mark = unsafe {
            (&*(addr as *const ObjectHeader))
                .mark_word
                .load(Ordering::Acquire)
        };
        ObjectHeader::is_forwarded_mark(mark) && mark & cratonvm_types::MARK_FWD_BUSY != 0    }

    /// Whether a `MIN_OBJECT_SIZE` object at `addr` (inside the arena) ends
    /// inside `addr`'s own region. Every real object does: `bump_alloc` carves
    /// whole objects out of one region (a humongous one starts at its region's
    /// base), and `MIN_OBJECT_SIZE` covers both header words.
    #[inline]
    pub fn min_object_fits_in_region(&self, addr: usize) -> bool {
        let index = (addr - self.arena_base) >> self.region_shift;
        let region_end = self.arena_base + ((index + 1) << self.region_shift);
        addr + cratonvm_types::MIN_OBJECT_SIZE <= region_end
    }

    /// Whether `header`'s forwarding chain (none, a self-forward, or up to
    /// eight forwards, as `ObjectHeader::shape_source` follows) stays on
    /// 8-aligned addresses inside the arena's committed prefix. Reads only
    /// mark words and second words of committed arena addresses.
    pub fn forward_chain_in_arena(&self, header: &ObjectHeader) -> bool {
        let end = self.arena_base + self.arena.committed_len();
        let mut h = header;
        for _ in 0..8 {
            let mark = h.mark();
            if !ObjectHeader::is_forwarded_mark(mark) || mark & cratonvm_types::MARK_FWD_SELF != 0 {
                return true;
            }
            let t = h.forwarding_address() as usize;
            if t & 7 != 0
                || t < self.arena_base
                || t.saturating_add(cratonvm_types::MIN_OBJECT_SIZE) > end
            {
                return false;
            }
            if std::ptr::eq(t as *const ObjectHeader, h) {
                return true;
            }
            // SAFETY: an aligned arena address with both header words inside
            // the arena's fixed backing allocation.
            h = unsafe { &*(t as *const ObjectHeader) };
        }
        true
    }

    #[inline]
    pub fn is_object_address_inner(&self, addr: usize) -> Option<*mut u8> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        // Round 11 wave 10 (lane `rt`): an object-START proof for humongous
        // memory, from the region kind the liveness lookup already reads. A
        // humongous object begins at offset 0 of its `HumongousStart` region
        // and its span holds nothing else, so every other word there is payload
        // (the likeliest false root by volume: interior words into big arrays),
        // however its bytes decode.
        match self.live_span_at(addr)? {
            LiveSpan::Allocated => {}
            LiveSpan::HumongousStart { base } => {
                if addr != base {
                    return None;
                }
            }
            LiveSpan::HumongousContinuation => return None,
        }
        // Strings round 2 (`str17`): no object starts in its region's last
        // `MIN_OBJECT_SIZE - 1` bytes, and the checks below read the header's
        // SECOND word (`num_slots` / `array_length`, offset 8) -- which, for a
        // candidate in a region's last word, is the NEXT region's first word,
        // possibly decommitted under a small heap: a conservative JIT-frame word
        // there took the root scan down with SIGSEGV in `resolved_shape`
        // (`-XX:+UseG1GC -Xmx24m`, about one run in five, base and dev alike).
        if !self.min_object_fits_in_region(addr) {
            return None;
        }
        let raw = addr as *const u8;
        // Validate raw enum tags before borrowing as `ObjectHeader`; this path
        // is fed by conservative JIT-frame words and must reject garbage
        // without letting invalid `#[repr(u8)]` values reach a debug enum
        // match.
        let kind = unsafe { object_kind_from_tag(cratonvm_types::kind_tag_at(raw)) }?;
        if !unsafe { cratonvm_types::element_type_tag_ok_at(raw) } {
            return None;
        }
        if kind == ObjectKind::HumongousFiller {
            return None;
        }

        // SAFETY: address is inside a live region and both enum tag bytes have
        // been validated.
        let header = unsafe { &*(raw as *const ObjectHeader) };
        // Round 11 (lane `rt`): outside humongous memory this check has no
        // object-START proof (see
        // `docs/feature-designs/concurrent-gc-maturation.md`),
        // so an interior word whose tag bytes happen to decode was accepted.
        // `MARK_RESERVED_MASK` is zero in every header any allocator publishes
        // (the JIT's one-byte kind guards depend on it), so a set bit there is
        // proof of NOT-a-header: rejecting on it can never drop a real object,
        // and it turns away 3 of every 4 random words that got this far. Used
        // only to reject, never to accept, per `reserved_mark_bits_clear`'s
        // contract.
        if !header.reserved_mark_bits_clear() {
            return None;
        }
        // The G1 near-full proxy row (`CRATONVM_G1_SCREEN_FORWARD_IN_ARENA`,
        // default ON, `=0` kills): the shape reads below follow a forwarded
        // mark through its second word, so a candidate whose mark reads as
        // forwarded is accepted only when that chain stays inside this
        // collector's arena. A conservative JIT word into a dead husk (or any
        // bytes that decode as a forward) took the root scan down with
        // SIGSEGV in `resolved_shape_slow` following `0x10100000000`: the
        // shape walk's own screen is a coarse provenance-map test, and 0x2000
        // and 0x37c8de78 passed it too. A real forward this collector made
        // targets the arena.
        if !self.forward_chain_in_arena(header) {
            return None;
        }
        // Multi-array reloc fix (2026-05-22): `alloc_array` mirrors the
        // array length into `num_slots`, so a legitimate 256 MB int[] has
        // num_slots = 2^26 > 1<<24 and would be falsely rejected here.
        // Gate num_slots only for non-arrays, and bound array_length at
        // the JVM `Integer.MAX_VALUE` ceiling (matches `array_length()`).
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24;
        let is_array = kind == ObjectKind::Array;
        // Round 11 wave 15 (lane `rt`): only a LONG (legacy) header has a
        // shape word to screen. Since the 8-byte header, `num_slots()` on a
        // compact instance is `compact_field_count(class_id)` -- a per-thread
        // layout-cache probe plus a `layout_generation` load -- whose answer is
        // a class's field count (< 65 536, JVMS 4.1) or `0`, so it can never
        // exceed the bound. Skipping it keeps the verdict identical and takes
        // that probe off the read barrier's hot path (`load_and_forward`).
        if !is_array && !header.is_short() && header.num_slots() > MAX_PLAUSIBLE_SLOTS {
            return None;
        }
        if is_array && header.array_length() > i32::MAX as u32 {
            return None;
        }
        Some(raw as *mut u8)
    }

    /// [`Self::is_object_address`] plus a region-end size screen, for the
    /// conservative JIT stack-band scans only (rt proposal W11-2, page
    /// `r11-rt-g1-object-check-accepts-interior-words`).
    ///
    /// Outside humongous memory, a word the plain check accepts is only a
    /// header-shaped word. The plain check has no object-START proof. This
    /// entry rejects one more class of such words: a decoded header whose object
    /// would run past the end of its region. That cannot be a real object.
    ///
    /// * Every non-humongous object is carved by `G1Region::bump_alloc_initialized`,
    ///   which refuses any claim whose end passes `data.len()`, the region
    ///   size. That covers TLAB chunks, per-object allocation and evacuation
    ///   destinations. `set_cursor` only lowers a cursor (a TLAB retire, or
    ///   `reset`), or sizes a humongous start. The region walkers depend on the
    ///   same bound: `walk_objects` stops at an object whose end passes the
    ///   cursor.
    /// * A humongous object is the only object that crosses a region end. The
    ///   plain check accepts one only at the base of its `HumongousStart`
    ///   region (wave 10). This entry keeps that answer: when the size screen
    ///   fails, it asks the span kind once more, and accepts exactly the
    ///   humongous base.
    ///
    /// So a reject here drops only words that are not objects. The accept side
    /// is unchanged, so a live root is never lost. It is a separate entry
    /// because `is_object_address` is also the software read barrier's hot path
    /// (`load_and_forward`), and that path should not pay a size computation on
    /// every real object. A compact object's size reads the class-layout cache.
    /// A layout miss answers an impossible size, so such a word is rejected as
    /// an object whose size is unknown. That is the same verdict the region
    /// walkers reach for it.
    ///
    /// # Not a start proof, on purpose (r11w19 g1store)
    ///
    /// A header-shaped word whose object fits below its region's cursor is
    /// still accepted, the `obj + 8` word of a compact instance included. That
    /// acceptance is load-bearing: under G1 every band root is also a PIN (the
    /// root gatherer pins `roots[jit_scan_start..]` by address), and the pin
    /// keeps the word's region out of the collection set, so the object it is
    /// interior to cannot move under a derived pointer. Rejecting the word drops
    /// that pin. Translating it to its containing object would keep the pin
    /// but needs a walkable region grid, which exists only inside a pause
    /// (mutators fill TLABs, and carves are zeroed after their claim, while
    /// these scans run). So the start proof lives at every consumer that acts on
    /// a start: the evacuator pins a root without `GC_FLAG_HEADER`
    /// (`pinned_region_set_including_non_object_roots`), `remark` places a mark
    /// root in its containing object (`mark_root_seed`), and the marker's gray
    /// gate refuses a flag-less header as torn (`classify_mark_scan_target`),
    /// as pinned by
    /// `r11w19_the_root_scan_entry_keeps_a_flagless_interior_word_for_its_region_pin`.
    pub fn is_object_address_for_root_scan(&self, addr: usize) -> Option<ObjectRef> {
        // A forwarding claim belongs only to the active CSet. Do this before
        // the generic header screen: its size accessors resolve forwarding
        // targets, and a BUSY source has no target to resolve yet. Returning no
        // root is the existing bounded-wait fallback, reached immediately
        // rather than after thousands of useless probes.
        if self.is_busy_forwarding_claim_in_active_cset(addr) {
            return None;
        }
        let obj = self.is_object_address(addr)?;
        // Accepted, so `addr` is in the arena and `region_size != 0`: the same
        // shift-indexed region arithmetic as `lookup_region_for_addr`.
        let region_index = (addr - self.arena_base) >> self.region_shift;
        let region_end = self.arena_base + ((region_index + 1) << self.region_shift);
        // SAFETY: `is_object_address` validated both tag bytes of the header
        // at `addr`, which lies inside a live region.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        let size = if header.kind() == ObjectKind::Array {
            array_data_size(header.array_length() as usize, header.element_type())
                .ok()
                .and_then(|data| data.checked_add(ARRAY_DATA_OFFSET))
        } else {
            // Header included (the 8-byte compact header made the compact
            // layout's size absolute); a layout miss answers the impossibly
            // large `IMPLAUSIBLE_BODY_SIZE`, which fails the region screen.
            Some(cratonvm_gc_core::object_instance_size(header))
        };
        let end = size.and_then(|size| addr.checked_add(size));
        if let Some(end) = end {
            if end <= region_end && self.root_scan_end_is_below_cursor(region_index, addr, end) {
                return Some(obj);
            }
        }
        match self.live_span_at(addr) {
            Some(LiveSpan::HumongousStart { base }) if base == addr => Some(obj),
            _ => None,
        }
    }

    /// `root_scan_end_is_below_cursor`: does a decoded object at `addr` that
    /// ends at `end` lie below region `region_index`'s allocation cursor? The
    /// tighter half of [`Self::is_object_address_for_root_scan`]'s size screen
    /// (rt proposal W13-2, round 11 wave 14).
    ///
    /// No real object straddles a region's cursor. Every claim advances the
    /// cursor past the whole claimed range in one step: a TLAB chunk, which
    /// holds every object bump-allocated into it (inline compiled-code bumps
    /// included), a single object, or a humongous start sized to its object.
    /// A cursor written wholesale by `set_cursor` (an evacuation TLAB's
    /// retire, a humongous start, a reset) is the top of what was placed below
    /// it, and those writes are stop-the-world or under the `regions` write
    /// guard. The plain check already requires `addr` below a cursor it read,
    /// and every cursor value above `addr` is at or past the object's end, so
    /// a decoded header whose object runs past it is not an object. That
    /// rejects garbage in the partly filled Eden region, where the region-end
    /// bound is far away.
    ///
    /// The cursor must be read soundly, because a stale LOW value would reject
    /// a real object carved after it. The thread's `live_region_memo` limit
    /// (the cursor seen when its span was filled) is only used to ACCEPT, and
    /// accepting is what this entry did before the screen existed. A REJECT
    /// needs a fresh read, taken under the `regions` guard. A real object
    /// that passed the plain check ends below its span's limit, so the lock is
    /// paid essentially only for the garbage this screen rejects.
    pub fn root_scan_end_is_below_cursor(&self, region_index: usize, addr: usize, end: usize) -> bool {
        let epoch = self.rset_cache_epoch.load(Ordering::Acquire);
        if let Some(limit) = live_region_memo::limit(self.instance_id, epoch, region_index, addr) {
            if end <= limit {
                return true;
            }
        }
        let regions = self.regions.read();
        match regions.get(region_index) {
            Some(r) => end <= (r.data.as_ptr() as usize).saturating_add(r.cursor()),
            // Unreachable for a word the plain check accepted. Accept: this
            // screen only rejects on evidence.
            None => true,
        }
    }

    /// The pin a conservative JIT band word still owes its region after
    /// [`Self::is_object_address_for_root_scan`] REJECTED it (round 12 wave 1,
    /// lane g1store, proposal W19-1; page
    /// `r11w19-g1store-band-root-rejects-drop-derived-pointer-pins`).
    ///
    /// Under G1 an accepted band word is both a root and a pin, and a rejected
    /// one was neither. A derived pointer (an element cursor, a field address)
    /// is always rejected: its bytes are payload, not a header. So the region
    /// it points into lost the pin that kept the object under it from moving,
    /// and a humongous span lost the pin that kept eager reclaim off it. This
    /// answers the pin half only; the word stays out of the root array.
    ///
    /// `None` when `addr` is not in a live region (the pin would name
    /// nothing). Otherwise [`BandRejectPin`], whose `addr` is:
    ///
    /// * humongous: the span's BASE, which is the one object start there. The
    ///   pin then lands on the `HumongousStart` region, exactly where an
    ///   accepted base root lands, so no consumer sees a pin on a
    ///   continuation region it has not seen before. A continuation run with
    ///   no start (a corrupt table) keeps the raw word, which only pins more.
    /// * otherwise the raw word: its region is what the pin must hold.
    ///
    /// Humongous spans are never in a collection set, so a humongous answer
    /// costs one pause's eager reclaim of that span and nothing else.
    pub fn band_reject_pin(&self, addr: usize) -> Option<BandRejectPin> {
        let span = self.live_span_at(addr)?;
        let idx = self.lookup_region_for_addr(addr)?;
        Some(match span {
            LiveSpan::Allocated => BandRejectPin {
                addr,
                humongous: false,
                region: idx,
            },
            LiveSpan::HumongousStart { base } => BandRejectPin {
                addr: base,
                humongous: true,
                region: idx,
            },
            LiveSpan::HumongousContinuation => {
                let regions = self.regions.read();
                match humongous_span_start(&regions, idx)
                    .and_then(|start| regions.get(start).map(|r| (start, r)))
                {
                    Some((start, r)) => BandRejectPin {
                        addr: r.data.as_ptr() as usize,
                        humongous: true,
                        region: start,
                    },
                    None => BandRejectPin {
                        addr,
                        humongous: true,
                        region: idx,
                    },
                }
            }
        })
    }

    /// Loose validity check: alignment + region containment only.
    ///
    /// Mirrors [`crate::gen_heap::GenerationalHeap::is_heap_addr`]. Does NOT
    /// read the object header — used by GC root scanning of ambiguous JVM
    /// long slots, where the strict header check is too aggressive (false
    /// negatives drop legitimately-rooted objects, leading to dangling
    /// pointers post-GC and downstream SEGV).
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        if !self.is_addr_in_live_region(addr) {
            return None;
        }
        // SAFETY: alignment + live-region containment confirmed.
        Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
    }

    /// Check if an address is within a live (non-Free) region.
    ///
    /// Used by reference processing to determine if a referent survived GC
    /// without being relocated (e.g., objects in Old/Humongous regions that
    /// were not part of the collection set).
    pub fn is_addr_in_live_region(&self, addr: usize) -> bool {
        self.live_span_at(addr).is_some()
    }

    /// gci i13/oa item 2: does any of `addrs` lie in a young (Eden, Survivor)
    /// or humongous region -- a region a young pause can reclaim (its
    /// collection set, or an eager humongous reclaim)? One shared guard for
    /// the whole slice; an address outside the heap answers no.
    pub fn any_young_or_humongous_addr(&self, addrs: &[usize]) -> bool {
        let regions = self.regions.read();
        addrs.iter().any(|&addr| {
            self.region_for_ptr(&regions, addr as *mut u8)
                .is_some_and(|idx| region_type_young_pause_reclaims(regions[idx].region_type()))
        })
    }

    /// [`Self::is_addr_in_live_region`], also naming the kind of live span
    /// `addr` falls in (`None` exactly when that predicate is `false`). The
    /// kind costs nothing extra: the region type is read on the locked path
    /// anyway and the memo records it beside the span.
    #[inline]
    pub fn live_span_at(&self, addr: usize) -> Option<LiveSpan> {
        // Fast lock-free arena-bounds gate. `[arena_base, arena_end)` is
        // immutable for the collector's lifetime (single contiguous `Box<[u8]>`,
        // never moved/resized), so the test needs no atomics and no lock. The
        // gate was added for the conservative JIT/native root scan
        // (`scan_active_jit_frames` / `update_root_snapshot`), whose candidate
        // words -- return addresses, ints, native-stack addresses -- mostly lie
        // OUTSIDE the arena and are rejected here without touching
        // `regions.read()` or scanning any region. The implementation before it
        // took the regions mutex and linearly scanned all ~`num_regions` regions
        // for EVERY word, which made JIT-on, deep-stack workloads (e.g. Spring
        // Boot buildSrc JUnit annotation walks) run for minutes / appear hung
        // under G1 while serial GC -- whose `gen_heap` adopted exactly this
        // lock-free gate -- finished in seconds.
        //
        // **The root scan is NOT what makes this hot on every workload, and
        // saying so sent one investigation to the wrong place.** Measured
        // 2026-09-07 on `org.h2.test.store.TestMVStoreTool`'s create phase
        // (`probes/MvsCreate.java`), where this function is the #1 self-time
        // symbol of the whole profile at 14%: `CRATONVM_DBG_JIT_SCAN_PROF=1`
        // reports `scans=0 cache_hits=0 band_scans=0 band_words=0` beside
        // `jit_entries=1617960` -- the band scan never ran. A sampled-backtrace
        // build attributed the 495 M `is_object_address` calls of that run
        // 57% to `VmHeap::load_and_forward` (the software read barrier, one
        // walk per reference field/array access through `get_field`,
        // `set_field`, `get_array_element`, `set_array_element`) and 17% to
        // `autobox_payload`'s reference-array unbox screen. On a workload like
        // that the candidate is an in-arena address 99.997% of the time, so
        // every call takes the WHOLE body below, not the bounds rejection.
        if addr < self.arena_base || addr >= self.arena_end {
            return None;
        }
        // In-arena candidate: index the single owning region in O(1). Region `k`
        // occupies `[arena_base + k*region_size, +region_size)` and lives at
        // `regions[k]` (the Vec has fixed length and is never reordered; each
        // slot's `data` buffer is address-stable). Only genuine
        // heap-pointer-shaped words reach here, so the lock is taken rarely. The
        // explicit base/cursor bounds re-check below is the authoritative test
        // (and guards against any indexing skew).
        let region_size = self.config.region_size;
        if region_size == 0 {
            return None;
        }
        // F-09, second half. `lookup_region_for_addr` and
        // `classify_candidate_header_view` were converted from `/ region_size`
        // to `>> region_shift` because the divisor is a runtime value and `/`
        // therefore compiles to a real 64-bit `div` -- tens of cycles,
        // unpipelined. THIS site was missed, and it is the hottest of the
        // three: `is_addr_in_live_region` is the whole body of `is_heap_addr`
        // and the gate of `is_object_address`, so every conservative root-scan
        // word, every JIT native-dispatch argument and every typecheck
        // receiver paid one divide. Measured on `org.h2.test.store.
        // TestMVStoreTool`'s create phase (see
        // `docs/feature-designs/concurrent-gc-maturation.md`): 240 M calls in a
        // 12 s run, with `is_addr_in_live_region` the #1 self-time symbol of
        // the whole profile at 14%.
        //
        // `region_shift` is `config.region_size.trailing_zeros()` taken in
        // `new` AFTER `normalize_region_size` rounded the size up to a power of
        // two, so the shift and the divide agree by construction; the
        // `region_size == 0` guard above is what keeps a 64-wide shift
        // unreachable.
        let idx = (addr - self.arena_base) >> self.region_shift;
        debug_assert_eq!(
            idx,
            (addr - self.arena_base) / region_size,
            "region_shift disagrees with config.region_size"
        );
        // Per-thread POSITIVE memo, checked before the lock. See
        // `live_region_memo` for the soundness argument; the epoch is read
        // BEFORE the lock below so a recycle concurrent with this call can only
        // ever produce a memo that is already stale-tagged, never one that
        // outlives its region.
        let epoch = self.rset_cache_epoch.load(Ordering::Acquire);
        if let Some(span) = live_region_memo::hit(self.instance_id, epoch, idx, addr) {
            return Some(span);
        }
        let regions = self.regions.read();
        match regions.get(idx) {
            None => None,
            Some(r) => match r.region_type() {
                // Free regions hold no live object.
                RegionType::Free => None,
                // A humongous object is physically contiguous across its
                // slices; only the `HumongousStart` region carries the full
                // `cursor = object_size`, while every continuation slice keeps
                // `cursor = 0` as a sentinel (see `claim_humongous_span_locked`).
                // The whole continuation slice is therefore live — matching the
                // previous linear scan, which found such interior addresses via
                // the start region's full-span cursor. (A conservative root
                // scan tolerates the harmless over-retention of any tail
                // padding past the object's true end; `is_object_address`
                // rejects every word here by kind: no object starts in a
                // continuation.)
                RegionType::HumongousContinuation => Some(LiveSpan::HumongousContinuation),
                // Eden / Survivor / Old / HumongousStart: live iff the address
                // is below the region's allocation cursor.
                region_type => {
                    let base = r.data.as_ptr() as usize;
                    if addr < base || addr >= base + r.cursor() {
                        return None;
                    }
                    let humongous_start = region_type == RegionType::HumongousStart;
                    let span = if humongous_start {
                        LiveSpan::HumongousStart { base }
                    } else {
                        LiveSpan::Allocated
                    };
                    // G1CORE-3: a kept region with UNRESOLVED evacuation
                    // failures holds exactly the recorded self-forwarded
                    // objects as live — the rest of its below-cursor bytes
                    // are dead garbage the failing pass never scanned, whose
                    // ref slots were never rewritten. Reporting those as
                    // live lets reference processing "restore" a dead weak
                    // referent whose fields dangle into regions freed the
                    // same pause. Both sets are empty except in the rare
                    // wedged-drain window (one lock + one lookup then).
                    if self.kept_unresolved_any.load(Ordering::Acquire)
                        && self.kept_unresolved_regions.lock().contains(&idx)
                    {
                        // Deliberately NOT memoized: liveness here is a
                        // per-ADDRESS question (the recorded self-forwarded
                        // set), not a per-span one, and the memo can only
                        // express a span.
                        return self
                            .kept_unresolved_live
                            .lock()
                            .contains(&addr)
                            .then_some(span);
                    }
                    // Memoize the span this answer came from, so the next
                    // address in the same region — the common case for a
                    // stack scan or a run of native calls, which see the same
                    // Eden over and over — is answered without the lock.
                    live_region_memo::fill(
                        self.instance_id,
                        epoch,
                        idx,
                        base,
                        base + r.cursor(),
                        humongous_start,
                    );
                    Some(span)
                }
            },
        }
    }

    /// Walk all live objects across all non-Free regions.
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut result = Vec::new();
        let regions = self.regions.read();
        let jit_skips = self.jit_tlab_skip_spans();
        for r in regions.iter() {
            if r.region_type() == RegionType::Free {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            let used = r.cursor();
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, base + offset) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
                if let Some(gap) = gap_filler_len(ptr) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: HumongousFiller is a walker sentinel;
                // never report it as a real object.
                if is_humongous_filler(header) {
                    break;
                }
                // `object_total_size` (not an inline recompute): its 0
                // sentinel on a corrupt/overflowing array header stops the
                // walk instead of yielding a HEADER_SIZE-strided phantom.
                let total_size = object_total_size(header);
                if total_size < HEADER_SIZE || offset + total_size > used {
                    break;
                }
                result.push((ptr, total_size));
                offset += total_size;
            }
        }
        result
    }

    /// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
    /// item 4): [`Self::walk_objects`] as a visitor. A heap dump or a class
    /// histogram of a large heap no longer builds a `Vec` of every object
    /// first (16 bytes per object, grown by doubling: on the
    /// `-XX:+HeapDumpOnOutOfMemoryError` path that allocation could itself
    /// fail, and a failed Rust allocation aborts the process). The same steps
    /// and skips as `walk_objects`, object for object. The region table is
    /// read once into `(base, cursor)` pairs and its guard dropped before `f`
    /// runs, so `f` may call accessors that take the region lock themselves.
    /// The same contract: stop-the-world, nothing retypes, fills or moves a
    /// region while the walk runs.
    pub fn for_each_object(&self, f: &mut dyn FnMut(*mut u8, usize)) {
        let spans: Vec<(usize, usize)> = {
            let regions = self.regions.read();
            regions
                .iter()
                .filter(|r| r.region_type() != RegionType::Free)
                .map(|r| (r.data.as_ptr() as usize, r.cursor()))
                .collect()
        };
        let jit_skips = self.jit_tlab_skip_spans();
        for (base, used) in spans {
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, base + offset) {
                    offset += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(ptr) {
                    offset += gap;
                    continue;
                }
                // SAFETY: `offset < used`, inside the region's allocated
                // prefix, with the world stopped (the caller's contract).
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let total_size = object_total_size(header);
                if total_size < HEADER_SIZE || offset + total_size > used {
                    break;
                }
                f(ptr, total_size);
                offset += total_size;
            }
        }
    }

    /// gci i14/hd second pass (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
    /// item 7): [`Self::for_each_object`] minus every object the last
    /// completed, trusted mark proves dead ([`MarkProvenDead`], the oracle the
    /// low-space remembered-set walk and `cleanup` already rely on: an Old
    /// region of the snapshot's incarnation and type, below its TAMS, mark bit
    /// clear, not in the finalizable closure). A live-only heap walk after a
    /// full collection then no longer reports the dead objects of an Old
    /// region the compaction pause skipped (HotSpot's full GC leaves none).
    /// Returns the number of objects skipped, or `None` -- having visited
    /// nothing -- when no such proof exists (a cycle running, none completed,
    /// or its closure not trusted); the caller then walks everything.
    ///
    /// The region table is read once (its guard dropped before `f` runs, as
    /// in `for_each_object`); the regions themselves are then read through
    /// pointers taken under that guard. Stop-the-world only: the table has a
    /// fixed length, is never reordered, and no region is retyped, reset or
    /// re-marked while the world is stopped, so each pointer stays valid and
    /// each region's mark state stays the proof's.
    pub fn for_each_object_skipping_proven_dead(
        &self,
        f: &mut dyn FnMut(*mut u8, usize),
    ) -> Option<u64> {
        let (proof, spans) = {
            let regions = self.regions.read();
            let proof = self.mark_proven_dead(&regions)?;
            let spans: Vec<(usize, usize, usize, *const G1Region)> = regions
                .iter()
                .enumerate()
                .filter(|(_, r)| r.region_type() != RegionType::Free)
                .map(|(i, r)| (r.data.as_ptr() as usize, r.cursor(), i, r as *const G1Region))
                .collect();
            (proof, spans)
        };
        let jit_skips = self.jit_tlab_skip_spans();
        let mut skipped = 0u64;
        for (base, used, idx, region) in spans {
            // SAFETY: see the doc -- the table element outlives this walk
            // with the world stopped.
            let region = unsafe { &*region };
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, base + offset) {
                    offset += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(ptr) {
                    offset += gap;
                    continue;
                }
                // SAFETY: `offset < used`, inside the region's allocated
                // prefix, with the world stopped.
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let total_size = object_total_size(header);
                if total_size < HEADER_SIZE || offset + total_size > used {
                    break;
                }
                if proof.proves_dead(region, idx, base + offset) {
                    skipped += 1;
                } else {
                    f(ptr, total_size);
                }
                offset += total_size;
            }
        }
        Some(skipped)
    }

    /// The class id of every object the regions hold, live or dead, and
    /// whether the walk parsed EVERY non-free region to its cursor. The G1
    /// retained-layout census (`gengc-r5w3-unload7-g1-remark-may-unregister-layouts-of-unswept-instances`):
    /// a class the final remark unloaded keeps its compact layout while any
    /// instance of it is still in a region, because the region walkers size
    /// that instance from the layout. `false` means some stretch could not be
    /// sized, so an absent id proves nothing. Stop-the-world, no heap lock
    /// held. The same steps as [`Self::walk_objects`]; a humongous
    /// continuation's filler ends its region, as there.
    pub fn object_class_ids(&self) -> (FxHashSet<u32>, bool) {
        let mut ids = FxHashSet::default();
        let mut complete = true;
        let regions = self.regions.read();
        let jit_skips = self.jit_tlab_skip_spans();
        for r in regions.iter() {
            if r.region_type() == RegionType::Free {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            let used = r.cursor();
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, base + offset) {
                    offset += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(ptr) {
                    offset += gap;
                    continue;
                }
                // SAFETY: `offset < used`, inside the region's data, with the
                // world stopped (the caller's contract).
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let total_size = object_total_size(header);
                if total_size < HEADER_SIZE || offset + total_size > used {
                    complete = false;
                    break;
                }
                ids.insert(header.class_id.as_u32());
                offset += total_size;
            }
        }
        drop(regions);
        if !complete {
            self.layout_census_incomplete
                .fetch_add(1, Ordering::Relaxed);
        }
        (ids, complete)
    }

    /// Should this pause take the retained-layout census? `retained_total` is
    /// the VM's cumulative count of retained layouts. Granted (and recorded)
    /// once per `(old regions reclaimed, retained_total)` pair: the first pause
    /// after a remark retained a layout (its young collection set frees the
    /// class's dead young instances), then once after each pause that handed
    /// an old region back. Between those nothing a census could see changed,
    /// so a layout pinned by a dead instance in a region no collection takes
    /// costs no walk per young pause.
    pub fn layout_census_due(&self, retained_total: u64) -> bool {
        let key = (
            self.old_regions_reclaimed.load(Ordering::Relaxed),
            retained_total,
        );
        let mut last = self.layout_census_key.lock();
        if *last == key {
            return false;
        }
        *last = key;
        true
    }

    /// `(old regions reclaimed, census walks that could not size an object)`.
    pub fn layout_census_counts(&self) -> (u64, u64) {
        (
            self.old_regions_reclaimed.load(Ordering::Relaxed),
            self.layout_census_incomplete.load(Ordering::Relaxed),
        )
    }
}
// ---------------------------------------------------------------------------
// Field-read primitive shared by the trait impl and the SATB pre-barrier
// ---------------------------------------------------------------------------

impl G1Collector {
    /// `get_field` WITHOUT the auto-box un-wrap — the raw contents of the
    /// slot, reference word and all.
    ///
    /// Two callers, and the split is load-bearing rather than tidy. The trait
    /// `get_field` un-boxes, because a wrapper must never escape to Java. The
    /// SATB pre-barrier must NOT un-box: a boxed primitive is a real object
    /// reachable only from the slot being overwritten, so logging the
    /// primitive instead of the wrapper drops the edge the barrier exists to
    /// keep. The reference-ARRAY store in this same file reads `old_raw` for
    /// exactly that reason (`set_array_element`, G1MAT-3); this is the field
    /// half of the same rule (W7-84-primitive-in-reference-store.md).
    pub fn get_field_raw(&self, obj: ObjectRef, index: usize) -> Value {
        // C2b (round-12 gc): runtime bounds + suspect-header guard, mirroring
        // `GenerationalHeap::get_field` (gen_heap.rs). A corrupted/oversized
        // header or an out-of-layout index must NOT dereference arbitrary
        // memory — return a benign null read instead, matching gen_heap.
        let header = self.get_header(obj);
        // An ARRAY receiver is never a plain-object field access, and neither
        // test below can see it: `alloc_array` MIRRORS the length into
        // `num_slots`, so every real element index is "in bounds" while the
        // byte offset this accessor computes names neither that element nor,
        // past the first few, any part of the allocation. See
        // `heap::refuse_array_receiver_field_access`.
        if header.kind() == ObjectKind::Array {
            // SAFETY: `obj` is a live allocation base and `header` is its
            // header, resolved one line above.
            unsafe {
                cratonvm_gc_core::heap::refuse_array_receiver_field_access(
                    obj.as_ptr() as usize,
                    header,
                    index,
                    "g1::get_field",
                )
            };
            return Value::Object(None);
        }
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                "g1::get_field: suspect header (returning null)",
            );
            return Value::Object(None);
        }
        if index >= num_slots {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                "g1::get_field: out-of-bounds field read dropped (returning null)",
            );
            return Value::Object(None);
        }

        let compact = cratonvm_types::compact_object_field_disp(header, index);
        // HIB-DCAST-LATEPHASE.1 (mutator side). `compact_object_field_storage`
        // returns `None` for TWO different reasons and the `unwrap_or` below
        // treats them as one: "this is a legacy object" (correct — the uniform
        // `index * SLOT_SIZE` 16-byte cell, unchanged) and "this IS a compact
        // object (`GC_FLAG_COMPACT`, set at allocation) whose
        // `(class_id, num_slots)` no longer resolves to a registered layout"
        // (its `class_layout_for_fields(..)?` early return — e.g. a class
        // redefinition racing the layout registry).
        //
        // For the second, `alloc_object` above sized this object's body with
        // `compact_object_body_size`, NOT `num_fields * SLOT_SIZE`, and
        // `num_slots()` on a compact object is the FIELD COUNT — so the
        // `index >= num_slots` screen above does not bound the legacy stride,
        // and the read at `ARRAY_DATA_OFFSET + index * SLOT_SIZE` runs past
        // the allocation. That is the read half of the `SIGSEGV` observed
        // against the real `DefaultCatalogAndSchemaTest` workload.
        //
        // `is_compact_object(header)` is the per-object header bit, read
        // independently of the registry, and is exactly how this collector's
        // own walkers already separate the two cases — see
        // `for_each_flat_object_reference_trusting_header` and the concurrent mark's object
        // arm, both of which then simply skip the object when
        // `with_class_layout` misses. An accessor cannot skip, so it degrades
        // as this function's neighbouring guards do: benign null read, loud
        // `cratonvm::gc::guard` record, no panic. Matches
        // `GenerationalHeap::get_field` (gen_heap.rs), which carries the full
        // rationale and the open `TODO` about the unchecked `layout_domain` on
        // the read path.
        if compact.is_none() && cratonvm_types::is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                "g1::get_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — returning null rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return Value::Object(None);
        }
        // `payload_off` is relative to `ARRAY_DATA_OFFSET` (16), the long-header
        // payload base the humongous translation works in. A compact field's
        // displacement is ABSOLUTE and may lie below 16 (compact fields start
        // at 8), so its relative offset wraps: `ARRAY_DATA_OFFSET.wrapping_add`
        // below recovers the displacement exactly, and `humongous_copy`'s
        // bounds check refuses the wrapped value (a compact instance is never
        // humongous in practice -- it would need tens of thousands of fields).
        let (payload_off, payload_size) = compact
            .map(|(disp, storage)| {
                (
                    disp.wrapping_sub(ARRAY_DATA_OFFSET),
                    storage.size_runtime() as usize,
                )
            })
            .unwrap_or((index * SLOT_SIZE, SLOT_SIZE));
        let total_size = object_total_size(header);

        // C2 (round-12 gc): humongous objects are region-fragmented; translate
        // the flat payload offset to the owning continuation region's buffer so
        // the read can never escape the object's backing memory.
        //
        // `may_be_humongous` first, so an ordinary object's field read never
        // touches the `regions` lock. This block used to take it
        // unconditionally, making every `getfield` on this collector contend
        // with every other thread's field access — see `may_be_humongous`.
        if self.accessor_needs_regions_lock(total_size) {
            let regions = self.regions.read();
            if let Some((start, total_payload)) = self.humongous_span(&regions, obj, total_size) {
                let mut tmp = [0u64; 2];
                if self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    tmp.as_mut_ptr().cast(),
                    payload_size,
                    false,
                ) {
                    return if let Some((_, storage)) = compact {
                        unsafe {
                            cratonvm_types::read_compact_field(
                                tmp.as_ptr().cast(),
                                storage,
                                Ordering::Relaxed,
                            )
                        }
                    } else {
                        let bytes =
                            unsafe { &*(tmp.as_ptr().cast::<u8>() as *const [u8; SLOT_SIZE]) };
                        value_from_bytes(bytes)
                    };
                }
                return Value::Object(None);
            }
        }

        // SAFETY: `index < num_slots` (checked above) so the slot lies within
        // the object's allocated, single-region backing store.
        //
        // PLAIN-SLOT TEARING FIX (2026-07-06): was a bare `ptr::read::<Value>`,
        // a non-atomic 16-byte copy that could tear against a concurrent
        // plain `set_field` from another mutator thread -- see
        // elasticsearch-lucene-binary-docvalues-range-hangs.md
        // #3 and commit 4e6b560f (the GC-marker-vs-JIT-store counterpart fix,
        // which covered g1::scan_object_refs but not this mutator-side path).
        let ptr = unsafe {
            obj.as_ptr()
                .add(ARRAY_DATA_OFFSET.wrapping_add(payload_off))
        };
        if let Some((_, storage)) = compact {
            unsafe { cratonvm_types::read_compact_field(ptr, storage, Ordering::Relaxed) }
        } else {
            // Discriminant-screened, like `gen_heap::read_slot` — see
            // `heap::read_value_cell_checked` for why the unscreened read made
            // one corrupt cell fatal here and merely loud on Generational.
            unsafe { cratonvm_gc_core::heap::read_value_cell_checked(ptr as *const Value, "g1::get_field") }
        }
    }
}

impl GarbageCollector for G1Collector {
    fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        G1Collector::try_alloc_object(self, class_id, num_fields)
    }

    fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        G1Collector::try_alloc_array(self, class_id, element_type, length)
    }

    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        let compact_body = cratonvm_types::compact_object_total_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let total_size = compact_body.unwrap_or(HEADER_SIZE + num_fields * SLOT_SIZE);
        let num_slots = u32::try_from(num_fields).expect("field count exceeds u32::MAX");
        // The header is written by the INITIALIZER -- under the regions lock,
        // before the allocation is published. See
        // `G1Region::bump_alloc_initialized`.
        let (ptr, _region) = self
            .alloc_in_region_initialized(total_size, |ptr| {
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
                // SAFETY: `ptr` is the base of a reserved, unpublished span,
                // 8-aligned and at least `HEADER_SIZE` bytes.
                unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
            })
            .unwrap_or_else(|| {
            if gc_flags().g1_dbg_diag {
                let regions = self.regions.read();
                let mut free = 0usize;
                let mut eden = 0usize;
                let mut survivor = 0usize;
                let mut old = 0usize;
                let mut hstart = 0usize;
                let mut hcont = 0usize;
                for r in regions.iter() {
                    match r.region_type() {
                        RegionType::Free => free += 1,
                        RegionType::Eden => eden += 1,
                        RegionType::Survivor => survivor += 1,
                        RegionType::Old => old += 1,
                        RegionType::HumongousStart => hstart += 1,
                        RegionType::HumongousContinuation => hcont += 1,
                    }
                }
                eprintln!(
                    "DBG-G1DIAG: heap_size={} region_size={} num_regions={} free={} eden={} survivor={} old={} hstart={} hcont={} collection_count={}",
                    self.config.heap_size,
                    self.config.region_size,
                    regions.len(),
                    free, eden, survivor, old, hstart, hcont,
                    self.collection_count.load(Ordering::Relaxed)
                );
            }
            eprintln!(
                "FATAL: G1: out of heap space for object allocation ({} bytes) \
                 -- every region consumed with no collection able to run. This \
                 entry point cannot report failure (see `native_alloc_pressure`), \
                 so the pressure latch is supposed to have forced a collection at \
                 the preceding `safe_native_call` boundary; re-run with \
                 CRATONVM_DBG_G1DIAG=1 to see the region census per collection.",
                total_size
            );
            // gc-common w2-d: name the caller. Every VM allocation ladder uses
            // the fallible `try_*` entry points, so reaching this arm means an
            // infallible `alloc_object` caller that cannot report failure (a
            // native context outside `safe_native_call`, a class mirror, a JNI
            // mint, ...). One run with this line names it; see
            // `docs/feature-designs/concurrent-gc-maturation.md`.
            // Unconditional: the process is about to abort, so the capture
            // costs nothing that matters.
            eprintln!(
                "FATAL: G1 infallible alloc_object caller:\n{}",
                std::backtrace::Backtrace::force_capture()
            );
            std::process::abort();
        });

        // SAFETY: the initializer above wrote a well-formed header at `ptr`.
        unsafe { ObjectRef::from_raw(ptr) }
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        let data_size = array_data_size(length, element_type)
            .expect("array data size overflow in g1 alloc_array");
        let total_size = ARRAY_DATA_OFFSET + data_size;
        let length_u32 = u32::try_from(length).expect("array length exceeds u32::MAX");
        // The header is written by the INITIALIZER -- under the regions lock,
        // before the allocation is published. See
        // `G1Region::bump_alloc_initialized`, and `try_alloc_array` for why an
        // array header is the shape whose torn intermediate state reads back
        // as a legacy object claiming `length` sixteen-byte slots.
        //
        // Mirror `length` into BOTH `array_length` and `num_slots`, matching
        // `Heap::alloc_array` (heap.rs:367-370,403-410) and
        // `GenerationalHeap::alloc_array` (gen_heap.rs:497-500). The shared
        // header decoders in `vm_heap` / `walk_objects` (e.g.
        // `VmHeap::num_fields(arr)`) consume `num_slots` and were silently
        // returning 0 for any G1-allocated array prior to that fix.
        let (ptr, _region) = self
            .alloc_in_region_initialized(total_size, |ptr| {
                // SAFETY: as in `alloc_object`.
                unsafe {
                    std::ptr::write(
                        ptr as *mut ObjectHeader,
                        ObjectHeader::new(
                            class_id,
                            ObjectKind::Array,
                            element_type,
                            length_u32,
                            length_u32,
                        ),
                    );
                }
            })
            .unwrap_or_else(|| {
                eprintln!(
                    "FATAL: G1: out of heap space for array allocation ({} bytes) \
                 -- see the object-allocation abort above for the invariant \
                 this encodes (`native_alloc_pressure`).",
                    total_size
                );
                // Name the caller, as the object arm does since gc-common w2-d
                // (gc-common w5-c, `handoff-w4c-infallible-callers-residue.md`
                // §3). Unconditional: the process is about to abort.
                eprintln!(
                    "FATAL: G1 infallible alloc_array caller:\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
                std::process::abort();
            });

        // SAFETY: the initializer above wrote a well-formed header at `ptr`.
        unsafe { ObjectRef::from_raw(ptr) }
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        unsafe { &*(obj.as_ptr() as *const ObjectHeader) }
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.get_header(obj).class_id
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.get_header(obj).kind()
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.get_header(obj).element_type()
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let header = self.get_header(obj);
        match header.identity_hash(|| match self.next_hash() {
            0 => i32::MAX,
            h => h,
        }) {
            Ok(hash) => hash,
            Err(()) => cratonvm_gc_core::collector::displaced_identity_hash(obj.as_ptr() as usize),
        }
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        // Un-box the wrapper `set_field` installs for a non-reference value
        // stored into a declared-REFERENCE slot. G1 used to hand such a value
        // straight to `write_compact_field`, whose `FieldStorageKind::Reference`
        // arm maps every non-`Object` value to raw 0, so the write was silently
        // dropped to null while `gen_heap` boxed it and the legacy 16-byte cell
        // kept it verbatim (W7-84-primitive-in-reference-store.md).
        //
        // `autobox_payload` is the same validated read the reference-ARRAY path
        // here has always used; `cratonvm_gc_core::autobox` puts it behind the process-wide
        // latch so a run that never boxes never reaches it.
        //
        // gc-common w4-e: that screen is now per slot (`unbox_reference_field`),
        // because the boot-time mirror overlays arm the process-wide latch in
        // every run (common-g-autobox-latch-always-armed). The receiver's class
        // id is read only for a non-null reference, so a primitive or null
        // field pays nothing extra; it is the same `header.class_id`
        // `set_field` passed to `box_for_reference_slot` for this object.
        let v = self.get_field_raw(obj, index);
        if !matches!(v, Value::Object(Some(_))) {
            return v;
        }
        cratonvm_gc_core::autobox::unbox_reference_field(
            v,
            self.get_header(obj).class_id,
            index,
            |r| {
                self.is_object_address(r.as_ptr() as usize)?;
                // SAFETY: `is_object_address` confirmed `r` points at a valid
                // object header inside one of this collector's regions.
                Some(unsafe { (*(r.as_ptr() as *const ObjectHeader)).class_id })
            },
            |r| self.get_field_raw(r, 0),
        )
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        // C2b (round-12 gc): runtime bounds + suspect-header guard. Drop
        // out-of-layout writes rather than corrupting the neighboring object,
        // mirroring `GenerationalHeap::set_field`.
        let header = self.get_header(obj);
        // Symmetric with `get_field_raw`: a 16-byte `Value` cell stamped over
        // packed element data corrupts every element sharing those bytes, and
        // past the first few indices lands outside the allocation. See
        // `heap::refuse_array_receiver_field_access`.
        if header.kind() == ObjectKind::Array {
            // SAFETY: `obj` is a live allocation base and `header` is its
            // header, resolved one line above.
            unsafe {
                cratonvm_gc_core::heap::refuse_array_receiver_field_access(
                    obj.as_ptr() as usize,
                    header,
                    index,
                    "g1::set_field",
                )
            };
            return;
        }
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                value = ?value,
                "g1::set_field: suspect header (dropping write)",
            );
            return;
        }
        if index >= num_slots {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                value = ?value,
                "g1::set_field: out-of-bounds field write dropped",
            );
            return;
        }

        // C2c (round-12 gc): for reference stores, fire the SATB pre-barrier
        // (log the OLD slot value before it is overwritten — only matters while
        // concurrent marking is active) so the marker never loses an edge, and
        // the RSet post-barrier (after the store) so cross-region roots are
        // tracked. This mirrors how `GenerationalHeap::set_field` fires its
        // barrier internally; callers do NOT need to call the barriers
        // separately. Primitive stores skip both barriers.
        //
        // INT-8: `satb_pre_suppressed()` exempts the weak-reference PROTOCOL
        // writes (the pre-pause referent null pass and the remark-time
        // referent clears) — those are not semantic overwrites, and logging
        // them recorded every active referent as a mark root at every
        // mid-cycle young pause, tainting the bitmap verdicts remark-time
        // reference processing depends on. The TLS read is gated behind the
        // marking-active check, so the non-marking hot path pays nothing.
        // W7-84: resolve the layout BEFORE the barrier decisions, because a
        // non-reference value bound for a declared-REFERENCE slot is boxed into
        // an `AUTOBOX_CLASS_ID` wrapper — and once boxed the store IS a
        // reference store, so both barriers have to see the boxed value.
        // Handing it to `write_compact_field` unboxed is what used to drop it
        // to raw 0, i.e. to null, while `gen_heap` boxed it and the legacy
        // 16-byte cell kept it verbatim
        // (W7-84-primitive-in-reference-store.md).
        //
        // The allocation happens here, BEFORE the `regions` lock is taken
        // below — the same ordering constraint `autobox_for_reference_array`
        // records for the array half.
        let compact = cratonvm_types::compact_object_field_disp(header, index);
        let value = match compact {
            Some((_, storage)) if storage.is_reference() => {
                cratonvm_gc_core::autobox::box_for_reference_slot(value, header.class_id, index, |v| {
                    let wrapper = self.alloc_object(cratonvm_gc_core::heap::AUTOBOX_CLASS_ID, 1);
                    self.set_field(wrapper, 0, v);
                    wrapper
                })
            }
            _ => value,
        };

        let is_ref_store = matches!(value, Value::Object(_));
        if is_ref_store && self.satb_pre_barrier_required() && !satb_pre_suppressed() {
            // `get_field_raw`, NOT `get_field`: if the slot currently holds a
            // wrapper, the wrapper is the object the marker must not lose, and
            // `get_field` would hand back the primitive inside it. See
            // `get_field_raw`'s own note.
            let old = self.get_field_raw(obj, index);
            if let Value::Object(Some(old_ref)) = old {
                self.satb_pre_barrier(old_ref.as_ptr() as usize);
            }
        }

        // HIB-DCAST-LATEPHASE.1 (mutator side, write half). See the long note
        // on the matching guard in `get_field` above for why this `None` is
        // two different states and why only the legacy one may reach the
        // `unwrap_or` below. This half is the more damaging: the legacy stride
        // does not merely read past a compact-sized body, it *writes* a
        // 16-byte `Value` cell over whatever follows the object.
        //
        // The SATB pre-barrier above has already run, and in this state its
        // `self.get_field_raw(obj, index)` returned `Value::Object(None)`
        // through that same guard — so no bogus edge was logged, and the second
        // `cratonvm::gc::guard` record it emits for this object is expected.
        if compact.is_none() && cratonvm_types::is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                value = ?value,
                "g1::set_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — dropping the write rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return;
        }
        // `payload_off` is relative to `ARRAY_DATA_OFFSET` (16), the long-header
        // payload base the humongous translation works in. A compact field's
        // displacement is ABSOLUTE and may lie below 16 (compact fields start
        // at 8), so its relative offset wraps: `ARRAY_DATA_OFFSET.wrapping_add`
        // below recovers the displacement exactly, and `humongous_copy`'s
        // bounds check refuses the wrapped value (a compact instance is never
        // humongous in practice -- it would need tens of thousands of fields).
        let (payload_off, payload_size) = compact
            .map(|(disp, storage)| {
                (
                    disp.wrapping_sub(ARRAY_DATA_OFFSET),
                    storage.size_runtime() as usize,
                )
            })
            .unwrap_or((index * SLOT_SIZE, SLOT_SIZE));
        let total_size = object_total_size(header);

        // C2 (round-12 gc): route humongous stores through the region-aware
        // translation so the write can never escape the object's memory.
        //
        // The flat arm is hoisted into a closure so it can run WITHOUT the
        // `regions` lock: `may_be_humongous` settles the question by size, and
        // an ordinary object's field write has no business serializing against
        // every other thread's field access. See `may_be_humongous`.
        //
        // SAFETY (both call sites): `index < num_slots`, so the slot is
        // in-bounds of the object's single-region backing store.
        //
        // PLAIN-SLOT TEARING FIX (2026-07-06): was a bare `ptr::write::<Value>`
        // -- see the matching note on `get_field`'s read side above. The
        // atomic write is what makes the lock-free path safe against a
        // concurrent plain `get_field_raw` on the same slot; the lock never
        // provided that ordering anyway, because the read side took it
        // separately.
        let flat_store = || {
            let ptr = unsafe {
                obj.as_ptr()
                    .add(ARRAY_DATA_OFFSET.wrapping_add(payload_off))
            };
            if let Some((_, storage)) = compact {
                unsafe {
                    cratonvm_types::write_compact_field(ptr, storage, value, Ordering::Relaxed)
                };
            } else {
                unsafe {
                    cratonvm_types::write_value_atomic(ptr as *mut Value, value);                }            }
            true
        };
        let stored = if !self.accessor_needs_regions_lock(total_size) {
            flat_store()
        } else {
            let regions = self.regions.read();
            if let Some((start, total_payload)) = self.humongous_span(&regions, obj, total_size) {
                let mut tmp = [0u64; 2];
                if let Some((_, storage)) = compact {
                    unsafe {
                        cratonvm_types::write_compact_field(
                            tmp.as_mut_ptr().cast(),
                            storage,
                            value,
                            Ordering::Relaxed,
                        )
                    };
                } else {
                    let bytes =
                        unsafe { &mut *(tmp.as_mut_ptr().cast::<u8>() as *mut [u8; SLOT_SIZE]) };
                    value_to_bytes(value, bytes);
                }
                self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    tmp.as_mut_ptr().cast(),
                    payload_size,
                    true,
                )
            } else {
                // `may_be_humongous` said "maybe" on size alone and the region
                // type says otherwise — an ordinary object that happens to be
                // large. Same flat store, just reached with the lock held.
                flat_store()
            }
        };

        if stored && is_ref_store {
            self.post_write_barrier_rset(
                obj,
                match value {
                    Value::Object(Some(r)) => r,
                    // Null store: nothing to record in the RSet.
                    _ => return,
                },
            );
        }
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        // JLS §17.7 atomicity for 16-byte `Value` slots: acquire the per-slot
        // stripe lock so paired writers don't expose a torn (tag, payload) to
        // this read. See `cratonvm_gc_core::collector::volatile_stripe_lock` for the
        // design rationale.
        //
        // Round-9 HIGH-3: restore the bracketing `fence(SeqCst)` pair.
        // Round-8 removed it on the (incorrect) premise that the mutex
        // acquire/release JMM edge subsumed it. It does not: parking_lot's
        // mutex acquire is Acquire-ordered and release is Release-ordered,
        // which gives happens-before WITHIN A SINGLE STRIPE but does NOT
        // establish a global total order across distinct stripes. JLS
        // §17.4.5 requires a total order over all `volatile` accesses
        // (synchronization order), so an IRIW-style observer can otherwise
        // see two volatile writes on different stripes in opposite orders
        // from two reader threads — a JMM violation. The SeqCst fence pair
        // adds the cross-stripe total order the per-stripe Acquire/Release
        // alone cannot supply, on top of which the lock still provides
        // 16-byte slot atomicity.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let _guard = cratonvm_gc_core::collector::volatile_stripe_lock(obj, index);
        let v = self.get_field(obj, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        v
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        // Pair with `get_field_volatile`: the stripe lock makes the 16-byte
        // `Value` write appear atomic to a concurrent volatile reader, and
        // the bracketing SeqCst fences guarantee the JLS §17.4.5 total order
        // across distinct stripes (the lock alone is per-stripe HB only).
        // Round-9 HIGH-3: round-8 removed these fences and reintroduced an
        // IRIW-observable JMM hole — see `get_field_volatile` for details.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let _guard = cratonvm_gc_core::collector::volatile_stripe_lock(obj, index);
        self.set_field(obj, index, value);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        self.get_header(obj).array_length() as usize
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.get_header(obj);
        let len = header.array_length() as usize;
        if index >= len {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        let element_type = header.element_type();
        let elem_size = cratonvm_gc_core::heap::element_byte_size(element_type);
        let payload_off = index * elem_size;

        // Read the raw element bytes (`elem_size` of them) into a fixed buffer.
        // The flat single-region path reads in place; the humongous path
        // translates to the owning continuation region. Either way the read is
        // bounds-confined to the object's own backing memory.
        let mut raw = [0u8; 8]; // largest element is 8 bytes (long/double/ref)
                                // C2: array data_size mirrors HEADER_SIZE + elements; recompute the
                                // total so the humongous span / payload bound is exact.
        let total_size =
            ARRAY_DATA_OFFSET + cratonvm_gc_core::heap::array_data_size(len, element_type).unwrap_or(0);
        // SAFETY: `index < len` so `[payload_off, payload_off+elem_size)` is
        // inside the array's single-region payload.
        let flat_read = |raw: &mut [u8; 8]| {
            let slot_ptr = unsafe {
                obj.as_ptr()
                    .add(ARRAY_DATA_OFFSET.wrapping_add(payload_off))
            };
            unsafe {
                std::ptr::copy_nonoverlapping(slot_ptr, raw.as_mut_ptr(), elem_size);
            }
        };
        // `may_be_humongous` first: an ordinary array's element read must not
        // take the `regions` lock, which this block used to do on every call —
        // see `may_be_humongous` for what that cost.
        if !self.accessor_needs_regions_lock(total_size) {
            flat_read(&mut raw);
        } else {
            let regions = self.regions.read();
            if let Some((start, total_payload)) = self.humongous_span(&regions, obj, total_size) {
                if !self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    raw.as_mut_ptr(),
                    elem_size,
                    false,
                ) {
                    return Err(cratonvm_gc_core::heap::oob_index_code(index));
                }
            } else {
                flat_read(&mut raw);
            }
        }
        let value = array_element_from_bytes(element_type, &raw);
        // Un-box the auto-box wrapper the store side installs for a non-Object
        // value — see `set_array_element`, and `GenerationalHeap::
        // get_array_element` for the same read.
        // Latched for the same reason as the two ZGC sites: `autobox_payload`
        // opens with `is_object_address`, a region/registry probe, and can only
        // answer `Some` if a wrapper was ever created — which is what sets the
        // latch. See `cratonvm_gc_core::autobox::array_read_may_hold_wrapper`.
        if element_type == ArrayElementType::Reference
            && cratonvm_gc_core::autobox::array_read_may_hold_wrapper()
        {
            if let Value::Object(Some(boxed)) = value {
                if let Some(inner) = self.autobox_payload(boxed) {
                    return Ok(inner);
                }
            }
        }
        Ok(value)
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        // A reference array element is a raw 8-byte pointer, so a non-Object
        // `Value` cannot be stored in one directly. Natives across the tree
        // nonetheless use a reference array as a generic `Value` store (the
        // stream pipeline collects `Value::Long`/`Value::Double` into one), and
        // `GenerationalHeap::set_array_element` has always honoured that by
        // auto-boxing into a one-field `AUTOBOX_CLASS_ID` wrapper.
        //
        // G1 did not: its encoder is `Value::Object(Some(r)) => r.as_ptr(),
        // _ => 0`, so a `Value::Long(42)` was written as a null reference and
        // read back as `Value::Object(None)`. `.mapToDouble(...).toArray()`
        // therefore returned all zeros under `-XX:+UseG1GC` and was correct
        // under the default collector, with no GC involved at all.
        //
        // Boxed BEFORE the `regions` lock below: `alloc_object` takes that same
        // lock.
        let Some(value) = self.autobox_for_reference_array(obj, value) else {
            return Err(cratonvm_gc_core::heap::ARRAY_STORE_OUT_OF_MEMORY);
        };
        let header = self.get_header(obj);
        let len = header.array_length() as usize;
        if index >= len {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        let element_type = header.element_type();
        let elem_size = cratonvm_gc_core::heap::element_byte_size(element_type);
        let payload_off = index * elem_size;
        let is_ref = element_type == ArrayElementType::Reference;

        // Encode the element value into a fixed byte buffer.
        let mut raw = [0u8; 8];
        array_element_to_bytes(element_type, value, &mut raw);

        // Write the raw bytes back — flat single-region path or humongous
        // region-translated path. Either way the write is bounds-confined.
        //
        // G1MAT-3: the SATB pre-barrier (read the OLD element, log it, THEN
        // overwrite) now runs inside the SAME `regions` critical section as
        // the store. It used to call the public `get_array_element`, which
        // takes and RELEASES the `regions` lock on its own — so the barrier's
        // read-then-store was split by a full lock release/re-acquire.
        // Widening that window widens the classic SATB lost-update race: two
        // mutators both read old value V, both log V, one stores A and the
        // other stores B; A is then reachable from no logged edge and the
        // marker can miss it. Keeping read and store under one acquisition
        // shrinks the window to the minimum this collector's slot model
        // allows, and drops a redundant `humongous_span` recomputation from
        // every reference-array store.
        //
        // INT-8 parity with `set_field`: honour `satb_pre_suppressed()` so the
        // weak-reference PROTOCOL writes never log an edge as a mark root.
        // `satb_pre_barrier` touches only TLS + the SATB shards (never the
        // `regions` lock), so calling it here cannot deadlock.
        //
        // …but only when the lock buys something. Two things can want it: a
        // humongous span (settled by SIZE alone — see `may_be_humongous`) and
        // the SATB read-then-store pair above. When marking is idle there is no
        // SATB read, so an ordinary array store was taking the one global
        // regions mutex purely to ask a question whose answer its own `len`
        // already determined. That is what made an `int[]` store convoy: 768 ms
        // on one thread against 3172 ms on eight for the same total work
        // (`BarrierProbe primstore`), with ZGC flat.
        let total_size =
            ARRAY_DATA_OFFSET + cratonvm_gc_core::heap::array_data_size(len, element_type).unwrap_or(0);
        let needs_satb_read = is_ref && self.satb_pre_barrier_required() && !satb_pre_suppressed();
        // SAFETY: `index < len` so the slot is inside the array payload.
        let flat_store = || {
            let slot_ptr = unsafe {
                obj.as_ptr()
                    .add(ARRAY_DATA_OFFSET.wrapping_add(payload_off))
            };
            unsafe {
                std::ptr::copy_nonoverlapping(raw.as_ptr(), slot_ptr, elem_size);            }
            true
        };
        // Evaluate the predicate BEFORE the `&&`, not inside it: `&&`
        // short-circuits, so with the census wired through
        // `accessor_needs_regions_lock` a store that takes the lock for SATB
        // would never be counted at all — a funnel with one branch invisible.
        let size_needs_lock = self.accessor_needs_regions_lock(total_size);
        let stored = if !needs_satb_read && !size_needs_lock {
            flat_store()
        } else {
            let regions = self.regions.read();
            let span = self.humongous_span(&regions, obj, total_size);

            if needs_satb_read {
                let mut old_raw = [0u8; 8];
                let read_ok = match span {
                    Some((start, total_payload)) => self.humongous_copy(
                        &regions,
                        start,
                        total_payload,
                        payload_off,
                        old_raw.as_mut_ptr(),
                        elem_size,
                        false,
                    ),
                    None => {
                        // SAFETY: `index < len` so the slot is inside the payload.
                        let slot_ptr = unsafe {
                            obj.as_ptr()
                                .add(ARRAY_DATA_OFFSET.wrapping_add(payload_off))
                        };
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                slot_ptr,
                                old_raw.as_mut_ptr(),
                                elem_size,
                            );
                        }
                        true
                    }
                };
                if read_ok {
                    if let Value::Object(Some(old_ref)) =
                        array_element_from_bytes(element_type, &old_raw)
                    {
                        self.satb_pre_barrier(old_ref.as_ptr() as usize);
                    }
                }
            }

            if let Some((start, total_payload)) = span {
                self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    raw.as_mut_ptr(),
                    elem_size,
                    true,
                )
            } else {
                flat_store()
            }
        };
        if !stored {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }

        // C2c: RSet post-barrier for reference element stores of a non-null ref.
        if is_ref {
            if let Value::Object(Some(target)) = value {
                self.post_write_barrier_rset(obj, target);
            }
        }
        Ok(())
    }

    fn needs_gc(&self) -> bool {
        // Read the cache; one mutator re-scans only every
        // `NEEDS_GC_RECOUNT_INTERVAL` queries. This used to lock `regions` and
        // count Free regions on every call — and `maybe_gc` calls this after
        // every single `new` bytecode.
        // See `G1Collector::free_region_count` for the measurement.
        // `regions` is built once in `new` and never grows or shrinks, so its
        // length is exactly the figure `new` computed — no lock needed to
        // learn it.
        //
        // LANE E wave 2, small finding 7 — through the one accessor, not a
        // fourth private copy of `heap_size / region_size`. This site and
        // `note_region_consumed_locked` / `mixed_phase_has_work` (which use
        // `regions.len()`) agreed only because `new` derives the table length
        // from exactly this expression; that is a latent divergence of the kind
        // that survives a change to how the table is built.
        let table_regions = self.region_table_len();
        // LANE E wave 2 — the trigger measures against the SOFT capacity, not
        // the whole `-Xmx` grid.
        //
        // With `CRATONVM_G1_HEAP_RESIZE` unset these are the same number and
        // the arithmetic below is byte for byte what it was. With it armed,
        // this is the entire mechanism by which the policy acts: raising the
        // target means "you may fill more before I collect", lowering it means
        // "collect sooner and hold less". There is nothing else for a growth
        // rule to widen — every region of the table is already claimable and
        // `claim_free_region*` commits through on demand — which is why the
        // finding's "commit more" reading turned out to be a no-op.
        let total = self.heap_target_regions().min(table_regions.max(1));
        let free_count = if needs_gc_recount_due() {
            self.recount_free_regions()
        } else {
            self.free_region_count.load(Ordering::Relaxed)
        };
        // Free regions ABOVE the soft capacity are not capacity this collector
        // is currently willing to spend, so they must not count toward the free
        // fraction that decides whether it has room. Subtracting them is what
        // makes a lowered target actually trigger sooner rather than merely
        // promise to. `saturating_sub` because the cache can read high for a
        // window after a target drop, and reading it high is the direction that
        // delays a collection rather than storms one.
        let free_count = free_count.saturating_sub(table_regions.saturating_sub(total));
        // Trigger a GC when the Free fraction drops below the (adaptive)
        // threshold. Baseline 25%; raised after a collection that hit
        // evacuation failure so the NEXT pause starts with a to-space pool
        // big enough for its live-young set (see
        // `retry_after_evacuation_failure` — the free pool at trigger time
        // IS the young evacuation's to-space, and a live-young set larger
        // than it self-forwards wholesale every pause).
        //
        // …but only once the young generation holds something a pause could
        // give back. On a heap whose Old regions are full of LIVE data the
        // Free fraction sits under the threshold for good, and a pause of a
        // one- or two-region young generation cannot lift it: the arm then
        // asked for a collection on every allocation poll.
        // `GenR4W4HeapFullThrashProbe -XX:+UseG1GC -Xmx128m` ran 14,823 young
        // pauses in 40 s at a steady 30 Free / 95 Old, each copying the same
        // megabyte of survivors plus the ONE node allocated since the last
        // pause, and never reached its `OutOfMemoryError` -- the G1 twin of
        // `docs/feature-designs/concurrent-gc-maturation.md`.
        // The floor is a quarter of the remaining Free pool, so it tightens to
        // one region as the pool empties, and the evacuation-headroom arm
        // below still bounds the young generation from above.
        // `CRATONVM_G1_FREE_TRIGGER_YOUNG_FLOOR=0` restores the bare arm.
        let pct = self.needs_gc_free_percent.load(Ordering::Relaxed).max(1);
        let young = self.young_region_count.load(Ordering::Relaxed);
        // gci i18/mx: `CRATONVM_G1_YOUNG_SIZES_TO_RESERVE` moves this arm to
        // HotSpot's reserve floor; see `free_fraction_trigger_fires`.
        // The near-full proxy row: both low-Free arms hold off while the
        // reserve is already breached and Eden may still grow
        // (`breached_reserve_defers`).
        let breached_defers = self.breached_reserve_defers(young, free_count, total);
        if free_fraction_trigger_fires(free_count, total, pct, self.young_sizes_to_reserve)
            && self.low_free_trigger_allowed(young, free_count)
            && !breached_defers
        {
            return true;
        }
        // The same floor on the headroom arm: with the Free pool down near the
        // 10% reserve, one young region would trip it on every poll too.
        if young > 0
            && self.low_free_trigger_allowed(young, free_count)
            && g1_evac_headroom_trigger()
            && !breached_defers
            && self.young_exceeds_evac_headroom(young, free_count, total)
        {
            return true;
        }
        // G1AUD-9 — the young half of the trigger: collect once the young
        // generation reaches its adaptive pause-target size, even while the
        // Free pool is still comfortable. Without this, `max_gc_pause_ms`
        // bounded only the OLD half of a mixed collection set and young pause
        // time scaled with `-Xmx`. See `young_target_regions`.
        //
        // Two relaxed loads, on the path `maybe_gc` runs after every allocation
        // bytecode; both counters are maintained by the same region-consumption
        // pass that already maintained the Free count.
        if self.external_gpu.should_trigger_gc() {
            return true;
        }
        if !gc_flags().g1_young_pause_target {
            return false;
        }
        let target = self.young_target_regions.load(Ordering::Relaxed);
        if target == 0 {
            return false;
        }
        // The target must have been TIGHTENED by a measured overrun before it
        // is allowed to trigger anything.
        //
        // MEASURED (2026-08-18, `probes/G1ChurnPauseProbe 64 300`, `-Xmx2048m`,
        // 1.2 GiB of garbage): without this test the ceiling ITSELF was a
        // trigger. 60% of 2048 regions is 1228, and the free-pool trigger does
        // not fire until 1536 regions are consumed — so on a heap with headroom
        // the cap manufactured a 210 ms pause in a run that otherwise took NONE,
        // for a 20% wall-clock cost and no latency to gain (there were no pauses
        // to shorten). That is the exact failure the field doc claims cannot
        // happen: "it STARTS at the ceiling, so a workload already meeting its
        // goal never leaves it and sees no change". The claim was right about
        // the target and wrong about the trigger.
        //
        // Below the ceiling, `update_young_target` has seen a productive pause
        // exceed `max_gc_pause_ms` — the collector's own evidence that this
        // workload's young generation is too big for its goal.
        let (_min, max) = self.young_region_bounds();
        // gci i14/yt — under `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL=1` the
        // young generation is measured as HotSpot's young list would hold it:
        // Eden claimed since the last pause plus the packed Survivor
        // footprint. Pinned-kept regions drop out (HotSpot G1 retypes a pinned
        // young region Old at the end of the pause) and so do the sparse
        // per-worker Survivor destinations. Subsumes the net-of-pins arm.
        //
        // gci i43/gy: under an OPERATOR's ceiling (`-Xmn`, `-XX:MaxNewSize`,
        // `-XX:NewRatio`) the ceiling is not CratonVM's slack but HotSpot's
        // maximum young length, and HotSpot collects when Eden reaches the
        // target there too: the arm fires at the ceiling, never before one
        // Eden region has been claimed since the last pause
        // ([`binding_young_arm_fires`], HotSpot's `absolute_min_young_length`).
        let binding = self.young_ceiling_binding();
        if self.young_target_hotspot_model {
            let survivor_equiv = self.young_survivor_equiv_regions.load(Ordering::Relaxed);
            let counted = hotspot_young_arm_count(
                young,
                self.young_after_pause_regions.load(Ordering::Relaxed),
                survivor_equiv,
            );
            if binding {
                return binding_young_arm_fires(counted, target, survivor_equiv);
            }
            return young_target_arm_fires(counted, target, max, 0);
        }
        // gci i7/g1y — net of the regions the last pause could not collect,
        // under `CRATONVM_G1_YOUNG_TARGET_NET_OF_PINS=1`; see
        // `young_target_arm_fires`.
        let pinned_kept = if self.young_target_net_of_pins {
            self.young_pinned_kept_regions.load(Ordering::Relaxed)        } else {
            0
        };
        if binding {
            return binding_young_arm_fires(
                young.saturating_sub(pinned_kept),
                target,
                self.young_after_pause_regions
                    .load(Ordering::Relaxed)
                    .saturating_sub(pinned_kept),
            );
        }
        young_target_arm_fires(young, target, max, pinned_kept)
    }

    fn collect_garbage(
        &self,
        _stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcPause);
        // Phase 6 #1: defer until every live `SafepointToken` has
        // dropped — see `vm_heap::wait_for_gpu_critical_drain` for
        // the rationale. No-op when `gpu-offload` is off.
        let _gpu_drain = cratonvm_gc_core::gpu_critical::wait_for_gpu_critical_drain();
        // gci i27/gr: no per-object referent verdict until an evacuation pause
        // of THIS collection records itself (a refused pause records nothing,
        // and its post-pause verdict stays region-granular; since gci i32/ec an
        // empty-CSet pause that pinned out every young region arms it through
        // `note_refproc_empty_cset_pause`).
        self.reset_refproc_kept_young_token();
        // The G1 near-full proxy-define page: a slide belongs to the pause
        // that made it (`last_pause_slid`).
        self.last_pause_slid.lock().clear();
        // r23 w6: count this call's self-forwards (`self_forward_tracking`).
        struct SelfForwardTrackingScope<'a>(&'a AtomicBool);
        impl Drop for SelfForwardTrackingScope<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        self.self_forward_count.store(0, Ordering::Release);
        self.self_forwarded_addrs.lock().clear();
        self.clear_peer_object_pins();
        self.self_forward_tracking
            .store(true, Ordering::Release);
        let _self_forward_scope = SelfForwardTrackingScope(&self.self_forward_tracking);
        // A collection's condemned overlay owners belong to that collection
        // (`take_overlay_condemned_owners`).
        self.overlay_collection_begin();
        // gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip): the
        // `System.gc()` door's pause tenures every live young object, as
        // HotSpot's Full GC compacts all of them into old regions. Read from
        // the door's thread-local request (this pause runs on the requesting
        // thread, like the Generational major request), held for exactly this
        // call, cleared on every exit by the guard. Unarmed: always false, and
        // `evac_tenuring_threshold` is `tenuring_threshold`.
        struct PauseTenuresAllScope<'a>(&'a AtomicBool);
        impl Drop for PauseTenuresAllScope<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
        }
        // The near-full proxy row: so does a pause that starts with the
        // to-space reserve already breached (`breached_reserve_tenures`).
        self.pause_tenures_all.store(
            cratonvm_gc_core::gc_quiescence::system_gc_tenure_requested() || self.breached_reserve_tenures(),
            Ordering::Relaxed,
        );
        let _pause_tenures_all = PauseTenuresAllScope(&self.pause_tenures_all);

        // ----- Invariant: a published skip span holds NO ROOT ---------------
        //
        // The G1 twin of the generational sweep's check. G1's walks consume
        // the same published list, so a stale span hides live objects from
        // this collector too, and the netty `ObjectCleanerTest` reclamation
        // reproduced 8/8 here as well. G1 holds the spans as ABSOLUTE
        // `[start, end)` pairs rather than young-from offsets, which is the
        // only difference; see `cratonvm_gc_core::heap::skip_spans_hold_no_root`.
        //
        // This is the once-per-collection site that has the roots in hand,
        // before any of them are followed.
        cratonvm_gc_core::heap::skip_spans_hold_no_root(&self.jit_tlab_skip_spans(), &roots[..], "g1");

        // gci i29/cv2 — opt-in `CRATONVM_G1_DBG_FLAGLESS_SWEEP`: what the
        // mutator (and remark/cleanup) left since the last sweep.
        if g1_dbg_flagless_sweep() {
            let regions = self.regions.read();
            self.flagless_header_sweep(&regions, "pause-start", None);
        }

        // 1. Check if mixed GC is needed. Bracket the inner collection in an
        //    `Instant` so we can feed the actual pause delta (milliseconds)
        //    into `update_ihop` below. Previously this site passed
        //    `bytes_freed` to `update_ihop`, which expects milliseconds and
        //    compares against `config.max_gc_pause_ms` — feeding a byte
        //    count (typically 10^4-10^8) caused the adaptive IHOP threshold
        //    to floor on essentially every collection.
        if gc_flags().g1_dbg_diag {
            let regions = self.regions.read();
            let mut free = 0usize;
            let mut eden = 0usize;
            let mut survivor = 0usize;
            let mut old = 0usize;
            for r in regions.iter() {
                match r.region_type() {
                    RegionType::Free => free += 1,
                    RegionType::Eden => eden += 1,
                    RegionType::Survivor => survivor += 1,
                    RegionType::Old => old += 1,
                    _ => {}
                }
            }
            let pinned = regions.iter().filter(|r| r.pinned).count();
            eprintln!(
                "DBG-G1DIAG-PRE: collection #{} regions={} free={} eden={} survivor={} old={} pinned={}",
                self.collection_count.load(Ordering::Relaxed),
                regions.len(), free, eden, survivor, old, pinned
            );
        }
        // Name the backend in the process-wide decision report, AND whether
        // this pause was allowed to evacuate at all. G1 normally has no
        // young-moving *choice* to record — its collection set is always
        // evacuated — but it does have one refusal, and a report that stays
        // silent under `-XX:+UseG1GC` is exactly how the `docs/GC.md` drift
        // went unnoticed for the generational path.
        //
        // The refusal: this collection's JIT root set may be incomplete, so
        // no region can be proven free of an object whose only reference the
        // root scan never saw. See
        // [`Self::root_coverage_incomplete_reason`] for why
        // `jit_pinned_region_set` alone does not cover this and what the
        // empty-CSet fail-safe costs.
        let coverage_incomplete = Self::root_coverage_incomplete_reason();
        // Counted in BOTH arms — see `root_coverage_incomplete_reason`.
        cratonvm_gc_core::gc_metrics::record_g1_pause_coverage_reason(coverage_incomplete);
        // Only the REFUSAL is gated, and it is OFF by default — see
        // `root_coverage_incomplete_reason` for the measurement that says why.
        // ... or the take-over licence (gc-common w3-g; opt-in
        // `CRATONVM_G1_TAKEOVER_LICENCE`, see `gc_quiescence::g1_takeover_licence_refusal`).
        let refuse = Self::refuse_evacuation(coverage_incomplete, gc_flags().g1_coverage_pin)
            .or_else(cratonvm_gc_core::gc_quiescence::g1_takeover_licence_refusal);
        // The second, NARROW detection: a compiled frame is live and the
        // conservative scan published nothing to pin, so `pin_addrs=0` cannot be
        // read as "no JIT roots". Counted on EVERY run; acting on it is opt-in
        // (`CRATONVM_G1_PIN_EMPTY_PUBLICATION`) because refusing a pause whose
        // publication is persistently empty starves the heap rather than saving
        // it — see `empty_jit_publication` for the measurement.
        let empty_publication = self.empty_jit_publication();
        if empty_publication {
            cratonvm_gc_core::gc_metrics::record_g1_pause_empty_jit_publication();
            // Warn on the DETECTION, not the refusal: this is the state in
            // which G1 evacuated a live JIT-held object, and it is worth saying
            // so even on a run that goes on to evacuate anyway. First eight,
            // then powers of two.
            let n = EMPTY_JIT_PUBLICATION_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] pause #{n} ran with a live compiled frame and an EMPTY JIT \
                     root publication: `pin_addrs=0` here means the conservative scan \
                     found none, not that there are none, so this pause evacuated \
                     against a root set it could not prove. The scan is what needs \
                     repairing; CRATONVM_G1_PIN_EMPTY_PUBLICATION only declines the \
                     pause, which on a persistently-empty publication trades the wrong \
                     answer for an OutOfMemoryError."
                );
            }
        }
        let refuse_empty_publication = Self::refuse_evacuation_for_empty_publication(
            empty_publication,
            gc_flags().g1_pin_empty_publication,
        );
        if gc_flags().g1_dbg_pins && refuse_empty_publication {
            eprintln!(
                "[g1][PINS] pause REFUSED: jit_active={} unregistered_frame={} \
                 pin_addrs=0 tlab_skips=0 -> empty CSet",
                cratonvm_gc_core::gc_quiescence::is_active(),
                cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_on_stack(),
            );
        }
        // Which refusal names this pause in the decision report. The
        // empty-publication reason wins when both apply: it is the strictly
        // more specific statement, and the coverage lever (opt-in, ~100% rate)
        // subsumes this case whenever it is on, so recording IT here would hide
        // every real instance behind a reason that means almost nothing.
        // A device DMA against the heap arena that the bounded GPU
        // critical-section wait could not outlast. G1 has no non-moving
        // sweep to divert to, so the fail-safe is the same one the two
        // refusals above use: an empty collection set, every object at its
        // address. See `vm_heap::gpu_relocation_forbidden`.
        let gpu_refuse = self.external_gpu.gpu_relocation_forbidden();
        let (decision, degraded_reason_bit) = if refuse_empty_publication {
            (
                cratonvm_gc_core::gc_metrics::decision_reason::NON_MOVING_G1_EMPTY_JIT_PUBLICATION,
                cratonvm_gc_core::gc_metrics::g1_degraded::JIT_PUBLICATION_EMPTY,
            )
        } else if refuse.is_some() {
            (
                cratonvm_gc_core::gc_metrics::decision_reason::NON_MOVING_G1_ROOT_COVERAGE_INCOMPLETE,
                cratonvm_gc_core::gc_metrics::g1_degraded::ROOT_COVERAGE_INCOMPLETE,
            )
        } else if gpu_refuse {
            (
                cratonvm_gc_core::gc_metrics::decision_reason::NON_MOVING_G1_GPU_CRITICAL,
                cratonvm_gc_core::gc_metrics::g1_degraded::NONE,
            )
        } else {
            (
                cratonvm_gc_core::gc_metrics::decision_reason::MOVING_BACKEND_ALWAYS_EVACUATES,
                cratonvm_gc_core::gc_metrics::g1_degraded::NONE,
            )
        };
        cratonvm_gc_core::gc_metrics::record_collector_decision(
            "g1",
            decision,
            coverage_incomplete.unwrap_or(cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE),
        );
        if refuse.is_some() || refuse_empty_publication || gpu_refuse {
            cratonvm_gc_core::gc_metrics::record_g1_cycle(
                cratonvm_gc_core::gc_metrics::g1_cycle_kind::YOUNG,
                0,
                0,
                0,
                0,
                cratonvm_gc_core::gc_metrics::g1_degraded::EMPTY_COLLECTION_SET | degraded_reason_bit,
            );
            self.native_alloc_pressure.store(false, Ordering::Relaxed);
            self.external_gpu.on_gc_cycle_end();
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map: cratonvm_types::PointerMap::default(),
            };
        }
        let pause_start = std::time::Instant::now();
        let young_bytes_before =
            self.young_region_count.load(Ordering::Relaxed) * self.config.region_size;
        // gci i20/ys (`gci-i19ys-g1-young-sizing-residuals-FIXED-20261017` item 1,
        // opt-in `CRATONVM_G1_YOUNG_SURVIVAL_YOUNG_ONLY`): the survival
        // sample's young-only test (no branch below entered a mixed /
        // compaction pause) and its denominator (Eden handed out since the
        // last pause plus the packed survivor footprint the last census left,
        // never above the gross count). Read before the pause: the pause
        // resets the Eden byte count (`note_pause_productivity`) only after
        // the sample, but the census is rewritten by the record.
        let mixed_entered_before = self.mixed_pauses_entered.load(Ordering::Relaxed);
        let young_survival_net_before = young_survival_denominator(
            young_bytes_before,
            self.eden_bytes_since_pause.load(Ordering::Relaxed),
            self.young_survivor_equiv_regions.load(Ordering::Relaxed),
            self.config.region_size,
        );
        // LANE W6-M — the refusal census. One relaxed add per pause, on a path
        // that has already stopped the world. See `mixed_gate_polls`.
        self.mixed_gate_polls.fetch_add(1, Ordering::Relaxed);
        // A fresh full cycle armed this only for a failed humongous request
        // with enough total Free regions but no adequate run. It must bypass
        // the ordinary mixed gate: that gate intentionally refuses nearly-full
        // Old regions and low aggregate waste, exactly the blockers a full
        // compaction is allowed to move on the OOME path.
        let compacting_humongous = self.pending_humongous_compaction.lock().is_some();
        // gcd d10/o: an armed full compaction is consumed by this pause either
        // way; a humongous-window compaction takes precedence over it.
        let full_compaction =
            self.full_compaction_armed.swap(false, Ordering::AcqRel) && !compacting_humongous;
        // JIT round 22 wave 11: a maximal arm this pause does not run (a
        // humongous window plan won it, or no arm) is dropped with it.
        if !full_compaction {
            self.full_compaction_maximal.store(false, Ordering::Release);
        }
        // r23 w6 (`CRATONVM_G1_STARVED_FULL`): the starved full's arm rides
        // the full-compaction arm, and is dropped with it.
        let starved_full = self.starved_full_armed.swap(false, Ordering::AcqRel) && full_compaction;
        self.starved_full_this_pause.store(starved_full, Ordering::Relaxed);
        let in_place_only =
            self.in_place_only_armed.swap(false, Ordering::AcqRel) && full_compaction;
        self.in_place_only_this_pause.store(in_place_only, Ordering::Relaxed);
        let fills_free =
            self.in_place_fills_free_armed.swap(false, Ordering::AcqRel) && in_place_only;
        self.in_place_fills_free_this_pause.store(fills_free, Ordering::Relaxed);
        // gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018`
        // item 4; `CRATONVM_G1_SURVIVOR_REGION_CAP`, default ON): HotSpot's
        // `_max_survivor_regions`, `ceil(young target / SurvivorRatio)` at
        // pause start, for an evacuation pause (young or mixed) and its drain
        // passes; a compaction pause (humongous window or the staged full
        // compaction, HotSpot's full GC) is not capped. Disarmed after the
        // drain below.
        let survivor_cap_armed =
            !compacting_humongous && !full_compaction;
        if survivor_cap_armed {
            // gci i43/gy: at this collector's `-XX:SurvivorRatio` (8 unless typed).
            // g1ep6: of HotSpot's young target, and no more than the Free pool
            // (`_max_survivor_regions = MIN2(desired, num_free_or_available_regions)`).
            self.arm_survivor_region_cap(
                g1_max_survivor_regions_at(
                    self.young_target_within_free(),
                    self.survivor_ratio.load(Ordering::Relaxed),
                )
                .min(self.free_region_count.load(Ordering::Relaxed)),
            );
        }
        let result = if compacting_humongous {
            self.mixed_pauses_entered.fetch_add(1, Ordering::Relaxed);
            self.mixed_collection(roots, monitors)
        } else if full_compaction {
            self.mixed_pauses_entered.fetch_add(1, Ordering::Relaxed);
            self.full_compaction_pauses.fetch_add(1, Ordering::Relaxed);
            self.full_compaction_this_pause
                .store(true, Ordering::Relaxed);
            let r = self.mixed_collection(roots, monitors);
            self.full_compaction_this_pause
                .store(false, Ordering::Relaxed);
            self.full_compaction_maximal.store(false, Ordering::Release);
            r
        } else if self.needs_mixed_gc() {
            if self.prepare_mixed_pending.swap(false, Ordering::Relaxed) {
                // gci i11/go (finding 9, `CRATONVM_G1_PREPARE_MIXED`): HotSpot's
                // `Prepare Mixed` -- the first pause after cleanup is young-only.
                // Only the switch arm ever arms the flag; with it the gate
                // census reads `polls == no_cycle + no_work + entered +
                // prepare_mixed`.
                self.prepare_mixed_pauses.fetch_add(1, Ordering::Relaxed);
                self.young_collection(roots, monitors)
            } else if self.mixed_phase_has_work() {
                self.mixed_pauses_entered.fetch_add(1, Ordering::Relaxed);
                self.mixed_collection(roots, monitors)
            } else {
                self.mixed_gate_no_work.fetch_add(1, Ordering::Relaxed);
                // Ten-findings item 2: nothing old is worth collecting, so end
                // the mixed phase now instead of spending the rest of its
                // budgeted pauses as young pauses in disguise.
                self.end_mixed_phase("no old region is worth collecting");
                self.young_collection(roots, monitors)
            }
        } else {
            self.mixed_gate_no_cycle.fetch_add(1, Ordering::Relaxed);
            self.young_collection(roots, monitors)
        };
        let result = self.retry_after_evacuation_failure(result, roots, monitors);
        // g1pin: the object pins belong to the young or mixed pass and its
        // retry; the in-place compaction below pins by region.
        self.clear_peer_object_pins();
        // The G1 near-full proxy-define page: an armed full compaction whose
        // copying pass left the Free pool starved slides the movable Old
        // regions in place (`g1_compact_in_place.rs`), as HotSpot's full
        // collection does; the two passes' forwards compose as the retry's do.
        let result = if full_compaction {
            self.full_compaction_in_place_after(result, roots, monitors)
        } else {
            result
        };
        // The G1 near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_FULL`):
        // a breached heap down to one Eden region compacts next.
        self.note_breached_reserve_full(full_compaction);
        if survivor_cap_armed {
            self.disarm_survivor_region_cap();
        }
        // gci i20/ys: on `CRATONVM_G1_YOUNG_SURVIVAL_YOUNG_ONLY` only a young-only
        // pause samples the young survival ratio, over the bytes young
        // actually held (`young_survival_sample`); the default keeps the gross
        // whole-region denominator and every non-humongous-compaction pause.
        let young_only = self.mixed_pauses_entered.load(Ordering::Relaxed) == mixed_entered_before;
        // The G1 near-full proxy row (`CRATONVM_G1_FULL_COMPACTION_SKIPS_SURVIVAL`):
        // a full compaction's moved Old bytes are not young survival.
        let compaction_pause =
            compacting_humongous || (full_compaction);
        if let Some(denominator) = young_survival_sample(
            g1_young_survival_young_only(),
            compaction_pause,
            young_only,
            young_bytes_before,
            young_survival_net_before,
        ) {
            self.note_young_survival(denominator, result.stats.bytes_copied);
        }
        // THE DRAIN THAT COVERS EVERY PAUSE PATH.
        //
        // `report_pending_corrupt_holders` is also called from five pause
        // bodies, which is where it can run with the regions lock already held
        // and answer soonest. That is not the same as covering every path:
        // `g1-eight-byte-write-at-a-live-objects-base-20260906` measured
        // **0 grid verdicts against 14 corrupt cells** on
        // `TestHostConfigAutomaticDeploymentXmlExternalWarXml` at `-Xmx2g`,
        // because that class's reports arrive from five walks on a pause path
        // that reaches none of them -- so the ONE question the page most needed
        // answered went unanswered for an instrument reason rather than a
        // defect one, and the same drain emits eight verdicts a run on H2.
        //
        // This site is the pause's single funnel: every young, mixed and
        // retry path returns through it. Draining here costs a lock acquire on
        // an empty vector per pause and makes "a corrupt cell was reported and
        // no verdict followed" mean the crash beat the drain, which is a fact
        // about the run rather than about the wiring.
        {
            let regions = self.regions.read();
            self.publish_old_tail_bytes(&regions);
            self.report_pending_corrupt_holders(&regions);
            // gci i29/cv2 — opt-in `CRATONVM_G1_DBG_FLAGLESS_SWEEP`: what this
            // pause (young, mixed, compaction or its retry) produced.
            self.flagless_header_sweep(&regions, "pause-end", Some(&result.pointer_map));
        }
        // The empty-header grid-proof budget is per PAUSE; this is the funnel
        // every pause returns through. See
        // `EMPTY_HEADER_PROOF_BUDGET_PER_PAUSE`.
        EMPTY_HEADER_PROOFS_THIS_PAUSE.store(0, Ordering::Relaxed);
        // One pause's worth of writes; the ledger answers questions about the
        // pause that made them and a run-long map would be gigabytes.
        if let Some(w) = REF_WRITE_WATCH.get() {
            w.writes.lock().clear();
        }
        let pause_ms = pause_start.elapsed().as_millis() as u64;
        if gc_flags().g1_dbg_diag {
            eprintln!(
                "DBG-G1DIAG-POST: objects_copied={} bytes_copied={} bytes_freed={}",
                result.stats.objects_copied, result.stats.bytes_copied, result.stats.bytes_freed
            );
        }

        // 2. IHOP / concurrent-mark triggering is driven by the VM layer
        //    (`interpreter::maybe_concurrent_gc` -> `g1_concurrent_mark_cycle`),
        //    which has the thread + STW-barrier context to run the FULL cycle:
        //    brief-STW initial mark, root marking, the background marker, and
        //    the completion watcher that flips into mixed GC.
        //
        //    This site used to ALSO call `start_concurrent_mark()` here, but
        //    that only flips the phase Idle -> ConcurrentMark (activates SATB,
        //    clears bitmaps) WITHOUT spawning the marker or marking roots. Run
        //    first (inside collect_garbage), it left `is_marking_active()` true,
        //    so the VM's `should_start && !is_marking_active` gate then BLOCKED
        //    the real cycle forever — the phase was stuck in ConcurrentMark,
        //    concurrent marking never actually ran, and so mixed GC never fired
        //    and old-gen was never reclaimed (a major cause of G1's footprint
        //    gap). The gc crate has no thread/barrier context to run the real
        //    cycle, so triggering belongs to the VM layer alone; this premature
        //    phase-flip is removed.

        // 3. Adaptive IHOP.
        //
        //    F-15: pause time no longer moves the MARKING threshold. It is a
        //    property of the young live set and says nothing about whether the
        //    concurrent cycle started early enough — it does still drive the
        //    young-generation size, which is the thing it actually describes
        //    (`update_young_target`, called from `record_collection_with_phases`).
        //    The marking threshold is now recomputed from the measured mark
        //    duration and old-generation growth rate at each cycle boundary
        //    (`note_mark_cycle_end`) and tightened on to-space exhaustion
        //    (`note_to_space_exhausted`, from Phase 5).
        //
        //    `CRATONVM_G1_ADAPTIVE_IHOP=0` restores the pause-time model.
        if pause_ms > 0 && !gc_flags().g1_adaptive_ihop {
            self.update_ihop(pause_ms);
        }

        // 4. The Free pool has just been rebuilt, so any outstanding
        //    native-allocation pressure request has been served. Clear the
        //    latch; the next region claim re-latches it if the workload is
        //    still outrunning the collector (see `native_alloc_pressure`).
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
        self.external_gpu.on_gc_cycle_end();

        // 5. …and that rebuilt pool is exactly what `needs_gc` reads. Republish
        //    it here, or the cache keeps answering with the pre-collection
        //    count and every allocation until the next backstop re-scan asks
        //    for another collection. See `G1Collector::free_region_count`.
        self.recount_free_regions();
        self.note_pause_productivity(result.stats.bytes_freed);
        // G1AUD-9: the regions this pause freed are behind the allocation
        // cursor (the CSet is the young generation, which the last cycle filled
        // front-to-back), so restart the Free search at the head. Search order
        // only — see `reset_free_scan_hint`.
        self.reset_free_scan_hint();

        result
    }

    fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        // Post-write barrier: track cross-region references in remembered sets.
        //
        // PRECONDITION (SATB pre-barrier): when `satb_pre_barrier_required()`
        // (gc-common w5-e: the SATB queue is active — the barrier's one gate,
        // no longer the phase) returns true, the caller MUST have invoked
        // `VmHeap::satb_barrier(old_slot_value)` BEFORE performing the store
        // whose result is being signalled here. SATB needs the *old* slot
        // value to be logged before it is overwritten, and the trait shape
        // (post-store hook) cannot recover that value after the fact. See
        // `GarbageCollector::write_barrier` doc on the trait for the full
        // contract.
        //
        // Best-effort assertion: in debug builds, fire if the marking phase
        // is active and this thread has not invoked `satb_pre_barrier` since
        // its last `write_barrier` call. We cannot mechanically check that
        // the caller logged the *correct* old value (the old value is gone
        // by the time we get here) — only that *some* pre-call happened on
        // the same thread between consecutive post-store hooks. Misses are
        // false-positives on the very first store after marking activates;
        // they are still useful for surfacing call sites that need auditing
        // for SATB callsite coverage.
        debug_assert!(
            !self.satb_pre_barrier_required() || consume_satb_pre_barrier_epoch(),
            "G1 write_barrier invoked while concurrent marking is active without a \
             corresponding SATB pre-barrier on this thread. The trait contract \
             requires callers to invoke `VmHeap::satb_barrier(old_value)` BEFORE \
             the reference store. See `GarbageCollector::write_barrier` doc and \
             `G1Collector::satb_pre_barrier`."
        );

        if let Value::Object(Some(ref target)) = stored_value {
            self.post_write_barrier_rset(obj, *target);
        }
    }

    /// Task #25: G1 implements the SATB pre-store barrier by enqueueing
    /// the old reference into the global SATB log via the per-thread
    /// buffer. Inactive when concurrent mark is idle — `satb_pre_barrier`
    /// short-circuits on the `is_active()` Acquire load.
    ///
    /// `slot` is currently unused; we keep it in the trait signature so
    /// the debug triad-assertion in callers can pair a `(pre, post)`
    /// barrier by slot identity without breaking the API later.
    #[inline]
    fn write_barrier_pre(&self, _slot: *mut ObjectRef, old: ObjectRef) {
        self.satb_pre_barrier(old.as_ptr() as usize);
    }

    fn allocated_bytes(&self) -> usize {
        let regions = self.regions.read();
        regions.iter().map(|r| r.cursor()).sum()
    }
}

// ---------------------------------------------------------------------------
// SATB pre-barrier debug tracking
// ---------------------------------------------------------------------------
//
// Per-thread flag bumped by `G1Collector::satb_pre_barrier` and consumed by
// the debug-only assertion in `G1Collector::write_barrier`. This is purely a
// best-effort detector for missing SATB pre-calls — the post-store trait
// shape cannot see the old slot value, so we cannot mechanically verify
// correctness here. What we *can* do is detect the obvious bug where a
// caller invokes the post-store barrier while marking is active without
// having issued any pre-barrier on the same thread.
//
// Compiled to a no-op in release builds (only the `debug_assert!` consumer
// references these helpers, and the body of `consume_satb_pre_barrier_epoch`
// is trivially DCE-able when the assertion is stripped).

thread_local! {
    pub static SATB_PRE_BARRIER_FLAG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark that this thread has invoked the SATB pre-barrier; the next
/// post-store `write_barrier` call will consume the flag.
///
/// LANE-C — the `cfg!` is not belt-and-braces, it is what makes the module
/// header's "compiled to a no-op in release builds" claim TRUE. The claim
/// rested on the flag's only reader being inside a `debug_assert!` and the
/// store therefore being dead — but a `thread_local!` `Cell` store is a store
/// to a TLS slot whose address escapes through `LocalKey::with`, and nothing
/// obliges LLVM to prove it dead across the whole crate. This sits on
/// `satb_pre_barrier`, which is on the reference-store write barrier, which is
/// the hottest non-allocating path the collector owns; a TLS store there is
/// worth exactly as much as the assertion it feeds, which in release is
/// nothing. `cfg!` (rather than `#[cfg]`) keeps both arms type-checked.
#[inline]
pub fn bump_satb_pre_barrier_epoch() {
    if !cfg!(debug_assertions) {
        return;
    }
    SATB_PRE_BARRIER_FLAG.with(|c| c.set(true));
}

/// Consume the per-thread SATB pre-barrier flag and return whether one was
/// observed since the last call. Used only by the debug-only assertion in
/// `write_barrier`.
#[inline]
pub fn consume_satb_pre_barrier_epoch() -> bool {
    SATB_PRE_BARRIER_FLAG.with(|c| c.replace(false))
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Assemble the [`cratonvm_gc_core::gc_metrics::g1_degraded`] mask for one evacuation
/// pause from the facts the pause already computed.
///
/// An identity entry (`key == value`) in the forwarding map IS the definition
/// of an evacuation failure: `evacuate_object` installs one when no to-space
/// could be allocated, and `free_or_keep_cset` reads exactly the same predicate
/// to decide which regions it must keep. Deriving the flag from the map rather
/// than from a separate counter is what stops the report and the reclamation
/// decision from ever disagreeing.
#[inline]
fn g1_pause_degraded_flags_common(
    jni_pinned_out: usize,
    jit_pinned_out: usize,
    parallel: bool,
    evacuation_failure: bool,
) -> u32 {
    use cratonvm_gc_core::gc_metrics::g1_degraded as flag;
    let mut degraded = flag::NONE;
    if evacuation_failure {
        degraded |= flag::EVACUATION_FAILURE;
    }
    if jni_pinned_out > 0 {
        degraded |= flag::JNI_PINNED_REGIONS_EXCLUDED;
    }
    if jit_pinned_out > 0 {
        degraded |= flag::JIT_PINNED_REGIONS_EXCLUDED;
    }
    if parallel {
        degraded |= flag::PARALLEL_EVACUATOR;
    }
    degraded
}

pub fn g1_pause_degraded_flags(
    pointer_map: &cratonvm_types::PointerMap,
    jni_pinned_out: usize,
    jit_pinned_out: usize,
    parallel: bool,
) -> u32 {
    g1_pause_degraded_flags_common(
        jni_pinned_out,
        jit_pinned_out,
        parallel,
        pointer_map.iter().any(|(k, v)| k == v),
    )
}

impl G1Collector {
    /// Pause-aware variant of [`g1_pause_degraded_flags`] that excludes expected peer-object
    /// self-forwards and short-circuits when `no_self_forward_this_pause()` is true.
    pub fn g1_pause_degraded_flags_for(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        jni_pinned_out: usize,
        jit_pinned_out: usize,
        parallel: bool,
    ) -> u32 {
        let evac_failed = !self.no_self_forward_this_pause()
            && pointer_map
                .iter()
                .any(|(k, v)| k == v && !self.peer_object_pinned(*k));
        g1_pause_degraded_flags_common(jni_pinned_out, jit_pinned_out, parallel, evac_failed)
    }
}

/// How many *young* (Eden/Survivor) regions each pin vocabulary kept out of a
/// collection set, as `(jni_no_relocation_pins, jit_conservative_root_pins)`.
///
/// The two vocabularies are deliberately counted apart because they mean
/// different things and are fixed by different people: `G1Region::pinned` is a
/// JNI critical section the application controls, while
/// `G1Collector::jit_pinned_region_set` is a conservative JIT root (or a frozen
/// peer's un-retired TLAB tail) the runtime could in principle make precise. A
/// region carrying both is attributed to the JNI pin, which is the one that
/// will not go away on its own.
pub fn count_young_regions_pinned_out(regions: &[G1Region], jit_pinned: &RegionSet) -> (usize, usize) {
    let mut jni = 0usize;
    let mut jit = 0usize;
    for (i, r) in regions.iter().enumerate() {
        if r.region_type() != RegionType::Eden && r.region_type() != RegionType::Survivor {
            continue;
        }
        if r.pinned {
            jni += 1;
        } else if jit_pinned.contains(&i) {
            jit += 1;
        }
    }
    (jni, jit)
}

/// Assert, under `CRATONVM_G1_DBG_REACH=1`, the invariant every `Free` region
/// is retyped on: **`Free` implies `cursor == 0`**.
///
/// [`find_free_region_from`] filters on `region_type == RegionType::Free` and nothing
/// else, and none of its three callers resets the cursor before allocating from
/// the region they just claimed. So the whole design rests on `reset` being the
/// only way to become `Free` — and a violation would produce, in one step,
/// exactly the shape under investigation: a region whose `cursor` commits a span
/// no object header was ever written into. That makes the invariant worth
/// stating where it is relied on rather than inferring it from the reset sites.
///
/// Diagnostic only: it reports and continues, because the retype itself is not
/// the defect and aborting here would replace a walk break with a crash.
#[inline]
pub fn debug_free_region_cursor(region: &G1Region, idx: usize, site: &'static str) {
    if !gc_flags().g1_dbg_reach || region.cursor() == 0 {
        return;
    }
    eprintln!(
        "[g1][FREE-CURSOR] {site}: claimed Free region={idx} with cursor={:#x} != 0 \
         (reuse_epoch={} recycled_in_generation={} live_bytes={} pinned={} age={}) — \
         this region became Free without `reset`, so [0,{:#x}) is committed with no \
         object grid. bumps=[{}]",
        region.cursor(),
        region.reuse_epoch,
        region.recycled_in_generation(),
        region.live_bytes,
        region.pinned,
        region.age(),
        region.cursor(),
        region.bump_trail.lock().render(),
    );
}

/// Size of a TLAB carve: the request, capped by what the region has left, and
/// **rounded DOWN to a multiple of 8**.
///
/// The rounding is the whole point, and its absence was a live heap-corruption
/// bug (`known-issues/springboot/g1-fullsuite-regression-20260808.md`).
/// [`G1Region::bump_alloc`] aligns the carve's START to 8 and then commits
/// exactly `size` bytes, so an unaligned `size` leaves `region.cursor()` on an odd
/// boundary. `Tlab::new` meanwhile rounds its `end` DOWN to 8 — its documented
/// "release-mode safety net ... giving up at most 7 bytes of tail". The two
/// disagree, and the bytes between them belong to nobody:
///
/// * `Tlab::retire`'s filler covers `[cursor, end)` and stops at the TRIMMED end;
/// * `Tlab::reserved_tail` publishes `[cursor, end)` and stops there too;
/// * the next `bump_alloc(_, 8)` re-aligns UP, skipping past them.
///
/// So a linear walk arriving at the trimmed end finds zero bytes that no filler,
/// no skip span and no object describes. It can only read them as an all-zero
/// 16-byte object, which puts every later step off the real grid — the walk
/// desyncs, is abandoned, and every heap reference past that offset goes
/// un-rewritten by the pause.
///
/// Observed exactly: a carve of `0x11664` bytes at `0xf198` ends at `0x207fc`,
/// the TLAB ends at `0x207f8`, the next object starts at `0x20800`, and the
/// walker strode `0x207f8 -> 0x20808` into its middle.
///
/// `GenerationalHeap::refill_tlab` has masked with `& !7` since 2026-07-18
/// (`internal/fixed-suite-bugs/tlab-trigger-gc-young-walk-corruption-FIXED.md`),
/// for this same reason. G1's copy of the carve never got it, which is precisely
/// why the corruption reproduced under `-XX:+UseG1GC` and not under the default
/// collector.
#[inline]
pub fn tlab_carve_size(requested_size: usize, remaining: usize) -> usize {
    requested_size.min(remaining) & !7
}

// LANE E wave 2, small finding 2 — `find_free_region` is GONE.
//
// It was `find_free_region_from(regions, 0)` with no caller anywhere; it
// survived only because three doc comments linked to it. Those links describe
// what `find_free_region_from` does (the hinted scan that replaced it) and now
// name it. `region.rs` has a same-named METHOD which is unrelated and
// untouched.

/// Item 8 — the downward twin of [`find_free_region_from`]: the first Free
/// region at or below `start`, then (wrapping) the first at or below `top`
/// and above `start`. Both bounds are inclusive and `top` is the last region
/// of the committed prefix, so the search never leaves committed memory.
pub fn find_free_region_down_from(regions: &[G1Region], start: usize, top: usize) -> Option<usize> {
    let len = regions.len();
    if len == 0 {
        return None;
    }
    let top = top.min(len - 1);
    let start = start.min(top);
    let mut probed = 0usize;
    for i in (0..=start).rev() {
        probed += 1;
        if regions[i].region_type() == RegionType::Free {
            note_free_scan(probed);
            return Some(i);
        }
    }
    for i in ((start + 1)..=top).rev() {
        probed += 1;
        if regions[i].region_type() == RegionType::Free {
            note_free_scan(probed);
            return Some(i);
        }
    }
    note_free_scan(probed);
    None
}

/// Find a free region, starting the scan at `start` and wrapping.
///
/// G1AUD-9. Exhaustive — every region is probed exactly once — so this answers
/// exactly what an unhinted scan from region 0 does, only without paying for
/// the prefix the caller already consumed. See [`G1Collector::free_scan_hint`] for why
/// restarting at 0 made region claiming quadratic in the region count.
pub fn find_free_region_from(regions: &[G1Region], start: usize) -> Option<usize> {
    let len = regions.len();
    if len == 0 {
        return None;
    }
    let start = if start >= len { 0 } else { start };
    let mut probed = 0usize;
    for i in start..len {
        probed += 1;
        if regions[i].region_type() == RegionType::Free {
            note_free_scan(probed);
            return Some(i);
        }
    }
    for i in 0..start {
        probed += 1;
        if regions[i].region_type() == RegionType::Free {
            note_free_scan(probed);
            return Some(i);
        }
    }
    note_free_scan(probed);
    None
}

/// The allocation-failure compactor must leave its chosen target window empty.
/// This is the ordinary hinted search with one closed interval excluded; it is
/// called only while the VM is stopped and the compactor has installed that
/// interval, so preserving its exact normal fast path matters more than a
/// clever shared abstraction.
pub fn find_free_region_from_outside(
    regions: &[G1Region],
    start: usize,
    avoid: (usize, usize),
) -> Option<usize> {
    let len = regions.len();
    if len == 0 {
        return None;
    }
    let start = start.min(len - 1);
    let mut probed = 0usize;
    for i in start..len {
        probed += 1;
        if regions[i].region_type() == RegionType::Free && !(avoid.0..avoid.1).contains(&i) {
            note_free_scan(probed);
            return Some(i);
        }
    }
    for i in 0..start {
        probed += 1;
        if regions[i].region_type() == RegionType::Free && !(avoid.0..avoid.1).contains(&i) {
            note_free_scan(probed);
            return Some(i);
        }
    }
    note_free_scan(probed);
    None
}

/// Downward counterpart of [`find_free_region_from_outside`].
pub fn find_free_region_down_from_outside(
    regions: &[G1Region],
    start: usize,
    top: usize,
    avoid: (usize, usize),
) -> Option<usize> {
    let len = regions.len();
    if len == 0 {
        return None;
    }
    let top = top.min(len - 1);
    let start = start.min(top);
    let mut probed = 0usize;
    for i in (0..=start).rev() {
        probed += 1;
        if regions[i].region_type() == RegionType::Free && !(avoid.0..avoid.1).contains(&i) {
            note_free_scan(probed);
            return Some(i);
        }
    }
    for i in ((start + 1)..=top).rev() {
        probed += 1;
        if regions[i].region_type() == RegionType::Free && !(avoid.0..avoid.1).contains(&i) {
            note_free_scan(probed);
            return Some(i);
        }
    }
    note_free_scan(probed);
    None
}

// F-14 — how much the two linear free-region searches actually cost.
//
// The finding proposed replacing both with a free-region bitmap. Doing that
// means a SECOND source of truth for "is this region Free": the field is read
// in roughly two hundred places and written in eight, so a bitmap has to be
// maintained beside it, and a bitmap that says Free about a live region hands
// the allocator memory that is in use. That is the worst failure this file can
// have, and it is worth paying for only if the searches are actually expensive.
//
// So they are counted first. `[GC] g1 free-scan:` reports calls, total regions
// probed and the worst single probe for each search, and the decision is made
// on that rather than on the shape of the loop.
pub static FREE_SCAN_CALLS: AtomicU64 = AtomicU64::new(0);
pub static FREE_SCAN_PROBES: AtomicU64 = AtomicU64::new(0);
pub static FREE_SCAN_WORST: AtomicU64 = AtomicU64::new(0);
pub static CONTIG_SCAN_CALLS: AtomicU64 = AtomicU64::new(0);
pub static CONTIG_SCAN_PROBES: AtomicU64 = AtomicU64::new(0);
pub static CONTIG_SCAN_WORST: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn note_free_scan(probed: usize) {
    FREE_SCAN_CALLS.fetch_add(1, Ordering::Relaxed);
    FREE_SCAN_PROBES.fetch_add(probed as u64, Ordering::Relaxed);
    FREE_SCAN_WORST.fetch_max(probed as u64, Ordering::Relaxed);
}

#[inline]
pub fn note_contig_scan(probed: usize) {
    CONTIG_SCAN_CALLS.fetch_add(1, Ordering::Relaxed);
    CONTIG_SCAN_PROBES.fetch_add(probed as u64, Ordering::Relaxed);
    CONTIG_SCAN_WORST.fetch_max(probed as u64, Ordering::Relaxed);
}

/// F-14 — `(calls, regions probed, worst single scan)` for the single-region
/// free search and for the contiguous-run search.
pub fn free_region_scan_counts() -> (u64, u64, u64, u64, u64, u64) {
    (
        FREE_SCAN_CALLS.load(Ordering::Relaxed),
        FREE_SCAN_PROBES.load(Ordering::Relaxed),
        FREE_SCAN_WORST.load(Ordering::Relaxed),
        CONTIG_SCAN_CALLS.load(Ordering::Relaxed),
        CONTIG_SCAN_PROBES.load(Ordering::Relaxed),
        CONTIG_SCAN_WORST.load(Ordering::Relaxed),
    )
}

/// Find `count` contiguous free regions.
///
/// # F-14: measured, and deliberately still a linear scan
///
/// The finding proposed replacing this and its single-region sibling with a
/// free-region bitmap, because both are O(regions) and the region count was
/// growing without bound with `-Xmx`. Two measurements settle it, and neither
/// says "bitmap".
///
/// **The ordinary path is already free.** On young churn over a 2048-region
/// heap, `find_free_region_from` probed **exactly one region per call, worst
/// case one**, across 1,543 calls — the rotating hint does its job — and this
/// function was not called at all.
///
/// **The humongous path is the expensive one, and it is still cheap.** Under
/// 400 humongous allocations on a 256-region heap it probed 35,943 regions over
/// 400 calls: ~90 per call, worst 227 of 256. That is most of the heap per
/// call — and it is 36,000 comparisons of an enum against a constant over a
/// whole run, on a path that then memsets megabytes for the span it found. A
/// hint was tried and measured: pointing the search at the free-scan cursor
/// took it from 35,943 probes to 35,617, which is 0.9% and not worth the
/// parameter, because that cursor tracks single-region Eden claims and has no
/// relationship to where a humongous span was freed.
///
/// **What actually bounds it is the other half of F-14.** The worry was that
/// the region count grows with the heap — 8192 regions at 16 GiB under the old
/// two-step ladder. The region-size ergonomic now targets ~2048 regions at any
/// heap size, so this scan is bounded by a constant instead of by `-Xmx`, which
/// is the property the finding was actually asking for.
///
/// So the bitmap is refused on the number, not on the effort. It would buy
/// those comparisons at the price of a SECOND source of truth for "is this
/// region Free" — a field read in roughly two hundred places and written in
/// eight — and a bitmap that says Free about a live region hands the allocator
/// memory that is in use, which is the worst failure this file can have.
///
/// `free_region_scan_counts` keeps the instrument, so this decision can be
/// revisited against a workload rather than against the shape of the loop.
pub fn find_contiguous_free(regions: &[G1Region], count: usize) -> Option<usize> {
    find_contiguous_free_best_fit(regions, count)
}

/// Find a humongous-sized run without touching a compacted retry reservation.
/// The reservation is tiny-lived and this scan runs only on humongous paths,
/// so keeping it explicit is safer than making the normal best-fit chooser
/// infer an ambient exclusion.
pub fn find_contiguous_free_outside(
    regions: &[G1Region],
    count: usize,
    avoid: (usize, usize),
) -> Option<usize> {
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut probed = 0usize;
    for (idx, region) in regions.iter().enumerate() {
        probed += 1;
        if (avoid.0..avoid.1).contains(&idx) || region.region_type() != RegionType::Free {
            run_len = 0;
            continue;
        }
        if run_len == 0 {
            run_start = idx;
        }
        run_len += 1;
        if run_len >= count {
            note_contig_scan(probed);
            return Some(run_start);
        }
    }
    note_contig_scan(probed);
    None
}

/// Compute total object size from header.
///
/// Defensive corruption handling (gc-abort-cleanup, mirrors `gc.rs`): a corrupt /
/// implausible array header (e.g. a stale `array_length` so large that
/// `header + length * element_size` overflows `usize`) must NOT abort the whole
/// VM. This previously `.expect()`-panicked here, killing the process on a bad
/// header, whereas the non-moving sweep (`gen_heap.rs::gen_object_total_size`)
/// returns a `0` sentinel and lets its walker re-sync. Mirror that behavior: on
/// overflow, log a diagnostic and return `0`. `0 < HEADER_SIZE`, so every
/// caller's existing corruption guard treats it as a bad header and stops /
/// re-syncs the linear walk rather than advancing the cursor by 0 and spinning.
///
/// This does not mask genuine bugs silently — the corruption is logged — but it
/// converts a hard process abort into a recoverable / fail-safe path.
#[inline(always)]
pub fn object_total_size_cached(
    header: &ObjectHeader,
    layout_cache: &mut CompactLayoutCache,
) -> usize {
    if header.kind() != ObjectKind::Array && cratonvm_types::is_compact_object(header) {
        layout_cache
            .total_size(header.class_id.as_u32(), header.num_slots())
            .map_or(1 << 40, |total| total as usize)
    } else {
        object_total_size(header)
    }
}

pub fn object_total_size(header: &ObjectHeader) -> usize {
    if header.kind() == ObjectKind::Array {
        match array_data_size(header.array_length() as usize, header.element_type()) {
            Ok(data) => ARRAY_DATA_OFFSET + data,
            Err(_) => {
                // Implausible array header — treat as corrupt. Return 0 so the
                // caller's `total_size < HEADER_SIZE` guard fires (matching the
                // non-moving sweep's re-sync contract) instead of panicking.
                tracing::warn!(
                    "g1: implausible array_length {} (element_type={:?}) in object header — \
                     treating as corrupt; caller will skip/stop the walk",
                    header.array_length(),
                    header.element_type(),
                );
                0
            }
        }
    } else {
        // object_body_size() honours the per-object GC_FLAG_COMPACT bit: a
        // compact-layout instance stores its true body size (ref fields = 8B,
        // primitive fields = 16B, packed by declared field, NOT a uniform
        // num_slots*SLOT_SIZE) in the header's array_length. Using the plain
        // num_slots*SLOT_SIZE legacy formula unconditionally here OVER-sizes
        // every compact object (up to 8 bytes wasted per reference field),
        // which corrupted evacuation: evacuate_object()/its equivalent below
        // used this inflated size both to reserve destination space AND as
        // the copy_nonoverlapping() length, desyncing every subsequent
        // object's stride through the region from its actual body size.
        // Root-caused via the JavaPoet LineWrapper NPE
        // (CRATONVM-SPRING-GENUINE-BUGLIST): LineWrapper
        // mixes ref/primitive fields with its LAST field (nextFlush, a ref)
        // landing at a compact byte offset the legacy formula never accounted
        // for. Mirrors the already-correct gen_heap.rs::gen_object_total_size.
        cratonvm_gc_core::object_instance_size(header)
    }
}

/// Round-9 gc CRIT-1: returns true if this header marks the start of a
/// humongous continuation filler region. Walkers MUST check this before
/// computing a per-object size — the filler covers the whole region
/// regardless of the (synthetic) per-field values stored in the header.
#[inline]
pub fn is_humongous_filler(header: &ObjectHeader) -> bool {
    matches!(header.kind(), ObjectKind::HumongousFiller)
}

thread_local! {
    /// INT-8: same-thread SATB pre-barrier suppression window. Set only by
    /// [`SatbPreSuppressGuard`] around the weak-reference protocol writes
    /// (`G1Collector::set_field_no_satb`); consulted by `set_field`'s SATB
    /// block ONLY while marking is active, so the non-marking store path
    /// never touches TLS.
    pub static SATB_PRE_SUPPRESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// INT-8: is the current thread inside a `set_field_no_satb` protocol write?
#[inline]
pub fn satb_pre_suppressed() -> bool {
    SATB_PRE_SUPPRESSED.with(std::cell::Cell::get)
}

/// RAII scope for [`SATB_PRE_SUPPRESSED`] — restores the flag on drop (also
/// on unwind), so a panicking store can never leak suppression into later,
/// semantic stores on the same thread.
pub struct SatbPreSuppressGuard {
    pub prev: bool,
}

impl SatbPreSuppressGuard {
    pub fn new() -> Self {
        let prev = SATB_PRE_SUPPRESSED.with(|f| f.replace(true));
        Self { prev }
    }
}

impl Drop for SatbPreSuppressGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        SATB_PRE_SUPPRESSED.with(|f| f.set(prev));
    }
}

/// G1MARK-8: can `obj_addr` be trusted as an object start for a mark scan?
///
/// Gray-set entries are raw field bytes — a corrupt or stale slot can name
/// any address inside a live region, and `scan_object_refs` would then walk
/// a garbage header's `num_slots`/`array_length` extent, pushing more
/// garbage as children (corruption amplifier). Gate: object starts are
/// 8-aligned (`is_heap_addr`'s own invariant), the header must sit inside
/// the region's allocated prefix, its fields must be self-consistent
/// (shared `concurrent_mark_object_size` validator), and — except for a
/// humongous span, whose scan goes through the bounds-checked
/// `humongous_copy` — the object's extent must not cross the cursor.
///
/// False positives (garbage that happens to look plausible) are bounded by
/// the containment check; false negatives (a real object rejected, e.g. a
/// torn header) under-mark, which is why the caller sets
/// `mark_saw_implausible` and `cleanup` retains everything for the cycle.
/// Why a gray-set entry cannot be scanned -- and, crucially, whether that
/// says anything about the CYCLE.
///
/// The G1MARK-8 gate answered one bit, and the fail-safe it drives (retain
/// every region, no in-place frees, no humongous reclaim) is the most
/// expensive response this collector has. Two very different conditions were
/// reaching it:
///
///   * an address that is provably not a live object, and
///   * an address inside allocated memory whose header does not decode.
///
/// Only the second is evidence that the mark closure might be incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrayRefusal {
    /// **Not allocated.** The address is not the start of any object in this
    /// region's CURRENT incarnation: it is unaligned, at or beyond the
    /// allocation cursor, or in a region type that holds no object starts.
    ///
    /// Skipping it under-marks nothing, because nothing live is there --
    /// `cursor` only grows within an incarnation (every allocation path bumps
    /// it under the regions lock, and a TLAB carve bumps it for the whole
    /// carve), so every live object satisfies `off + size <= cursor`. The
    /// closure stays complete and the cycle keeps its verdicts.
    ///
    /// This is NOT a rare corruption. Freed regions are deliberately no longer
    /// scrubbed (see `G1Region::reset`, G1AUD-10), so a recycled region still
    /// holds its previous incarnation's bytes above the new cursor -- and SATB
    /// retention means the marker legitimately scans objects that died during
    /// the cycle and whose referents were freed with their region. Measured on
    /// `org.h2.test.store.TestKillProcessWhileWriting`: 2 226 of 2 249 refusals
    /// in one 32 s run were this, every one of them a live object's reference
    /// slot (`CRATONVM_G1_DBG_GRAY_PROV=1` names the pusher) naming an address
    /// above a Survivor region's cursor.
    NotAllocated,
    /// **Torn header.** The address IS inside the allocated prefix, so
    /// something was placed there, and its header does not describe an object.
    /// The scan it would have driven may have been the only path to a live
    /// subtree, so the cycle's `live_bytes == 0` verdicts cannot be trusted.
    /// This is what the fail-safe is for.
    TornHeader,
}

/// [`GrayRefusal`]'s question, asked of a [`HeaderVerdict`]: is this refusal
/// evidence of CORRUPTION, or of an address that simply is not a live object?
///
/// `true` only for the verdicts that mean "something IS allocated here and its
/// header does not describe it". Everything else -- unaligned, outside the
/// arena, no such region, a `Free` region, at or above the cursor -- says the
/// address was never an object start in this region's current incarnation,
/// which is routine for the reasons [`GrayRefusal::NotAllocated`] spells out.
///
/// `HumongousFiller` counts as TORN here and does NOT on the marking side, and
/// the difference is deliberate: the marker may legitimately be handed a
/// continuation slice's base, whereas a *reference slot* pointing at one names
/// an address no allocation ever returned.
pub fn evac_refusal_is_torn(verdict: HeaderVerdict) -> bool {
    match verdict {
        HeaderVerdict::BadKindTag
        | HeaderVerdict::BadElementTag
        | HeaderVerdict::ImplausibleShape
        | HeaderVerdict::HumongousFiller => true,
        HeaderVerdict::Object
        | HeaderVerdict::NullOrUnaligned
        | HeaderVerdict::OutsideArena
        | HeaderVerdict::NoRegionGeometry
        | HeaderVerdict::NoSuchRegion
        | HeaderVerdict::RegionFree
        | HeaderVerdict::BelowRegionBase
        | HeaderVerdict::AboveCursor => false,
    }
}

/// `CRATONVM_G1_LATE_HEADER_WRITE=1` -- publish an out-of-line allocation
/// BEFORE its header is written, i.e. the ordering G1 had until 2026-09-01.
///
/// The one-binary A/B for that fix. `alloc_in_region_initialized` normally runs
/// the caller's header write over the fresh span while the regions lock is
/// still held; with this set it publishes first and calls the initializer after
/// the lock is dropped, reopening the window in which a heap walk reads an
/// address the cursor covers and a header that is not there yet. See
/// [`G1Region::bump_alloc_initialized`] for what a walker reads in that window
/// and why an ARRAY is the shape that makes it fatal.
///
/// Default OFF. It exists because the fix UNBLOCKS a workload rather than
/// moving a number, and a fix like that has no A/B unless the old behaviour is
/// reachable on the same binary.
/// Value-parsed, NOT `is_some()`. A kill switch whose OFF word turns it ON is
/// a control arm that silently is not one: an operator bisecting with
/// `CRATONVM_G1_LATE_HEADER_WRITE=0` would have been running the buggy
/// ordering while believing they had disabled it. `truthy_word` is the same
/// reading `CRATONVM_G1_PARALLEL_EVAC` gets -- empty, `0`, `false`, `off` and
/// `no` are all OFF.
pub fn g1_late_header_write() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.into_string().ok())
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v.is_empty() || v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(false)
    })
}

/// `CRATONVM_G1_MARK_OOB_FAILSAFE=1` -- treat [`GrayRefusal::NotAllocated`]
/// as a torn header again, i.e. restore the single-bit gate.
///
/// The one-binary A/B for the split. With it set, `TestKillProcessWhileWriting`
/// goes back to retaining every region on essentially every cycle.
pub fn mark_oob_failsafe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        false
    })
}



/// `CRATONVM_G1_MARK_ROOT_SCREEN=0` -- push every mark root onto the gray set
/// raw again, i.e. the behaviour before [`G1Collector::mark_root_seed`]
/// (r11w18 g1store). Default ON. Value-parsed like
/// [`g1_late_header_write`]: `0`, `false`, `off` and `no` are OFF.
pub fn g1_mark_root_screen() -> bool {
    None::<std::ffi::OsString>
        .and_then(|v| v.into_string().ok())
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "off" || v == "no")
        })
        .unwrap_or(true)
}

/// `CRATONVM_G1_OVERFLOW_RESCAN_EDEN_BITMAP=0` -- walk Eden regions' object
/// grids in the concurrent overflow rescan again instead of their mark bitmaps
/// (r11w18 g1store; see the rescan in `concurrent_mark_step_worker`). Default
/// ON. Value-parsed like [`g1_mark_root_screen`].
pub fn g1_overflow_rescan_eden_by_bitmap() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.into_string().ok())
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true)
    })
}

/// `CRATONVM_G1_EVAC_HEADROOM_TRIGGER=0` -- drop the evacuation-headroom half
/// of `needs_gc` (see [`G1Collector::young_exceeds_evac_headroom`]) and
/// collect only on the Free fraction and the pause-time young target, the
/// behaviour before 2026-09-30. Default on.
pub fn g1_evac_headroom_trigger() -> bool {
    // gci i19 (orchestrator): the latch is process-wide, so under `cargo test`
    // whichever test polled `needs_gc` first decided it for every later
    // `with_thread_overrides` — `a_young_target_at_its_ceiling_never_triggers_a_collection`
    // pins this switch off and failed once an earlier test latched it on.
    // Tests read it per call; the VM keeps the latch (`needs_gc` is a hot poll).
    if cfg!(test) {
        return true;    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.into_string().ok())
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true)
    })
}

/// gci i7/g1y — `CRATONVM_G1_YOUNG_TARGET_NET_OF_PINS`, default OFF: should the
/// young-target arm of `needs_gc` subtract the young regions the last pause
/// kept out of its collection set under a pin
/// ([`G1Collector::young_pinned_kept_regions`])?
///
/// The storm it addresses (vi5g-g1f J, G1 4-thread in-process javac at
/// -Xmx512m, 3 of 3 runs at the 600 s timeout after ~2 800 young pauses):
/// the target sat at its floor (25 regions) while 15-27 young regions were
/// pinned out of every pause (`jit_pinned_out=`) and stayed young, so the arm
/// fired once ~3-7 Eden regions had been allocated, and each pause freed
/// 70 KB-1 MB with ~210 regions Free. The JIT refill trigger
/// (`tlab_alloc_shaped_inner`, site `tlab-alloc-shaped`) takes those pauses
/// through `maybe_gc_forced_at`, which is why they print `door=alloc-failure`
/// although no allocation failed. A policy change (more Eden per pause, so
/// fewer and longer pauses), hence opt-in until its rows decide.
///
/// gci i12/sx: read once per collector, into
/// `G1Collector::young_target_net_of_pins`, not in a process `OnceLock`.
pub fn g1_young_target_net_of_pins() -> bool {
    false
}

/// gci i13/ef finding 9: `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD=1` (default
/// OFF) -- the serial evacuator retries a Survivor copy that found no room in
/// Old before it self-forwards (HotSpot's `allocate_copy_slow`). Read once per
/// collector into `G1Collector::survivor_overflow_to_old`. gci i27/gs: the
/// parallel evacuator too (`SharedEvac::evacuate`, after the cap's retry).
pub fn g1_survivor_overflow_to_old() -> bool {
    false
}

/// gci i13/ef finding 4: `CRATONVM_G1_PINNED_YOUNG_AS_SURVIVOR=1` (default
/// OFF) -- an Eden region a pin kept out of the CSet is retyped Survivor at
/// the end of the pause. Read once per collector into
/// `G1Collector::pinned_young_as_survivor`.
pub fn g1_pinned_young_as_survivor() -> bool {
    false
}

/// The young-target arm of `needs_gc` as a pure function: fire once the
/// collectable young generation (`young` minus `pinned_kept`, the regions the
/// last pause had to keep) reaches `target`, and never while the target sits
/// at its ceiling `max` (see the MEASURED note in `needs_gc`). With
/// `pinned_kept == 0` this is exactly the arm before gci i7/g1y.
#[inline]
pub fn young_target_arm_fires(young: usize, target: usize, max: usize, pinned_kept: usize) -> bool {
    if target >= max {
        return false;
    }
    young.saturating_sub(pinned_kept) >= target
}

/// gci i43/gy — the young-target arm under an operator's young ceiling
/// ([`G1Collector::young_ceiling_binding`]): fire once `counted` reaches the
/// target, at the ceiling too, but never before it exceeds `floor` (the young
/// count the last pause left, in the arm's own measure): HotSpot's
/// `absolute_min_young_length = survivors + 1`, so a pause that could free
/// nothing is not re-requested before one Eden region has been claimed. Pure.
#[inline]
pub fn binding_young_arm_fires(counted: usize, target: usize, floor: usize) -> bool {
    counted >= target.max(floor.saturating_add(1))
}

/// gci i43/gy — `CRATONVM_G1_YOUNG_GEN_SIZER`, default ON (`=0` kills):
/// G1 honours `-Xmn` / `-XX:NewSize` / `-XX:MaxNewSize` / `-XX:NewRatio` as
/// HotSpot 25's `G1YoungGenSizer` does
/// (`gci-i42ta-g1-ignores-xmn-newsize-maxnewsize-newratio-FIXED-20261016`). Off:
/// the four are ignored under G1 and the bounds stay `G1NewSizePercent` /
/// `G1MaxNewSizePercent`, as before i43. Read once per heap, by
/// [`G1Collector::set_young_gen_sizing`]; the launcher reads it to decide
/// whether its "ignored under G1" note still applies.
pub fn g1_young_gen_sizer_enabled() -> bool {
    true
}

/// gci i45/gy — `CRATONVM_G1_YOUNG_BOUNDS_COMMITTED`, default ON (`=0`
/// kills): G1's young bounds (every sizer kind but `-Xmn` / `NewSize` +
/// `MaxNewSize`, which are fixed) are taken from the COMMITTED regions and
/// re-derived on every heap size change, as HotSpot 25's
/// `G1YoungGenSizer::heap_size_changed(num_committed_regions)` does
/// (`gci-i43gy-g1-young-bounds-use-max-regions-not-committed-RETIRED-UNFIXED-20261018`). The
/// committed count is the soft capacity, which differs from the `-Xmx` grid
/// only under the opt-in `CRATONVM_G1_HEAP_RESIZE`; off: the grid, as before.
/// Read once per collector, by `G1Collector::new`.
pub fn g1_young_bounds_committed_enabled() -> bool {
    true
}

/// gci i45/gy — the region count G1's young sizer sizes from: the committed
/// regions (`committed`, clamped into `[1, table]`) when `follow_committed`,
/// else the whole table (the `-Xmx` grid, the bounds before i45). Pure.
#[inline]
pub fn g1_young_sizer_heap_regions(follow_committed: bool, committed: usize, table: usize) -> usize {
    let table = table.max(1);
    if follow_committed {
        committed.clamp(1, table)
    } else {
        table
    }
}

/// gci i45/gy — where the young target goes when the young bounds move from
/// `old` to `new` (`(min, max)` each) on a heap size change: a target at or
/// above the old ceiling stays at the ceiling (the new one), any other is
/// clamped into the new bounds. Pure.
#[inline]
pub fn g1_young_target_after_heap_size_change(
    target: usize,
    old: (usize, usize),
    new: (usize, usize),
) -> usize {
    let (new_min, new_max) = (new.0, new.1.max(new.0));
    if target >= old.1 {
        new_max
    } else {
        target.clamp(new_min, new_max)
    }
}

/// gci i43/gy — HotSpot 25's `G1YoungGenSizer::SizerKind` with the region
/// counts the command line fixed (`g1YoungGenSizer.cpp`). Every count is a
/// region count, `>= 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum G1YoungSizer {
    /// None of the four flags: `G1NewSizePercent` / `G1MaxNewSizePercent` of
    /// the heap's regions, adaptive.
    #[default]
    Defaults,
    /// `-XX:NewSize` alone: its regions are the minimum; the maximum is the
    /// default one, never below the minimum. Adaptive.
    NewSizeOnly { min: usize },
    /// `-XX:MaxNewSize` alone: its regions are the maximum; the minimum is
    /// the default one, never above the maximum. Adaptive.
    MaxNewSizeOnly { max: usize },
    /// `-XX:NewSize` and `-XX:MaxNewSize` (`-Xmn` sets both): fixed bounds,
    /// adaptive only when they differ.
    MaxAndNewSize { min: usize, max: usize },
    /// `-XX:NewRatio` without either size: `regions / (NewRatio + 1)` (floor
    /// 1) for both bounds, re-derived from the heap's regions; NOT adaptive.
    NewRatio { ratio: usize },
}

impl G1YoungSizer {
    /// `(min, max)` young length for a `total_regions` heap: HotSpot's
    /// `recalculate_min_max_young_length`. [`Self::Defaults`] is
    /// byte-for-byte the pre-i43 `young_region_bounds`.
    pub fn bounds(&self, total_regions: usize) -> (usize, usize) {
        let total = total_regions.max(1);
        let default_min = (total * G1_MIN_NEW_PERCENT / 100).max(1);
        let default_max = (total * G1_MAX_NEW_PERCENT / 100).max(1);
        match *self {
            G1YoungSizer::Defaults => (default_min, default_max.max(default_min)),
            G1YoungSizer::NewSizeOnly { min } => {
                let min = min.max(1);
                (min, default_max.max(min))
            }
            G1YoungSizer::MaxNewSizeOnly { max } => {
                let max = max.max(1);
                (default_min.min(max), max)
            }
            G1YoungSizer::MaxAndNewSize { min, max } => {
                let min = min.max(1);
                (min, max.max(min))
            }
            G1YoungSizer::NewRatio { ratio } => {
                let n = (total / ratio.saturating_add(1)).max(1);
                (n, n)
            }
        }
    }

    /// HotSpot's `use_adaptive_young_list_length()`.
    pub fn adaptive(&self) -> bool {
        match *self {
            G1YoungSizer::NewRatio { .. } => false,
            G1YoungSizer::MaxAndNewSize { min, max } => min != max,
            _ => true,
        }
    }

    /// `true` when the maximum young length came from the command line
    /// (`MaxNewSize`, `-Xmn`, or `NewRatio`), so the young-target arm must
    /// collect at it; `NewSize` alone keeps the default ceiling.
    pub fn ceiling_is_operators(&self) -> bool {
        matches!(
            self,
            G1YoungSizer::MaxNewSizeOnly { .. }
                | G1YoungSizer::MaxAndNewSize { .. }
                | G1YoungSizer::NewRatio { .. }
        )
    }

    /// Pack into `(kind, a, b)` for the collector's three atomics.
    pub fn encode(&self) -> (u8, usize, usize) {
        match *self {
            G1YoungSizer::Defaults => (0, 0, 0),
            G1YoungSizer::NewSizeOnly { min } => (1, min, 0),
            G1YoungSizer::MaxNewSizeOnly { max } => (2, 0, max),
            G1YoungSizer::MaxAndNewSize { min, max } => (3, min, max),
            G1YoungSizer::NewRatio { ratio } => (4, ratio, 0),
        }
    }

    /// Inverse of [`Self::encode`]; an unknown kind is [`Self::Defaults`].
    pub fn decode(kind: u8, a: usize, b: usize) -> Self {
        match kind {
            1 => G1YoungSizer::NewSizeOnly { min: a },
            2 => G1YoungSizer::MaxNewSizeOnly { max: b },
            3 => G1YoungSizer::MaxAndNewSize { min: a, max: b },
            4 => G1YoungSizer::NewRatio { ratio: a },
            _ => G1YoungSizer::Defaults,
        }
    }
}

/// gci i43/gy — HotSpot 25's `G1YoungGenSizer` constructor over the typed
/// flags: the sizer and the `[warning][gc,ergo]` texts it logs. Pure.
///
/// * `-Xmn<n>` is `NewSize = MaxNewSize = n` on the command line (HotSpot's
///   `Arguments::parse_each_vm_init_arg`), so it wins over a typed `NewSize` /
///   `MaxNewSize` here (HotSpot: the later option wins; this request has no
///   order).
/// * `NewRatio` with `NewSize` or `MaxNewSize`: warning
///   `-XX:NewSize and -XX:MaxNewSize override -XX:NewRatio`, ratio ignored;
///   `NewRatio` alone: [`G1YoungSizer::NewRatio`].
/// * `NewSize > MaxNewSize` (both typed): warning `NewSize (<k>k) is greater
///   than the MaxNewSize (<k>k). A new max generation size of <k>k will be
///   used.` and `MaxNewSize = NewSize` (a fixed young length: the flag's
///   was-on-command-line bit survives `FLAG_SET_ERGO`).
/// * a size is `max(size / region_size, 1)` regions.
pub fn g1_young_sizer_from_flags(
    req: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    region_size: usize,
) -> (G1YoungSizer, Vec<String>) {
    let mut warnings = Vec::new();
    let new_size = req.xmn.or(req.new_size);
    let max_new_size = req.xmn.or(req.max_new_size);
    if let Some(ratio) = req.new_ratio {
        if new_size.is_some() || max_new_size.is_some() {
            warnings.push("-XX:NewSize and -XX:MaxNewSize override -XX:NewRatio".to_string());
        } else {
            return (G1YoungSizer::NewRatio { ratio }, warnings);
        }
    }
    let region = region_size.max(1);
    let regions = |bytes: usize| (bytes / region).max(1);
    let sizer = match (new_size, max_new_size) {
        (Some(n), Some(m)) => {
            let m = if n > m {
                warnings.push(format!(
                    "NewSize ({}k) is greater than the MaxNewSize ({}k). A new max generation \
                     size of {}k will be used.",
                    n / 1024,
                    m / 1024,
                    n / 1024
                ));
                n
            } else {
                m
            };
            G1YoungSizer::MaxAndNewSize {
                min: regions(n),
                max: regions(m),
            }
        }
        (Some(n), None) => G1YoungSizer::NewSizeOnly { min: regions(n) },
        (None, Some(m)) => {
            // gc-known-issues retire: HotSpot compares the DEFAULT `NewSize`
            // too (`-XX:MaxNewSize=0`, local 25.0.4.1: `NewSize (1331k) is
            // greater than the MaxNewSize (0k). ...`), then raises the maximum.
            let n = cratonvm_gc_core::gen_young_sizing::HOTSPOT_DEFAULT_NEW_SIZE;
            let m = if n > m {
                warnings.push(format!(
                    "NewSize ({}k) is greater than the MaxNewSize ({}k). A new max generation \
                     size of {}k will be used.",
                    n / 1024,
                    m / 1024,
                    n / 1024
                ));
                n
            } else {
                m
            };
            G1YoungSizer::MaxNewSizeOnly { max: regions(m) }        }
        (None, None) => G1YoungSizer::Defaults,    };
    (sizer, warnings)}

/// gci i14/yt — `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL` (default ON since
/// 2026-10-15, see the WARNING line below; it was opt-in): size
/// the G1 young generation the way HotSpot 25's
/// `G1Policy::update_young_length_bounds` does, and count it the way HotSpot's
/// young list does.
///
/// The storm it addresses (vi7-g1y J on i10a, G1 4-thread in-process javac
/// -Xmx512m, `[GC-STAT]` medians after the first 12 pauses): 3/3 default runs
/// at the 600 s timeout, 1 881-2 495 pauses, each pause 183-231 ms of which
/// fix-up 111-139 ms and closure 71-91 ms spent walking 35-57 MB of rset
/// sources, while it copied 3 KB-431 KB and freed 44-545 KB. The pause is
/// young-size independent (the `CRATONVM_G1_YOUNG_PAUSE_TARGET=0` control
/// copies 46 MB per pause at a 649 ms median and completes in 130 s with 31
/// pauses), so the 20 %-per-overrun loop walked the target to its 25-region
/// floor in 12 pauses and no pause could bring it back. At the floor the arm
/// compared 25 against a young count of 29-31 that was already ~24 regions of
/// survivors and pinned-kept regions right after the pause (`surv_regions`
/// 18-24, `jit_pinned_out` 17-24, Eden 5-12 per pause), so every few Eden
/// regions fired another ~200 ms pause.
///
/// HotSpot's answer to "the base time alone exceeds the goal" is the minimum
/// young length -- `max(G1NewSizePercent of the heap, survivors + 1)` -- and
/// its young list at that length is Eden plus PLAB-packed survivors, with a
/// pinned young region retyped Old at the end of the pause. That leaves ~20
/// Eden regions per pause where ours left 3-8. A pause-shape policy, hence
/// opt-in until its rows decide. Read once per collector, into
/// `G1Collector::young_target_hotspot_model`.
///
/// WARNING gci-user-flip-20261013: default ON since 2026-10-15 (vi19-ys M5 +
/// S PASS, paired with `CRATONVM_G1_SURVIVOR_PLAB`); `=0` kills.
pub fn g1_young_target_hotspot_model() -> bool {
    true
}





/// JIT round 23 G1 evacuation page: `CRATONVM_G1_EVAC_ONE_STORE_MARK`
/// (default ON, `=0` kills). Read once per pause.
pub fn g1_evac_one_store_mark_enabled() -> bool {
    true
}

/// JIT round 23 G1 evacuation page: `CRATONVM_G1_EVAC_COPY_END_OWNER_STORE`
/// (default ON, `=0` kills). Read once per pause.
pub fn g1_evac_copy_end_owner_store_enabled() -> bool {
    true
}

/// g1sz: the parallel evacuation copy. Objects of 16..=64 bytes (a tree node,
/// a boxed value, a short string's header) are two fixed-width loads and
/// stores whose ranges overlap in the middle, inlined, instead of a call into
/// the platform `memcpy` with a variable length; anything else is that call.
///
/// # Safety
/// `src` and `dst` are valid for `len` bytes and do not overlap.
#[inline(always)]
pub unsafe fn copy_object_bytes(src: *const u8, dst: *mut u8, len: usize) {
    if (16..=32).contains(&len) {
        let a = std::ptr::read_unaligned(src as *const [u8; 16]);
        let b = std::ptr::read_unaligned(src.add(len - 16) as *const [u8; 16]);
        std::ptr::write_unaligned(dst as *mut [u8; 16], a);
        std::ptr::write_unaligned(dst.add(len - 16) as *mut [u8; 16], b);
    } else if (33..=64).contains(&len) {
        let a = std::ptr::read_unaligned(src as *const [u8; 32]);
        let b = std::ptr::read_unaligned(src.add(len - 32) as *const [u8; 32]);
        std::ptr::write_unaligned(dst as *mut [u8; 32], a);
        std::ptr::write_unaligned(dst.add(len - 32) as *mut [u8; 32], b);
    } else {
        std::ptr::copy_nonoverlapping(src, dst, len);
    }
}


/// The mark word an evacuation copy's destination takes in one plain store:
/// the source snapshot `observed` with the old-generation stamp (a promotion)
/// or its age plus one (a Survivor copy) folded in -- exactly the word the
/// snapshot store followed by `add_gc_flags(GC_FLAG_OLD_GEN)` /
/// `set_gc_age(gc_age + 1)` leaves. `None` under `=0`: the caller stores the
/// snapshot and runs those two read-modify-writes, as before. Pure.
#[inline]
pub fn destination_mark(observed: u32, promote: bool, enabled: bool) -> Option<u32> {
    if !enabled {
        return None;
    }
    Some(if promote {
        ObjectHeader::mark_with_gc_flags(observed, GC_FLAG_OLD_GEN)
    } else {
        ObjectHeader::mark_with_gc_age(observed, ObjectHeader::gc_age_of(observed).saturating_add(1))
    })
}

/// Is a copy at `addr` inside the whole region `tlab` owns? Then this worker
/// is that region's only writer of `parallel_copy_end` this pause: a pool or
/// resume region is claimed by one TLAB at a time and `shared_dest_alloc`
/// never bumps into one, while a SHARED PLAB chunk (`tlab.shared`) and a
/// shared-destination copy outside the TLAB (other workers may bump the same
/// region) are not. Pure.
#[inline]
pub fn copy_end_owned_by(tlab: &Tlab, addr: usize) -> bool {
    !tlab.shared
        && tlab.region_idx.is_some()
        && addr >= tlab.base
        && addr < tlab.base.saturating_add(tlab.len)
}

/// gci i25/pl (`gci-i24ym-g1-survivor-plab-arm-promoting-pause-is-slower-per-byte-RETIRED-UNFIXED-20261018`):
/// must a parallel copy publish its end through
/// `G1Region::publish_parallel_copy_end`? Pure.
///
/// The watermark exists because a per-worker TLAB publishes its region's
/// cursor only at `retire_tlab`, so a forward target in a live TLAB lies
/// above the cursor and the only reader of the watermark,
/// `forward_target_has_readable_header` on a `RegionView::Raw` (the
/// candidate screen's `screen_shape` of a source already forwarded by a
/// peer), admits it against `max(cursor, parallel_copy_end)`. A SHARED Survivor PLAB chunk is
/// different: it was claimed with the region's own cursor CAS
/// (`claim_unzeroed`), so the cursor already stands past every copy in it
/// before the copy is made, and the copier's CAS happens-before any reader
/// that acquires the forward it then publishes. The publish is redundant
/// there -- and it is the one per-copy write on that arm that every worker
/// makes to the SAME cache line (one shared region per pause, whose
/// `cursor`, `data` and `region_type` the validators and `view().get` also
/// read), where the per-worker arm writes one line per worker. `dest_shared`
/// is the destination TLAB's `shared` bit after the allocation.
#[inline]
pub fn plab_copy_end_needs_publish(elide: bool, dest_shared: bool) -> bool {
    !elide || !dest_shared
}

/// The Survivor PLAB size: HotSpot's `YoungPLABSize` default (4096 words,
/// 32 KiB on a 64-bit VM). Fixed: HotSpot's `ResizePLAB` adapts it per pause
/// from the waste it measured; ours retires at most one tail per worker per
/// region, a few hundred KiB a pause at ~20 workers.
pub const SURVIVOR_PLAB_BYTES: usize = 32 * 1024;

/// gci i15/ra: the value of `SharedEvac::survivor_plab_region` while one
/// worker installs the pause's next shared Survivor region
/// (`SharedEvac::claim_survivor_plab`). Never a region index (a region table
/// is far shorter), and distinct from `usize::MAX` ("no region yet").
pub const SURVIVOR_PLAB_SWITCHING: usize = usize::MAX - 1;
