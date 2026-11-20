#[allow(unused_imports)]
use super::*;

/// Scan a single JIT frame's spill region.
///
/// `low_sp` is the lowest address the scan should touch (typically the
/// scanner's own stack pointer or the next-inner JIT entry). `high_sp` is
/// the address recorded at JIT entry — one past the topmost spill slot.
/// We walk `[low_sp, high_sp)` in 8-byte strides.
pub(in crate::jit::conservative_roots) fn scan_one_frame(low_sp: usize, high_sp: usize, heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    if high_sp <= low_sp {
        // Either the chain is inverted or the JIT call hasn't actually
        // pushed any locals yet. Nothing to scan.
        return;
    }
    // Round low_sp up to the nearest 8-byte boundary so we never read an
    // unaligned qword (would be a bus error on some platforms).
    let aligned_low = (low_sp + 7) & !7usize;
    if aligned_low >= high_sp {
        return;
    }
    // Bound the scan so a stale `high_sp` (e.g. from a recycled stack region
    // after a thread tear-down) cannot send us into unmapped pages.
    //
    // gen r4w2/youngmark (2026-09-23): by the thread's REAL stack limits, not a
    // fixed 8 MiB from the bottom. Every band this is asked for puts the JIT
    // frames at its HIGH end and the scanner at its low end, and `main-vm` runs
    // on a 128 MiB stack (`vm-cli`), so a collection more than 8 MiB below the
    // outermost compiled frame (deep interpreted recursion — what that stack is
    // sized for) silently skipped exactly the words that mattered. The old
    // comment's "8 MiB matches the default `CRATONVM_STACK` size" was not true
    // of `main-vm`. See `own_stack_scan_hi`.
    let aligned_high = own_stack_scan_hi(
        aligned_low,
        high_sp,
        current_stack_pointer(),
        own_stack_top(),
    );

    // SAFETY: the loop reads aligned qwords inside the calling thread's own
    // stack region between two known-valid stack pointers. The lower bound
    // came from `current_stack_pointer()` taken on the same thread; the upper
    // bound was captured at JIT entry on the same thread. Rust stacks are
    // backed by mapped pages for their entire reserved range, so reads in
    // this interval are well-defined. We never write through the pointer.
    //
    // Hoist the heap's address envelope out of the loop. `is_object_address`
    // is an out-of-line call that re-reads the arena bounds (three `Acquire`
    // load pairs on the generational backend) for EVERY word -- and almost
    // every word on a stack is a return address, an int, or a native pointer
    // that fails that very first test. Rejecting those inline against a
    // hoisted `[lo, hi)` leaves the full validator to run only for words that
    // could plausibly be object headers. `None` means the backend has no cheap
    // envelope (ZGC), in which case every word goes through the validator as
    // before.
    let span = heap.conservative_addr_span();
    let record_provenance = record_provenance_for(heap);
    // JIT round 21 lane refs: a scan outside a collection's armed window (a
    // blocking deposit, a snapshot scan) under `CRATONVM_DBG_REFERENT_HOLDER`.
    let holder_prov = !record_provenance && crate::runtime::env_cache::dbg_referent_holder();
    // Once per scan: a G1 root gatherer that will publish the pins of rejected
    // words has armed the capture (`g1_band_reject_pins`).
    let capture_rejects = g1_band_reject_pins::armed();
    let mut addr = aligned_low;
    let hits_before = out.len();
    while addr + 8 <= aligned_high {
        let qword = unsafe { (addr as *const usize).read() };
        // gen r4/mark: the SLOT the word was read from, taken before the
        // cursor advances. The provenance string used to format `addr` after
        // `addr += 8`, i.e. it named the NEXT slot.
        let slot = addr;
        addr += 8;
        if let Some((lo, hi)) = span {
            if qword < lo || qword >= hi {
                continue;
            }
        }
        // Object STARTS only: on G1 this also rejects a header-shaped interior
        // word whose object would cross its region end
        // (`VmHeap::is_object_address_for_root_scan`).
        if let Some(obj) = heap.is_object_address_for_root_scan(qword) {
            // gcf f7/f: census only (see the section above).
            note_frame_scan_interior_word(heap, qword);
            if record_provenance || holder_prov {
                note_root_provenance(
                    record_provenance,
                    qword,
                    format!("scan_one_frame addr=0x{slot:x}"),
                );
            }
            out.push(obj);
        } else if capture_rejects {
            g1_band_reject_pins::note(heap, qword, slot, || band_reject_origin(slot, aligned_high));
        }
    }
    // Counted per CALL, never per word — see `rootprof::note_stack_scan` for
    // why these exist. Free when `CRATONVM_DBG_ROOTPROF` is unset (one
    // already-resolved `OnceLock` load).
    crate::memory::native_roots::rootprof::note_stack_scan(
        (aligned_high.saturating_sub(aligned_low) / 8) as u64, // Widening: a stack band length
        (out.len() - hits_before) as u64,                      // Widening: a Vec length
    );
}

/// `CRATONVM_JIT_BAND_SKIP=<class>[,<class>...]` — **a MEASUREMENT LEVER, and
/// unsafe. Default unset, which is byte-for-byte today's scan.**
///
/// Drops the named [`cratonvm_jit::FrameLayout::region_name`] classes from the
/// conservative band scan, so words there stop being marking roots. It exists
/// to put a NUMBER on what a precise compiled-frame root set could buy, because
/// the alternative is arguing about it:
/// `docs/feature-designs/jit-ir-optimizations.md`
/// spent its "where to start" list on two narrowings that measurement then
/// refuted, and the third — precise oop maps — is a project nobody will start
/// on a guess about its payoff.
///
/// Recognised classes are `region_name`'s own vocabulary; the three worth
/// measuring are `operand-spill` (words above the safepoint's live cursor —
/// which is the only class the VM already CLAIMS is dead, see
/// `OopMapEntry::live_frame_hi`), `outgoing-args-or-deopt-regs` (the
/// uninitialised outgoing-argument reserve, which holds whatever a previous,
/// deeper frame left below the old stack pointer), and
/// `safepoint-gpr-spill-image` (the blind GPR spill, where a dead scratch
/// register is indistinguishable from a live one without register liveness).
///
/// **Do not enable this in production.** Dropping a root frees what it named.
/// Only `operand-spill` above the live cursor has a written deadness argument
/// behind it; the other two are conservative backstops and skipping them is a
/// use-after-free waiting for the right frame. The lever measures the ceiling,
/// it does not implement the fix.
pub(in crate::jit::conservative_roots) fn band_skip_classes() -> Option<&'static [String]> {
    static G: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    G.get_or_init(|| {
        let v = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()?;
        let classes: Vec<String> = v
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        (!classes.is_empty()).then_some(classes)
    })
    .as_deref()
}

/// [`scan_one_frame`] over `[rbp - frame_size, rbp)`, minus the words the
/// active safepoint's claims prove dead, and minus a measurement lever:
///
/// * the blind-spill slots this safepoint's [`ActiveMapFacts::reg_mask`]
///   excludes -- the production narrowing, on by default;
/// * the operand-spill slots above its live cursor
///   ([`spill_slot_is_dead_above_cursor`]) and the dead outgoing reserve
///   ([`is_dead_outgoing_reserve`]) -- on by default;
/// * the words the body's liveness table proves dead
///   ([`is_dead_by_frame_liveness`], gen r5w3/live7) -- only for a body the
///   optimizing tier compiled under `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`;
/// * the storage classes [`band_skip_classes`] names -- the unsafe measurement
///   lever, off by default.
///
/// (Until gen r5w3 this said "minus two filters", the first and the last, and
/// had not been updated for the two claims in between.) Only reachable when at
/// least one of them has something to say. `facts` is [`active_map_facts`] for
/// this frame.
///
/// The same walk also publishes the frame's movable/unrewritable pin partition
/// (round 11 wave 13, rt proposal W10-3). It used to be a second walk,
/// [`publish_unrewritable_band_roots`], which re-evaluated the skips, re-read
/// every word and re-validated every candidate the scan had just validated.
/// Now each surviving word is read once and validated once, and the one answer
/// feeds both halves, so the partition is a partition of exactly the words this
/// scan roots by construction rather than by keeping two loops in step.
///
/// The halves' word sets differ in two places, and both are kept exactly:
///
/// * the deopt XMM image is never a root but IS published (movable, see
///   [`publish_validated_band_word`]), unless the publication's own dead-spill
///   or outgoing skip drops it -- those two tests are then neither counted nor
///   noted, as before, because the scan skipped the image first;
/// * a [`band_skip_classes`] word is not a root but is still published; the
///   lever is a measurement of the ROOT set only.
///
/// Per word, the side effects happen in word order, where they used to happen
/// root half first; none of them reads another (`out` is local, the pin sets
/// and the provenance map are written, not read, during a scan).
///
/// gci i23/rf: this is [`scan_one_frame_filtered_from`] with no
/// reference-free floor, the shape every unit test of the claims calls.
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn scan_one_frame_filtered(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    skip: Option<&[String]>,
    claims: FrameClaimFacts<'_>,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
    remap_covered: bool,
    walk_hi: usize,
    caller_claims: Option<&mut CallerClaimMemo>,
) {
    scan_one_frame_filtered_from(
        rbp,
        frame_size,
        cm,
        skip,
        claims,
        heap,
        out,
        remap_covered,
        walk_hi,
        caller_claims,
        None,
    );
}

/// [`scan_one_frame_filtered`], plus the reference-free floor (gci i23/rf,
/// [`reference_free_band_floor`]): with `ref_free_lo = Some(lo)` no word at
/// `[rbp - off]` with `off < lo` is read -- neither a root, a pin, nor a
/// publication, like every other claim's word -- and the words at or beyond
/// `lo` (the CALLER's register images, the blind spill, the deopt block, the
/// outgoing reserve) are scanned exactly as before. `None` is the old scan.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn scan_one_frame_filtered_from(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    skip: Option<&[String]>,
    claims: FrameClaimFacts<'_>,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
    remap_covered: bool,
    walk_hi: usize,
    mut caller_claims: Option<&mut CallerClaimMemo>,
    ref_free_lo: Option<i32>,
) {
    if frame_size == 0 || frame_size > rbp {
        return;
    }
    // gcf f2/f: every claim's input was resolved in one pass by the caller
    // ([`frame_claim_facts`]).
    let FrameClaimFacts {
        map: facts,
        ir_prims,
        sp_locals,
        live_sp,
        deopt_residue,
        image_rule: image_rule_on,
        unnamed_dbg,
        map_dead_claim,
        dead_locals_no_pin,
    } = claims;
    let ActiveMapFacts {
        reg_mask,
        live_hi,
        staged_unmapped,
        spill_written,
    } = facts;
    // A memo made under another bound answers another question.
    if caller_claims.as_ref().is_some_and(|m| m.walk_hi != walk_hi) {
        caller_claims = None;
    }
    let layout = &cm.frame_layout;
    // gen r5w6/oomjit10: the fifth claim -- a callee-saved GPR image word
    // whose register the owning CALLER frame's claim calls dead
    // ([`caller_register_claim`]).
    let image_rule = walk_hi > rbp && image_rule_on;
    let dead_word_rejects_no_pin = dead_word_rejects_owe_no_pin_enabled();
    let mut image_excluded = 0usize;
    // gen r5w6/oomjit10: the sixth -- an optimizing-tier frame's `Prim`
    // colours ([`ir_prim_slots`]): `ir_prims`.
    let mut prim_excluded = 0usize;
    // JIT round 25 wave 4: `ir_prims` IS the body's per-body claim
    // (`ir_prim_colour_claim`, spent by the caller at every poll) rather than
    // a map's `non_oop_stack_slots`. Such a word is ignored whole, the G1
    // reject pin included ([`ir_body_claim_ignores_word`]); a map's list keeps
    // the object-start-only rule below. The words of the claim the old rule
    // would have reject-pinned are counted in `body_claim_unpinned`.
    let body_claim: Option<&[i16]> = ir_prims.filter(|p| ir_body_claim_ignores_word(cm, p));
    let mut body_claim_unpinned = 0usize;
    // gce e1/f: the seventh -- a single-pass frame's java-local homes that
    // no map of the active safepoint names ([`SpLocalClaim`]): `sp_locals`.
    let mut sp_local_excluded = 0usize;
    // gce e1/f: the eighth -- a single-pass frame-deopt GPR block this
    // activation's stub never wrote ([`frame_deopt_image_is_residue`]):
    // `deopt_residue`.
    let mut deopt_residue_excluded = 0usize;
    let mut addr = (rbp - frame_size + 7) & !7usize;
    let envelope = heap.conservative_addr_span();
    // gcf f1/j: see `in_native_envelope_first`; `None` (every scan but the
    // package's in-native deposit) leaves the loop below unchanged.
    let envelope_first = envelope.filter(|_| in_native_envelope_first());
    let (mut env_skipped, mut env_kept) = (0u64, 0u64);
    // Hoisted as in `scan_one_frame`: nothing in the loop can change it.
    let record_provenance = record_provenance_for(heap);
    // JIT round 21 lane refs: a scan outside a collection's armed window (a
    // blocking deposit, a snapshot scan) under `CRATONVM_DBG_REFERENT_HOLDER`.
    let holder_prov = !record_provenance && crate::runtime::env_cache::dbg_referent_holder();
    let capture_rejects = g1_band_reject_pins::armed();
    // gci i6/mv: the root-remap audit's census (armed around the audit's own
    // re-scan only, `gc_quiescence::arm_band_root_census`): every rooted word
    // is recorded with whether the active map proves it dead
    // ([`band_word_is_map_dead`]). Unarmed -- every collection's own scan --
    // this is one thread-local read per frame and no map resolution.
    let band_census = cratonvm_gc::gc_quiescence::band_root_census_armed();
    let census_map = if band_census {
        frame_active_map_slots(rbp, cm)
    } else {
        None
    };
    // gci i38/ds: the active map's slots for the unnamed-root diagnostic and
    // the opt-in map-dead claim, resolved once per frame and only when one of
    // the two switches is on (`None` otherwise, and when no map resolved).
    // gci i41/rs: both switches come in `claims` (read once per chain walk,
    // [`BandClaimSwitches::read`]), not one `runtime_flag_on` each per frame.
    let frame_map: Option<Vec<i32>> = if unnamed_dbg || map_dead_claim || dead_locals_no_pin || band_skip_in_map_enabled() {
        frame_active_named_slots(rbp, cm)
    } else {
        None
    };
    let mut published = 0usize;
    // gen r5w1/young5 (P4 of
    // `docs/feature-designs/generative-gc-architecture.md`):
    // the three exclusion counters are accumulated here and added once per
    // frame, not bumped on a process-global cache line per excluded word. Same
    // totals; nothing reads them during a scan.
    let (mut regoop_excluded, mut deadspill_excluded, mut outgoing_excluded) =
        (0usize, 0usize, 0usize);
    // gen r5w3/live7: the fourth claim (`CRATONVM_JIT_PRECISE_FRAME_LIVENESS`).
    // `None` -- claim nothing -- off the flag and for every body compiled
    // without it, so the loop below is unchanged there: `live_sp`.
    let mut liveset_excluded = 0usize;
    // gci i19/nt: the frame's current shadow push, resolved on the first
    // spill-image word that asks ([`SpillImagePushProof`]).
    let spill_push = SpillImagePushProof::new(rbp, cm, heap.is_g1());
    // gci i23/rf: the words the reference-free floor cut ([`REF_FREE_WORDS_EXCLUDED`]).
    let mut ref_free_excluded = 0usize;
    while addr + 8 <= rbp {
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        let slot = addr;
        addr += 8;
        // gci i23/rf: the walk runs from the deep end up, so `off` only falls;
        // the first word below the floor starts the run of words up to `rbp`
        // that a `frame_holds_no_references` body never wrote a reference to
        // (its contract 1). None of them is read; the oracle
        // (`CRATONVM_DBG_VERIFY_REG_OOP_MAPS`) still sees each one.
        if ref_free_lo.is_some_and(|lo| off < lo) {
            ref_free_excluded = (rbp - slot) / 8;
            if verify_reg_oop_maps() {
                let mut a = slot;
                while a + 8 <= rbp {
                    // Cast: as `off` above.
                    note_excluded_band_word(a, (rbp - a) as i32, layout, heap, "reference-free");
                    a += 8;
                }
            }
            break;
        }
        // This frame tail is ABI bookkeeping and padding, not Java oop storage.
        // A prior caller can leave a heap address in the reused bytes; rooting
        // it here keeps dead arrays alive even though no frame map can name it.
        if layout.is_non_oop_reserved_tail_slot(off) {
            continue;
        }
        // gcf f1/j: a word outside the envelope is never an object start, so
        // it is neither a root, a pin nor a publication, and the claims below
        // have nothing to decide about it. Only their (diagnostic) exclusion
        // counters stop counting it.
        if let Some((lo, hi)) = envelope_first {
            // SAFETY: aligned read inside this thread's own live compiled
            // frame, the word the scan itself reads below.
            let w = unsafe { (slot as *const usize).read() };
            if w < lo || w >= hi {
                env_skipped += 1;
                continue;
            }
            env_kept += 1;
        }
        // Both halves skip a slot the mask excludes: it is not a root, so it
        // is not a pin either.
        if !spill_slot_may_hold_oop(layout, off, reg_mask) {
            regoop_excluded += 1;
            note_excluded_band_word(slot, off, layout, heap, "reg-mask");
            continue;
        }
        // gcg g3/v: an image slot no spill of this id wrote holds an earlier
        // safepoint's leftover (`ActiveMapFacts::spill_written`); the same
        // "both halves skip it" rule. Counted with the mask's exclusions.
        if !spill_slot_may_hold_oop(layout, off, spill_written) {
            regoop_excluded += 1;
            note_excluded_band_word(slot, off, layout, heap, "spill-unwritten");
            continue;
        }
        // The deopt `SavedRegisters` XMM image never holds object references.
        // `deopt::try_resolve_value` only yields an `Object` from
        // `FrameValue::RegisterRef`, which reads `regs.gpr`, not `regs.xmm`.
        // Scanning these slots is what produced the `prov=[deopt-saved-xmm-image]`
        // interior-pointer false roots that forced G1 to pin entire Eden/Survivor
        // regions. The publication still classifies them, via
        // `add_movable_jit_root` (no rewriter needed because nothing reads them as
        // references); they are just not emitted as conservative roots.
        let xmm_image = layout.is_deopt_saved_xmm_image(off);
        // gci i4/so: the optimizing tier's last colour is the same claim's
        // word ([`ir_last_colour_is_dead_above_cursor`]).
        if spill_slot_is_dead_above_cursor(layout, off, live_hi)
            || ir_last_colour_is_dead_above_cursor(cm, off, live_hi)
        {
            if !xmm_image {
                deadspill_excluded += 1;
                note_excluded_band_word(slot, off, layout, heap, "dead-spill");
            }
            continue;
        }
        if is_dead_outgoing_reserve(layout, off, reg_mask, staged_unmapped) {
            if !xmm_image {
                outgoing_excluded += 1;
                note_excluded_band_word(slot, off, layout, heap, "outgoing");
            }
            continue;
        }
        // gen r5w3/live7: a word the emitter's liveness record proves dead at
        // this safepoint is neither a root nor a pin -- the same "both halves
        // skip it" rule as the three claims above.
        if is_dead_by_frame_liveness(cm, off, live_sp) {
            if !xmm_image {
                liveset_excluded += 1;
                note_excluded_band_word(slot, off, layout, heap, "live-set");
                note_liveset_contradiction(cm, off, live_sp);
            }
            continue;
        }
        // gen r5w6/oomjit10: neither a root nor a pin, like the four above --
        // but only for a word the scan would ROOT (an object start); any
        // other word falls through unchanged, so a derived pointer keeps the
        // G1 region pin `g1_band_reject_pins` captures for it.
        if shadow_stack_enabled()
            && cm.shadow_off_in_thread != 0
            && layout.spill_image_index(off).is_some()
        {
            image_excluded += 1;
            note_excluded_band_word(slot, off, layout, heap, "spill-image-shadow");
            continue;
        }
        if image_rule {
            if cm.ir_saved_gpr_slots.is_some()
                && is_callee_saved_gpr_image(off, layout)
                && callee_saved_image_register(cm, off).is_none()
            {
                image_excluded += 1;
                note_excluded_band_word(slot, off, layout, heap, "ir-tail-non-saved");
                continue;
            }
            if let Some(reg) = callee_saved_image_register(cm, off) {
                // SAFETY: aligned read inside this thread's own live compiled
                // frame, the word the scan itself reads below.
                let qword = unsafe { (slot as *const usize).read() };
                if heap.is_object_address_for_root_scan(qword).is_some() {
                    let claim = match caller_claims.as_deref_mut() {
                        Some(memo) => memo.claim(rbp, reg),
                        None => caller_register_claim(rbp, reg, walk_hi),
                    };
                    // gci i13/oj: `DoorOwned` drops only a word the
                    // promising door handed over.
                    if claim.excludes_word(qword) {
                        image_excluded += 1;
                        let why = if claim == CallerRegisterClaim::DoorOwned {
                            "door-handed"
                        } else {
                            "caller-dead"
                        };
                        note_excluded_band_word(slot, off, layout, heap, why);
                        continue;
                    }
                } else if dead_word_rejects_no_pin {
                    // gkr (`T1W13BigAfterChurn chain-alone` on G1): a word that
                    // is not an object start, in the save of a register a
                    // single-pass owner proves dead, is a stale value, not a
                    // derived pointer: no reject pin. `row`'s save of `main`'s
                    // R12 held an interior word into a dropped 52 MiB array and
                    // kept its humongous span through every collection.
                    let claim = match caller_claims.as_deref_mut() {
                        Some(memo) => memo.claim(rbp, reg),
                        None => caller_register_claim(rbp, reg, walk_hi),
                    };
                    if claim == CallerRegisterClaim::DeadValue {
                        image_excluded += 1;
                        continue;
                    }
                }
            }
        }
        if let Some(prims) = ir_prims {
            if i16::try_from(off).is_ok_and(|o| prims.binary_search(&o).is_ok()) {
                // SAFETY: aligned read inside this thread's own live compiled
                // frame, the word the scan itself reads below.
                let qword = unsafe { (slot as *const usize).read() };
                // JIT round 25 wave 4: a word of the PER-BODY claim is ignored
                // whole -- no root, no G1 reject pin, no publication -- unless
                // it is a minted handle (see `ir_body_claim_ignores_word`).
                if body_claim.is_some()
                    && !crate::memory::smuggled_longs::is_minted(heap, qword as u64)
                {
                    if heap.is_object_address_for_root_scan(qword).is_some() {
                        prim_excluded += 1;
                        note_excluded_band_word(slot, off, layout, heap, "prim-colour");
                    } else {
                        body_claim_unpinned += 1;
                    }
                    continue;
                }
                // Only a word the scan would ROOT (an object start) is let go
                // of. Anything else falls through unchanged -- in particular a
                // derived pointer (an element cursor, never an object start),
                // whose G1 region pin `g1_band_reject_pins` still captures.
                // Cast: a stack word widened to the `long` bits it carries.
                if heap.is_object_address_for_root_scan(qword).is_some()
                    && !crate::memory::smuggled_longs::is_minted(heap, qword as u64)
                {
                    prim_excluded += 1;
                    note_excluded_band_word(slot, off, layout, heap, "prim-colour");
                    continue;
                }
            }
        }
        // t1 W13 (`CRATONVM_GC_BAND_DEAD_LOCALS_OWE_NO_PIN`, default ON since gkr): a dead
        // java-local home is neither a root nor a pin, WHATEVER word it holds
        // ([`dead_local_home_owes_nothing`]).
        if dead_locals_no_pin
            && dead_local_home_owes_nothing(
                layout,
                off,
                live_hi,
                frame_map.as_deref(),
                sp_locals.is_some_and(|claim| claim.proves_dead(layout, off)),
            )
        {
            note_excluded_band_word(slot, off, layout, heap, "dead-local-home");
            continue;
        }
        // gce e1/f: neither a root nor a pin, like the claims above. Only an
        // object start is let go of, as for the prim colours, so a derived
        // pointer keeps its G1 region pin; and a minted handle stays a root
        // (a `long` local is not a reference local either).
        if let Some(claim) = sp_locals {
            if claim.proves_dead(layout, off) {
                // SAFETY: aligned read inside this thread's own live compiled
                // frame, the word the scan itself reads below.
                let qword = unsafe { (slot as *const usize).read() };
                // Cast: a stack word widened to the `long` bits it carries.
                if heap.is_object_address_for_root_scan(qword).is_some()
                    && !crate::memory::smuggled_longs::is_minted(heap, qword as u64)
                {
                    sp_local_excluded += 1;
                    note_excluded_band_word(slot, off, layout, heap, "sp-local-map");
                    continue;
                }
            }
        }
        // gce e1/f: a residue block is neither a root nor a pin. Only an
        // object start is let go of, as above.
        if deopt_residue
            && layout.is_deopt_saved_gpr_image(off)
            && !deopt_residue_word_may_be_a_save(cm, off)
        {
            // SAFETY: aligned read inside this thread's own live compiled
            // frame, the word the scan itself reads below.
            let qword = unsafe { (slot as *const usize).read() };
            if heap.is_object_address_for_root_scan(qword).is_some() {
                deopt_residue_excluded += 1;
                note_excluded_band_word(slot, off, layout, heap, "deopt-image-residue");
                continue;
            }
            // gkr: and no reject pin for any other word of the residue.
            if dead_word_rejects_no_pin {
                deopt_residue_excluded += 1;
                continue;
            }
        }
        // gci i38/ds (`CRATONVM_GC_BAND_MAP_DEAD_NOT_ROOTS`, opt-in): a word in
        // a region the dataflow models that the RESOLVED active map does not
        // name is neither a root nor a pin ([`band_map_dead_word_excluded`]).
        // Only an object start is let go of, and a minted handle stays a root,
        // as for the prim colours and the single-pass locals above.
        if map_dead_claim && band_map_dead_word_excluded(layout, off, live_hi, frame_map.as_deref())
        {
            // SAFETY: aligned read inside this thread's own live compiled
            // frame, the word the scan itself reads below.
            let qword = unsafe { (slot as *const usize).read() };
            // Cast: a stack word widened to the `long` bits it carries.
            if let Some(obj) = heap.is_object_address_for_root_scan(qword) {
                if !crate::memory::smuggled_longs::is_minted(heap, qword as u64) {
                    note_excluded_band_word(slot, off, layout, heap, "map-dead");
                    if unnamed_dbg {
                        note_unnamed_frame_root(
                            obj,
                            heap,
                            rbp,
                            cm,
                            off,
                            frame_map.as_deref(),
                            live_hi,
                            reg_mask,
                            walk_hi,
                            "excluded-map-dead",
                        );
                    }
                    continue;
                }
            }
        }
        let in_map = band_skip_in_map_enabled()
            && frame_map
                .as_deref()
                .is_some_and(|slots| slots.contains(&off));
        let roots = !xmm_image
            && !in_map
            && !skip.is_some_and(|skip| {
                let class = layout.region_name(off);
                // `operand-spill` is skipped only ABOVE the live cursor. Below it
                // the slots hold the CURRENT operand stack, which is as live as a
                // root gets -- skipping those would not be measuring a ceiling, it
                // would be measuring a crash.
                skip.iter().any(|c| c == class)
                    && (class != "operand-spill" || live_hi.is_some_and(|hi| off >= hi))
            });
        let verifiable = band_slot_is_verifiable(off, layout, live_hi);
        let publishes = roots && band_publication_reads(verifiable, remap_covered);
        if !roots && !publishes {
            continue;
        }
        // SAFETY: aligned read inside this thread's own live compiled frame,
        // over the same interval `scan_one_frame` reads.
        let qword = unsafe { (slot as *const usize).read() };
        if let Some((lo, hi)) = envelope {
            if qword < lo || qword >= hi {
                continue;
            }
        }
        let Some(obj) = heap.is_object_address_for_root_scan(qword) else {
            // A word that would have been a root owes its pin; one that is
            // not a root (the XMM image, a `CRATONVM_JIT_BAND_SKIP` class) was
            // never a pin either.
            if roots && capture_rejects {
                g1_band_reject_pins::note(heap, qword, slot, || {
                    // gkr: and the frame's safepoint facts for the word.
                    format!(
                        "{}{}{}",
                        compiled_reject_origin(cm, layout, off),
                        register_image_detail(rbp, cm, off, reg_mask, walk_hi),
                        band_word_context(rbp, cm, off, live_hi)
                    )                });            }
            continue;
        };
        // gcf f7/f: census only (see the section above `scan_one_frame`).
        if roots {
            note_frame_scan_interior_word(heap, qword);
        }
        if roots {
            // gen r4/mark: G1 only -- see `record_provenance_for`.
            if record_provenance || holder_prov {
                // gen r5w6/oomjit10: a register image also names its register
                // and the claim that kept it (`register_image_detail`).
                // gcd d1/b: and the frame's tier, active safepoint, live
                // cursor and whether that safepoint's map names the word
                // (`band_word_context`).
                note_root_provenance(
                    record_provenance,
                    qword,
                    format!(
                        "method={} off={} region={}{}{}{}",
                        cm.method_label,
                        off,
                        layout.region_name(off),
                        register_image_detail(rbp, cm, off, reg_mask, walk_hi),
                        // gcg g3/v: the written set that kept an image word
                        // (only under `CRATONVM_JIT_SPILL_WRITTEN_SET`).
                        spill_written
                            .filter(|_| layout.spill_image_index(off).is_some())
                            .map_or(String::new(), |w| format!(" written=0x{w:x}")),
                        band_word_context(rbp, cm, off, live_hi),
                    ),
                );
            }
            if band_census {
                cratonvm_gc::gc_quiescence::note_band_root_census(
                    obj.as_ptr() as usize,
                    verifiable && band_word_is_map_dead(layout, off, census_map.as_deref()),
                );
            }
            // gci i38/ds: `CRATONVM_DBG_FRAME_ROOT_UNNAMED` (diagnostic).
            if unnamed_dbg {
                note_unnamed_frame_root(
                    obj,
                    heap,
                    rbp,
                    cm,
                    off,
                    frame_map.as_deref(),
                    live_hi,
                    reg_mask,
                    walk_hi,
                    "rooted",
                );
            }
            out.push(obj);
        }
        if publishes
            && publish_validated_band_word(
                qword,
                off,
                verifiable,
                cm,
                live_hi,
                remap_covered,
                &spill_push,
            )
        {
            published += 1;
        }
    }
    if envelope_first.is_some() {
        coverage_census_count(crate::native::jni::JniPhase::EnvWordsSkipped, env_skipped);
        coverage_census_count(crate::native::jni::JniPhase::EnvWordsKept, env_kept);
    }
    if published > 0 {
        UNREWRITABLE_BAND_ROOTS.fetch_add(published, Ordering::Relaxed);
    }
    if regoop_excluded > 0 {
        REGOOP_WORDS_EXCLUDED.fetch_add(regoop_excluded, Ordering::Relaxed);
    }
    if deadspill_excluded > 0 {
        DEADSPILL_WORDS_EXCLUDED.fetch_add(deadspill_excluded, Ordering::Relaxed);
    }
    if outgoing_excluded > 0 {
        OUTGOING_WORDS_EXCLUDED.fetch_add(outgoing_excluded, Ordering::Relaxed);
    }
    if liveset_excluded > 0 {
        LIVESET_CENSUS[LIVESET_WORDS_EXCLUDED].fetch_add(liveset_excluded, Ordering::Relaxed);
    }
    if image_excluded > 0 {
        LIVESET_CENSUS[IMAGE_CALLER_DEAD_EXCLUDED].fetch_add(image_excluded, Ordering::Relaxed);
    }
    if prim_excluded > 0 {
        LIVESET_CENSUS[IR_PRIM_EXCLUDED].fetch_add(prim_excluded, Ordering::Relaxed);
    }
    if body_claim_unpinned > 0 {
        LIVESET_CENSUS[IR_BODY_CLAIM_UNPINNED].fetch_add(body_claim_unpinned, Ordering::Relaxed);
    }
    if sp_local_excluded > 0 {
        LIVESET_CENSUS[SP_LOCAL_EXCLUDED].fetch_add(sp_local_excluded, Ordering::Relaxed);
    }
    if deopt_residue_excluded > 0 {
        LIVESET_CENSUS[DEOPT_RESIDUE_EXCLUDED].fetch_add(deopt_residue_excluded, Ordering::Relaxed);
    }
    // gci i23/rf: a frame counts as engaged when the floor applied, whether
    // or not its band reached below it.
    if ref_free_lo.is_some() {
        LIVESET_CENSUS[REF_FREE_FRAMES].fetch_add(1, Ordering::Relaxed);
        if ref_free_excluded > 0 {
            LIVESET_CENSUS[REF_FREE_WORDS_EXCLUDED].fetch_add(ref_free_excluded, Ordering::Relaxed);
        }
    }
}

/// gci i38/ds -- `CRATONVM_DBG_FRAME_ROOT_UNNAMED=1` (diagnostic, changes no
/// root): name every compiled-frame band word the marking scan ROOTS although
/// the frame's active safepoint map does not name its slot.
///
/// The question it answers (i38 brief, lane ds): three shapes keep an object
/// whose only remaining word sits in a live compiled frame
/// (`GciWy26WeakYoungProbe` inline, i35/wy WY3-P = P-BAND;
/// `GciOm31KeptOomeProbe` drop-warm / keep-warm; `GciI32EcKeptFinalizableChildProbe
/// aged`). [`scan_one_frame_filtered_from`] roots every object-start word of
/// a resolved frame's band that none of its claims excludes, whether or not
/// the active map names it, so a dead slot (an operand spill below the live
/// cursor, a java-local home of the optimizing tier, a register image, the
/// blind spill, the outgoing reserve) is a ROOT, not only a pin. The line
/// names the object, its class id, the frame, the slot's region and the
/// tier / safepoint / live cursor / map facts, so a row can match it against
/// the probe's referent (`CRATONVM_DBG_WATCHREF` prints the referent
/// address) and name the claim that would have to cover that region.
///
/// Read once per chain walk ([`BandClaimSwitches::read`], gci i41/rs; no process
/// static: the statics ratchet).
pub(in crate::jit::conservative_roots) fn dbg_frame_root_unnamed() -> bool {
    false
}

/// Is the slot at `off` one the resolved map does NOT name? `None` (no map
/// resolved) names nothing. Pure, for the tests.
#[inline]
pub(in crate::jit::conservative_roots) fn frame_slot_unnamed_by_map(map: Option<&[i32]>, off: i32) -> bool {
    !map.is_some_and(|slots| slots.contains(&off))
}

/// The `[frame-root-unnamed]` line. Pure, for the tests. `verdict` is
/// `rooted` (the scan roots the word) or `excluded-map-dead` (the opt-in
/// [`band_map_dead_word_excluded`] claim let it go).
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn unnamed_frame_root_line(
    obj: usize,
    class_id: u32,
    method: &str,
    off: i32,
    region: &str,
    map_resolved: bool,
    register_detail: &str,
    context: &str,
    verdict: &str,
) -> String {
    format!(
        "[frame-root-unnamed] obj=0x{obj:x} cid={class_id} method={method} off={off} \
         region={region} map={}{register_detail}{context} verdict={verdict}",
        if map_resolved { "resolved" } else { "none" },
    )
}

/// Print the [`dbg_frame_root_unnamed`] line for a band word the scan roots
/// (or the map-dead claim excludes), when the active map does not name its
/// slot.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn note_unnamed_frame_root(
    obj: ObjectRef,
    heap: &VmHeap,
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    off: i32,
    map: Option<&[i32]>,
    live_hi: Option<i32>,
    reg_mask: Option<u16>,
    walk_hi: usize,
    verdict: &'static str,
) {
    if !frame_slot_unnamed_by_map(map, off) {
        return;
    }
    eprintln!(
        "{}",
        unnamed_frame_root_line(
            obj.as_ptr() as usize,
            heap.class_id_of(obj).as_u32(),
            &cm.method_label,
            off,
            cm.frame_layout.region_name(off),
            map.is_some(),
            &register_image_detail(rbp, cm, off, reg_mask, walk_hi),
            &band_word_context(rbp, cm, off, live_hi),
            verdict,
        )
    );
}

/// gci i38/ds -- `CRATONVM_GC_BAND_MAP_DEAD_NOT_ROOTS=1` (OPT-IN): spend the
/// map's deadness claim in the MARKING band scan too.
///
/// The defect (i38 brief, lane ds): [`scan_one_frame_filtered_from`] roots a
/// band word in a region the dataflow models (java-local homes, the operand
/// spill below the live cursor, a mapped ref-hoist slot) even when the
/// active safepoint's RESOLVED map does not name the slot -- the very claim
/// the band verifier already spends ([`band_slot_is_verifiable_with_map`]:
/// such a word is not demanded on the shadow stack, so a relocating cycle is
/// licensed past it) and the young pin ledger spends when it trusts deadness
/// ([`young_pin_slot_excuses`]). Only the single-pass java-local homes had a
/// root-side claim ([`SpLocalClaim`], `CRATONVM_GC_SP_LOCAL_MAP_ROOTS`); an
/// optimizing-tier home, an OSR body's spill and a mapped ref-hoist slot did
/// not, so a dead word there keeps its object alive for as long as the frame
/// lives (the `GciOm31KeptOomeProbe` / `GciI32EcKeptFinalizableChildProbe
/// aged` shapes, if `CRATONVM_DBG_FRAME_ROOT_UNNAMED` names such a word).
///
/// Why opt-in: it changes retention, and its soundness rests on the map
/// being complete for the modelled regions -- the bet the relocation licence
/// already makes, but a wrong map here frees an object instead of leaving a
/// word stale. `vi38-ds.list` block M decides it. Read once per chain walk
/// ([`BandClaimSwitches::read`], gci i41/rs; no process static).
pub(in crate::jit::conservative_roots) fn band_map_dead_not_roots_enabled() -> bool {
    true
}

/// The slots the active safepoint's maps NAME, for the i38/ds diagnostic and
/// claim: [`frame_active_map_slots`] (the union of every map with the frame's
/// id, `None` when none resolved) widened by each such map's
/// `local_oop_mask` homes (`[rbp - 8*(k+1)]`), as [`SpLocalClaim`] widens it,
/// so a local the dataflow oracle calls an oop is never called dead here even
/// if no slot offset lists it. Sorted, deduplicated.
pub(in crate::jit::conservative_roots) fn frame_active_named_slots(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<Vec<i32>> {
    let mut slots = frame_active_map_slots(rbp, cm)?;
    if let Some(id) = active_safepoint_id(rbp, cm) {
        for m in cm.oop_maps.iter().filter(|m| m.bytecode_pc == id) {
            slots.extend(local_mask_homes(m.local_oop_mask));
        }
    }
    slots.sort_unstable();
    slots.dedup();
    Some(slots)
}

/// The `[rbp - off]` homes of the locals a `local_oop_mask` names (bit `k`
/// = local `k`, home `8*(k+1)`). `None` names none. Pure, for the tests.
pub(in crate::jit::conservative_roots) fn local_mask_homes(mask: Option<u64>) -> impl Iterator<Item = i32> {
    let mask = mask.unwrap_or(0);
    (0..64i32).filter(move |&k| mask & (1u64 << k) != 0).map(|k| 8 * (k + 1))
}

/// The claim [`band_map_dead_not_roots_enabled`] spends: the word at `off`
/// is verifiable without a map (not a register image, not the callee-saved
/// area or beyond, not an operand spill above the live cursor -- those keep
/// their own rules) and the resolved map proves it dead
/// ([`band_word_is_map_dead`]: a modelled region, absent from the map). No
/// resolved map (`None`) proves nothing. Pure, for the tests.
#[inline]
pub(in crate::jit::conservative_roots) fn band_map_dead_word_excluded(
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
    live_hi: Option<i32>,
    map: Option<&[i32]>,
) -> bool {
    map.is_some()
        && band_slot_is_verifiable_with_map(off, layout, live_hi, None)
        && band_word_is_map_dead(layout, off, map)
}

/// t1 W13 (`docs/feature-designs/jit-ir-optimizations.md`):
/// `CRATONVM_GC_BAND_DEAD_LOCALS_OWE_NO_PIN` (default ON since gkr, 2026-10-28,
/// `=0` kills; WARNING gkr-dead-locals-flip-20261028). The map-dead and
/// single-pass locals claims let go only of an object START, so a derived
/// pointer keeps its G1 region pin. A java-local HOME holds a reference,
/// never a derived pointer, so when a claim proves the home dead, an interior
/// word there is stale too: before, a dead `catch` variable's home in a
/// single-pass `row(String, Runnable)` frame held an interior word into a
/// dropped 52 MiB array, and its reject pin kept the array through every
/// collection. Read once per chain walk ([`BandClaimSwitches::read`]).
pub(in crate::jit::conservative_roots) fn band_dead_locals_owe_no_pin_enabled() -> bool {
    // WARNING gkr-dead-locals-flip-20261028: default ON, `=0` restores the pin.
    true
}

/// The claim [`band_dead_locals_owe_no_pin_enabled`] spends: `off` is a
/// java-local home and either the single-pass locals claim (`sp_dead`) or the
/// resolved map ([`band_map_dead_word_excluded`]) proves it dead. Pure.
#[inline]
pub(in crate::jit::conservative_roots) fn dead_local_home_owes_nothing(
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
    live_hi: Option<i32>,
    map: Option<&[i32]>,
    sp_dead: bool,
) -> bool {
    layout.java_locals_hi > 0
        && off < layout.java_locals_hi
        && (sp_dead || band_map_dead_word_excluded(layout, off, live_hi, map))
}

/// Should a conservative JIT-root hit record its provenance string?
///
/// gen r4/mark (2026-09-23): only under G1. `record_jit_root_provenance` costs a
/// `format!` (a heap `String`) plus an insert into a process-wide
/// `Mutex<HashMap>` for EVERY root found, on every stack scan -- the collector's
/// own and the snapshot scans some native calls take. The one reader in the
/// tree is G1's non-object-root warning (`g1.rs`,
/// `pinned_region_set_including_non_object_roots`), so on the Generational
/// and ZGC backends every one of those allocations and lock acquisitions was
/// written and never read, and the map only grew until the next
/// `clear_pinned_jit_roots`.
///
/// r11 wave 11 (lane rt): on G1 only during a collection's own root scan
/// (`gc_quiescence::jit_root_provenance_wanted`). The collection empties the
/// map when it starts, so what a peer's deposit or a native call's snapshot
/// scan recorded before that was never read either.
///
/// gen r4w6/oomjit6 (2026-09-24): ALSO under `CRATONVM_DBG=oldmark-root-census`,
/// on any backend. That census is the second reader: its top-root lines print
/// `prov="method=… off=… region=…"` from this map
/// (`gc/src/gen_heap_oldmark_census.rs`), which names the frame, the offset
/// and the storage class of a compiled-frame word that keeps old-gen data
/// alive. Without it a Generational run printed no provenance at all, which is
/// item 3 of
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// Asking `oldmark_census_on` here also arms the census's VM labeller at the
/// first scan, instead of at the first compiled-code exception door.
#[inline]
pub(in crate::jit::conservative_roots) fn record_provenance_for(heap: &VmHeap) -> bool {
    // JIT round 21 lane refs: `CRATONVM_DBG_REFERENT_HOLDER` is the third
    // reader (`gc_and_alloc::report_referent_holder`), on every backend, inside
    // a collection's own root scan only (as the G1 arm).
    ((heap.is_g1() || crate::runtime::env_cache::dbg_referent_holder())
        && cratonvm_gc::gc_quiescence::jit_root_provenance_wanted())
        || crate::jit::helpers::oldmark_census_on()
}

/// JIT round 21 lane refs: route one root provenance record. Inside a
/// collection's armed root scan (`armed`) it goes to the per-collection map
/// (`record_jit_root_provenance`); otherwise -- a blocking deposit or snapshot
/// scan under `CRATONVM_DBG_REFERENT_HOLDER`, which runs on the depositing
/// thread before the collection clears that map -- to the referent-holder map
/// (`record_jit_holder_provenance`), which survives the clear.
pub(in crate::jit::conservative_roots) fn note_root_provenance(armed: bool, qword: usize, prov: String) {
    if armed {
        cratonvm_gc::gc_quiescence::record_jit_root_provenance(qword, prov);
    } else {
        cratonvm_gc::gc_quiescence::record_jit_holder_provenance(
            qword,
            format!(
                "unarmed-scan thread={:?} {prov}",
                std::thread::current().id()
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// The register-oop mask's falsification oracle
// ---------------------------------------------------------------------------
//
// `ActiveMapFacts::reg_mask` is a licence to DROP roots, so the only question that
// matters about it is whether it ever drops one that mattered. A count of
// dropped words cannot answer that -- a blind spill image is full of dead
// register leftovers, and dropping those is the entire point.
//
// The discriminating question is narrower: of the words this mask excluded, how
// many named an object that NO OTHER ROOT names? Those, and only those, are the
// ones the collector would now free on the mask's word alone. Everything else
// is a duplicate the rest of the root set already covers, and its removal
// cannot change what survives.
//
// So the oracle records every excluded word that resolved to an object, and
// `verify_excluded_spill_words` re-checks them against the FINISHED root set --
// which is why it cannot run inside the band scan, where the root set is still
// half-built. A non-zero `only-root` count is a refutation of the mask and
// should stop the change; a large `duplicate` count beside a zero `only-root`
// count is what a correct narrowing looks like.
//
// `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`. Off by default and free when off: the
// recording site is one already-resolved `OnceLock` load per excluded word.

/// `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` -- see the module comment above.
pub(in crate::jit::conservative_roots) fn verify_reg_oop_maps() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

thread_local! {
    /// `(object address, frame offset, register name)` for each blind-spill
    /// word this pass excluded that nevertheless resolved to an object.
    pub(in crate::jit::conservative_roots) static EXCLUDED_SPILL_WORDS: std::cell::RefCell<Vec<(usize, i32, &'static str, &'static str)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Running totals for the oracle. See [`verify_excluded_band_words`].
pub(in crate::jit::conservative_roots) static ORACLE_WORDS: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) static ORACLE_REACHABLE: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) static ORACLE_UNREACHABLE: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) static ORACLE_INCOMPLETE: AtomicUsize = AtomicUsize::new(0);

/// `(words, reachable, unreachable, walk_incomplete)`.
///
/// **`unreachable` is the number that decides whether the narrowings are
/// sound**, and `walk_incomplete` is the number that decides whether
/// `unreachable` may be read at all.
pub fn reg_oop_mask_oracle() -> (usize, usize, usize, usize) {
    (
        ORACLE_WORDS.load(Ordering::Relaxed),
        ORACLE_REACHABLE.load(Ordering::Relaxed),
        ORACLE_UNREACHABLE.load(Ordering::Relaxed),
        ORACLE_INCOMPLETE.load(Ordering::Relaxed),
    )
}

/// Record a band word the narrowings excluded, for
/// [`verify_excluded_band_words`]. Inert unless the oracle is armed.
#[inline]
pub(in crate::jit::conservative_roots) fn note_excluded_band_word(
    addr: usize,
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    heap: &VmHeap,
    why: &'static str,
) {
    if !verify_reg_oop_maps() {
        return;
    }
    // SAFETY: the same aligned in-frame read the scan itself would have made at
    // this address had the narrowing not excluded it.
    let qword = unsafe { (addr as *const usize).read() };
    // The predicate the scan itself would have rooted the word with.
    if heap.is_object_address_for_root_scan(qword).is_none() {
        return;
    }
    let reg = layout
        .spill_image_register(off)
        .unwrap_or_else(|| layout.region_name(off));
    EXCLUDED_SPILL_WORDS.with(|v| v.borrow_mut().push((qword, off, reg, why)));
}

/// Clear the oracle's per-pass record. Called by the root gatherer beside the
/// other per-collection resets.
pub fn clear_excluded_spill_words() {
    if !verify_reg_oop_maps() {
        return;
    }
    EXCLUDED_SPILL_WORDS.with(|v| v.borrow_mut().clear());
}

/// Decide, for each band word the narrowings dropped, whether dropping it can
/// have changed what survives.
///
/// # Why the first version of this was worthless
///
/// It asked whether any OTHER ROOT named the same address, and reported the
/// ones where none did. That is not the question. A genuinely dead object --
/// exactly what these narrowings exist to release -- is by construction named
/// only by dead words, so it always "refuted" the change. The instrument fired
/// 20 times on `TestValueMemory` and could not say whether even one of them
/// mattered, which is the same as not having an instrument.
///
/// # The question that does decide it
///
/// **Is the object still TRANSITIVELY REACHABLE from the narrowed root set?**
///
/// * reachable -- dropping the word changed nothing. The object survives on
///   another path and the narrowing is provably free here.
/// * unreachable -- the object's survival depended on the dropped word. It is
///   either garbage (the intended win) or a live object the narrowing lost, and
///   nothing short of the program's own behaviour separates those. Every one of
///   these is a case that needs a human.
///
/// So `unreachable == 0` over a corpus is the licence to ship, and any non-zero
/// count is a finding to explain rather than a number to watch.
///
/// # Reading the walk-incomplete count first
///
/// [`cratonvm_gc::gc::for_each_object_reference`] DECLINES on an object it
/// cannot walk flatly (a G1 humongous object is region-fragmented) and on a
/// header it cannot decode. Those objects contribute no edges, so the closure
/// is an UNDER-approximation and "unreachable" is over-reported. A run with a
/// non-zero `walk_incomplete` cannot have its `unreachable` count read as a
/// refutation -- that is the trap the previous version fell into from the other
/// direction, and it is stated here so the next reader does not have to
/// rediscover it.
pub fn verify_excluded_band_words(roots: &[ObjectRef], heap: &VmHeap) {
    if !verify_reg_oop_maps() {
        return;
    }
    // gen r5w3/live7: the liveness claim's own counts, once per collection
    // (this runs once per root gathering) and only once it has dropped
    // anything, so a run without `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` prints
    // nothing new. Cumulative; read the growth.
    let (words, contradictions) = frame_liveness_census();
    if words > 0 || contradictions > 0 {
        eprintln!("[liveset] words_excluded={words} contradictions={contradictions}");
    }
    // gce e1/f: the single-pass java-local claim's count, same shape.
    let sp_local = LIVESET_CENSUS[SP_LOCAL_EXCLUDED].load(Ordering::Relaxed);
    let residue = LIVESET_CENSUS[DEOPT_RESIDUE_EXCLUDED].load(Ordering::Relaxed);
    if sp_local > 0 || residue > 0 {
        eprintln!("[sp-local-map] words_excluded={sp_local} deopt_image_residue={residue}");
    }
    // gcf f7/f: the frame-scan interior census's count, same shape.
    let interior_seen = frame_scan_interior_census();
    if interior_seen > 0 {
        eprintln!("[frame-interior] words_seen={interior_seen}");
    }
    // gci i23/rf: the reference-free floor's engagement, same shape.
    let (ref_free_frames, ref_free_words) = reference_free_frame_census();
    if ref_free_frames > 0 {
        eprintln!("[ref-free] frames={ref_free_frames} words_excluded={ref_free_words}");
    }
    let pending: Vec<(usize, i32, &'static str, &'static str)> =
        EXCLUDED_SPILL_WORDS.with(|v| std::mem::take(&mut *v.borrow_mut()));
    if pending.is_empty() {
        return;
    }
    // Closure from the NARROWED root set -- the set the collector is about to
    // use. Capped so a pathological heap cannot turn a debug flag into a hang;
    // hitting the cap is itself an incomplete walk.
    const MAX_VISITED: usize = 4_000_000;
    // UNIFORM, and NOT raiseable per collector. A flat payload read past a
    // certain size is unsafe on EVERY backend here, because they all place a
    // large object across spans that are not contiguous in the address space --
    // G1 as `HumongousStart` plus continuations with their own buffers, ZGC
    // across pages. This was tried as `if heap.is_g1() { 512K } else { 64M }`
    // on the reasoning that only G1 fragments, and the 64 MiB arm SEGFAULTED
    // 3 runs in 6 on `TestValueMemory` under ZGC (0 in 6 with the oracle off,
    // narrowings on or off, so it was the walk and not the change under test).
    // The cost of the low cap is real and is reported rather than hidden:
    // declining H2's 1 000 016-byte `Object[]` leaves its 125 000 elements
    // looking unreachable, which is why `walk_incomplete` has to be read before
    // `UNREACHABLE` means anything.
    const MAX_OBJECT_BYTES: usize = 512 * 1024;
    // addr-keyed: function-local walk of this pause's narrowed roots (debug oracle).
    let mut seen: std::collections::HashSet<usize> =
        std::collections::HashSet::with_capacity(roots.len() * 2);
    let mut stack: Vec<usize> = Vec::with_capacity(roots.len());
    let mut incomplete = 0usize;
    for r in roots {
        let a = r.as_ptr() as usize;
        if seen.insert(a) {
            stack.push(a);
        }
    }
    while let Some(obj) = stack.pop() {
        if seen.len() >= MAX_VISITED {
            incomplete += 1;
            break;
        }
        let mut children: Vec<usize> = Vec::new();
        // SAFETY: the world is stopped for this collection, and every address
        // reaching here was validated by `is_object_address` -- the roots by
        // their producers, the children just below.
        let walked = unsafe {
            cratonvm_gc::gc::for_each_object_reference(obj, MAX_OBJECT_BYTES, &mut |c| {
                children.push(c)
            })
        };
        if !walked {
            incomplete += 1;
        }
        for c in children {
            if heap.is_object_address(c).is_none() {
                continue;
            }
            if seen.insert(c) {
                stack.push(c);
            }
        }
    }
    if incomplete > 0 {
        ORACLE_INCOMPLETE.fetch_add(incomplete, Ordering::Relaxed);
    }
    for (addr, off, reg, why) in pending {
        ORACLE_WORDS.fetch_add(1, Ordering::Relaxed);
        if seen.contains(&addr) {
            ORACLE_REACHABLE.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let n = ORACLE_UNREACHABLE.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 16 || n.is_power_of_two() {
            eprintln!(
                "[regoop] UNREACHABLE (#{n}, {why}): band slot off={off} ({reg}) named \
                 0x{addr:x}, which is NOT reachable from the narrowed root set — this \
                 narrowing decided its fate. Garbage, or a live object just lost; only \
                 behaviour tells them apart. walk_incomplete={} so far.",
                ORACLE_INCOMPLETE.load(Ordering::Relaxed),
            );
        }
    }
}

/// Total addresses published to the unrewritable-root veto this process.
pub(in crate::jit::conservative_roots) static UNREWRITABLE_BAND_ROOTS: AtomicUsize = AtomicUsize::new(0);
/// Words that were UNVERIFIABLE yet published movable because
/// `remap_register_image_words` rewrites them anyway.
///
/// The number that says whether the "unverifiable" and "unrewritten" sets had
/// actually drifted apart: a non-zero count is a word that was being pinned for
/// a rewrite that was already happening.
pub(in crate::jit::conservative_roots) static REMAPPED_NOT_PINNED: AtomicUsize = AtomicUsize::new(0);

/// Diagnostic counterpart of [`unrewritable_band_root_count`].
pub fn remapped_not_pinned_count() -> usize {
    REMAPPED_NOT_PINNED.load(Ordering::Relaxed)
}

/// Objects published MOVABLE from a verifiable band word this run.
///
/// The complement of [`unrewritable_band_root_count`], and the two are read
/// together: a movable count of zero with a non-zero unrewritable count means
/// every band word this workload holds is one nothing rewrites, which is a
/// different situation from the partition never having been computed.
pub(in crate::jit::conservative_roots) static MOVABLE_BAND_ROOTS: AtomicUsize = AtomicUsize::new(0);

/// Diagnostic counterpart of [`unrewritable_band_root_count`].
pub fn movable_band_root_count() -> usize {
    MOVABLE_BAND_ROOTS.load(Ordering::Relaxed)
}

/// `CRATONVM_GC_MOVABLE_BAND_ROOTS=0` — stop publishing the verifiable half of
/// the band partition as movable, i.e. go back to every conservative JIT root
/// pinning its G1 region. **Default ON.**
///
/// The bisect lever for the publication in `publish_unrewritable_band_roots`.
/// It is separate from `CRATONVM_GC_G1_MOVABLE_PINS` (the CONSUMER) because the
/// two can fail differently: a wrong publication here is a movable claim for a
/// word something still reads, while a wrong consumption there is a filter that
/// ignores the veto. A stale-pointer report has to be attributable to one.
pub(in crate::jit::conservative_roots) fn gc_movable_band_roots_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G,
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        },
    )
}

/// Addresses published to the unrewritable-root veto since process start.
/// (gci i23/rf: this line sat stranded as the first line of
/// `scan_a5_band_as_frames`'s doc.)
///
/// Diagnostic only. A non-zero count means at least one compiled frame held a
/// live object in a word `band_slot_is_verifiable` refuses to inspect — i.e.
/// the pin below is doing work, not just costing a branch.
pub fn unrewritable_band_root_count() -> usize {
    UNREWRITABLE_BAND_ROOTS.load(Ordering::Relaxed)
}

/// Which of [`band_slot_is_verifiable`]'s three refusals `off` earned.
///
/// Diagnostic vocabulary only, and it must stay in step with that function --
/// the point of the split is that the three refusals have DIFFERENT repairs
/// (`callee-saved` and `register-image` can only be pinned; `dead-spill` is a
/// word nothing reads again, so it need not have been a marking root at all).
pub(in crate::jit::conservative_roots) fn unverifiable_region(
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
) -> &'static str {
    if !layout.callee_saved_shallow && layout.callee_saved_lo > 0 && off >= layout.callee_saved_lo {
        return "callee-saved";
    }
    if layout.is_register_image(off) {
        return "register-image";
    }
    if let Some(hi) = live_hi {
        if layout.spill_hi > layout.spill_lo && off >= layout.spill_lo && off >= hi {
            return "dead-spill";
        }
    }
    "verifiable"
}

/// Publish every object reachable from one compiled frame's UNVERIFIABLE band
/// words to the pin veto (`gc_quiescence::add_unrewritable_jit_root`), and the
/// verifiable half as movable.
///
/// `scan_one_frame` has already walked the whole band and pushed these objects
/// as marking roots — that is what keeps them alive across the pause, and it is
/// also what gets them COPIED, because a movable claim from any other slot
/// naming the same address wins at the pin decision. This second pass says
/// which of those roots arrived through a word nobody can rewrite, so that
/// claim can be vetoed.
///
/// Only the UNFILTERED band arm runs this as a second walk (no register mask,
/// no skip list, `CRATONVM_GC_DEAD_SPILL_ROOTS=0`). The default arm,
/// [`scan_one_frame_filtered`], roots and publishes in ONE walk with one read
/// and one validation per word; both reach the per-word verdict through
/// [`publish_validated_band_word`].
///
/// The two halves must stay the same partition: the words that can pin here
/// are exactly the ones `band_slot_is_verifiable` returns `false` for, which
/// is exactly the set `remap_register_image_words` would otherwise have to
/// REWRITE. Pinning is the sound half of that choice — see the module comment
/// on `UNREWRITABLE_JIT_ROOTS` in `gc_quiescence`.
pub(in crate::jit::conservative_roots) fn publish_unrewritable_band_roots(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    facts: ActiveMapFacts,
    heap: &VmHeap,
    remap_covered: bool,
) {
    if frame_size == 0 || frame_size > rbp {
        return;
    }
    let ActiveMapFacts {
        reg_mask,
        live_hi,
        staged_unmapped,
        spill_written,
    } = facts;
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    // Same clamp as `band_has_unpublished_word_with`: a stale bound must never
    // walk into unmapped pages. A compiled frame is orders of magnitude
    // smaller, so this is unreachable in practice.
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = rbp.min(addr.saturating_add(MAX_SCAN_BYTES));
    let envelope = heap.conservative_addr_span();
    let mut published = 0usize;
    // gen r5w3/live7: the same fourth claim as `scan_one_frame_filtered`.
    let live_sp = frame_liveness_sp_id(rbp, cm);
    // gci i19/nt: as in `scan_one_frame_filtered`.
    let spill_push = SpillImagePushProof::new(rbp, cm, heap.is_g1());
    while addr + 8 <= hi {
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if cm.frame_layout.is_non_oop_reserved_tail_slot(off) {
            addr += 8;
            continue;
        }
        // A blind-spill slot the map excludes is not a root, so it is not a
        // pin either. Skipping it here as well as in the scan is what keeps the
        // two halves the same partition -- pinning a word nothing marks would
        // hold a region out of a G1 collection set for an object that is no
        // longer in the root set at all.
        if !spill_slot_may_hold_oop(&cm.frame_layout, off, reg_mask)
            || !spill_slot_may_hold_oop(&cm.frame_layout, off, spill_written)
            || spill_slot_is_dead_above_cursor(&cm.frame_layout, off, live_hi)
            || ir_last_colour_is_dead_above_cursor(cm, off, live_hi)
            || is_dead_outgoing_reserve(&cm.frame_layout, off, reg_mask, staged_unmapped)
            || is_dead_by_frame_liveness(cm, off, live_sp)
        {
            addr += 8;
            continue;
        }
        let verifiable = band_slot_is_verifiable(off, &cm.frame_layout, live_hi);
        if !band_publication_reads(verifiable, remap_covered) {
            addr += 8;
            continue;
        }
        // SAFETY: aligned read inside this thread's own live compiled frame,
        // bounded by the frame size recorded at compile time — the same
        // interval `scan_one_frame` has already read.
        let qword = unsafe { (addr as *const usize).read() };
        addr += 8;
        if let Some((elo, ehi)) = envelope {
            if qword < elo || qword >= ehi {
                continue;
            }
        }
        // The scan's own predicate, so the two halves stay one partition of
        // the words the scan roots.
        if heap.is_object_address_for_root_scan(qword).is_some()
            && publish_validated_band_word(
                qword,
                off,
                verifiable,
                cm,
                live_hi,
                remap_covered,
                &spill_push,
            )
        {
            // gcj j1pn ledger item 5: the `CRATONVM_DBG_JIT_ROOTSCAN`
            // `[bandword]` line is `publish_validated_band_word`'s own; this
            // caller printed the same line a second time for every word.
            published += 1;
        }
    }
    if published > 0 {
        UNREWRITABLE_BAND_ROOTS.fetch_add(published, Ordering::Relaxed);
    }
}

/// Does the pin publication READ the band word at `off` at all?
///
/// An unverifiable word is always read: it may be a pin. A verifiable word is
/// read only to be published movable, which needs the publication lever
/// ([`gc_movable_band_roots_enabled`]) and a frame the remap walks
/// (`remap_covered`, see [`publish_validated_band_word`]).
#[inline]
pub(in crate::jit::conservative_roots) fn band_publication_reads(verifiable: bool, remap_covered: bool) -> bool {
    !verifiable || (gc_movable_band_roots_enabled() && remap_covered)
}

/// The pin publication's verdict for ONE band word that has already passed the
/// publication's skips (mask, dead spill, dead outgoing reserve) and validated
/// as an object start. Returns `true` when the word was published UNREWRITABLE,
/// i.e. it pins its object.
///
/// Shared by [`publish_unrewritable_band_roots`] and the fused walk in
/// [`scan_one_frame_filtered`] (round 11 wave 13, rt proposal W10-3), so the two
/// cannot classify a word differently.
pub(in crate::jit::conservative_roots) fn publish_validated_band_word(
    qword: usize,
    off: i32,
    verifiable: bool,
    cm: &cratonvm_jit::CompiledMethod,
    live_hi: Option<i32>,
    remap_covered: bool,
    push: &SpillImagePushProof<'_>,
) -> bool {
    if verifiable {
        // Verified storage. An unpublished movable oop here already forces
        // the non-moving sweep, and a published one is rewritten by
        // `remap_one_jit_frame`, so it needs no pin and pinning it would
        // give back the drain this set exists to preserve.
        //
        // SAY SO. That sentence was true and unsaid: nothing published the
        // verifiable half anywhere, so on G1 -- whose pin set was every
        // conservative root without exception -- the word pinned its region
        // regardless, and a REGION is a megabyte at `-Xmx2g`. Publishing
        // the complement is what lets `jit_pinned_region_set` act on the
        // partition this function already computes.
        //
        // The claim per word is the one above, in two cases. NAMED by the
        // active map: `remap_one_jit_frame` rewrites the slot, so the
        // object may move. NOT named, in a region the abstract interpreter
        // MODELS: the word is DEAD -- `band_slot_is_verifiable_with_map`
        // spends exactly that claim to excuse it from shadow publication,
        // and its own measurement is the 74 unpublished words of
        // `MVStore.closeStore` that were javac's out-of-scope copies. A
        // dead word needs no rewrite.
        //
        // Fail-closed by construction: this publishes only what it can
        // argue about. A foreign innermost frame has no layout and never
        // reaches here, so its words stay pinned, and any address ALSO
        // reaching an unverifiable word is vetoed by
        // `add_unrewritable_jit_root` below -- the pin set is keyed by
        // OBJECT, and the veto outranks every movable claim.
        //
        // `remap_covered` is REQUIRED, and leaving it out was a defect in
        // the first version of this: both arms of the claim above name a
        // rewriter (`remap_one_jit_frame`) or rest on the word being dead,
        // and the first is only true for a frame the remap actually walks.
        // A frame recovered by inspection in the A5 band is not on the
        // entry chain and nothing rewrites its slots, so publishing its
        // words movable would have licensed moving an object out from under
        // a slot that keeps pointing at the old address.
        //
        // Callers ask `band_publication_reads` first and never get here
        // otherwise; the test is repeated so this verdict stands on its own.
        //
        // WHY G1 SPENDS THE DEADNESS CLAIM THE YOUNG PIN LEDGER REFUSES
        // (gcj-j1pn-young-pin-ledger-review-findings item 6). When the ledger
        // licenses a relocating Generational cycle it excuses no map-dead word
        // (gen r5w4/pin8, `young_pin_slot_excuses`'s `trust_deadness`), while
        // this arm publishes the same word MOVABLE and G1 evacuates its region
        // on that claim. The two are consistent, for these reasons:
        //
        // * The claim is the oop-map contract, not a heuristic: a slot of a
        //   region the JIT's dataflow models that the ACTIVE safepoint's map
        //   does not name holds nothing the frame reads again. HotSpot's G1
        //   moves objects on exactly that contract (it visits, pins and
        //   rewrites nothing outside the oop map), so trusting it is parity.
        // * The ledger's baseline is different. It licenses moves on a
        //   Generational cycle whose DEFAULT is the non-moving sweep (term 4:
        //   a live compiled frame plus a conservative scan), so its rule is to
        //   add no trust the sweep never needed, and a wrong claim there would
        //   be amplified at once (the pinned copy re-serves the vacated bytes
        //   in the same pause). G1's default is evacuation; the claim only
        //   decides whether a REGION is pinned for a word nobody reads, and
        //   pinning it costs a whole region (a megabyte at `-Xmx2g`; the
        //   `MVStore.closeStore` and `TvmProbe` measurements above).
        // * The claim still fails closed per object: any other word naming the
        //   same object from an unverifiable region vetoes the move
        //   (`add_unrewritable_jit_root` outranks every movable claim).
        //
        // The bisect lever is `CRATONVM_GC_MOVABLE_BAND_ROOTS=0`
        // (`gc_movable_band_roots_enabled`): no verifiable word is published
        // movable, so G1 pins every region such a word names, as before this
        // arm existed.
        if gc_movable_band_roots_enabled() && remap_covered {
            note_movable_band_root(qword);
            MOVABLE_BAND_ROOTS.fetch_add(1, Ordering::Relaxed);
        }
        return false;
    }
    // UNVERIFIABLE, but not necessarily unrewritten. `band_slot_is_verifiable`
    // asks whether the JIT's abstract interpreter MODELS the region;
    // the pin decision needs the different question of whether anything
    // REWRITES the word, and for one region the answer is already yes.
    //
    // `remap_register_image_words` runs on every collection
    // (`CRATONVM_REGISTER_IMAGE_REMAP`, default on) and rewrites exactly
    // what `register_image_remap_admits` accepts, which by default is
    // the callee-saved GPR image, the deopt GPR image and (since
    // 2026-10-01) the blind GPR spill image. The first two are fixed up
    // after a move already, and pinning them bought nothing -- it just
    // held a megabyte-granular G1 region for an object that was going
    // to be rewritten anyway.
    //
    // Two conditions, and both are needed. The region has to be one the
    // remap admits, and the FRAME has to be one the remap reaches:
    // `remap_register_image_words` walks the registered entry chain, so
    // a frame recovered by inspection in the A5 band is not covered and
    // `remap_covered` is false for it.
    // gcg g1/s: `CRATONVM_REGISTER_IMAGE_REMAP=0` must not publish image
    // words as movable (gcg-g1s-register-image-remap-off-...).
    //
    // gci i8/nt: and the per-safepoint BLIND GPR SPILL image is rewritten but
    // never READ BACK -- `emit_pre_safepoint_spill` says so ("no post-call
    // reload is needed") -- so rewriting it does not refresh the register it
    // copies. Only the shadow reload (`ShadowHome::Reg`) does that, so without
    // shadow emission a spill-image word must keep pinning its object; see
    // [`register_image_word_publishes_movable`].
    //
    // gci i19/nt: and WITH shadow emission the reload refreshes only the
    // registers the frame's CURRENT push published. A frame whose active
    // safepoint does not claim complete coverage (an oop the marks could not
    // type, an OSR artifact with no shadow layout, an unmapped splice), whose
    // push bailed or was already popped, or whose push does not hold this very
    // value has a register nothing refreshes; G1's movable filter is not gated
    // on the cycle's coverage (`g1.rs` `jit_pinned_region_set`: "the
    // fail-closed direction is preserved by the PUBLISHER"), so the word must
    // pin here. See [`SpillImagePushProof`].
    if remap_covered
        && register_image_remap_enabled()
        && register_image_remap_admits(off, &cm.frame_layout)
        && register_image_word_publishes_movable(off, &cm.frame_layout, || {
            spill_image_register_is_reloaded() && push.licenses(qword)
        })
    {
        note_movable_band_root(qword);
        MOVABLE_BAND_ROOTS.fetch_add(1, Ordering::Relaxed);
        REMAPPED_NOT_PINNED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // THE DEOPT `SavedRegisters` XMM IMAGE, on the OTHER half of the
    // partition's argument: not "something rewrites it" but "nothing
    // reads it as a reference", which is the same claim
    // `band_slot_is_verifiable`'s dead-word arm spends and needs no
    // rewriter behind it -- hence no `remap_covered` term here.
    //
    // The claim is checkable in one function. `deopt::try_resolve_value`
    // touches `regs.xmm` from exactly two arms, `FrameValue::XmmFloat`
    // and `FrameValue::XmmDouble`, and tags them `Float` and `Double`.
    // The one arm that yields an `Object` out of the spilled register
    // file, `FrameValue::RegisterRef`, reads `regs.gpr`. So a moved
    // object named ONLY from this half is never dereferenced through
    // it, and holding a megabyte-granular G1 region for it buys
    // nothing.
    //
    // This is why the GPR half is REWRITTEN (see
    // `register_image_remap_admits`) and this half is LET GO of: the
    // two halves of one 256-byte region, with opposite arguments, and
    // neither argument reaches the other's half.
    //
    // Measured on `probes/TvmProbe.java` under `-XX:+UseG1GC --Xmx 2g`:
    // with the GPR half rewritten but this half still vetoing, the Eden
    // region pinned for three objects totalling 200 bytes survived and
    // two of forty rows stayed over threshold; every one of its
    // `[bandword]` refusals sat in `[deopt_regs_base - 248,
    // deopt_regs_base - 120)`.
    if cm.frame_layout.is_deopt_saved_xmm_image(off) {
        note_movable_band_root(qword);
        MOVABLE_BAND_ROOTS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    note_unrewritable_band_root(qword);
    // `CRATONVM_DBG_JIT_ROOTSCAN=1` — WHICH unverifiable region this
    // word sits in. The three have nothing in common but the verdict:
    // a callee-saved image holds the CALLER's live registers and can
    // only be pinned, while a spill slot above the safepoint's live
    // cursor is one nothing will ever read again. Reporting them under
    // one `unrewritable=4` cannot separate "must pin" from "need not
    // even mark", which is the only question with a fix behind it.
    if dbg_jit_rootscan() {
        eprintln!(
            "[bandword] 0x{qword:x} off={off} refusal={} region={} \
             live_hi={:?} frame_size={} reg={:?} method={}",
            unverifiable_region(off, &cm.frame_layout, live_hi),
            cm.frame_layout.region_name(off),
            live_hi,
            cm.frame_layout.frame_size,
            cm.frame_layout.spill_image_register(off),
            cm.method_label,
        );
    }
    true
}


// ---------------------------------------------------------------------------
// Post-remap stale-reference detector (`CRATONVM_DBG_JIT_STALE_AFTER_REMAP`)
// ---------------------------------------------------------------------------
//
// `remap_active_jit_frames` rewrites exactly the slots the active oop map
// names. Everything else in a compiled frame -- the callee-saved GPR/XMM save
// areas, the per-safepoint blind GPR spill, operand-spill slots above the
// safepoint's live cursor, the outgoing-argument reserve, and every word of the
// Rust/interpreter frames the compiled method called INTO -- keeps whatever it
// held before the move. The conservative scan READS those regions (that is what
// makes an object there survive), so a moving cycle can copy an object whose
// only holder is a word nothing rewrites.
//
// This walks the same bands the conservative scan reads, AFTER the remap, and
// reports any word that is still a key of `pointer_map` -- i.e. an address the
// collection moved away from. It names the method, the frame offset, the
// `FrameLayout` region and the CLASS of the object at the new address, so the
// storage class responsible is named rather than guessed at. Diagnostic only;
// default off.
pub(in crate::jit::conservative_roots) fn stale_after_remap_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// `CRATONVM_DBG_JIT_STALE_BELOW_RBP` -- additionally walk the Rust /
/// interpreter frames beneath the innermost compiled frame. That range is NOT
/// read by the conservative band scan when the bounded bands are in use, so
/// most of what it holds is dead stack slop and it drowns out the
/// compiled-frame findings; separate flag.
pub(in crate::jit::conservative_roots) fn stale_below_rbp_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

pub(in crate::jit::conservative_roots) static STALE_AFTER_REMAP_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Of those, the ones in a region something RESUMES FROM.
pub(in crate::jit::conservative_roots) static STALE_AFTER_REMAP_RESUMED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Total words found still naming a moved-from address after a remap.
pub fn stale_after_remap_hits() -> usize {
    STALE_AFTER_REMAP_HITS.load(Ordering::Relaxed)
}

/// Of those, how many sat in a region something resumes from — the
/// callee-saved GPR image. See [`stale_after_remap_census`].
pub fn stale_after_remap_resumed_hits() -> usize {
    STALE_AFTER_REMAP_RESUMED.load(Ordering::Relaxed)
}

/// Name the class of the object now living at `addr`, for diagnostics.
///
/// `addr` is a POST-move address (a `pointer_map` value), so the object is
/// live and its header is intact -- which is the whole reason the report is
/// taken here rather than at the point of use, where the from-space cell has
/// already been zeroed and reads back as `java.lang.Object`.
pub(in crate::jit::conservative_roots) fn class_name_at(shared: Option<&crate::vm::SharedVm>, addr: usize) -> String {
    let Some(shared) = shared else {
        return "?".to_string();
    };
    let Some(obj) = shared.mem.heap.is_object_address(addr) else {
        return "<not-an-object>".to_string();
    };
    let cid = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .try_read()
        .and_then(|cm| cm.get_class(cid).map(|c| c.name.to_string()))
        .unwrap_or_else(|| format!("class_id={}", cid.as_u32()))
}

pub(in crate::jit::conservative_roots) fn report_stale_words_in(
    lo: usize,
    hi: usize,
    cm: Option<&cratonvm_jit::CompiledMethod>,
    rbp: usize,
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
    tag: &str,
) {
    if hi <= lo {
        return;
    }
    let mut addr = (lo + 7) & !7usize;
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = hi.min(addr.saturating_add(MAX_SCAN_BYTES));
    while addr + 8 <= hi {
        // SAFETY: aligned read inside this thread's own live stack interval,
        // bounded by the caller's frame bounds.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(&new) = pointer_map.get(&w) {
            if new == w {
                // The map records a no-op relocation for objects the copy left
                // where they were. Not a stale reference.
                addr += 8;
                continue;
            }
            let n = STALE_AFTER_REMAP_HITS.fetch_add(1, Ordering::Relaxed);
            // Split the hit by whether anything RESUMES from the word. The
            // shared census is what the `System.exit` shutdown trailer prints;
            // the local counter is what this crate's tests read. See
            // `cratonvm_types::stale_remap_census`.
            let resumed_from = cm.is_some_and(|cm| {
                // Cast: a compiled frame is far smaller than i32::MAX.
                is_callee_saved_gpr_image((rbp - addr) as i32, &cm.frame_layout)
            });
            if resumed_from {
                STALE_AFTER_REMAP_RESUMED.fetch_add(1, Ordering::Relaxed);
            }
            cratonvm_types::stale_remap_census::note(resumed_from);
            if n < 4000 {
                let class = class_name_at(shared, new);
                match cm {
                    Some(cm) => {
                        // Cast: a compiled frame is far smaller than i32::MAX.
                        let off = (rbp - addr) as i32;
                        // gcg g3/e: does the active safepoint's map name the
                        // slot? `verifiable=true in_map=false` is a word the
                        // abstract interpreter calls dead (no rewrite owed);
                        // `in_map=true` is a missed rewrite.
                        let in_map = match (active_safepoint_id(rbp, cm), i16::try_from(off)) {
                            (Some(id), Ok(off16)) => cm
                                .oop_maps
                                .iter()
                                .filter(|m| m.bytecode_pc == id)
                                .any(|m| m.frame_slot_offsets.contains(&off16)),
                            _ => false,
                        };
                        eprintln!(
                            "[jit-stale-after-remap] {tag} method={} off={off} region={} \
                             verifiable={} in_map={in_map} resumed_from={} class={class} \
                             value=0x{w:x} moved_to=0x{new:x}",
                            cm.method_label,
                            cm.frame_layout.region_name(off),
                            band_slot_is_verifiable(
                                off,
                                &cm.frame_layout,
                                moving_young_frame_live_hi(rbp, cm),
                            ),
                            is_callee_saved_gpr_image(off, &cm.frame_layout),
                        );
                    }
                    None => {
                        eprintln!(
                            "[jit-stale-after-remap] {tag} depth_below_rbp={} class={class} \
                             value=0x{w:x} moved_to=0x{new:x}",
                            rbp.saturating_sub(addr),
                        );                    }                }            }        }
        addr += 8;
    }
}

/// Walk this thread's live compiled frame bands and report any word naming an
/// address the LAST collection vacated.
///
/// # Why this exists next to `reclaim_guard::audit_thread_frames`
///
/// That auditor tests `thread.frames` -- the INTERPRETER frames -- against the
/// same ledger, and its report ("a LIVE frame slot still names an address the
/// LAST collection moved an object away from") is the exact verdict wanted
/// here. It simply cannot see a compiled frame: a JIT frame's oops live in the
/// machine stack band, not in a `Frame`. So on a workload whose stale holder is
/// compiled, the interpreter auditor is silent and the first symptom is the
/// SIGSEGV.
///
/// The ledger is what makes this sharper than
/// [`report_stale_after_remap`]. That one flags any word equal to a pointer-map
/// KEY, which on a real stack is mostly dead slop that happens to look like a
/// moved address (thousands per run). `gc_quiescence::was_vacated` subtracts the
/// destination set, so a hit is a word naming an address the collector moved an
/// object AWAY from and did not move anything back INTO -- which is precisely
/// the read that faults.
///
/// No-op unless `CRATONVM_DBG_VACATED_FRAMES` is armed.
pub fn audit_jit_frames_for_vacated(
    shared: Option<&crate::vm::SharedVm>,
    tid: u64,
    site: &'static str,
) {
    if !cratonvm_gc::gc_quiescence::vacated_frames_enabled() {
        return;
    }
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else { continue };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                continue;
            }
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                continue;
            };
            // SAFETY: same contract as `report_stale_after_remap` -- the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            // gch h1/jw (r17): the entry band's guard, not 4096.
            let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
            while walk.step() {
                let frame_size = cm.osr_frame_size;
                if frame_size <= 0 {
                    break;
                }
                let frame_size = frame_size as usize;
                const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
                if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
                    break;
                }
                report_vacated_words_in(rbp - frame_size, rbp, cm, rbp, shared, tid, site);
                // SAFETY: `rbp` is a validated frame base in this thread's live
                // JIT stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
        }
    });
}

/// Hits reported by [`audit_jit_frames_for_vacated`] this process.
pub static JIT_VACATED_FRAME_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The subset of [`JIT_VACATED_FRAME_HITS`] in a slot `band_slot_is_verifiable`
/// INSPECTS -- a java local, or an operand-spill slot below the safepoint's
/// live cursor.
///
/// This is the number that separates the two candidate stories. An unverifiable
/// hit is a dead register image or an abandoned outgoing-argument word, which
/// is what every conservative frame scan carries and what
/// `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE` was measured against to no effect. A
/// VERIFIABLE hit is a slot the coverage machinery claims to describe and the
/// remap still did not rewrite -- a live oop of a live compiled frame left
/// naming a vacated address.
pub static JIT_VACATED_FRAME_HITS_VERIFIABLE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Per-region tally, indexed by [`vacated_region_bucket`].
pub static JIT_VACATED_BY_REGION: [std::sync::atomic::AtomicU64; 8] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Bucket names for [`JIT_VACATED_BY_REGION`], in index order.
pub const JIT_VACATED_REGION_NAMES: [&str; 8] = [
    "java-local",
    "operand-spill",
    "licm-or-scalar",
    "callee-saved-gpr-image",
    "safepoint-gpr-spill-image",
    "outgoing-args-or-deopt-regs",
    "reserved-locals-tail",
    "other",
];

pub(in crate::jit::conservative_roots) fn vacated_region_bucket(region: &str) -> usize {
    match region {
        "java-local" => 0,
        "operand-spill" => 1,
        "licm-ref-hoist" | "licm-arith" | "scalar-replaced-field" => 2,
        "callee-saved-gpr-image" | "callee-saved-xmm-image" => 3,
        "safepoint-gpr-spill-image" => 4,
        // `deopt-saved-gpr-image` was carved out of the
        // `outgoing-args-or-deopt-regs` bucket when its bounds became
        // publishable; it shares the bucket so a census taken before and after
        // that split still compares.
        "outgoing-args-or-deopt-regs" | "deopt-saved-gpr-image" | "deopt-saved-xmm-image" => 5,
        "reserved-locals-tail" => 6,
        _ => 7,
    }
}

/// `(total, verifiable, per-region)` for the run's `[jit-vacated-frame]` census.
pub fn jit_vacated_frame_census() -> (u64, u64, [u64; 8]) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut per = [0u64; 8];
    for (i, c) in JIT_VACATED_BY_REGION.iter().enumerate() {
        per[i] = c.load(Relaxed);
    }
    (
        JIT_VACATED_FRAME_HITS.load(Relaxed),
        JIT_VACATED_FRAME_HITS_VERIFIABLE.load(Relaxed),
        per,
    )
}

pub(in crate::jit::conservative_roots) fn report_vacated_words_in(
    lo: usize,
    hi: usize,
    cm: &cratonvm_jit::CompiledMethod,
    rbp: usize,
    shared: Option<&crate::vm::SharedVm>,
    tid: u64,
    site: &'static str,
) {
    if hi <= lo {
        return;
    }
    let mut addr = (lo + 7) & !7usize;
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = hi.min(addr.saturating_add(MAX_SCAN_BYTES));
    while addr + 8 <= hi {
        // SAFETY: aligned read inside this thread's own live stack interval,
        // bounded by the caller's frame bounds.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(moved_to) = cratonvm_gc::gc_quiescence::was_vacated(w) {
            let n = JIT_VACATED_FRAME_HITS.fetch_add(1, Ordering::Relaxed);
            // Cast: a compiled frame is far smaller than i32::MAX.
            let off_t = (rbp - addr) as i32;
            let region_t = cm.frame_layout.region_name(off_t);
            let verifiable_t = band_slot_is_verifiable(
                off_t,
                &cm.frame_layout,
                moving_young_frame_live_hi(rbp, cm),
            );
            if verifiable_t {
                JIT_VACATED_FRAME_HITS_VERIFIABLE.fetch_add(1, Ordering::Relaxed);
            }
            JIT_VACATED_BY_REGION[vacated_region_bucket(region_t)].fetch_add(1, Ordering::Relaxed);
            // Report the VERIFIABLE ones without a budget: they are the finding,
            // and on a healthy run there are none. The unverifiable tail is dead
            // slop every conservative scan carries, so it keeps a cap.
            if verifiable_t || n < 200 {
                // Cast: a compiled frame is far smaller than i32::MAX.
                let off = (rbp - addr) as i32;
                eprintln!(
                    "[jit-vacated-frame] site={site} tid={tid} method={} off={off} region={} \
                     verifiable={} resumed_from={} value=0x{w:x} moved_to=0x{moved_to:x} \
                     class_at_target={}",
                    cm.method_label,
                    cm.frame_layout.region_name(off),
                    band_slot_is_verifiable(
                        off,
                        &cm.frame_layout,
                        moving_young_frame_live_hi(rbp, cm),
                    ),
                    is_callee_saved_gpr_image(off, &cm.frame_layout),
                    class_name_at(shared, moved_to),
                );
            }
        }
        addr += 8;
    }
}

/// Walk every live compiled frame band after a moving collection has remapped
/// the oop-map slots and report words that still name a moved-from address.
///
/// See [`stale_after_remap_enabled`]. No-op unless the flag is set.
pub fn report_stale_after_remap(
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
) {
    if pointer_map.is_empty() || !stale_after_remap_enabled() {
        return;
    }
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else { continue };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                continue;
            }
            if stale_below_rbp_enabled() {
                report_stale_words_in(
                    scanner_sp,
                    rbp,
                    None,
                    rbp,
                    pointer_map,
                    shared,
                    "below-innermost-rbp",
                );
            }
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                continue;
            };
            // SAFETY: same contract as `scan_precise_frames_and_bands` -- the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            // gch h1/jw (r17): the entry band's guard, not 4096.
            let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
            while walk.step() {
                let frame_size = cm.osr_frame_size;
                if frame_size <= 0 {
                    break;
                }
                let frame_size = frame_size as usize;
                const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
                if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
                    break;
                }
                report_stale_words_in(
                    rbp - frame_size,
                    rbp,
                    Some(cm),
                    rbp,
                    pointer_map,
                    shared,
                    "frame-band",
                );
                // SAFETY: `rbp` is a validated frame base in this thread's live
                // JIT stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Register-image remap: the other half of the frame-word partition
// ---------------------------------------------------------------------------
//
// `band_slot_is_verifiable` splits a compiled frame's band in two. Words it
// inspects are VERIFIED: an unpublished movable oop in one of them makes
// `moving_young_unpublished_frame_oop_present` report
// `UNPUBLISHED_FRAME_OOP`, and the cycle diverts to the non-moving sweep.
// Words it skips were, until now, neither verified NOR rewritten -- the
// prologue's callee-saved GPR/XMM save areas, the per-safepoint blind GPR
// spill, the outgoing-argument / deopt-register reserve, and operand-spill
// slots above the safepoint's live cursor.
//
// The justification for skipping them is that they are register IMAGES and
// dead argument words: "not storage this frame resumes from". That is true of
// THIS frame and false of its caller. A compiled prologue saves the CALLER's
// callee-saved GPRs into its own frame, and the epilogue pops them straight
// back into the caller's registers -- so the caller resumes from exactly the
// words the verifier declined to look at. `scan_precise_frames_and_bands` READS
// them, which is what keeps the referenced object alive across the pause; the
// moving young collection then copies it, `remap_active_jit_frames` rewrites
// only the slots an oop map names, and the saved word is left holding a
// from-space address.
//
// This pass closes the half of that partition that something RESUMES FROM, and
// it is deliberately narrower than "every word the verifier refuses". The
// unverifiable regions are not equivalent, and lumping them together is what
// made the repair look unsafe enough to ship off:
//
//   * `callee-saved-gpr-image` -- the prologue's save area for the CALLER's
//     callee-saved GPRs, which the epilogue pops straight back into the
//     caller's registers. This one IS resumed from. It was the ONLY region this
//     pass rewrote until 2026-10-01; `register_image_remap_admits` now also
//     admits the deopt `SavedRegisters` GPR image (resumed from by
//     `x64_deopt_entry`) and the blind GPR spill image below (dc2f22aee0).
//     (gci i9/ng: this line still said "the ONLY region" after that commit.)
//   * `callee-saved-xmm-image` -- resumed from as well, but an XMM never holds
//     an object reference in this VM's calling convention, so a hit there is a
//     false positive by construction and rewriting it could only corrupt a
//     double.
//   * `safepoint-gpr-spill-image` -- the mutator never reloads these words,
//     but a later conservative GC scan DOES read them. Rewrite moved bases
//     here too: retaining the original image either pins its region forever
//     or lets a subsequent scan follow a recycled address. This is only the
//     blind GPR image, not either XMM save area. A non-reference bit pattern
//     equal to a map key can change here without changing a Java value:
//     generated code never reads this write-only copy back into a register.
//     Rewritten is not MOVABLE: since the image is never reloaded, the live
//     register it copies is refreshed only by the shadow reload, so a
//     spill-image word publishes movable only when shadow emission is on
//     (`register_image_word_publishes_movable`, gci i8/nt); otherwise it pins.
//     On G1, gci i19/nt: and only when the frame's live shadow push holds that
//     very value (`SpillImagePushProof`); a register the push did not publish
//     -- the frame's own unpublished oop, or a CALLER's value (a Rust frame's
//     callee-saved register) this frame never saved -- is refreshed by nothing.
//   * `outgoing-args-or-deopt-regs`, and operand-spill slots above the
//     safepoint's live cursor -- dead by definition. The cursor reclaims by
//     moving, so those slots hold whatever the deepest earlier operand stack
//     left behind.
//
// Restricting to the GPR image is also what makes the write defensible. The
// dead regions are where a non-pointer that merely LOOKS like an object base is
// plausible -- they are full of abandoned values nobody screens. A caller's
// live callee-saved GPR is not: for a hit there to be a false positive, the
// caller would have to be holding a non-reference whose value is exactly a
// young object's base address, which `heap.is_object_address` validated against
// the arena bounds and the object-start bitmap. Loop counters, sizes and PCs do
// not reach those addresses, and Rust-side pointers are in different mappings.
//
// The compiled callers do not actually need this: a compiled frame reloads its
// live oops from the shadow stack after every safepoint, so its registers are
// refreshed whatever the image held. What needs it is the OUTERMOST compiled
// frame, whose caller is the VM's own Rust code at the interpreter->JIT
// boundary -- which has no reload and resumes from exactly those popped
// registers. That is the frame every observation on this defect has named.
//
// SHIPS ON, with `CRATONVM_REGISTER_IMAGE_REMAP=0` as the kill switch and
// `CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` as the instrument that says whether it
// has anything to do on a given workload. See
// `moving-young-left-a-callee-saved-register-image-unrewritten-FIXED-20260823`
// for the measurement.
//
// The other half of the repair is a PIN rather than a write:
// `publish_unrewritable_band_roots` publishes every object an unverifiable word
// holds to `gc_quiescence::add_unrewritable_jit_root`, which vetoes the movable
// claim at the young sweep's pin decision. That covers the non-moving arm at no
// risk at all; a Cheney moving young collection has no pin, which is why this
// write exists for it.
pub(in crate::jit::conservative_roots) fn register_image_remap_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    // gci i3/fl: the shared default-ON value rule (`0` / `false` / `off` /
    // `no`, trimmed, any case), not `0` alone -- `=off` used to leave it on.
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

pub(in crate::jit::conservative_roots) static REGISTER_IMAGE_REMAP_WORDS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Total words rewritten by [`remap_register_image_words`] this process.
pub fn register_image_remap_words() -> usize {
    REGISTER_IMAGE_REMAP_WORDS.load(Ordering::Relaxed)
}

/// Is `off` inside the prologue's save area for the CALLER's callee-saved GPRs?
///
/// The one unverifiable region a frame's CALLER resumes from — the epilogue
/// pops these words straight back into its registers. `band_slot_is_verifiable`
/// refuses the whole `off >= callee_saved_lo` tail (GPR image, XMM image,
/// safepoint spill, outgoing-args/deopt reserve); this narrows it back to the
/// GPR image alone. It is one arm of [`register_image_remap_admits`], which
/// also admits the blind GPR spill image and the deopt `SavedRegisters` GPR
/// image (both since 2026-10-01); the XMM images and the dead reserve stay
/// unrewritten (gci i9/ng: this used to say "the other three are deliberately
/// left stale", which stopped being true for the spill image).
///
/// The two XMM images are carved out even when the published GPR range covers
/// them: the IR tier publishes `callee_saved_lo..callee_saved_hi` as the whole
/// frame tail, which also holds its prologue's XMM6/XMM7 save area and the
/// deopt `SavedRegisters` XMM half. A word there is the caller's FP bits (a
/// `MOVSS` float images as a value below 4 GiB), so rewriting one that happens
/// to equal a moved object's old address would silently change the caller's
/// `float`. Nothing resumes a reference from either area.
#[inline]
pub(in crate::jit::conservative_roots) fn is_callee_saved_gpr_image(off: i32, layout: &cratonvm_jit::FrameLayout) -> bool {
    layout.callee_saved_hi > layout.callee_saved_lo
        && off >= layout.callee_saved_lo
        && off < layout.callee_saved_hi
        && !(layout.xmm_saved_hi > layout.xmm_saved_lo
            && off >= layout.xmm_saved_lo
            && off < layout.xmm_saved_hi)
        && !layout.is_deopt_saved_xmm_image(off)
}

/// Does the register-image remap rewrite the word at `off`?
///
/// Default: the callee-saved GPR image alone, which is what
/// [`is_callee_saved_gpr_image`] answers and what the module comment above
/// argues for at length.
///
/// `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` widens it to the whole unverifiable
/// tail (`off >= callee_saved_lo`). **A DIAGNOSTIC, and deliberately not a
/// proposed default.** It exists because that module comment partitions the
/// tail into one region something resumes from and three it calls dead or
/// write-only, and that partition is an ARGUMENT rather than a measurement --
/// while the instrument that looks like it could check it cannot: the
/// stale-word census reports `resumed_from` as `is_callee_saved_gpr_image` and
/// nothing else, so it reads `false` for all three of the regions in question
/// by construction, and its zero is not an all-clear for them.
///
/// The experiment this enables is a single A/B on the reproducer in
/// `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
/// §10.4 -- `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` on
/// `io.netty.handler.ipfilter.UniqueIpFilterTest`, which SIGSEGVs 3 runs in 13
/// with compiled code reading a decommitted span. If widening the write removes
/// the crash, one of the three "dead" regions is read after all and the
/// partition is wrong. If the crash survives, they are exonerated on this
/// workload and the stale reference lives somewhere neither pass touches.
///
/// Stated cost of the widened arm, so nobody ships it by accident: it also
/// rewrites the callee-saved XMM image, where an object-shaped bit pattern
/// would be a double rather than a reference. `heap.is_object_address` vets
/// every candidate against the arena bounds and the object-start bitmap, so a
/// false positive needs a double whose bits are exactly a live young object's
/// base -- unlikely, not impossible, and a reason this is off by default.
///
/// # THE ANSWER, measured 2026-09-06 -- widening changes NOTHING
///
/// | arm | crashes / runs | rate |
/// |---|---|---|
/// | default (callee-saved GPR image only) | **11 / 81** | 13.6 % |
/// | `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` | **2 / 32** | 6.3 % |
///
/// Fisher p ~ 0.37 -- not a difference. Two interleaved matched batches on one
/// binary (narrow 0/14 vs wide 2/14, then narrow 2/18 vs wide 0/18) land on
/// either side of the pooled baseline, which is what a null result looks like
/// when each batch is read on its own; the first batch alone reads as "widening
/// makes it worse" and that was wrong.
///
/// So `operand-spill`, `safepoint-gpr-spill-image` and
/// `outgoing-args-or-deopt-regs` are **not** where this defect's repair goes:
/// rewriting every one of them does not move the crash rate. That is a negative
/// result worth keeping, because the blind GPR spill area is one of the three
/// storage classes `moving-young-corruption-rootcause.md` nominates and this
/// eliminates it on this workload.
///
/// The flag stays for the next person who wants to re-ask the question on a
/// different workload -- it is a few lines and it is off by default -- but it
/// is NOT a candidate fix and should not be flipped on.
///
/// (gci i19/nt, doc drift: the "Default" sentence at the top and the 2026-09-06
/// answer predate dc2f22aee0 (2026-10-01), which admits the blind GPR spill
/// image by default -- the first arm below -- and the deopt GPR image. What the
/// rewrite of the spill image does NOT do is refresh the register it copies;
/// whether such a word may be MOVABLE is decided separately, in
/// `publish_validated_band_word` (`register_image_word_publishes_movable`,
/// `SpillImagePushProof`).)
#[inline]
pub(in crate::jit::conservative_roots) fn register_image_remap_admits(off: i32, layout: &cratonvm_jit::FrameLayout) -> bool {
    if layout.spill_image_index(off).is_some() {
        return true;
    }
    if is_callee_saved_gpr_image(off, layout) {
        return true;
    }
    // THE FRAME-DEOPT `SavedRegisters` GPR IMAGE, on the same argument as the
    // callee-saved image beside it: something RESUMES from these words.
    //
    // The deopt stub spills all sixteen GPRs into this region and hands its
    // address to `x64_deopt_entry`, which reconstructs the interpreter frame by
    // reading `gpr[r]` back (`deopt::resolve_value`, `FrameValue::Register`).
    // A reference that moved while the frame sat in that region and was not
    // rewritten is therefore reconstructed at its OLD address -- so admitting
    // it here is a correctness completion, not only a pin optimisation.
    //
    // And it is what takes the pin off. `publish_unrewritable_band_roots` asks
    // this predicate to decide movable-versus-unrewritable, so before this arm
    // existed every word of the block vetoed its object and G1 held the whole
    // MEGABYTE-granular region that object sat in. Measured on
    // `probes/TvmProbe.java` (the standalone port of H2's `TestValueMemory`)
    // under `-XX:+UseG1GC --Xmx 2g`: four of forty rows over the
    // `used > memory * 3` threshold, every one of them pinning an Eden region
    // for 200 bytes of objects named only from this block.
    //
    // The XMM half of the region is NOT admitted -- `deopt_gpr_lo` says why.
    if layout.is_deopt_saved_gpr_image(off) {
        return true;
    }
    remap_all_unverifiable()
        && layout.callee_saved_hi > layout.callee_saved_lo
        && off >= layout.callee_saved_lo
}

/// gci i8/nt: may a band word that [`register_image_remap_admits`] accepts be
/// published MOVABLE, i.e. let its object move without a pin?
///
/// The remap rewrites every admitted word, but a rewrite only protects the
/// mutator when the mutator READS that word back:
///
/// * the callee-saved GPR image is popped into the caller's registers by the
///   epilogue, and the deopt `SavedRegisters` GPR image is read by
///   `x64_deopt_entry` -- rewritten and resumed from, so movable;
/// * the per-safepoint blind GPR spill (`safepoint-gpr-spill-image`,
///   `FrameLayout::spill_image_index`) is WRITE-ONLY: the compiled code keeps
///   using the live register after the call (`emit_pre_safepoint_spill`: "no
///   post-call reload is needed"). A callee-saved register survives the
///   runtime call untouched, so the register still names the OLD address
///   after G1 evacuated the object. The image was admitted to the remap on
///   2026-10-01 (dc2f22aee0) so later scans stop following a recycled
///   address -- right for the rewrite -- but that also made it movable here,
///   which is only sound when something else refreshes the register: the
///   shadow reload of a `ShadowHome::Reg` home (`emit_shadow_reload`).
///
/// So a spill-image word is movable only when `spill_image_register_is_reloaded`
/// (shadow emission on, and the opt-in `CRATONVM_GC_SPILL_IMAGE_PINS` unset);
/// otherwise it falls through to the unrewritable veto and pins its object,
/// as it did before 2026-10-01. Generational with shadow emission off never
/// relocates under a live compiled frame (its coverage proof fails), so the
/// extra veto changes nothing there; G1 is the collector that relocated.
/// Pure, so both arms are testable.
///
/// gci i19/nt: the caller's predicate also asks the frame's own push
/// ([`SpillImagePushProof`], `CRATONVM_GC_SPILL_IMAGE_PUSH_PROOF`, default ON):
/// shadow emission on says the reload EXISTS, not that it refreshes this
/// register.
pub(in crate::jit::conservative_roots) fn register_image_word_publishes_movable(
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    spill_image_register_is_reloaded: impl FnOnce() -> bool,
) -> bool {
    if layout.spill_image_index(off).is_none() {
        return true;
    }
    spill_image_register_is_reloaded()
}

/// gci i8/nt: is a register whose blind-spill image the remap rewrote also
/// refreshed after the safepoint? Only through the shadow reload, so: shadow
/// emission on ([`shadow_stack_enabled`], default on; off under
/// `CRATONVM_JIT_MY_SHADOW_EMISSION=0`), and the default on
/// `CRATONVM_GC_SPILL_IMAGE_PINS` (default OFF) unset. That switch pins every
/// spill-image word even with shadow emission on: a register the shadow push
/// did not publish (the oop tracker missed it, which is what the blind spill
/// exists for) is refreshed by nothing. It is the bisection arm for the netty
/// page on the default path. Read only for a validated spill-image word.
pub(in crate::jit::conservative_roots) fn spill_image_register_is_reloaded() -> bool {
    shadow_stack_enabled()
        && !true}



/// gci i19/nt: does the frame's CURRENT shadow push hold the value a
/// blind-spill image word holds, so that the shadow reload after the call
/// refreshes the register that word copies?
///
/// [`spill_image_register_is_reloaded`] asks only whether shadow emission is
/// on. That is a property of the build, not of the frame, and the reload
/// writes back only the homes the paired push published
/// (`jit/src/x64/safepoint.rs` `emit_shadow_push` / `emit_shadow_reload`). A
/// callee-saved register holding a live reference the push did not publish is
/// refreshed by nothing, so a movable spill-image word lets G1 evacuate the
/// object while the register keeps the old address -- the i8/nt mechanism on
/// the DEFAULT path (shadow emission on). The cases, all read off the frame:
///
/// * the active safepoint's map does not claim
///   `moving_young_coverage_complete` -- marks inexact, an oop in a scratch
///   register, a refused local dataflow, an unmapped splice, a push that was
///   not emitted (an OSR artifact without a shadow layout, a
///   `..._without_shadow` safepoint). The x64 push publishes every live oop
///   register only when that claim holds (`collect_live_oop_homes`);
/// * no readable safepoint id, or no map for it (fail closed);
/// * the push bailed (the overflow tag, bit 0 of the saved base) or has
///   already been popped (`savebase + 8 * n > top`): its entries are not on
///   the stack, so the reload restores nothing;
/// * the push is live but does not hold this value: the word copies a
///   register this safepoint did not publish (a leftover the map's register
///   mask did not exclude, or a register whose live copy is elsewhere).
///
/// The value test reads only this frame's own push, the `n` words from the
/// saved base, with `n` the SMALLEST `shadow_pushed` of the id's maps (a splice can
/// put two maps on one id; the smaller count is a prefix of either push, so a
/// match is always this frame's entry, never a deeper frame's). An indirect
/// (frame-slot) entry has bit 0 set and never equals an object start, so a
/// match is a register home (or, with `CRATONVM_JIT_MY_SHADOW_INDIRECT=0`, a
/// frame-home copy of the same object -- the reload then refreshes that slot,
/// and the complete-coverage claim says no other live register holds it
/// unpublished).
///
/// The shadow window is resolved through the frame's cached thread
/// (`shadow_window_from_frame_why`, no fallback to the scanning thread's own
/// window): a scan of another thread's frames by a thread that has its own JIT
/// thread installed resolves nothing and the word pins (fail closed).
///
/// Residual, not covered: a frame stopped in a call that writes no safepoint
/// id while an EARLIER safepoint's push area has been re-occupied by a deeper
/// frame's push holding the same object. The match then names the deeper
/// frame's entry. `CRATONVM_GC_SPILL_IMAGE_PINS=1` pins every spill-image word
/// and is the arm that bounds it.
///
/// G1 only. The Generational collector moves under live compiled frames only
/// after its whole-cycle coverage proof and band verifier pass, and turns an
/// unrewritable veto into a conservative divert (`gen_heap.rs`
/// `ConservativeDivertSample`), so a new veto there would change its cycle
/// shape for a hole it does not have; ZGC is out of this round. On both the
/// proof licenses every word, so their behaviour is byte for byte i18's.
pub(in crate::jit::conservative_roots) struct SpillImagePushProof<'a> {
    pub(in crate::jit::conservative_roots) rbp: usize,
    pub(in crate::jit::conservative_roots) cm: &'a cratonvm_jit::CompiledMethod,
    /// The scan's heap is G1 (`VmHeap::is_g1`); otherwise the proof is inert.
    pub(in crate::jit::conservative_roots) g1: bool,
    /// `(savebase, n)` of the frame's live push, or `None` (no proof);
    /// resolved on the first question.
    pub(in crate::jit::conservative_roots) entries: std::cell::OnceCell<Option<(usize, usize)>>,
}

impl<'a> SpillImagePushProof<'a> {
    pub(in crate::jit::conservative_roots) fn new(rbp: usize, cm: &'a cratonvm_jit::CompiledMethod, g1: bool) -> Self {
        SpillImagePushProof {
            rbp,
            cm,
            g1,
            entries: std::cell::OnceCell::new(),
        }
    }

    /// May a spill-image word holding `qword` be published movable, as far as
    /// the frame's push is concerned? Always on a non-G1 heap and with
    /// `CRATONVM_GC_SPILL_IMAGE_PUSH_PROOF=0`; otherwise [`Self::publishes`].
    pub(in crate::jit::conservative_roots) fn licenses(&self, qword: usize) -> bool {
        if !self.g1 {
            return true;
        }
        self.publishes(qword)
    }

    /// Is `qword` one of the values the frame's live push published?
    pub(in crate::jit::conservative_roots) fn publishes(&self, qword: usize) -> bool {
        let entries = *self.entries.get_or_init(|| frame_live_push_entries(self.rbp, self.cm));
        let Some((lo, n)) = entries else {
            return false;
        };
        (0..n).any(|i| {
            // SAFETY: `frame_live_push_entries` proved `[lo, lo + 8 * n)` lies
            // inside the thread's validated shadow window `[base, top)`, an
            // aligned range of the shadow-stack buffer.
            let entry = unsafe { ((lo + 8 * i) as *const usize).read() };
            entry == qword
        })
    }
}

/// The `(savebase, n)` of the frame's live shadow push, or `None` when there
/// is no proof (see [`SpillImagePushProof`] for every refusal).
pub(in crate::jit::conservative_roots) fn frame_live_push_entries(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
) -> Option<(usize, usize)> {
    // The IR tier's frame block publishes once per activation by address; it
    // has no blind spill image, and nothing here describes it.
    if cm.shadow_frame_block {
        return None;
    }
    let sp_id = active_safepoint_id(rbp, cm)?;
    let n = complete_push_count(cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id))?;
    if n == 0 {
        return None;
    }
    let off = cm.shadow_savebase_slot_off;
    if off <= 0 || rbp < off as usize {
        return None;
    }
    let slot = rbp - off as usize;
    if slot & 0x7 != 0 {
        return None;
    }
    // SAFETY: the aligned saved-base slot of a live compiled frame
    // (`CompiledMethod::shadow_savebase_slot_off`), inside the frame the caller
    // is scanning.
    let savebase = unsafe { (slot as *const usize).read() };
    let (base, top) = shadow_window_from_frame_why(rbp, cm).ok()?;
    push_entries_in_window(savebase, n, base, top)
}

/// The push size a frame's active safepoint id vouches for: the smallest
/// `shadow_pushed` over the id's maps, or `None` when there is no map or any
/// map does not claim `moving_young_coverage_complete`. Pure.
pub(in crate::jit::conservative_roots) fn complete_push_count<'m>(
    maps: impl Iterator<Item = &'m cratonvm_jit::OopMapEntry>,
) -> Option<usize> {
    let mut n: Option<usize> = None;
    for m in maps {
        if !m.moving_young_coverage_complete {
            return None;
        }
        let k = usize::from(m.shadow_pushed);
        n = Some(n.map_or(k, |c| c.min(k)));
    }
    n
}

/// `[savebase, savebase + 8 * n)` as `(savebase, n)` when it is a live push
/// inside the shadow window `[base, top)`: not tagged by an overflow bail (bit
/// 0), aligned, and not popped (a popped push reset `top` to its base). Pure.
pub(in crate::jit::conservative_roots) fn push_entries_in_window(
    savebase: usize,
    n: usize,
    base: usize,
    top: usize,
) -> Option<(usize, usize)> {
    if savebase & 0x7 != 0 || savebase < base {
        return None;
    }
    let end = savebase.checked_add(n.checked_mul(8)?)?;
    if end > top {
        return None;
    }
    Some((savebase, n))
}

/// `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` -- see
/// [`register_image_remap_admits`]. Default off.
pub(in crate::jit::conservative_roots) fn remap_all_unverifiable() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        false    })}

/// Rewrite the moved references held in one frame's callee-saved GPR image.
pub(in crate::jit::conservative_roots) fn remap_one_frame_register_images(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
    dbg: bool,
    band: cratonvm_gc::shadow_stack::SlotBand,
) -> usize {
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return 0;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return 0;
    }
    // No `live_hi` here: it bounds the operand stack, which this pass does
    // not touch. Register-image bounds come from the published frame layout.
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    let mut rewritten = 0usize;
    while addr + 8 <= rbp {
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if !register_image_remap_admits(off, &cm.frame_layout) {
            // Everything else is either VERIFIED storage -- where an
            // unpublished movable oop has already forced the non-moving sweep
            // and a published one was rewritten by `remap_one_jit_frame` -- or
            // an unverifiable region nothing resumes from. The module comment
            // above enumerates the four and says why each is excluded; this is
            // the line that keeps the write off the dead ones.
            addr += 8;
            continue;
        }
        // SAFETY: aligned read inside this thread's own live compiled frame,
        // bounded by the frame size recorded at compile time.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(&new) = pointer_map.get(&w) {
            // gen r5w2/roots6: stored only inside the walk's own live stack
            // band (`jit_remap_store_band`).
            if new != w
                && store_jit_remap_word(
                    cratonvm_gc::root_write_audit::RootWriteKind::JitRegisterImage,
                    band,
                    addr,
                    w,
                    new,
                )
            {
                rewritten += 1;
                if dbg {
                    eprintln!(
                        "[jit-register-image-remap] method={} off={off} region={} class={} \
                         0x{w:x}->0x{new:x}",
                        cm.method_label,
                        cm.frame_layout.region_name(off),
                        class_name_at(shared, new),
                    );
                }
            }
        }
        addr += 8;
    }
    rewritten
}

/// Rewrite moved references held in the register-image / outgoing-argument
/// words of every live compiled frame on this thread.
///
/// Companion to [`remap_active_jit_frames`], which covers the oop-map slots.
pub fn remap_register_image_words(
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
) {
    if pointer_map.is_empty() || !register_image_remap_enabled() {
        return;
    }
    let dbg = stale_after_remap_enabled();
    let scanner_sp = current_stack_pointer();
    // gen r5w2/roots6: every store lands in `[scanner_sp, stack top)`.
    let store_band = jit_remap_store_band(scanner_sp);
    let mut total = 0usize;
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else { continue };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                continue;
            }
            // Same resolution rule as `scan_precise_frames_and_bands` and
            // `remap_active_jit_frames`: when nothing describes the frame at
            // `exact_rbp`, its layout would be read out of the wrong method,
            // so leave it alone. The coverage refresh reports
            // `FOREIGN_INNERMOST_RBP` for it, which already forces the
            // non-moving sweep, so there is no move to repair.
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                continue;
            };
            // SAFETY: same contract as `scan_precise_frames_and_bands` -- the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live, and a resolved callee is kept alive by the
            // live frame whose return address resolved it.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            // gch h1/jw (r17): every activation of the entry, not the first
            // 4096 (the same guard as Stage 5's walk).
            let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
            while walk.step() {
                total +=
                    remap_one_frame_register_images(rbp, cm, pointer_map, shared, dbg, store_band);
                // SAFETY: `rbp` is a validated frame base in this thread's live
                // JIT stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
        }
    });
    if total > 0 {
        REGISTER_IMAGE_REMAP_WORDS.fetch_add(total, Ordering::Relaxed);
    }
}

