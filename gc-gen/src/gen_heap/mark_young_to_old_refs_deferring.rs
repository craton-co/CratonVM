#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// [`Self::mark_young_to_old_refs`], plus (gcj j3/rf3, ec47's legacy seed)
    /// `defer = Some((members, out))`: a PARSED young object whose address is
    /// in `members` (sorted; [`OldGenFinalizerPass::young_resurrected`]) has
    /// its old-gen reference slot values appended to `out` instead of marked.
    /// The caller hands `out` to the resurrection round
    /// (`OldGenFinalizerPass::deferred_seeds`), which marks every one of them
    /// through the same screen (`mark_and_push_old_gen_in`), so nothing this
    /// walk would have marked is left unmarked; it is only marked later, as
    /// finalizer-reachable. A member's defining loader is still seeded here
    /// (a class-level edge), and an unparseable stretch keeps its
    /// conservative marking whatever it holds.
    #[allow(clippy::too_many_arguments)]
    pub fn mark_young_to_old_refs_deferring<G: OldMarkGrid + ?Sized>(
        young_from: &Arena,
        old_gen: &OldGen,
        walked: &G,
        young_skips: &[(usize, usize)],
        worklist: &mut Vec<*mut u8>,
        mut anomaly_pins: Option<&mut Vec<usize>>,
        mut defer: Option<(&[usize], &mut Vec<usize>)>,
    ) {
        YOUNG_WALK_ENTRIES[2].fetch_add(1, Ordering::Relaxed);
        // Skip the zeroed holes the non-moving sweep leaves in from-space.
        // Without this, this linear walk strides into a reclaimed hole, decodes
        // its zeroed bytes as a `num_slots=0` (40-byte) object, and desyncs off
        // the true object grid — landing on a live object's interior field cell
        // (the bintrees18 "inconsistent header class_id=4" false positive) and
        // `break`ing early, which abandons the rest of the young→old mark scan
        // (a real correctness bug: old objects referenced past the hole go
        // unmarked). The non-moving sweep itself skips holes the same way; every
        // linear from-space walker must too. (Cheney is immune: it resets
        // from-space each cycle, so holes never accumulate there.)
        //
        // H2-CID0 (2026-08-01): the skip list must ALSO carry the reserved
        // TLAB tails of forcibly-stopped in-JIT peers (BUG-03), which is why it
        // is passed in rather than read from `young_from` here.
        //
        // A reserved tail is the unallocated remainder of a TLAB whose owner
        // was frozen mid-JIT. It is not on the free list and it holds no
        // objects -- `sweep_young_non_moving` merges these regions into its own
        // skip list for exactly that reason ("an un-retired tail is neither
        // walked as objects nor reclaimed"). These two walks did not, so they
        // parsed the tail's bytes as headers. When those bytes decode plausibly
        // the walk strides off the object grid and keeps going: the REAL young
        // objects after the tail can be read at wrong offsets.
        //
        // MEASURED, so the next reader does not over-claim this: a negative
        // control (2026-08-01) shows the walk survives an unparseable reserved
        // tail WITHOUT this merge, because the desync trips the anomaly path
        // and the conservative word scan still finds every old-gen base in the
        // stretch. So this is a CONSISTENCY fix -- every other linear
        // from-space walker skips these regions, and parsing a tail that holds
        // no objects is pointless work resting on that fallback -- not a
        // demonstrated bug fix.
        //
        // Why the fallback matters so much here: `OldGen::close_live_set`
        // (`5750caf5f`) closes the live set over references from marked
        // OLD-GEN objects, so it cannot rescue a block whose only referrer is
        // YOUNG -- the shape H2's `CacheLongKeyLIRS` has, holding its `Page` /
        // `ByteBuffer` / `Chunk` payloads from entries that are still young.
        // Nor would `CRATONVM_DBG_SWEEP_EDGES` see it: that assertion looks
        // for references INTO young, never young->old.
        let free_blocks: Vec<(usize, usize)> = if young_skips.is_empty() {
            young_from.free_blocks_sorted()
        } else {
            let mut v = young_from.free_blocks_sorted();
            v.extend_from_slice(young_skips);
            v.sort_by_key(|&(off, _)| off);
            v
        };
        let mut free_iter = free_blocks.iter().peekable();
        let mut cursor: usize = 0;
        let base = young_from.base_ptr() as usize;
        let used = young_from.used();
        // gcd d1/c (2026-09-27) — INSTANCE → LOADER for YOUNG instances.
        //
        // The old generation's BFS follows `loader_pin` (a live instance keeps
        // its class's defining loader) only from the OLD objects it marks, and
        // this walk seeded only the young objects' FIELDS. So an OLD loader
        // whose live instances are all YOUNG was left unmarked by every
        // stop-the-world old collection unless something else named it: the
        // collection then judged it dead, the unload transaction removed its
        // classes under their live young instances, and the old sweep freed
        // the loader (and its class mirror, reached only through its
        // `mirror_pin` row). Until now a YOUNG mirror of such a class usually
        // hid it: rooted, and scanned here, its `classLoader` field seeded the
        // loader. Once the mirror is old -- promoted under
        // `CRATONVM_GEN_YOUNG_MIRROR_DEFER`, or by any cycle that tenures it --
        // and deferred to its loader by a licensed root scan, nothing did
        // (`GenR5W5ConcUnloadProbe`: `NullPointerException` on a live static
        // under that flag, 3/3 with the JIT; the concurrent cycle already
        // covers the same edge, `gen_conc_young_instance_loaders`). Every
        // young object is retained by an old collection, so its loader is
        // seeded: the conservative direction, one hash probe per parsed young
        // object, and nothing at all while no user loader has defined a class.
        // See `gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929.md`.
        let young_loaders = if cratonvm_types::loader_pin::loader_pinning_enabled() {
            cratonvm_types::loader_pin::snapshot_where(|a| {
                old_gen.contains(a as *const u8) || (a >= base && a < base + used)
            })
        } else {
            None
        };
        // DoHead walk-desync hardening (2026-07-02): a stretch this walk
        // cannot parse (zero span / implausible header) must NOT be silently
        // dropped — an old object referenced only from that stretch would go
        // unmarked and be freed by the compaction (use-after-free). Fall back
        // to a conservative word scan of the stretch: mark every aligned word
        // that is EXACTLY an old-gen object base. Over-marking is always
        // safe; base validation is mandatory (`OldGen::contains` is a raw
        // range check, so an interior/colliding word would otherwise get a
        // mark-bit write into a live object's payload and feed a garbage
        // "header" into the BFS).
        //
        // GCAUD-8: the sorted base list used to be built lazily here, so only
        // sweeps that actually hit an unparseable stretch paid for it.
        // `old_gen_gc` now derives the same list once, before the mark, because
        // every precise push site needs it as a fail-closed admission oracle —
        // so it is passed in, and this walk shares it instead of re-walking.
        // gen r5w1/young5: the grid rules are `probe_grid_at`'s. This walk's
        // policy is unchanged: a malformed GAP filler is parsed as a header
        // (`bad_gap_filler_is_anomaly: false`), and a header whose extent
        // crosses a free block is still parsed as an object (the hole-crossing
        // verdict is deliberately ignored here; see the drift page's wave-7
        // section for why this walk keeps it).
        //
        // Bug-D fix (2026-06-12): a GAP-filler sentinel (a sub-`HEADER_SIZE`
        // TLAB tail) is dead filler with no refs to scan, strided before any
        // size read. Unlisted zeroed span (see the sweep walk): zero words
        // carry no old-gen refs, so re-anchor at the next free block; the
        // remainder up to the anchor is conservatively word-scanned.
        //
        // A run of EMPTY objects is not a desync, and here the second half of
        // the argument is what carries it: an empty object has NO FIELDS, so a
        // run of them contains no young->old reference for this pass to find.
        // Stepping over it cannot miss a ref. That matters because this walk
        // has no young live set to hand the predicate — a major cycle retains
        // young conservatively and deliberately walks live and dead objects
        // alike, so there is nothing to pass but `&[]` and the
        // no-marked-base-inside condition is vacuous. The alignment and
        // plausible-next-header conditions still apply, and they are the two
        // that establish that `resume` is on-grid. The anomaly arm below is not
        // free: it re-anchors at the next free block and falls back to a
        // CONSERVATIVE scan of the skipped stretch, marking every word that
        // looks like an old-gen base. Measured on `probes/GcWalkProbe.java`, it
        // fired on 4 of 4 entries — i.e. every major cycle seeded young->old
        // from a conservative scan rather than a parse.
        let rules = GridRules {
            bad_gap_filler_is_anomaly: false,
            ..GridRules::unmarked(base, used)
        };
        let mut no_phantom_probe = 0usize;
        while cursor < used {
            if skip_free_blocks(&mut cursor, &mut free_iter).0 {
                continue;
            }
            // SAFETY: `cursor` is within `young_from.used()`; pointer arithmetic stays in the arena.
            let obj_ptr = unsafe { young_from.base_ptr().add(cursor) as *mut u8 };
            let next_free = free_iter.peek().map(|&&(off, _)| off);
            // `cursor < used` inside mapped from-space (`probe_grid_at`'s contract).
            let verdict = probe_grid_at(cursor, next_free, &rules, &mut no_phantom_probe);
            let (anomaly, total_size) = match verdict {
                GridProbe::Gap { len } => {
                    cursor += len;
                    continue;
                }
                GridProbe::EmptyRun { resume } => {
                    cursor = resume;
                    continue;
                }
                GridProbe::ZeroSpan { .. } => {
                    LATE_WALK_ZERO_RUNS[0].fetch_add(1, Ordering::Relaxed);
                    (true, 0)
                }
                // gcf f4/y (young5 P1): a header whose extent crosses the next
                // free block is not parsed through the hole (its "slots" there
                // are the free block's stale bytes, and its stride leaves the
                // grid). The anomaly arm scans `[cursor, free_off)`
                // conservatively and resumes on the free block, which
                // `resync_to_next_free_block` lands on exactly.
                GridProbe::BadSize { .. }
                | GridProbe::BadGapFiller
                | GridProbe::CrossesFreeBlock { .. } => (true, 0),
                GridProbe::Object { size } | GridProbe::Phantom { size, .. } => (false, size),
            };
            // SAFETY: `obj_ptr` is an in-bounds header address of mapped from-space.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if anomaly {
                let stretch_lo = cursor;
                let resynced = resync_to_next_free_block(&mut cursor, &mut free_iter);
                let stretch_hi = if resynced { cursor } else { used };
                let mut mark_if_old_base = |candidate: usize| {
                    if walked.has_base(candidate) {
                        // SAFETY: `candidate` is a verified old-gen object
                        // BASE; its header is valid and mutable for marking.
                        let ref_header = unsafe { &mut *(candidate as *mut ObjectHeader) };
                        if ref_header.gc_flags() & GC_FLAG_MARKED == 0 {
                            ref_header.add_gc_flags(GC_FLAG_MARKED);
                            worklist.push(candidate as *mut u8);
                            walked.note_marked(candidate);
                        } else if walked.retrace_stale_mark(candidate) {
                            worklist.push(candidate as *mut u8);
                        }
                        if let Some(pins) = anomaly_pins.as_deref_mut() {
                            pins.push(candidate);
                        }
                    } else if let Some(pins) = anomaly_pins.as_deref_mut() {
                        // The unpinned mark never resolves an interior word
                        // from this scan (only a pinned compaction passes
                        // `anomaly_pins`).
                        if let Some((base, _)) = walked.interior_base(candidate) {
                            pins.push(base);
                            // gcd d1/d (`gengc-r4w6-review6-old-pinned-compaction-residuals`,
                            // "Also noted" 1): MARK and trace it here, as the
                            // exact-base arm above does. It used to be marked
                            // only by the compaction's Phase 0a, whose
                            // closure (`close_live_set_over_old_gen`) follows
                            // reference SLOTS only — so the side-table edges
                            // the BFS follows for every other marked object
                            // (external overlays, the loader, mirror and
                            // metadata pins) were never followed for it, and
                            // what only those edges kept could be freed under
                            // a kept owner. The pin keeps the object either
                            // way, so marking it keeps nothing the compaction
                            // was not already keeping.
                            // SAFETY: `base` is a walked old-gen object base
                            // (the oracle's answer), its header valid and
                            // mutable for marking, as in the arm above.
                            let ref_header = unsafe { &mut *(base as *mut ObjectHeader) };
                            if ref_header.gc_flags() & GC_FLAG_MARKED == 0 {
                                ref_header.add_gc_flags(GC_FLAG_MARKED);
                                worklist.push(base as *mut u8);
                                walked.note_marked(base);
                            } else if walked.retrace_stale_mark(base) {
                                worklist.push(base as *mut u8);
                            }
                        }
                    }
                };
                // SAFETY: `[base+stretch_lo, base+stretch_hi)` is mapped
                // from-space memory.
                unsafe {
                    for_each_conservative_ref_slot(
                        base,
                        stretch_lo,
                        stretch_hi,
                        active_narrow_geometry(),
                        |_slot, _width, value| mark_if_old_base(value),
                    );
                }
                // gcd d2/g (`gcd-d1c-young-instance-loader-lost-in-an-unparseable-young-stretch`):
                // the instance → loader seed below needs each object's CLASS,
                // which an unparseable stretch does not give. Every young
                // object is retained by this collection, so seed the loader of
                // every word in the stretch that reads as a PUBLISHED header
                // (`GC_FLAG_HEADER`) of a class with a `loader_pin` row. The
                // grid is unknown here, so this probes every 8-byte-aligned
                // word: a real header inside the stretch is always found, and
                // a field that merely looks like one can only keep a loader
                // alive one more cycle — the conservative direction, as the
                // word scan above is for old referents. Nothing runs while no
                // user loader has defined a class (`young_loaders` is `None`).
                if let Some(pins) = young_loaders.as_ref() {
                    let word = cratonvm_types::HEADER_WORD_SIZE;
                    let mut off = (stretch_lo + (word - 1)) & !(word - 1);
                    while off + word <= stretch_hi {
                        // SAFETY: `[base+off, base+off+word)` lies inside the
                        // stretch, which is mapped from-space; only the header
                        // word — the `class_id` `u32` at offset 0 and the mark
                        // word `u32` after it — is read, and the world is
                        // stopped.
                        let (class_id, mark) = unsafe {
                            let p = (base + off) as *const u32;
                            (p.read(), p.add(1).read())
                        };
                        if ObjectHeader::gc_flags_of(mark) & GC_FLAG_HEADER != 0 {
                            if let Some(&loader) = pins.get(&class_id) {
                                mark_and_push_old_gen_in(
                                    loader as *mut u8,
                                    old_gen,
                                    walked,
                                    worklist,
                                    "young stretch->loader_pin",
                                );
                            }
                        }
                        off += word;                    }                }
                if resynced {
                    continue;
                }
                break;
            }

            // gcj j3/rf3 (ec47): a member of the young resurrection set defers
            // its old referents to the resurrection round (see the doc above).
            let deferred_out = match defer.as_mut() {
                Some((members, out)) if members.binary_search(&(obj_ptr as usize)).is_ok() => {
                    Some(&mut **out)                }
                _ => None,
            };
            if let Some(out) = deferred_out {
                // SAFETY: `obj_ptr`/`header` are a valid live young-from object.
                unsafe {
                    for_each_ref_slot(obj_ptr, header, |ref_ptr, _| {
                        if old_gen.contains(ref_ptr) {
                            out.push(ref_ptr as usize);
                        }
                    });
                }
            } else {
                // Mark every old-gen referent (arrays + compact objects: 8-byte
                // pointers; legacy objects: 16-byte Value cells).
                // SAFETY: `obj_ptr`/`header` are a valid live young-from object.
                unsafe {
                    for_each_ref_slot(obj_ptr, header, |ref_ptr, _| {
                        mark_and_push_old_gen_in(
                            ref_ptr,
                            old_gen,
                            walked,
                            worklist,
                            "young->old ref slot",
                        );
                    });
                }
            }
            // gcd d1/c: the young instance's defining loader (see
            // `young_loaders` above). A young loader needs nothing: young is
            // retained by this collection.
            if let Some(&loader) = young_loaders
                .as_ref()
                .and_then(|pins| pins.get(&header.class_id.as_u32()))
            {
                mark_and_push_old_gen_in(
                    loader as *mut u8,
                    old_gen,
                    walked,
                    worklist,
                    "young instance->loader_pin",
                );
            }

            cursor += total_size;
        }
    }

    /// Scan a single object's reference slots for old-gen pointers and mark them.
    pub fn scan_object_for_old_refs(
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        walked: &[(*mut u8, usize)],
        worklist: &mut Vec<*mut u8>,
    ) {
        Self::scan_object_for_old_refs_in(obj_ptr, old_gen, walked, worklist);
    }

    /// [`Self::scan_object_for_old_refs`] over any [`OldMarkGrid`]
    /// (gen r5w5/old9; the O(live) collection passes the block-offset oracle).
    pub fn scan_object_for_old_refs_in<G: OldMarkGrid + ?Sized>(
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        walked: &G,
        worklist: &mut Vec<*mut u8>,
    ) {
        let _ = Self::scan_object_for_old_refs_split_in(obj_ptr, old_gen, walked, worklist, None);
    }

    /// gch h4/pc (`gch-h3mj-proposal-true-root-rounds-rescan-every-marked-old-object-DONE-20261015`,
    /// fix 1) — [`Self::scan_object_for_old_refs_in`], and with `young =
    /// Some((lo, hi, out))` every slot value inside the young range `[lo, hi)`
    /// is appended to `out` instead of being offered to the old mark (which
    /// would return at once for it: young and old gen are disjoint mappings).
    /// That is the slot half of `TrueRootYoung::round`'s per-object scan, done
    /// in the one scan the BFS already makes. Returns `false` when the object
    /// was rejected unscanned (a bad kind tag or an implausible extent): the
    /// caller then hands it to `round` as before, whose own scan does not
    /// screen, so the young targets seeded are the same on both arms.
    pub fn scan_object_for_old_refs_split_in<G: OldMarkGrid + ?Sized>(
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        walked: &G,
        worklist: &mut Vec<*mut u8>,
        young: Option<(usize, usize, &mut Vec<usize>)>,
    ) -> bool {
        // A worklist entry whose `kind`/`element_type` byte is not a valid
        // discriminant was never an object base: some push site handed us a
        // dangling or interior address. This was long COUNTED but not
        // rejected here — see
        // `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`,
        // which explicitly deferred the reject as follow-up work. Deferring it
        // left the door open for the exact hazard that doc's Half 2 fix was
        // written to close: `gen_object_total_size(header)` below reads
        // `header.kind`/`header.element_type` as TYPED `#[repr(u8)]` enums,
        // which is instant UB for an out-of-range discriminant — optimized
        // code is free to lower that into a hardware trap rather than doing
        // anything resembling "the wrong thing but not crashing"
        // (`HIB-DCAST-LATEPHASE.1`: reached this way as a SIGSEGV reading
        // through a garbage `array_length` treated as a slot/byte count).
        //
        // gengc-round1-core (2026-09-20) — THE "INSTANT UB" HALF OF THAT
        // JUSTIFICATION NO LONGER HOLDS, and the reject is kept on the OTHER
        // half. The quartet now lives in the atomic mark word and both
        // decoders are TOTAL: `ObjectHeader::kind_of` coerces the one unmapped
        // 2-bit value to `Object`, and `element_type()` is
        // `array_element_type_from_tag(..).unwrap_or(Reference)`. No
        // `#[repr(u8)]` byte is transmuted anywhere on this path any more, so
        // there is no discriminant UB left to prevent. What still argues for
        // the reject is the SECOND half — a bad tag means this was never an
        // object base, so scanning it reads garbage "referents" — and that is
        // unchanged. Recorded because the reject FAILS OPEN (it drops every
        // referent of the entry and returns), and a fail-open screen whose
        // stated reason has evaporated deserves to be re-decided on current
        // grounds rather than inherited ones; see
        // `docs/feature-designs/generative-gc-architecture.md`.
        // Reject now, mirroring `OldGen::scan_region`'s equivalent fix in
        // `defaultcatalogandschema-late-phase-instability-20260801-FIXED.md`:
        // validate the raw tag bytes through
        // `object_kind_from_tag`/`array_element_type_from_tag` before ever
        // forming a `&ObjectHeader` reference.
        // SAFETY: every push site range-checked `obj_ptr` against old gen, so
        // offsets 4/5 are mapped; reading a raw `u8` has no validity
        // requirement beyond in-bounds-and-readable.
        let kind_tag = unsafe { cratonvm_types::kind_tag_at(obj_ptr) };
        let elem_tag = unsafe { cratonvm_types::element_type_tag_at(obj_ptr) };
        if object_kind_from_tag(kind_tag).is_none()
            || !cratonvm_types::element_tag_ok(kind_tag, elem_tag)
        {
            OLDMARK_BAD_KIND_HITS.fetch_add(1, Ordering::Relaxed);
            // gcj j3/og3: its slots go unfollowed (`OldGen::note_mark_gap_fact`).
            old_gen.note_mark_gap_fact();
            return false;
        }
        // SAFETY: `obj_ptr` is a live old-gen object from the mark worklist,
        // and the kind/element_type tag bytes were just validated above.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        // DoHead comb-7 fix (2026-07-03): validate the claimed extent before
        // scanning — the young-mark twin of this check caught a corrupt
        // header whose `array_length` was pointer bytes; scanning that count
        // of slots runs off the mapped region (SIGSEGV). An old object whose
        // extent leaves old gen is definitionally corrupt: skip the scan.
        //
        // GCAUD-8: this bail is itself a fail-OPEN mark source — every referent
        // of this object goes unmarked, and the in-place sweep would then free
        // them. It stays (scanning a corrupt extent is a SIGSEGV, and the
        // walk-base oracle cannot supply a slot count) because it is no longer
        // the last word: `OldGen::close_live_set`, which runs over the walked
        // grid before the free loop and does not consult the header's extent,
        // promotes anything this object still points at. Same repair the
        // compactor's Phase 0 has always made.
        let total = gen_object_total_size(header);
        // `wrapping_add`, not `add`: `total` comes from an UNPROVEN header, so the
        // last-byte address may lie outside the allocation, and `add` would be UB
        // before `contains` could reject it. It is only range-checked, never
        // dereferenced.
        //
        // gen r5w3/oldgen7: the whole extent, not its last byte
        // (`OldGen::contains_range`). Identical without holes; with an interior
        // decommit's holes a stale header can claim an extent whose two ends
        // are committed and whose middle is not, and the slot scan below
        // would fault in it.
        if total < HEADER_SIZE || !old_gen.contains_range(obj_ptr, total) {
            // gcj j3/og3: its slots go unfollowed (`OldGen::note_mark_gap_fact`).
            old_gen.note_mark_gap_fact();
            let n = SWEEP_BAD_EXTENT_HITS.fetch_add(1, Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    "old-gen mark: rejecting object at {:p} with implausible extent {} \
                     (kind={}, array_len={}, num_slots={}) — corrupt header, not scanned",
                    obj_ptr,
                    total,
                    ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                    header.array_length(),
                    header.num_slots(),
                );
            }
            return false;
        }
        // Mark every old-gen referent (arrays + compact objects: 8-byte pointers;
        // legacy objects: 16-byte Value cells).
        // gch h4/pc: and, when asked, collect the young-range ones instead.
        match young {
            None => {
                // SAFETY: `obj_ptr`/`header` are a valid live object.
                unsafe {
                    for_each_ref_slot(obj_ptr, header, |ref_ptr, _| {
                        mark_and_push_old_gen_in(
                            ref_ptr,
                            old_gen,
                            walked,
                            worklist,
                            "old-gen ref slot",
                        );
                    });
                }
            }
            Some((lo, hi, out)) => {
                // SAFETY: as above.
                unsafe {
                    for_each_ref_slot(obj_ptr, header, |ref_ptr, _| {
                        let a = ref_ptr as usize;
                        if a >= lo && a < hi {
                            out.push(a);
                        } else {
                            mark_and_push_old_gen_in(
                                ref_ptr,
                                old_gen,
                                walked,
                                worklist,
                                "old-gen ref slot",
                            );
                        }
                    });
                }
            }
        }
        true
    }

    /// After old-gen compaction, update references in young from-space that
    /// pointed to old-gen objects which have been relocated.
    pub fn fixup_young_old_refs(
        young_from: &Arena,
        compact_map: &cratonvm_types::PointerMap,
        young_skips: &[(usize, usize)],
    ) {
        YOUNG_WALK_ENTRIES[3].fetch_add(1, Ordering::Relaxed);
        // Skip non-moving-sweep holes — same rationale as `mark_young_to_old_refs`:
        // a linear from-space walk must not stride into a reclaimed zeroed hole
        // (it would desync off the object grid and misread a live object's
        // interior cell, then `break` and leave the rest of from-space's
        // old-gen refs un-fixed-up after a compaction → dangling pointers).
        //
        // H2-CID0 (2026-08-01): the skip list must ALSO carry the reserved
        // TLAB tails of forcibly-stopped in-JIT peers (BUG-03), which is why it
        // is passed in rather than read from `young_from` here.
        //
        // A reserved tail is the unallocated remainder of a TLAB whose owner
        // was frozen mid-JIT. It is not on the free list and it holds no
        // objects -- `sweep_young_non_moving` merges these regions into its own
        // skip list for exactly that reason ("an un-retired tail is neither
        // walked as objects nor reclaimed"). These two walks did not, so they
        // parsed the tail's bytes as headers. When those bytes decode plausibly
        // the walk strides off the object grid and keeps going: the REAL young
        // objects after the tail can be read at wrong offsets.
        //
        // MEASURED, so the next reader does not over-claim this: a negative
        // control (2026-08-01) shows the walk survives an unparseable reserved
        // tail WITHOUT this merge, because the desync trips the anomaly path
        // and the conservative word scan still finds every old-gen base in the
        // stretch. So this is a CONSISTENCY fix -- every other linear
        // from-space walker skips these regions, and parsing a tail that holds
        // no objects is pointless work resting on that fallback -- not a
        // demonstrated bug fix.
        //
        // Why the fallback matters so much here: `OldGen::close_live_set`
        // (`5750caf5f`) closes the live set over references from marked
        // OLD-GEN objects, so it cannot rescue a block whose only referrer is
        // YOUNG -- the shape H2's `CacheLongKeyLIRS` has, holding its `Page` /
        // `ByteBuffer` / `Chunk` payloads from entries that are still young.
        // Nor would `CRATONVM_DBG_SWEEP_EDGES` see it: that assertion looks
        // for references INTO young, never young->old.
        let free_blocks: Vec<(usize, usize)> = if young_skips.is_empty() {
            young_from.free_blocks_sorted()
        } else {
            let mut v = young_from.free_blocks_sorted();
            v.extend_from_slice(young_skips);
            v.sort_by_key(|&(off, _)| off);
            v
        };
        let mut free_iter = free_blocks.iter().peekable();
        let mut cursor: usize = 0;
        let base = young_from.base_ptr() as usize;
        let used = young_from.used();
        // DoHead walk-desync hardening (2026-07-02): a stretch this walk
        // cannot parse must not be dropped — a stale reference to a MOVED
        // old-gen object is a guaranteed dangling pointer. Fall back to a
        // conservative word rewrite over the stretch: any aligned word that
        // exactly matches a relocated old address is rewritten to the new
        // address. (A primitive that happens to equal a moved object's old
        // address would be corrupted — vanishingly unlikely — whereas an
        // unrewritten real reference is a certain use-after-free.)
        // The mark-phase counterpart of this scan is in
        // `mark_young_to_old_refs`; both have to agree about slot WIDTH or the
        // pair leaves dangling refs behind. See gc/src/compressed_oops.rs,
        // hole 2: with narrow oops on, an 8-byte read spans two 4-byte
        // reference slots, so no relocated address is ever recognised and
        // every reference in an unparseable stretch is left pointing at the
        // old, now-evacuated address.
        //
        // A narrow rewrite only fires when the DECODED value is a key in
        // `compact_map` — i.e. the exact old base of an object this cycle
        // actually moved — and writes back the re-encoded new base at the same
        // width it read. `encode` returns 0 for anything outside the
        // compressible range, so a slot is only ever rewritten to a value that
        // round-trips; the guard keeps a non-encodable destination from
        // silently becoming a null.
        let narrow = active_narrow_geometry();
        let rewrite_stretch_conservatively = |lo: usize, hi: usize| {
            // SAFETY: `[base+lo, base+hi)` is mapped from-space memory, and
            // each `slot` handed back is a location this scan just read.
            unsafe {
                for_each_conservative_ref_slot(base, lo, hi, narrow, |slot, width, value| {
                    let Some(&new_addr) = compact_map.get(&value) else {
                        return;
                    };
                    if width == 8 {
                        *(slot as *mut u64) = new_addr as u64;
                    } else {
                        // Re-encode at the width it was read. `encode` returns
                        // 0 for an address outside the compressible range, and
                        // writing that would turn a live reference into null —
                        // so leave the slot alone instead. A destination
                        // outside the range cannot happen for a young-to-old
                        // move inside the configured heap; the guard is here so
                        // that if it ever does, the failure is a stale
                        // reference the next cycle can still see rather than a
                        // silent null.
                        let re = cratonvm_types::narrow_oop::encode(new_addr as u64);
                        if re != 0 {
                            *(slot as *mut u32) = re;
                        }
                    }
                });
            }
        };
        // gen r5w1/young5: the grid rules are `probe_grid_at`'s, with this
        // walk's policy unchanged exactly as in `mark_young_to_old_refs` (a
        // malformed GAP filler is parsed; a hole-crossing header is still
        // parsed as an object).
        //
        // A run of EMPTY objects is not a desync, and the second half of the
        // argument is what carries it here, exactly as in
        // `mark_young_to_old_refs`: an empty object has NO FIELDS, so a run of
        // them holds no reference into old gen for this pass to rewrite.
        // Stepping over it cannot strand one at a stale pre-compaction
        // address. There is no young live set to hand the predicate — this
        // walk is given a from-space, not a mark result — so `&[]` is all
        // there is and the no-marked-base-inside condition is vacuous. The
        // alignment and plausible-next-header conditions still establish that
        // `resume` is on-grid. The anomaly arm below is not free: it
        // re-anchors at the next free block and falls back to
        // `rewrite_stretch_conservatively` over everything skipped, which
        // rewrites any aligned word that merely EQUALS a moved object's old
        // address — a primitive included — and gives up the precise parse of
        // every real object in the stretch.
        let rules = GridRules {
            bad_gap_filler_is_anomaly: false,
            ..GridRules::unmarked(base, used)
        };
        let mut no_phantom_probe = 0usize;
        while cursor < used {
            if skip_free_blocks(&mut cursor, &mut free_iter).0 {
                continue;
            }
            // SAFETY: `cursor` is within `young_from.used()`; pointer arithmetic stays in the arena.
            let obj_ptr = unsafe { young_from.base_ptr().add(cursor) as *mut u8 };
            let next_free = free_iter.peek().map(|&&(off, _)| off);
            // `cursor < used` inside mapped from-space (`probe_grid_at`'s contract).
            let verdict = probe_grid_at(cursor, next_free, &rules, &mut no_phantom_probe);
            let (anomaly, total_size) = match verdict {
                GridProbe::Gap { len } => {
                    cursor += len;
                    continue;
                }
                GridProbe::EmptyRun { resume } => {
                    cursor = resume;
                    continue;
                }
                GridProbe::ZeroSpan { .. } => {
                    LATE_WALK_ZERO_RUNS[1].fetch_add(1, Ordering::Relaxed);
                    (true, 0)
                }
                // gcf f4/y (young5 P1): a hole-crossing header is rewritten
                // conservatively over `[cursor, free_off)`, not parsed through
                // the free block (see `mark_young_to_old_refs`).
                GridProbe::BadSize { .. }
                | GridProbe::BadGapFiller
                | GridProbe::CrossesFreeBlock { .. } => (true, 0),
                GridProbe::Object { size } | GridProbe::Phantom { size, .. } => (false, size),
            };
            // SAFETY: `obj_ptr` is an in-bounds header address of mapped from-space.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if anomaly {
                let stretch_lo = cursor;
                let resynced = resync_to_next_free_block(&mut cursor, &mut free_iter);
                let stretch_hi = if resynced { cursor } else { used };
                rewrite_stretch_conservatively(stretch_lo, stretch_hi);
                if resynced {
                    continue;
                }
                break;
            }

            // Remap any reference into a relocated old-gen object (arrays +
            // compact objects: 8-byte pointers; legacy objects: 16-byte cells).
            // SAFETY: `obj_ptr`/`header` are a valid live young-from object.
            unsafe {
                forward_ref_slots(obj_ptr, header, |ref_ptr| {
                    compact_map
                        .get(&(ref_ptr as usize))
                        .map(|&new_addr| new_addr as *mut u8)
                });
            }

            cursor += total_size;
        }
    }

    // ----- Internal ----------------------------------------------------------

    /// Allocate bytes in the young from-space and run `init` before publishing
    /// the span to any GC walker.
    ///
    /// Returns `None` if the young generation is exhausted and cannot satisfy
    /// the allocation. The caller should trigger a GC cycle and retry, or
    /// throw `OutOfMemoryError`.
    pub fn try_alloc_young_initialized<F>(&self, size: usize, init: F) -> Option<*mut u8>
    where
        F: FnOnce(*mut u8),
    {
        // NUMA stub: query the preferred node for the calling thread so
        // the topology probe gets exercised in production. On multi-node
        // hosts this records when the heap's primary node disagrees with
        // the caller's node — that delta is the win the multi-arena
        // upgrade will eventually capture. On single-node hosts (every
        // current test target) this is two integer compares and a return.
        let node = self.numa_slow_path_hint();
        if self.numa_num_nodes > 1 && node != self.numa_node_hint {
            tracing::trace!(
                target: "cratonvm::gc::numa",
                node, primary = self.numa_node_hint, size,
                "try_alloc_young_initialized: cross-node slow path (single-arena fallback)",
            );
        }

        // gen r4w2/alloc2 (2026-09-23): the rounded footprint, and the part of
        // it that must still be zeroed once the guard is dropped (see the
        // large-object arm below).
        let zero_len = size.checked_add(7).map_or(size, |v| v & !7);
        // gen r4w4/alloc4: the one-zeroing-point skip, as for TLAB refills —
        // see `zero_young_hand_out` for the invariant.
        let zero_once = gc_flags().gen_zero_once;
        let (ptr, deferred_zero, known_zero) = {
            let mut from = self.lock_young_from();
            let cursor_before = from.used_low_for_compaction();
            let Some(ptr) = from.alloc(size, 8) else {
                // gch h2/yg (`gcg-g5yg-proposal-defrag-gate-counts-large-request-misses`):
                // a large request young cannot place although its free list
                // holds many times it arms the pinned copy's defrag gate.
                self.note_young_fragmentation_miss(size, from.free_list_bytes());
                return None;
            };
            // perf/gc-oracle-anchors (2026-07-25): this offset is an object
            // base by definition — record it as a sweep/oracle anchor. See
            // `Arena::note_object_start`.
            let off = ptr as usize - from.base_ptr() as usize;
            from.note_object_start(off);
            // Which part of this hand-out has never been written since the
            // arena was created (and so already reads as zero)? `None` unless
            // the arena's pristine skip is armed -- only by unit tests since
            // gce e2/o removed `CRATONVM_GEN_PRISTINE_CHUNKS`; asked while
            // the guard still pins the answer to THIS hand-out.
            let pristine = from.take_last_hand_out_pristine(ptr);
            // gen r4w4/alloc4: a hand-out off the bump tail is zero already
            // (`zero_young_hand_out`'s invariant), header included — `init`
            // writes the whole header — so neither memset below runs for it.
            // The debug-build sample runs under the guard, before `init`.
            // gce e1/o: asked whatever the switch says (the reuse census).
            let from_bump =
                hand_out_is_bump_tail(cursor_before, off, zero_len, from.used_low_for_compaction());
            // gce e1/y (`CRATONVM_GEN_ZERO_ONCE_FREE_LIST`, opt-in): a free-list
            // hand-out wholly inside a span the last sweep zeroed
            // (`Arena::hand_out_is_known_zero`) is zero too; it takes the
            // bump-tail arm, sample check and repair included.
            let known_zero_reuse = zero_once
                && !from_bump
                && from.hand_out_is_known_zero(off, zero_len);
            // JIT round 23 wave 4, lane L4: a hand-out overlapping bytes a
            // pinned in-place cycle left unzeroed (`Arena::take_lazy_dirty`)
            // is memset in full here, the bump tail included.
            let lazy_dirty = match from.take_lazy_dirty_counted(off, zero_len) {
                Some((_, dirty)) => {
                    // t1 lane F: `HeapStats::young_lazy_dirty_zeroed_bytes`.
                    self.stats
                        .young_lazy_dirty_zeroed_bytes
                        .fetch_add(dirty as u64, Ordering::Relaxed);
                    true
                }
                None => false,
            };
            let bump_tail = zero_once && (from_bump || known_zero_reuse) && !lazy_dirty;
            if !from_bump && !known_zero_reuse {
                self.stats
                    .young_zeroed_reuse_bytes
                    .fetch_add(zero_len as u64, Ordering::Relaxed);
            }
            // SAFETY: `[ptr, ptr + zero_len)` is the span `Arena::alloc` just
            // carved for this caller (it rounds the request to `zero_len`).
            let known_zero = bump_tail && unsafe { known_zero_sample_is_clean(ptr, zero_len) };
            if bump_tail && !known_zero {
                self.stats
                    .young_zero_repairs
                    .fetch_add(1, Ordering::Relaxed);
            }
            // LARGE OBJECTS ARE ZEROED OUTSIDE THE LOCK (gen r4w2/alloc2,
            // 2026-09-23). Every allocation above `TLAB_MAX_ALLOC` comes here,
            // and anything below the humongous threshold (up to 256 MiB) stays
            // young — so a thread allocating a large array used to hold
            // `young_from` for the whole memset while every other thread's
            // refill, slow-path allocation and locked probe queued behind it.
            // Above `YOUNG_ZERO_OUTSIDE_LOCK_MIN` only the HEADER is zeroed and
            // written under the guard, which is all a lock-holding walker can
            // need (a parseable header at the object's base, so it strides the
            // object by its size); the body is zeroed after the guard drops.
            // The object is reachable by no other thread until this function
            // returns, and no collection can run before the caller reaches a
            // safepoint — the premise `refill_tlab` documents for its chunk.
            // Below the threshold the code is unchanged: zero, init, unlock.
            let split =
                !known_zero && zero_len >= YOUNG_ZERO_OUTSIDE_LOCK_MIN && zero_len > HEADER_SIZE;
            let locked_len = if known_zero {
                0
            } else if split {
                HEADER_SIZE
            } else {
                zero_len
            };
            // gengc-round1-core (2026-09-20): zero the ROUNDED footprint, not
            // the requested size. `Arena::alloc` rounds the request to
            // `(size + 7) & !7` and hands out that whole span, and every
            // collector walk strides objects by the rounded size — so for a
            // `size` that is not a multiple of 8, up to 7 bytes INSIDE the
            // object's own extent were left holding whatever the previous
            // occupant wrote, while all three callers' SAFETY notes assert a
            // "zeroed region of `total_size` bytes". Every caller today passes
            // an 8-aligned size (`HEADER_SIZE + n * SLOT_SIZE`,
            // `ARRAY_DATA_OFFSET + array_data_size(..)`), so this is latent
            // rather than live — but the helper is generic and its contract
            // should be the one it claims.
            //
            // SAFETY: `ptr` was allocated from the arena for the rounded
            // footprint (`Arena::alloc` rounds before it reserves), so zeroing
            // `locked_len <= zero_len` bytes is within the handed-out span.
            // `init` writes the valid header before the arena lock is released.
            unsafe { zero_span_skipping_pristine(ptr, locked_len, pristine) };
            init(ptr);
            if cratonvm_gc_core::tlab::watch_covers(ptr as usize, ptr as usize + size) {
                cratonvm_gc_core::tlab::watch_note(format_args!(
                    "ARENA SLOW PATH handed it out: ptr=0x{:x} size={size} arena_used=0x{:x}",
                    ptr as usize,
                    from.used(),
                ));
            }
            (ptr, if split { Some(pristine) } else { None }, known_zero)
        };
        let counter = if known_zero {
            &self.stats.young_zero_skipped_bytes
        } else {
            &self.stats.young_zeroed_bytes
        };
        counter.fetch_add(zero_len as u64, Ordering::Relaxed);
        if let Some(pristine) = deferred_zero {
            // SAFETY: `[ptr + HEADER_SIZE, ptr + zero_len)` is the body of the
            // span `Arena::alloc` carved above for this caller alone
            // (`zero_len > HEADER_SIZE` is part of `split`). No other thread
            // holds a reference to the object yet and no collection can run
            // before this thread reaches a safepoint, so the body cannot be
            // observed half-zeroed by anything that would read it as fields.
            unsafe {
                zero_span_skipping_pristine(ptr.add(HEADER_SIZE), zero_len - HEADER_SIZE, pristine)
            };
        }
        self.stats.young_allocations.fetch_add(1, Ordering::Relaxed);
        Some(ptr)
    }

    /// Check if an allocation of `size` bytes would succeed in the young from-space.
    /// Does NOT allocate — just probes available space.
    ///
    /// Mirrors what [`Arena::alloc`] would actually do: it can satisfy a request
    /// from the bump tail OR from a single free-list block. Checking ONLY the
    /// bump cursor (`used()` vs `capacity()`) is wrong after a non-moving sweep:
    /// that sweep reclaims dead objects into the free list but cannot retreat the
    /// cursor (it can't relocate survivors while conservative JIT roots are
    /// live), so once the cursor reaches capacity a cursor-only probe reports OOM
    /// even with gigabytes free. The JIT slow path (`jit_new_object`) treats that
    /// false OOM as "retire TLAB + force GC", so EVERY slow-path allocation
    /// forces a young GC — the bintrees18 allocation-failure thrash (~840 GCs/s,
    /// each reclaiming nothing). Falling back to the free list here lets the
    /// allocation proceed from reclaimed space without a spurious GC.
    pub fn try_alloc_young_probe(&self, size: usize) -> Option<()> {
        let mut from = self.lock_young_from();
        // Bump tail.
        if Self::young_bump_fits(from.used(), from.capacity(), size) {
            return Some(());
        }
        // Reclaimed free-list space (only reached when the bump tail can't
        // satisfy the request — keeps the common path a single cursor compare).
        // `has_free_block_at_least` early-exits at the first satisfying block
        // and answers repeated too-large requests in O(1) via the cached
        // upper bound — the former `largest_free_block()` full scan here ran
        // once per JIT slow-path allocation (~7% of binarytrees-18).
        if from.has_free_block_at_least(size) {
            Some(())
        } else {
            // gch h2/yg: the JIT slow path's miss is a young miss too (it
            // forces the collection the allocation then waits for).
            self.note_young_fragmentation_miss(size, from.free_list_bytes());
            None
        }
    }

    /// gch h2/yg (`gcg-g5yg-proposal-defrag-gate-counts-large-request-misses`,
    /// under `CRATONVM_GEN_PINNED_COPY_DEFRAG`): a young allocation of `size`
    /// bytes found no room -- neither the bump tail nor any single free block
    /// -- while the free list held `free_list_bytes`. When that is a
    /// FRAGMENTATION miss ([`young_miss_is_fragmentation`]), arm
    /// `young_defrag_pending`, so the next term-4-alone cycle runs the
    /// (defragmenting) pinned copy instead of the sweep, which cannot make the
    /// request fit. The sweep-time measure ([`young_space_fragmented`]) counts
    /// only sub-TLAB holes and misses this shape (holes of TLAB size and up,
    /// none as large as one big array). Inert with the switch off.
    pub fn note_young_fragmentation_miss(&self, size: usize, free_list_bytes: usize) {
        if self.pinned_copy_defrag && young_miss_is_fragmentation(size, free_list_bytes) {
            self.young_defrag_pending.store(true, Ordering::Relaxed);
            self.pinned_copy_defrag_census[4].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// O(1) bump-tail-only headroom probe — see `VmHeap::young_bump_headroom`.
    /// Deliberately does NOT fall back to the free list: this feeds the JIT
    /// TLAB-refill gate, where a per-allocation `largest_free_block` scan of a
    /// fragmented young free list is exactly the pathology being avoided.
    ///
    /// # Lock-free (gen r4/alloc, 2026-09-23)
    ///
    /// This used to take `young_from.lock()` to read two words. It is the
    /// FIRST of up to three acquisitions of that mutex on every guarded TLAB
    /// refill (`tlab_alloc_shaped_inner`: this gate, then
    /// [`Self::young_has_free_block`], then [`Self::refill_tlab`]) — and each
    /// acquisition is an RMW on the one line every allocating thread wants,
    /// plus three republishing stores on `YoungFromGuard::drop` that invalidate
    /// every reader of the trigger triple. The two words it needs ARE that
    /// triple: `used` and `capacity` as of the last unlock, which is exactly
    /// the state a locked read would have seen an instant earlier. The answer
    /// was already stale the moment the lock was released (a peer can refill
    /// before this caller's `refill_tlab` runs), so the gate has always been a
    /// heuristic whose backstop is `refill_tlab` itself failing — the same
    /// argument `needs_gc`'s lock-free trigger rests on, under the same kill
    /// switch: `CRATONVM_GC_TRIGGER_LOCKFREE=0` restores the locked read.
    pub fn young_bump_headroom(&self, size: usize) -> bool {
        let (used, capacity) = if trigger_lockfree_enabled() {
            let (used, _free, capacity) = self.young_from_published();
            (used, capacity)
        } else {
            let from = self.lock_young_from();
            (from.used(), from.capacity())
        };
        Self::young_bump_fits(used, capacity, size)
    }

    /// The bump-tail arithmetic [`Self::young_bump_headroom`] and
    /// [`Self::try_alloc_young_probe`] share: would a `size`-byte request,
    /// placed at the 8-rounded `used` cursor, end at or before `capacity`?
    /// Checked throughout, so a `size` near `usize::MAX` answers `false`
    /// rather than wrapping to a small end.
    #[inline]
    pub fn young_bump_fits(used: usize, capacity: usize, size: usize) -> bool {
        used.checked_add(7)
            .map(|v| v & !7)
            .and_then(|aligned| aligned.checked_add(size))
            .is_some_and(|end| end <= capacity)
    }

    /// Amortized-O(1) probe: could a young `refill_tlab(size)` be served from
    /// the RECLAIMED free list right now? Early-exits at the first
    /// sufficiently-large block and fail-fasts through the cached
    /// `max_free_upper` bound (`Arena::has_free_block_at_least`), so unlike
    /// `largest_free_block` it is safe to consult per allocation. Paired with
    /// [`Self::young_bump_headroom`] this is the JIT TLAB-refill gate: after
    /// a non-moving sweep + coalesce the young free list is a handful of big
    /// spans, so refills keep flowing out of reclaimed space without forcing
    /// a GC — while a genuinely-full young answers `false` in O(1) and the
    /// caller takes the old-gen spill exactly as before.
    pub fn young_has_free_block(&self, size: usize) -> bool {
        self.lock_young_from().has_free_block_at_least(size)
    }

    /// DBG: one-shot young-arena state snapshot `(used, capacity,
    /// free_list_bytes, largest_free_block)` — diagnostics only (full
    /// free-list scans under the lock).
    pub fn young_arena_diag(&self) -> (usize, usize, usize, usize) {
        let from = self.lock_young_from();
        (
            from.used(),
            from.capacity(),
            from.free_list_bytes(),
            from.largest_free_block(),
        )
    }

    /// Carve out a TLAB-sized chunk from the young from-space.
    ///
    /// Returns `Some((ptr, size))` on success, where `ptr` is the start of
    /// the zeroed region and `size` is the actual TLAB size (may be smaller
    /// than requested if the arena is nearly full).
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        self.refill_tlab_at_least(requested_size, 0)
    }

    /// [`Self::refill_tlab`] with a floor: carve at least `min_size` bytes or
    /// nothing (gcd d1/d, 2026-09-27).
    ///
    /// `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object-FIXED-20261012.md`,
    /// clamp 2. The VM's refill site (`tlab_alloc_shaped_inner`) refills
    /// because ONE object of `total_size` bytes missed, and it tries that
    /// object in the fresh buffer. `refill_tlab` clamps the carve to what young
    /// has left (`requested.min(available)`) and its fragmentation fallback
    /// serves the largest span it finds, down to `frag_tlab_floor()`; either
    /// can hand out a chunk smaller than the object. The object then fails on
    /// the fresh buffer, the function returns `None` with an UNUSABLE buffer
    /// installed, and the bytes are carved (and memset) for nothing — the
    /// span stays out of the free list and the bump tail until the next
    /// retire gives it back. Here the refill declines BEFORE carving instead,
    /// and the caller's `None` handling (the per-object fallback, the old-gen
    /// spill of the native allocator, the wedge breaker's count on the JIT
    /// path) is exactly what it was for the useless buffer.
    ///
    /// `min_size` is on the 8-byte grid (the caller passes the object's
    /// footprint rounded up). `0` is [`Self::refill_tlab`] byte for byte. The
    /// debug stress cap (`CRATONVM_DBG_GC_STRESS`) never cuts a request below
    /// `min_size` either: under stress a carve now holds the object that asked
    /// for it (at most `tlab_max_alloc()`, 32 KiB), where it used to be cut to
    /// `t / 8` and then fail the object.
    ///
    /// No other backend's refill changes: `VmHeap::refill_tlab_at_least`
    /// forwards to G1's and ZGC's plain `refill_tlab` (their half is written
    /// as an exact remaining edit on the page).
    pub fn refill_tlab_at_least(
        &self,
        requested_size: usize,
        min_size: usize,
    ) -> Option<(*mut u8, usize)> {
        // NUMA stub: same shape as try_alloc_young_initialized. The TLAB itself is
        // per-thread so its fast path is already NUMA-local; this records
        // the *refill* node so that once we land per-node arenas we can
        // size each node's young-from to match its TLAB refill pressure.
        let node = self.numa_slow_path_hint();
        if self.numa_num_nodes > 1 && node != self.numa_node_hint {
            tracing::trace!(
                target: "cratonvm::gc::numa",
                node, primary = self.numa_node_hint, requested_size,
                "refill_tlab: cross-node refill (single-arena fallback)",
            );
        }
        // gci i4/yg (`CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`): the
        // progress base of this young cycle, taken at its first refill. One
        // branch on a heap field with the switch off.
        if self.young_exhausted_refill_collects {
            self.note_young_refill_epoch();
        }

        // THE LOCK COVERS THE CARVE, NOT THE MEMSET (gen r4/alloc, 2026-09-23).
        //
        // This used to zero the chunk — up to `MAX_TLAB_SIZE`, 1 MiB — with
        // `young_from` still held, so every other thread's refill, slow-path
        // allocation, trigger probe and `young_bump_headroom` gate queued behind
        // one thread's memset: the lock hold time was O(TLAB size) where the
        // work it protects (a cursor bump or a free-list pop, plus an anchor)
        // is O(1). The chunk is this caller's alone the moment `Arena::alloc`
        // returns it: no other mutator can be handed an overlapping span, and
        // no collection can run until this thread reaches a safepoint, which
        // it does not do before its caller has installed the chunk as a TLAB
        // (the same premise the caller has always relied on between this
        // function's return and `Tlab::new`). So the tripwire read and the
        // memset run after the guard drops, on memory nobody else can see
        // move.
        // gen r4w4/alloc4: ONE ZEROING POINT for bump-tail memory. See
        // `zero_young_hand_out`, called below the carve.
        let flags = gc_flags();
        let zero_once = flags.gen_zero_once;
        // gen r4w6/tlab6: under `CRATONVM_DBG_GC_STRESS=<t>` no carve exceeds
        // `stress_tlab_carve_cap(t)` (about `t / 8`), so the stress trigger's
        // carve bound (`gc_stress_carve_bound`) can tell "t bytes were
        // allocated" from "a few threads each carved a buffer". A debug knob
        // only: with stress off the request is untouched.
        // gcd d1/d: never below `min_size` (0 = no floor, the historical cap).
        let requested_size = match flags.gc_stress_bytes {
            Some(t) => requested_size.min(stress_tlab_carve_cap(t)).max(min_size),
            None => requested_size,
        };
        let (ptr, actual_size, arena_base, arena_used, pristine, bump_tail, from_bump, lazy_dirty) = {
            let mut from = self.lock_young_from();
            let available = from.remaining();
            if available < 256 {
                return None; // Not enough for a useful TLAB
            }
            // The low bump cursor before the carve: `hand_out_is_bump_tail`
            // compares the hand-out against it and against the cursor after.
            let cursor_before = from.used_low_for_compaction();
            // Alignment invariant (perf/halfgap residuals, 2026-07-18): a
            // TLAB's size must be a multiple of 8. `available` can carry a
            // mod-8 dreg (unaligned free-list dust, or the historical
            // unaligned-capacity bump tail), and an unaligned TLAB both mints
            // an off-grid free-list split remnant AND gets its end rounded
            // DOWN by `Tlab::new`'s release safety net — leaving an untracked
            // zeroed sliver between the TLAB's filler and the next region that
            // derails the non-moving walk (the trigger-ON bt18 corruption; see
            // tlab-trigger-gc-young-walk-corruption-FIXED.md).
            let actual_size = requested_size.min(available) & !7;
            // gcd d1/d: `actual_size < min_size` means young holds fewer free
            // bytes in total than the object needs; no span can hold it.
            if actual_size == 0 || actual_size < min_size {
                return None;
            }
            // gci i22/dr (`CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`,
            // opt-in): the smallest whole free block first. `None` (switch
            // off, or no free block makes a useful TLAB) is the default carve.
            let whole = if self.tlab_refill_smallest_whole {
                self.refill_smallest_whole_block(&mut from, actual_size, min_size)
            } else {
                None
            };
            let (ptr, carved) = match whole {
                Some(hit) => hit,
                None => match from.alloc(actual_size, 8) {
                    Some(ptr) => (ptr, actual_size),
                    None => Self::refill_fragmentation_fallback(&mut from, actual_size, min_size)?,
                },
            };
            // perf/gc-oracle-anchors (2026-07-25): a TLAB's first byte is an
            // object base — the owning thread bumps its first object there,
            // and `Tlab::retire` tail-fills whatever it did not use, so the
            // whole TLAB is object-covered end to end. This is the anchor that
            // matters: the JIT's inline TLAB fast path never re-enters this
            // file, so TLAB granularity (256 KiB–1 MiB) is the finest grid the
            // Rust side can see for JIT-allocated workloads. The fragmentation
            // fallback's mini-TLAB is anchored by the same line — same
            // argument.
            let off = ptr as usize - from.base_ptr() as usize;
            from.note_object_start(off);
            // gen r4w2/alloc2: the never-written part of this chunk, if the
            // arena's pristine skip is armed (unit tests only since gce e2/o
            // removed `CRATONVM_GEN_PRISTINE_CHUNKS`; `None` otherwise).
            // Asked under the guard, for THIS hand-out.
            let pristine = from.take_last_hand_out_pristine(ptr);
            // gce e1/o: `from_bump` is asked whatever the switch says, so the
            // census can split a reuse from a bump-tail memset.
            let from_bump =
                hand_out_is_bump_tail(cursor_before, off, carved, from.used_low_for_compaction());
            // gce e1/y (`CRATONVM_GEN_ZERO_ONCE_FREE_LIST`, opt-in): a free-list
            // hand-out wholly inside a span the last sweep zeroed
            // (`Arena::hand_out_is_known_zero`) is zero too; it takes the
            // bump-tail arm of `zero_young_hand_out`, sample check and repair
            // included. `from_bump` then counts it as not-reused below.
            let known_zero_reuse = zero_once
                && !from_bump
                && from.hand_out_is_known_zero(off, carved);
            // JIT round 23 wave 4, lane L4: a chunk overlapping bytes a pinned
            // in-place cycle left unzeroed (`Arena::take_lazy_dirty`) is
            // memset in full. r23 w4 review (R-L4z item 4): the dirty extent
            // comes back relative to the chunk, so the occupied-word tripwire
            // still runs on the parts of the chunk OUTSIDE it (stale bytes
            // inside it are expected, not a double issue).
            let lazy_dirty = from.take_lazy_dirty_counted(off, carved).map(|((lo, hi), dirty)| {
                // t1 lane F: `HeapStats::young_lazy_dirty_zeroed_bytes`.
                self.stats
                    .young_lazy_dirty_zeroed_bytes
                    .fetch_add(dirty as u64, Ordering::Relaxed);
                (lo - off, hi - off)
            });
            let bump_tail = zero_once && (from_bump || known_zero_reuse) && lazy_dirty.is_none();
            (
                ptr,
                carved,
                from.base_ptr() as usize,
                from.used(),
                pristine,
                bump_tail,
                from_bump || known_zero_reuse,
                lazy_dirty,
            )
        };
        // Both refill arms (the main carve and the fragmentation fallback's
        // mini-TLAB) reach here with the size actually carved — lane `oom`'s
        // `young_stress_carved` counter (see its accessor).
        self.stats
            .young_tlab_carved_bytes
            .fetch_add(actual_size as u64, Ordering::Relaxed);
        // r23 w4 review (R-L4z item 4): with a lazy-dirty extent `[dlo, dhi)`
        // inside the chunk, the tripwire reads the head `[0, dlo)` and the tail
        // `[dhi, size)` -- unallocated bytes the set does not excuse -- so a
        // double-issued span is still caught there (both parts are on the
        // 8-byte grid; an empty one is not asked). The gaps between dirty
        // spans inside the extent stay unchecked.
        let occupied = match lazy_dirty {
            None => Self::refill_chunk_tripwire(ptr, actual_size, arena_base, arena_used),
            Some((dlo, dhi)) => {
                let head = dlo >= 8 && Self::refill_chunk_tripwire(ptr, dlo, arena_base, arena_used);
                let tail = actual_size.saturating_sub(dhi) >= 8
                    && Self::refill_chunk_tripwire(
                        // SAFETY: `dhi < actual_size` (checked above), inside the chunk.
                        unsafe { ptr.add(dhi) },
                        actual_size - dhi,
                        arena_base,
                        arena_used,
                    );
                head | tail
            }
        };
        self.zero_young_hand_out(ptr, actual_size, pristine, bump_tail, occupied);
        if !from_bump {
            // gce e1/o: a reuse is always memset (see the stat's doc); gce
            // e1/y: a known-zero reuse is not a reuse here (see above).
            self.stats
                .young_zeroed_reuse_bytes
                .fetch_add(actual_size as u64, Ordering::Relaxed);
        }
        Some((ptr, actual_size))
    }

    /// gci i4/yg: the first TLAB refill of a young cycle records the cycle
    /// (`minor_gc_count`) and the carve count at that moment, the base
    /// [`Self::young_exhausted_refill_wants_collection`] measures progress
    /// from. Later refills of the same cycle pay two relaxed loads. Racing
    /// first refills store the same epoch and nearly the same count; the
    /// count is stored before the epoch is published, so a reader that sees
    /// the epoch sees a count at least that recent.
    #[inline]
    pub fn note_young_refill_epoch(&self) {
        let epoch = self.stats.minor_gc_count.load(Ordering::Relaxed);
        if self.young_refill_epoch.load(Ordering::Acquire) != epoch {
            let carved = self.stats.young_tlab_carved_bytes.load(Ordering::Relaxed);
            self.young_refill_carved_at_epoch
                .store(carved, Ordering::Relaxed);
            self.young_refill_epoch.store(epoch, Ordering::Release);
        }
    }

    /// gci i4/yg (`CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`, opt-in): a
    /// JIT-guarded TLAB refill found young unable to hold the object at all
    /// (`refill_tlab_at_least` refused, or the refill gate found neither a
    /// bump tail nor a free block). Should the caller collect young now and
    /// retry, instead of sending the object to the old generation?
    ///
    /// `false` whenever the switch is off. Otherwise [`exhausted_refill_verdict`]:
    /// yes once per young cycle, and only after the cycle carved a quarter of
    /// the semi into TLABs since its first refill (so a young generation full
    /// of live data, whose refills fail before it carves that much, takes the
    /// old-generation spill as before and never storms). A granted verdict
    /// claims the cycle with one CAS, so of two racing threads one collects.
    /// Counted in [`Self::young_exhausted_census`]
    /// (`gen_refill_exhausted_collects/_no_progress/_repeat=` on
    /// `[GC] gen-alloc:`). Lock-free.
    pub fn young_exhausted_refill_wants_collection(&self) -> bool {
        if !self.young_exhausted_refill_collects {
            return false;
        }
        let epoch = self.stats.minor_gc_count.load(Ordering::Relaxed);
        let carved_since = if self.young_refill_epoch.load(Ordering::Acquire) == epoch {
            self.stats
                .young_tlab_carved_bytes
                .load(Ordering::Relaxed)
                .saturating_sub(self.young_refill_carved_at_epoch.load(Ordering::Relaxed))
        } else {
            // No refill yet in this young cycle: nothing carved to reclaim.
            0
        };
        let capacity = self.young_from_pub_capacity.load(Ordering::Relaxed) as u64;
        let fired = self.young_exhausted_fired_epoch.load(Ordering::Relaxed);
        let verdict = exhausted_refill_verdict(carved_since, capacity, fired == epoch);
        let granted = verdict == ExhaustedRefillVerdict::Collect
            && self
                .young_exhausted_fired_epoch
                .compare_exchange(fired, epoch, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
        let slot = match verdict {
            ExhaustedRefillVerdict::Collect if granted => 0,
            ExhaustedRefillVerdict::NoProgress => 1,
            // A lost claim is a repeat of the winner's collection.
            ExhaustedRefillVerdict::Collect | ExhaustedRefillVerdict::Repeat => 2,
        };
        self.young_exhausted_census[slot].fetch_add(1, Ordering::Relaxed);
        granted
    }

    /// Bytes carved out of the young from-space as TLAB chunks since this heap
    /// was built — both arms of [`Self::refill_tlab`] (the main carve and the
    /// fragmentation fallback's mini-TLAB), counted with the size actually
    /// carved. Cumulative and monotone; never reset.
    ///
    /// gen r4w4/alloc4, a cross-lane request from lane `oom` for
    /// `docs/feature-designs/generative-gc-architecture.md`:
    /// the `CRATONVM_DBG_GC_STRESS` arm of `needs_gc_with_jit_allocation_frame`
    /// reads the young `used` figure, which a carve raises by the WHOLE chunk
    /// the moment it is carved. With this counter the stress arm can take
    /// `used - carved` (direct slow-path allocation) and bound the carved part
    /// separately. Because it is cumulative, the consumer snapshots it at its
    /// epoch (the way it already keeps `young_stress_floor`) and reads deltas —
    /// no reset in `collect_garbage_inner` is needed. One relaxed add per
    /// refill, never per object.
    pub fn young_stress_carved(&self) -> u64 {
        self.stats.young_tlab_carved_bytes.load(Ordering::Relaxed)
    }

    /// [`Self::young_stress_carved`] less the tails [`Self::return_tlab_tail`]
    /// took back: the carve bytes still held by TLABs or by what was allocated
    /// in them. Cumulative; both terms only grow and a tail is returned only
    /// after its carve was counted. gen r4w6/tlab6.
    pub fn young_stress_net_carved(&self) -> u64 {
        self.stats
            .young_tlab_carved_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(
                self.stats
                    .young_tlab_tail_returned_bytes
                    .load(Ordering::Relaxed),
            )
    }

    /// Whether this heap takes retiring TLAB tails back
    /// (`CRATONVM_GEN_TLAB_TAIL_SINK`, latched at construction). The VM's refill
    /// site attaches the heap to a fresh buffer only when this is `true`, so
    /// with the flag off no retire ever reaches [`Self::return_tlab_tail`].
    #[inline]
    pub fn tlab_tail_sink_enabled(&self) -> bool {
        self.tlab_tail_sink
    }

    /// Take back the unused tail `[start, end)` of a retiring TLAB this heap
    /// carved, instead of letting `Tlab::retire` bury it under a filler (gen
    /// r4w6/tlab6, opt-in `CRATONVM_GEN_TLAB_TAIL_SINK`). Returns whether the
    /// tail was taken; `false` means the caller writes its filler as before.
    ///
    /// `docs/feature-designs/generative-gc-architecture.md`,
    /// direction 1. Every blocking native, park, sleep and contended
    /// `monitorenter` retires the TLAB; the filler it leaves stays in young
    /// until the next cycle, the young trigger (`used - free_list_bytes`)
    /// counts it as occupied, and the refill carves a fresh chunk — so a thread
    /// that blocks once per unit of work spends up to a carve of young per
    /// unit, however little it allocates. Here the tail goes back instead:
    ///
    /// * **ending at the bump cursor** (the thread carved last, the common case
    ///   for a thread that allocates and then blocks): the cursor retracts onto
    ///   the tail, so `used` falls by the tail and the next carve — this
    ///   thread's or another's — is contiguous with what was used;
    /// * **anywhere else, if at least `min_tlab_size()`**: onto the young free
    ///   list, which the trigger subtracts, the refill's fragmentation fallback
    ///   and slow-path objects reuse, every linear walker skips (they merge
    ///   `free_blocks_sorted`), and a moving cycle's `reset` discards;
    /// * **otherwise** declined: a smaller stray tail is not worth a free-list
    ///   block, and gets the filler.
    ///
    /// # Why this is sound
    ///
    /// * **The bytes are zero.** A TLAB chunk is zero when carved (memset, or
    ///   the bump-tail invariant `zero_young_hand_out` proves) and its owner
    ///   writes only below its cursor, so the tail was never written. A
    ///   retraction therefore keeps "every from-space byte at or above the
    ///   cursor reads zero" — the next carve over it may skip its memset — and
    ///   a listed block is as clean as a swept one. `Arena::return_unused_tail`
    ///   drops the start bit and anchor a whole-chunk return would leave behind.
    /// * **Never blocks.** The retire may run on a path that holds `young_from`
    ///   or races a collection that does; [`Self::try_lock_young_from`] turns
    ///   that into a decline (the filler), never a wait.
    /// * **Only this from-space's bytes.** A tail outside the current
    ///   from-space is declined. A live TLAB never spans a flip: every thread's
    ///   buffer is retired before the pause that swaps the semi-spaces (the
    ///   premise of `ThreadRegistry::tlab_addr`'s safety note).
    /// * **The retiring buffer publishes nothing afterwards.** `Tlab::retire`
    ///   sets `cursor = end` and nulls the buffer right after this returns, and
    ///   never writes the tail; a peer that carves the returned span in the
    ///   meantime owns bytes the retiring thread no longer touches.
    pub fn return_tlab_tail(&self, start: usize, end: usize) -> bool {
        if !self.tlab_tail_sink || end <= start {
            return false;
        }
        let outcome = match self.try_lock_young_from() {
            Some(mut from) => {
                let base = from.base_ptr() as usize;
                if start >= base && end <= base.saturating_add(from.capacity()) {
                    let (off, len) = (start - base, end - start);
                    // gcj j3/yg3: the VM's typed `-XX:MinTLABSize` (8 KiB
                    // untyped, or under `CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR=0`).
                    let min_free = self.min_tlab_floor;
                    // gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`): a
                    // sunk tail reads zero (the sink's contract, checked by
                    // `Tlab::retire` before it offers it), so it need not
                    // forget the sweep's known-zero set.
                    //
                    // gcj j3/x2 R4: `Tlab::retire` reads only the tail's first
                    // and last word in a release build, and a free-listed tail
                    // that joins the set is never memset again -- a stray
                    // write in its middle (a JIT or runtime store past the
                    // TLAB cursor) used to be scrubbed by the next free-list
                    // refill and would now persist. So the keeping door also
                    // samples one word per page of the tail, release builds
                    // included (O(len / 4 KiB) loads), and a dirty sample
                    // takes the plain door: the set is forgotten and every
                    // later refill memsets, as before the carry.
                    // SAFETY: `[start, end)` lies inside the young from-space
                    // (checked above), which the lock held here keeps mapped;
                    // the retiring thread no longer writes the tail.
                    //
                    // t1 wave 8, lane YGZERO (`CRATONVM_GEN_TLAB_TAIL_DIRTY_DECLINES`,
                    // default ON): a dirty sample used to take the plain door,
                    // whose RETRACTION arm puts the tail back above the bump
                    // cursor -- where the zero-once skip
                    // (`zero_young_hand_out`) hands it out un-memset in a
                    // release build -- and whose free-list arm lists bytes the
                    // next sweep's carry cannot tell from zero. Declined, the
                    // retire buries the tail under its filler instead, and the
                    // stray bytes are dead until a pause reclaims (and zeroes)
                    // them. `=0`: the historical doors (no sample without the
                    // carry).
                    let clean = !(self.young_known_zero_carry || self.tlab_tail_dirty_declines)
                        || unsafe { sunk_tail_sample_reads_zero(start as *const u8, len) };
                    if !clean && self.tlab_tail_dirty_declines {
                        static WARNED: AtomicU64 = AtomicU64::new(0);
                        if WARNED.fetch_add(1, Ordering::Relaxed) < 4 {
                            tracing::error!(
                                target: "cratonvm::gc::guard",
                                tail = format!("{start:#x}..{end:#x}"),
                                "[zero-once] a retiring TLAB's unused tail holds a non-zero word \
                                 (a store past the TLAB cursor); declining it to the filler \
                                 instead of returning it to young",
                            );
                        }
                        cratonvm_gc_core::arena::TailReturn::Declined
                    } else if self.young_known_zero_carry && clean {
                        from.return_unused_tail_keeping_known_zero(off, len, min_free)
                    } else {
                        from.return_unused_tail(off, len, min_free)
                    }
                } else {
                    cratonvm_gc_core::arena::TailReturn::Declined
                }
            }
            None => cratonvm_gc_core::arena::TailReturn::Declined,
        };
        let bytes = (end - start) as u64;
        let counter = match outcome {
            cratonvm_gc_core::arena::TailReturn::Retracted => &self.stats.young_tlab_tail_retracts,
            cratonvm_gc_core::arena::TailReturn::FreeListed => &self.stats.young_tlab_tail_free_listed,
            cratonvm_gc_core::arena::TailReturn::Declined => {
                self.stats
                    .young_tlab_tail_declined
                    .fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.stats
            .young_tlab_tail_returned_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        if cratonvm_gc_core::tlab::watch_covers(start, end) {
            cratonvm_gc_core::tlab::watch_note(format_args!(
                "TLAB TAIL returned to young ({outcome:?}): tail=[0x{start:x},0x{end:x})"
            ));
        }
        true
    }

    /// Zero a young hand-out `[ptr, ptr + len)` outside the `young_from` lock,
    /// UNLESS it came off the bump tail — the one zeroing point for that memory
    /// is behind it (gen r4w4/alloc4, `GcFlags::gen_zero_once`, default ON).
    ///
    /// # Why bump-tail memory is already zero (the invariant, by induction)
    ///
    /// Claim: while mutators run, every byte of the young FROM-space at or
    /// above its low bump cursor reads as zero.
    ///
    /// * **Birth.** A young arena's store is zero when created
    ///   (`HeapStore::new`: a lazily committed reservation, or `alloc_zeroed`),
    ///   and a granule committed later reads as zero (`reservation.rs`: "Committed
    ///   memory reads as zero"; a decommitted one reads as zero again when
    ///   re-committed). `Arena::grow` extends into fresh zero memory.
    /// * **While it is from-space**, nothing writes at or above the cursor:
    ///   every writer of young memory is handed its span by `Arena::alloc`,
    ///   which moves the cursor past a bump hand-out before the caller writes
    ///   (TLAB chunks, slow-path objects); a TLAB's owner writes only inside its
    ///   chunk (`Tlab::alloc_initialized`, the JIT inline bump, the tail
    ///   filler); the non-moving sweep frees and zeroes spans BELOW the cursor
    ///   and never retracts it (the generational collector calls none of the
    ///   `retract_cursor_*` doors — ZGC does); `debug_assert_young_arena_is_low_only`
    ///   pins that the young arenas never serve the high end. The one retraction
    ///   a mutator can make (gen r4w6/tlab6, opt-in `CRATONVM_GEN_TLAB_TAIL_SINK`,
    ///   [`Self::return_tlab_tail`]) moves the cursor back over a retiring TLAB's
    ///   UNUSED tail only — bytes zero since their carve, which the owner never
    ///   wrote — so everything at or above the new cursor still reads zero.
    /// * **While it is to-space**, the evacuators write only what they claim
    ///   from its cursor (`young_to.alloc`, the parallel plan's shared
    ///   `to_cursor`, whose end becomes the cursor via
    ///   `commit_parallel_evacuation`); the PLAB gap fillers sit below that end.
    /// * **At the flip**, the evacuated semi-space is reset and zeroed below its
    ///   old cursor and above its old high cursor — by `Arena::reset` (the sync
    ///   and quarantine arms) or by the off-pause wipe of exactly those extents
    ///   (`reset_deferring_zero` + `spawn_evacuated_wipe`, after the give-back
    ///   decommits whole granules, which read as zero on re-commit). Its middle
    ///   was zero by this same claim. The next collection JOINS the wipe
    ///   (`join_evacuated_wipe`, before it locks the arenas) before it copies
    ///   into that space, and only a collection makes it from-space again.
    ///
    /// So a hand-out `hand_out_is_bump_tail` identifies is zero already, and a
    /// second memset of it is pure memory bandwidth on the allocating thread —
    /// and, with `CRATONVM_GEN_UNCOMMIT` on, the refill memset is ALSO what
    /// faults the given-back pages in, so the kernel zeroed them and the
    /// memset zeroed them again (`clear_page_erms` + `memset`, the double
    /// zeroing gc-common's known-zero page measured). The skip leaves the page
    /// fault to the thread's first bump, which pays the kernel's zeroing once.
    ///
    /// # What is NOT skipped
    ///
    /// * A free-list hand-out (reclaimed space below the cursor): it held
    ///   objects, and its zeroing point is here.
    /// * A bump-tail hand-out whose tripwire word was non-zero (`occupied`), or
    ///   whose debug-build sample found a non-zero word: the invariant is broken
    ///   for this span, so it gets the repair memset it always had, and
    ///   `young_zero_repairs` counts it — a non-zero count is a finding.
    /// * Everything, with `CRATONVM_GEN_ZERO_ONCE=0`.
    ///
    /// Under `CRATONVM_DBG_DEADREF_STORE` the refill tripwire has scanned the
    /// whole chunk before this runs, so there a skip is a proof that the memset
    /// would have written only zeros.
    pub fn zero_young_hand_out(
        &self,
        ptr: *mut u8,
        len: usize,
        pristine: Option<(usize, usize)>,
        bump_tail: bool,
        occupied: bool,
    ) {
        if bump_tail {
            // SAFETY: `[ptr, ptr + len)` is the hand-out `Arena::alloc` just
            // carved for this caller alone, committed and 8-aligned.
            let clean = !occupied && unsafe { known_zero_sample_is_clean(ptr, len) };
            if clean {
                self.stats
                    .young_zero_skipped_bytes
                    .fetch_add(len as u64, Ordering::Relaxed);
                return;
            }
            self.stats
                .young_zero_repairs
                .fetch_add(1, Ordering::Relaxed);
            static WARNED: AtomicU64 = AtomicU64::new(0);
            if WARNED.fetch_add(1, Ordering::Relaxed) < 4 {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    chunk = format!("{:#x}+{len:#x}", ptr as usize),
                    "[zero-once] a young hand-out carved from the bump tail was not zero; \
                     something wrote above the from-space cursor, which the one-zeroing-point \
                     invariant (GenerationalHeap::zero_young_hand_out) says nothing may. \
                     Zeroing it; CRATONVM_GEN_ZERO_ONCE=0 restores the unconditional memset.",
                );
            }
        }
        self.stats
            .young_zeroed_bytes
            .fetch_add(len as u64, Ordering::Relaxed);
        // SAFETY: `[ptr, ptr + len)` was carved for this caller alone by
        // `Arena::alloc` (which commits it before handing it out), is 8-aligned
        // at both ends, and cannot be observed moving by any other thread
        // before the caller installs or publishes it — see `refill_tlab`'s note
        // on the lock. The pristine range, when present, is the arena's own
        // record of bytes in this hand-out that were never written.
        unsafe { zero_span_skipping_pristine(ptr, len, pristine) };
    }

    /// The `REFILL_OVER_OBJECT` tripwire for a chunk [`Self::refill_tlab`]
    /// just carved, read BEFORE the chunk is zeroed.
    ///
    /// Split out (gen r4/alloc, 2026-09-23) so that BOTH refill arms run it.
    /// It used to sit inside the main arm only, and the fragmentation
    /// fallback's mini-TLAB — carved from a free-list remnant, i.e. from
    /// exactly the reclaimed memory where a double-issued span is most likely
    /// to be found — was zeroed with no check at all. `arena_base` /
    /// `arena_used` are the arena's state at the carve, captured under the
    /// lock by the caller.
    ///
    /// Returns whether the chunk was found OCCUPIED (a non-zero word), so the
    /// zero-once skip (`zero_young_hand_out`, gen r4w4/alloc4) keeps the
    /// repair memset for exactly the chunks this tripwire fired on.
    pub fn refill_chunk_tripwire(
        ptr: *mut u8,
        actual_size: usize,
        arena_base: usize,
        arena_used: usize,
    ) -> bool {
        // A chunk the arena hands out is unallocated by construction, so its
        // first word is zero. A live class id there means the allocator has
        // handed this thread memory that already holds an object — and the
        // caller's very next statement zeroes it, destroying that object with
        // no other trace. The twin tripwire in `Tlab::install_tail_filler`
        // catches the other half of the same failure (the retire burying what
        // the thread itself allocated); this one catches the arena
        // double-issuing the span. Neither can be inferred from the other.
        if cratonvm_gc_core::tlab::watch_covers(ptr as usize, ptr as usize + actual_size) {
            cratonvm_gc_core::tlab::watch_note(format_args!(
                "REFILL carved a chunk over it: chunk=[0x{:x},0x{:x}) \
                 arena_base=0x{arena_base:x} arena_used=0x{arena_used:x}",
                ptr as usize,
                ptr as usize + actual_size,
            ));
        }
        // SAFETY: `ptr` is `actual_size >= 8` bytes (the caller masks the size
        // to a non-zero multiple of 8) of committed arena memory just carved
        // for the calling thread alone, 8-aligned.
        let mut occupant = unsafe { std::ptr::read_unaligned(ptr as *const u32) };
        // gengc-round1-core (2026-09-20): a separate "found" flag, because
        // `occupant` alone cannot carry the deep scan's verdict. The scan below
        // reported its hit as `w as u32`, and a word whose LOW 32 bits are zero
        // -- a pointer-shaped word, a high-half-only field, an `array_length`
        // in the upper half -- truncates to 0, so the `occupant != 0` report
        // below silently discarded a detection the scan had just made. That is
        // the exact failure the deep scan was added to catch ("it double-issued
        // a chunk that overlaps live objects further along"), so the truncation
        // defeated the whole point of the arm.
        let mut occupied = occupant != 0;
        // The O(1) check above reads the chunk's FIRST word only, which is
        // exactly the word a bump-allocated arena is most likely to have left
        // clean. Under `CRATONVM_DBG_DEADREF_STORE` scan the whole span:
        // unallocated arena memory is zero end to end, so ANY non-zero word in
        // it is an object this refill is about to erase, wherever in the chunk
        // it sits. Two runs apart, that is the difference between "the
        // allocator double-issued this chunk from its base" and "it
        // double-issued a chunk that overlaps live objects further along", and
        // only the second was happening.
        if !occupied && gc_flags().dbg_deadref_store {
            let words = actual_size / 8;
            for i in 0..words {
                // SAFETY: `i < actual_size / 8`, so the word lies inside the
                // chunk carved for this caller; 8-aligned and mapped for its
                // whole length.
                let w = unsafe { std::ptr::read_unaligned((ptr as *const u64).add(i)) };
                if w != 0 {
                    // The low half is what a class id lives in, so it is still
                    // the right thing to REPORT -- it just is not the right
                    // thing to test.
                    occupant = w as u32;
                    occupied = true;
                    break;
                }
            }
        }
        if occupied {
            cratonvm_gc_core::tlab::REFILL_OVER_OBJECT.fetch_add(1, Ordering::Relaxed);
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 8 || n.is_power_of_two() {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    chunk = format!("{:#x}+{actual_size:#x}", ptr as usize),
                    arena_base = format!("{arena_base:#x}"),
                    arena_used = format!("{arena_used:#x}"),
                    occupant_class_id = occupant,
                    occurrence = n + 1,
                    "[tlab-audit] refill_tlab carved a chunk that already holds a \
                     non-zero word (a live class id at its base, or anywhere in it \
                     under CRATONVM_DBG_DEADREF_STORE) — the arena handed out memory \
                     that already holds an object, and the caller's zeroing is about \
                     to destroy it.",
                );
            }
        }
        occupied
    }

    /// The fragmentation fallback of [`Self::refill_tlab`]: `from.alloc` could
    /// not serve `actual_size`, so carve the largest span that CAN be served,
    /// floored at [`cratonvm_gc_core::tlab::frag_tlab_floor`]. Returns the chunk and its
    /// size, or `None` when nothing reaches the floor. Called with the
    /// `young_from` guard held.
    ///
    /// gen r4/alloc (2026-09-23): the candidates are the largest FREE BLOCK
    /// **and the un-bumped tail**. This arm used to consider free blocks only,
    /// and returned `None` outright when none reached the floor — so a
    /// from-space with, say, 100 KiB of bump tail and some sub-floor free-list
    /// dust refused a 256 KiB refill (`actual_size` is sized against the tail
    /// PLUS the dust, so the main arm's request exceeds the tail and fails),
    /// and the caller spilled to old gen or forced a collection while a
    /// perfectly good 100 KiB span sat unused. `Arena::alloc` serves `take`
    /// from the bump tail when no block fits it, which is exactly when the
    /// tail is the larger candidate. This only ever turns a `None` into a
    /// smaller-than-requested chunk; the same arm has always returned those.
    ///
    /// gcd d1/d: `min_size` (0 = none) raises the floor for this call — a
    /// span below it cannot hold the object the refill is for, so it is not
    /// carved (see [`Self::refill_tlab_at_least`]).
    /// gci i22/dr (P2/yy, `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`,
    /// opt-in): serve a young TLAB refill from the SMALLEST free block that
    /// makes a useful TLAB -- at least `tlab::min_tlab_size()` and at least
    /// `min_size` (the object that asked) -- taken WHOLE, or a `actual_size`
    /// head of it when it is larger than the request
    /// (`Arena::take_smallest_block_at_least`). `None` sends the refill to
    /// the default carve ([`Arena::alloc`], then
    /// [`Self::refill_fragmentation_fallback`]): no free block reaches the
    /// floor, or the request itself is below it.
    ///
    /// `gch-h1yy-proposal-refill-takes-the-smallest-whole-free-block-20261008`:
    /// the default carve best-fits the REQUEST (up to 1 MiB), so it cuts the
    /// large spans a sweep has just coalesced while the medium holes wait;
    /// this is best fit by SIZE, the policy a non-moving young needs because
    /// it cannot compact. The chunk is a free-list hand-out either way, so the
    /// caller's anchor, tripwire and zeroing (a reuse is memset) are unchanged.
    /// Counted in `[GC] young_premium:` (`yp_refill_whole_below_request` = the
    /// refills whose chunk differs from the default arm's).
    pub fn refill_smallest_whole_block(
        &self,
        from: &mut Arena,
        actual_size: usize,
        min_size: usize,
    ) -> Option<(*mut u8, usize)> {
        // gcj j3/yg3: the VM's typed `-XX:MinTLABSize`, not the 8 KiB
        // constant (`Self::min_tlab_floor`; equal to it when untyped).
        let floor = self.min_tlab_floor.max(min_size);
        if floor > actual_size {
            return None;
        }
        let (ptr, taken) = from.take_smallest_block_at_least(floor, actual_size)?;
        let slot = if taken < actual_size {
            YP_REFILL_WHOLE_BELOW_REQUEST
        } else {
            YP_REFILL_AT_REQUEST
        };
        self.young_premium_census[slot].fetch_add(1, Ordering::Relaxed);
        Some((ptr, taken))
    }

    pub fn refill_fragmentation_fallback(
        from: &mut Arena,
        actual_size: usize,
        min_size: usize,
    ) -> Option<(*mut u8, usize)> {
        // Fragmentation fallback (the bimodal-bt18 wedge): no single span fits
        // `actual_size`, but a smaller one can still make a useful TLAB.
        // Steady-state refills carve exact `requested_size` chunks out of the
        // sweep's coalesced spans; the split leftovers converge on blocks a
        // few bytes SHORT of the adaptive sizer's request (observed: a ~2 GiB
        // free list made entirely of 131056-byte blocks vs. a 131072-byte
        // request). Without this fallback the refill gate then fails on every
        // allocation while tiny object allocations keep succeeding off the
        // remnants — so the young collection (whose coalescer would heal the
        // fragmentation) is never triggered either, and the entire rest of
        // the run crawls through the per-object slow path (bt18: 1.9s → 4.5s
        // on a ~50% coin flip of whether the post-sweep workload outlasted
        // the recycled spans). Serving the largest available block (capped at
        // the request, floored at a useful TLAB size) keeps the remnants
        // flowing through the bump fast path instead; the cost — one
        // O(free-list) largest-block scan — is paid once per served TLAB,
        // not per object.
        //
        // gengc-round1-core (2026-09-20): "once per served TLAB" was only true
        // of the arm that SERVES. The scan also ran on every refill this
        // function goes on to REFUSE (`largest < floor`, or the `from.alloc`
        // below failing) — and that is the badly-fragmented state in which the
        // JIT falls back to per-object slow-path allocation and calls
        // `refill_tlab` again, so the scan became per-allocation in exactly
        // the pathology `try_alloc_young_probe` documents replacing. Gate it
        // on the amortized-O(1) cached-bound probe first: if no block can
        // reach the floor there is nothing for the scan to find, and the
        // refusal is the same.
        let floor = cratonvm_gc_core::tlab::frag_tlab_floor().max(min_size);
        // The un-bumped middle (`capacity - used` is exactly that, see
        // `Arena::used`), on the 8-byte grid. See the method doc.
        let bump_tail = from.capacity().saturating_sub(from.used()) & !7;
        let free_block_reaches_floor = from.has_free_block_at_least(floor);
        if !free_block_reaches_floor && bump_tail < floor {
            return None;
        }
        let largest_free = if free_block_reaches_floor {
            from.largest_free_block()        } else {
            0
        };
        let largest = largest_free.max(bump_tail);
        // Second-wedge fix (perf/halfgap-20260717): the floor here must match
        // the guarded-refill gate's fragmentation floor, and both must sit
        // BELOW the sizes steady-state splitting converges on. With the old
        // `min_tlab_size().max(256)` (= 8192) floor, a free list that had
        // degraded to 4080-byte remnants (8 KiB splits minus header slack)
        // wedged BOTH this fallback and the gate shut — see
        // `tlab::FRAG_TLAB_FLOOR` for the live BinTrees capture. A ~4 KB
        // mini-TLAB still serves ~100 small objects at bump speed, vastly
        // outperforming the per-object slow path this None would otherwise
        // condemn every allocation to.
        if largest < floor {
            return None;
        }
        // `& !7`: same TLAB-size alignment invariant as the main path. The
        // anchor, the tripwire and the zeroing are the caller's, shared with
        // the main arm.
        let take = largest.min(actual_size) & !7;
        let ptr = from.alloc(take, 8)?;
        Some((ptr, take))
    }

    /// Allocate bytes in the young from-space and initialize the object before
    /// publishing it.
    ///
    /// If the from-space is full, logs a fatal error and aborts. The
    /// interpreter's `gc_alloc_*` functions use fallible allocation with
    /// GC-and-retry; this method is only called by the panicking
    /// `alloc_object`/`alloc_array` convenience wrappers.
    pub fn alloc_young_initialized<F>(&self, size: usize, init: F) -> *mut u8
    where
        F: FnOnce(*mut u8),
    {
        self.try_alloc_young_initialized(size, init)
            .unwrap_or_else(|| {
                self.report_fatal_oom(size);
                std::process::abort();
            })
    }

    /// The `[GC] gen-alloc:` shutdown line: which door this heap's
    /// allocations went through, beside the TLAB refill outcomes.
    ///
    /// gen r4w3/alloc3 (2026-09-23). The Generational summary printed no
    /// allocation-path count at all: `HeapStats::young_allocations` (objects
    /// that missed the TLAB and took the young slow path) and
    /// `old_allocations` (humongous arrays and young-exhaustion spills) had
    /// no reader but `publish_gc_metrics_occupancy`, and the refill census
    /// (`gc_entry_census::refill_totals`) is printed only in the ZGC arm. So
    /// the three opt-in TLAB arms of this wave (`CRATONVM_TLAB_SIZE_RETIRED` /
    /// `_WASTE_SHRINK`, `_GATE_BUMP_FLOOR`) could not be A/B'd on what they
    /// change — fewer slow-path objects and old spills for the gate, fewer
    /// refills for the sizer — from a Generational log. A pull, formatted
    /// here; printing it is `VmHeap::print_gc_summary`'s (cross-lane request
    /// in `docs/feature-designs/generative-gc-architecture.md`).
    ///
    /// The two `gen_*` counts are this heap's. The four `tlab_refill_*` counts
    /// are process-wide (every backend's refill site feeds them), as on the
    /// ZGC line. Keys are prefixed so none collides with another summary
    /// line's.
    pub fn alloc_path_census_line(&self) -> String {
        let s = self.stats.snapshot();
        let (attempts, ok, _) = cratonvm_types::gc_entry_census::refill_totals();
        let (retries, retry_ok) = cratonvm_types::gc_entry_census::refill_retry_totals();
        let tenuring = self.tenuring_census();
        // gcf f1/p: the inline card check's engagement (cards4), counted in the
        // card view by compiled code under CRATONVM_DBG_{SP,IR}_REF_STORE_TRACE=1.
        let card = self.card_table.jit_card_check_trace();
        // gci i23/lc: the single-pass check's skip edges (the denominator of a
        // zero `gen_jit_card_sp_*`) and the lean POST's one-compare answers.
        let skip = self.card_table.jit_card_skip_trace();
        let lean_post = self.card_table.jit_card_lean_post_trace();
        // gcj j2/jt2: post-only card sites (`CRATONVM_JIT_POST_ONLY_CARD_MARK`) and
        // the register words the take-over's completion dirtied for them.
        let post_only_sites = self.card_table.jit_post_only_card_sites();
        let post_only_completed = self.card_table.jit_post_only_card_completions();
        // gen r4w4/alloc4: the zero-once skip's engagement (`gen_zero_skipped_bytes`
        // against `gen_zeroed_bytes`; `gen_zero_repairs` non-zero is a finding)
        // and the humongous outside-the-lock memset count.
        format!(
            "[GC] gen-alloc: gen_young_slow_allocs={} gen_old_allocs={} \
             tlab_refill_attempts={attempts} tlab_refill_ok={ok} \
             tlab_refill_retries={retries} tlab_refill_retry_ok={retry_ok} \
             gen_tlab_carved_bytes={} gen_zero_skipped_bytes={} gen_zeroed_bytes={} \
             gen_zero_repairs={} gen_humongous_unlocked_zeroes={} \
             gen_tlab_tail_sink={} gen_tlab_tail_returned_bytes={} gen_tlab_tail_retracts={} \
             gen_tlab_tail_free_listed={} gen_tlab_tail_declined={} \
             gen_zeroed_reuse_bytes={} gen_lazy_dirty_zeroed_bytes={} \
             gen_jit_card_sp_dirty={} gen_jit_card_sp_clean={} \
             gen_jit_card_ir_dirty={} gen_jit_card_ir_clean={} \
             gen_jit_card_sp_skip_young={} gen_jit_card_sp_skip_null={} \
             gen_jit_card_sp_skip_oldval={} gen_jit_card_sp_skip_outside={} \
             gen_jit_card_sp_lean_post={} \
             gen_jit_card_post_only_sites={post_only_sites} \
             gen_jit_card_post_only_completed={post_only_completed} \
             gen_objects_copied_young={} gen_bytes_copied_young={} \
             gen_objects_promoted={} gen_bytes_promoted={} gen_tenuring_threshold={} \
             gen_tenuring_adapted={} gen_tenuring_raised={} gen_tenuring_lowered={} \
             gen_refill_exhausted_enabled={} gen_refill_exhausted_collects={} \
             gen_refill_exhausted_no_progress={} gen_refill_exhausted_repeat={}",
            s.young_allocations,
            s.old_allocations,
            s.young_tlab_carved_bytes,
            s.young_zero_skipped_bytes,
            s.young_zeroed_bytes,
            s.young_zero_repairs,
            s.humongous_unlocked_zeroes,
            // gen r4w6/tlab6: the Generational tail sink's engagement.
            // `gen_tlab_tail_sink=0` says the four after it cannot move.
            u8::from(self.tlab_tail_sink),
            s.young_tlab_tail_returned_bytes,
            s.young_tlab_tail_retracts,
            s.young_tlab_tail_free_listed,
            s.young_tlab_tail_declined,
            // gce e1/o: the reuse share of `gen_zeroed_bytes` (see the stat).
            s.young_zeroed_reuse_bytes,
            s.young_lazy_dirty_zeroed_bytes,
            card[0],
            card[1],
            card[2],
            card[3],
            skip[0],
            skip[1],
            skip[2],
            skip[3],
            lean_post,
            // gcf f1/y: what young copied young-to-young over the run. On a
            // probe whose promoted set is fixed by construction (every
            // promoted byte of `GenR4W4SurvivorOverflowProbe` is its spike
            // set, which outlives any threshold <= 15), adaptive tenuring can
            // only move the COPIES (a lower threshold tenures the spike after
            // fewer semi-space copies), so its item 4 reads these two.
            s.objects_copied_young,
            s.bytes_copied_young,
            // gce e2/y: the adaptive-tenuring gate's deciding counters
            // (`CRATONVM_GC_ADAPTIVE_TENURING`, item 4 of its flip gate): what
            // young promoted over the run, the threshold the NEXT cycle uses
            // (HotSpot units), and the adaptation census.
            s.objects_promoted,
            s.bytes_promoted,
            self.hotspot_tenuring_threshold(),
            tenuring[0],
            tenuring[1],
            tenuring[2],
            // gci i4/yg: the exhausted-refill collection's engagement
            // (`CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`).
            u8::from(self.young_exhausted_refill_collects),
            self.young_exhausted_census[0].load(Ordering::Relaxed),
            self.young_exhausted_census[1].load(Ordering::Relaxed),
            self.young_exhausted_census[2].load(Ordering::Relaxed),
        )
    }

    /// Print the full generational picture behind a "both generations are
    /// exhausted" abort.
    ///
    /// The historical one-line message named only the young from-space, which
    /// reads as "the young gen is too small" even when the actual blocker is a
    /// full (or badly fragmented) old generation — the spill target every
    /// panicking allocator tries before it reaches here. Naming old-gen
    /// occupancy, its largest free block, and the GC counters separates the
    /// three very different situations that land on this line: a genuinely
    /// full heap, a heap that is mostly garbage but was never collected
    /// (`minor_gc` stuck), and an old gen with free bytes but no block large
    /// enough for the request.
    pub fn report_fatal_oom(&self, size: usize) {
        let (yf_used, yf_cap) = {
            let f = self.lock_young_from();
            (f.used(), f.capacity())
        };
        let (yt_used, yt_cap) = {
            let t = self.young_to.lock();
            (t.used(), t.capacity())
        };
        let (og_used, og_cap, og_largest, og_blocks) = {
            let og = self.old_gen.lock();
            (
                og.used(),
                og.capacity(),
                og.largest_free_block(),
                og.free_block_count(),
            )
        };
        let s = self.stats.snapshot();
        let committed = yf_cap + yt_cap + og_cap;
        let budget = if self.max_heap_bytes == usize::MAX {
            "unbounded (explicit arena sizes)".to_string()
        } else {
            format!("{}", self.max_heap_bytes)
        };
        eprintln!(
            "FATAL: OutOfMemoryError: young gen exhausted — tried to allocate {} bytes, \
             from-space has {}/{} used",
            size, yf_used, yf_cap,
        );
        eprintln!(
            "FATAL-OOM budget: committed heap {committed} bytes \
             (young_from + young_to + old_gen) against an -Xmx budget of {budget}",
        );
        eprintln!(
            "FATAL-OOM detail: young_to {}/{} used; old_gen {}/{} used \
             (largest free block {}, {} free blocks); minor_gc={} major_gc={} \
             promoted_bytes={} old_allocs={} young_allocs={}",
            yt_used,
            yt_cap,
            og_used,
            og_cap,
            og_largest,
            og_blocks,
            s.minor_gc_count,
            s.major_gc_count,
            s.bytes_promoted,
            s.old_allocations,
            s.young_allocations,
        );
        // Opt-in: the allocating call chain. Release builds carry line tables,
        // so this names the exact panicking-allocator caller — i.e. which
        // native / VM-internal path could not tolerate a GC-and-retry.
        if false {
            eprintln!(
                "FATAL-OOM backtrace:\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
    }

    /// Generate the next identity hash code.
    ///
    /// H1: exposed publicly so `interpreter::init_object_header` (TLAB
    /// fast path) can mint a unique hash at allocation time, matching the
    /// non-TLAB allocators. Without this, freshly TLAB-allocated objects
    /// of `ClassId(0)` (java/lang/Object) with no fields produce an
    /// all-zero header that the stale-pointer detector mis-flags as
    /// stale.
    ///
    /// # Why this hands out per-thread BLOCKS
    ///
    /// This is on the *interpreted allocation* path — every `new` calls it
    /// once. A process-shared atomic read-modify-write there costs a cache-line
    /// ping-pong per allocation under any concurrency, and it is one of three
    /// such counters on a path whose HotSpot equivalent is roughly five
    /// instructions end to end. Claiming [`IDENTITY_HASH_BLOCK`] values with a
    /// single `fetch_add` and serving the rest from a thread-local cursor
    /// removes 63 of every 64 RMWs; the remaining cost is one `Cell` read, a
    /// compare and a `Cell` write.
    ///
    /// What the callers actually require of this value, and what still holds:
    ///
    /// * **Non-zero.** `0` means "no hash minted yet" to the stale-pointer
    ///   detector and to `identity_hash_code`'s CAS-from-zero. The counter is
    ///   an `i32` and wraps, so the one value that would ever be `0` is stepped
    ///   over explicitly below (`mint_identity_hash_code` also maps a `0` to
    ///   `i32::MAX` — belt and braces, kept).
    /// * **Distinct per object, in practice.** Blocks are disjoint by
    ///   construction, so two threads can never serve the same value.
    /// * **Increasing within one thread.** Preserved: a thread's cursor only
    ///   ever moves forward, and a new block starts above every block handed
    ///   out so far. Cross-thread ordering was never guaranteed by the old
    ///   `fetch_add` either — two threads racing it observed no defined order.
    ///
    /// Waste is bounded by `IDENTITY_HASH_BLOCK` values per thread that ever
    /// allocates, i.e. `2^31 / 64` ≈ 33 million threads before the space is
    /// exhausted any faster than it already was.
    ///
    /// The cursor is per-*thread*, not per-heap: a process with two
    /// `GenerationalHeap`s (unit tests, embedders) can serve one heap's `new`
    /// from a block claimed against the other. That is harmless — the value's
    /// only contracts are the three above — but it is why this must not be
    /// used as a per-heap allocation counter.
    pub fn next_hash(&self) -> i32 {
        IDENTITY_HASH_CURSOR.with(|c| {
            let (next, remaining) = c.get();
            let (mut value, mut left) = if remaining > 0 {
                (next, remaining)
            } else {
                (
                    self.next_hash_code
                        .fetch_add(IDENTITY_HASH_BLOCK, Ordering::Relaxed),
                    IDENTITY_HASH_BLOCK,
                )
            };
            if value == 0 {
                // Step over the reserved sentinel rather than returning it.
                // `left` may go to 0 (or below) here; the `remaining > 0` test
                // above simply claims a fresh block on the next call.
                value = 1;
                left -= 1;
            }
            c.set((value.wrapping_add(1), left - 1));
            value
        })
    }

    /// `CRATONVM_DBG_ROOT_REMAP_AUDIT`: what the object-start walk that built
    /// THIS cycle's bitmap actually did.
    ///
    /// A refusal names an address the bitmap does not hold; whether that is the
    /// walk's fault needs the walk's own numbers, and by the time
    /// `forward_object` runs they are three hundred lines out of scope. Stored
    /// rather than printed: it would otherwise be one line per collection on a
    /// run that does seven thousand of them.
    ///
    /// gen r4/sweep (2026-09-23): `forward_object`'s doc comment and its
    /// `#[allow(clippy::too_many_arguments)]` used to sit ABOVE this function
    /// (a mis-merge), so rustdoc showed "Forward (copy or promote) a single
    /// object" as this function's summary and the lint allow landed on a
    /// one-argument function while the nine-argument one went without. Both
    /// moved back onto `forward_object`.
    ///
    /// gcd d3/m: stored in THIS heap's refusal ledger (it was the process
    /// static `LAST_OBJSTART_WALK`).
    pub fn note_objstart_walk(&self, summary: String) {
        if !false {
            return;
        }
        *self.forward_refusals.objstart_walk.lock() = Some(summary);
    }

    /// `CRATONVM_DBG_ROOT_REMAP_AUDIT`: the evacuator REFUSED to move a root.
    ///
    /// `forward_object_impl` has three refusal paths and every one of them
    /// returns `old_ptr` unchanged, which `seed_roots` writes straight back
    /// into the root slot. On the moving path that is indistinguishable from a
    /// successful no-op: the object is left in from-space, gets no pointer-map
    /// entry, and from-space is then reset -- so the slot dangles and nothing
    /// downstream can say the evacuator declined it rather than the scan
    /// missing it.
    ///
    /// The BindableTests residual is exactly that shape: `in_root_set=true`,
    /// `scan_would_root=true`, and no pointer-map entry.
    ///
    /// `ledger` is the collecting heap's refusal ledger (gcd d3/m; `None` only
    /// for a unit test driving the evacuator without a heap cycle).
    #[cold]
    pub fn note_forward_refusal(
        ledger: Option<&ForwardRefusalLedger>,
        old_ptr: *mut u8,
        reason: &'static str,
        young_from: &Arena,
        starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
    ) {
        // The ledger is recorded whether or not the printout is armed: the
        // post-GC verifier that reads it must be able to answer on a FIRST
        // occurrence, on a run nobody thought to pre-arm a flag for.
        if let Some(ledger) = ledger {
            ledger.record(old_ptr as usize, reason);        }
        if !false {
            return;
        }
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 24 {
            let base = young_from.base_ptr() as usize;
            let (bm_base, bm_span) = starts.extent();
            let off = (old_ptr as usize).wrapping_sub(base);
            let in_free_block = young_from
                .free_blocks_sorted()
                .iter()
                .any(|(o, sz)| off >= *o && off < *o + *sz);
            eprintln!(
                "[forward-refused] {reason} old_ptr=0x{:x} (#{n}) off=0x{off:x} \
                 arena=[0x{base:x} cap=0x{:x} used=0x{:x}] bitmap=[0x{bm_base:x} span=0x{bm_span:x}] \
                 off_beyond_bitmap={} in_free_block={in_free_block} -- the root stays in \
                 from-space with no pointer-map entry, and from-space is about to be reset",
                old_ptr as usize,
                young_from.capacity(),
                young_from.used(),
                off >= bm_span,
            );
            // The header is still intact here: from-space is not reset until
            // after the copy phase. A plausible header at `old_ptr` says the
            // walk missed a real object; an implausible one says the reference
            // was already wrong before this cycle, and the refusal is right.
            let near = starts.nearest_start_at_or_below(old_ptr as usize);
            // SAFETY: `old_ptr` is inside from-space (checked by the caller),
            // which is mapped and not yet reset; two byte reads and a u32 read
            // at the fixed header offsets are in bounds.
            let (kind_tag, elem_tag, cid) = unsafe {
                (
                    cratonvm_types::kind_tag_at(old_ptr),
                    cratonvm_types::element_type_tag_at(old_ptr),
                    std::ptr::read_unaligned(old_ptr as *const u32),
                )
            };
            eprintln!(
                "[forward-refused] ^ header_at_old_ptr: class_id={cid} kind_tag={kind_tag} \
                 elem_tag={elem_tag} nearest_recorded_start={} delta={}",
                near.map(|a| format!("0x{a:x}"))
                    .unwrap_or_else(|| "none".into()),
                near.map(|a| (old_ptr as usize).saturating_sub(a) as i64)
                    .unwrap_or(-1),
            );
            if let Some(w) = ledger.and_then(|l| l.objstart_walk.lock().clone()) {
                eprintln!("[forward-refused] ^ objstart_walk: {w}");
            }
            // THE OBJECT THE WALK STRODE OVER THIS ADDRESS WITH.
            //
            // The refused address is either an INTERIOR word of the object at
            // `nearest_recorded_start` -- in which case the reference itself is
            // wrong and this refusal is correct -- or the walk mis-sized that
            // object and strode over live ones. Its class, kind and computed
            // stride say which, and it is the one fact the census could not
            // supply.
            if let Some(near) = near {
                if near != old_ptr as usize {
                    // SAFETY: `near` is an object start this cycle's own walk
                    // recorded, inside from-space, which is mapped and not yet
                    // reset.
                    let h = unsafe { &*(near as *const ObjectHeader) };
                    eprintln!(
                        "[forward-refused] ^ object_at_nearest_start: addr=0x{near:x} class_id={} kind={:?} num_slots={} array_length={} stride=0x{:x} covers_refused={}",
                        h.class_id.as_u32(),
                        h.kind(),
                        h.num_slots(),
                        h.array_length(),
                        gen_object_total_size(h),
                        near + gen_object_total_size(h) > old_ptr as usize,
                    );
                }
            }
        }
    }

    /// gcd d2/h (2026-09-27), `gengc-r5w6-sizer10-refused-young-root-dangles`:
    /// does a PRECISE root name from-space at an address this cycle's
    /// object-start walk did not record? If so, should the cycle be refused?
    ///
    /// `forward_object_impl`'s first refusal exit ("not-an-object-start")
    /// returns such an address unmoved and records no `pointer_map` entry, and
    /// the cycle then resets from-space: the root is left naming zeroed
    /// memory, and whatever the object reached dies with it. Every other way a
    /// moving cycle can fail to know an object fails CLOSED (an incomplete
    /// walk, a reserved tail, a failed parallel chunk); this was the one
    /// place a precise root could dangle.
    ///
    /// The caller asks only on a cycle whose root set is precise: no compiled
    /// frame is live (`compiled_frames_live()` false, so neither the
    /// conservative JIT scan nor the conservative interpreter-local probe fed
    /// `roots`), and no pin words were injected. An unrecorded from-space
    /// address among such roots is therefore not an interior word of a
    /// conservative scan: it is either an object the walk skipped (under a
    /// free block, behind a filler) or a root that was already stale. Asked
    /// right after the walk, before any copy and before the dirty-card take,
    /// so refusing costs exactly the refused-walk exit's price (over-retain
    /// for a cycle; nothing destructive has happened). gci i10/gr: and, under
    /// `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`, again right after the take
    /// over the dirty-card and overlay values (a refusal there re-dirties the
    /// consumed cards first; `gen_heap_gci_i10gr.rs`). The warn below then
    /// says "a precise root" for such a value too; the
    /// `[unrecorded-young-card-root]` line that follows names its source.
    ///
    /// # Bounded
    ///
    /// A cycle refuses only when the lowest offending address differs from
    /// the one that refused the previous time ([`Self::last_unrecorded_root`]).
    /// A stale root that persists would otherwise refuse every young
    /// collection for the rest of the run (the objection the
    /// "invalid-kind-or-element-tag" exit records against failing closed);
    /// with the latch it costs ONE refused cycle, after which the cycle runs
    /// and the old fail-open exit (recorded, `[forward-refused]`) applies.
    ///
    /// Census `[cycles_refused, failed_open, roots_sum]`: `roots_sum` counts
    /// every offending root seen. All three are ZERO on a healthy run; a
    /// non-zero reading is a finding, and the warn below names the first
    /// address with its header evidence.
    pub fn unrecorded_young_root_refuses_cycle(
        &self,
        roots: &[ObjectRef],
        young_from: &Arena,
        starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
        exclude: &FxHashSet<usize>,
    ) -> bool {
        let mut lowest: Option<usize> = None;
        let mut count = 0u64;
        // gch h1/yp: offenders the exclusion set absorbed. Their only effect is
        // a refusal NOT taken, so this is the engagement-with-effect figure of
        // the exclusion switches (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`,
        // `_LEDGER`), readable on a single run without a refusing control.
        let mut excluded = 0u64;
        for r in roots {
            let a = r.as_ptr() as usize;
            // gcf f4/y: `exclude` is the cycle's known conservative population
            // (empty on a precise-root cycle).
            if young_from.contains(a as *const u8) && !starts.contains(a) {
                if exclude.contains(&a) {
                    excluded += 1;
                } else {
                    count += 1;
                    lowest = Some(lowest.map_or(a, |l| l.min(a)));
                }
            }
        }
        let c = &self.unrecorded_root_census;
        if excluded > 0 {
            c[4].fetch_add(1, Ordering::Relaxed);
            c[5].fetch_add(excluded, Ordering::Relaxed);
        }
        let Some(addr) = lowest else {
            // gch h2/yg: a decision that refuses nothing ends the streak.
            c[6].store(0, Ordering::Relaxed);
            return false;
        };
        c[2].fetch_add(count, Ordering::Relaxed);
        let refuse = unrecorded_root_refusal_decision(
            self.last_unrecorded_root.swap(addr, Ordering::Relaxed),
            addr,
        );
        c[if refuse { 0 } else { 1 }].fetch_add(1, Ordering::Relaxed);
        // gch h2/yg (`gch-h1yp-unrecorded-root-latch-does-not-bound-a-stream-of-distinct-stale-words`):
        // the latch bounds one address, not a stream whose lowest member
        // changes; the longest run of consecutive refusals says whether such a
        // stream exists. Only the collecting thread decides, so load + store.
        let (now, max) = unrecorded_root_refusal_streak(
            c[6].load(Ordering::Relaxed),
            c[7].load(Ordering::Relaxed),
            refuse,
        );
        c[6].store(now, Ordering::Relaxed);
        c[7].store(max, Ordering::Relaxed);
        // Rate-limited on this heap's own count (no process static): the
        // first eight and then every power of two.
        let seen = c[0].load(Ordering::Relaxed) + c[1].load(Ordering::Relaxed);
        if seen <= 8 || seen.is_power_of_two() {
            let base = young_from.base_ptr() as usize;
            let used = young_from.used();
            let off = addr - base;
            // Header evidence only for a whole header below `used`: the
            // arena commits lazily, so a byte at or past the frontier may not
            // be mapped.
            let header = (off + HEADER_SIZE <= used).then(|| {
                // SAFETY: `addr` is 8-aligned (an `ObjectRef`) and
                // `[addr, addr + HEADER_SIZE)` lies below the allocation
                // frontier of from-space, which is committed and not yet reset
                // (no copy has run this cycle). Two plain loads of the header's
                // own fields; no reference outlives the block.
                unsafe {
                    let h = &*(addr as *const ObjectHeader);
                    (h.class_id.as_u32(), h.mark_word.load(Ordering::Relaxed))
                }
            });
            let near = starts
                .nearest_start_at_or_below(addr)
                .map(|a| format!("0x{a:x}"))
                .unwrap_or_else(|| "none".into());
            // gcg g1/j: which VM root source handed this address over, when
            // `CRATONVM_DBG_ROOT_SOURCE` is on (`None`: no VM source -- a
            // thread snapshot, frames or JNI locals).
            let src = cratonvm_gc_core::gc_quiescence::root_source_of(addr);
            tracing::warn!(
                "[unrecorded-young-root] a precise root names from-space at 0x{addr:x} \
                 (off=0x{off:x} used=0x{used:x}), which this cycle's object-start walk did not \
                 record; {count} such root(s) this cycle; header(class_id, mark)={header:?} \
                 nearest_recorded_start={near} root_source={src:?}. {} (occurrence #{seen}; see \
                 gengc-r5w6-sizer10-refused-young-root-dangles)",
                if refuse {
                    "REFUSING this young collection (over-retain; retried next cycle)"
                } else {
                    "Same address as the last refusal: running the cycle, the root stays \
                     unmoved (fail-open, recorded as a forward refusal)"
                },
            );
        }
        refuse
    }

    /// [`Self::unrecorded_young_root_refuses_cycle`]'s census over this heap's
    /// life: `[cycles_refused, failed_open, roots_sum]`. gcd d2/h (2026-09-27).
    ///
    /// * `cycles_refused` — moving cycles skipped (decision
    ///   `skipped-young-walk-incomplete`) because a precise root named an
    ///   unrecorded from-space address;
    /// * `failed_open` — cycles that met the SAME lowest address as the last
    ///   refusal and ran anyway (the address stays unmoved, as before);
    /// * `roots_sum` — every such root seen, both kinds of cycle.
    ///
    /// All three are zero on a healthy run.
    pub fn unrecorded_young_root_census(&self) -> [u64; 3] {
        std::array::from_fn(|i| self.unrecorded_root_census[i].load(Ordering::Relaxed))
    }

    /// gcf f4/y: moving cycles WITH a live compiled frame on which the
    /// unrecorded-root guard ran, excluding the ledger's known conservative
    /// words (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_LEDGER`). Zero with the
    /// switch off -- the engagement number for that switch.
    pub fn unrecorded_young_root_ledger_guarded_cycles(&self) -> u64 {
        self.unrecorded_root_census[3].load(Ordering::Relaxed)
    }

    /// gch h1/yp: `[excluded_cycles, excluded_roots_sum]` -- guarded cycles on
    /// which the exclusion set (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`'s
    /// deposit words, `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_LEDGER`'s ledger
    /// words) held a from-space root the object-start walk did not record,
    /// and how many such roots. Each is a word that would otherwise have
    /// counted toward a refusal. Zero with both switches off (the exclusion
    /// set is then empty). `gcg-g5jn-unrecorded-root-guard-reads-a-conservative-deposit-as-precise`.
    pub fn unrecorded_young_root_exclusion_census(&self) -> [u64; 2] {
        [
            self.unrecorded_root_census[4].load(Ordering::Relaxed),
            self.unrecorded_root_census[5].load(Ordering::Relaxed),
        ]
    }

    /// gch h2/yg: the longest run of consecutive guard decisions that refused
    /// their cycle, over this heap's life. The same-address latch caps one
    /// persistent stale word at 1; a value above 2 is the stream of distinct
    /// stale words `gch-h1yp-unrecorded-root-latch-does-not-bound-a-stream-of-distinct-stale-words`
    /// describes (each refused cycle is a SKIPPED young collection, which
    /// HotSpot Serial never does). Zero on a healthy run.
    pub fn unrecorded_young_root_max_streak(&self) -> u64 {
        self.unrecorded_root_census[7].load(Ordering::Relaxed)
    }

    /// gci i10/gr: moving cycles on which the dirty-card / overlay value guard
    /// ran (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`). Zero with the switch
    /// off -- the engagement number for that switch.
    pub fn unrecorded_young_root_cards_guarded_cycles(&self) -> u64 {
        self.unrecorded_root_census[8].load(Ordering::Relaxed)
    }

    /// gch h2/yg: one relocating young cycle's blocked-peer capture held
    /// `words` young non-start words; `pinned` says whether the cycle's plan
    /// took them (`plan_takes_blocked_peer_words`).
    pub fn note_blocked_peer_interior(&self, words: usize, pinned: bool) {
        let c = &self.blocked_peer_interior_census;
        c[BPI_CYCLES].fetch_add(1, Ordering::Relaxed);
        if words > 0 {
            c[BPI_CYCLES_WITH_WORDS].fetch_add(1, Ordering::Relaxed);
            // Widening: a capture holds at most `PEER_STACK_SLOTS_CAP` words.
            c[BPI_WORDS_SUM].fetch_add(words as u64, Ordering::Relaxed);
            if pinned {
                c[BPI_CYCLES_PINNED].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// gch h2/yg (`gcd-d1e-proposal-every-moving-cycle-pins-blocked-peer-interior-words`):
    /// `[cycles, cycles_with_words, words_sum, cycles_pinned]` over this
    /// heap's life -- relocating young cycles whose blocked-peer native-stack
    /// capture was examined, those whose capture held a young word that is not
    /// an object start (the blocked-wake fold rewrites bases only), the sum of
    /// such words, and the cycles whose plan pinned them (the pinned copy or
    /// option B). `cycles_with_words - cycles_pinned` is how many cycles
    /// relocated under a word nobody rewrites (0 on every vh2-yg row).
    pub fn blocked_peer_interior_census(&self) -> [u64; BPI_LEN] {
        std::array::from_fn(|i| self.blocked_peer_interior_census[i].load(Ordering::Relaxed))
    }

    /// gch h2/yg: the pinned copy's `ledger_incomplete` declines on which a
    /// thread of the VM held raw JNI local references (a subset of
    /// `pinned_young_census()[3]`).
    /// `gch-h1yp-raw-jni-locals-count-turns-the-pinned-copy-off-for-the-vms-life`.
    pub fn pinned_ledger_incomplete_raw_jni(&self) -> u64 {
        self.pinned_ledger_incomplete_raw_jni
            .load(Ordering::Relaxed)
    }

    /// Why THIS heap's evacuator declined to relocate `addr` on its most
    /// recent moving young cycle, if it declined at all.
    ///
    /// `Some(reason)` means the collector WAS handed this address and refused
    /// it: the object was not reclaimed out from under the holder, the address
    /// was never a live object start to begin with. `None` means it was never
    /// offered — a genuinely missing root, which is the opposite defect and
    /// the opposite fix.
    ///
    /// `POST-GC RECLAIMED-WHILE-HELD` printed both cases identically until this
    /// existed, and separating them took the whole `[forward-refused]` chain by
    /// hand (see the BindableTests residual page, 2026-09-09).
    ///
    /// gcd d3/m (2026-09-27): a method of the heap. It was the free function
    /// `gen_heap::forward_refusal_reason` over a process static, so another
    /// Generational VM's moving cycle could clear this heap's answer before
    /// its verifier read it (`gcd-d2h-forward-refusal-ledger-is-process-global-FIXED-20260928`).
    pub fn forward_refusal_reason(&self, addr: usize) -> Option<&'static str> {
        self.forward_refusals.reason(addr)
    }

    /// gcd d3/m: the pinned copy's take-over arm
    /// (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`, opt-in), over this heap's
    /// life: `[candidates, taken, ledger_incomplete, over_bound]`.
    ///
    /// * `candidates` — young cycles whose ONLY divert was the take-over
    ///   licence (a helper-window pause whose licence for a pinning backend is
    ///   `MovePinned`), offered to the pinned copy;
    /// * `taken` — of those, cycles whose pin ledger was complete and within
    ///   the 1/8 page bound: they ran the moving branch (in place when a pin
    ///   landed on an object, as a Cheney copy otherwise);
    /// * `ledger_incomplete`, `over_bound` — the two declines; the cycle then
    ///   diverts exactly as without the flag, under its coverage reason.
    ///
    /// Kept apart from [`Self::pinned_young_census`] so that census's
    /// term-4 identities (`cjdiv_diverts = ... + over_bound +
    /// ledger_incomplete`) still hold with the flag on. All zero with it off.
    pub fn pinned_takeover_census(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.pinned_takeover_census[i].load(Ordering::Relaxed))
    }

    /// gcj j1/yg item 1: the unproven-frame arm
    /// (`CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES`) over this heap's
    /// life, `[candidates, taken, ledger_incomplete, over_bound]` as
    /// [`Self::pinned_takeover_census`]. All zero with the switch off.
    pub fn pinned_unproven_census(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.pinned_unproven_census[i].load(Ordering::Relaxed))
    }

    /// gcd d5/s: why the take-over arm did not take a cycle, over this heap's
    /// life: `[reached, no_takeover_peer, frozen, unread, window_refused,
    /// derived_unresolved, other_coverage, other_divert]`.
    ///
    /// `reached` counts the young cycles that reached the arm's test (both
    /// flags on, term 3 fired); each other slot the declines whose FIRST
    /// failing condition it names (`pinned_takeover_decline_reason`). So
    /// `reached - sum(rest)` equals `ptko_candidates`
    /// ([`Self::pinned_takeover_census`]). A run whose `window_refused` holds
    /// the declines has helper windows the pass could not pin whole -- the
    /// licence then says `NonMoving` for every backend, so no arm may move
    /// such a pause; `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` names the peer. All
    /// zero with either flag off.
    pub fn pinned_takeover_declines(&self) -> [u64; takeover_decline::LEN] {
        std::array::from_fn(|i| self.pinned_takeover_declines[i].load(Ordering::Relaxed))
    }

    /// Fold one term-4 divert into [`Self::conservative_divert_census`].
    pub fn note_conservative_divert(&self, s: ConservativeDivertSample) {
        let c = &self.conservative_divert_census;
        c[0].fetch_add(1, Ordering::Relaxed);
        if s.pins_young == 0 {
            c[1].fetch_add(1, Ordering::Relaxed);
        }
        c[2].fetch_add(s.pins_young, Ordering::Relaxed);
        c[3].fetch_max(s.pins_young, Ordering::Relaxed);
        c[4].fetch_add(s.pins_total, Ordering::Relaxed);
        if !s.initiator_veto {
            c[5].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The conservative-JIT-root divert census over this heap's life, gen
    /// r4w4/young4 (2026-09-24): `[diverts, diverts_with_no_young_pin,
    /// young_pins_sum, young_pins_max, pins_sum, diverts_without_initiator_veto]`.
    ///
    /// * `diverts` — young cycles `unrewritable_conservative_jit_roots` (term 4
    ///   of `divert_non_moving`, reason `nonmoving-unrewritable-conservative-jit-roots`)
    ///   sent to the non-moving sweep;
    /// * `diverts_with_no_young_pin` — the subset whose pin registry named NO
    ///   young from-space object: on those, the only thing between the cycle
    ///   and a copy is the interior/derived-pointer population the registry
    ///   cannot see (the QDox failure mode), i.e. what precise derived-pointer
    ///   tables would unlock;
    /// * `young_pins_sum` / `young_pins_max` — how many objects a pin-aware
    ///   copying cycle would have had to leave in place (mean = sum / diverts);
    /// * `pins_sum` — all published pins, young or not;
    /// * `diverts_without_initiator_veto` — cycles on which the initiator's own
    ///   band scan found no word in an unrewritable frame region naming a live
    ///   object.
    ///
    /// Printed by `VmHeap::print_gc_summary` as `[GC] young_conservative_divert:`.
    /// See `docs/feature-designs/generative-gc-architecture.md`.
    pub fn conservative_divert_census(&self) -> [u64; 6] {
        let c = &self.conservative_divert_census;
        [
            c[0].load(Ordering::Relaxed),
            c[1].load(Ordering::Relaxed),
            c[2].load(Ordering::Relaxed),
            c[3].load(Ordering::Relaxed),
            c[4].load(Ordering::Relaxed),
            c[5].load(Ordering::Relaxed),
        ]
    }

    /// Fold one completed pinned in-place cycle into
    /// [`Self::pinned_young_census`].
    pub fn note_pinned_young_cycle(&self, ip: &InPlaceEvac) {
        // Called after the drain (serial or absorbed parallel), so `visited`
        // is final: an unvisited pinned object is exactly what
        // `finish_in_place_young_cycle` drops (`pinned_droppable`).
        let (dropped, dropped_bytes) = if ip.drop_unvisited_pinned {
            ip.pinned
                .iter()
                .enumerate()
                .filter(|&(i, _)| ip.pinned_droppable(i))
                .fold((0u64, 0u64), |(n, b), (_, &(_, s))| (n + 1, b + s as u64))
        } else {
            (0, 0)
        };
        let pinned_bytes: u64 = ip.pinned.iter().map(|&(_, s)| s as u64).sum();
        // gcj j1/yg: this cycle's survivors in young, plus the ones it
        // promoted only because the free spans ran out (counted in objects:
        // charged at the in-young survivors' mean size).
        let live_bytes: usize = ip.live.iter().map(|&(_, s)| s).sum();
        let mean = live_bytes / ip.live.len().max(1);
        let overflow = usize::try_from(ip.overflow_promotions).unwrap_or(usize::MAX);
        self.pinned_survivor_estimate.store(
            live_bytes.saturating_add(overflow.saturating_mul(mean)),
            Ordering::Relaxed,
        );
        let c = &self.pinned_young_census;
        c[0].fetch_add(1, Ordering::Relaxed);
        c[4].fetch_add(ip.pages as u64, Ordering::Relaxed);
        c[5].fetch_max(ip.pages as u64, Ordering::Relaxed);
        // gcj j2/pn2: objects (and bytes) LEFT IN PLACE, so the ones the same
        // cycle reclaims are not counted (they used to be).
        c[6].fetch_add(
            (ip.pinned.len() as u64).saturating_sub(dropped),
            Ordering::Relaxed,
        );
        c[7].fetch_add(pinned_bytes.saturating_sub(dropped_bytes), Ordering::Relaxed);
        c[8].fetch_add(ip.copies, Ordering::Relaxed);
        c[9].fetch_add(ip.overflow_promotions, Ordering::Relaxed);
        c[10].fetch_add(ip.kept_count, Ordering::Relaxed);
        if ip.drop_unvisited_pinned {
            let e = &self.pinned_exact_census;
            if ip.exact_rule {
                // gcg g1/y: the exact / named-roots switches' engagement.
                e[0].fetch_add(1, Ordering::Relaxed);
                e[1].fetch_add(ip.exact_seeds, Ordering::Relaxed);
                e[2].fetch_add(ip.pinned.len() as u64, Ordering::Relaxed);
                e[3].fetch_add(dropped, Ordering::Relaxed);
                e[4].fetch_add(dropped_bytes, Ordering::Relaxed);
            } else {
                // gcj j2/pn2: only the default-ON watched / finalizable
                // filters named the seeds (`pfd_*`).
                e[5].fetch_add(1, Ordering::Relaxed);
                e[6].fetch_add(dropped, Ordering::Relaxed);
                e[7].fetch_add(dropped_bytes, Ordering::Relaxed);
            }
        }
        // gcj j2/pn2: pinned objects kept because their forward was refused.
        self.pinned_exact_census[8].fetch_add(ip.refused_kept, Ordering::Relaxed);
        // gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 5):
        // seeds kept by `CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED` alone.
        self.pinned_exact_census[9].fetch_add(ip.protected_seeds_kept(), Ordering::Relaxed);
    }

    /// gcg g1/y: `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`'s engagement,
    /// `[GC] pinned_exact_roots:`. `pxr_cycles` pinned cycles ran with exact
    /// roots; over them the words named `pxr_seeds_sum` objects of the plans'
    /// `pxr_plan_objects_sum`, and `pxr_dropped_sum` page neighbours
    /// (`pxr_dropped_bytes_sum` bytes) that nothing reached were reclaimed --
    /// the objects the page rule would have kept. All zero with the switch
    /// off (the negative control); `pxr_dropped_sum=0` with it on means the
    /// switch changed nothing on that run.
    ///
    /// gcj j2/pn2: `pxr_*` count only cycles whose seeds
    /// `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS` / `_NAMED_ROOTS` named (before,
    /// every drop-mode cycle, so the "all zero with the switch off" control
    /// failed on the default path). The default-ON watched / finalizable
    /// filters' drop-mode cycles are `pfd_cycles`, their reclaimed objects
    /// `pfd_dropped_sum` / `pfd_dropped_bytes_sum`; `prk_kept_sum` pinned
    /// objects were kept because their forward was refused
    /// (`CRATONVM_GEN_PINNED_REFUSED_FORWARD_KEPT`; 0 on a healthy run, any
    /// other value names a corrupt header the old drop mode would have freed).
    ///
    /// gcj j2/x1: `psk_kept_sum` seeds were kept only by
    /// `CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED` (unvisited, not refused: the
    /// drop mode would have reclaimed them); 0 under its `=0` and on a healthy
    /// run, so a def-vs-kill pair shows the switch engage.
    pub fn pinned_exact_roots_census_line(&self) -> String {
        let e = |i: usize| self.pinned_exact_census[i].load(Ordering::Relaxed);
        format!(
            "[GC] pinned_exact_roots: pxr_cycles={} pxr_seeds_sum={} pxr_plan_objects_sum={} pxr_dropped_sum={} pxr_dropped_bytes_sum={} pfd_cycles={} pfd_dropped_sum={} pfd_dropped_bytes_sum={} prk_kept_sum={} psk_kept_sum={}",
            e(0),
            e(1),
            e(2),
            e(3),
            e(4),
            e(5),
            e(6),
            e(7),
            e(8),
            e(9),
        )
    }

    /// gcf f2/y: the defragmentation gate of the pinned copy
    /// (`CRATONVM_GEN_PINNED_COPY_DEFRAG`), `[GC] pinned_copy_defrag:`.
    /// `pcd_sweeps_measured` non-moving sweeps measured the free list,
    /// `pcd_sweeps_fragmented` of them left young fragmented
    /// ([`young_space_fragmented`]); of the term-4-alone cycles that reached
    /// the pinned copy's candidacy, `pcd_copies_admitted` were let through and
    /// `pcd_copies_refused` swept instead. gch h2/yg: `pcd_large_misses`
    /// large young requests that missed on a fragmented free list and armed
    /// the gate ([`young_miss_is_fragmentation`]). All zero with the switch
    /// off.
    pub fn pinned_copy_defrag_census_line(&self) -> String {
        let c = |i: usize| self.pinned_copy_defrag_census[i].load(Ordering::Relaxed);
        format!(
            "[GC] pinned_copy_defrag: pcd_enabled={} pcd_sweeps_measured={} pcd_sweeps_fragmented={} pcd_copies_admitted={} pcd_copies_refused={} pcd_large_misses={}",
            u8::from(self.pinned_copy_defrag),
            c(0),
            c(1),
            c(2),
            c(3),
            c(4),
        )
    }

    /// gcj j1/yg: why term-4 cycles did or did not reach the pinned copy,
    /// `[GC] young_term4_offer:`. `t4o_cycles` cycles on which term 4
    /// (unrewritable conservative JIT roots) fired; `t4o_offered` of them were
    /// offered to the copy (`term4_alone`; the copy may still decline, under
    /// `pycopy_*`); the rest name the FIRST other divert term, in the order
    /// GPU veto, explicit full collection, promotion-OOM guard
    /// (`t4o_gpu` + `t4o_explicit_full_gc` + `t4o_promotion_risk` =
    /// `t4o_cycles - t4o_offered`); `t4o_promotion_risk_admitted` counts the
    /// offered cycles `CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK`
    /// (`t4o_takes_promotion_risk=1`) let through. All zero on a run with no
    /// live compiled frame.
    pub fn term4_offer_census_line(&self) -> String {
        let c = |i: usize| self.term4_offer_census[i].load(Ordering::Relaxed);
        format!(
            "[GC] young_term4_offer: t4o_takes_promotion_risk={} t4o_cycles={} t4o_offered={} t4o_gpu={} t4o_explicit_full_gc={} t4o_promotion_risk={} t4o_promotion_risk_admitted={}",
            u8::from(self.pinned_copy_takes_promotion_risk),
            c(T4O_CYCLES),
            c(T4O_OFFERED),
            c(T4O_GPU),
            c(T4O_EXPLICIT_FULL_GC),
            c(T4O_PROMOTION_RISK),
            c(T4O_PROMOTION_RISK_ADMITTED),
        )
    }

    /// gcf f3/q: the young-premium cuts of the non-moving sweep,
    /// `[GC] young_premium:`. `yp_memo_hits` compact objects sized from the
    /// walk size memo (`CRATONVM_GEN_WALK_SIZE_MEMO`; walk (2), fix-up 3a and
    /// the sequential sweep walk, not the parallel chunks);
    /// `yp_batch_walks` selective walks that published their census once
    /// (`CRATONVM_GEN_SWEEP_BATCH_CENSUS`); `yp_3a_replays` / `yp_3a_walks`
    /// promoting cycles whose fix-up 3a replayed the mark set
    /// (`CRATONVM_GEN_SWEEP_3A_REPLAY`) / re-walked the arena;
    /// gcg g4/yg: the `yp_3c_listed_*`, `yp_evac_reserved` and
    /// `yp_coalesce_*` fields went with their deleted switches; gcg g5/yg:
    /// `yp_frag_spill_advised` with `CRATONVM_GEN_YOUNG_FRAG_SPILL`.
    /// gch h1/yy: `yp_par_walks` / `yp_par_walk_fallbacks` selective walks run
    /// as parallel chunks / attempted and refused
    /// (`CRATONVM_GEN_SWEEP_PARALLEL_PROMOTION_WALK`); `yp_par_card_seeds`
    /// card seeds run on the pool (`CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED`);
    /// gch h3/yc: `yp_refill_spare_caps` went with the deleted
    /// `CRATONVM_GEN_TLAB_REFILL_SPARES_LARGE_BLOCKS`. gci i22/dr:
    /// `yp_refill_whole_below_request` / `yp_refill_at_request` young TLAB
    /// refills served by a whole free block below the request / at the full
    /// request (`CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`). gcj j2/yg2:
    /// `yp_kz_carried_bytes` / `yp_kz_carry_sweeps` the known-zero bytes the
    /// sweeps kept claiming from the previous set, and the sweeps that kept
    /// any (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`).
    pub fn young_premium_census_line(&self) -> String {
        let c = |i: usize| self.young_premium_census[i].load(Ordering::Relaxed);
        format!(
            "[GC] young_premium: yp_memo_hits={} yp_batch_walks={} yp_3a_replays={} yp_3a_walks={} \
             yp_par_walks={} yp_par_walk_fallbacks={} yp_par_card_seeds={} \
             yp_refill_whole_below_request={} yp_refill_at_request={} \
             yp_kz_carried_bytes={} yp_kz_carry_sweeps={} yp_fused_sweeps={} \
             yp_dead_chunks={} yp_dead_chunk_bytes={} yp_dead_chunk_declined={}",
            c(YP_MEMO_HITS),
            c(YP_BATCH_WALKS),
            c(YP_3A_REPLAYS),
            c(YP_3A_WALKS),
            c(YP_PAR_WALKS),
            c(YP_PAR_WALK_FALLBACKS),
            c(YP_PAR_CARD_SEEDS),
            c(YP_REFILL_WHOLE_BELOW_REQUEST),
            c(YP_REFILL_AT_REQUEST),
            c(YP_KZ_CARRIED_BYTES),
            c(YP_KZ_CARRY_SWEEPS),
            c(YP_FUSED_SWEEPS),
            c(YP_DEAD_CHUNKS),
            c(YP_DEAD_CHUNK_BYTES),
            c(YP_DEAD_CHUNK_DECLINED),
        )
    }

    /// The pinned in-place young copy's census over this heap's life, gen
    /// r4w5/pinned5 (2026-09-24). Only cycles the conservative-JIT-root term
    /// decided, under `CRATONVM_GEN_PINNED_YOUNG_COPY`, move it:
    ///
    /// * `[0] pinned_cycles` — cycles that ran the pinned copy (decision
    ///   `moving-pinned-pages`);
    /// * `[1] no_young_pin_words` — cycles whose complete pin ledger named no
    ///   young object, which therefore ran the ordinary Cheney copy
    ///   (`moving-no-young-pin-words`);
    /// * `[2] over_bound` — diverted because the pinned pages exceeded 1/8 of
    ///   from-space (`nonmoving-pinned-pages-over-bound`);
    /// * `[3] ledger_incomplete` — diverted because
    ///   `gc_quiescence::pause_young_pin_words` said the ledger was incomplete
    ///   (`nonmoving-pin-ledger-incomplete`);
    /// * `[4] pages_sum`, `[5] pages_max` — pinned pages per pinned cycle;
    /// * `[6] objects_sum`, `[7] bytes_sum` — objects (and their bytes) left in
    ///   place;
    /// * `[8] in_place_copies` — survivors copied into from-space's free spans;
    /// * `[9] overflow_promotions` — survivors too young to tenure that were
    ///   promoted because the free spans ran out;
    /// * `[10] kept_in_place` — survivors neither a free span nor old gen could
    ///   take, left where they were.
    ///
    /// The engagement identity: `pinned_cycles + no_young_pin_words` equals the
    /// decision histogram's `moving-pinned-pages + moving-no-young-pin-words`,
    /// and the divert census's `cjdiv_diverts` equals
    /// `nonmoving-unrewritable-conservative-jit-roots + over_bound +
    /// ledger_incomplete`.
    ///
    /// Printed by `VmHeap::print_gc_summary` as `[GC] young_pinned_copy:`.
    pub fn pinned_young_census(&self) -> [u64; PINNED_YOUNG_CENSUS_LEN] {
        std::array::from_fn(|i| self.pinned_young_census[i].load(Ordering::Relaxed))
    }

    /// Arm promote-on-pressure for the next young cycle when this one's
    /// survival was high ([`promote_pressure_verdict`]), and count the arm.
    /// Called at the end of every young cycle, moving or not.
    pub fn arm_promote_pressure_on_high_survival(
        &self,
        occupied: usize,
        survived: usize,
        capacity: usize,
    ) {
        if promote_pressure_verdict(occupied, survived, capacity) {
            tracing::debug!(
                "GC: high survival ({survived} of {occupied} bytes stayed young) — \
                 arming promote-on-pressure for next minor GC",
            );
            self.force_promote_all.store(true, Ordering::Relaxed);
            self.promote_pressure_census[0].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `(armed, consumed, consumed_stale, dropped_on_divert)` for the
    /// promote-on-pressure arm over this heap's life (gen r4w2/youngpolicy).
    ///
    /// * `armed` — young cycles, moving or (since
    ///   `gen-gcd1-pinned-callee-oome-probe-crawls-unaged-young-sweeps`) non-moving,
    ///   that observed >75 % survival at >= half occupancy and armed the next
    ///   cycle to tenure everything;
    /// * `consumed` — cycles that tenured their whole (unpinned) survivor set
    ///   on an arm: a moving cycle, or (gen r4w4/young4) the non-moving sweep
    ///   of the very next cycle;
    /// * `consumed_stale` — a moving consumption of an arm carried across a
    ///   non-moving cycle. Since gen r4w4/young4 no arm is carried, so this
    ///   stays 0; a non-zero reading is a regression of
    ///   `docs/feature-designs/generative-gc-architecture.md`;
    /// * `dropped_on_divert` — arms discarded without tenuring: at a divert
    ///   under `CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES`, or by a sweep that could
    ///   not promote that cycle (`selective_on` false).
    ///
    /// Printed by `VmHeap::print_gc_summary` as `[GC] promote_pressure:`
    /// (gen r4w3/obs2).
    pub fn promote_pressure_census(&self) -> (u64, u64, u64, u64) {
        (
            self.promote_pressure_census[0].load(Ordering::Relaxed),
            self.promote_pressure_census[1].load(Ordering::Relaxed),
            self.promote_pressure_census[2].load(Ordering::Relaxed),
            self.promote_pressure_census[3].load(Ordering::Relaxed),
        )
    }

    /// Serial Cheney re-drain after a LATE seed (gen r4w3/rooting): scan young
    /// to-space from `scan_cursor` and every unscanned promoted object until
    /// neither grows, forwarding ref slots and the `loader_pin` instance ->
    /// loader edge exactly as the main serial scan and Phase 2.5b do
    /// (including the once-per-holder deferred card for a promoted object that
    /// keeps a young referent).
    ///
    /// Used by `collect_garbage_inner`'s "loader_pin from OLD instances" phase,
    /// which runs after EITHER copy arm has reached its fix-point — after the
    /// parallel arm `scan_cursor` already sits at `young_to.used()`, the same
    /// continuation Phase 2.5b relies on. Phase 2.5b still carries its own
    /// inline copy of this loop; folding it onto this helper is a listed
    /// follow-up (`gengc-round4-w3-rooting-20260923.md`), not done here to keep
    /// the hunk inside the rooting lane's ownership.
    #[allow(clippy::too_many_arguments)]
    pub fn redrain_serial_closure(
        young_from: &Arena,
        young_object_starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        objects_copied: &mut usize,
        pointer_map: &mut cratonvm_types::PointerMap,
        promoted_worklist: &mut PromotionQueue,
        copy_policy: CopyPolicy,
        scan_cursor: &mut usize,
        scanned_promoted: &mut FxHashSet<usize>,
        deferred_dirty_cards: &mut Vec<(usize, usize)>,
        loader_pin_on: bool,
    ) {
        loop {
            let mut made_progress = false;
            while *scan_cursor < young_to.used() {
                made_progress = true;
                // SAFETY: `scan_cursor` is below `young_to.used()` and advances
                // by each object's total size, so this is a copied object's
                // header in young to-space.
                let obj_ptr = unsafe { young_to.base_ptr_mut().add(*scan_cursor) };
                // SAFETY: `obj_ptr` is a copied object with a valid header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let total_size = gen_object_total_size(header);
                // Captured before forwarding may grow young_to.
                let cid = header.class_id.as_u32();
                // SAFETY: `obj_ptr`/`header` are a valid copied object in young_to.
                unsafe {
                    forward_ref_slots(obj_ptr, header, |ref_ptr| {
                        if young_from.contains(ref_ptr) {
                            Some(Self::forward_object(
                                young_from,
                                young_object_starts,
                                young_to,
                                old_gen,
                                ref_ptr,
                                objects_copied,
                                pointer_map,
                                promoted_worklist,
                                copy_policy,
                            ))
                        } else {
                            None
                        }
                    });
                }
                if loader_pin_on {
                    if let Some(loader_old) = young_loader_pin(
                        cid,
                        &young_from,
                        promoted_worklist.loader_pins.as_ref(),
                    ) {
                        let lp = loader_old as *mut u8;
                        if young_from.contains(lp) {
                            Self::forward_object(
                                young_from,
                                young_object_starts,
                                young_to,
                                old_gen,
                                lp,
                                objects_copied,
                                pointer_map,
                                promoted_worklist,
                                copy_policy,
                            );
                        }
                    }
                }
                *scan_cursor += total_size;
            }
            // gen r4w5/pinned5: what the pinned in-place cycle queued.
            while let Some(obj_ptr) = promoted_worklist.pop_in_place() {
                made_progress = true;
                Self::scan_in_place_young(
                    obj_ptr,
                    young_from,
                    young_object_starts,
                    young_to,
                    old_gen,
                    objects_copied,
                    pointer_map,
                    promoted_worklist,
                    copy_policy,
                    loader_pin_on,
                );
            }
            while let Some(obj_ptr) = promoted_worklist.pop() {
                if !scanned_promoted.insert(obj_ptr as usize) {
                    continue;
                }
                made_progress = true;
                // SAFETY: `obj_ptr` was pushed by `forward_object` only after
                // the promotion landed in old gen, with a valid copied header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let promoted_cid = header.class_id.as_u32();
                // gcd d2/h: per element card for a reference array on an
                // element-precise table (`HolderCardDefer`).
                let mut card_defer = HolderCardDefer::new(
                    obj_ptr as usize,
                    header,
                    promoted_worklist.element_card_base,
                );
                // SAFETY: `obj_ptr`/`header` are a valid promoted old-gen object.
                unsafe {
                    forward_ref_slots_at(obj_ptr, header, |off, ref_ptr| {
                        if young_from.contains(ref_ptr) {
                            let new_ref_ptr = Self::forward_object(
                                young_from,
                                young_object_starts,
                                young_to,
                                old_gen,
                                ref_ptr,
                                objects_copied,
                                pointer_map,
                                promoted_worklist,
                                copy_policy,
                            );
                            if !old_gen.contains(new_ref_ptr) {
                                card_defer.slot_stays_young(off, deferred_dirty_cards);
                            }
                            Some(new_ref_ptr)
                        } else {
                            None
                        }
                    });
                }
                if loader_pin_on {
                    if let Some(loader_old) = young_loader_pin(
                        promoted_cid,
                        &young_from,
                        promoted_worklist.loader_pins.as_ref(),
                    ) {
                        let lp = loader_old as *mut u8;
                        if young_from.contains(lp) {
                            let new_lp = Self::forward_object(
                                young_from,
                                young_object_starts,
                                young_to,
                                old_gen,
                                lp,
                                objects_copied,
                                pointer_map,
                                promoted_worklist,
                                copy_policy,
                            );
                            if !old_gen.contains(new_lp) {
                                card_defer.whole_object();
                            }
                        }
                    }
                }
                card_defer.finish(deferred_dirty_cards);
            }
            if !made_progress {
                break;
            }
        }
    }

    /// t1 w8 PIN10 / gcki-pin-age (`CRATONVM_GEN_PINNED_COPY_AGES_IN_PLACE`,
    /// default ON): age a pinned or kept survivor that stays in place, called
    /// once per object (first visit of a pinned object; the kept insertion;
    /// the parallel arm after its absorb), with the cycle's age step. Its
    /// census entry is a copy's (`note_survivor_age`) only when HotSpot would
    /// have copied it to survivor space: one due to tenure would have been
    /// promoted, and HotSpot's `AgeTable` holds survivor copies only. Nothing
    /// when [`PromotionQueue::in_place_tenure_age`] is `None`.
    pub fn age_in_place_survivor_serial(old_ptr: *mut u8, size: usize, q: &mut PromotionQueue) {
        let Some(tenure_age) = q.in_place_tenure_age else {
            return;
        };
        // SAFETY: `old_ptr` is a proved from-space object start that stays in
        // place this cycle; neither arm leaves a forward in a pinned or kept
        // object's mark word, and the pause is stopped.
        let (old_age, new_age) = unsafe { age_in_place_survivor(old_ptr as usize, q.age_step) };
        if q.age_census && !should_tenure(old_age, false, tenure_age) {
            q.note_survivor_age(new_age, size);
        }
    }

    /// gen r4w5/pinned5: where the pinned in-place cycle copies a survivor of
    /// `size` bytes. Old gen first when it should tenure, else a from-space
    /// span that was free at the start of the cycle; then the other one.
    /// `None` when neither can take it (the caller leaves it in place).
    pub fn in_place_destination(
        q: &mut PromotionQueue,
        old_gen: &mut OldGen,
        size: usize,
        should_promote: bool,
    ) -> Option<*mut u8> {
        if should_promote {
            if let Some(p) = q.alloc(old_gen, size) {
                return Some(p);
            }
        }
        if let Some(p) = q.in_place.as_deref_mut().and_then(|ip| ip.dest_alloc(size)) {
            return Some(p as *mut u8);
        }
        if !should_promote {
            if let Some(p) = q.alloc(old_gen, size) {
                if let Some(ip) = q.in_place.as_deref_mut() {
                    ip.overflow_promotions += 1;
                }
                return Some(p);
            }
        }
        None
    }

    /// gen r4w5/pinned5: arm a pinned in-place young cycle before its first
    /// forward — the pinned objects of `plan`, and the destination spans:
    /// the bytes of from-space that were FREE when the cycle began (its free
    /// blocks and a window of its un-bumped tail, committed here since the
    /// lazy backing may not have mapped it) minus the pinned pages, so a copy
    /// never lands on a byte a live object occupies, nor on a page a compiled
    /// frame holds a raw word into. Returns the state and the pinned bases,
    /// which the caller forwards first (each is a root of the cycle).
    ///
    /// gen r5w3/evac7: factored out of the serial arm, unchanged, so the
    /// parallel arm (`CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`, L1) arms the
    /// same state; plus the tail window's default on size (L2,
    /// `CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE`): `used - free_list_bytes`, the
    /// bytes allocated at the start of the cycle and therefore an upper bound
    /// on the survivors, instead of `max(used / 2, 1 MiB)`. Both are capped by
    /// the un-bumped tail.
    pub fn arm_in_place_young_cycle(
        young_from: &mut Arena,
        plan: PinnedYoungPlan,
        young_base: usize,
        young_used: usize,
    ) -> (InPlaceEvac, Vec<usize>) {
        let initial_free = young_from.free_blocks_sorted();
        let cap = young_from.capacity();
        let window = if gc_flags().gen_pinned_tail_window_live {
            let free_bytes: usize = initial_free.iter().map(|&(_, len)| len).sum();
            young_used.saturating_sub(free_bytes).max(1 << 20)
        } else {
            (young_used / 2).max(1 << 20)
        };
        let tail_len = cap.saturating_sub(young_used).min(window) & !7;
        let mut candidates: Vec<(usize, usize)> = initial_free
            .iter()
            .copied()
            .filter(|&(off, len)| len >= IN_PLACE_MIN_DEST_SPAN && off + len <= young_used)
            .collect();
        if tail_len > 0 {
            candidates.push((young_used, tail_len));
        }
        // gcj j2/pn2 (`CRATONVM_GEN_PINNED_COPY_OBJECT_WINDOWS`): the plan's
        // windows plus its objects' extents instead of whole pages.
        let pinned_page_spans: Vec<(usize, usize)> = match plan.reserved.as_ref() {
            Some(reserved) => reserved.clone(),
            None => plan
                .pages
                .iter()
                .map(|&p| (p << PIN_PAGE_SHIFT, PIN_PAGE))
                .collect(),
        };
        let mut dest: Vec<(usize, usize)> = Vec::new();
        for (off, len) in subtract_spans(&candidates, &pinned_page_spans) {
            if len >= IN_PLACE_MIN_DEST_SPAN && young_from.commit_for_relocation(off, len) {
                dest.push((young_base + off, young_base + off + len));
            }
        }
        // gcj-j1pn-pinned-cycle-review-findings item 7: the tail bytes below
        // the highest tail destination that no destination covers (a pinned
        // page reaching past `young_used`) end up free-listed below the new
        // cursor, so they are committed too; refused, the tail destinations
        // are given up (the copies promote or land in the free blocks).
        let tail_offsets: Vec<(usize, usize)> =
            dest.iter().map(|&(s, e)| (s - young_base, e - s)).collect();
        if let Some((off, len)) = in_place_tail_gap(&tail_offsets, young_used) {
            if !young_from.commit_for_relocation(off, len) {
                dest.retain(|&(s, _)| s - young_base < young_used);
            }
        }
        // r23 w4 review (R-L4z item 2): a destination span may lie in the
        // lazy-dirty set, whose mirror makes `is_object_address` decline every
        // address in it. The copies this cycle writes there must never be
        // declined, so the mirror is cleared over the destinations now (the
        // set itself is kept: an unused remainder still needs its hand-out
        // memset); `finish_in_place_young_cycle` takes the copies out of the
        // set and restores the rest.
        let dest_offsets: Vec<(usize, usize)> =
            dest.iter().map(|&(s, e)| (s - young_base, e - s)).collect();
        young_from.suspend_lazy_mirror_over(&dest_offsets);
        // gcf f5/y: when the plan names its roots (`_EXACT_ROOTS`,
        // `_NAMED_ROOTS`, or -- on the default path -- the watched /
        // finalizable filters; gcj j2/pn2 corrected "exact roots only") only
        // those are seeds; every other pinned object stays where it is if the
        // drain reaches it (the in-place arms test the pinned set, not the
        // seeds) and is reclaimed otherwise.
        let seeds: Vec<usize> = match plan.roots.as_ref() {
            Some(roots) => roots.clone(),
            None => plan.objects.iter().map(|&(b, _)| b).collect(),
        };
        let ip = InPlaceEvac::new(plan, dest, initial_free, young_base, young_used);
        (ip, seeds)
    }

    /// gen r5w3/evac7 (L1): fold a PARALLEL pinned in-place drain back into
    /// the serial state `ip` it was armed from, so every later phase sees what
    /// a serial in-place drain leaves.
    ///
    /// 1. Every self-forward the workers used to CLAIM an object in place — a
    ///    pinned object, or a survivor kept because nothing could take it — is
    ///    undone (`restore_self_forwarded_mark`, from the mark word each
    ///    claimer saved). This must run before anything reads those headers
    ///    again: after it, a pinned or kept object is an ordinary object in
    ///    place, exactly as the serial path leaves it (which never writes
    ///    their mark words).
    /// 2. Claimed pinned objects become `visited`; kept ones join `kept`.
    /// 3. Copies and kept survivors join `live` (pinned ones are there since
    ///    arming); the census counters add up.
    /// 4. Each destination span's cursor comes back from the pool, so a
    ///    later serial forward (the loader rescue, finalizer resurrection)
    ///    carves after what the workers used, and `high_water` is right.
    ///
    /// `ip.scan` stays empty: the workers scanned everything they claimed.
    pub fn absorb_parallel_in_place(
        ip: &mut InPlaceEvac,
        view: Option<&crate::gen_evac::ParInPlace>,
        shards: &[crate::gen_evac::EvacShard],
    ) {
        for shard in shards {
            for &(addr, saved) in shard
                .in_place_pinned
                .iter()
                .chain(shard.in_place_kept.iter())
            {
                // SAFETY: `addr` is a from-space object base this cycle's
                // workers claimed with a self-forward; every worker has
                // returned (the pool's completion barrier) and every mutator
                // is stopped, so nothing else reads or writes the word.
                unsafe { (*(addr as *const ObjectHeader)).restore_self_forwarded_mark(saved) };
            }
        }
        // gcj j1yg-pause: one allocation for every shard's survivors (~300 K
        // entries a JIT-warm javac cycle), not a doubling per shard.
        ip.live
            .reserve(shards.iter().map(|s| s.in_place_live.len()).sum());
        for shard in shards {
            for &(addr, _) in &shard.in_place_pinned {
                if let Some(i) = ip.pinned_index(addr) {
                    ip.visited[i] = true;
                }
            }
            for &(addr, _) in &shard.in_place_kept {
                ip.kept.insert(addr);
                ip.kept_count += 1;
            }
            // gcj j2/pn2: a pinned object a worker refused counts as reached
            // (the serial arm's `note_refused_forward`). gcj j3/x2 R9: the
            // shard keeps at most `REFUSALS_PER_SHARD` refusals for the
            // ledger, but every refused pinned base past that cap too.
            for &(addr, _) in &shard.refusals {
                ip.note_refused_forward(addr);
            }
            for &addr in &shard.refused_pinned_past_cap {
                ip.note_refused_forward(addr);
            }
            ip.live.extend(shard.in_place_live.iter().copied());
            ip.copies += shard.in_place_copies;
            ip.overflow_promotions += shard.in_place_overflow;
        }
        if let Some(view) = view {
            for (d, cursor) in ip.dest.iter_mut().zip(view.cursors()) {
                d.1 = cursor;
            }
        }
        // gen r5w4/pin8: the spans the workers vacated, for the rebuild's
        // quarantine (`CRATONVM_DBG_STALE_OBJREF`): every forward whose source
        // is a from-space byte of the cycle and whose target is elsewhere. A
        // CAS loser repeats its winner's pair; the rebuild de-duplicates.
        if ip.quarantine {
            let from_end = ip.base + ip.used_at_start;
            for shard in shards {
                for &(old, new) in &shard.forwards {
                    if new == 0 || new == old || old < ip.base || old >= from_end {
                        continue;
                    }
                    // SAFETY: `new` is the complete copy a worker published
                    // before its forward (young destination span or old gen);
                    // every worker has returned, so its header is whole and
                    // nothing writes it.
                    let size = gen_object_total_size(unsafe { &*(new as *const ObjectHeader) });
                    ip.vacated.push((old, size));
                }
            }
        }
    }

    /// gen r4w5/pinned5: Phase 3 of a pinned in-place young cycle — make
    /// from-space what a non-moving sweep would leave, holding exactly the
    /// cycle's `live` objects. Returns the absolute `(addr, len)` spans the
    /// cycle took back -- every byte below the starting cursor that is neither
    /// live nor was already free, quarantined spans included -- sorted and
    /// disjoint: what the stale-root audits must treat as vacated
    /// (gen r5w5/pin9; `GenerationalHeap::young_vacated_probe`).
    ///
    /// 1. Zero every byte below the starting cursor that is neither live nor
    ///    was already free (a forwarded object's old copy, a dead object, a
    ///    retired TLAB's filler): from here on this arena is allocated from, a
    ///    conservative scanner must not resurrect a corpse in it, and a
    ///    free-list hand-out's tripwire and the bump tail's zero-once
    ///    invariant both read these bytes as zero.
    /// 2. The complement of `live` below the new cursor becomes the free list;
    ///    a sub-`HEADER_SIZE` sliver cannot be a block and gets the 8-byte GAP
    ///    sentinel every linear young walk already strides.
    /// 3. `Arena::rebuild_after_in_place_evacuation` publishes cursor, free
    ///    list, anchors and the object-start registry.
    ///
    /// gcf f1/y: `zero_threads` / `zero_pool` are the width and the heap's
    /// persistent pool for step 1's zeroing (`None`: the calling thread, or
    /// scoped threads when `zero_threads > 1`, which only unit tests ask for).
    /// The set of bytes zeroed does not depend on either.
    pub fn finish_in_place_young_cycle(
        young_from: &mut Arena,
        mut ip: InPlaceEvac,
        cycle: u64,
        zero_threads: usize,
        zero_pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
        starts: Option<&cratonvm_gc_core::young_mark::ObjectStartBits>,
    ) -> Vec<(usize, usize)> {
        let base = ip.base;
        // gcf f5/y (`CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`): a pinned object
        // that was not a seed and that nothing reached is dead -- take it out
        // of `live`, so step 1 zeroes it and step 2 frees it like any other
        // dead object. (Without the switch every pinned object was a seed,
        // hence visited, and this removes nothing.) gcj j1/pn: the plan's
        // roots are also set by the default-ON watched and finalizable
        // filters, so this runs on the default path; and a seed is never
        // dropped (`InPlaceEvac::pinned_droppable`, a seed whose forward was
        // refused before the pinned arm stays where it is).
        //
        // gcj j1yg-pause: `live` is sorted first (below) and the dead pinned
        // bases, sorted too, are removed by one merge pass -- not by a hash
        // probe per survivor (~300 K a JIT-warm javac cycle, and with the
        // named rule most pinned cycles have unvisited pinned objects).
        let dead_pinned: Vec<usize> = if ip.drop_unvisited_pinned {
            let mut d: Vec<usize> = (0..ip.pinned.len())
                .filter(|&i| ip.pinned_droppable(i))
                .map(|i| ip.pinned[i].0)
                .collect();
            d.sort_unstable();
            d
        } else {
            Vec::new()
        };
        // gen r5w6/pin10: the STABLE sort, which merges natural ascending runs
        // (O(n log runs)) where `sort_unstable` is O(n log n) on anything but a
        // single run. `live` is a handful of ascending runs by construction:
        // the pinned objects (sorted when armed), then the copies in
        // destination order -- `dest_alloc` bumps each span's cursor upward and
        // moves to a higher span when one is spent; on the parallel arm each
        // worker's buffers are claimed upward from the same spans, and the
        // shards are concatenated -- plus the few kept survivors. The entries
        // are distinct `(address, size)` pairs, so stable and unstable orders
        // are the same order: the result is bit-identical.
        //
        // gcj pcp: the sort runs in slices on the heap's pool and one merge
        // writes the offsets, dropping the dead pinned bases on the way
        // (`sorted_live_offsets`) -- the serial sort and the offset copy were
        // most of this rebuild's driver time on JIT-warm javac (~300 K
        // survivors, as many runs as the workers claimed buffers).
        let live = sorted_live_offsets(&mut ip.live, base, &dead_pinned, zero_threads, zero_pool);
        debug_assert!(
            live.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0),
            "the in-place cycle's survivors overlap: a destination was handed out twice"
        );
        let high_water = ip.high_water();
        let dead_all = subtract_spans(&complement_spans(&live, ip.used_at_start), &ip.initial_free);
        // gen r5w5/pin9: every byte this cycle takes back, for the audits.
        let took_back: Vec<(usize, usize)> = dead_all.iter().map(|&(o, l)| (base + o, l)).collect();
        // gen r5w4/pin8 (`CRATONVM_DBG_STALE_OBJREF`): the spans this cycle
        // VACATED are not zeroed and free-listed with the rest of the dead
        // bytes. Each is stamped as a poisoned filler (`poison_quarantined_span`)
        // and kept below the cursor for one more cycle, so a reference the
        // cycle failed to rewrite reads poison -- and trips the stale-objref
        // canary in `get_header` -- instead of a NEW object the allocator put
        // on its old address. Empty (and the rebuild unchanged) without the
        // flag.
        let quarantined: Vec<(usize, usize)> = if ip.quarantine {
            let vacated: Vec<(usize, usize)> = ip
                .vacated
                .iter()
                .filter_map(|&(a, s)| a.checked_sub(base).map(|off| (off, s)))
                .collect();
            quarantine_spans(vacated, &dead_all)
        } else {
            Vec::new()
        };
        let dead = if quarantined.is_empty() {
            dead_all
        } else {
            subtract_spans(&dead_all, &quarantined)
        };
        // What stays below the cursor: the survivors, plus the quarantine
        // stamps (dead but walkable objects, freed by the next young cycle).
        let kept: Vec<(usize, usize)> = if quarantined.is_empty() {
            live
        } else {
            let mut k = live;
            k.extend_from_slice(&quarantined);
            k.sort_unstable();
            k
        };
        debug_assert!(
            kept.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0),
            "a quarantined span overlaps a survivor"
        );
        let new_cursor = kept.last().map_or(0, |&(o, s)| o + s);
        // gen r5w2/roots6 (`CRATONVM_DBG=root-write-audit`): every span zeroed
        // or filled below is THIS arena's; anything else aborts here.
        let audit = cratonvm_gc_core::root_write_audit::enabled();
        let arena_extent = [(base, base + young_from.capacity())];
        if audit {
            for &(off, len) in &dead {
                cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                    cratonvm_gc_core::root_write_audit::HeapWriteKind::InPlaceZero,
                    base.wrapping_add(off),
                    len,
                    &arena_extent,
                );
            }
        }
        // SAFETY: every span is an 8-aligned part of `[0, used_at_start)` of
        // this arena — below the starting cursor, so committed — that holds no
        // survivor (the complement of `live`) and was not free (free blocks
        // already read as zero); the spans are pairwise disjoint, and every
        // mutator is stopped. gcf f1/y: on the heap's pool when one is given
        // (the caller has no dispatch open), instead of `zero_spans_parallel`'s
        // fresh scoped threads -- the gce e1/y fix the sweep's zeroing got and
        // this rebuild did not: `threads - 1` OS thread creations per pinned
        // cycle whose dead bytes pass `ZERO_SPANS_PARALLEL_MIN_BYTES`.
        // JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_IN_PLACE_LAZY_ZERO`,
        // default ON, `0` restores the in-pause memset): instead of zeroing
        // every dead byte here (12-15 ms per 245 MB of dead eden on
        // `R21AllocLean`, O(allocated) on a ~1 MB live set), zero only the
        // HEADER of every object the walk found starting in a dead span, and
        // hand the spans to the arena's lazy-dirty set:
        //
        // * the corpse-header hazard the memset closed -- a conservative word
        //   or `is_object_address` meeting a stale but plausible header -- is
        //   closed the same way at every place a header can be: each object
        //   start in the span reads as the all-zero header the memset left;
        // * nothing walks the rest: below the new cursor every dead byte is on
        //   the free list (or under a filler), which every young walker skips,
        //   and above it nothing is walked at all;
        // * every hand-out overlapping a lazy-dirty span is memset in full
        //   (`Arena::take_lazy_dirty` at both young hand-out doors, which also
        //   suspends the occupied-chunk tripwire), and `reset` /
        //   `reset_deferring_zero` zero up to the set's end, so the bump-tail
        //   "reads zero above the cursor" invariant holds again for the next
        //   owner.
        //
        // Declines to the memset without a start bitmap.
        //
        // Wave 4b (`gen_in_place_dead_mirror_only`): once the review's exact
        // lazy-dirty mirror covers the dead spans, the header poisoning is
        // redundant -- every structural validator that could meet a corpse
        // header (`object_start` behind `is_object_address` /
        // `is_object_start`, the non-moving mark's fallback) declines an
        // address the mirror holds before it reads a byte, and every walker
        // skips the spans (free blocks below the cursor, nothing above). So
        // the spans are noted first and the poisoning runs only when no
        // mirror exists (an arena of no capacity). That is what makes the
        // dead chunks of a lazy start bitmap need no walk at all.
        // r23 w4 review (R-L4z item 2): what stays below the cursor (the
        // survivors, copies into lazy-dirty destinations among them) leaves the
        // set and its mirror -- the mirror must never decline a live object --
        // and the destinations `arm_in_place_young_cycle` suspended get their
        // still-dirty remainder back. Runs with the switch off too: a set left
        // by earlier cycles may still name a destination.
        //
        // t1 lane F (`t1-F-proposal-young-reset-phase-share-on-windows`):
        // `kept` and `dead` are strictly disjoint partitions of the space by
        // construction. We forget kept survivors BEFORE noting dead spans: this
        // clears any preexisting lazy-dirty entries (from prior cycles) that
        // overlap kept objects in O(prior dirty) rather than scanning hundreds
        // of thousands of kept survivors against newly noted dead spans.
        young_from.forget_lazy_dirty_over(&kept);
        let dest_offsets: Vec<(usize, usize)> = ip
            .dest
            .iter()
            .map(|&(s, _, e)| (s - base, e - s))
            .collect();
        young_from.restore_lazy_mirror_over(&dest_offsets);

        if let Some(starts) = starts.filter(|_| gen_in_place_lazy_zero_enabled()) {
            young_from.note_lazy_dirty(&dead);
            if !(gen_in_place_dead_mirror_only() && young_from.lazy_dirty_mirror().is_some()) {
                // SAFETY: every dead span lies inside the arena below
                // `used_at_start`; each start the bitmap names in it is an
                // 8-aligned object base inside the span.
                unsafe { poison_dead_object_headers(base, &dead, starts) };
            }
        } else {
            unsafe {
                cratonvm_gc_core::young_mark::zero_spans_parallel_on(base, &dead, zero_threads, zero_pool)
            };
        }
        // H2-CID0: the lock-free freed-span record, as the sweep keeps it.
        // gcd d1/e: in one batch (see `record_young_spans_freed`).
        record_young_spans_freed(base, &dead, cycle);
        for &(off, len) in &quarantined {
            if audit {
                cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                    cratonvm_gc_core::root_write_audit::HeapWriteKind::InPlaceQuarantine,
                    base.wrapping_add(off),
                    len,
                    &arena_extent,
                );
            }
            // SAFETY: `quarantine_spans` kept only 8-aligned spans of at least
            // `HEADER_SIZE` bytes lying wholly inside one dead span below the
            // starting cursor -- committed, holding a moved survivor's old
            // copy and nothing live -- pairwise disjoint; every mutator is
            // stopped.
            unsafe { poison_quarantined_span(base + off, len) };
            // Recorded as this cycle's, so `young_freed_lookup` (and the
            // canary's panic) can name the cycle that vacated it.
            record_young_span_freed(base + off, len, cycle);
        }
        // gcj j1/pn item 7: a free block at or above the starting cursor (the
        // part of the last pinned page or object window above `used_at_start`,
        // when copies land past it) is committed: it lies in the 2 MiB granule
        // of the pinned page's live byte below `used_at_start` or in that of
        // the destination span starting at its end (committed whole by
        // `arm_in_place_young_cycle`). Hand-out commits again anyway
        // (`Arena::hand_out`); this states the fact the sliver fillers below
        // and the lazy-dirty set rely on.
        debug_assert!(
            complement_spans(&kept, new_cursor)
                .iter()
                .chain(kept.iter())
                .filter(|&&(off, len)| len > 0 && off + len > ip.used_at_start)
                .all(|&(off, len)| young_from.is_readable_at(off)
                    && young_from.is_readable_at(off + len - 1)),
            "the in-place rebuild free-lists or keeps an uncommitted span above the starting cursor"
        );
        let mut free: Vec<(usize, usize)> = Vec::new();
        for (off, len) in complement_spans(&kept, new_cursor) {
            if len >= HEADER_SIZE {
                free.push((off, len));
            } else if len > 0 {
                if audit {
                    cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                        cratonvm_gc_core::root_write_audit::HeapWriteKind::InPlaceFiller,
                        base.wrapping_add(off),
                        len,
                        &arena_extent,
                    );
                }
                // SAFETY: an 8-aligned sliver shorter than a header between two
                // survivors below the new cursor: dead, committed, zeroed above
                // or never written, and owned by the collector during the pause.
                unsafe { crate::gen_evac::write_filler(base + off, len) };
            }
        }
        // t1 wave 11, lane GCB (same switch): the old claim's bytes that sat in
        // the cycle's initial free blocks and that no destination wrote, read
        // BEFORE the rebuild forgets the set (`zero_on_reclaim_carried`).
        let zero_on_reclaim = gen_young_zero_on_reclaim_enabled();
        let carried = if zero_on_reclaim {
            let prior = young_from.known_zero_within(&ip.initial_free);
            zero_on_reclaim_carried(&prior, &ip.dest, base)
        } else {
            Vec::new()
        };
        young_from.rebuild_after_in_place_evacuation(new_cursor, &kept, &free, high_water);
        // t1 wave 8, lane YGZERO (`CRATONVM_GEN_YOUNG_ZERO_ON_RECLAIM`, opt-in):
        // the switch forces `gen_in_place_lazy_zero_enabled` off, so `dead` was
        // memset above, after every copy of this cycle; the rebuild's free
        // blocks inside it read zero and may skip their hand-out memset. The
        // rebuild forgot the old claim (`add_free_block`); this replaces it.
        if zero_on_reclaim {
            young_from.set_known_zero_free_spans(zero_on_reclaim_claim(&free, &dead, &carried));
        }
        took_back
    }

    /// gen r4w5/pinned5: scan one from-space object IN PLACE — a pinned
    /// object, a copy the pinned in-place cycle made into a free span, or a
    /// survivor it kept — forwarding its reference slots and its
    /// instance -> loader edge, exactly as the serial drain scans a to-space
    /// copy. Called from every serial drain loop (the main one,
    /// [`Self::redrain_serial_closure`], Phase 2.5b) through
    /// [`PromotionQueue::pop_in_place`].
    #[allow(clippy::too_many_arguments)]
    pub fn scan_in_place_young(
        obj_ptr: *mut u8,
        young_from: &Arena,
        young_object_starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        objects_copied: &mut usize,
        pointer_map: &mut cratonvm_types::PointerMap,
        promoted_worklist: &mut PromotionQueue,
        copy_policy: CopyPolicy,
        loader_pin_on: bool,
    ) {
        // SAFETY: `obj_ptr` was queued by `forward_object_impl` as a live
        // from-space object that stays where it is this cycle (pinned, kept,
        // or a completed copy into a free span), so a whole header lies there;
        // nothing forwards or overwrites it (its bytes are no destination).
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let cid = header.class_id.as_u32();
        // SAFETY: `obj_ptr`/`header` are a valid, fully-written young object.
        unsafe {
            forward_ref_slots(obj_ptr, header, |ref_ptr| {
                if young_from.contains(ref_ptr) {
                    Some(Self::forward_object(
                        young_from,
                        young_object_starts,
                        young_to,
                        old_gen,
                        ref_ptr,
                        objects_copied,
                        pointer_map,
                        promoted_worklist,
                        copy_policy,
                    ))
                } else {
                    None
                }
            });
        }
        if loader_pin_on {
            if let Some(loader_old) =
                young_loader_pin(cid, &young_from, promoted_worklist.loader_pins.as_ref())
            {
                let lp = loader_old as *mut u8;
                if young_from.contains(lp) {
                    Self::forward_object(
                        young_from,
                        young_object_starts,
                        young_to,
                        old_gen,
                        lp,
                        objects_copied,
                        pointer_map,
                        promoted_worklist,
                        copy_policy,
                    );
                }
            }
        }
    }

    /// Forward (copy or promote) a single object from young from-space.
    ///
    /// If the object has been forwarded already, returns the existing address.
    /// If the object has survived enough GCs (age >= PROMOTION_AGE), promotes
    /// it to old gen. Otherwise copies to young to-space with incremented age.
    ///
    /// When `copy.force_promote_all` is true (set by an earlier moving minor GC
    /// after observing a high survival rate, see
    /// [`GC_PROMOTE_PRESSURE_PERCENT`]), every survivor is promoted regardless
    /// of age. This breaks the long-lived-tree semispace death spiral.
    /// `copy.diag` carries the cycle's diagnostic gates (see [`CopyDiag`]).
    #[allow(clippy::too_many_arguments)]
    pub fn forward_object(
        young_from: &Arena,
        young_object_starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        old_ptr: *mut u8,
        objects_copied: &mut usize,
        pointer_map: &mut cratonvm_types::PointerMap,
        promoted_worklist: &mut PromotionQueue,
        copy: CopyPolicy,
    ) -> *mut u8 {
        // DBG (bc math-ec, CRATONVM_DBG_GCWRITE): wrap the forwarder so we can
        // see if it EVER returns a small (<0x1000) address — that would mean a
        // GC ref-update writes `Object(Some(0x4))` from forward_object's result
        // (the value-source for every minor-GC reference write).
        let r = Self::forward_object_impl(
            young_from,
            young_object_starts,
            young_to,
            old_gen,
            old_ptr,
            objects_copied,
            pointer_map,
            promoted_worklist,
            copy,
        );
        if (r as usize) != 0 && (r as usize) < 0x1000 && copy.diag.gcwrite {
            eprintln!(
                "[gcwrite] forward_object RETURNED 0x{:x} for old_ptr=0x{:x}",
                r as usize, old_ptr as usize,
            );
        }
        r
    }

    #[allow(clippy::too_many_arguments)]
    pub fn forward_object_impl(
        young_from: &Arena,
        young_object_starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        old_ptr: *mut u8,
        objects_copied: &mut usize,
        pointer_map: &mut cratonvm_types::PointerMap,
        promoted_worklist: &mut PromotionQueue,
        copy: CopyPolicy,
    ) -> *mut u8 {
        let CopyPolicy {
            force_promote_all,
            promotion_age,
            diag,
        } = copy;
        // gen r4w5/pinned5: a pinned in-place cycle (see `InPlaceEvac`).
        let in_place = promoted_worklist.in_place.is_some();
        if !young_object_starts.contains(old_ptr as usize) {
            // gen r4w5/pinned5: on a pinned in-place cycle a copy's
            // destination is inside from-space too, so a slot read AFTER it was
            // rewritten (the same card slot seeded twice) names a byte that is
            // no object start. It is already the right answer.
            if in_place
                && promoted_worklist
                    .in_place
                    .as_deref()
                    .is_some_and(|ip| ip.dest_written(old_ptr as usize))
            {
                return old_ptr;
            }
            // Exact pre-GC membership rejects aligned interior words from
            // conservative roots before forwarding writes through them.
            if young_from.contains(old_ptr) {
                // ... but an address INSIDE from-space that the start bitmap
                // does not know is not an interior word from a conservative
                // root: it is an object the pre-GC walk that built the bitmap
                // never saw.
                Self::note_forward_refusal(
                    promoted_worklist.refusal_ledger.as_deref(),
                    old_ptr,
                    "not-an-object-start",
                    young_from,
                    young_object_starts,
                );
            }
            return old_ptr;
        }
        // HIB-DCAST-LATEPHASE.1: `young_object_starts.contains(old_ptr)` above
        // is exact pre-GC membership, but this function's own next comment
        // block already documented the residual risk — "A corrupted header
        // (e.g., slot tag mis-identified a non-pointer bit-pattern as an
        // ObjectRef) would cause forward_object to walk into arbitrary
        // memory" — for `old_ptr` values that reach here as a false
        // conservative root rather than a genuine object start. Validate the
        // raw tag bytes through
        // `object_kind_from_tag` / `array_element_type_from_tag` — the same
        // screen `OldGen::scan_region` and `scan_object_for_old_refs` carry
        // (see `defaultcatalogandschema-late-phase-instability-20260801-FIXED.md`).
        //
        // # gengc-round2-core2 (2026-09-20): re-decided, and the stated reason
        // # corrected.
        //
        // The text this replaces justified the screen by discriminant UB —
        // "Loading an out-of-range discriminant is immediate UB the moment the
        // typed `.read()` happens", citing a `SIGILL` on
        // `DefaultCatalogAndSchemaTest`. That premise is stale. The header
        // quartet lives in the atomic mark word and `ObjectHeader::kind_of` /
        // `ObjectHeader::element_type` are both TOTAL decoders (`0b11` maps to
        // `Object`; an unknown element tag maps to `Reference`). The typed
        // reads at `(*h).kind()` / `(*h).element_type()` below cannot be UB
        // for any byte pattern. See
        // `docs/feature-designs/generative-gc-architecture.md`.
        //
        // The screen is KEPT, on the argument that survived and is still
        // sound: a tag outside the declared sets proves `old_ptr` was never an
        // object base. Totality removed the crash, not the hazard — it turns a
        // garbage tag into a plausible-looking `Reference` array whose garbage
        // `array_length` is then used as a slot count, which is the
        // out-of-bounds read HIB-DCAST-LATEPHASE.1 actually measured.
        //
        // # Why it still fails OPEN, decided rather than inherited
        //
        // This is the one screen of the three whose fail-open exit DANGLES:
        // returning `old_ptr` unmoved records no `pointer_map` entry, and
        // from-space is reset moments later. The fail-CLOSED alternative
        // exists two exits down (`start_walk_complete = false`, i.e. refuse
        // the whole young collection) and would cost one over-retained cycle.
        //
        // It is not taken, for three reasons, and this paragraph is the
        // decision:
        //   1. Reaching this line already means the address passed
        //      `young_object_starts.contains(old_ptr)` — exact pre-GC grid
        //      membership — and then failed a tag screen. A real object cannot
        //      do both, so the address is corrupt or the grid is, and neither
        //      is repaired by retrying the same collection next cycle.
        //   2. The fail-closed exit is per-CYCLE, not per-object. A single
        //      persistently-corrupt word would refuse every young collection
        //      for the rest of the run, i.e. convert a bounded dangle into an
        //      unbounded heap exhaustion, and `note_skipped_young_cycle`'s
        //      trigger floor defers rather than abandons — allocation failure
        //      would be the only backstop left.
        //   3. The refusal is not silent: `note_forward_refusal` records it
        //      and `CRATONVM_DBG_ROOT_REMAP_AUDIT=1` prints the header bytes
        //      and the nearest recorded object start.
        //
        // Expected value: `forward_refusal_reason("invalid-kind-or-element-tag")`
        // is ZERO on a healthy run. A non-zero reading names an address that
        // is about to dangle and is a finding, not tuning.
        //
        // gen r4w4/young4 (2026-09-24): ONE mark-word snapshot drives the tag
        // screen and the owned header below, the way `gen_evac::ParEvac::
        // evacuate` has always done it. The quartet (kind / element_type /
        // gc_age / gc_flags) lives in the mark word, so the two raw tag reads
        // here plus the four quartet getters and the final load the owned-
        // header build used to make were six loads of the same word per
        // copied object. Under stop-the-world nothing else writes it, so every
        // one of them returned this value.
        // SAFETY: `old_ptr` is confirmed inside young from-space via
        // `young_object_starts.contains` above — an exact pre-GC object start —
        // so a whole mapped, 8-aligned header lies there; the mark word is an
        // `AtomicU64` valid for any bit pattern, read atomically through a
        // field projection. No reference outlives this expression.
        let mark_snapshot: u32 = unsafe {
            (*(old_ptr as *const ObjectHeader))
                .mark_word
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        let kind_tag = ObjectHeader::kind_tag(mark_snapshot);
        let elem_tag = ObjectHeader::element_type_tag(mark_snapshot);
        if object_kind_from_tag(kind_tag).is_none()
            || !cratonvm_types::element_tag_ok(kind_tag, elem_tag)
        {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                old_ptr = ?old_ptr,
                kind_tag,
                elem_tag,
                backtrace = ?std::backtrace::Backtrace::capture(),
                "gen_heap::forward_object: invalid kind/element_type tag — false root or \
                 corrupted header, not decoded as ObjectHeader",
            );
            Self::note_forward_refusal(
                promoted_worklist.refusal_ledger.as_deref(),
                old_ptr,
                "invalid-kind-or-element-tag",
                young_from,
                young_object_starts,
            );
            // gcj j2/pn2: a refused pinned object counts as reached.
            if let Some(ip) = promoted_worklist.in_place.as_deref_mut() {
                ip.note_refused_forward(old_ptr as usize);            }
            return old_ptr;
        }

        // SAFETY: `old_ptr` points to a live young-gen object; its header is
        // valid. Build an owned *copy* of the header rather than holding a
        // shared `&ObjectHeader`: later in this function we install the
        // forwarding pointer through a `&mut`/raw write to the same address,
        // and a live `&` aliasing that write would be undefined behavior.
        //
        // ATOMIC-UB fix: do NOT `std::ptr::read` the whole `ObjectHeader` by
        // value. `ObjectHeader` embeds `mark_word: AtomicU64`; a `ptr::read`
        // of a struct containing an atomic performs a *non-atomic* read of an
        // atomic location, which is undefined behavior. Instead read each
        // scalar field individually through field-projected raw pointers, and
        // read `mark_word` via an explicit `AtomicU64::load`, then reconstruct
        // an owned header from those values.
        // SAFETY: `old_ptr` points at a live young-gen object, so each field of its
        // `ObjectHeader` is initialized and individually readable via addr_of! reads;
        // no aliasing `&` is held while we later install the forwarding pointer. The
        // kind/element_type tag bytes were just validated above, so reading them as
        // typed enums here is sound.
        //
        // gen r4w4/young4: the owned header's mark word is `mark_snapshot`,
        // the same value the tag checks above validated, not a second read.
        let header: ObjectHeader = unsafe {
            let owned = ObjectHeader::snapshot(old_ptr as *const ObjectHeader);
            owned
                .mark_word
                .store(mark_snapshot, std::sync::atomic::Ordering::Relaxed);
            owned
        };
        let header = &header;

        // KC16 SIGSEGV audit: sanity-check header before using it. A corrupted
        // header (e.g., slot tag mis-identified a non-pointer bit-pattern as an
        // ObjectRef) would cause forward_object to walk into arbitrary memory.
        // Emitting forensic output here converts a silent SIGSEGV into a visible
        // diagnostic.
        //
        // Multi-array reloc fix (2026-05-22): the `num_slots > 1<<24` clause
        // used to trip for ANY array whose length exceeds 2^24 elements,
        // because `alloc_array` stores the array length in BOTH
        // `array_length` and `num_slots`. With three back-to-back 2^26-int
        // arrays the third allocation triggers a minor GC; `forward_object`
        // would then refuse to relocate the first two large arrays, return
        // their old pointers without inserting into the pointer map, and
        // young_from.reset() would zero the original headers. After the
        // semispace swap, the still-unremapped roots dereferenced the now-
        // zeroed memory, giving the all-zero `kind/class_id/element_type/
        // array_length` symptom and a `.length` of 0. Guard num_slots only
        // for non-arrays (where it really is a field count). For arrays,
        // gate on `array_length` against the JVM's i32::MAX ceiling — the
        // same cap as `MAX_REASONABLE_ARRAY_LEN` in `array_length()`.
        let kind_byte = ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed));
        let is_array = header.kind() == ObjectKind::Array;
        let array_length_too_large = is_array && header.array_length() > i32::MAX as u32;
        let num_slots_too_large = !is_array && header.num_slots() > (1 << 24);
        if kind_byte > 1 || num_slots_too_large || array_length_too_large {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                old_ptr = ?old_ptr,
                kind_byte,
                num_slots = header.num_slots(),
                array_length = header.array_length(),
                class_id = ?header.class_id,
                gc_flags = format!("{:x}", header.gc_flags()),
                backtrace = ?std::backtrace::Backtrace::capture(),
                "gen_heap::forward_object: suspect header",
            );
            // Leave the object unmoved; this is a suspected false root or
            // corrupted slot. Returning old_ptr preserves progress while the
            // eprintln above gives us the evidence needed to diagnose.
            Self::note_forward_refusal(
                promoted_worklist.refusal_ledger.as_deref(),
                old_ptr,
                "suspect-header",
                young_from,
                young_object_starts,
            );
            // gcj j2/pn2: a refused pinned object counts as reached.
            if let Some(ip) = promoted_worklist.in_place.as_deref_mut() {
                ip.note_refused_forward(old_ptr as usize);            }
            return old_ptr;
        }

        // gen r4w5/pinned5: a PINNED object forwards to itself. The first
        // encounter sets its side bit, records the identity in `pointer_map`
        // (reference processing, finalizer resurrection and the loader rescue
        // all read "in the map" as "survived") and queues it for its in-place
        // scan; every later one returns it. A survivor the in-place copy could
        // place nowhere (`kept`) is the same kind of identity.
        if let Some(ip) = promoted_worklist.in_place.as_deref_mut() {
            if let Some(i) = ip.pinned_index(old_ptr as usize) {
                if !ip.visited[i] {
                    ip.visited[i] = true;
                    ip.scan.push(old_ptr);
                    pointer_map.insert(old_ptr as usize, old_ptr as usize);
                    // t1 w8 PIN10 / gcki-pin-age: age it where it stays,
                    // once, at first visit.
                    if promoted_worklist.in_place_tenure_age.is_some() {
                        let size = gen_object_total_size(header);
                        Self::age_in_place_survivor_serial(old_ptr, size, promoted_worklist);
                    }
                }
                return old_ptr;
            }
            // (`kept_count` first: the set is empty on every cycle whose free
            // spans and old gen could take every survivor, which is nearly all
            // of them, and then no forward pays the hash probe.)
            if ip.kept_count > 0 && ip.kept.contains(&(old_ptr as usize)) {
                return old_ptr;
            }
        }

        // Already forwarded?
        if header.is_forwarded() {
            let fwd = header.forwarding_address();
            // KC16 SIGSEGV audit: verify the forwarding address is sane before
            // returning it. A stale forwarding pointer left over from a prior
            // GC cycle that wasn't cleared by session 73's fix would send
            // callers to freed memory.
            //
            // BUG-Z fix: the null/alignment check below is NOT enough — under
            // heavy multi-threaded churn (TestFileStoreConcurrency) the
            // `forwarding_ptr` field of a young_from source has been observed
            // holding 8-aligned non-null GARBAGE (small integers like 0x10/0x1110,
            // or `old - small_offset`) that passes those two checks and then gets
            // recorded into `pointer_map`, sending the GC's post-copy walk (and
            // `update_all_roots`) into a wild pointer → SIGSEGV. A real
            // forwarding address must land inside the to-space (`young_to`) or
            // the old gen; reject anything else as a stale/corrupt forward
            // (return `old_ptr` unmoved, exactly as the null/unaligned arm does)
            // rather than trusting the bogus pointer and recording it.
            // gen r4w5/pinned5: on a pinned in-place cycle a forward may land in
            // from-space itself (a free span), and only then.
            let fwd_in_heap = young_to.contains(fwd as *const u8)
                || old_gen.contains(fwd as *const u8)
                || (in_place && young_from.contains(fwd as *const u8));
            if fwd.is_null() || (fwd as usize) % 8 != 0 || !fwd_in_heap {
                tracing::debug!(
                    target: "cratonvm::gc::guard",
                    fwd = ?fwd,
                    old_ptr = ?old_ptr,
                    class_id = ?header.class_id,
                    gc_flags = format!("{:x}", header.gc_flags()),
                    gc_age = header.gc_age(),
                    backtrace = ?std::backtrace::Backtrace::capture(),
                    "gen_heap::forward_object: bad forwarding_ptr",
                );
                // gcd d2/h: a refusal like the three named ones (the address
                // stays unmoved, with no map entry), so it goes in the same
                // ledger `forward_refusal_reason` answers from. It was the one
                // exit a post-GC verifier would call NEVER-OFFERED.
                Self::note_forward_refusal(
                    promoted_worklist.refusal_ledger.as_deref(),
                    old_ptr,
                    "bad-forwarding-target",
                    young_from,
                    young_object_starts,
                );
                return old_ptr;
            }
            // Ensure pointer_map has this entry so update_all_roots can update
            // all references to this old address, even if this is a second
            // encounter of the same object (e.g., root + dirty card + Cheney scan).
            //
            // gengc-round4-move (2026-09-23): only for a forward that may NOT
            // already be in the map. A target inside young TO-space was
            // installed by THIS cycle — to-space starts every moving cycle
            // empty, and the only writers into it are this function (which
            // inserts right after it installs, below) and the parallel
            // evacuator (whose shards are merged into `pointer_map` before
            // any serial call that can follow it, i.e. Phase 2.5) — so its
            // entry is present and the probe is pure cost: one random-access
            // hash lookup per re-encounter, the `fwd_reencounters` term of
            // `cheney_drain`. An OLD-gen target keeps the probe: it may be a
            // selective-promotion forward from an earlier non-moving cycle,
            // for which this is the only place the mapping is recorded.
            if diag.tally_arm {
                copy_tally_arm(true);
            }
            // gen r5w6/pin10: the same rule for a pinned in-place cycle's
            // from-space destinations, which `gen_evac::ParEvac::evacuate`
            // already applies to its `in_place_target`. On such a cycle a
            // FORWARDED from-space header whose target is ALSO in from-space
            // was forwarded by this cycle: pinned and kept objects are never
            // marked on the serial arm (and the parallel arm's self-forward
            // claims are undone by `absorb_parallel_in_place` before any serial
            // forward), a previous cycle's vacated copies were zeroed or
            // quarantine-stamped by its rebuild, and every copy into a span
            // inserts its pair right after the install below (or, from the
            // workers, in `map_merge`, which precedes every serial forward on
            // that arm). So the entry is present and the probe is pure cost.
            let installed_this_cycle = young_to.contains(fwd as *const u8)
                || (in_place && young_from.contains(fwd as *const u8));
            if !installed_this_cycle {
                pointer_map.entry(old_ptr as usize).or_insert(fwd as usize);
            }
            return fwd;
        }

        let total_size = gen_object_total_size(header);

        // Sanity: skip objects whose computed extent does not fit entirely
        // within the young from-space. This guards against false conservative
        // roots from JIT spill slots that bit-match a heap address but don't
        // point at a real object — a bogus header yields a size that runs off
        // the end of the arena. A genuine object, however large, fits within
        // the arena it was allocated from by construction, so this never
        // rejects a real live array (a fixed byte cap did: a 64 MB int[] is
        // perfectly valid at a multi-GB heap).
        let from_base = young_from.base_ptr() as usize;
        let from_end = from_base + young_from.capacity();
        let obj_addr = old_ptr as usize;
        let fits_in_arena = obj_addr >= from_base
            && obj_addr
                .checked_add(total_size)
                .is_some_and(|obj_end| obj_end <= from_end);
        if total_size < HEADER_SIZE || !fits_in_arena {
            tracing::warn!(
                "GC: skipping suspected false root at {:p} (computed size {} bytes, \
                 kind={:?}, num_slots={}, array_len={})",
                old_ptr,
                total_size,
                header.kind(),
                header.num_slots(),
                header.array_length(),
            );
            // gcd d2/h: recorded like the other refusal exits (see the
            // bad-forwarding-target arm above).
            Self::note_forward_refusal(
                promoted_worklist.refusal_ledger.as_deref(),
                old_ptr,
                "extent-outside-from-space",
                young_from,
                young_object_starts,
            );
            return old_ptr; // Leave unmoved — likely not a real object
        }

        // bc math-ec 0x4 (2026-06-09): a STALE/INTERIOR pointer whose garbage
        // bytes PARSE plausibly (kind 0/1, small num_slots — common when the
        // bytes are long[] math data) slips past every guard above; the copy
        // is wasteful but the killer is the forwarding-pointer install below:
        // an 8-byte raw write at old_ptr+24 INTO THE MIDDLE OF A LIVE OBJECT
        // (gen r5w6/pin10: since the 2026-08-06 header shrink the target goes
        // to the SECOND word, `old_ptr + FORWARDING_TARGET_OFFSET` = +8, and
        // the mark word's state changes too; the hazard is the same),
        // corrupting whatever field/element lives there. One seed then
        // self-propagates: the corrupted cell feeds the next GC another false
        // root. Discriminator: every REAL object's class_id resolves in the
        // registry; garbage class_ids essentially never do. (class_id==0 ==
        // java/lang/Object resolves and stays allowed — zeroed-region refs are
        // handled by the existing guards.)
        // `CRATONVM_DBG_FWDGUARD` logs offenders; `CRATONVM_FWD_RESOLVE_STRICT`
        // rejects them (leave unmoved, no forwarding install) — candidate FIX.
        if diag.fwdguard || diag.fwd_resolve_strict {
            let cid = header.class_id.as_u32();
            if cratonvm_gc_core::gc::resolve_class_info(cid).is_none() {
                if diag.fwdguard {
                    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
                    static N: AtomicUsize = AtomicUsize::new(0);
                    let k = N.fetch_add(1, AOrd::Relaxed);
                    if k < 24 {
                        eprintln!(
                            "[fwdguard] #{k} UNRESOLVABLE class_id={} at old_ptr=0x{:x} \
                             (kind_byte={} num_slots={} array_len={} total_size={}) — {}",
                            cid,
                            old_ptr as usize,
                            ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                            header.num_slots(),
                            header.array_length(),
                            total_size,
                            if diag.fwd_resolve_strict {
                                "REJECTED"
                            } else {
                                "copied anyway"
                            },
                        );
                    }
                }
                if diag.fwd_resolve_strict {
                    return old_ptr; // false root — never install a forward in it
                }
            }
        }
        // Promote if this GC survival would reach or exceed the promotion age,
        // OR if the previous minor GC observed high survival pressure and
        // armed the force-promote-all flag. The latter breaks the death
        // spiral where a long-lived object is repeatedly copied semi→semi
        // because its age hasn't yet reached PROMOTION_AGE.
        //
        // E.g., with PROMOTION_AGE=3: an object at age 2, surviving this GC,
        // would become age 3 → promote instead.
        //
        // One rule, one implementation: `should_tenure` is shared with
        // `gen_evac::ParEvac::evacuate` and the non-moving sweep's
        // selective-promotion pass. It is where the `saturating_add` lives and
        // where the age-255 boundary is tested; see its doc for why a bare
        // `+ 1` here was a collector-internal overflow.
        //
        // gen r4w6/young6: `promotion_age` is the cycle's (`CopyPolicy`) —
        // `PROMOTION_AGE` by default, the adaptive threshold + 1 when adaptive
        // tenuring is engaged.
        //
        // gci i1/yg: against the cycle's HotSpot age-clock step
        // (`promoted_worklist.age_step`, 1 unless the clock is armed), which
        // is also what the destination's age grows by below.
        let age_step = promoted_worklist.age_step;
        let should_promote = should_tenure(
            header.gc_age(),
            force_promote_all,
            clock_promotion_age(promotion_age, age_step),
        );

        let new_ptr = if in_place {
            // gen r4w5/pinned5: the pinned in-place cycle never touches
            // to-space. A survivor goes where its age says — old gen, or a
            // from-space span that was free at the start of the cycle — then
            // to the other one, and when neither can take it, it STAYS: an
            // identity forward, like a pinned object, instead of the Cheney
            // path's abort. Staying is sound here because from-space is not
            // reset; the rebuild keeps every `live` object.
            match Self::in_place_destination(promoted_worklist, old_gen, total_size, should_promote)
            {
                Some(p) => p,
                None => {
                    if let Some(ip) = promoted_worklist.in_place.as_deref_mut() {
                        ip.kept.insert(old_ptr as usize);
                        ip.kept_count += 1;
                        ip.scan.push(old_ptr);
                        ip.live.push((old_ptr as usize, total_size));
                    }
                    pointer_map.insert(old_ptr as usize, old_ptr as usize);
                    // t1 w8 PIN10 / gcki-pin-age: a kept survivor ages where
                    // it stays, once (a later encounter takes the `kept`
                    // early return).
                    Self::age_in_place_survivor_serial(old_ptr, total_size, promoted_worklist);
                    return old_ptr;
                }
            }
        } else if should_promote {
            // Promote to old gen.
            //
            // UNZEROED (gengc-round4-move, 2026-09-23), as the parallel
            // evacuator's promotion path has been since gen-gc-five item 4.
            // `OldGen::alloc` memsets the block and the `copy_nonoverlapping`
            // below then overwrites every one of those `total_size` bytes — a
            // second full pass over each promoted object for nothing. The
            // `alloc_unzeroed` contract ("no walker may reach the block
            // before it holds objects end to end") holds: nothing between
            // here and the copy reads old gen, the collector is
            // stop-the-world, and `total_size` is already 8-aligned
            // (`gen_object_total_size` rounds), so `alloc_from_buckets` hands
            // back exactly `total_size` bytes with the remainder on the free
            // list.
            //
            // gen r4w4/young4: through the serial promotion buffer
            // (`PromotionQueue::alloc`), which carves unzeroed buffers the same
            // way and keeps the same contract — its tail is returned by
            // `retire_plab` before the first old-gen walk after the drain.
            match promoted_worklist.alloc(old_gen, total_size) {
                Some(ptr) => ptr,
                None => {
                    // Old gen full — fall back to young to-space
                    // (Major GC will be needed later)
                    match young_to.alloc(total_size, 8) {
                        Some(ptr) => ptr,
                        None => {
                            // UNREACHABLE by construction since the semispace
                            // equalisation + headroom check in
                            // `collect_garbage_inner` (see the "Cheney
                            // invariant" block): to-space is guaranteed to be
                            // able to hold ALL of from-space before the copy
                            // phase starts, and promotion to old gen only ever
                            // removes demand from it. Kept as the backstop for
                            // a future regression of that guarantee.
                            //
                            // We cannot degrade gracefully HERE: returning
                            // `old_ptr` leaves the object in from-space, which
                            // the caller resets immediately after this
                            // collection — every still-live reference would
                            // dangle (use-after-free). A copying collector has
                            // no valid address to hand back for an unrelocated
                            // object, which is exactly why the guarantee is
                            // made up-front instead.
                            eprintln!(
                                "FATAL: GC could not relocate a live object — both old gen and \
                                 young to-space are full during promotion (tried {} bytes, \
                                 to-space {}/{} used). This is an invariant violation: the \
                                 to-space >= from-space guarantee in collect_garbage_inner \
                                 should have made this unreachable.",
                                total_size,
                                young_to.used(),
                                young_to.capacity(),
                            );
                            std::process::abort();
                        }
                    }
                }
            }
        } else {
            // Copy to young to-space -- gcj yg3-residuals R3: unless that would
            // take it past the geometry's survivor cap, where the survivor is
            // tenured first (HotSpot's premature promotion) and kept young only
            // when old gen refuses (`geometry_cap_destination`).
            match geometry_cap_destination(promoted_worklist, old_gen, young_to.used(), total_size)
                .or_else(|| young_to.alloc(total_size, 8))
            {
                Some(ptr) => ptr,
                // gcj j3/yg3 (`CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES`, default
                // ON, `=0` kills): a survivor to-space cannot take is tenured,
                // as the parallel arm does (`PAR_EVAC_OVERFLOW_PROMOTIONS`) and
                // HotSpot's DefNew does on a survivor overflow ("premature
                // promotion"). Downstream keys on `landed_in_old` (the old-gen
                // mark, the promoted worklist push, the age census), exactly as
                // for the promotion arm, so the copy is scanned like any
                // promoted survivor. Unreachable while the Cheney invariant
                // holds; the prerequisite of the eden + survivor geometry
                // (`gcj-j1yg-proposal-serial-young-geometry-and-no-pause-goal`
                // part 1). The flag is read only here, on the overflow.
                None => match cheney_overflow_destination(promoted_worklist, old_gen, total_size) {
                    Some(ptr) => ptr,
                    None => {
                        // UNREACHABLE by construction — see the identical case in
                        // the promotion branch above and the "Cheney invariant"
                        // block in `collect_garbage_inner`. Returning `old_ptr`
                        // here would leave the object in from-space, which the
                        // caller resets right after this collection, dangling every
                        // live reference to it; that is why the to-space >=
                        // from-space guarantee is established BEFORE the copy phase
                        // rather than patched up here.
                        eprintln!(
                            "FATAL: GC could not relocate a live object — young to-space is full \
                             (tried {} bytes, to-space has {}/{} used). This is an invariant \
                             violation: the to-space >= from-space guarantee in \
                             collect_garbage_inner should have made this unreachable.",
                            total_size,
                            young_to.used(),
                            young_to.capacity(),
                        );
                        std::process::abort();
                    }
                },
            }
        };

        // perf/gc-oracle-anchors (2026-07-25): a Cheney destination inside
        // to-space is an object base, and after the from/to swap that arena
        // becomes the next collection's from-space. Anchoring it here keeps
        // the opt-in moving-young path from starting each epoch with an empty
        // anchor table (which is correct — every consumer falls back — but
        // gives up the parallel sweep for no reason). No-op for old-gen
        // destinations, which are a different arena.
        if young_to.contains(new_ptr) {
            let off = new_ptr as usize - young_to.base_ptr() as usize;
            young_to.note_object_start(off);
        }
        // Where the object ACTUALLY landed, asked once (gen r4w2/youngpolicy:
        // this was three `old_gen.contains(new_ptr)` calls below). The
        // promotion branch falls back to to-space when old gen is full, so
        // `should_promote` is not the answer.
        let landed_in_old = old_gen.contains(new_ptr);

        // Copy the entire object
        // gcstress face-1 hunt (no-op unless gated): validate the source body
        // (see promote-src) and dump the SOURCE qword pair at the watched
        // offset, so a copy that IMPORTS corrupt content is distinguishable
        // from one that copies a clean cell.
        if diag.cell_corrupt {
            // SAFETY: `old_ptr` is the live source object under STW.
            validate_copy_source_cells(
                old_ptr,
                unsafe { &*(old_ptr as *const ObjectHeader) },
                "cheney-src",
            );
        }
        {
            let w = diag.cell_watch;
            let dst = new_ptr as usize;
            if w != 0 && dst <= w && w.wrapping_sub(dst) < total_size {
                let src_at = old_ptr as usize + (w - dst);
                // SAFETY: src object spans total_size bytes; src_at is within it.
                let pair = unsafe { std::ptr::read(src_at as *const [u64; 2]) };
                cratonvm_gc_core::heap::cell_watch_check(
                    dst,
                    total_size,
                    "cheney-copy",
                    &format!(
                        "src=0x{:x} src[watch]=0x{:016x},0x{:016x}",
                        old_ptr as usize, pair[0], pair[1]
                    ),
                );
            }
        }
        // Phase H statistics, at the one place that already knows both the
        // size and the destination arena, classified by `landed_in_old`.
        copy_tally_add(landed_in_old, total_size as u64);
        if diag.tally_arm {
            copy_tally_arm(false);
        }
        if diag.obj_watch {
            note_obj_watch_copy(header, old_ptr, new_ptr, total_size, landed_in_old);
        }
        // gen r5w2/roots6 (`CRATONVM_DBG=root-write-audit`): the destination
        // is to-space, old gen, or -- on a pinned in-place cycle only -- a
        // from-space span; anything else aborts here, at the writer.
        if cratonvm_gc_core::root_write_audit::enabled() {
            let to_base = young_to.base_ptr() as usize;
            let mut regions = vec![(to_base, to_base + young_to.capacity()), old_gen.extent()];
            if in_place {
                regions.push((from_base, from_end));
            }
            cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                cratonvm_gc_core::root_write_audit::HeapWriteKind::EvacuationCopy,
                new_ptr as usize,
                total_size,
                &regions,
            );
        }
        // SAFETY: `old_ptr` and `new_ptr` are valid, non-overlapping regions of `total_size` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(old_ptr, new_ptr, total_size);
        }
        // DBG (bc math-ec, CRATONVM_DBG_GCWRITE): does the GC object copy
        // produce a small (<0x1000) object-field payload that the SOURCE did
        // not have? That would mean the copy truncated / used a wrong size and
        // the destination field holds stale to-space bytes (the 0x4). If the
        // source ALSO has the small payload, the 0x4 pre-existed in from-space.
        // Legacy 16-byte-cell objects only (gengc-round4-move, 2026-09-23): a
        // compact-layout object's `num_slots` counts fields over a PACKED
        // body, so `HEADER_SIZE + si * SLOT_SIZE` strides past its end — into
        // to-space the copy phase has not written yet, or past the committed
        // part of it.
        if !is_array && !is_compact_object(header) && diag.gcwrite {
            let ns = header.num_slots() as usize;
            for si in 0..ns {
                // SAFETY: `si < ns == header.num_slots()`, so the slot lies in the freshly
                // copied object's field area at `new_ptr`; the `Value` read is in-bounds.
                let dv = unsafe {
                    std::ptr::read(new_ptr.add(HEADER_SIZE + si * SLOT_SIZE) as *const Value)
                };
                if let Value::Object(Some(r)) = dv {
                    let p = r.as_ptr() as usize;
                    if p != 0 && p < 0x1000 {
                        // SAFETY: same `si < num_slots` bound applies to the source object
                        // at `old_ptr`, so reading the matching slot is in-bounds.
                        let sv = unsafe {
                            std::ptr::read(old_ptr.add(HEADER_SIZE + si * SLOT_SIZE) as *const Value)
                        };
                        let src_small = matches!(sv, Value::Object(Some(rr))
                            if { let q = rr.as_ptr() as usize; q != 0 && q < 0x1000 });
                        eprintln!(
                            "[gcwrite] COPY fld[{}]=0x{:x} src_small={} cid={} num_slots={} total_size={} new=0x{:x}",
                            si, p, src_small, header.class_id.as_u32(), ns, total_size, new_ptr as usize,
                        );
                    }
                }
            }
        }

        // Round-2 fix (T2-4): explicit atomic load+store for the mark_word
        // field. The bulk memcpy above is technically UB for `AtomicU64`:
        // even under STW (no mutator is racing), the memory model still
        // requires that reads/writes of atomic locations go through atomic
        // ops. Replicating the mark word atomically materializes the
        // correct happens-before edge for any observer that may later
        // perform a CAS on the new copy (monitor inflation, etc.).
        //
        // NOTE for future concurrent-GC support: this STW-only protocol is
        // not sufficient for a concurrent collector. A concurrent
        // forwarding protocol must instead CAS-install the forwarding
        // pointer and re-read the mark word if a mutator raced.
        // gen r4w4/young4: from `mark_snapshot` — nothing has written the
        // source header since it was taken (stop-the-world, one worker, and
        // the forwarding install is below).
        //
        // gen r4w5/pinned5: `landed_in_old` alone, no longer `should_promote &&
        // landed_in_old`. On the Cheney path the two are equal (a survivor
        // that should not tenure goes to to-space, never to old gen); on the
        // pinned in-place path a young survivor OVERFLOWS to old gen when the
        // free spans run out, and it must then be flagged old and queued for
        // the promoted-object scan like any other promotion.
        //
        // gen r5w6/pin10 (the pinned5 residual 2, "the destination RMW"): the
        // old-gen flag or the incremented age is folded into the snapshot
        // BEFORE the one store, instead of a store followed by `add_gc_flags`
        // (a locked `fetch_or`) or `set_gc_age` (a compare-exchange loop) on a
        // header no other thread can see yet. `mark_with_gc_flags` /
        // `mark_with_gc_age` compute exactly the word those RMWs left
        // (`the_pure_mark_helpers_agree_with_the_setters` in `types`), so
        // the destination header is bit-identical; one locked instruction per
        // survivor is gone.
        let landed_in_old_gen = landed_in_old;
        let dest_mark = if landed_in_old_gen {
            ObjectHeader::mark_with_gc_flags(mark_snapshot, GC_FLAG_OLD_GEN)
        } else {
            // gc wave 9 (lane gcC): `cycle_age_step` is 1 except on a cycle
            // the Serial-eden aging epoch holds.
            ObjectHeader::mark_with_gc_age(
                mark_snapshot,
                // gci i1/yg: the cycle's age step (`promoted_worklist.age_step`:
                // the clock's step, or gcC's `cycle_age_step`, folded by
                // `merged_age_step`; 1 when neither is armed).
                ObjectHeader::gc_age_of(mark_snapshot).saturating_add(age_step),
            )
        };
        // SAFETY: `new_ptr` points at a fully written ObjectHeader (the copy
        // above) whose `mark_word` field lives at MARK_WORD_OFFSET.
        unsafe {
            let new_header_ptr = new_ptr as *mut ObjectHeader;
            (*new_header_ptr)
                .mark_word
                .store(dest_mark, std::sync::atomic::Ordering::Relaxed);
        }
        // gen r4w6/young6: the age table (HotSpot's `AgeTable::add`), counted
        // at the NEW age, only when adaptive tenuring or the distribution
        // print reads it.
        if !landed_in_old_gen && promoted_worklist.age_census {
            promoted_worklist.note_survivor_age(ObjectHeader::gc_age_of(dest_mark), total_size);
        }

        // Install forwarding pointer in the old header.
        // SAFETY: `old_ptr` is a valid young-gen object; writing the forwarding
        // pointer into its header is safe. Written through a raw pointer so no
        // `&mut ObjectHeader` is ever live alongside another reference to this
        // header (we earlier read an owned copy rather than borrowing it).
        unsafe {
            (*(old_ptr as *const ObjectHeader)).set_forwarding_address(new_ptr);
        }

        pointer_map.insert(old_ptr as usize, new_ptr as usize);
        *objects_copied += 1;
        // gen r5w4/pin8: on a pinned in-place cycle under
        // `CRATONVM_DBG_STALE_OBJREF`, the source span this copy vacated is
        // quarantined by the rebuild rather than re-served.
        if let Some(ip) = promoted_worklist.in_place.as_deref_mut() {
            if ip.quarantine {
                ip.vacated.push((old_ptr as usize, total_size));
            }
        }
        if diag.desc_trace {
            if let Some((cname, _)) = cratonvm_gc_core::gc::resolve_class_info(header.class_id.as_u32()) {
                if cname == "java/util/concurrent/ConcurrentLinkedQueue" {
                    eprintln!(
                        "[desctrace-fwd] {} ihash={} old=0x{:x} new=0x{:x} promoted={} age={} jit_active={} moving_young={}",
                        cname,
                        header.installed_identity_hash(),
                        old_ptr as usize,
                        new_ptr as usize,
                        landed_in_old_gen,
                        header.gc_age(),
                        cratonvm_gc_core::gc_quiescence::is_active(),
                        cratonvm_gc_core::gc_quiescence::moving_young_enabled(),
                    );
                }
            }
        }
        // CRIT-P2 fix: enqueue promoted objects so the alternating Cheney
        // loop can scan them in O(1) per object instead of re-filtering
        // `pointer_map.values()` per iteration. Young to-space copies are
        // already handled by the bump-cursor Cheney scan in the caller, so
        // we only enqueue when the object actually landed in old gen.
        if landed_in_old_gen {
            promoted_worklist.push(new_ptr);
        } else if let Some(ip) = promoted_worklist.in_place.as_deref_mut() {
            // gen r4w5/pinned5: a copy into a from-space free span. The linear
            // to-space scan never sees it, so it is queued for an in-place
            // scan, and it is `live` for the rebuild.
            ip.scan.push(new_ptr);
            ip.live.push((new_ptr as usize, total_size));
            ip.copies += 1;
        }

        debug_assert!(young_from.contains(old_ptr));

        new_ptr
    }

}
