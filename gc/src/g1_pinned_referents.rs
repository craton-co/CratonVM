// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i27/gr -- the per-object referent verdict for the young regions an
//! evacuation pause kept in place (page
//! `docs/internal/gc/jr20-g1-young-pause-keeps-every-referent-in-a-jit-pinned-region-FIXED-20261016.md`).
//!
//! # The defect
//!
//! A young region a pause cannot evacuate (a JIT pin -- an unrewritable
//! conservative band word, or a frozen peer's TLAB tail --, a JNI critical
//! pin, an evacuation failure) stays where it is, and every object in it, dead
//! or alive, stays below its cursor. The post-pause survival verdict for an
//! address the pause did not move (`VmHeap::watched_pre_gc_addr_survived`, G1
//! arm `is_addr_in_live_region`) is REGION-granular, so every weak / phantom
//! referent in such a region read live: `new WeakReference<>(new Object())`
//! held by a compiled frame's register kept its dead referent on every young
//! pause, and only a marking cycle could clear it. HotSpot clears it: a pinned
//! region is evacuated in place, and liveness comes from the copy closure, not
//! from region membership.
//!
//! # The verdict, and why it is sound
//!
//! [`G1Collector::pinned_young_referents_unheld`] answers, for candidate
//! addresses the VM hands it (the weak / phantom `Reference` objects and
//! referents the pause neither moved nor identity-mapped), which ones NOTHING
//! holds after the pause. "Holds" is an over-approximation of reachability that
//! never under-approximates it, built from exactly the vocabulary the pause
//! itself uses to keep a collection-set object alive -- had the candidate's
//! region been in the collection set, these are the places its survival would
//! have come from:
//!
//! * a root word (the pause's root set, which carries the precise roots, the
//!   frozen peers' conservative words and the native / side-table roots), or a
//!   published JIT pin word (`gc_quiescence::pinned_jit_roots_snapshot`) --
//!   matched against the candidate's whole extent, so an interior word holds;
//! * a reference slot of ANY object (garbage included: a dead holder is still
//!   a holder here, which can only over-retain) in every young region after
//!   the pause (the to-space Survivor copies, every kept young region), and in
//!   every remembered-set source of a candidate's region. The barrier records
//!   every cross-region store (`post_write_barrier_rset`; G1AUD-4) and Phase 4
//!   re-records the edges the evacuation itself wrote
//!   (`collect_outgoing_cross_region_edges_noting_young`), which is the
//!   completeness every young pause already relies on to keep a young object
//!   an Old object names; a coarsened set names every region;
//! * an overlay row (`g1_overlay.rs`): every row of every owner, whatever the
//!   owner (the pause roots the rows of owners outside its collection set).
//!
//! An object's slot naming the object itself is not a holder (a dead
//! self-cycle). A `Reference`'s slot 0 is null here for every row the
//! pre-collection pass nulled, so it is never a holder for those; a slot the
//! pass left alone (a policy-kept soft referent) is one, as on HotSpot.
//!
//! A candidate nothing holds is unreachable: the VM's round then clears and
//! enqueues it as for an evacuated region. Nothing is freed or moved -- its
//! region stays where the pause left it -- so a wrong "unheld" could only be a
//! wrong clear, never memory corruption; that is why every way of not knowing
//! answers "no verdict" (`Err`) and leaves the region-granular answer in place:
//!
//! * the last collection of this `collect_garbage` was not an evacuation pause
//!   that kept a young region ([`G1Collector::pinned_referent_verdict_armed`];
//!   since gci i32/ec an evacuation pause whose collection set came out empty
//!   because every young region was pinned out counts as one --
//!   [`G1Collector::note_refproc_empty_cset_pause`] -- a refused pause does
//!   not);
//! * a marking cycle is open (its SATB keep-alive and gray set are holders
//!   this walk does not read);
//! * a walked region does not parse to its cursor;
//! * the walk would exceed [`PINNED_REFERENT_SCAN_BUDGET`].
//!
//! No candidate in a kept young region: no walk at all.
//!
//! # One candidate list per pause (gci i28/wt)
//!
//! `docs/internal/gc/gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016.md`:
//! the VM takes this verdict ONCE per pause (`run_collection_pause`), over the
//! weak / phantom rows AND every uncleared JNI weak global's referent, and
//! carries the unheld set on the pause's `addr_keyed::RelocationTargets`, so
//! the reference round, the JNI weak-global sweep and every side-table sweep
//! that takes `InPlaceVerdict::survived` refuse the same objects. That is
//! sound because a candidate's verdict does not depend on the other
//! candidates: "held" is decided by the walk, the root and pin words and the
//! overlay rows, none of which reads the candidate list except to name what
//! it found; more candidates can only add regions (their remembered-set
//! sources) to the walk, which can only add holders. Test
//! `gci_i28_wt_a_candidate_s_verdict_does_not_depend_on_the_others`. The one
//! dependence is a DECLINE: a larger walk may cross the budget or meet an
//! unparsable region, and then no candidate gets a verdict (the
//! region-granular answer stands for all of them -- over-retention, never a
//! wrong clear).
//!
//! # Holders the collection resurrected (gci i30/rp)
//!
//! `i27-gr-report.md` review item 4: a dead finalizable `X` in the collection
//! set is copied by Phase 3.5 (`resurrect_dead_finalizers`) so `finalize()`
//! can run; its copy sits in a young region after the pause, so the walk above
//! found it as a holder and a weak referent `W` in a kept region that only `X`
//! names was HELD -- its `WeakReference` survived the pause. HotSpot clears it
//! (finalizer-reachable is not weakly reachable). The walk now books the slots
//! of every object in `resurrection_holders` (the collection's resurrection
//! closure at its final addresses) apart: a candidate only they name is
//! [`G1PinnedReferentVerdict::finalizer_only`], not `unheld`. It stays HELD for
//! every in-place verdict (JNI weak globals keep phantom strength, side-table
//! rows keep their owner, which `finalize()` may still use), and the VM notes
//! the weak referents among them as resurrected for its reference round only
//! (`ReferenceProcessor::note_resurrected_finalizables`: unmarked for soft and
//! weak, marked for final and phantom) -- the rule every evacuated object of
//! the same closure already gets. Sound for the same reason: the closure
//! holds only objects the strong closure had not copied. A chain through a
//! kept-region object (`X` -> `Y` in a kept region -> `W`) still holds `W`:
//! `Y` is not in the closure (it was not copied) and a kept-region holder,
//! dead or alive, holds -- the over-retention the walk already accepts.
//!
//! # Private children of a kept finalizable (gci i32/ec)
//!
//! gci i31/gq queues a dead registered finalizable `F` of a kept region at
//! the pause (the VM splits it out of the verdict). Its children were
//! another matter: a kept region's outgoing edges are sources of the pause,
//! so a child `C` of `F` in the collection set was COPIED as if strongly
//! reachable, and a `WeakReference` to `C` survived the pause; HotSpot clears
//! it (finalizer-reachable is not weakly reachable; the w7-d depth-2 case,
//! which the evacuated finalizables get from Phase 3.5's closure).
//! [`G1Collector::kept_finalizables_private_closure`] computes, from the dead
//! `F`s, the closure of young objects their slots reach (walk by walk: a
//! frontier word is only compared, never read, until a walk parses it as an
//! object start), then which members something OUTSIDE the closure holds --
//! the verdict's vocabulary: root and pin words, overlay rows, a slot of any
//! other object of the walk (garbage included; the slots of this
//! collection's Phase 3.5 holders are finalizer-reachable and do not count) --
//! and everything such a member reaches inside the closure. The rest is
//! `finalizer_only`: the VM maps those to pre-collection addresses and notes
//! the weak referents among them as resurrected for its round, exactly as for
//! an evacuated finalizable's closure. Nothing is freed or moved by the
//! answer: a wrong one could only be a wrong clear, so the walk cap and the
//! budget only stop early (the last walk's answer, or none), and every member
//! the walk could not confirm is kept. A cycle inside the closure that an
//! outside object names is strong as a whole; an old-generation child
//! (promoted by the pause) is not followed (kept).

use super::*;

/// Bytes the holder walk may cover in one pause before it declines (the
/// region-granular answer stands). The walk is every young region after the
/// pause plus the candidate regions' remembered-set sources.
pub(super) const PINNED_REFERENT_SCAN_BUDGET: usize = 64 << 20;

/// gci i32/ec: `CRATONVM_G1_PINNED_REFERENT_EMPTY_CSET` -- default ON (an
/// evacuation pause whose collection set is empty because every young region
/// was pinned out arms the per-object verdict, as HotSpot clears a dead
/// referent of a pinned region at that pause), `=0` keeps such a pause
/// unarmed (the region-granular answer until a later pause has a collection
/// set). Read only on an empty-CSet pause that excluded a young region; no
/// `static`.
pub(super) fn g1_pinned_referent_empty_cset_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_G1_PINNED_REFERENT_EMPTY_CSET")
}

/// What [`G1Collector::pinned_young_referents_unheld`] decided, and what it
/// cost (the VM's `CRATONVM_DBG_G1_PINNED_REFERENTS` line).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct G1PinnedReferentVerdict {
    /// The candidates nothing holds: object starts in a young region the pause
    /// kept, judged unreachable. A subset of the candidates.
    pub unheld: FxHashSet<usize>,
    /// gci i30/rp: the candidates held ONLY by a reference slot of an object
    /// this collection's finalizer resurrection copied (`resurrection_holders`:
    /// a dead finalizable and its fresh closure) -- no root word, pin word,
    /// overlay row or other object's slot names them. Finalizer-reachable,
    /// not strongly reachable: HotSpot clears a soft / weak reference to such
    /// an object before `finalize()` runs, and keeps phantom / JNI-weak
    /// strength. Disjoint from `unheld` (they are held, so every in-place
    /// verdict keeps them); the VM notes the weak referents among them as
    /// resurrected for its reference round only.
    pub finalizer_only: FxHashSet<usize>,
    /// Candidates that lie below the cursor of a young region.
    pub candidates: usize,
    /// Of those, the ones the walk found as object starts.
    pub confirmed: usize,
    /// Regions walked.
    pub regions_walked: usize,
    /// Bytes walked.
    pub bytes_walked: usize,
}

/// gci i32/ec: walks [`G1Collector::kept_finalizables_private_closure`] may
/// take (one per closure depth it discovers); past it the last walk's answer
/// stands, which only over-retains.
pub(super) const KEPT_FINALIZABLE_CLOSURE_MAX_WALKS: usize = 4;

/// gci i32/ec: what [`G1Collector::kept_finalizables_private_closure`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct G1KeptFinalizableClosure {
    /// POST-pause addresses of young objects (copies the pause made, or
    /// objects of a kept region) that only the dead kept-region finalizables
    /// (the seeds, never listed here) and their closure reach: no root word,
    /// pin word, overlay row, or slot of an object outside that closure names
    /// them, nor of one a closure member strongly held reaches. Finalizer-
    /// reachable, as the w7-d resurrection closure of an evacuated
    /// finalizable.
    pub finalizer_only: FxHashSet<usize>,
    /// Confirmed closure members, seeds included.
    pub closure: usize,
    /// Walks taken.
    pub walks: usize,
    /// Bytes walked over all walks.
    pub bytes_walked: usize,
    /// The last walk saw the whole closure (no new member past it).
    pub complete: bool,
    /// gci i38/ec (diagnostic, the `CRATONVM_DBG_G1_PINNED_REFERENTS` line):
    /// in the last walk, how many slots of objects outside the closure, root
    /// words, published JIT pin words and overlay row values named a member.
    /// A member a root or pin word names is strongly held by this vocabulary
    /// (a stale compiled-frame word included).
    pub held_by_slot: usize,
    /// See [`Self::held_by_slot`].
    pub held_by_root: usize,
    /// See [`Self::held_by_slot`].
    pub held_by_pin: usize,
    /// See [`Self::held_by_slot`].
    pub held_by_overlay: usize,
    /// gci i41/hc (diagnostic, the VM's `CRATONVM_G1_DBG_CLOSURE_ROOT_ENTRIES`
    /// census): WHICH word named a member in the last walk -- every root
    /// entry (by its index in the pause's root slice), published pin word,
    /// overlay row value and outside slot the counts above count, in walk
    /// order, at most [`KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX`]. Recorded on
    /// every call (a handful of pushes on the one pause that found a dead
    /// kept-region finalizable); read only by the diagnostic. Changes no
    /// answer.
    pub holders: Vec<G1ClosureHolder>,
    /// Holders past [`KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX`] (counted, not
    /// recorded).
    pub holders_dropped: usize,
    /// gci i41/hc (diagnostic): the confirmed members of the last walk,
    /// `(post-collection start, size)`, ascending, seeds included.
    pub members: Vec<(usize, usize)>,
}

/// gci i41/hc: at most this many [`G1KeptFinalizableClosure::holders`] are
/// recorded per call; the rest are only counted.
pub(super) const KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX: usize = 32;

/// gci i41/hc: who published a pin word that named a closure member
/// ([`G1ClosureHolderKind::Pin`]). The three halves partition
/// `gc_quiescence::pinned_jit_roots_snapshot` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G1ClosurePinSource {
    /// The collecting thread's own entry (its root scan's compiled frames).
    Own,
    /// This cycle's helper-window words (a peer the collector froze and read
    /// itself: `gc_quiescence::xt_cycle_pinned_jit_roots_snapshot`).
    HelperWindow,
    /// Another thread's published entry: a parked or blocked peer's compiled
    /// frames, deposited by that thread at its safepoint / blocking region.
    Peer,
}

/// gci i41/hc: the vocabulary item that named a closure member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G1ClosureHolderKind {
    /// Entry `index` of the pause's root slice (`roots`).
    Root {
        /// Index into the root slice the closure was given.
        index: usize,
    },
    /// A published JIT pin word.
    Pin {
        /// Its publisher.
        source: G1ClosurePinSource,
    },
    /// An overlay row value.
    Overlay,
    /// A reference slot of `holder`, an object outside the closure.
    Slot {
        /// The holding object's (post-collection) start.
        holder: usize,
    },
}

/// gci i41/hc: one word that named a member of the kept finalizables'
/// closure in its last walk ([`G1KeptFinalizableClosure::holders`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct G1ClosureHolder {
    /// What named it.
    pub kind: G1ClosureHolderKind,
    /// The word as the closure read it (post-collection; may be interior).
    pub word: usize,
    /// The member's (post-collection) start.
    pub member: usize,
}

/// gci i41/hc: record `h` unless the cap is reached (then count it). Pure.
fn push_closure_holder(
    holders: &mut Vec<G1ClosureHolder>,
    dropped: &mut usize,
    h: G1ClosureHolder,
) {
    if holders.len() < KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX {
        holders.push(h);
    } else {
        *dropped += 1;
    }
}

/// gci i41/hc: the publisher of pin word `word`, given the collecting
/// thread's own words and this cycle's helper-window words (the rest of the
/// snapshot is other threads' entries). Pure.
fn closure_pin_source(
    word: usize,
    own: &FxHashSet<usize>,
    helper: &FxHashSet<usize>,
) -> G1ClosurePinSource {
    if own.contains(&word) {
        G1ClosurePinSource::Own
    } else if helper.contains(&word) {
        G1ClosurePinSource::HelperWindow
    } else {
        G1ClosurePinSource::Peer
    }
}

/// Why [`G1Collector::pinned_young_referents_unheld`] gave no verdict. Every
/// one of them leaves the region-granular answer in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G1PinnedReferentDecline {
    /// No evacuation pause that kept a young region is the last collection.
    NotArmed,
    /// A marking cycle is open.
    MarkActive,
    /// No candidate lies in a young region.
    NoCandidates,
    /// The walk would exceed the budget.
    OverBudget,
    /// A walked region did not parse to its cursor.
    Unparsable,
}

impl G1Collector {
    /// Start of `collect_garbage`: no verdict until an evacuation pause of this
    /// collection records itself.
    ///
    /// gci i30/rp: also forgets the last collection's resurrection holders, so
    /// a plain `collect_garbage` (no finalizer candidates, which never writes
    /// the set) is never judged with an older collection's holders.
    pub(super) fn reset_refproc_kept_young_token(&self) {
        self.refproc_kept_young_token.store(0, Ordering::Relaxed);
        self.resurrection_holders.lock().clear();
    }

    /// Called by the record funnel (`record_collection_with_phases_within`)
    /// right after it counted the collection: an evacuation pause (young or
    /// mixed, or a drain pass of one) arms the verdict for this collection
    /// count; it keeps the "kept a young region" bit of an earlier record of
    /// the same `collect_garbage` (a drain pass after an evacuation failure
    /// records again, with its own phases).
    pub(super) fn note_refproc_evacuation_record(
        &self,
        collection_type: G1CollectionType,
        phases: &G1PausePhases,
    ) {
        if collection_type == G1CollectionType::Full {
            self.refproc_kept_young_token.store(0, Ordering::Relaxed);
            return;
        }
        let count = self.collection_count.load(Ordering::Relaxed);
        let kept_now = phases.jit_pinned_out > 0
            || phases.jni_pinned_out > 0
            || phases.evacuation_failed
            || phases.drain_pass;
        let prev = self.refproc_kept_young_token.load(Ordering::Relaxed);
        let kept = kept_now || (prev != 0 && prev & 1 == 1);
        self.refproc_kept_young_token
            .store((count << 1) | u64::from(kept), Ordering::Relaxed);
    }

    /// gci i32/ec (`i30-rp-report.md` hand-back 1): an evacuation pause whose
    /// collection set came out EMPTY because every young region was pinned
    /// out (`young_pinned_out` > 0: a JNI critical pin or a JIT pin kept each
    /// of them) returns before the record funnel, so
    /// [`Self::note_refproc_evacuation_record`] never ran and every referent in
    /// those regions kept the region-granular answer until a later pause had a
    /// collection set. HotSpot evacuates a pinned region in place and clears a
    /// dead referent at that very pause. Such a pause is still a pause the VM
    /// runs its reference round after, with the world stopped and nothing
    /// moved, which is exactly the state the verdict reads (every young region
    /// is a kept one; their incoming edges are remembered-set sources as on
    /// every pause), so it arms the verdict for the CURRENT collection count
    /// (the empty pause records nothing, so the count does not move; the next
    /// record or `collect_garbage` disarms it).
    ///
    /// Only called from the empty-CSet returns of the evacuation drivers
    /// (`young_collection_serial`, `young_collection_parallel`,
    /// `mixed_collection_parallel`); a REFUSED pause (`collect_garbage`'s
    /// coverage / empty-publication / GPU refusals) returns before any driver
    /// runs and still arms nothing, and a pause with no young region at all
    /// (`young_pinned_out` == 0) has nothing to judge. Kill:
    /// `CRATONVM_G1_PINNED_REFERENT_EMPTY_CSET=0`.
    pub(super) fn note_refproc_empty_cset_pause(&self, young_pinned_out: usize) {
        if young_pinned_out == 0 || !g1_pinned_referent_empty_cset_enabled() {
            return;
        }
        let count = self.collection_count.load(Ordering::Relaxed);
        self.refproc_kept_young_token
            .store((count << 1) | 1, Ordering::Relaxed);
    }

    /// Is the last collection an evacuation pause that kept a young region in
    /// place, with no other collection since? The VM asks before it gathers
    /// candidates, so a pause that kept nothing costs one load.
    pub fn pinned_referent_verdict_armed(&self) -> bool {
        let token = self.refproc_kept_young_token.load(Ordering::Relaxed);
        token & 1 == 1 && token >> 1 == self.collection_count.load(Ordering::Relaxed)
    }

    /// The per-object verdict: which of `candidates` (PRE-collection
    /// addresses the pause neither moved nor identity-mapped) lie in a young
    /// region the pause kept and are held by nothing. `roots` are the pause's
    /// root words, as the collection left them. See the module doc.
    ///
    /// Stop-the-world, after the collection and before the next one, with no
    /// heap lock held by the caller.
    pub fn pinned_young_referents_unheld(
        &self,
        candidates: &[usize],
        roots: &[usize],
    ) -> Result<G1PinnedReferentVerdict, G1PinnedReferentDecline> {
        self.pinned_young_referents_unheld_within(
            candidates,
            roots,
            PINNED_REFERENT_SCAN_BUDGET,
            true,
        )
    }

    /// [`Self::pinned_young_referents_unheld`] with the budget and the overlay
    /// read as parameters (the unit tests drive both).
    pub(super) fn pinned_young_referents_unheld_within(
        &self,
        candidates: &[usize],
        roots: &[usize],
        budget: usize,
        read_overlays: bool,
    ) -> Result<G1PinnedReferentVerdict, G1PinnedReferentDecline> {
        if !self.pinned_referent_verdict_armed() {
            return Err(G1PinnedReferentDecline::NotArmed);
        }
        if self.satb_queue.is_active() || self.overlay_snapshot_len.load(Ordering::Acquire) != 0 {
            return Err(G1PinnedReferentDecline::MarkActive);
        }
        let mut verdict = G1PinnedReferentVerdict::default();
        // Candidate -> its object size once the walk confirms it as an object
        // start (0 until then).
        let mut cand: FxHashMap<usize, usize> = FxHashMap::default();
        let mut held: FxHashSet<usize> = FxHashSet::default();
        // gci i30/rp (`i27-gr-report.md` review item 4): what only the
        // objects this collection resurrected for `finalize()` hold. Taken
        // (a copy; a leaf lock, released) before the regions guard.
        let resurrection_holders: FxHashSet<usize> =
            self.resurrection_holders.lock().iter().copied().collect();
        let mut held_by_resurrected: FxHashSet<usize> = FxHashSet::default();
        {
            let regions = self.regions.read();
            let mut cand_regions = RegionSet::new();
            for &addr in candidates {
                if addr == 0 || addr & 0x7 != 0 {
                    continue;
                }
                let Some(idx) = self.lookup_region_for_addr(addr) else {
                    continue;
                };
                let Some(r) = regions.get(idx) else {
                    continue;
                };
                if !matches!(r.region_type(), RegionType::Eden | RegionType::Survivor) {
                    continue;
                }
                let base = r.data.as_ptr() as usize;
                if addr < base || addr >= base + r.cursor() {
                    continue;
                }
                cand.insert(addr, 0);
                cand_regions.insert(idx);
            }
            if cand.is_empty() {
                return Err(G1PinnedReferentDecline::NoCandidates);
            }
            verdict.candidates = cand.len();

            // The regions to walk: every young region, and every live
            // remembered-set source of a candidate's region (a humongous
            // continuation source is walked from its span's start).
            let mut walk = RegionSet::new();
            for (i, r) in regions.iter().enumerate() {
                if matches!(r.region_type(), RegionType::Eden | RegionType::Survivor) {
                    walk.insert(i);
                }
            }
            let cand_list: Vec<usize> = cand_regions.iter().collect();
            for src in Self::live_rset_sources(&regions, &cand_list).iter() {
                match regions.get(src).map(|r| r.region_type()) {
                    None | Some(RegionType::Free) => {}
                    Some(RegionType::HumongousContinuation) => {
                        if let Some(start) = humongous_span_start(&regions, src) {
                            walk.insert(start);
                        }
                    }
                    Some(_) => {
                        walk.insert(src);
                    }
                }
            }
            let bytes: usize = walk
                .iter()
                .filter_map(|i| regions.get(i))
                .filter(|r| {
                    !matches!(
                        r.region_type(),
                        RegionType::Free | RegionType::HumongousContinuation
                    )
                })
                .map(|r| r.cursor())
                .fold(0usize, |a, b| a.saturating_add(b));
            if bytes > budget {
                return Err(G1PinnedReferentDecline::OverBudget);
            }
            verdict.bytes_walked = bytes;

            let jit_skips = self.jit_tlab_skip_spans();
            for idx in walk.iter() {
                let Some(r) = regions.get(idx) else {
                    continue;
                };
                if matches!(
                    r.region_type(),
                    RegionType::Free | RegionType::HumongousContinuation
                ) {
                    continue;
                }
                verdict.regions_walked += 1;
                let base = r.data.as_ptr() as usize;
                let used = r.cursor();
                let mut offset = 0usize;
                while offset < used {
                    let addr = base + offset;
                    if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, addr) {
                        offset += skip;
                        continue;
                    }
                    if let Some(gap) = gap_filler_len(addr as *const u8) {
                        offset += gap;
                        continue;
                    }
                    // SAFETY: below the region's cursor, inside its allocated
                    // prefix, with the world stopped and the regions guard held.
                    let header = unsafe { &*(addr as *const ObjectHeader) };
                    if is_humongous_filler(header) {
                        // The filler covers the rest of the region.
                        break;
                    }
                    let size = object_total_size(header);
                    if size < HEADER_SIZE || size > used - offset {
                        return Err(G1PinnedReferentDecline::Unparsable);
                    }
                    if let Some(s) = cand.get_mut(&addr) {
                        *s = size;
                    }
                    // gci i30/rp: a holder the finalizer resurrection copied
                    // is finalizer-reachable only; what it names is booked
                    // apart, so a weak referent nothing else holds is cleared
                    // as HotSpot clears it before `finalize()`.
                    let into = if resurrection_holders.contains(&addr) {
                        &mut held_by_resurrected
                    } else {
                        &mut held
                    };
                    self.note_pinned_referent_holder_slots(
                        &regions,
                        addr as *mut u8,
                        header,
                        &cand,
                        into,
                    );
                    offset += size;
                }
            }
        }

        // Root and pin words: any word inside a confirmed candidate's extent.
        let mut extents: Vec<(usize, usize)> = cand
            .iter()
            .filter(|&(_, &size)| size != 0)
            .map(|(&addr, &size)| (addr, size))
            .collect();
        extents.sort_unstable();
        verdict.confirmed = extents.len();
        let covering = |word: usize| -> Option<usize> {
            let i = extents.partition_point(|&(a, _)| a <= word);
            let (a, s) = *extents.get(i.checked_sub(1)?)?;
            (word < a + s).then_some(a)
        };
        for &word in roots {
            if let Some(a) = covering(word) {
                held.insert(a);
            }
        }
        for word in crate::gc_quiescence::pinned_jit_roots_snapshot() {
            if let Some(a) = covering(word) {
                held.insert(a);
            }
        }
        if read_overlays && crate::external_roots::has_providers() {
            let (arena_base, arena_end) = (self.arena_base, self.arena_end);
            let in_heap = move |owner: usize| owner >= arena_base && owner < arena_end;
            if crate::external_roots::has_row_providers() {
                for (_, row) in crate::external_roots::all_owner_rows(&in_heap) {
                    for r in &row.refs {
                        if let Some(a) = covering(r.as_ptr() as usize) {
                            held.insert(a);
                        }
                    }
                }
            }
            if crate::external_roots::has_rowless_providers() {
                for r in crate::external_roots::rowless_roots_for_matching_owners(&|_| true) {
                    if let Some(a) = covering(r.as_ptr() as usize) {
                        held.insert(a);
                    }
                }
            }
        }
        verdict.unheld = extents
            .iter()
            .map(|&(a, _)| a)
            .filter(|a| !held.contains(a) && !held_by_resurrected.contains(a))
            .collect();
        verdict.finalizer_only = extents
            .iter()
            .map(|&(a, _)| a)
            .filter(|a| !held.contains(a) && held_by_resurrected.contains(a))
            .collect();
        Ok(verdict)
    }

    /// gci i32/ec: the young objects reachable ONLY through the dead
    /// finalizables a kept-region verdict found (`dead`, their unchanged
    /// addresses; `G1PinnedReferentVerdict` split by the VM, gci i31/gq).
    /// See the module doc "Private children of a kept finalizable".
    ///
    /// Same preconditions and declines as
    /// [`Self::pinned_young_referents_unheld`]; `roots` are the pause's root
    /// words as the collection left them. Stop-the-world, after the
    /// collection, with no heap lock held by the caller.
    pub fn kept_finalizables_private_closure(
        &self,
        dead: &[usize],
        roots: &[usize],
    ) -> Result<G1KeptFinalizableClosure, G1PinnedReferentDecline> {
        self.kept_finalizables_private_closure_within(
            dead,
            roots,
            PINNED_REFERENT_SCAN_BUDGET,
            true,
            KEPT_FINALIZABLE_CLOSURE_MAX_WALKS,
        )
    }

    /// [`Self::kept_finalizables_private_closure`] with the budget, the
    /// overlay read and the walk cap as parameters (the unit tests drive
    /// them).
    pub(super) fn kept_finalizables_private_closure_within(
        &self,
        dead: &[usize],
        roots: &[usize],
        budget: usize,
        read_overlays: bool,
        max_walks: usize,
    ) -> Result<G1KeptFinalizableClosure, G1PinnedReferentDecline> {
        if !self.pinned_referent_verdict_armed() {
            return Err(G1PinnedReferentDecline::NotArmed);
        }
        if self.satb_queue.is_active() || self.overlay_snapshot_len.load(Ordering::Acquire) != 0 {
            return Err(G1PinnedReferentDecline::MarkActive);
        }
        let mut out = G1KeptFinalizableClosure::default();
        // Holders this collection's Phase 3.5 resurrected: finalizer-reachable
        // too, so their slots are not strong holders (as in the verdict).
        let resurrection_holders: FxHashSet<usize> =
            self.resurrection_holders.lock().iter().copied().collect();
        let seeds: FxHashSet<usize> = dead.iter().copied().filter(|&a| a != 0).collect();
        // The closure so far: member -> its size once a walk confirmed it as
        // an object start (0 until then). Only a CONFIRMED member's slots are
        // read; a frontier word is only compared, never dereferenced.
        let mut members: FxHashMap<usize, usize> = FxHashMap::default();
        // Members some word or slot outside the closure names (the last walk).
        let mut external: FxHashSet<usize> = FxHashSet::default();
        // Member -> member edges (the last walk).
        let mut edges: Vec<(usize, usize)> = Vec::new();
        let mut frontier: Vec<usize> = seeds.iter().copied().collect();
        loop {
            let mut next: Vec<usize> = Vec::new();
            // gci i38/ec: this walk's holder census (diagnostic only).
            let (mut by_slot, mut by_root, mut by_pin, mut by_overlay) =
                (0usize, 0usize, 0usize, 0usize);
            // gci i41/hc: WHICH words those were (diagnostic only; capped).
            let mut holders: Vec<G1ClosureHolder> = Vec::new();
            let mut holders_dropped = 0usize;
            {
                let regions = self.regions.read();
                let mut admitted = 0usize;
                for addr in frontier.drain(..) {
                    if addr == 0 || addr & 0x7 != 0 || members.contains_key(&addr) {
                        continue;
                    }
                    let Some(idx) = self.lookup_region_for_addr(addr) else {
                        continue;
                    };
                    let Some(r) = regions.get(idx) else {
                        continue;
                    };
                    if !matches!(r.region_type(), RegionType::Eden | RegionType::Survivor) {
                        continue;
                    }
                    let base = r.data.as_ptr() as usize;
                    if addr < base || addr >= base + r.cursor() {
                        continue;
                    }
                    members.insert(addr, 0);
                    admitted += 1;
                }
                if out.walks == 0 && admitted == 0 {
                    return Err(G1PinnedReferentDecline::NoCandidates);
                }
                if admitted == 0 {
                    // The last walk saw the whole closure.
                    out.complete = true;
                    break;
                }
                if out.walks >= max_walks {
                    // The members just admitted were outside the closure for
                    // the last walk: their slots counted as outside holders,
                    // so the last walk's answer only over-retains.
                    break;
                }
                // The walk: every young region and every live remembered-set
                // source of a member's region (the verdict's walk).
                let mut member_regions = RegionSet::new();
                for &a in members.keys() {
                    if let Some(idx) = self.lookup_region_for_addr(a) {
                        member_regions.insert(idx);
                    }
                }
                let mut walk = RegionSet::new();
                for (i, r) in regions.iter().enumerate() {
                    if matches!(r.region_type(), RegionType::Eden | RegionType::Survivor) {
                        walk.insert(i);
                    }
                }
                let member_list: Vec<usize> = member_regions.iter().collect();
                for src in Self::live_rset_sources(&regions, &member_list).iter() {
                    match regions.get(src).map(|r| r.region_type()) {
                        None | Some(RegionType::Free) => {}
                        Some(RegionType::HumongousContinuation) => {
                            if let Some(start) = humongous_span_start(&regions, src) {
                                walk.insert(start);
                            }
                        }
                        Some(_) => {
                            walk.insert(src);
                        }
                    }
                }
                let bytes: usize = walk
                    .iter()
                    .filter_map(|i| regions.get(i))
                    .filter(|r| {
                        !matches!(
                            r.region_type(),
                            RegionType::Free | RegionType::HumongousContinuation
                        )
                    })
                    .map(|r| r.cursor())
                    .fold(0usize, |a, b| a.saturating_add(b));
                if bytes > budget {
                    if out.walks == 0 {
                        return Err(G1PinnedReferentDecline::OverBudget);
                    }
                    // Keep the last walk's answer (over-retains, as above).
                    break;
                }
                external.clear();
                edges.clear();
                for s in members.values_mut() {
                    *s = 0;
                }
                out.walks += 1;
                out.bytes_walked = out.bytes_walked.saturating_add(bytes);
                let jit_skips = self.jit_tlab_skip_spans();
                for idx in walk.iter() {
                    let Some(r) = regions.get(idx) else {
                        continue;
                    };
                    if matches!(
                        r.region_type(),
                        RegionType::Free | RegionType::HumongousContinuation
                    ) {
                        continue;
                    }
                    let base = r.data.as_ptr() as usize;
                    let used = r.cursor();
                    let mut offset = 0usize;
                    while offset < used {
                        let addr = base + offset;
                        if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, addr) {
                            offset += skip;
                            continue;
                        }
                        if let Some(gap) = gap_filler_len(addr as *const u8) {
                            offset += gap;
                            continue;
                        }
                        // SAFETY: below the region's cursor, inside its
                        // allocated prefix, with the world stopped and the
                        // regions guard held.
                        let header = unsafe { &*(addr as *const ObjectHeader) };
                        if is_humongous_filler(header) {
                            break;
                        }
                        let size = object_total_size(header);
                        if size < HEADER_SIZE || size > used - offset {
                            return Err(G1PinnedReferentDecline::Unparsable);
                        }
                        let is_member = match members.get_mut(&addr) {
                            Some(s) => {
                                *s = size;
                                true
                            }
                            None => false,
                        };
                        if is_member {
                            // A confirmed member: its slots extend the
                            // closure (members) or name the next frontier.
                            self.for_each_pinned_referent_holder_slot(
                                &regions,
                                addr as *mut u8,
                                header,
                                |raw| {
                                    if raw == 0 || raw == addr {
                                        return;
                                    }
                                    if members.contains_key(&raw) {
                                        edges.push((addr, raw));
                                    } else {
                                        next.push(raw);
                                    }
                                },
                            );
                        } else if !resurrection_holders.contains(&addr) {
                            // Any other object, garbage included, holds what
                            // it names (over-retention only).
                            self.for_each_pinned_referent_holder_slot(
                                &regions,
                                addr as *mut u8,
                                header,
                                |raw| {
                                    if raw != addr && members.contains_key(&raw) {
                                        external.insert(raw);
                                        by_slot += 1;
                                        push_closure_holder(
                                            &mut holders,
                                            &mut holders_dropped,
                                            G1ClosureHolder {
                                                kind: G1ClosureHolderKind::Slot { holder: addr },
                                                word: raw,
                                                member: raw,
                                            },
                                        );
                                    }
                                },
                            );
                        }
                        offset += size;
                    }
                }
            }
            // Root and pin words, overlay rows: any word inside a confirmed
            // member's extent (the regions guard is released, as in the
            // verdict).
            let mut extents: Vec<(usize, usize)> = members
                .iter()
                .filter(|&(_, &size)| size != 0)
                .map(|(&addr, &size)| (addr, size))
                .collect();
            extents.sort_unstable();
            let covering = |word: usize| -> Option<usize> {
                let i = extents.partition_point(|&(a, _)| a <= word);
                let (a, s) = *extents.get(i.checked_sub(1)?)?;
                (word < a + s).then_some(a)
            };
            for (index, &word) in roots.iter().enumerate() {
                if let Some(a) = covering(word) {
                    external.insert(a);
                    by_root += 1;
                    push_closure_holder(
                        &mut holders,
                        &mut holders_dropped,
                        G1ClosureHolder {
                            kind: G1ClosureHolderKind::Root { index },
                            word,
                            member: a,
                        },
                    );
                }
            }
            // gci i41/hc: the snapshot split by publisher (the union is
            // exactly `pinned_jit_roots_snapshot`, so every answer above is
            // unchanged); the helper-window words are asked for only when a
            // pin word named a member.
            let (own_pins, other_pins) = crate::gc_quiescence::pinned_jit_roots_snapshot_split();
            let own_pin_set: FxHashSet<usize> = own_pins.iter().copied().collect();
            let mut helper_pin_set: Option<FxHashSet<usize>> = None;
            for word in own_pins.into_iter().chain(other_pins) {
                if let Some(a) = covering(word) {
                    external.insert(a);
                    by_pin += 1;
                    let helper = helper_pin_set.get_or_insert_with(|| {
                        crate::gc_quiescence::xt_cycle_pinned_jit_roots_snapshot()
                            .into_iter()
                            .collect()
                    });
                    push_closure_holder(
                        &mut holders,
                        &mut holders_dropped,
                        G1ClosureHolder {
                            kind: G1ClosureHolderKind::Pin {
                                source: closure_pin_source(word, &own_pin_set, helper),
                            },
                            word,
                            member: a,
                        },
                    );
                }
            }
            if read_overlays && crate::external_roots::has_providers() {
                let (arena_base, arena_end) = (self.arena_base, self.arena_end);
                let in_heap = move |owner: usize| owner >= arena_base && owner < arena_end;
                if crate::external_roots::has_row_providers() {
                    for (_, row) in crate::external_roots::all_owner_rows(&in_heap) {
                        for r in &row.refs {
                            if let Some(a) = covering(r.as_ptr() as usize) {
                                external.insert(a);
                                by_overlay += 1;
                                push_closure_holder(
                                    &mut holders,
                                    &mut holders_dropped,
                                    G1ClosureHolder {
                                        kind: G1ClosureHolderKind::Overlay,
                                        word: r.as_ptr() as usize,
                                        member: a,
                                    },
                                );
                            }
                        }
                    }
                }
                if crate::external_roots::has_rowless_providers() {
                    for r in crate::external_roots::rowless_roots_for_matching_owners(&|_| true) {
                        if let Some(a) = covering(r.as_ptr() as usize) {
                            external.insert(a);
                            by_overlay += 1;
                            push_closure_holder(
                                &mut holders,
                                &mut holders_dropped,
                                G1ClosureHolder {
                                    kind: G1ClosureHolderKind::Overlay,
                                    word: r.as_ptr() as usize,
                                    member: a,
                                },
                            );
                        }
                    }
                }
            }
            out.held_by_slot = by_slot;
            out.held_by_root = by_root;
            out.held_by_pin = by_pin;
            out.held_by_overlay = by_overlay;
            out.holders = holders;
            out.holders_dropped = holders_dropped;
            out.members = extents;
            frontier = next;
        }

        // Strongly held: a member something outside the closure names, and
        // everything such a member reaches inside the closure. An unconfirmed
        // member (never parsed as an object start) is not reported.
        let mut adjacency: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
        for &(from, to) in &edges {
            adjacency.entry(from).or_default().push(to);
        }
        let mut strong: FxHashSet<usize> = FxHashSet::default();
        let mut work: Vec<usize> = Vec::new();
        for &a in &external {
            if strong.insert(a) {
                work.push(a);
            }
        }
        while let Some(a) = work.pop() {
            if let Some(tos) = adjacency.get(&a) {
                for &t in tos {
                    if strong.insert(t) {
                        work.push(t);
                    }
                }
            }
        }
        out.closure = members.values().filter(|&&s| s != 0).count();
        out.finalizer_only = members
            .iter()
            .filter(|&(a, &size)| size != 0 && !seeds.contains(a) && !strong.contains(a))
            .map(|(&a, _)| a)
            .collect();
        Ok(out)
    }

    /// Every candidate a reference slot of `obj` names (other than `obj`
    /// itself) is held. The slot walks are Phase 4's
    /// (`collect_outgoing_cross_region_edges_noting_young`): a reference
    /// array's elements, an instance's flat walk, both clamped to the holder.
    fn note_pinned_referent_holder_slots(
        &self,
        regions: &[G1Region],
        obj: *mut u8,
        header: &ObjectHeader,
        cand: &FxHashMap<usize, usize>,
        held: &mut FxHashSet<usize>,
    ) {
        let me = obj as usize;
        self.for_each_pinned_referent_holder_slot(regions, obj, header, |raw| {
            if raw != 0 && raw != me && cand.contains_key(&raw) {
                held.insert(raw);
            }
        });
    }

    /// The raw word of every reference slot of `obj` (null included), by
    /// Phase 4's slot walks: a reference array's elements, an instance's flat
    /// walk, both clamped to the holder. Shared by the kept-region verdict and
    /// (gci i32/ec) the kept finalizables' private closure.
    fn for_each_pinned_referent_holder_slot(
        &self,
        regions: &[G1Region],
        obj: *mut u8,
        header: &ObjectHeader,
        mut note: impl FnMut(usize),
    ) {
        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                let walkable =
                    self.holder_walkable_slots(regions, obj, header.array_length() as usize, 8);
                // SAFETY: `obj` is an object start the caller sized inside its
                // region; `walkable` is clamped to the holder's own extent.
                let data = unsafe { obj.add(ARRAY_DATA_OFFSET) };
                for k in 0..walkable {
                    let raw = unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
                    note(raw);
                }
            }
        } else {
            let walkable =
                self.holder_walkable_slots(regions, obj, header.num_slots() as usize, SLOT_SIZE);
            for_each_flat_object_reference_capped(obj, header, 0, walkable, |_, raw, _| note(raw));
        }
    }
}

#[cfg(test)]
mod tests {
    //! `cargo test -j 5 -p cratonvm-gc --lib gci_i27_gr_`.
    use super::*;
    use crate::collector::{GarbageCollector, MonitorCleanup};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    struct NoopMonitors;
    impl MonitorCleanup for NoopMonitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }

    fn collector() -> G1Collector {
        G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            initial_heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            promotion_age: 3,
            gc_worker_threads: 1,
            string_dedup_enabled: false,
            ..Default::default()
        })
    }

    fn region_of(gc: &G1Collector, obj: ObjectRef) -> usize {
        gc.lookup_region_for_addr(obj.as_ptr() as usize)
            .expect("fixture object in a region")
    }

    fn addr(obj: ObjectRef) -> usize {
        obj.as_ptr() as usize
    }

    /// Allocate a one-slot object in a region other than `avoid`.
    fn alloc_outside(gc: &G1Collector, avoid: usize) -> ObjectRef {
        loop {
            let o = gc.alloc_object(ClassId::new(2), 1);
            if region_of(gc, o) != avoid {
                return o;
            }
        }
    }

    /// The page's fixture: in one pinned Eden region a dead referent, a
    /// referent a rooted object of the same region holds, a dead self-cycle, a
    /// referent held by a copied holder of another region, and one an interior
    /// root word holds. After the pause the region is still young and in
    /// place, and the verdict names exactly the dead two.
    #[test]
    fn gci_i27_gr_a_kept_region_s_unheld_referents_are_named_and_held_ones_are_not() {
        let gc = collector();
        let reference = gc.alloc_object(ClassId::new(1), 2); // the `Reference`, slot 0 nulled
        let pinned = region_of(&gc, reference);
        let dead = gc.alloc_object(ClassId::new(1), 1);
        let held_in_region = gc.alloc_object(ClassId::new(1), 1);
        let holder_in_region = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(holder_in_region, 0, Value::Object(Some(held_in_region)));
        let self_cycle = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(self_cycle, 0, Value::Object(Some(self_cycle)));
        let held_from_outside = gc.alloc_object(ClassId::new(1), 1);
        let interior = gc.alloc_object(ClassId::new(1), 2);
        for o in [dead, held_in_region, holder_in_region, self_cycle, held_from_outside, interior] {
            assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
        }
        let outside = alloc_outside(&gc, pinned);
        gc.set_field(outside, 0, Value::Object(Some(held_from_outside)));

        gc.pin_region(pinned);
        let mut roots = vec![reference, holder_in_region, outside];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            result.pointer_map.contains_key(&addr(outside)),
            "fixture: the outside holder was copied"
        );
        assert!(
            !result.pointer_map.contains_key(&addr(dead)),
            "fixture: the kept region's objects are not in the map"
        );
        assert!(gc.pinned_referent_verdict_armed(), "a pause that kept a region arms it");

        let candidates = [
            addr(reference),
            addr(dead),
            addr(held_in_region),
            addr(self_cycle),
            addr(held_from_outside),
            addr(interior),
        ];
        let mut root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        root_words.push(addr(interior) + HEADER_SIZE + 8);
        let verdict = gc
            .pinned_young_referents_unheld_within(&candidates, &root_words, usize::MAX, false)
            .expect("a verdict");
        gc.unpin_region(pinned);
        assert_eq!(verdict.candidates, 6);
        assert_eq!(verdict.confirmed, 6);
        let expected: FxHashSet<usize> = [addr(dead), addr(self_cycle)].into_iter().collect();
        assert_eq!(verdict.unheld, expected);
    }

    /// gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
    /// the VM now offers ONE candidate list per pause for every weak consumer
    /// (the reference rows and the JNI weak globals' referents). That is sound
    /// only if a candidate's verdict does not depend on which other candidates
    /// are offered: asked alone, in pairs, or with all the others, each
    /// fixture object gets the same answer.
    #[test]
    fn gci_i28_wt_a_candidate_s_verdict_does_not_depend_on_the_others() {
        let gc = collector();
        let reference = gc.alloc_object(ClassId::new(1), 2);
        let pinned = region_of(&gc, reference);
        let dead = gc.alloc_object(ClassId::new(1), 1);
        let held_in_region = gc.alloc_object(ClassId::new(1), 1);
        let holder_in_region = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(holder_in_region, 0, Value::Object(Some(held_in_region)));
        // A dead holder still holds: `dead_holder` is garbage, its target is
        // over-retained whether or not `dead_holder` itself is a candidate.
        let dead_holder = gc.alloc_object(ClassId::new(1), 1);
        let held_by_garbage = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(dead_holder, 0, Value::Object(Some(held_by_garbage)));
        for o in [dead, held_in_region, holder_in_region, dead_holder, held_by_garbage] {
            assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
        }
        // gci i30/rp hand-back 1: a rooted young object OUTSIDE the pinned
        // region, so the pause has a non-empty collection set. With the pinned
        // region the only young region, the CSet is empty, the pause returns
        // before the record funnel (`young_collection_serial` /
        // `young_collection_parallel`, `if cset.is_empty()`), and nothing arms
        // the verdict -- by design (i27/gr: "a refused / empty-CSet pause
        // records nothing"). The i27 fixture does the same (`outside`). Since
        // gci i32/ec such a pause arms it too (`note_refproc_empty_cset_pause`,
        // test `gci_i32_ec_an_empty_cset_pause_...`); `outside` stays so this
        // test reads the record-funnel arm.
        let outside = alloc_outside(&gc, pinned);
        gc.pin_region(pinned);
        let mut roots = vec![reference, holder_in_region, outside];
        gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            gc.pinned_referent_verdict_armed(),
            "a pause with a collection set that kept a region arms it"
        );
        let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

        let all = [
            addr(reference),
            addr(dead),
            addr(held_in_region),
            addr(dead_holder),
            addr(held_by_garbage),
        ];
        let whole = gc
            .pinned_young_referents_unheld_within(&all, &root_words, usize::MAX, false)
            .expect("a verdict");
        let expected: FxHashSet<usize> = [addr(dead), addr(dead_holder)].into_iter().collect();
        assert_eq!(whole.unheld, expected);
        for (i, &a) in all.iter().enumerate() {
            let alone = gc
                .pinned_young_referents_unheld_within(&[a], &root_words, usize::MAX, false)
                .expect("a verdict alone");
            assert_eq!(
                alone.unheld.contains(&a),
                whole.unheld.contains(&a),
                "candidate {i} alone"
            );
            let other = all[(i + 1) % all.len()];
            let pair = gc
                .pinned_young_referents_unheld_within(&[a, other], &root_words, usize::MAX, false)
                .expect("a verdict in a pair");
            for b in [a, other] {
                assert_eq!(pair.unheld.contains(&b), whole.unheld.contains(&b), "pair {i}");
            }
        }
        gc.unpin_region(pinned);
    }

    /// gci i30/rp (`i27-gr-report.md` review item 4): a referent in a kept
    /// region that only a dead finalizable's resurrected copy names is
    /// `finalizer_only` (held for every in-place verdict, finalizer-reachable
    /// for the reference round), one a strongly reachable copy also names is
    /// held, and one nothing names is unheld.
    #[test]
    fn gci_i30_rp_a_referent_only_a_resurrected_finalizable_holds_is_finalizer_only() {
        let gc = collector();
        let reference = gc.alloc_object(ClassId::new(1), 2);
        let pinned = region_of(&gc, reference);
        let only_fin = gc.alloc_object(ClassId::new(1), 1);
        let shared = gc.alloc_object(ClassId::new(1), 1);
        let dead = gc.alloc_object(ClassId::new(1), 1);
        for o in [only_fin, shared, dead] {
            assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
        }
        let fin = alloc_outside(&gc, pinned);
        gc.set_field(fin, 0, Value::Object(Some(only_fin)));
        let strong = alloc_outside(&gc, pinned);
        gc.set_field(strong, 0, Value::Object(Some(shared)));
        let fin_twin = alloc_outside(&gc, pinned);
        gc.set_field(fin_twin, 0, Value::Object(Some(shared)));
        gc.pin_region(pinned);
        let mut roots = vec![reference, strong];
        // SAFETY: the unit test runs the heap single-threaded; no mutator runs.
        let stw = unsafe { crate::collector::StopTheWorldToken::new() };
        let (_result, resurrected) = gc.collect_garbage_with_finalizers(
            &stw,
            &mut roots,
            &[addr(fin), addr(fin_twin)],
            &NoopMonitors,
        );
        assert_eq!(resurrected.len(), 2, "fixture: Phase 3.5 resurrected both finalizables");
        assert!(gc.pinned_referent_verdict_armed(), "fixture: the pause kept the region");
        let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        let verdict = gc
            .pinned_young_referents_unheld_within(
                &[addr(reference), addr(only_fin), addr(shared), addr(dead)],
                &root_words,
                usize::MAX,
                false,
            )
            .expect("a verdict");
        gc.unpin_region(pinned);
        let unheld: FxHashSet<usize> = [addr(dead)].into_iter().collect();
        let finalizer_only: FxHashSet<usize> = [addr(only_fin)].into_iter().collect();
        assert_eq!(verdict.unheld, unheld);
        assert_eq!(verdict.finalizer_only, finalizer_only);
        // The next collection forgets the holders at its start.
        assert!(!gc.resurrection_holders.lock().is_empty(), "written by the pause");
        gc.reset_refproc_kept_young_token();
        assert!(gc.resurrection_holders.lock().is_empty());
    }

    /// No verdict without an armed pause, over budget, or with no candidate in
    /// a young region; a pause that kept nothing does not arm it.
    #[test]
    fn gci_i27_gr_every_way_of_not_knowing_declines() {
        let gc = collector();
        let a = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(
            gc.pinned_young_referents_unheld_within(&[addr(a)], &[], usize::MAX, false),
            Err(G1PinnedReferentDecline::NotArmed),
            "no collection yet"
        );
        let mut roots = vec![a];
        gc.young_collection(&mut roots, &NoopMonitors);
        assert!(!gc.pinned_referent_verdict_armed(), "nothing kept, nothing armed");

        let b = gc.alloc_object(ClassId::new(1), 1);
        let pinned = region_of(&gc, b);
        gc.pin_region(pinned);
        let mut roots = vec![roots[0]];
        gc.young_collection(&mut roots, &NoopMonitors);
        assert!(gc.pinned_referent_verdict_armed());
        assert_eq!(
            gc.pinned_young_referents_unheld_within(&[addr(b)], &[], 0, false),
            Err(G1PinnedReferentDecline::OverBudget)
        );
        assert_eq!(
            gc.pinned_young_referents_unheld_within(&[0, 0x1000, addr(b) + 4], &[], usize::MAX, false),
            Err(G1PinnedReferentDecline::NoCandidates)
        );
        let v = gc
            .pinned_young_referents_unheld_within(&[addr(b)], &[], usize::MAX, false)
            .expect("a verdict");
        assert!(v.unheld.contains(&addr(b)), "nothing holds b");
        // A later record of another collection count disarms it until it
        // keeps a region again.
        gc.unpin_region(pinned);
        gc.reset_refproc_kept_young_token();
        assert!(!gc.pinned_referent_verdict_armed());
    }

    /// The record funnel keeps the "kept" bit across a drain pass of the same
    /// collection and refuses a full record.
    #[test]
    fn gci_i27_gr_the_token_follows_the_records() {
        let gc = collector();
        let kept = G1PausePhases {
            jit_pinned_out: 1,
            ..Default::default()
        };
        let plain = G1PausePhases::default();
        gc.collection_count.fetch_add(1, Ordering::Relaxed);
        gc.note_refproc_evacuation_record(G1CollectionType::YoungOnly, &kept);
        assert!(gc.pinned_referent_verdict_armed());
        gc.collection_count.fetch_add(1, Ordering::Relaxed);
        gc.note_refproc_evacuation_record(G1CollectionType::YoungOnly, &plain);
        assert!(gc.pinned_referent_verdict_armed(), "a drain pass keeps the bit");
        gc.reset_refproc_kept_young_token();
        gc.collection_count.fetch_add(1, Ordering::Relaxed);
        gc.note_refproc_evacuation_record(G1CollectionType::Mixed, &plain);
        assert!(!gc.pinned_referent_verdict_armed(), "a fresh collection that kept nothing");
        gc.collection_count.fetch_add(1, Ordering::Relaxed);
        gc.note_refproc_evacuation_record(G1CollectionType::Full, &kept);
        assert!(!gc.pinned_referent_verdict_armed(), "a full record never arms it");
        gc.collection_count.fetch_add(1, Ordering::Relaxed);
        gc.note_refproc_evacuation_record(G1CollectionType::Mixed, &kept);
        gc.collection_count.fetch_add(1, Ordering::Relaxed);
        assert!(
            !gc.pinned_referent_verdict_armed(),
            "a later collection count disarms it"
        );
    }

    /// gci i32/ec (`i30-rp-report.md` hand-back 1): a pause whose ONLY young
    /// region is pinned has an empty collection set and returns before the
    /// record funnel; it now arms the verdict (both young drivers), so the
    /// dead referent of that region is named at that pause, and the rooted one
    /// is not. The collection count does not move (the pause records
    /// nothing).
    #[test]
    fn gci_i32_ec_an_empty_cset_pause_that_pinned_every_young_region_arms_the_verdict() {
        for parallel in [false, true] {
            let gc = collector();
            let reference = gc.alloc_object(ClassId::new(1), 2);
            let pinned = region_of(&gc, reference);
            let dead = gc.alloc_object(ClassId::new(1), 1);
            let held = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(reference, 1, Value::Object(Some(held)));
            for o in [dead, held] {
                assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
            }
            gc.pin_region(pinned);
            let count_before = gc.collection_count.load(Ordering::Relaxed);
            let mut roots = vec![reference];
            let result = if parallel {
                gc.young_collection_parallel(&mut roots, &NoopMonitors)
            } else {
                gc.young_collection_serial(&mut roots, &NoopMonitors)
            };
            assert!(result.pointer_map.is_empty(), "fixture: an empty collection set");
            assert_eq!(
                gc.collection_count.load(Ordering::Relaxed),
                count_before,
                "an empty pause records nothing"
            );
            assert!(
                gc.pinned_referent_verdict_armed(),
                "parallel={parallel}: an empty CSet that pinned out a young region arms it"
            );
            let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
            let verdict = gc
                .pinned_young_referents_unheld_within(
                    &[addr(reference), addr(dead), addr(held)],
                    &root_words,
                    usize::MAX,
                    false,
                )
                .expect("a verdict");
            let expected: FxHashSet<usize> = [addr(dead)].into_iter().collect();
            assert_eq!(verdict.unheld, expected, "parallel={parallel}");
            gc.unpin_region(pinned);
            gc.reset_refproc_kept_young_token();
        }
    }

    /// gci i32/ec (`i31-gq-report.md` residual): a dead finalizable `fin` in a
    /// pinned region names `child` (another Eden region, so the pause copies
    /// it), `child` names `grandchild`, and `fin` names `shared` that a rooted
    /// object also names. The private closure is `child` and `grandchild` at
    /// their post-pause addresses; `shared` is strongly held; the seed is
    /// never listed. A walk cap below the closure's depth stops early and
    /// only over-retains.
    #[test]
    fn gci_i32_ec_a_kept_finalizable_s_copied_private_children_are_finalizer_only() {
        let gc = collector();
        let fin = gc.alloc_object(ClassId::new(1), 2);
        let pinned = region_of(&gc, fin);
        let child = alloc_outside(&gc, pinned);
        let grandchild = alloc_outside(&gc, pinned);
        let shared = alloc_outside(&gc, pinned);
        let rooted = alloc_outside(&gc, pinned);
        gc.set_field(fin, 0, Value::Object(Some(child)));
        gc.set_field(fin, 1, Value::Object(Some(shared)));
        gc.set_field(child, 0, Value::Object(Some(grandchild)));
        gc.set_field(rooted, 0, Value::Object(Some(shared)));
        gc.pin_region(pinned);
        let mut roots = vec![rooted];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        let moved = |o: ObjectRef| -> usize {
            *result
                .pointer_map
                .get(&addr(o))
                .expect("fixture: the pause copied the outside objects")
        };
        let (child_now, grandchild_now, shared_now) = (moved(child), moved(grandchild), moved(shared));
        assert!(
            !result.pointer_map.contains_key(&addr(fin)),
            "fixture: the kept region's objects are not in the map"
        );
        assert!(gc.pinned_referent_verdict_armed(), "fixture: the pause kept the region");
        let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

        let whole = gc
            .kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 4)
            .expect("a closure");
        let expected: FxHashSet<usize> = [child_now, grandchild_now].into_iter().collect();
        assert_eq!(whole.finalizer_only, expected);
        assert!(!whole.finalizer_only.contains(&shared_now), "shared is strongly held");
        assert!(whole.complete);
        assert_eq!(whole.closure, 4, "fin, child, shared, grandchild");

        // One walk confirms only the seed: nothing reported.
        let one = gc
            .kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 1)
            .expect("a closure");
        assert!(one.finalizer_only.is_empty());
        assert!(!one.complete);
        // Two walks confirm the children, not the grandchild.
        let two = gc
            .kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 2)
            .expect("a closure");
        let child_only: FxHashSet<usize> = [child_now].into_iter().collect();
        assert_eq!(two.finalizer_only, child_only);
        assert!(!two.complete);

        // A root word on the child makes it and its grandchild strong.
        let mut rooted_child = root_words.clone();
        rooted_child.push(child_now);
        let held = gc
            .kept_finalizables_private_closure_within(&[addr(fin)], &rooted_child, usize::MAX, false, 4)
            .expect("a closure");
        assert!(held.finalizer_only.is_empty());
        // gci i38/ec: the holder census names the root word, and on the
        // unrooted answer only the outside slot that holds `shared`.
        assert_eq!((held.held_by_root, held.held_by_pin), (1, 0));
        assert_eq!((whole.held_by_slot, whole.held_by_root, whole.held_by_pin), (1, 0, 0));
        // gci i41/hc: and WHICH words those were -- the root entry by its
        // index in the slice, the outside slot by its holder (slots first:
        // the walk runs before the root words are read).
        let shared_by_rooted = G1ClosureHolder {
            kind: G1ClosureHolderKind::Slot {
                holder: root_words[0],
            },
            word: shared_now,
            member: shared_now,
        };
        assert_eq!(
            held.holders,
            vec![
                shared_by_rooted,
                G1ClosureHolder {
                    kind: G1ClosureHolderKind::Root {
                        index: rooted_child.len() - 1
                    },
                    word: child_now,
                    member: child_now,
                }
            ]
        );
        assert_eq!(whole.holders, vec![shared_by_rooted]);
        assert_eq!((held.holders_dropped, whole.holders_dropped), (0, 0));
        // The members of the last walk, ascending, seed included.
        let mut member_starts: Vec<usize> = whole.members.iter().map(|&(a, _)| a).collect();
        let mut expected_members = vec![addr(fin), child_now, shared_now, grandchild_now];
        expected_members.sort_unstable();
        assert_eq!(member_starts, expected_members);
        member_starts.dedup();
        assert_eq!(member_starts.len(), 4);
        assert!(whole.members.iter().all(|&(_, size)| size >= HEADER_SIZE));

        // Every way of not knowing declines.
        assert_eq!(
            gc.kept_finalizables_private_closure_within(&[addr(fin)], &root_words, 0, false, 4),
            Err(G1PinnedReferentDecline::OverBudget)
        );
        assert_eq!(
            gc.kept_finalizables_private_closure_within(&[0, 0x1000], &root_words, usize::MAX, false, 4),
            Err(G1PinnedReferentDecline::NoCandidates)
        );
        gc.unpin_region(pinned);
        gc.reset_refproc_kept_young_token();
        assert_eq!(
            gc.kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 4),
            Err(G1PinnedReferentDecline::NotArmed)
        );
    }

    /// gci i41/hc: the holder record's pure parts -- the pin publisher split
    /// (own entry first, then this cycle's helper-window words, else a peer's
    /// entry) and the cap (recorded up to the bound, counted past it).
    #[test]
    fn gci_i41_hc_closure_holder_pin_source_and_cap() {
        let own: FxHashSet<usize> = [0x10].into_iter().collect();
        let helper: FxHashSet<usize> = [0x10, 0x20].into_iter().collect();
        assert_eq!(closure_pin_source(0x10, &own, &helper), G1ClosurePinSource::Own);
        assert_eq!(
            closure_pin_source(0x20, &own, &helper),
            G1ClosurePinSource::HelperWindow
        );
        assert_eq!(closure_pin_source(0x30, &own, &helper), G1ClosurePinSource::Peer);
        let mut holders: Vec<G1ClosureHolder> = Vec::new();
        let mut dropped = 0usize;
        for k in 0..KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX + 3 {
            push_closure_holder(
                &mut holders,
                &mut dropped,
                G1ClosureHolder {
                    kind: G1ClosureHolderKind::Root { index: k },
                    word: 0x1000 + 8 * k,
                    member: 0x1000,
                },
            );
        }
        assert_eq!(holders.len(), KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX);
        assert_eq!(dropped, 3);
        assert_eq!(holders[0].kind, G1ClosureHolderKind::Root { index: 0 });
    }

    /// gci i32/ec: the arming is the empty-CSet driver's only when a young
    /// region was pinned out, and `CRATONVM_G1_PINNED_REFERENT_EMPTY_CSET=0`
    /// keeps the old answer (unarmed).
    #[test]
    fn gci_i32_ec_the_empty_cset_arming_needs_a_pinned_out_region_and_has_a_zero_kill() {
        let name = "CRATONVM_G1_PINNED_REFERENT_EMPTY_CSET";
        cratonvm_types::flags::with_thread_overrides(&[(name, None)], || {
            assert!(g1_pinned_referent_empty_cset_enabled(), "unset must read ON");
            let gc = collector();
            gc.note_refproc_empty_cset_pause(0);
            assert!(!gc.pinned_referent_verdict_armed(), "no young region pinned out");
            gc.note_refproc_empty_cset_pause(2);
            assert!(gc.pinned_referent_verdict_armed());
            gc.collection_count.fetch_add(1, Ordering::Relaxed);
            assert!(!gc.pinned_referent_verdict_armed(), "a later count disarms it");
        });
        cratonvm_types::flags::with_thread_overrides(&[(name, Some("0"))], || {
            assert!(!g1_pinned_referent_empty_cset_enabled(), "=0 kills it");
            let gc = collector();
            gc.note_refproc_empty_cset_pause(3);
            assert!(!gc.pinned_referent_verdict_armed());
        });
    }

    /// gci i32/ec: the three empty-CSet returns of the evacuation drivers call
    /// the arming; `collect_garbage`'s refusals do not (they return before any
    /// driver).
    #[test]
    fn gci_i32_ec_the_empty_cset_returns_are_wired() {
        let src = include_str!("g1.rs").replace("\r\n", "\n");
        for driver in [
            "pub(crate) fn young_collection_serial(",
            "pub(crate) fn young_collection_parallel(",
            "pub(crate) fn mixed_collection_parallel(",
        ] {
            let at = src.find(driver).expect(driver);
            let rest = &src[at..];
            let end = rest[1..].find("\n    pub").map_or(rest.len(), |e| e + 1);
            let body = &rest[..end];
            let empty = body.find("if cset.is_empty()").expect(driver);
            let arm = body
                .find("self.note_refproc_empty_cset_pause(jni_pinned_out + jit_pinned_out);")
                .expect(driver);
            let ret = empty + body[empty..].find("return GcResult").expect(driver);
            assert!(empty < arm && arm < ret, "{driver}: armed inside the empty-CSet return");
        }
        assert_eq!(src.matches("self.note_refproc_empty_cset_pause(").count(), 3);
    }
}
