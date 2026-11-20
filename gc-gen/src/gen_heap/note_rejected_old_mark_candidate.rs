#[allow(unused_imports)]
use super::*;

/// Cold reporter for a rejected old-gen mark candidate.
#[cold]
#[inline(never)]
pub fn note_rejected_old_mark_candidate(ptr: *mut u8, old_gen: &OldGen, site: &'static str) {
    let n = OLDMARK_REJECTED_CANDIDATES.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        // gen r4/mark (2026-09-23): the dump used to read 24 bytes at `ptr`
        // unconditionally, on the argument that "`ptr` is inside old gen, so the
        // 24 bytes a header occupies are mapped". Only the FIRST byte is known
        // to be inside: a rejected candidate within 23 bytes of the end of the
        // backing store read past the `Vec` — and a rejected candidate is, by
        // definition, not a header whose extent anyone has checked. Words that
        // would cross the end are reported as zero. The end is the high-water
        // mark rather than the capacity: bytes above it were never written by
        // any allocation (see `report_doomed_referrers`).
        //
        // gen r4w6/oldpin6 (2026-09-24): and within the COMMITTED run that
        // holds `ptr`, not `[base, base + high_water)`. A shrink keeps
        // `high_water` above the decommitted tail, and an UNALIGNED candidate
        // just below the committed end (`keep_end - 17`, say) passed the old
        // bound with its third word crossing into a decommitted granule
        // (`gengc-r4w5-review5-old-gen-diagnostics-scan-to-high-water-past-a-shrink`).
        // A candidate in no committed run reads nothing.
        let at = ptr as usize;
        let (lo, hi) = old_gen
            .committed_spans_below(old_gen.high_water())
            .into_iter()
            .map(|(a, len)| (a, a.saturating_add(len)))
            .find(|&(a, e)| at >= a && at < e)
            .unwrap_or((0, 0));
        let w = header_words_within(at, lo, hi);
        tracing::warn!(
            "old-gen mark: rejecting {} candidate {:p} — not a plausible object \
             base (aligned={}, w0=0x{:016x} w1=0x{:016x} w2=0x{:016x}). A side \
             table or reference slot is holding a stale old-gen address; see \
             gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md",
            site,
            ptr,
            (ptr as usize) & 7 == 0,
            w[0],
            w[1],
            w[2],
        );
    }
}

/// t1 wave 4 lane G2 (`CRATONVM_GC_OLD_PAR_MARK`, default ON since 2026-10-27) — what one parallel
/// old-mark drain reads: every field is fixed for the pause (the walk, the
/// start set, the side-table snapshots) or atomic (the mark word, the gap
/// counter). Built by `old_gen_gc_inner` right before each drain and handed to
/// the workers through `old_gen::par_mark::AssertSync`.
pub struct ParMarkCtx<'a> {
    pub old_gen: &'a OldGen,
    pub walked: &'a [(*mut u8, usize)],
    pub starts: Option<&'a WalkedStarts>,
    /// `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS` on a true-root major.
    pub rounds_in_bfs: bool,
    /// A true-root major (`true_young` is `Some`).
    pub track_young: bool,
    pub bfs_lo: usize,
    pub bfs_hi: usize,
    pub bfs_young_loaders: &'a FxHashMap<u32, usize>,
    pub loader_pin_on: bool,
    pub rk_loader_rows: Option<&'a FxHashMap<u32, usize>>,
    pub old_mark_loader_rows: Option<&'a FxHashMap<u32, usize>>,
    pub mirror_pin_owners: &'a FxHashSet<usize>,
    pub rk_mirror_rows: Option<&'a FxHashMap<usize, Vec<usize>>>,
    pub metadata_pins: Option<&'a FxHashMap<usize, Vec<usize>>>,
    /// `TrueRootYoung::promoted_by_dest` (empty map: `None`).
    pub promoted_by_dest: Option<&'a FxHashMap<usize, usize>>,
    /// The true-root trace's deferred old targets (`TrueRootYoung::defer_old`),
    /// UNMARKED: the drain's null-array chunk tasks hand ranges of it to
    /// [`par_mark_scan_chunk`], which offers each to the push site.
    pub candidates: &'a [usize],
    /// t1 w8 J1OG -- the w7 child-header prefetch before each whole-object
    /// scan (`CRATONVM_GC_OLD_PAR_MARK_PREFETCH`, default ON, `=0` kills).
    pub prefetch: bool,
}

/// t1 G2 — one worker's output of a parallel old-mark drain, folded into the
/// true-root trace by the driver after the join (in worker order; every
/// consumer is order-independent).
#[derive(Default)]
pub struct ParMarkOut {
    /// Flat `[obj, n, t_1 .. t_n]` records: a scanned object whose young
    /// targets (or a promotion destination's source-keyed rows) the serial
    /// BFS would hand to `TrueRootYoung::seed_from_bfs` at its pop.
    pub young_recs: Vec<usize>,
    /// Objects the serial BFS would queue on `TrueRootYoung::pending_old`.
    pub pending_old: Vec<usize>,
    /// `loader_pin` snapshot lookups (`OldGen::note_loader_snapshot_lookups`).
    pub loader_lookups: u64,
    /// Per-pop scratch (the serial loop's `bfs_young`).
    pub bfs_young: Vec<usize>,
}

/// t1 G2 — the serial BFS's per-pop body (`old_gen_gc_inner`, outside the
/// finalizer resurrection round), for one worker of the parallel drain: the
/// slot scan, the overlay / loader / mirror / metadata edges, and the young
/// targets recorded for the driver instead of seeded in place. Every mark
/// goes through `mark_and_push_old_gen_in`, whose claim is one CAS, so each
/// object is pushed (hence scanned) once whatever the interleaving.
///
/// Returns `0`, or -- for a reference array longer than
/// `par_mark::PAR_ARRAY_CHUNK` (`par_mark_array_split_len`) -- its slot count:
/// its slots are then NOT scanned here but by [`par_mark_scan_chunk`] over
/// slot ranges the drain hands to any worker. Everything else of the object
/// (overlays, pins) is done here, once.
pub fn par_mark_scan_one(
    obj_ptr: *mut u8,
    c: &ParMarkCtx<'_>,
    stack: &mut Vec<*mut u8>,
    out: &mut ParMarkOut,
) -> usize {
    let split = par_mark_array_split_len(obj_ptr, c.old_gen);
    let old_gen = c.old_gen;
    let walked = c.walked;
    let admission = MarkAdmission {
        walked,
        starts: c.starts,
    };
    let rounds_in_bfs = c.rounds_in_bfs;
    let (bfs_lo, bfs_hi) = (c.bfs_lo, c.bfs_hi);
    let mut bfs_scanned = true;
    out.bfs_young.clear();
    if split == 0 && c.prefetch {
        par_mark_prefetch_targets(obj_ptr, old_gen);
    }
    if split > 0 {
        // Slots by `par_mark_scan_chunk`. `round` rescans a `pending_old`
        // object whole, as the serial arm queues it.
        if !rounds_in_bfs && c.track_young {
            out.pending_old.push(obj_ptr as usize);
        }
    } else if rounds_in_bfs {
        let mut young = std::mem::take(&mut out.bfs_young);
        bfs_scanned = GenerationalHeap::scan_object_for_old_refs_split_in(
            obj_ptr,
            old_gen,
            &admission,
            stack,
            Some((bfs_lo, bfs_hi, &mut young)),
        );
        out.bfs_young = young;
    } else {
        if c.track_young {
            out.pending_old.push(obj_ptr as usize);
        }
        GenerationalHeap::scan_object_for_old_refs_in(obj_ptr, old_gen, &admission, stack);
    }
    // SAFETY: `obj_ptr` is a marked old-gen object with a valid header (it
    // passed the push site's screen).
    let owner_class = Some(
        unsafe { &*(obj_ptr as *const ObjectHeader) }
            .class_id
            .as_u32(),
    );
    cratonvm_gc_core::external_roots::with_external_roots_for_owner(
        obj_ptr as usize,
        owner_class,
        |overlay_refs| {
            for overlay_ref in overlay_refs {
                if rounds_in_bfs {
                    let a = overlay_ref.as_ptr() as usize;
                    if a >= bfs_lo && a < bfs_hi {
                        out.bfs_young.push(a);
                    }
                }
                mark_and_push_old_gen(
                    overlay_ref.as_ptr(),
                    old_gen,
                    walked,
                    stack,
                    "external-overlay(BFS owner)",
                );
            }
        },
    );
    if rounds_in_bfs && !c.bfs_young_loaders.is_empty() {
        if let Some(&loader) = owner_class.and_then(|cid| c.bfs_young_loaders.get(&cid)) {
            out.bfs_young.push(loader);
        }
    }
    if c.loader_pin_on {
        // SAFETY: as above.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let loader_addr = match (c.rk_loader_rows, c.old_mark_loader_rows) {
            (Some(rows), _) => rows
                .get(&header.class_id.as_u32())
                .copied()
                .filter(|&a| old_gen.contains(a as *const u8)),
            (None, Some(rows)) => {
                out.loader_lookups += 1;
                rows.get(&header.class_id.as_u32()).copied()
            }
            (None, None) => cratonvm_types::loader_pin::loader_pin_addr_where(
                header.class_id.as_u32(),
                |a| old_gen.contains(a as *const u8),
            ),
        };
        if let Some(loader_addr) = loader_addr {
            mark_and_push_old_gen(loader_addr as *mut u8, old_gen, walked, stack, "loader_pin");
        }
    }
    if c.mirror_pin_owners.contains(&(obj_ptr as usize)) {
        if let Some(mirror_addrs) = pinned_mirrors_of(obj_ptr as usize, c.rk_mirror_rows) {
            for mirror_addr in mirror_addrs {
                if rounds_in_bfs && mirror_addr >= bfs_lo && mirror_addr < bfs_hi {
                    out.bfs_young.push(mirror_addr);
                }
                mark_and_push_old_gen(mirror_addr as *mut u8, old_gen, walked, stack, "mirror_pin");
            }
        }
    }
    if let Some(metadata_addrs) = c.metadata_pins.and_then(|m| m.get(&(obj_ptr as usize))) {
        for &metadata_addr in metadata_addrs {
            if rounds_in_bfs && metadata_addr >= bfs_lo && metadata_addr < bfs_hi {
                out.bfs_young.push(metadata_addr);
            }
            mark_and_push_old_gen(
                metadata_addr as *mut u8,
                old_gen,
                walked,
                stack,
                "metadata_pin",
            );
        }
    }
    if rounds_in_bfs && c.track_young {
        if bfs_scanned {
            // `seed_from_bfs` does nothing for an object with no young target
            // that is not a promotion destination: skip the record.
            let is_dest = c
                .promoted_by_dest
                .is_some_and(|m| m.contains_key(&(obj_ptr as usize)));
            if !out.bfs_young.is_empty() || is_dest {
                let ParMarkOut {
                    young_recs,
                    bfs_young,
                    ..
                } = out;
                young_recs.push(obj_ptr as usize);
                young_recs.push(bfs_young.len());
                young_recs.extend_from_slice(bfs_young);
            }
        } else {
            out.pending_old.push(obj_ptr as usize);
        }
    }
    split
}

/// t1 w7 G2 -- before the marking scan of `obj_ptr`, prefetch every slot
/// target's header, so the push sites' claims (one CAS each, each target a
/// likely miss) overlap instead of serialising. Only an object whose header
/// passes the slot scan's own screens (valid kind / element tags, an extent
/// inside the readable old gen) is read; prefetching any slot value is
/// harmless (a hint never faults). Changes no mark.
#[inline]
pub fn par_mark_prefetch_targets(obj_ptr: *mut u8, old_gen: &OldGen) {
    // SAFETY: every push site range-checked `obj_ptr` against old gen.
    let kind_tag = unsafe { cratonvm_types::kind_tag_at(obj_ptr) };
    let elem_tag = unsafe { cratonvm_types::element_type_tag_at(obj_ptr) };
    if object_kind_from_tag(kind_tag).is_none()
        || !cratonvm_types::element_tag_ok(kind_tag, elem_tag)
    {
        return;
    }
    // SAFETY: tags validated above.
    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
    let total = gen_object_total_size(header);
    if total < HEADER_SIZE || !old_gen.contains_range(obj_ptr, total) {
        return;
    }
    // SAFETY: a valid old-gen object whose whole extent is readable.
    unsafe {
        for_each_ref_slot(obj_ptr, header, |r, _| {
            crate::old_gen::par_mark::prefetch_read(r as usize);
        });
    }
}

/// t1 G2 — the slot count of a reference array the parallel old mark splits
/// into `par_mark::PAR_ARRAY_CHUNK`-slot tasks, or `0` (scan it whole). Only
/// an array whose header passes the slot scan's own screens
/// (`scan_object_for_old_refs_split_in`: valid kind / element tags, an extent
/// inside the readable old gen) is split; anything else takes the whole-object
/// path, which rejects it exactly as the serial BFS does.
pub fn par_mark_array_split_len(obj_ptr: *mut u8, old_gen: &OldGen) -> usize {
    // SAFETY: every push site range-checked `obj_ptr` against old gen, so the
    // tag bytes are mapped (as in `scan_object_for_old_refs_split_in`).
    let kind_tag = unsafe { cratonvm_types::kind_tag_at(obj_ptr) };
    let elem_tag = unsafe { cratonvm_types::element_type_tag_at(obj_ptr) };
    if object_kind_from_tag(kind_tag) != Some(ObjectKind::Array)
        || !cratonvm_types::element_tag_ok(kind_tag, elem_tag)
    {
        return 0;
    }
    // SAFETY: tags validated above.
    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
    if header.element_type() != ArrayElementType::Reference {
        return 0;
    }
    // The same `1 << 24` cap `for_each_ref_slot` applies.
    let len = (header.array_length() as usize).min(1 << 24);
    if len <= crate::old_gen::par_mark::PAR_ARRAY_CHUNK {
        return 0;
    }
    let total = gen_object_total_size(header);
    if total < HEADER_SIZE || !old_gen.contains_range(obj_ptr, total) {
        return 0;
    }
    len
}

/// t1 G2 — slots `[start, end)` of a split reference array (see
/// [`par_mark_array_split_len`]): the array arm of `for_each_ref_slot` under
/// the BFS's scan, range-limited. Young values are recorded for the driver
/// (`[obj, n, ...]`, as a whole-object scan records them; seeding twice for
/// one object is idempotent).
pub fn par_mark_scan_chunk(
    obj_ptr: *mut u8,
    start: usize,
    end: usize,
    c: &ParMarkCtx<'_>,
    stack: &mut Vec<*mut u8>,
    out: &mut ParMarkOut,
) {
    let old_gen = c.old_gen;
    let admission = MarkAdmission {
        walked: c.walked,
        starts: c.starts,
    };
    let (lo, hi) = (c.bfs_lo, c.bfs_hi);
    if obj_ptr.is_null() {
        // A range of the deferred candidates: the push site the serial
        // trace would have called for each (`TrueRootYoung::follow`).
        let cands = &c.candidates[start..end];
        let ahead = crate::old_gen::par_mark::CHUNK_PREFETCH_AHEAD;
        for &a in cands.iter().take(ahead) {
            crate::old_gen::par_mark::prefetch_read(a);
        }
        for (k, &a) in cands.iter().enumerate() {
            if let Some(&n) = cands.get(k + ahead) {
                crate::old_gen::par_mark::prefetch_read(n);
            }
            mark_and_push_old_gen_in(a as *mut u8, old_gen, c.walked, stack, "true-root young edge");
        }
        return;
    }
    out.bfs_young.clear();
    // t1 w7 G2: keep `CHUNK_PREFETCH_AHEAD` target headers in flight.
    let ahead = crate::old_gen::par_mark::CHUNK_PREFETCH_AHEAD;
    let slot_at = |i: usize| -> u64 {
        // SAFETY: `i < end <= len`, and `par_mark_array_split_len` proved the
        // whole extent readable.
        unsafe { read_ref_slot(obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size())) }
    };
    for i in start..(start + ahead).min(end) {
        crate::old_gen::par_mark::prefetch_read(slot_at(i) as usize);
    }
    for i in start..end {
        if i + ahead < end {
            crate::old_gen::par_mark::prefetch_read(slot_at(i + ahead) as usize);
        }
        let raw: u64 = slot_at(i);
        if raw == 0 {
            continue;
        }
        let a = raw as usize;
        if c.rounds_in_bfs && a >= lo && a < hi {
            out.bfs_young.push(a);
        } else {
            mark_and_push_old_gen_in(a as *mut u8, old_gen, &admission, stack, "old-gen ref slot");
        }
    }
    if c.rounds_in_bfs && c.track_young && !out.bfs_young.is_empty() {
        let ParMarkOut {
            young_recs,
            bfs_young,
            ..
        } = out;
        young_recs.push(obj_ptr as usize);
        young_recs.push(bfs_young.len());
        young_recs.extend_from_slice(bfs_young);
    }
}

/// The three 8-byte words at `at` for a rejected-candidate dump, reading only
/// words that lie wholly inside `[lo, hi)`; any other word is reported as 0.
///
/// gen r4/mark (2026-09-23): split out of [`note_rejected_old_mark_candidate`]
/// so the bound is unit-testable against a plain buffer.
pub fn header_words_within(at: usize, lo: usize, hi: usize) -> [u64; 3] {
    let mut w = [0u64; 3];
    for (i, slot) in w.iter_mut().enumerate() {
        let Some(a) = at.checked_add(i * 8) else {
            break;
        };
        if a >= lo && a.checked_add(8).is_some_and(|end| end <= hi) {
            // SAFETY: `[a, a + 8)` lies wholly inside `[lo, hi)`, which every
            // caller passes as a mapped, readable range (old gen's backing
            // store); `read_unaligned` has no alignment requirement.
            *slot = unsafe { std::ptr::read_unaligned(a as *const u64) };
        }
    }
    w
}

/// Rejects a header whose ALWAYS-ZERO field is not zero.
///
/// That field is now the mark word's two reserved bits
/// ([`cratonvm_types::MARK_RESERVED_MASK`], word bits 54..55), not the
/// `gc_flags` nibble. `GC_FLAG_HEADER` claimed the nibble's last undefined bit
/// on 2026-09-08, so `gc_flags & !defined == 0` became a tautology — a screen
/// that cannot fail. Reading it as still-load-bearing would have left this
/// predicate quietly weaker than its own doc claims, which is the failure this
/// file has been bitten by before.
///
/// The reserved bits are a strictly better field for the job: two bits instead
/// of one, so 3 of 4 random words are rejected on it rather than 1 of 2, and
/// their zero-ness is independently enforced — several JIT guards separate a
/// plain object from an array with `CMP BYTE [recv + KIND_TAGS_BYTE_OFFSET], 0`,
/// which is the same byte. The `gc_flags` clause is kept alongside so a FIFTH
/// flag added without revisiting this site is still rejected rather than
/// silently admitted.
///
/// **Do not make this test `gc_flags & GC_FLAG_HEADER != 0`.** It would be a far
/// stronger screen and it is exactly the change that must not be made on
/// reasoning alone: this predicate gates conservative-root candidates, where a
/// false negative does not over-retain — it drops a root and frees a live
/// object. `SWEEP_NO_HEADER_FLAG` exists to measure whether every allocation
/// path really does set the flag; it would have to be read clean across real
/// workloads first, and even then `mark_and_push_old_gen`'s walked-base oracle
/// is what makes a rejection survivable at a precise site.
#[inline]
pub fn header_reserved_fields_plausible(header: &ObjectHeader) -> bool {
    header.reserved_mark_bits_clear()
        && header.gc_flags()
            & !(GC_FLAG_OLD_GEN | GC_FLAG_MARKED | GC_FLAG_COMPACT | GC_FLAG_HEADER)
            == 0
}

/// xt-hardening follow-up (2026-07-03): targeted defense against the
/// `candidate = victim − 8` corruptor shape for a zero-word0 old-gen root
/// candidate — see the call site's comment for the full rationale. Returns
/// `true` if `candidate + 8` looks like the start of a genuine, independently
/// plausible object (in which case `candidate`'s all-zero 8 bytes are almost
/// certainly that object's own leading padding/hash bytes, not a real header
/// of its own).
#[inline]
pub fn victim8_neighbor_explains_zero_prefix(candidate: *mut u8, old_gen: &OldGen) -> bool {
    // SAFETY: caller has already verified `candidate` is 8-aligned and
    // inside old gen; `candidate + 8` stays 8-aligned. Bounds-check before
    // dereferencing.
    let neighbor = unsafe { candidate.add(8) };
    // The WHOLE neighbour header must be contained, not its first byte: on a
    // wholly committed old generation no `READ_SLACK` exists past the
    // reservation's end, so a neighbour in the last `HEADER_SIZE - 1` bytes
    // passes a one-byte test and its header read faults (gen r4w5/review5
    // H1, the same shape `4d430103e` fixed in the caller).
    if !old_gen.contains(neighbor) || !old_gen.contains(neighbor.wrapping_add(HEADER_SIZE - 1)) {
        return false;
    }
    // HIB-DCAST-LATEPHASE.1: `neighbor` is a deliberately UNPROVEN,
    // speculative address — this function's whole job is to guess whether it
    // looks like a real header — so its kind/element_type tag bytes are
    // exactly as likely to be garbage as any other candidate this file
    // screens. The removed code below constructed `nheader` and then read
    // `nheader.kind`/`nheader.kind as u8` unconditionally: loading an
    // out-of-range discriminant into a typed `#[repr(u8)]` enum is instant
    // UB the moment that load happens, regardless of what the code does with
    // the value afterward — `as u8` does not avoid it, because the enum is
    // still read *as an enum* first. Validate both raw tag bytes through
    // `object_kind_from_tag`/`array_element_type_from_tag` before ever
    // constructing a typed `&ObjectHeader`, mirroring every other fix in
    // `defaultcatalogandschema-late-phase-instability-20260801-FIXED.md`.
    // SAFETY: bounds-checked by `old_gen.contains(neighbor)` above.
    let kind_tag = unsafe { cratonvm_types::kind_tag_at(neighbor) };
    // SAFETY: same byte range as the read above — bounds-checked by
    // `old_gen.contains(neighbor)`; a raw `u8` read has no validity invariant.
    let elem_tag = unsafe { cratonvm_types::element_type_tag_at(neighbor) };
    let Some(kind) = object_kind_from_tag(kind_tag) else {
        return false;
    };
    if !cratonvm_types::element_tag_ok(kind_tag, elem_tag) {
        return false;
    }
    // SAFETY: bounds-checked above.
    let nword0 = unsafe { *(neighbor as *const u64) };
    if nword0 == 0 {
        return false;
    }
    // SAFETY: the kind/element_type tag bytes were just validated above.
    let nheader = unsafe { &*(neighbor as *const ObjectHeader) };
    // gen r5w5 (pin9 cross-lane): as in `old_gen_mark_candidate_plausible`.
    let in_old = |t: usize| {
        old_gen.contains(t as *const u8)
            && old_gen.contains(t.wrapping_add(HEADER_SIZE - 1) as *const u8)
    };
    if !forward_chain_stays_inside(nheader, in_old) {
        return false;
    }
    let is_array = kind == ObjectKind::Array;
    let plausible = (is_array || nheader.num_slots() <= (1 << 24))
        && (!is_array || nheader.array_length() <= i32::MAX as u32)
        && header_reserved_fields_plausible(nheader);
    if !plausible {
        return false;
    }
    let total = gen_object_total_size(nheader);
    // gen r4/mark: `wrapping_add` for the reason given in
    // `old_gen_mark_candidate_plausible` — the extent is an unproven claim, and
    // `add` past the allocation is UB before `contains` can reject it.
    // gen r5w3/oldgen7: the whole extent, as in the caller.
    total >= HEADER_SIZE && old_gen.contains_range(neighbor, total)
}

pub use cratonvm_gc_core::heap::{warn_corrupt_array_header, warn_implausible_num_slots, warn_non_object_kind_in_object_arm};



/// Constant-for-the-collection context for the precise young marker.
///
/// Shared by `&` across the parallel mark workers, so every field is `Sync`.
/// The two registry snapshots are taken once at safepoint instead of being
/// re-locked per marked object (see the `YoungMarkCtx` construction in
/// `sweep_young_non_moving`).
pub struct YoungMarkCtx {
    pub from_base: usize,
    pub from_end: usize,
    pub loader_pin_on: bool,
    /// Overlay-backed collection owners; `None` when none is registered.
    ///
    /// gen r4/mark (2026-09-23): an `FxHashSet`. It is probed once per SCANNED
    /// young object whenever any overlay is registered, and it used to be the
    /// std `HashSet` `external_owner_addrs` returns — a SipHash per marked
    /// object for a key that is a plain address.
    pub overlay_owners: Option<FxHashSet<usize>>,
    /// Loader-owned metadata roots; `None` when the registry is empty.
    pub metadata_pins: Option<FxHashMap<usize, Vec<usize>>>,
    /// [`cratonvm_gc_core::heap::young_mark_watch`], snapshotted once per collection so
    /// the per-edge test is a field compare and not an atomic load. `0`
    /// disables it, which is the default and the only value in a normal run.
    pub mark_why: usize,
    /// `cratonvm_types::loader_pin::snapshot_where(<in this from-space>)`,
    /// taken once per collection (`None` when loader pinning is off or the
    /// registry is empty). gc-common w36-d: `_where`, so a class id another
    /// VM shares names THIS heap's young loader.
    ///
    /// gen r4/mark (2026-09-23): `scan_young_object` used to call
    /// `loader_pin::loader_pin_addr` for EVERY scanned object — a read
    /// acquisition of a process-global `RwLock` per object as soon as any user
    /// loader had defined a class, i.e. on every application-server workload,
    /// and a shared cache line bounced between every parallel mark worker. The
    /// old-gen BFS and G1's marker already hoisted the same lookup; the mark
    /// runs at a safepoint, so a snapshot is exactly equivalent (the registry
    /// is mutated only by mutators and by post-GC reconciliation).
    pub loader_pins: Option<FxHashMap<u32, usize>>,
    /// `cratonvm_types::mirror_pin::snapshot()`, taken once per collection.
    /// Same argument as `loader_pins`, with a `Vec` clone per hit on top of the
    /// lock for the per-object `mirrors_for_loader` it replaces.
    pub mirror_pins: Option<FxHashMap<usize, Vec<usize>>>,
}

/// Report one young-mark edge that reached the watched address.
///
/// `#[cold]` and out of line so the armed test in `mark_edge_precise` costs a
/// predictable not-taken branch on the hot path. Prints every arrival, not just
/// the first: the question is which edges reach the object, and the second one
/// is as interesting as the first when the first turns out to be legitimate.
#[cold]
#[inline(never)]
pub fn report_young_mark_why(addr: usize, reason: &'static str) {
    eprintln!("[MARKWHY] young marker reached {addr:#x} via {reason}");
}

/// Precise-edge marker: mark + enqueue a value read out of an actual
/// reference slot (a BFS `for_each_ref_slot` referent, a dirty-card slot
/// read, an overlay/loader/mirror side-channel address).
///
/// These are object BASES by construction -- a store wrote a real reference
/// there -- so the conservative interior-pointer oracle used by `mark_young`
/// is a semantic no-op for them and is skipped. Keeps the same header
/// plausibility + extent rejection and the same never-write-through side-mark
/// channel: nothing here writes to the heap.
///
/// `reason` names the EDGE that led here — `"field"` for an ordinary reference
/// slot, and one of the side-table labels otherwise. It is a `&'static str`, so
/// it costs nothing to pass; it is read only when `ctx.mark_why` is armed. The
/// old-gen BFS has labelled its edges since it was written and the young marker
/// had no equivalent, which is the whole reason a mirror kept being reported as
/// "marked, by nothing visible".
#[inline]
pub fn mark_edge_precise(
    addr: usize,
    ctx: &YoungMarkCtx,
    bits: &cratonvm_gc_core::young_mark::YoungMarkBits,
    worklist: &mut Vec<usize>,
    reason: &'static str,
) {
    if ctx.mark_why != 0 && addr == ctx.mark_why {
        report_young_mark_why(addr, reason);
    }
    if addr < ctx.from_base || addr >= ctx.from_end || addr & 0x7 != 0 {
        return;
    }
    // gen r4/mark (2026-09-23): TEST before test-and-set. Most edges in a
    // closure point at an object some other edge already claimed, and for those
    // the header decode and extent computation below are pure repetition —
    // their answer was already accepted when the bit was set (every setter of a
    // bit in `bits` runs this screen or `mark_young`'s stricter one first). The
    // relaxed load also spares the `lock or` of `try_mark` on an already-set
    // bit, which is a cache-line invalidation shared by every parallel worker
    // that reaches a popular object. The only other effect of the early return
    // is on `SWEEP_BAD_EXTENT_HITS`, which a marked address can never
    // increment. A stale `false` just falls through to the full path.
    if bits.contains(addr) {
        return;
    }
    // SAFETY: the test above confirmed an 8-byte-aligned address inside the
    // live from-space region, so reading an `ObjectHeader` there is valid.
    let header = unsafe { &*(addr as *const ObjectHeader) };
    // One snapshot of the mark word for both the kind screen and the array
    // test (`ObjectHeader::kind_of` maps tag 1, and only tag 1, to `Array`).
    let kind_byte = ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed));
    let is_array = kind_byte == ObjectKind::Array as u8;
    if kind_byte > 1
        || (!is_array && header.num_slots() > (1 << 24))
        || (is_array && header.array_length() > i32::MAX as u32)
    {
        return;
    }
    let total = gen_object_total_size(header);
    if total < HEADER_SIZE || addr + total > ctx.from_end {
        let n = SWEEP_BAD_EXTENT_HITS.fetch_add(1, Ordering::Relaxed);
        if emit_conservative_candidate_diagnostic(n, cratonvm_gc_core::a2dbg::enabled()) {
            tracing::warn!(
                "mark_young_precise: ignoring edge referent at {:#x} with implausible extent {} (kind={}, array_len={}, num_slots={}); safe reject",
                addr,
                total,
                kind_byte,
                header.array_length(),
                header.num_slots(),
            );
        }
        return;
    }
    if bits.try_mark(addr) {
        worklist.push(addr);
    }
}

/// Scan one already-marked young object: every reference slot, plus the
/// conditional out-of-heap edges (collection overlay, defining ClassLoader,
/// class mirrors, loader-owned metadata) that only become roots once their
/// owner is itself known live.
///
/// Read-only on the heap. Callable from any marker worker.
pub fn scan_young_object(
    obj_addr: usize,
    ctx: &YoungMarkCtx,
    bits: &cratonvm_gc_core::young_mark::YoungMarkBits,
    worklist: &mut Vec<usize>,
    hide_referent: bool,
) {
    let obj_ptr = obj_addr as *mut u8;
    // SAFETY: `obj_addr` was validated by `mark_young`/`mark_edge_precise`
    // before it was enqueued -- it is a sane young-gen object header.
    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
    // Mark every referent (arrays + compact objects: 8-byte pointers;
    // legacy objects: 16-byte Value cells).
    // SAFETY: `obj_ptr`/`header` are a validated young object.
    let hidden_slot = hide_referent
        .then(|| y2o_live::referent_slot_key(header))
        .flatten();
    unsafe {
        for_each_ref_slot(obj_ptr, header, |ref_ptr, slot| {
            if hidden_slot != Some(slot) {
                mark_edge_precise(ref_ptr as usize, ctx, bits, worklist, "field");
            }
        });
    }
    // Collection-overlay liveness pin: an overlay is an out-of-heap edge owned
    // by its backing collection, not a process-global root. This non-moving
    // marker has stable object addresses, so once this collection itself is
    // marked we can follow only ITS side-table references. The root gatherer
    // omits the unconditional overlay scan for this exact collector mode.
    // The owner-set probe is the STW snapshot of the same index the lookup
    // would consult, so gating on it is exactly equivalent to calling
    // unconditionally and getting an empty `Vec` back.
    if ctx
        .overlay_owners
        .as_ref()
        .is_some_and(|owners| owners.contains(&obj_addr))
    {
        // Same owner-identity check as the old-gen BFS: reject an overlay
        // entry left behind by a previous tenant of this address.
        //
        // gen r4/mark: the allocation-free `with_` form, as the old-gen BFS
        // uses. Its scratch buffer is thread-local, so each parallel mark
        // worker has its own; `mark_edge_precise` never re-enters it.
        cratonvm_gc_core::external_roots::with_external_roots_for_owner(
            obj_addr,
            Some(header.class_id.as_u32()),
            |overlay_refs| {
                for overlay_ref in overlay_refs {
                    mark_edge_precise(
                        overlay_ref.as_ptr() as usize,
                        ctx,
                        bits,
                        worklist,
                        "overlay-owner",
                    );
                }
            },
        );
    }
    // HIB-CV-24: also mark this object's defining ClassLoader so a live
    // (e.g. leaked-via-ThreadLocal) instance keeps its loader alive.
    // `ctx.loader_pins` is `Some` only when pinning is on — see its doc.
    if ctx.loader_pin_on {
        if let Some(&loader_addr) = ctx
            .loader_pins
            .as_ref()
            .and_then(|pins| pins.get(&header.class_id.as_u32()))
        {
            mark_edge_precise(loader_addr, ctx, bits, worklist, "loader_pin");
        }
    }
    // Class-mirror liveness pin (companion to loader_pin): if this object IS
    // itself a user-defined `ClassLoader` that has defined mirror-having
    // classes, mark those mirrors alive too -- the edge a real JDK's
    // `ClassLoader.classes` field gives for free.
    if let Some(mirror_addrs) = ctx
        .mirror_pins
        .as_ref()
        .and_then(|pins| pins.get(&obj_addr))
    {
        for &mirror_addr in mirror_addrs {
            mark_edge_precise(mirror_addr, ctx, bits, worklist, "mirror_pin");
        }
    }
    // Loader-owned metadata roots (static reference fields, class monitor,
    // condy and reflective descriptor caches) are conditional edges: follow
    // them only after the loader itself is live.
    if let Some(metadata_addrs) = ctx
        .metadata_pins
        .as_ref()
        .and_then(|pins| pins.get(&obj_addr))
    {
        for &metadata_addr in metadata_addrs {
            mark_edge_precise(metadata_addr, ctx, bits, worklist, "metadata_pin");
        }
    }
}

/// The `mirror_pin` / `metadata_pin` values whose OWNER this cycle moved, as
/// old-mark roots: the values of every row whose key (the owner's
/// PRE-collection address -- the VM remaps the registries only after the
/// collector returns) `moved` says is a source of this cycle's pointer map.
/// Only non-null, 8-aligned values. An identity move (a pinned in-place
/// survivor) counts as moved, which is harmless (the legacy young seed already
/// follows it).
///
/// Iterates the ROWS (one per user loader: a handful) and asks the map, not
/// the other way round: the pointer map holds every survivor of the cycle
/// (over a million on a tenure cycle), and this runs inside the pause.
///
/// gce e1/c, closing
/// `gcd-d9a-moving-major-misses-side-table-rows-of-moved-owners-20260928`: the
/// moving cycle's Phase 5 major had no seed for these rows.
pub fn side_table_values_of_moved_owners(
    moved: impl Fn(usize) -> bool,
    mirror_rows: Option<&FxHashMap<usize, Vec<usize>>>,
    metadata_rows: Option<&FxHashMap<usize, Vec<usize>>>,
) -> Vec<usize> {
    let mut out = Vec::new();
    let rows = mirror_rows
        .into_iter()
        .flat_map(|m| m.iter())
        .chain(metadata_rows.into_iter().flat_map(|m| m.iter()));
    for (&owner, values) in rows {
        if moved(owner) {
            out.extend(values.iter().copied().filter(|&v| v != 0 && v % 8 == 0));
        }
    }
    out
}

/// gci i5/og (`gch-h3mj-old-gen-majors-and-concurrent-cycle-review-findings-20261008`
/// item 6, the `metadata_pin` half; P-C only) -- `rows` (the registry's
/// snapshot, keyed by each owner's PRE-collection address) re-keyed for the
/// moving true-root major: an owner this cycle moved is keyed by its
/// post-copy address (`pointer_map[owner]`: a young copy or an old
/// destination), every other owner keeps its key (an old owner that did not
/// move; a young one that did not survive, whose pre-copy key lies in the
/// evacuated arena and so is never an owner the major reaches). Each value is
/// mapped the same way (a young value the cycle copied is followed at its
/// copy; an unmapped value is unchanged, and the mark screens it as before).
/// Two keys that meet are merged (over-retention only). Returns the map and
/// how many owners moved.
///
/// With it, the major follows a moved owner's rows exactly when its trace
/// reaches the owner (`TrueRootYoung::side_table_targets` for a young copy,
/// the BFS's lookup for a promoted one), where the moving cycle used to root
/// every moved owner's values (`side_table_values_of_moved_owners`): a dead
/// owner the young phase copied only for a dead old holder's card kept its
/// values for one more major.
pub fn moving_metadata_rows_rekeyed(
    rows: &FxHashMap<usize, Vec<usize>>,
    pointer_map: &cratonvm_types::PointerMap,
) -> (FxHashMap<usize, Vec<usize>>, usize) {
    let post = |a: usize| -> Option<usize> { pointer_map.get(&a).copied().filter(|&p| p != 0) };
    let mut out: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    out.reserve(rows.len());
    let mut moved = 0usize;
    for (&owner, values) in rows {
        let key = match post(owner) {
            Some(p) => {
                moved += 1;
                p
            }
            None => owner,
        };
        out.entry(key)
            .or_default()
            .extend(values.iter().map(|&v| post(v).unwrap_or(v)));
    }
    (out, moved)
}

/// gci i8/pa — `CRATONVM_GC_MOVING_MAJOR_LOADER_REKEY` (opt-in, default OFF):
/// every P-C moving major carries the `loader_pin` / `mirror_pin` rows
/// re-keyed by post-copy address (`TrueRootAsk::loader_rows` /
/// `mirror_rows`) and stops seeding every moved pinned loader and its
/// mirrors. A licensed moving `System.gc()`
/// (`CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`) takes the re-key whatever this
/// says: its young mirrors are deferred to their loaders, and the registry's
/// pre-copy values would hide their copies from the true-root trace. Read
/// once per P-C moving major.
pub fn moving_major_loader_rekey_enabled() -> bool {
    false
}

/// gci i8/pa — the `loader_pin` rows (`class id -> loader`, this heap's row
/// per id: `loader_pin::snapshot_where`) with each loader mapped through this
/// pause's `pointer_map` to its post-copy address (a young copy or an old
/// destination); an unmoved loader (old) or a dead one (no entry) keeps its
/// address, and the consumers screen it (`old_gen.contains`, the post-swap
/// young range). A null map entry is never a destination.
pub fn moving_loader_rows_rekeyed(
    rows: &FxHashMap<u32, usize>,
    pointer_map: &cratonvm_types::PointerMap,
) -> FxHashMap<u32, usize> {
    rows.iter()
        .map(|(&cid, &loader)| {
            let post = pointer_map.get(&loader).copied().filter(|&p| p != 0).unwrap_or(loader);
            (cid, post)
        })
        .collect()
}

/// gci i8/pa — the mirrors `owner` pins: from the ask's re-keyed rows when it
/// carries them (`TrueRootAsk::mirror_rows`), else from the registry
/// (`mirror_pin::mirrors_for_loader`, keyed by the owner's current address).
/// The caller gates on its owner set first, as before.
pub fn pinned_mirrors_of(
    owner: usize,
    rekeyed: Option<&FxHashMap<usize, Vec<usize>>>,
) -> Option<Vec<usize>> {
    match rekeyed {
        Some(rows) => rows.get(&owner).filter(|v| !v.is_empty()).cloned(),
        None => cratonvm_types::mirror_pin::mirrors_for_loader(owner),
    }
}

/// The destinations of the pinned loaders (`loader_pin` values, every VM's
/// rows) this cycle moved INTO the old generation: `pointer_map[loader]` where
/// `is_old(dest)`. A loader that stayed young (copied to to-space) is left out:
/// the legacy young seed retains every young survivor. Another VM's rows never
/// match (its addresses are not keys of this heap's map).
///
/// gce e1/c: the moving cycle's Phase 5 major looks an old instance's loader up
/// by the row's PRE-collection address and missed a loader promoted in the
/// same pause.
pub fn promoted_pinned_loaders(
    loaders: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
    is_old: impl Fn(usize) -> bool,
) -> Vec<usize> {
    loaders
        .iter()
        .filter_map(|l| pointer_map.get(l).copied())
        .filter(|&dest| dest != 0 && dest % 8 == 0 && is_old(dest))
        .collect()
}

/// gcf f1/c — every value of `pointer_map` that `is_old` places in the old
/// generation (this pause's promotion destinations), non-null and 8-aligned
/// (`ObjectRef::from_raw`'s contract), ascending and deduplicated. The moving
/// cycle's Phase 5 major seeds them as roots; see the call site and
/// `gcf-f1c-moving-major-frees-this-pauses-promotions-under-a-live-pointer-map-entry-20261001`.
/// The root loop screens each against the walked old-gen grid, so a value that
/// is not an object base marks nothing.
pub fn promotion_destinations_in(
    pointer_map: &cratonvm_types::PointerMap,
    is_old: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let mut out: Vec<usize> = pointer_map
        .values()
        .copied()
        .filter(|&dest| dest != 0 && dest % 8 == 0 && is_old(dest))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// gch h2/og — `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS` (default ON since 2026-10-13 and JIT
/// r20 GC wave 9, host w9c `gm-A.sh d4j`: the HotSpot-parity arm (witness SAME,
/// default STALE 253) at no per-major or total cost over the legacy seed (rule
/// C2); `=0` kills it and restores the legacy all-young seed):
/// a moving young cycle's Phase 5 major seeds its old mark from the TRUE roots
/// (P-C of `gengc-core-system-gc-forces-non-moving`, `gcd-d9a` item 2) instead
/// of from every young survivor, so a dead old holder's young child no longer
/// keeps the holder's old subgraph for one more major (`gcd-d4j`'s moving
/// half). Read once per moving major.
pub fn moving_major_true_roots_enabled() -> bool {
    // Default ON since the gci orchestrator, 2026-10-13 (vi2-og A/C/D + vi5-og A/D/T-W PASS); `=0` kills it.
    true
}



/// t1 lane F (gch-h3mj step 3) — the `compiled_frames_live` term
/// [`moving_major_true_root_gate`] sees: the pause's own answer, unless the
/// opt-in switch admits it. Why the switch is sound (the page's two classes):
/// class 1 (a derived cursor whose base is dead in the oop map) does not
/// exist at a GC point on either tier (gcj j2/jt2 JT-1: no pointer IR type,
/// re-derived array bases at every poll), and a frozen peer's raw words are
/// already in `roots`; class 2 (captured words naming old objects) is rooted
/// by the caller (`jit_old_words`). `jit_switch` is asked only when frames
/// are live.
pub fn moving_major_gate_frames_term(
    compiled_frames_live: bool,
    jit_switch: impl FnOnce() -> bool,
) -> bool {
    compiled_frames_live && !jit_switch()
}

/// t1 wave 8 lane H3MJ — is this pause's class 2b set (the blocked peers'
/// stack-slot captures `jit_old_words` reads) complete? Not when the capture
/// is killed (`remap_on` false: `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1`
/// makes `record_peer_stack_slot` record nothing, and an empty buffer never
/// reads as saturated) and not when the buffer refused a word (`saturated`).
/// `saturated` is asked only with the capture on. Pure over its two answers.
pub fn moving_major_jit_class2b_complete(
    remap_on: impl FnOnce() -> bool,
    saturated: impl FnOnce() -> bool,
) -> bool {
    remap_on() && !saturated()
}

/// t1 lane F (gch-h3mj step 3) — the class-2 words a P-C major under live
/// compiled frames roots: `words` (already screened to the old range by the
/// caller) without zeros, ascending, deduplicated. Pure.
pub fn class2_old_words(mut words: Vec<usize>) -> Vec<usize> {
    words.retain(|&w| w != 0);
    words.sort_unstable();
    words.dedup();
    words
}

/// t1 wave 2 lane W — the captured words a P-C major under live compiled
/// frames roots: every word `is_old` holds, plus the `moved` (post-copy)
/// address of every other word that is an exact pre-copy base this pause
/// relocated. Zeros out, ascending, deduplicated. Pure.
pub fn class2_words_mapped(
    words: Vec<usize>,
    is_old: impl Fn(usize) -> bool,
    moved: impl Fn(usize) -> Option<usize>,
) -> Vec<usize> {
    let out: Vec<usize> = words
        .into_iter()
        .filter_map(|w| {
            if w == 0 {
                None
            } else if is_old(w) {
                Some(w)
            } else {
                moved(w).filter(|&p| p != 0 && p % 8 == 0)
            }
        })
        .collect();
    class2_old_words(out)
}



/// gcj hmg — `CRATONVM_GC_MOVING_MAJOR_IN_PLACE_PER_CYCLE_ARMS` (default ON,
/// `=0` restores the switch-level refusal): the in-place P-C admission refuses
/// a cycle because of the take-over / unproven-frame arms only when one of
/// them TOOK that cycle, not whenever their switches are on.
///
/// t1 wave 2 lane W kept P-C off those arms because their ledgers stand in for
/// compiled frames with no proven oop map, and its named rule
/// (`CRATONVM_GC_MOVING_MAJOR_IN_PLACE_NAMED_SEEDS`) roots only what the root
/// slice names. It tested the switches, which were opt-in then. gcj j1yg-close
/// flipped both ON (2026-10-27), and from then on every in-place cycle kept the
/// legacy all-young seed, including the cycles neither arm took (term 4's own
/// pinned copy, the cycles lane W measured the rule on): a dead young object
/// kept a dead old chain through every such major. `GcfF3qPromoteKeepProbe
/// hashmap-grow` (`-Xmx256m`): `oldsz_moving_legacy_seed_majors=15`,
/// `ptko_taken=0 pyun_taken=0`, and majors that reclaimed nothing
/// (`173M->173M`), a dead previous `HashMap` named only by an unrooted young
/// object (`oldmark-root-census`: `young/from-space-unrooted` 90 MB). A cycle
/// an arm did take is refused as before.
pub fn moving_major_in_place_per_cycle_arms_enabled() -> bool {
    true
}

/// gcj hmg — does the in-place P-C admission refuse this cycle for the
/// ledger arms? `took` is whether the take-over or unproven-frame arm took it;
/// `takeover_on` / `unproven_on` their switches. See
/// [`moving_major_in_place_per_cycle_arms_enabled`]. Pure, for the unit tests.
pub fn pc_in_place_refused_for_ledger_arms(
    took: bool,
    takeover_on: bool,
    unproven_on: bool,
    per_cycle: fn() -> bool,
) -> bool {
    if !(takeover_on || unproven_on) {
        return false;
    }
    !per_cycle() || took
}



/// t1 wave 3 lane W — the pin words (`pin_words`) that also occur in the
/// pause's root slice (`root_words`), ascending and deduplicated. Pure.
pub fn pc_in_place_root_pin_words(
    pin_words: &[usize],
    root_words: impl Iterator<Item = usize>,
) -> Vec<usize> {
    let mut rw: Vec<usize> = root_words.collect();
    rw.sort_unstable();
    rw.dedup();
    let mut out: Vec<usize> = pin_words
        .iter()
        .copied()
        .filter(|w| rw.binary_search(w).is_ok())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// t1 wave 2 lane W — the `in_place_cycle` term [`moving_major_true_root_gate`]
/// sees: the pause's own answer, unless `admit` (the switch above, and neither
/// opt-in arm that reads frames with no proven oop map -- the take-over and the
/// unproven-frame arms) lets the in-place cycle in. Why the in-place cycle is
/// sound for P-C, consumer by consumer:
///
/// 1. the young grid: `finish_in_place_young_cycle` rebuilt from-space before
///    Phase 5, so it holds the kept objects only (dead spans on the free list,
///    lazy-dirty and mirrored; slivers GAP-stamped) and `TrueRootYoung::build`
///    parses it with exactly the rules the legacy seed's
///    `mark_young_to_old_refs` uses on every such major today;
/// 2. `pointer_map`: a destination was a FREE byte when the cycle began and a
///    pinned object maps to itself, so keys and values meet only in identity
///    entries; every P-C consumer (`moving_watched_young_copies`,
///    `moving_true_root_extra_seeds`, the `metadata_pin` re-key, the j4/rf
///    closure map-back) reads an identity entry as "the copy is the source",
///    which it is, and `drop_dead_moving_promotions*` judge old values only;
/// 3. the pin ledger: its words name YOUNG objects only, and the young phase
///    rooted the objects they pin through the plan's seeds -- which are no
///    entry of `roots`. The caller therefore roots the plan's seeds (at their
///    unchanged addresses) in the P-C slice, so every pinned object the copy
///    kept as a root is traced, its old referents included. An old object an
///    unrewritable word names is kept no worse than by the legacy seed (which
///    marked old objects only through young fields too); the major never
///    compacts on this cycle (`phase5_major_no_compact`), so no word needs a
///    rewrite.
pub fn moving_major_gate_in_place_term(in_place_cycle: bool, admit: impl FnOnce() -> bool) -> bool {
    in_place_cycle && !admit()
}

/// t1 wave 2 lane W — the P-C roots of an in-place cycle: the young phase's
/// plan `seeds`, narrowed to the exact-rule `names` when the plan seeded by the
/// page rule (`None`: the plan already named its seeds). Ascending,
/// deduplicated. Pure.
pub fn pc_in_place_seeds_of(seeds: &[usize], names: Option<&[usize]>) -> Vec<usize> {
    let mut out: Vec<usize> = match names {
        Some(n) => seeds
            .iter()
            .copied()
            .filter(|s| n.binary_search(s).is_ok())
            .collect(),
        None => seeds.to_vec(),
    };
    out.sort_unstable();
    out.dedup();
    out
}

pub fn moving_major_true_root_gate(
    true_roots: bool,
    in_place_cycle: bool,
    compiled_frames_live: bool,
    switch_on: impl FnOnce() -> bool,
) -> bool {
    true_roots && !in_place_cycle && !compiled_frames_live && switch_on()
}

/// gci i8/pa (`gci-i8rv-oldgen-compact-default-disarms-moving-true-roots-FIXED-20261017`)
/// — does this moving cycle's Phase 5 major compact, and does P-C win it over
/// `CRATONVM_OLDGEN_COMPACT`? Returns `(major_compacts, pc_in_place)`.
///
/// * `no_compact` (the compaction-only veto: compiled frames under the JIT
///   guard, a ledger-pinned copy): never compacts, as before;
/// * a pending fragmentation request (`frag_pending`: the allocation-failure
///   ladder's relief, the proactive trigger) compacts whatever P-C could do --
///   that work is about space, not about which seed marks;
/// * otherwise `OLDGEN_COMPACT` compacts unless P-C can arm (`pc_could_arm`:
///   the true-root seed, P-C's switch, a plain moving cycle, no compiled
///   frame): the true-root seed exists for the in-place arm only, and P-C is
///   the parity fix (gcd-d4j's moving half, PWD) the user's compaction flip
///   would otherwise disarm on every such major.
///
/// `pc_in_place` is `true` exactly when `OLDGEN_COMPACT` alone would have
/// compacted and P-C took the major instead. Requested NON-moving majors are not
/// decided here (`requested_major_compaction_wanted` plans a compaction that
/// keeps the wide true-root seed).
pub fn moving_major_compaction_plan(
    no_compact: bool,
    oldgen_compact: bool,
    frag_pending: bool,
    pc_could_arm: bool,
) -> (bool, bool) {
    let major_compacts = !no_compact && (frag_pending || (oldgen_compact && !pc_could_arm));
    let pc_in_place = !no_compact && !frag_pending && oldgen_compact && pc_could_arm;
    (major_compacts, pc_in_place)
}

/// gch h2/og — the ask a moving true-root major hands `old_gen_gc_inner`. The
/// roots are post-copy (the young phase rewrote them), so no promotion is
/// resolved through its source: `promotions` is empty, and every promotion
/// destination sits in the skipped tail `roots[promo_tail..]` (rooted only on
/// a fallback). `wide` is moot (the moving major plans no pinned compaction).
pub fn moving_true_root_ask(promo_tail: usize) -> TrueRootAsk {
    TrueRootAsk {
        promo_tail,
        promotions: Vec::new(),
        wide: true,
        pre_copy: true,
        // gci i3/og: filled by the moving cycle under
        // `CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`.
        watched_post_copy: Vec::new(),
        // gci i3/og: filled by the moving cycle (its one snapshot).
        watched_snapshot: None,
        // gci i4/og: the moving cycle judges from `pc_tail` instead.
        hand_back_judged: false,
        // gci i5/og: filled by the moving cycle when a `metadata_pin` row
        // exists (`moving_metadata_rows_rekeyed`).
        metadata_rows: None,
        // gci i8/pa: filled by the moving cycle under the loader re-key.
        mirror_rows: None,
        loader_rows: None,
        fallback_loaders: Vec::new(),
    }
}

/// gch h2/og — P-C step 3: the PRE-copy addresses a source-keyed table hands
/// back after a moving pause, mapped through this pause's `pointer_map` to
/// their post-copy addresses (a young copy or an old destination), for the
/// moving true-root major's root slice. The legacy seed kept each of them
/// implicitly (every young survivor, every promotion destination); the
/// true-root seed sees only what the roots reach, and these are not roots:
///
/// * `finalizer_addrs` -- every registered finalizable (live or resurrected by
///   Phase 2.5): a young one left out would have its old referents freed and
///   then be finalized over them;
/// * `watched` -- every address the reference processor holds and every JNI
///   weak global's referent (the gcd d5/r rule): the restore hands a survivor
///   back through its weak reference;
/// * `loaders` -- every pinned `ClassLoader` (`loader_pin`): the true-root
///   trace's instance->loader edge is keyed by the loader's pre-copy address,
///   which the post-swap grid does not hold.
///
/// An address with no entry did not move (an old object) or did not survive;
/// either way it needs nothing here. Non-null, 8-aligned (`ObjectRef::from_raw`'s
/// contract), ascending, deduplicated.
pub fn moving_true_root_extra_seeds(
    pointer_map: &cratonvm_types::PointerMap,
    finalizer_addrs: &[usize],
    watched: Option<&cratonvm_gc_core::gc_quiescence::WatchedSnapshot>,
    loaders: &[usize],
) -> Vec<usize> {
    let mut out: Vec<usize> = finalizer_addrs
        .iter()
        .chain(watched.into_iter().flatten())
        .chain(loaders.iter())
        .filter_map(|a| pointer_map.get(a).copied())
        .filter(|&dest| dest != 0 && dest % 8 == 0)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// gch h2/og — P-C step 5: after a moving true-root major's in-place sweep,
/// remove every `pointer_map` entry whose destination `is_old` but is no
/// longer `is_allocated` (a promotion the seed did not reach, freed by the
/// sweep). Returns `(old destinations judged, entries removed)` for
/// `OldGen::note_true_root_promotions`. Young destinations (to-space copies)
/// are untouched: nothing frees young storage in this pause.
pub fn drop_dead_moving_promotions(
    pointer_map: &mut cratonvm_types::PointerMap,
    is_old: impl Fn(usize) -> bool,
    is_allocated: impl Fn(usize) -> bool,
) -> (usize, usize) {
    let mut judged = 0usize;
    let mut dead = 0usize;
    pointer_map.retain(|_, dest| {
        let d = *dest;
        if !is_old(d) {
            return true;
        }
        judged += 1;
        if is_allocated(d) {
            true
        } else {
            dead += 1;
            false
        }
    });
    (judged, dead)
}

/// gci i2/og (the next P-C cost cut after gch h4/pc's rounds cut; P-C missed
/// its steady cost bound by 1 % on `vi1-og.list` C) — [`drop_dead_moving_promotions`]
/// when `dests` already holds every OLD destination of `pointer_map`,
/// ascending and deduplicated: the `promotion_destinations_in` list the P-C
/// major built for its skipped root tail (`pc_tail`), from the same map,
/// before the collection. Same result, much less work on the common verdict:
///
/// * the sweep's verdict for all of them is ONE merge walk over the sorted
///   free blocks (`unallocated_among`, i.e. `OldGen::unallocated_among_ascending`)
///   instead of a hash-table `retain` over every entry with one
///   `is_allocated_addr` binary search per old value (~450k per steady major
///   of `GenR4W4SteadyPromotionProbe`, every one of them outside the
///   `[gcphase] old-gen` lines, so no attribution row ever saw it);
/// * when no destination died -- every steady and thrash row so far
///   (`oldsz_true_root_promotions_dead=0`) -- the map is not touched at all;
/// * otherwise only the entries naming a dead destination are removed.
///
/// `judged` is `dests.len()`: the old values of the map, each named by one
/// source (a copy has one source). The only values `promotion_destinations_in`
/// leaves out are 0 and misaligned ones, which no copy produces.
pub fn drop_dead_moving_promotions_sorted(
    pointer_map: &mut cratonvm_types::PointerMap,
    dests: &[usize],
    unallocated_among: impl FnOnce(&[usize]) -> Vec<usize>,
) -> (usize, usize) {
    let dead_dests = unallocated_among(dests);
    if dead_dests.is_empty() {
        return (dests.len(), 0);
    }
    let dead: FxHashSet<usize> = dead_dests.into_iter().collect();
    let mut removed = 0usize;
    pointer_map.retain(|_, dest| {
        let d: usize = *dest;
        if dead.contains(&d) {
            removed += 1;
            false
        } else {
            true
        }
    });
    (dests.len(), removed)
}

/// gci i3/og (`CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT`, opt-in;
/// `gci-i2og-old-gen-and-p-c-review-findings-FIXED-20261013` item 3) — the NON-moving
/// true-root major's dead-promotion verdict (`TrueRootSweepOut::dead_sources`)
/// from `by_dest`, this pause's promotions as `(destination, source)` sorted by
/// destination. `unallocated_indices` is
/// `OldGen::unallocated_indices_ascending_by(by_dest, |&(d, _)| d)` in the
/// caller: one merge walk over the sorted free blocks for every destination,
/// where the per-entry arm asks `is_allocated_addr` once per promotion. The
/// SOURCES of the unallocated destinations, ascending: the very list the
/// per-entry arm builds from the source-sorted list (each source names one
/// destination).
pub fn dead_promotion_sources_batched(
    by_dest: &[(usize, usize)],
    unallocated_indices: impl FnOnce(&[(usize, usize)]) -> Vec<usize>,
) -> Vec<usize> {
    let mut out: Vec<usize> = unallocated_indices(by_dest)
        .into_iter()
        .map(|i| by_dest[i].1)
        .collect();
    out.sort_unstable();
    out
}

/// Seed the young mark with every `mirror_pin` edge whose OWNER — the defining
/// `ClassLoader` — satisfies `is_old`. Returns the number of edges followed.
///
/// gen r4w2/youngmark (2026-09-23), closing
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// `mirror_pin` (loader -> the class mirrors it defined) is a side table: no
/// heap slot and no card describes it, so a marker sees it only when it scans
/// the owner, and this marker scans only young objects. The root gatherer
/// (`VmHeap::mirror_pin_deferrable` + `vm::memory::roots` step 6) leaves a
/// YOUNG user-loader mirror out of the root set on every cycle certain to take
/// this marker, relying on exactly this edge. With the loader tenured and the
/// mirror young (a class defined, or its mirror first materialised, after the
/// loader was promoted) nothing reached the mirror and the sweep freed a live
/// `java.lang.Class`.
///
/// Minor-GC semantics: the old generation is treated as live, so every old
/// owner's edges are followed — exactly what the dirty-card seed and the
/// `"overlay-old-owner"` seed already do for the other two kinds of
/// old->young edge. An owner that is dead-but-unswept old garbage retains its
/// mirrors for one more cycle, which is the safe direction. Cost: one pass
/// over the (small) per-cycle registry snapshot, `ctx.mirror_pins`.
pub fn seed_mirror_pins_of_old_owners(
    ctx: &YoungMarkCtx,
    is_old: &dyn Fn(usize) -> bool,
    bits: &cratonvm_gc_core::young_mark::YoungMarkBits,
    worklist: &mut Vec<usize>,
) -> usize {
    let Some(pins) = ctx.mirror_pins.as_ref() else {
        return 0;
    };
    let mut followed = 0usize;
    for (&owner, mirrors) in pins {
        if !is_old(owner) {
            continue;
        }
        for &mirror in mirrors {
            mark_edge_precise(mirror, ctx, bits, worklist, "mirror-old-owner");
            followed += 1;
        }
    }
    followed
}

/// Seed the young mark with every `metadata_pin` edge whose OWNER satisfies
/// `is_old`. Returns the number of edges followed.
///
/// gc-common w36-d, part 1 of
/// `docs/feature-designs/generative-gc-architecture.md`
/// and the exact twin of [`seed_mirror_pins_of_old_owners`]. `metadata_pin`
/// (owner loader -> the loader-conditional values the root gatherer deferred
/// to it) is a side table no card describes, and `scan_young_object` follows
/// it only from an owner it SCANS, i.e. a young one. Since w36-d
/// `VmHeap::metadata_pin_deferrable` defers a YOUNG value on every cycle
/// certain to take this marker (`young_marker_follows_side_tables`), so a
/// young value pinned to an OLD loader would otherwise be marked by nothing
/// and freed while its static / `ClassValue` row still names it. Minor-GC
/// semantics, as for the mirrors: every old owner is treated as live; a dead
/// old owner retains its values until the old generation decides it.
pub fn seed_metadata_pins_of_old_owners(
    ctx: &YoungMarkCtx,
    is_old: &dyn Fn(usize) -> bool,
    bits: &cratonvm_gc_core::young_mark::YoungMarkBits,
    worklist: &mut Vec<usize>,
) -> usize {
    let Some(pins) = ctx.metadata_pins.as_ref() else {
        return 0;
    };
    let mut followed = 0usize;
    for (&owner, values) in pins {
        if !is_old(owner) {
            continue;
        }
        for &value in values {
            mark_edge_precise(value, ctx, bits, worklist, "metadata-old-owner");
            followed += 1;
        }
    }
    followed
}

/// The `loader_pin` rows (`class_id -> loader`) whose loader lies in young
/// from-space and has NOT been marked. Empty — without allocating — whenever
/// loader pinning is off, the registry is empty, or every young pinned loader
/// is already marked, which is every healthy cycle.
///
/// gen r4w2/youngmark (2026-09-23): the candidate set for
/// [`seed_loaders_of_old_instances`].
pub fn unreached_young_pinned_loaders(
    ctx: &YoungMarkCtx,
    bits: &cratonvm_gc_core::young_mark::YoungMarkBits,
) -> FxHashMap<u32, usize> {
    let mut out = FxHashMap::default();
    if !ctx.loader_pin_on {
        return out;
    }
    let Some(pins) = ctx.loader_pins.as_ref() else {
        return out;
    };
    for (&class_id, &loader) in pins {
        if loader >= ctx.from_base && loader < ctx.from_end && !bits.contains(loader) {
            out.insert(class_id, loader);
        }
    }
    out
}

/// Mark every loader in `unreached` (`class_id -> young loader`) that has at
/// least one instance among `old_class_ids` — the class ids of the old
/// generation's objects. Returns the number of loaders marked.
///
/// gen r4w2/youngmark (2026-09-23): the `loader_pin` half of the side-table
/// gap. `scan_young_object` follows instance -> loader from the instances it
/// scans; an old instance is never scanned by a young cycle, so a young loader
/// whose live instances were all tenured was swept while live. The caller
/// passes only loaders the closure did not reach, and this stops reading
/// `old_class_ids` as soon as every candidate is resolved.
pub fn seed_loaders_of_old_instances(
    ctx: &YoungMarkCtx,
    mut unreached: FxHashMap<u32, usize>,
    old_class_ids: impl IntoIterator<Item = u32>,
    bits: &cratonvm_gc_core::young_mark::YoungMarkBits,
    worklist: &mut Vec<usize>,
) -> usize {
    let mut rescued = 0usize;
    for class_id in old_class_ids {
        if unreached.is_empty() {
            break;
        }
        let Some(loader) = unreached.remove(&class_id) else {
            continue;
        };
        mark_edge_precise(loader, ctx, bits, worklist, "loader-pin-old-instance");
        rescued += 1;
        // One instance is enough for the loader: drop its other classes.
        unreached.retain(|_, l| *l != loader);
    }
    rescued
}

/// The MOVING young cycle's twin of [`unreached_young_pinned_loaders`]: the
/// `loader_pin` rows (`class_id -> loader`) whose loader lies in young
/// from-space (`in_young_from`) and has NO forwarding entry yet
/// (`forwarded`) — i.e. the Cheney closure did not reach it. Empty, without
/// allocating, when `pins` is `None` (pinning off or registry empty) or every
/// young pinned loader was copied, which is every healthy cycle.
///
/// gen r4w3/rooting (2026-09-23): closes the moving half of
/// `gengc-r4w2-youngmark-moving-young-loader-pin-from-old-instances`.
pub fn moving_unreached_young_pinned_loaders(
    pins: Option<FxHashMap<u32, usize>>,
    in_young_from: impl Fn(usize) -> bool,
    forwarded: impl Fn(usize) -> bool,
) -> FxHashMap<u32, usize> {
    let mut out = FxHashMap::default();
    let Some(pins) = pins else {
        return out;
    };
    for (class_id, loader) in pins {
        if in_young_from(loader) && !forwarded(loader) {
            out.insert(class_id, loader);
        }
    }
    out
}

/// The loaders in `unreached` (`class_id -> young loader`) that have at least
/// one instance among `old_class_ids`, each reported ONCE, in first-found
/// order. Stops reading `old_class_ids` as soon as every candidate is resolved.
///
/// The moving twin of [`seed_loaders_of_old_instances`], which marks instead of
/// returning: the moving path has to FORWARD each loader through
/// `GenerationalHeap::forward_object`, which needs the collector's `&mut`
/// state, so the decision and the action are split.
pub fn loaders_of_old_instances(
    mut unreached: FxHashMap<u32, usize>,
    old_class_ids: impl IntoIterator<Item = u32>,
) -> Vec<usize> {
    let mut out = Vec::new();
    for class_id in old_class_ids {
        if unreached.is_empty() {
            break;
        }
        let Some(loader) = unreached.remove(&class_id) else {
            continue;
        };
        out.push(loader);
        // One instance is enough for the loader: drop its other classes.
        unreached.retain(|_, l| *l != loader);
    }
    out
}

/// The phantom-extent predicate, shared by the sequential young sweep walk and
/// the parallel `sweep_chunk`.
///
/// `side_sorted` is the cycle's mark set as ASCENDING absolute addresses, and
/// `live_probe` is a monotone cursor into it that the caller carries across the
/// whole walk — both walks visit objects in ascending address order, so the
/// total cost is one index advance per mark, not a search per object.
///
/// Answers "is a mark STRICTLY inside `[abs, abs + total_size)`", returning the
/// first such address. Live objects never nest, so under the premise that every
/// mark is an object BASE a hit proves the walk left the object grid. That
/// premise is only true because the late base-resolution pass clears the raw
/// conservative marks it proves are object-interior — see
/// `LATE_RESOLVE_RAW_INTERIOR_CLEARED`.
#[inline]
pub fn mark_strictly_inside(
    side_sorted: &[usize],
    live_probe: &mut usize,
    abs: usize,
    total_size: usize,
) -> Option<usize> {
    while *live_probe < side_sorted.len() && side_sorted[*live_probe] <= abs {
        *live_probe += 1;
    }
    side_sorted
        .get(*live_probe)
        .filter(|&&a| a < abs + total_size)
        .copied()
}

// ---------------------------------------------------------------------------
// Late conservative-candidate base resolution (H2-CID0, 2026-08-01)
// ---------------------------------------------------------------------------

/// Resolve each address in `cands` to the BASE of the young from-space object
/// that contains it, by chaining the arena's own object grid from offset 0.
///
/// This is the fallback for candidates the anchor oracle in
/// `sweep_young_non_moving` could not place. The oracle is a subsampled walk:
/// it chains each anchor interval independently and throws the interval away
/// unless the chain lands exactly on the upper anchor. An anchor that is not
/// really an object base (or an object whose extent straddles one) therefore
/// discards a whole interval's worth of resolutions, and every candidate in it
/// falls back to being side-marked at its RAW address — which retains nothing
/// when the address is object-INTERIOR, because the sweep matches side marks
/// against object STARTS.
///
/// This walk has no anchors to disagree with: it is the same linear chain the
/// sweep itself performs (free/TLAB-skip aware, GAP-filler aware, and
/// abandoning on the same header-plausibility failures), so where it resolves a
/// candidate it resolves it to exactly the object the sweep is about to
/// classify.
///
/// `cands` must be sorted ascending and hold ABSOLUTE addresses inside
/// `[from_base, from_base + used)`. `free_blocks` is the merged
/// free-list + reserved-TLAB-tail skip list, sorted ascending.
///
/// Returns `(bases, unreachable, interior)` — the covering bases it proved
/// (each at most once, ascending), the candidates the walk never got to
/// because it desynced first, and the candidates it PROVED are object-INTERIOR
/// (strictly inside an object it parsed, so provably not an object start).
/// The caller marks the first, pins the second, and clears the raw side marks
/// of the third.
///
/// The third output exists because a raw mark on an interior address is not
/// merely useless — the sweep matches marks against object STARTS, so it
/// retains nothing — it is actively harmful: `SWEEP_PHANTOM_EXTENTS` reads
/// `side_sorted` as a set of live BASES, and an interior mark inside the very
/// object being sized satisfies "this extent subsumes a live object" exactly,
/// condemning a valid object and unwinding every reclamation since the last
/// anchor. See `SWEEP_PHANTOM_INTERIOR_MARKS`.
pub fn resolve_candidate_bases(
    from_base: usize,
    used: usize,
    free_blocks: &[(usize, usize)],
    cands: &[usize],
) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let mut bases: Vec<usize> = Vec::new();
    let mut interior: Vec<usize> = Vec::new();
    if cands.is_empty() {
        return (bases, Vec::new(), interior);
    }
    let mut free_iter = free_blocks.iter().peekable();
    let mut cursor = 0usize;
    let mut ci = 0usize;
    // gen r5w1/young5: the grid rules are `probe_grid_at`'s. `Parse` keeps
    // this walk's historical zero-run behaviour (an all-zero header sizes as
    // one empty object, so a candidate inside a zero run resolves to the
    // empty object containing it); every anomaly still ends the walk, and the
    // candidates past it become late pins.
    let rules = GridRules {
        base: from_base,
        used,
        zero_run_hi: used,
        side_sorted: &[],
        zero_runs: ZeroRunRule::Parse,
        bad_gap_filler_is_anomaly: true,
        phantom: false,
    };
    let mut no_phantom_probe = 0usize;
    while cursor < used && ci < cands.len() {
        // Same robust free/TLAB-skip resync the sweep walk uses.
        if skip_free_blocks(&mut cursor, &mut free_iter).0 {
            continue;
        }
        if cursor >= used {
            break;
        }
        let next_free = free_iter.peek().map(|&&(foff, _)| foff);
        // `cursor < used` and free/TLAB ranges were skipped, so this is an
        // allocator-written object boundary inside the mapped from-space.
        let total = match probe_grid_at(cursor, next_free, &rules, &mut no_phantom_probe) {
            GridProbe::Gap { len } => {
                cursor += len;
                continue;
            }
            GridProbe::Object { size } => size,
            // `Parse` yields no zero-run verdicts and `phantom` is off; every
            // remaining verdict (malformed GAP filler, implausible size, an
            // extent crossing a free hole) ends the walk as it always did.
            GridProbe::BadGapFiller
            | GridProbe::EmptyRun { .. }
            | GridProbe::ZeroSpan { .. }
            | GridProbe::BadSize { .. }
            | GridProbe::CrossesFreeBlock { .. }
            | GridProbe::Phantom { .. } => break,
        };
        let start = from_base + cursor;
        let end = start + total;
        // Candidates below this object's start fell in free/gap space the walk
        // just strode over — they are provably not object interiors, exactly as
        // the oracle's verified-span drop concludes.
        while ci < cands.len() && cands[ci] < start {
            ci += 1;
        }
        if ci < cands.len() && cands[ci] < end {
            bases.push(start);
            while ci < cands.len() && cands[ci] < end {
                // A candidate that is not this object's own start is
                // object-INTERIOR under the grid the sweep itself walks.
                if cands[ci] != start {
                    interior.push(cands[ci]);
                }
                ci += 1;
            }
        }
        cursor += total;
    }
    (bases, cands[ci..].to_vec(), interior)
}

// ---------------------------------------------------------------------------
// Parallel non-moving sweep walk (perf/parallel-young-gc, 2026-07-25)
// ---------------------------------------------------------------------------

/// One anchored chunk's sweep decisions. Produced READ-ONLY: nothing here has
/// been zeroed, published, or header-written yet, so the whole attempt can be
/// thrown away and re-run sequentially at zero cost.
#[derive(Debug, Default)]
pub struct SweepChunkResult {
    /// `(offset, size, class_id, kind_byte, object_count)` — same shape as the
    /// sequential walk's `dead_regions`.
    pub dead: Vec<(usize, usize, u32, u8, usize)>,
    /// Offsets of header-marked survivors whose `GC_FLAG_MARKED` must be
    /// cleared and whose `gc_age` must be bumped. Deferred out of the walk so
    /// an aborted attempt never leaves a partially-aged arena.
    pub survivors: Vec<usize>,
    /// Live-object count (header-marked + side-marked).
    pub live: usize,
    /// Survivor ADDRESSES that are watched Weak/Soft/Phantom referents.
    pub watched: Vec<usize>,
    /// gen r4/sweep (2026-09-23): SIDE-marked survivors whose header
    /// nevertheless carries `GC_FLAG_MARKED`. The side arm retains without a
    /// header write, so such a bit is left for `clear_all_mark_bits_in_arena`;
    /// the sweep uses this (with `survivors.len()`) to decide whether that
    /// re-walk has anything to clear at all.
    pub side_with_header_mark: usize,
    /// gch h1/yy: the forwarding target of every forwarded object this chunk
    /// put in `dead`. The chunk can test a target only against the old-gen
    /// EXTENT (`SweepCtx::old_lo..old_hi`); the sequential walk asks
    /// `OldGen::contains`, which is narrower (the readable prefix, and the
    /// commit bitmap over holes), and RETAINS a span whose target fails it.
    /// The merge asks `contains` of every target here before it accepts the
    /// attempt, and falls back to the sequential walk if one fails, so a
    /// phantom forward into reserved-but-unreadable old space can no longer be
    /// reclaimed by the parallel prefix where the sequential walk keeps it.
    pub forward_targets: Vec<usize>,
}

/// Everything a sweep worker needs, as plain `Sync` data.
pub struct SweepCtx<'a> {
    pub from_base: usize,
    pub used: usize,
    pub existing_free: &'a [(usize, usize)],
    pub side_bits: &'a cratonvm_gc_core::young_mark::YoungMarkBits,
    /// The same mark set as `side_bits`, materialised as ascending absolute
    /// addresses. Used only by the phantom-extent check, which asks "is any
    /// marked base strictly inside this extent" — a range question the bitmap
    /// would answer one word at a time.
    pub side_sorted: &'a [usize],
    /// Old-gen extent as raw integers — `OldGen` itself is not `Sync`
    /// (it owns a `Vec<u8>` behind a `MutexGuard`). gch h1/yy: this is NOT
    /// `OldGen::contains`, which this used to claim: `contains` also requires
    /// the readable prefix and, over holes, the commit bitmap. The chunks use
    /// the extent as a first screen and report each target they accepted
    /// (`SweepChunkResult::forward_targets`) for the merge to ask `contains`.
    pub old_lo: usize,
    pub old_hi: usize,
    /// Watched-referent snapshot for this collection; `None` when empty.
    pub watched: Option<&'a cratonvm_gc_core::gc_quiescence::WatchedSnapshot>,
    /// gcf f3/q: arm each chunk's [`GridSizeMemo`]
    /// (`CRATONVM_GEN_WALK_SIZE_MEMO`, read once by the sweep on its own
    /// thread, since the pool workers do not see its per-thread overrides).
    pub size_memo: bool,
}

/// Walk `[lo, hi)` of the young from-space and classify every object.
///
/// Returns `None` on ANY grid anomaly (implausible header, unlisted zero span,
/// free-block overshoot, hole-crossing extent, malformed GAP filler, or a
/// chain that fails to land exactly on `hi`). The caller then discards every
/// chunk and re-runs the original sequential walk, which owns all the
/// diagnostic reporting and the unwind/re-anchor recovery policy. That keeps
/// this fast path small enough to audit: it only ever runs on a grid that the
/// oracle walk already verified, and it bails the moment anything disagrees.
pub fn sweep_chunk(ctx: &SweepCtx<'_>, lo: usize, hi: usize) -> Option<SweepChunkResult> {
    let mut out = SweepChunkResult::default();
    let from_base = ctx.from_base;

    // Position a free-block iterator at the first block that can still matter
    // for `[lo, hi)`.
    let start = ctx
        .existing_free
        .partition_point(|&(off, sz)| off + sz <= lo);
    let mut free_iter = ctx.existing_free[start..].iter().peekable();

    // H2-GCVAR: monotone probe into the ascending mark set for the
    // phantom-extent check below. One `partition_point` to enter this chunk's
    // window, then a forward-only advance per object.
    let mut live_probe = ctx.side_sorted.partition_point(|&a| a < from_base + lo);

    // gen r5w1/young5: the grid rules are `probe_grid_at`'s. Every verdict
    // other than an object, a GAP filler or a run of empty objects bails the
    // whole attempt under its historical reason index; the sequential walk
    // owns the report and the unwind/re-anchor policy.
    //
    // The zero-run arm: only SOME all-zero spans are evidence the grid broke.
    // A run of EMPTY objects is the ordinary post-HEADER_SIZE-16 shape (see
    // `zero_run_empty_object_resume`), and a marked object at the run start is
    // simply a live `ClassId(0)` container whose header legitimately reads
    // zero. Bailing on either was expensive out of all proportion — one `None`
    // makes `parallel_sweep_walk` discard EVERY chunk and re-run the whole
    // arena sequentially, and on the hibernate repro that happened on every
    // cycle (`par_attempts=5 par_fails=5`, all five the zero-run branch, so
    // `par_prefix_end` was 0 for the entire run). `unresolved: None`: this
    // walk's `ctx` carries no unresolved-mark set, so it cannot tell a proved
    // base from a conservative interior mark and must keep refusing. The run
    // measurement never steps over `hi` (`zero_run_hi`): the chain must land
    // exactly on the next anchor or the chunk is discarded anyway.
    //
    // The phantom arm: a header whose extent subsumes a live (marked) object
    // proves the walk is off the object grid — live objects never nest.
    let rules = GridRules {
        base: from_base,
        used: ctx.used,
        zero_run_hi: hi,
        side_sorted: ctx.side_sorted,
        zero_runs: ZeroRunRule::StepOver {
            unresolved: None,
            count_empty_runs: true,
        },
        bad_gap_filler_is_anomaly: true,
        phantom: true,
    };

    let mut cursor = lo;
    // gcf f3/q (r15w5/gcpage patch 1): one size memo per chunk.
    let mut size_memo = GridSizeMemo::new(ctx.size_memo);
    while cursor < hi {
        let (resynced, overshot) = skip_free_blocks(&mut cursor, &mut free_iter);
        if overshot {
            // The grid and the free list disagree — sequential recovery only.
            return chunk_bail(0);
        }
        if resynced {
            continue;
        }
        if cursor >= hi {
            break;
        }
        let obj_ptr = (from_base + cursor) as *mut u8;
        let next_free = free_iter.peek().map(|&&(off, _)| off);
        // `cursor < hi <= used`, so this is inside the mapped from-space
        // region, and the anchor chain has kept it on the object grid the
        // oracle walk verified (`probe_grid_at`'s contract).
        let total_size =
            match probe_grid_at_sized(cursor, next_free, &rules, &mut live_probe, &mut size_memo) {
                GridProbe::Object { size } => size,
                // GAP-filler sentinel: skipped, never freed (same as sequential).
                GridProbe::Gap { len } => {
                    cursor += len;
                    continue;
                }
                GridProbe::EmptyRun { resume } => {
                    cursor = resume;
                    continue;
                }
                GridProbe::BadGapFiller => return chunk_bail(1),
                GridProbe::ZeroSpan { .. } => return chunk_bail(2),
                GridProbe::BadSize { .. } => return chunk_bail(3),
                GridProbe::CrossesFreeBlock { .. } => return chunk_bail(4),
                GridProbe::Phantom { .. } => return chunk_bail(5),
            };
        // SAFETY: `probe_grid_at` parsed a plausible header whose extent lies
        // inside `used` at this in-bounds, on-grid offset.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

        let abs = from_base + cursor;

        if header.is_forwarded() {
            // Evacuated to old gen by selective promotion: reclaim the young
            // slot, unless the forwarding target is not actually in old gen
            // (a phantom write) — in which case retain, as the sequential
            // walk does.
            let fwd = header.forwarding_address() as usize;
            if fwd >= ctx.old_lo && fwd < ctx.old_hi {
                out.forward_targets.push(fwd);
                push_dead(&mut out.dead, cursor, total_size, header);
            }
        } else if ctx.side_bits.contains(abs) {
            // Side-marked survivor: pure retention, no header writes.
            out.live += 1;
            if header.gc_flags() & GC_FLAG_MARKED != 0 {
                out.side_with_header_mark += 1;
            }
            if ctx.watched.is_some_and(|w| w.contains(&abs)) {
                out.watched.push(abs);
            }
        } else if header.gc_flags() & GC_FLAG_MARKED != 0 {
            out.live += 1;
            out.survivors.push(cursor);
            if ctx.watched.is_some_and(|w| w.contains(&abs)) {
                out.watched.push(abs);
            }
        } else {
            push_dead(&mut out.dead, cursor, total_size, header);
        }
        cursor += total_size;
    }
    // The chain must land EXACTLY on the next anchor; anything else means the
    // anchor was not a real object start after all.
    if cursor != hi {
        return chunk_bail(6);
    }
    Some(out)
}

/// Append a dead span, merging with the immediately preceding one (the
/// sequential walk's `retain_dead_objects == false` behaviour).
#[inline]
pub fn push_dead(
    dead: &mut Vec<(usize, usize, u32, u8, usize)>,
    cursor: usize,
    total_size: usize,
    header: &ObjectHeader,
) {
    if let Some(last) = dead.last_mut() {
        if last.0 + last.1 == cursor {
            last.1 += total_size;
            last.4 += 1;
            return;
        }
    }
    dead.push((
        cursor,
        total_size,
        header.class_id.as_u32(),
        ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
        1,
    ));
}

// ---------------------------------------------------------------------------
// Parallel selective-promotion walk and card seed (gch h1/yy, opt-in)
// ---------------------------------------------------------------------------





/// Below this many dirty-card pairs the parallel card seed stays on the
/// collecting thread: a pool dispatch costs about what a few thousand
/// random header reads do.
pub const PAR_CARD_SEED_MIN_PAIRS: usize = 4096;

/// One dirty-card seed pair of the non-moving sweep's mark: mark the young
/// value `old_obj`'s slot `slot_idx` holds (an element index for a reference
/// array, a byte offset for a compact instance, a `Value` cell index for a
/// legacy one -- the encoding `scan_dirty_cards` records) and return the
/// address whose card must be re-dirtied for the next collection (the
/// element's card on an element-precise table, the header's otherwise).
///
/// gch h1/yy: the body of the sweep's serial seed loop, moved out unchanged so
/// the parallel seed (`CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED`) runs the same
/// code. Read-only on the heap; the only write is the atomic side-bit claim in
/// [`mark_edge_precise`], so pairs may be seeded on any worker in any order:
/// the claimed set, and so the closure the drain computes from it, does not
/// depend on the order (the confluence `young_mark` documents).
///
/// # Safety (caller)
/// `(old_obj, slot_idx)` is a pair the dirty-card scan produced this pause:
/// a live old-gen object and an in-bounds reference slot of it. STW.
#[inline]
pub fn seed_one_card_root(
    old_obj: ObjectRef,
    slot_idx: usize,
    precise_array_cards: bool,
    ctx: &YoungMarkCtx,
    bits: &cratonvm_gc_core::young_mark::YoungMarkBits,
    worklist: &mut Vec<usize>,
) -> usize {
    // SAFETY: `old_obj` is a live old-gen object from dirty-card
    // scanning; its header is valid.
    let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
    let is_ref_array =
        header.kind() == ObjectKind::Array && header.element_type() == ArrayElementType::Reference;
    // Card covering this old→young slot must remain dirty for the
    // next GC (the edge survives an in-place sweep): the element's
    // own card on an element-precise table, the header's otherwise.
    let card = if is_ref_array && precise_array_cards {
        (old_obj.as_ptr() as usize).wrapping_add(ARRAY_DATA_OFFSET + slot_idx * ref_element_size())
    } else {
        old_obj.as_ptr() as usize
    };
    if is_ref_array {
        // SAFETY: `slot_idx` is within array bounds (from card scan).
        let slot_ptr = unsafe {
            old_obj
                .as_ptr()
                .add(ARRAY_DATA_OFFSET + slot_idx * ref_element_size())
        };
        // SAFETY: `slot_ptr` is a valid 8-byte ref element.
        let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
        if raw != 0 {
            mark_edge_precise(raw as usize, ctx, bits, worklist, "dirty-card");
        }
    } else if is_compact_object(header) {
        // Compact object: `slot_idx` is the BYTE OFFSET of an 8-byte
        // reference slot (as recorded by scan_dirty_cards).
        // SAFETY: byte offset within this object's body (from card scan).
        let slot_ptr = unsafe { old_obj.as_ptr().add(slot_idx) };
        // SAFETY: an in-bounds reference slot of a live old object.
        let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
        if raw != 0 {
            mark_edge_precise(raw as usize, ctx, bits, worklist, "dirty-card");
        }
    } else {
        // SAFETY: `slot_idx` is within `num_slots` (from card scan).
        let slot_ptr = unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
        // SAFETY: `slot_ptr` is a valid Value-sized slot.
        let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
        if let Value::Object(Some(ref_obj)) = value {
            mark_edge_precise(ref_obj.as_ptr() as usize, ctx, bits, worklist, "dirty-card");
        }
    }
    card
}

/// Everything a selective-promotion walk chunk reads, as plain `Sync` data
/// (gch h1/yy). The values are the sweep's own for walk (2): the free-block
/// snapshot, the mark set (both channels), the pin set, and the cycle's
/// promotion rule.
pub struct PromotionWalkCtx<'a> {
    pub from_base: usize,
    pub used: usize,
    pub free_blocks: &'a [(usize, usize)],
    pub side_bits: &'a cratonvm_gc_core::young_mark::YoungMarkBits,
    pub side_sorted: &'a [usize],
    pub pinned: &'a FxHashSet<usize>,
    /// The walk promotes (`false`: the age-only walk).
    pub selective_on: bool,
    /// `defrag_escalate || promote_pressure_tenure`: `should_tenure`'s
    /// `force_promote_all` slot.
    pub force_tenure: bool,
    pub promotion_age: u8,
    /// [`GridSizeMemo`] armed (read once by the sweep, as for `SweepCtx`).
    pub size_memo: bool,
    /// JIT round 23 wave 6, lane L4 (`CRATONVM_GEN_SWEEP_FUSED_WALK`): also
    /// decide the sweep walk's [`sweep_chunk`] result for the chunk in the
    /// same pass, under the sweep's grid rules (see [`promotion_walk_chunk`]).
    /// `None`: the promotion walk alone, as before.
    pub fuse: Option<FusedSweepCtx<'a>>,
}

/// Wave 6: the [`SweepCtx`] fields the fused pass needs beyond the promotion
/// walk's own (its free list, mark set, extent and memo are the same values:
/// `existing_free` IS `sweep_free_blocks`, both the oracle's `exact_skips`).
#[derive(Clone, Copy)]
pub struct FusedSweepCtx<'a> {
    pub old_lo: usize,
    pub old_hi: usize,
    pub watched: Option<&'a cratonvm_gc_core::gc_quiescence::WatchedSnapshot>,
}

/// One anchored chunk of walk (2), decided READ-ONLY: nothing is copied,
/// installed or aged here, so a refused attempt is thrown away whole and the
/// serial walk runs as if nothing had happened.
#[derive(Debug, Default)]
pub struct PromotionWalkChunk {
    /// `(address, size)` of every marked, tenurable, unpinned grid object, in
    /// ascending address order: the serial walk's promotion candidates.
    pub candidates: Vec<(usize, usize)>,
    /// The serial walk's deferred age bumps, in the same order.
    pub age_bumps: Vec<usize>,
    /// The serial walk's `SP_CENSUS` contributions (`candidates`, `unaged`,
    /// `pinned`), summed.
    pub census_candidates: u64,
    pub census_unaged: u64,
    pub census_pinned: u64,
    /// Grid objects per mark channel (fix-up 3a's replay proof).
    pub side_marked: usize,
    pub header_marked: usize,
    /// [`GridSizeMemo::hits`] of this chunk.
    pub memo_hits: u64,
    /// Wave 6 (`CRATONVM_GEN_SWEEP_FUSED_WALK`): the sweep walk's result for
    /// this chunk, decided in the same pass; `None` when not fused.
    pub sweep: Option<SweepChunkResult>,
    /// Wave 8 (`CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP`): `hi - lo` when this
    /// chunk was declared dead whole by [`dead_chunk_sweep`] (no header read);
    /// 0 for a walked chunk.
    pub dead_chunk_bytes: usize,
}

/// JIT round 23 wave 8, lane W8-L3 (`CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP`):
/// what decides whether a fused chunk may be declared dead whole.
#[derive(Clone, Copy)]
pub struct DeadChunkScreen<'a> {
    /// Ascending, deduplicated ABSOLUTE addresses that force a walk of the
    /// chunk holding them: every conservative candidate (roots, finalizer
    /// addresses, dirty-card walk-gap words), every pin, every mark the oracle
    /// left unresolved. A chunk holding one keeps the walk's own policy for it
    /// (the zero-run / late-pin / root-in-dead-span rules all key on these
    /// addresses), so declaring dead never meets a word some root names.
    pub keep: &'a [usize],
}

/// Wave 8 (`CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP`, proposal 14): decide the
/// fused chunk `[lo, hi)` WITHOUT walking it, or `None` to walk it.
///
/// Declared dead whole only when all of these hold:
/// * the walk is fused (`ctx.fuse`), so the phantom rule is on in every
///   WALKED chunk: a marked base strictly inside a parsed extent refuses the
///   attempt. With the landing rule (a walked chain must end exactly on its
///   `hi`), every marked base of a walked chunk is then parsed exactly at its
///   base with its true size and ends inside its own chunk, even when that
///   chunk's `lo` follows a dead chunk instead of an induced chain (the
///   anchors are allocator-certified boundaries since wave 4c; a wrong one can
///   only make that walk refuse). So promotion candidates and age bumps, which
///   are taken only at marked bases, are the full walk's;
/// * no marked base (`side_sorted`, the complete young live set since the
///   Family-A hardening: nothing on this path writes `GC_FLAG_MARKED` into a
///   young header) lies in `[lo, hi)`;
/// * no address of `screen.keep` lies in `[lo, hi)`;
/// * the nearest marked base below `lo` sizes to a plausible extent ending at
///   or before `lo` (defence in depth: the landing rule of its own walked
///   chunk already proves it).
///
/// The result is the walk's for a chunk with no live object: no candidate,
/// no age bump, no survivor, no watched referent, no forward (a cycle whose
/// fused result is used promotes nothing), and dead spans covering every byte
/// of `[lo, hi)` outside the free / skip list. Unlike the walk, it also frees
/// runs of unmarked empty objects (the walk steps over them and keeps them,
/// a deliberate over-retention) and folds GAP fillers into the span around
/// them; a span shorter than `HEADER_SIZE` (a lone GAP filler between two
/// free blocks) is left where it is, as the walk leaves it. Each span counts
/// as one object in the sweep's `objects_swept` (a diagnostic figure).
pub fn dead_chunk_sweep(
    ctx: &PromotionWalkCtx<'_>,
    screen: &DeadChunkScreen<'_>,
    lo: usize,
    hi: usize,
) -> Option<PromotionWalkChunk> {
    ctx.fuse?;
    if lo >= hi || hi > ctx.used {
        return None;
    }
    let (a_lo, a_hi) = (ctx.from_base + lo, ctx.from_base + hi);
    let si = ctx.side_sorted.partition_point(|&a| a < a_lo);
    if ctx.side_sorted.get(si).is_some_and(|&a| a < a_hi) {
        return None;
    }
    let ki = screen.keep.partition_point(|&a| a < a_lo);
    if screen.keep.get(ki).is_some_and(|&a| a < a_hi) {
        return None;
    }
    if let Some(&prev) = si.checked_sub(1).and_then(|j| ctx.side_sorted.get(j)) {
        if prev < ctx.from_base {
            return None;
        }
        // SAFETY: `prev` is a marked young base below `lo`, inside mapped
        // from-space (`side_sorted` holds from-space addresses only); the
        // pause is stopped the world and nothing has been written yet.
        let h = unsafe { &*(prev as *const ObjectHeader) };
        let size = gen_object_total_size(h);
        if size < HEADER_SIZE || prev.checked_add(size).is_none_or(|end| end > a_lo) {
            return None;
        }
    }
    let mut sweep = SweepChunkResult::default();
    let mut push = |off: usize, len: usize| {
        if len < HEADER_SIZE {
            return;
        }
        // SAFETY: `off` is inside `[lo, hi)`, mapped from-space; the header is
        // read only for the span's forensic class / kind (`record_swept`),
        // never trusted for a size.
        let header = unsafe { &*((ctx.from_base + off) as *const ObjectHeader) };
        sweep.dead.push((
            off,
            len,
            header.class_id.as_u32(),
            ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
            1,
        ));
    };
    let first = ctx.free_blocks.partition_point(|&(off, sz)| off + sz <= lo);
    let mut cur = lo;
    for &(off, sz) in &ctx.free_blocks[first..] {
        if off >= hi || cur >= hi {
            break;
        }
        if off > cur {
            push(cur, off - cur);
        }
        cur = cur.max(off + sz);
    }
    if cur < hi {
        push(cur, hi - cur);
    }
    Some(PromotionWalkChunk {
        sweep: Some(sweep),
        dead_chunk_bytes: hi - lo,
        ..PromotionWalkChunk::default()
    })
}

/// Walk (2) of `sweep_young_non_moving` over `[lo, hi)`, as one chunk.
///
/// Returns `None` on ANY step the serial walk would treat as an anomaly
/// (free-block overshoot, unlisted zero span, implausible size, hole
/// crossing, malformed GAP filler) and when the chain does not land exactly on
/// `hi`. The rules are the serial walk's (`GridRules` below is its literal,
/// with `zero_run_hi = used` as there), so a chunk that started on the serial
/// walk's grid and landed on `hi` parsed exactly the objects the serial walk
/// parses in `[lo, hi)`; the first chunk starts at 0, where the serial walk
/// starts, and by induction every accepted attempt is the serial walk's parse
/// of the whole arena with no anomaly in it. The per-object decisions read
/// only state nothing in the walk writes (headers, the mark set, the pin set),
/// so they are the serial walk's decisions. A malformed GAP filler, which the
/// serial walk parses as a header, refuses here (the conservative direction:
/// the serial walk then runs).
pub fn promotion_walk_chunk(
    ctx: &PromotionWalkCtx<'_>,
    lo: usize,
    hi: usize,
) -> Option<PromotionWalkChunk> {
    let mut out = PromotionWalkChunk::default();
    let start = ctx.free_blocks.partition_point(|&(off, sz)| off + sz <= lo);
    let mut free_iter = ctx.free_blocks[start..].iter().peekable();
    // Wave 6 (`CRATONVM_GEN_SWEEP_FUSED_WALK`): fused, the walk takes
    // `sweep_chunk`'s grid rules -- the empty-run measure bounded by the chunk
    // end and the phantom check on -- so it parses exactly what `sweep_chunk`
    // would and refuses wherever it would. Both are stricter than the
    // promotion walk's own rules and agree with them on every clean grid (a
    // run crossing `hi` fails the landing either way; a phantom extent is an
    // anomaly the serial walk then owns), so a refusal costs the serial
    // promotion walk, never a different decision.
    let fused = ctx.fuse;
    let mut sweep = fused.map(|_| SweepChunkResult::default());
    let rules = GridRules {
        base: ctx.from_base,
        used: ctx.used,
        zero_run_hi: if fused.is_some() { hi } else { ctx.used },
        side_sorted: ctx.side_sorted,
        zero_runs: ZeroRunRule::StepOver {
            unresolved: None,
            count_empty_runs: true,
        },
        bad_gap_filler_is_anomaly: true,
        phantom: fused.is_some(),
    };
    // `sweep_chunk`'s monotone phantom probe (unused when not fused: the
    // promotion rules leave `phantom` off).
    let mut live_probe = ctx.side_sorted.partition_point(|&a| a < ctx.from_base + lo);
    let mut size_memo = GridSizeMemo::new(ctx.size_memo);
    let mut cursor = lo;
    while cursor < hi {
        let (resynced, overshot) = skip_free_blocks(&mut cursor, &mut free_iter);
        if overshot {
            return None;
        }
        if resynced {
            continue;
        }
        if cursor >= hi {
            break;
        }
        let next_free = free_iter.peek().map(|&&(off, _)| off);
        // `cursor < hi <= used`: inside the mapped from-space region
        // (`probe_grid_at`'s contract).
        let total_size = match probe_grid_at_sized(
            cursor,
            next_free,
            &rules,
            &mut live_probe,
            &mut size_memo,
        ) {
            GridProbe::Object { size } => size,
            GridProbe::Gap { len } => {
                cursor += len;
                continue;
            }
            GridProbe::EmptyRun { resume } => {
                cursor = resume;
                continue;
            }
            // `Phantom` only when fused (the sweep's rule); every other
            // verdict is an anomaly the serial walk owns.
            GridProbe::Phantom { .. }
            | GridProbe::ZeroSpan { .. }
            | GridProbe::BadSize { .. }
            | GridProbe::BadGapFiller
            | GridProbe::CrossesFreeBlock { .. } => return None,
        };
        let addr = ctx.from_base + cursor;
        // SAFETY: `probe_grid_at` parsed a plausible header at this in-bounds,
        // on-grid offset of mapped from-space.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        // The serial walk's decisions, verbatim (`late_pins` is empty and the
        // census is batched whenever this walk runs).
        let aged = should_tenure(header.gc_age(), ctx.force_tenure, ctx.promotion_age);
        let header_marked = header.gc_flags() & GC_FLAG_MARKED != 0;
        let side_marked = ctx.side_bits.contains(addr);
        let marked = header_marked || side_marked;
        out.side_marked += usize::from(side_marked);
        out.header_marked += usize::from(header_marked);
        let is_pinned = marked && ctx.pinned.contains(&addr);
        if marked && ctx.selective_on {
            out.census_candidates += 1;
            if !aged {
                out.census_unaged += 1;
            } else if is_pinned {
                out.census_pinned += 1;
            }
        }
        if marked && !aged && (!header_marked || side_marked) {
            out.age_bumps.push(addr);
        }
        if ctx.selective_on && marked && aged && !is_pinned {
            out.candidates.push((addr, total_size));
        }
        // Wave 6: `sweep_chunk`'s classification of the same object, verbatim
        // (it reads only what nothing between the two walks writes when the
        // cycle promotes nothing: the forwarding state, the mark set, the
        // header mark bit; the age bumps applied in between touch the age
        // bits alone, and only of objects that are not in `survivors`).
        if let (Some(f), Some(s)) = (fused, sweep.as_mut()) {
            if header.is_forwarded() {
                let fwd = header.forwarding_address() as usize;
                if fwd >= f.old_lo && fwd < f.old_hi {
                    s.forward_targets.push(fwd);
                    push_dead(&mut s.dead, cursor, total_size, header);
                }
            } else if side_marked {
                s.live += 1;
                if header_marked {
                    s.side_with_header_mark += 1;
                }
                if f.watched.is_some_and(|w| w.contains(&addr)) {
                    s.watched.push(addr);
                }
            } else if header_marked {
                s.live += 1;
                s.survivors.push(cursor);
                if f.watched.is_some_and(|w| w.contains(&addr)) {
                    s.watched.push(addr);
                }
            } else {
                push_dead(&mut s.dead, cursor, total_size, header);
            }
        }
        cursor += total_size;
    }
    if cursor != hi {
        return None;
    }
    out.memo_hits = size_memo.hits;
    out.sweep = sweep;
    Some(out)
}

/// Run walk (2) over `[anchors[0], anchors[last])`, one chunk per anchor
/// interval, on `threads` workers (inline without a pool). `None` when any
/// chunk refused; the caller then runs the serial walk from scratch, which
/// owns every anomaly report and the unwind / re-anchor policy.
///
/// gch h1/yy (`gengc-r5w3-evac7-...`, "the serial selective-promotion walk"):
/// the walk visits every object allocated since the last cycle, dead or alive,
/// and only a promoted survivor needs the collecting thread (its old-gen
/// allocation and copy). The chunks decide; the caller copies the candidates
/// serially in ascending order, so every old-gen destination is the one the
/// serial walk would have allocated.
pub fn parallel_promotion_walk(
    ctx: &PromotionWalkCtx<'_>,
    anchors: &[usize],
    threads: usize,
    pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
) -> Option<Vec<PromotionWalkChunk>> {
    parallel_promotion_walk_screened(ctx, anchors, threads, pool, None)
}

/// [`parallel_promotion_walk`], with each chunk first offered to
/// [`dead_chunk_sweep`] when `dead_screen` is given (wave 8,
/// `CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP`; only on a fused walk).
pub fn parallel_promotion_walk_screened(
    ctx: &PromotionWalkCtx<'_>,
    anchors: &[usize],
    threads: usize,
    pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
    dead_screen: Option<&DeadChunkScreen<'_>>,
) -> Option<Vec<PromotionWalkChunk>> {
    let nchunks = anchors.len().checked_sub(1)?;
    if nchunks == 0 {
        return None;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<PromotionWalkChunk>>> =
        (0..nchunks).map(|_| std::sync::Mutex::new(None)).collect();
    let failed = std::sync::atomic::AtomicBool::new(false);

    let worker = || loop {
        if failed.load(Ordering::Relaxed) {
            return;
        }
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= nchunks {
            return;
        }
        let dead = dead_screen.and_then(|s| dead_chunk_sweep(ctx, s, anchors[i], anchors[i + 1]));
        match dead.or_else(|| promotion_walk_chunk(ctx, anchors[i], anchors[i + 1])) {
            Some(r) => {
                *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            }
            None => {
                failed.store(true, Ordering::Relaxed);
                return;
            }
        }
    };

    match pool {
        Some(p) if threads > 1 => run_chunk_workers(p, threads, &worker),
        _ => worker(),
    }
    if failed.load(Ordering::Relaxed) {
        return None;
    }
    let mut out = Vec::with_capacity(nchunks);
    for r in results {
        out.push(r.into_inner().unwrap_or_else(|e| e.into_inner())?);
    }
    Some(out)
}

/// gc-genpause F1 -- walk `[lo, hi)` of from-space recording every object
/// start, and PROVE the chunk by requiring the chain to land exactly on `hi`.
///
/// The parallel half of the young object-start walk. `None` means "this chunk
/// is not provable", and the driver then abandons the whole parallel attempt
/// and lets the untouched sequential walk run from scratch -- so every
/// judgement call here is made in the refusing direction.
///
/// # Deliberately stricter than the sequential walk
///
/// The sequential walk RESYNCS when its cursor lands inside a free block
/// (`skip_free_blocks` moves to the block end and warns). This refuses
/// instead. It is walking from an ANCHOR rather than from offset 0, so a
/// cursor inside a free block means the anchor grid and the free list
/// disagree about this chunk, and the exact-landing proof is worth nothing if
/// the walk is allowed to guess its way back onto the grid. Refusing costs one
/// sequential walk; guessing costs a bitmap with a bit set on a non-object,
/// and `forward_object` relocates through those.
///
/// This duplicates the sequential walk's per-object stepping, which is a drift
/// risk and is the same trade `sweep_chunk` already makes against its own
/// sequential walk. The duplication is safe in one direction only: because
/// every disagreement here returns `None`, drift can cause an unnecessary
/// fallback but never a wrong bitmap.
pub fn objstart_chunk(
    young_base: usize,
    lo: usize,
    hi: usize,
    skips: &[(usize, usize)],
    bits: &cratonvm_gc_core::young_mark::ObjectStartBits,
) -> Option<()> {
    objstart_chunk_with(young_base, lo, hi, skips, bits, false)
}

/// [`objstart_chunk`]; with `settle_claims`, a header another worker is
/// claiming right now (`FORWARDED | BUSY`) is waited out before it is sized.
///
/// gcj pcp (`CRATONVM_GEN_PRECISE_EDGES_SKIP_STARTS`): precise edges forward
/// without asking the lazy grid, so a chunk can be walked for the first time
/// after objects in it moved. A forwarded header still sizes as its object
/// (`ObjectHeader::resolved_shape` follows the forward to the copy, and a
/// compact instance sizes from its class id, which a forward leaves alone),
/// except for the instant between a claim and its publish, when the second
/// word may already hold the target; that instant is what this waits out.
pub fn objstart_chunk_with(
    young_base: usize,
    lo: usize,
    hi: usize,
    skips: &[(usize, usize)],
    bits: &cratonvm_gc_core::young_mark::ObjectStartBits,
    settle_claims: bool,
) -> Option<()> {
    // Position in the skip list. A block that STRADDLES `lo` means the anchor
    // sits inside free/TLAB space and is not an object start at all.
    // One atomic OR per 512-byte word instead of one per object: a chunk
    // visits object starts in ascending order, so consecutive starts land in
    // the same word about a dozen times running.
    //
    // gen r4w2/youngmark: every refusal names its reason through
    // `objstart_bail` (see `OBJSTART_CHUNK_BAILS`); the decisions are unchanged.
    let mut bits = bits.run();
    // gkr: sized through the walk's size memo, as the sweeps' walks are
    // (`CRATONVM_GEN_WALK_SIZE_MEMO`): this chain visits every object of the
    // chunk, and on JIT-warm javac the start walk covers most of eden.
    let mut size_memo = GridSizeMemo::new(gen_walk_size_memo_enabled());
    let mut idx = skips.partition_point(|&(off, _)| off < lo);
    if idx > 0 {
        let (off, sz) = skips[idx - 1];
        let Some(end) = off.checked_add(sz) else {
            return objstart_bail(1);
        };
        if end > lo {
            return objstart_bail(0);
        }
    }

    let mut cursor = lo;
    while cursor < hi {
        // Free / un-retired-TLAB spans are skipped, not parsed -- but only
        // when the cursor is exactly at one's start (see the doc comment).
        //
        // gcj j1yg-close: and never past the chunk end. A skip that ends
        // exactly at `hi` (a TLAB tail) followed by one that starts there (a
        // free block) used to be crossed in one go, so the chain overshot a
        // chunk whose layout was exactly what the allocator wrote and the
        // chunk was refused -- a FATAL stop once the lazy grid is sealed
        // (`GenR5W1SoftRefYoungProbe`, 3 of 3 runs, since the Serial young
        // geometry made such pairs common). A skip starting at `hi` is the
        // next chunk's.
        while cursor < hi && idx < skips.len() && skips[idx].0 == cursor {
            let Some(next) = cursor.checked_add(skips[idx].1) else {
                return objstart_bail(1);
            };
            cursor = next;
            idx += 1;
        }
        if cursor >= hi {
            break;
        }
        if idx < skips.len() && skips[idx].0 < cursor {
            return objstart_bail(2); // cursor is past a block start: inside it
        }

        let obj_ptr = (young_base + cursor) as *mut u8;
        // `cursor` is an allocator-written object boundary inside the young
        // arena -- either the chunk's proven anchor or the end of the previous
        // object's proven extent -- and free/TLAB spans were skipped.
        //
        // gen r5w1/young5: the grid rules are `probe_grid_at`'s, bounded by
        // the chunk end (`used: hi`): a GAP filler or an object may not run
        // past `hi` (the chain must land on it exactly), `Parse` zero runs
        // (every empty object gets its start bit, as the sequential walk
        // gives it), and every anomaly refuses the chunk under its reason.
        // Bug-D parity: a sub-HEADER_SIZE TLAB tail sentinel is strided over,
        // not parsed as an object.
        let rules = GridRules {
            base: young_base,
            used: hi,
            zero_run_hi: hi,
            side_sorted: &[],
            zero_runs: ZeroRunRule::Parse,
            bad_gap_filler_is_anomaly: true,
            phantom: false,
        };
        let next_free = skips.get(idx).map(|&(off, _)| off);
        if settle_claims {
            // SAFETY: `cursor` is an object boundary inside mapped from-space
            // (as for the probe below), so a whole header word lies there.
            let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if h.class_id.as_u32() != cratonvm_gc_core::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                settle_forwarding_claim(h);
            }
        }
        let mut no_phantom_probe = 0usize;
        let size = match probe_grid_at_sized(
            cursor,
            next_free,
            &rules,
            &mut no_phantom_probe,
            &mut size_memo,
        ) {
            GridProbe::Gap { len } => {
                cursor += len;
                continue;
            }
            GridProbe::Object { size } => size,
            GridProbe::BadGapFiller => return objstart_bail(3),
            // An object may not span a free/TLAB range.
            GridProbe::CrossesFreeBlock { .. } => return objstart_bail(5),
            // `Parse` yields no zero-run verdicts and `phantom` is off: this
            // arm is the implausible extent.
            GridProbe::BadSize { .. }
            | GridProbe::EmptyRun { .. }
            | GridProbe::ZeroSpan { .. }
            | GridProbe::Phantom { .. } => return objstart_bail(4),
        };
        // A misaligned start cannot be represented in the bitmap; aliasing a
        // neighbour's bit would make `forward_object` relocate through an
        // interior word.
        if !bits.insert(obj_ptr as usize) {
            return objstart_bail(6);
        }
        cursor += size;
    }

    // THE PROOF. Landing anywhere but exactly on the next anchor means this
    // chunk's chain and the grid disagree, and nothing it recorded is trusted.
    if cursor == hi {
        Some(())
    } else {
        objstart_bail(7)
    }
}

/// gc-genpause F1 -- drive [`objstart_chunk`] across `threads` workers, or
/// return `None` and leave the caller to walk sequentially.
///
/// Mirrors [`parallel_sweep_walk`] deliberately, including the wholesale
/// abandon on the first refusal: a partially-filled bitmap is worse than no
/// bitmap, so the caller rebuilds a fresh one for the fallback rather than
/// reusing whatever this left behind.
///
/// `pool` is the heap's persistent worker pool (`GenerationalHeap::evac_pool`);
/// see [`run_chunk_workers`] for how `threads` maps onto it.
pub fn parallel_objstart_walk(
    young_base: usize,
    anchors: &[usize],
    skips: &[(usize, usize)],
    bits: &cratonvm_gc_core::young_mark::ObjectStartBits,
    threads: usize,
    pool: &cratonvm_gc_core::evac_pool::EvacPool,
) -> bool {
    let nchunks = anchors.len() - 1;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);

    let worker = || loop {
        if failed.load(Ordering::Relaxed) {
            return;
        }
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= nchunks {
            return;
        }
        if objstart_chunk(young_base, anchors[i], anchors[i + 1], skips, bits).is_none() {
            failed.store(true, Ordering::Relaxed);
            return;
        }
    };

    run_chunk_workers(pool, threads, &worker);
    !failed.load(Ordering::Relaxed)
}

/// Sweep `[anchors[0], anchors[last])` with `threads` workers, one chunk per
/// anchor interval. Returns `None` if ANY chunk saw a grid anomaly.
///
/// `pool` is the heap's persistent worker pool (`GenerationalHeap::evac_pool`);
/// see [`run_chunk_workers`] for how `threads` maps onto it.
pub fn parallel_sweep_walk(
    ctx: &SweepCtx<'_>,
    anchors: &[usize],
    threads: usize,
    pool: &cratonvm_gc_core::evac_pool::EvacPool,
) -> Option<Vec<SweepChunkResult>> {
    let nchunks = anchors.len() - 1;
    let next: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<SweepChunkResult>>> =
        (0..nchunks).map(|_| std::sync::Mutex::new(None)).collect();
    let failed = std::sync::atomic::AtomicBool::new(false);

    let worker = || loop {
        if failed.load(Ordering::Relaxed) {
            return;
        }
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= nchunks {
            return;
        }
        match sweep_chunk(ctx, anchors[i], anchors[i + 1]) {
            Some(r) => {
                *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            }
            None => {
                failed.store(true, Ordering::Relaxed);
                return;
            }
        }
    };

    run_chunk_workers(pool, threads, &worker);
    if failed.load(Ordering::Relaxed) {
        return None;
    }
    let mut out = Vec::with_capacity(nchunks);
    for r in results {
        out.push(r.into_inner().unwrap_or_else(|e| e.into_inner())?);
    }
    Some(out)
}

/// gcj j1yg-pause: what the dirty-card scan reads per walked object
/// ([`GenerationalHeap::scan_dirty_cards_inner`]): the reference slots of one
/// card-walk hit that hold a young from-space address, as `(holder, slot, 0)`
/// edges. Plain shared data, so the parallel scan ([`par_card_scan`]) runs
/// the same reads on every worker as the serial loop runs on the driver.
pub struct CardHitScan<'a> {
    /// Young from-space's CAPACITY extent (`Arena::contains`).
    pub young: (usize, usize),
    pub trust_element_cards: bool,
    pub completion_whole_cards: &'a [usize],
    pub dirty_ranges: &'a [(usize, usize)],
    pub card_base: usize,
    pub card_size: usize,
    pub old_base: usize,
}

impl CardHitScan<'_> {
    /// The edges of one hit, appended to `out`; with `vals`, each edge's
    /// young value too, at the same index (what `card_pair_value_and_card`
    /// would read back from the slot).
    ///
    /// # Safety
    /// `hit` is one the old generation's dirty-card walk produced over
    /// `self.dirty_ranges` in this pause: a valid old-gen object, sized by
    /// the walk.
    pub unsafe fn scan(
        &self,
        hit: crate::old_gen::CardRangeHit,
        out: &mut Vec<(ObjectRef, usize, usize)>,
        mut vals: Option<&mut Vec<usize>>,
    ) {
        let (obj_ptr, total_size, window) = (hit.ptr, hit.size, hit.window);
        let (young_lo, young_hi) = self.young;
        let in_young = |p: *mut u8| (p as usize) >= young_lo && (p as usize) < young_hi;
        // SAFETY: `obj_ptr` is from `old_gen.walk_card_ranges()` (which answers
        // with the same object boundaries as `walk_objects_in_card_ranges()`),
        // pointing to a valid old-gen object header.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

        // [LOW gc-genheap-cards] Plausibility cap, mirroring the non-moving
        // sweep walker (gen_object_total_size + the `num_slots <= 1<<24` /
        // `array_length <= i32::MAX` re-sync checks ~4239-4246). Previously
        // the ref-slot loops below trusted `header.num_slots()` /
        // `header.array_length()` verbatim, so a corrupt old-gen header (e.g.
        // an inline-alloc path that left a garbage slot count, or a header
        // straddling a buffer that walk_objects_in_card_ranges mis-bounded)
        // would drive an out-of-range slot scan reading past the object.
        //
        // `gen_object_total_size` returns 0 for an implausible header
        // (oversized num_slots, kind=Object-with-array_length, bad array
        // length); skip such an object with a diagnostic rather than
        // scanning bogus slots. Then bound the per-object iteration to the
        // ref-slot count that actually fits in `total_size` (the object's
        // region size as established by the walker), so even a header that
        // passes the coarse plausibility gate but reports more slots than
        // its bytes hold cannot read out of bounds.
        let safe_size = gen_object_total_size(header);
        if safe_size < HEADER_SIZE || safe_size != total_size {
            tracing::warn!(
                "GC scan_dirty_cards: implausible/inconsistent old-gen header \
                 (class_id={} kind=0x{:02x} num_slots={} array_length={} \
                 gen_size={} walker_size={}) — skipping ref-slot scan",
                header.class_id.as_u32(),
                ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                header.num_slots(),
                header.array_length(),
                safe_size,
                total_size,
            );
            return;
        }
        // Field/element bytes available in this object's region (total
        // minus header). Used to cap the slot/element count.
        let body_bytes = total_size - HEADER_SIZE;

        // Scan ref slots: ref arrays use compact 8-byte pointers,
        // object fields use 16-byte Value.
        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                // Cap element count at what the array's data region holds.
                let esz = ref_element_size();
                let max_elems = body_bytes / esz;
                let elems = (header.array_length() as usize).min(max_elems);
                // gen r4w3/cards3: a WINDOW hit reads only the elements
                // whose slot starts in `[lo, hi)` (byte offsets from the
                // header) — exactly the slots whose card
                // `write_barrier_ref_array_element` would have dirtied.
                // A whole hit reads them all, as before.
                let mut scan_elements = |first: usize, last: usize| {
                    for i in first..last {
                        // SAFETY: `i` < capped element count; offset within array data region.
                        let s_ptr =
                            unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                        let raw: u64 = unsafe { read_ref_slot(s_ptr) };
                        if raw != 0 {
                            let ref_ptr = raw as usize as *mut u8;
                            if in_young(ref_ptr) {
                                // SAFETY: `obj_ptr` is a valid old-gen object pointer; wrapping in ObjectRef is sound.
                                let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                                out.push((obj_ref, i, 0));
                                if let Some(v) = vals.as_deref_mut() {
                                    v.push(raw as usize);
                                }
                            }
                        }
                    }
                };
                match window {
                    // gcd d5/s (opt-in, see `trust_element_cards`): a
                    // header-card hit reads only the dirty ranges it
                    // overlaps, the header's own range included.
                    // gcj j2/jt2 residual 4: not when the take-over's
                    // register completion dirtied the header's card (a
                    // frozen post-only `aastore` may have held only the
                    // header): read whole, as without the opt-in.
                    None if self.trust_element_cards
                        && self.completion_whole_cards
                            .binary_search(
                                &((obj_ptr as usize).wrapping_sub(self.card_base) / self.card_size),
                            )
                            .is_err() =>
                    {
                        for (lo, hi) in array_dirty_windows(
                            self.dirty_ranges,
                            (obj_ptr as usize).wrapping_sub(self.old_base),
                            total_size,
                        ) {
                            let (first, last) = window_element_span(lo, hi, elems, esz);
                            scan_elements(first, last);
                        }
                    }
                    None => scan_elements(0, elems),
                    Some((lo, hi)) => {
                        let (first, last) = window_element_span(lo, hi, elems, esz);
                        scan_elements(first, last);
                    }
                }
            }
        } else if window.is_some() {
            // gen r4w3/cards3: the walk produces windows for reference
            // arrays only (nothing marks a non-header card of any other
            // object), so this arm is unreachable; a window on anything
            // else has no element range to read.
            return;
        } else if is_compact_object(header) {
            // Compact object: 8-byte reference slots at the oop-map offsets.
            // Record the BYTE OFFSET as slot_idx — the seed/fixup consumers
            // read it back as a byte offset for compact receivers.
            //
            // gengc-round4/cards (2026-09-23): the arm is chosen by the
            // per-object `GC_FLAG_COMPACT` bit, exactly as every CONSUMER
            // of these tuples chooses it (`seed_dirty_card_roots` and the
            // non-moving seed loop both test `is_compact_object`). It used
            // to be `if let Some(..) = compact_oop_scan(header)`, whose
            // `None` means BOTH "legacy object" and "compact object whose
            // layout did not resolve" — and the second case fell through
            // to the legacy arm below, which would record LEGACY slot
            // indices for an object the consumers then treat as compact,
            // reading and rewriting `HEADER_SIZE + slot_idx` as a byte
            // offset. `for_each_ref_slot` was fixed for the same ambiguity
            // under HIB-DCAST-LATEPHASE.1; this walk was not. (Today the
            // legacy arm's `body_bytes` cap happens to make that case scan
            // nothing, because `object_body_size` sizes an unresolved
            // compact body as implausible and the walker/header size check
            // above then skips the object — but that is a coincidence of
            // two other functions, not a property of this one.) A compact
            // object with no layout has no provably-safe slots: skip it.
            //
            // gc-common w3-e: borrowing form (`with_compact_oop_scan`), no
            // layout `Arc` clone/drop per scanned object; `None` (no
            // layout) still skips the object.
            let _ = cratonvm_gc_core::heap::with_compact_oop_scan(header, |layout, compact_total| {
                // Absolute displacements against the object's total size.
                let body = compact_total.min(total_size);
                for &off in &layout.ref_disps {
                    let off = off as usize;
                    if off + ref_field_size() > body {
                        break;
                    }
                    // SAFETY: `off` is within the object's body (capped above).
                    let s_ptr = unsafe { obj_ptr.add(off) };
                    let raw: u64 = unsafe { read_ref_slot(s_ptr) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if in_young(ref_ptr) {
                            // SAFETY: valid old-gen object pointer.
                            let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                            out.push((obj_ref, off, 0));
                            if let Some(v) = vals.as_deref_mut() {
                                v.push(raw as usize);
                            }
                        }
                    }
                }
            });
        } else {
            // Cap slot count at what the object's field region holds.
            let max_slots = body_bytes / SLOT_SIZE;
            let slots = (header.num_slots() as usize).min(max_slots);
            for slot_idx in 0..slots {
                // SAFETY: `slot_idx` < capped slot count; offset within the object's field region.
                let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    if in_young(ref_obj.as_ptr()) {
                        // SAFETY: `obj_ptr` is a valid old-gen object pointer; wrapping in ObjectRef is sound.
                        let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                        out.push((obj_ref, slot_idx, 0));
                        if let Some(v) = vals.as_deref_mut() {
                            v.push(ref_obj.as_ptr() as usize);
                        }
                    }
                }
            }
        }
    }
}

/// One group's buffers in [`par_card_scan`]: the walk's hits, the edges they
/// yield and, when asked, the edges' young values at the same indices. gcj
/// pcp: kept by the heap between cycles (`card_scan_parts`), so a pause does
/// not grow ~64 groups' lists from empty.
#[derive(Default)]
pub struct CardScanPart {
    pub hits: Vec<crate::old_gen::CardRangeHit>,
    pub edges: Vec<(ObjectRef, usize, usize)>,
    pub vals: Vec<usize>,
}

// SAFETY: a part holds addresses (old-gen hits and edges) and integers as
// plain data. They are dereferenced only by the dirty-card scan inside a pause,
// with every mutator stopped; the heap keeps the emptied buffers between
// pauses, and a worker of the pool fills one group's part under its lock.
unsafe impl Send for CardScanPart {}

/// Below this many dirty ranges the scan stays on the driver: a dispatch
/// costs more than the walk.
pub const PAR_CARD_SCAN_MIN_RANGES: usize = 512;



/// gcj j1yg-pause: the dirty-card walk and its slot reads on `workers`
/// threads of `pool`. The ranges are cut into contiguous groups (several per
/// worker, claimed one at a time); each group is walked from its own anchor
/// ([`crate::old_gen::CardWalkView::walk_group`]) and its hits scanned into
/// the group's own edge list. The lists, in group order, are the serial
/// loop's edges in its order. `None` when the view is refused or any group
/// met a header it cannot size: the caller then takes the serial walk, which
/// reports what it cannot parse as gap windows. Nothing here writes the heap.
///
/// gcj pcp: the groups' buffers come from (and go back to) `parts_buf`; the
/// answer is the number of groups, whose output is `parts_buf[..groups]`.
#[allow(clippy::too_many_arguments)]
pub fn par_card_scan(
    old_gen: &OldGen,
    dirty_ranges: &[(usize, usize)],
    overlaps: bool,
    hit_scan: &CardHitScan<'_>,
    pool: &cratonvm_gc_core::evac_pool::EvacPool,
    workers: usize,
    with_values: bool,
    parts_buf: &mut Vec<CardScanPart>,
) -> Option<usize> {
    let n = dirty_ranges.len();
    if workers < 2 || n < PAR_CARD_SCAN_MIN_RANGES {
        return None;
    }
    let groups = (workers * 8).min(n / 64).max(2);
    if parts_buf.len() < groups {
        parts_buf.resize_with(groups, Default::default);
    }
    let parts: Vec<std::sync::Mutex<CardScanPart>> = parts_buf
        .drain(..)
        .map(|mut p| {
            p.hits.clear();
            p.edges.clear();
            p.vals.clear();
            std::sync::Mutex::new(p)
        })
        .collect();
    let next = AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);
    let timed = gc_flags().dbg_gcpause.then(std::time::Instant::now);
    old_gen
        .with_card_walk_view(dirty_ranges, |view| {
            if let Some(t0) = timed {
                moving_phase_count_push("cs_prep_us", t0.elapsed().as_micros());
            }
            let worker = || loop {
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                let g = next.fetch_add(1, Ordering::Relaxed);
                if g >= groups {
                    return;
                }
                let mut guard = parts[g].lock().unwrap_or_else(|e| e.into_inner());
                let part = &mut *guard;
                let (g0, g1) = (n * g / groups, n * (g + 1) / groups);
                if view
                    .walk_group(dirty_ranges, g0, g1, overlaps, &mut part.hits)
                    .is_err()
                {
                    failed.store(true, Ordering::Relaxed);
                    return;
                }
                for &hit in &part.hits {
                    // SAFETY: a hit of the dirty-card walk over this
                    // pause's ranges (`walk_group` reports exactly the
                    // serial walk's hits for its group).
                    unsafe {
                        hit_scan.scan(hit, &mut part.edges, with_values.then_some(&mut part.vals))
                    };
                }
            };
            run_chunk_workers_elastic(pool, workers.min(groups), &worker);
            if let Some(t0) = timed {
                moving_phase_count_push("cs_walk_done_us", t0.elapsed().as_micros());
            }
        })?;
    // The buffers go back whatever the outcome; only the first `groups` are
    // this scan's output.
    parts_buf.extend(parts.into_iter().map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner())));
    if failed.load(Ordering::Relaxed) {
        return None;
    }
    Some(groups)
}

/// Run a chunked young walk's `worker` on `threads` workers — the calling
/// thread plus `threads - 1` helpers from the heap's PERSISTENT pool.
///
/// # gen r4/sweep (2026-09-23): design proposal #1, carried out
///
/// `docs/feature-designs/concurrent-gc-maturation.md` item 1. Both
/// chunked walks ([`parallel_objstart_walk`], [`parallel_sweep_walk`]) used to
/// open a `std::thread::scope` and spawn `threads - 1` fresh OS threads on
/// EVERY pause — the exact shape `gen_evac::ParEvac::drain` measured as
/// helpers that never reached their first lock before the driver had finished,
/// and the reason `cratonvm_gc_core::evac_pool::EvacPool` exists. The walks are chunked at
/// an 8 MiB anchor stride, so on a small young generation they have a handful
/// of chunks and thread creation was a large fraction of the phase.
///
/// The contract is unchanged: `threads` counts the calling thread, the driver
/// runs one `worker()` itself, and nothing returns until every helper has
/// finished (`EvacPool::scope`'s barrier, which holds on the unwind path too
/// and re-raises a helper's own panic payload — the same guarantees
/// `std::thread::scope`'s join gave). `worker` is the same `Fn() + Sync`
/// closure the scoped version spawned by reference, so no new `unsafe` and no
/// new synchronisation is introduced here.
///
/// `ensure_helpers` first, so a pool built narrower (by an earlier
/// `ParEvac::drain` at its plan's headroom-scaled width, or under a per-thread
/// `CRATONVM_GC_PAR_THREADS` override in a test) cannot silently clamp the
/// walk below the width the scoped version ran at. It is a no-op once the pool
/// is wide enough, and a refused OS thread degrades to the narrower pool
/// rather than failing.
///
/// # Nesting
///
/// `EvacPool::scope` holds the pool's `dispatch` mutex for its whole extent,
/// so this must never run from INSIDE another dispatch on the same pool (it
/// would block rather than alias — see `evac_pool`'s module note). Both callers
/// run on the collecting thread outside any dispatch: the object-start walk
/// completes before the parallel copy phase's `ParEvac::drain` begins, and the
/// non-moving sweep never evacuates in parallel. Neither `worker` touches the
/// pool.
///
/// # Side effect worth knowing when reading `[GC] evac_pool:`
///
/// Whenever the object-start walk engages (a young generation spanning more
/// than one `CRATONVM_GC_SWEEP_ANCHOR_STRIDE`), it runs before the copy phase
/// of the same cycle, so it is usually the FIRST user of the lazily-built
/// pool: the pool is then created at the young worker POLICY width
/// (`young_gc_threads`) rather than at the first cycle's `ParEvac::plan`
/// width, and `ParEvac::drain`'s `min(plan - 1, pool.helpers())` clamp stops
/// binding below the plan. `pool_dispatches` / `pool_inline_runs` now count
/// walk dispatches as well as evacuations.
pub fn run_chunk_workers<W: Fn() + Sync>(
    pool: &cratonvm_gc_core::evac_pool::EvacPool,
    threads: usize,
    worker: &W,
) {
    if threads <= 1 {
        worker();
        return;
    }
    let helpers = threads - 1;
    pool.ensure_helpers(helpers);
    let body = |_helper: usize| worker();
    // `&W` is itself `Fn()`, hence a valid `FnOnce()` driver.
    pool.scope(helpers, &body, worker);
}

/// gcj pcp: [`run_chunk_workers`] on an ELASTIC dispatch
/// (`EvacPool::scope_elastic`): a helper that has not started when the
/// driver's own `worker()` returns is not waited for. Only for a `worker` that
/// CLAIMS its work (an atomic cursor over chunks) until none is left, so the
/// driver alone finishes whatever no helper took -- every caller on the
/// moving young pause's path is such a loop.
pub fn run_chunk_workers_elastic<W: Fn() + Sync>(
    pool: &cratonvm_gc_core::evac_pool::EvacPool,
    threads: usize,
    worker: &W,
) {
    if threads <= 1 {
        worker();
        return;
    }
    let helpers = threads - 1;
    pool.ensure_helpers(helpers);
    let body = |_helper: usize| worker();
    pool.scope_elastic(helpers, &body, worker);
}

/// Visit every aligned value in `[base+lo, base+hi)` that a **conservative**
/// scan of an unparseable young-from stretch must treat as a possible
/// reference, at every width a reference can occupy.
///
/// `visit(slot_addr, width, value)`:
///   * `width == 8` — the raw 64-bit word, the historical behaviour.
///   * `width == 4` — an aligned 32-bit half DECODED as a narrow oop. Only
///     produced when compressed oops are on, and never for a zero (null) slot.
///
/// `narrow` is `Some((base, shift))` — the geometry to decode halves under —
/// or `None` for wide-only. It is passed EXPLICITLY rather than read from
/// `narrow_oop`'s process-globals so that a test can exercise the narrow arm
/// without publishing a geometry every other test in the binary would then
/// read its own heap under (see `narrow_oop::disable_for_test`).
///
/// Both widths are visited rather than one or the other. Only reference FIELDS
/// and reference ARRAY ELEMENTS are narrowed, so an unparseable stretch can
/// still hold full-width pointers (legacy 16-byte `Value` cells, internal
/// pointers) alongside narrow ones. Over-visiting is safe here by construction:
/// every caller filters the value against a set of known object bases before
/// acting on it.
///
/// The mark walk (`mark_young_to_old_refs`) and the rewrite walk
/// (`fixup_young_old_refs`) MUST agree about width — a slot marked at one width
/// and rewritten at another leaves a dangling reference — so they share this.
/// See gc/src/compressed_oops.rs, hole 2: before this existed both scanned
/// 8-byte words only, so with narrow oops on, a pair of adjacent 4-byte
/// references never matched anything, marks were missed (premature
/// reclamation) and refs to moved objects were left unrewritten (dangling).
///
/// # Safety
/// `[base+lo, base+hi)` must be mapped, readable memory.
pub unsafe fn for_each_conservative_ref_slot(
    base: usize,
    lo: usize,
    hi: usize,
    narrow: Option<(u64, usize)>,
    mut visit: impl FnMut(usize, usize, usize),
) {
    let mut w = lo & !7;
    while w + 8 <= hi {
        let addr = base + w;
        // SAFETY: caller guarantees `[base+lo, base+hi)` is mapped.
        let word = unsafe { *(addr as *const u64) };
        visit(addr, 8, word as usize);
        // gen r5w2/roots6: a visitor that REWROTE the 8-byte word (the
        // conservative rewrite walks store a relocated address over a word
        // equal to a moved base) has decided what those 8 bytes are. Its two
        // halves are then the halves of the NEW address, not narrow slots:
        // decoding them could match another moved base and store a 4-byte
        // re-encoding into half of the pointer just written, turning it into
        // a wild 64-bit value. Unchanged word (every read-only visitor, and
        // every rewrite visitor that found no match): both halves visited
        // exactly as before.
        // SAFETY: the same mapped, aligned word, re-read after the visit.
        if narrow.is_some() && unsafe { *(addr as *const u64) } != word {
            w += 8;
            continue;
        }
        if let Some((nbase, nshift)) = narrow {
            for half in 0..2 {
                let slot = addr + half * 4;
                // SAFETY: within the same mapped 8 bytes.
                let encoded = unsafe { *(slot as *const u32) };
                if encoded != 0 {
                    visit(
                        slot,
                        4,
                        cratonvm_types::narrow_oop::decode_with(nbase, nshift, encoded) as usize,
                    );
                }
            }
        }
        w += 8;
    }
}

/// The published narrow-oop geometry, or `None` when compression is off.
/// The one place the conservative walks read the globals; everything below
/// takes the geometry as an argument.
#[inline]
pub fn active_narrow_geometry() -> Option<(u64, usize)> {
    use cratonvm_types::narrow_oop;
    narrow_oop::narrow_oops_enabled()
        .then(|| (narrow_oop::narrow_base(), narrow_oop::narrow_shift()))
}

pub use cratonvm_gc_core::heap::gen_object_total_size;

/// Compute a pointer to the slot at `index` within an object/array.
#[inline]
pub fn slot_ptr(obj_ref: ObjectRef, index: usize) -> *mut u8 {
    // SAFETY: Caller guarantees `index` is within the object's slot count; pointer arithmetic stays within the allocation.
    unsafe { obj_ref.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE) }
}

/// Plan an object allocation under the compact reference-field layout.
///
/// Returns `(total_size, array_length, gc_flags)`:
/// - When the flag is on, a layout is registered for `class_id`, and its field
///   count matches `num_fields`, the object uses the **compact** layout:
///   `total_size = HEADER_SIZE + body_size`, `array_length = body_size` (the
///   object's body bytes, since arrays-vs-objects is disambiguated by `kind`),
///   and `gc_flags = GC_FLAG_COMPACT`.
/// - Otherwise the legacy uniform layout: `total_size = HEADER_SIZE +
///   num_fields*SLOT_SIZE`, `array_length = 0`, `gc_flags = 0`.
///
/// `None` only on size overflow.
#[inline]
pub fn plan_object_alloc(
    layout_domain: u32,
    class_id: ClassId,
    num_fields: usize,
) -> Option<(usize, u32, u8)> {
    if compact_ref_fields_enabled() {
        if let Some(total) =
            cratonvm_types::compact_object_total_size(layout_domain, class_id.as_u32(), num_fields)
        {
            return Some((total, 0, GC_FLAG_COMPACT));
        } else if gc_flags().dbg_compact_legacy {
            let name = cratonvm_gc_core::gc::resolve_class_info(class_id.as_u32())
                .map(|(n, _)| n)
                .unwrap_or_else(|| "<unresolved>".to_string());
            eprintln!(
                "[compact-legacy] class={} id={} alloc num_fields={} has no matching current compact layout -> LEGACY object",
                name,
                class_id.as_u32(),
                num_fields,
            );
        }
    }
    let body = num_fields.checked_mul(SLOT_SIZE)?;
    Some((HEADER_SIZE.checked_add(body)?, 0, 0))
}

/// Resolve a field access on a (possibly compact) object to `(byte_offset,
/// is_ref)` within the object body. Returns `None` for a legacy object (caller
/// uses the uniform `index * SLOT_SIZE` 16-byte cell). Keys on the per-object
/// `GC_FLAG_COMPACT` bit, so legacy and compact objects coexist correctly.
#[inline]
pub fn compact_field_slot(
    header: &ObjectHeader,
    index: usize,
) -> Option<(usize, FieldStorageKind)> {
    if !is_compact_object(header) {
        return None;
    }
    cratonvm_types::compact_field_disp_storage(header.class_id.as_u32(), index)
}

/// Visit every reference slot of an object/array (read-only), invoking
/// `f(referent_ptr, slot_id)` for each non-null reference.
///
/// `slot_id` is the slot's GC identifier, reused by the dirty-card scan/fixup
/// pair: an **element index** for reference arrays, the **byte offset** for a
/// compact object's 8-byte reference slot, or the **field index** for a legacy
/// object's 16-byte cell. This three-way split mirrors the layout the heap
/// writes, so flag-off runs only the array + legacy arms (identical to dev).
///
/// # Safety
/// `obj_ptr` must point to a valid, fully-initialized object/array header whose
/// body is in-bounds for the slot ranges implied by `header`.
#[inline]
// SAFETY: callers uphold the `# Safety` contract above -- `obj_ptr` points at a valid,
// fully-initialized header whose body is in-bounds for the slot ranges implied by `header`.
pub unsafe fn for_each_ref_slot(
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    mut f: impl FnMut(*mut u8, usize),
) {
    if header.kind() == ObjectKind::Array {
        if header.element_type() == ArrayElementType::Reference {
            // HIB-DCAST-LATEPHASE.1: cap by the same `1 << 24` plausibility
            // bound `old_gen_mark_candidate_plausible`/`gen_object_total_size`
            // already apply to a header's own `array_length`/`num_slots` —
            // "no real array/object has this many elements/fields." This
            // function has ~25 call sites across gen_heap.rs/old_gen.rs/
            // vm_heap.rs, several without a walked size handy to cap against
            // precisely (see `OldGen::for_each_old_gen_ref`'s doc comment for
            // that more precise fix, applied where a caller has one); this
            // coarser cap is the one every caller gets for free without
            // threading one through. It does not guarantee correctness for a
            // header whose `array_length` is wrong but still under the cap —
            // only that this function cannot itself stride tens of millions
            // of slots into unmapped memory.
            let elems = (header.array_length() as usize).min(1 << 24);
            for i in 0..elems {
                let s = obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size());
                let raw: u64 = read_ref_slot(s);
                if raw != 0 {
                    f(raw as usize as *mut u8, i);
                }
            }
        }
    } else if is_compact_object(header) {
        // HIB-DCAST-LATEPHASE.1: `compact_oop_scan` returns `None` for TWO
        // different reasons — "this is a legacy object" (its own documented
        // contract) and, via its `class_layout_for_fields(..)?` early return,
        // "this IS a compact object but its class's layout is not registered
        // for this (class_id, field_count) right now" (e.g. a redefinition
        // raced the registry). Testing `is_compact_object(header)` directly
        // — the per-object `GC_FLAG_COMPACT` header bit set at allocation,
        // independent of the registry — distinguishes the two. Treating the
        // second case as "fall back to legacy 16-byte-cell scanning" (the old
        // behaviour) reads this object's *compact*, packed, possibly much
        // smaller body under the *legacy* `num_slots * SLOT_SIZE` formula —
        // `num_slots()` returns the compact field count here, not a legacy
        // slot count, so the loop below strides past the object's real
        // extent (validated only for the layout-less HEADER_SIZE-only size
        // `object_body_size` reports) into unmapped memory: a `SIGSEGV`
        // reached this way against the real `DefaultCatalogAndSchemaTest`
        // workload. A compact object whose layout cannot be resolved has no
        // provably-safe reference slots to visit; skip it exactly as an
        // implausible/corrupt object is skipped elsewhere, rather than
        // guessing.
        //
        // gc-common w3-e: borrowing form -- no layout `Arc` clone/drop per
        // scanned object (see `cratonvm_gc_core::heap::with_compact_oop_scan`).
        let _ = cratonvm_gc_core::heap::with_compact_oop_scan(header, |layout, body| {
            for &off in &layout.ref_disps {
                let off = off as usize;
                if off + ref_field_size() > body {
                    break;
                }
                let s = obj_ptr.add(off);
                let raw: u64 = read_ref_slot(s);
                if raw != 0 {
                    f(raw as usize as *mut u8, off);
                }
            }
        });
    } else {
        // HIB-DCAST-LATEPHASE.1: same cap as the array arm above.
        let slots = (header.num_slots() as usize).min(1 << 24);
        for slot_idx in 0..slots {
            let s = obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE);
            if let Value::Object(Some(r)) = std::ptr::read(s as *const Value) {
                f(r.as_ptr(), slot_idx);
            }
        }
    }
}

/// Seed the copy phase from the precise root set, rewriting every root in
/// place.
///
/// `forward` returns the address every reference to its argument must now use:
/// [`GenerationalHeap::forward_object`] for the serial copy phase,
/// [`crate::gen_evac::ParEvac::evacuate`] for the parallel one. The seeding is
/// factored out rather than written twice because WHAT gets seeded is the half
/// no evacuator's own tests would catch — an overlay edge or a card slot the
/// parallel path forgot would show up as a live object silently reclaimed, a
/// week later, on the other collector.
pub fn seed_roots(
    roots: &mut [ObjectRef],
    young_from: &Arena,
    forward: &mut dyn FnMut(*mut u8) -> *mut u8,
) {
    for root in roots.iter_mut() {
        let old_ptr = root.as_ptr();
        if !young_from.contains(old_ptr) {
            continue; // Skip roots not in young gen (e.g., old gen objects)
        }
        let new_ptr = forward(old_ptr);
        // SAFETY: `new_ptr` came from the evacuator, which either allocated
        // space in young_to / old_gen and copied a valid object there, or
        // refused the address and handed back the original.
        *root = unsafe { ObjectRef::from_raw(new_ptr) };
    }
}

/// Seed the overlay-backed collections' Rust-side edges.
///
/// LinkedList / LinkedHashMap / TreeMap / TreeSet keep their backing arrays in
/// process-global side-tables, not in Java heap slots, so neither a root slot
/// nor a dirty card can describe the edge. `roots` carries them ONLY when the
/// VM's unconditional overlay scan ran (`native_roots::scan_collection_overlays`)
/// — and the Generational collector deliberately SKIPS that scan while JIT
/// quiescence is engaged, an unregistered JIT frame is on the stack, or a major
/// GC is pending, relying instead on the marker walking each owner and pulling
/// in `external_roots_for_owner`.
///
/// `sweep_young_non_moving` (and `old_gen_gc`) implement that owner walk. The
/// moving Cheney path never did: when the two conditions met — overlay scan
/// skipped, moving young chosen — every overlay-held young array was silently
/// reclaimed. The collection then read its own state through the
/// relocation-invariant identity-hash key and got a dangling pointer whose
/// zeroed header reads back as `ClassId(0)` with `array_length == 0`, surfacing
/// far away as an `Int(0)` where an element belongs (`TreeSet.contains` →
/// `ts_binary_search` → `Comparator.compare` → `checkcast: not an object
/// reference`, or a SIGSEGV).
///
/// Seed every current owner's refs, exactly as the non-moving young path does —
/// a minor collection leaves old gen intact and cannot decide which owners will
/// later prove dead. Only forwarding is needed here: the side tables themselves
/// are repointed afterwards by `remap_external_roots` from `pointer_map`.
pub fn seed_overlay_roots(young_from: &Arena, forward: &mut dyn FnMut(*mut u8) -> *mut u8) {
    for overlay_ref in cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|_| true) {
        let old_ptr = overlay_ref.as_ptr();
        if !young_from.contains(old_ptr) {
            continue;
        }
        let _ = forward(old_ptr);
    }
}

/// gci i8/pa — forward the caller's young side-table values (gci i9/yl: the
/// old owners' rows, [`side_table_values_due`]'s Phase 1 answer) as
/// young-phase roots. Nothing is written back: the registries are remapped by
/// the VM from `pointer_map` once the collector returns, and the copies are
/// deliberately not appended to the root slice, so the Phase 5 true-root
/// major does not see them as roots.
pub fn seed_side_table_young_values(values: &[usize], forward: &mut dyn FnMut(*mut u8) -> *mut u8) {
    for &v in values {
        let _ = forward(v as *mut u8);
    }
}

/// gci i9/yl (residual 3c of `i8-pa-report.md`; P-A item 1-2 of the e1/y
/// design on `gengc-core-system-gc-forces-non-moving-20260920`) — the
/// `mirror_pin` and `metadata_pin` rows a licensed moving `System.gc()`
/// follows, as `(owner, young values)`: each row keeps only its values that
/// `is_young` places in this heap's young from-space (non-null, 8-aligned,
/// ascending, deduplicated), and a row left with none is dropped, so a row of
/// another VM (whose values never lie in this from-space) never appears. An
/// owner present in both registries yields two rows; following either is
/// independent. Owners are the registries' keys: PRE-collection addresses
/// (the VM remaps the registries only after the collector returns).
pub fn side_table_young_rows_of(
    mirror_rows: Option<&FxHashMap<usize, Vec<usize>>>,
    metadata_rows: Option<&FxHashMap<usize, Vec<usize>>>,
    is_young: impl Fn(usize) -> bool,
) -> Vec<(usize, Vec<usize>)> {
    let mut out: Vec<(usize, Vec<usize>)> = Vec::new();
    for (&owner, values) in mirror_rows
        .into_iter()
        .flat_map(|m| m.iter())
        .chain(metadata_rows.into_iter().flat_map(|m| m.iter()))
    {
        let mut young: Vec<usize> = values
            .iter()
            .copied()
            .filter(|&v| v != 0 && v % 8 == 0 && is_young(v))
            .collect();
        if young.is_empty() {
            continue;
        }
        young.sort_unstable();
        young.dedup();
        out.push((owner, young));
    }
    // Deterministic order (the registries are hash maps): by owner.
    out.sort_by_key(|&(owner, _)| owner);
    out
}

/// gci i9/yl — the young values the copy must forward NOW: those of every row
/// not yet followed (`done[i]` false) whose owner is live for this young
/// phase, i.e. outside young from-space (`!is_young(owner)`: old, treated as
/// live by a minor collection) or already forwarded (`forwarded(owner)`: a
/// copy, a promotion, or a pinned identity entry). Each such row is marked
/// done, and only its values not yet forwarded are returned (ascending,
/// deduplicated). A row whose owner is young and unforwarded stays pending: a
/// later call follows it if the closure reaches the owner, and if nothing
/// ever does, the owner and the values only it kept die together.
///
/// Called with `forwarded = |_| false` for the Phase 1 seed (old owners
/// only), then to a fix-point after the closure and inside Phase 2.5b.
pub fn side_table_values_due(
    rows: &[(usize, Vec<usize>)],
    done: &mut [bool],
    is_young: impl Fn(usize) -> bool,
    forwarded: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for (i, (owner, values)) in rows.iter().enumerate() {
        if done.get(i).copied().unwrap_or(true) {
            continue;
        }
        if is_young(*owner) && !forwarded(*owner) {
            continue;
        }
        done[i] = true;
        out.extend(values.iter().copied().filter(|&v| !forwarded(v)));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// gen r4w4/cards4 — is the card a barrier dirties for the old→young edge in
/// slot `slot_idx` of the old object at `holder` dirty right now? Returns
/// `(dirty, card_index_to_report)`.
///
/// The header card always counts: a header mark means "scan the whole object"
/// (`deferred_dirty_cards`, JIT header marks, `CRATONVM_GC_PRECISE_ARRAY_CARDS=0`).
/// When the table carries element-precise marks and the holder is a reference
/// array, the ELEMENT's card counts too — that is the card
/// `write_barrier_ref_array_element` and the JIT's precise `aastore` dirty, and
/// the one `scan_dirty_cards_inner`'s window arm reads. `slot_idx` is what
/// `for_each_ref_slot` yields for an array: the element index. The reported
/// card is the element's when the header card was clean.
///
/// Diagnostic callers only (`CRATONVM_DBG_RSET_AUDIT`, the stale-reference
/// holder report); `CRATONVM_GC_VERIFY_RSET` compares seeded pairs instead.
pub fn rset_edge_card_is_dirty(
    card_table: &CardTable,
    holder: usize,
    hdr: &ObjectHeader,
    slot_idx: usize,
) -> (bool, usize) {
    let cbase = card_table.base_addr();
    let header_card = holder.wrapping_sub(cbase) / cratonvm_gc_core::card_table::CARD_SIZE;
    if card_table.is_dirty(header_card) {
        return (true, header_card);
    }
    if card_table.precise_ref_array_marks()
        && hdr.kind() == ObjectKind::Array
        && hdr.element_type() == ArrayElementType::Reference
    {
        let slot = slot_idx
            .checked_mul(ref_element_size())
            .and_then(|o| o.checked_add(ARRAY_DATA_OFFSET))
            .and_then(|o| holder.checked_add(o));
        if let Some(slot) = slot {
            let element_card = slot.wrapping_sub(cbase) / cratonvm_gc_core::card_table::CARD_SIZE;
            return (card_table.is_dirty(element_card), element_card);
        }
    }
    (false, header_card)
}

/// Seed old→young references named by this cycle's dirty cards, rewriting each
/// slot through its own encoding (ref arrays and compact objects store bare
/// 8-byte pointers; legacy objects store 16-byte `Value` cells).
///
/// A referent that STAYS young after forwarding leaves a persistent old→young
/// edge, so the holder's card is pushed onto `deferred_dirty_cards` to be
/// re-marked after Phase 3's `clear_all()` — a direct mark would be wiped, and
/// the edge would then be remembered for exactly one cycle (the promoting one).
/// The object-field case once missed this (BouncyCastle
/// `X9ECParametersHolder.params` -> young `X9ECParameters` lost its
/// remembered-set entry), which is why all three branches carry it.
///
/// `old_extent` is [`OldGen::extent`] — the same range test `OldGen::contains`
/// performs, as two plain words so the parallel evacuator does not have to hold
/// the old-generation lock to ask.
///
/// `deferred_dirty_cards` takes `(holder, offset)` pairs (gen r5w2/roots6,
/// cards3 item 2): with `precise_array_cards` (the table's
/// `precise_ref_array_marks`) a reference-array slot defers its OWN element's
/// card (`offset = ARRAY_DATA_OFFSET + slot_idx * ref_element_size()`), which
/// `scan_dirty_cards_inner` reads alone when the array's header card is clean;
/// every other slot, and every slot on a header-grained table, defers the
/// header card (`offset = 0`).
///
/// `in_young` is young from-space's CAPACITY test (`Arena::contains` on the
/// serial arm, `ParEvac::in_from_extent` on the parallel one): a slot is
/// forwarded when it names an address inside it. gen r5w3/evac7: a predicate
/// rather than `&Arena` so the parallel card seed (`CRATONVM_GC_PAR_EVAC_CARD_SEED`)
/// can run this on its workers without sharing the arena guard.
///
/// # Safety
/// Every `(old_obj, slot_idx)` must name an in-bounds reference slot of a live
/// old-generation object, which is what the dirty-card scan produces.
#[allow(clippy::too_many_arguments)]
pub unsafe fn seed_dirty_card_roots(
    extra_roots: &[(ObjectRef, usize, usize)],
    in_young: &dyn Fn(*const u8) -> bool,
    old_extent: (usize, usize),
    precise_array_cards: bool,
    deferred_dirty_cards: &mut Vec<(usize, usize)>,
    forward: &mut dyn FnMut(*mut u8) -> *mut u8,
) {
    let (old_lo, old_hi) = old_extent;
    let in_old = |p: *mut u8| {
        let a = p as usize;
        a >= old_lo && a < old_hi
    };
    // gcj j1yg-pause: the card scan reports a holder's edges consecutively,
    // so a pair equal to the last one pushed is the same re-mark; skipping it
    // keeps the deferred list at about one pair per holder (and per element
    // card) instead of one per young-staying edge (~700 K a JIT-warm javac
    // pause). The re-marks themselves are unchanged.
    let mut defer = |pair: (usize, usize)| {
        if deferred_dirty_cards.last() != Some(&pair) {
            deferred_dirty_cards.push(pair);
        }
    };
    // gce e2/o: the opt-in referent prefetch (`CRATONVM_GEN_EVAC_SCAN_PREFETCH`,
    // gen r5w3/evac7) is removed -- no gain measured in two rounds.
    for &(old_obj, slot_idx, _) in extra_roots.iter() {
        // SAFETY: `old_obj` is a live old-gen ObjectRef collected from dirty card
        // scanning, so its pointer targets a valid ObjectHeader.
        let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
        let is_ref_array = header.kind() == ObjectKind::Array
            && header.element_type() == ArrayElementType::Reference;

        if is_ref_array {
            let element_offset = ARRAY_DATA_OFFSET + slot_idx * ref_element_size();
            // SAFETY: `old_obj` is a valid old-gen object and `slot_idx` was collected
            // from dirty card scanning (within array bounds). Pointer arithmetic stays
            // within the object's allocation.
            let slot_ptr = unsafe { old_obj.as_ptr().add(element_offset) };
            // SAFETY: `slot_ptr` points to a valid 8-byte ref element within the array.
            let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
            if raw != 0 {
                let ref_ptr = raw as usize as *mut u8;
                if in_young(ref_ptr) {
                    let new_ptr = forward(ref_ptr);
                    // SAFETY: Writing the forwarded pointer back to the same valid slot.
                    unsafe { write_ref_slot(slot_ptr, new_ptr as u64) };
                    if !in_old(new_ptr) {
                        // gen r5w2/roots6: the element's own card on an
                        // element-precise table, the header's otherwise.
                        let offset = if precise_array_cards {
                            element_offset
                        } else {
                            0
                        };
                        defer((old_obj.as_ptr() as usize, offset));
                    }
                }
            }
        } else if is_compact_object(header) {
            // Compact object: `slot_idx` is the BYTE OFFSET of an 8-byte
            // reference slot (recorded that way by the card scan). Mirror
            // the ref-array branch.
            // SAFETY: `slot_idx` (byte offset) was recorded by dirty-card
            // scanning within this object's body; the 8-byte read is in-bounds.
            let slot_ptr = unsafe { old_obj.as_ptr().add(slot_idx) };
            // SAFETY: as above — an in-bounds 8-byte reference slot.
            let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
            if raw != 0 {
                let ref_ptr = raw as usize as *mut u8;
                if in_young(ref_ptr) {
                    let new_ptr = forward(ref_ptr);
                    // SAFETY: writing the forwarded pointer back to the slot.
                    unsafe { write_ref_slot(slot_ptr, new_ptr as u64) };
                    if !in_old(new_ptr) {
                        defer((old_obj.as_ptr() as usize, 0));
                    }
                }
            }
        } else {
            // SAFETY: `old_obj` is a valid old-gen object, `slot_idx` is within
            // `num_slots` (from dirty card scanning). Arithmetic stays in bounds.
            let slot_ptr = unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
            // SAFETY: `slot_ptr` points to a valid `Value`-sized slot in the object.
            let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
            if let Value::Object(Some(ref_obj)) = value {
                let ref_ptr = ref_obj.as_ptr();
                if in_young(ref_ptr) {
                    let new_ptr = forward(ref_ptr);
                    // SAFETY: `new_ptr` is a valid forwarded allocation.
                    let new_value = Value::Object(Some(unsafe { ObjectRef::from_raw(new_ptr) }));
                    // SAFETY: Writing updated Value back to the same valid slot.
                    unsafe {
                        cratonvm_types::write_value_atomic(slot_ptr as *mut Value, new_value)
                    };
                    if !in_old(new_ptr) {
                        defer((old_obj.as_ptr() as usize, 0));
                    }
                }
            }
        }
    }
}

/// The deferred card re-mark(s) one OLD-generation holder leaves when a slot
/// of it keeps a referent that stays young after this cycle — the scan of a
/// promoted object (serial Cheney loop, Phase 2.5b, `redrain_serial_closure`)
/// and `gen_evac::ParEvac::scan_object` on the parallel arm.
///
/// gcd d2/h (2026-09-27), `gengc-r4w3-cards3-precise-array-cards-residuals`
/// item 3, producer (c): these producers pushed `(holder, 0)` — the HEADER
/// card, "scan the whole object" — once per holder. For a promoted wide
/// reference array with one young element that is a whole-array rescan at the
/// next young pause. On an element-precise table (`element_card_base` is
/// `Some`, the table's `precise_ref_array_marks()`) a reference-array holder
/// now defers the card of each young-staying ELEMENT instead, exactly the pair
/// `seed_dirty_card_roots` already pushes for a card-seeded element (gen
/// r5w2/roots6): `(holder, ARRAY_DATA_OFFSET + i * ref_element_size())`, which
/// the compaction arm remaps holder-first. Consecutive elements on one card
/// push once (the scan visits elements in ascending order, so a card's
/// elements are adjacent), so an array of N young referents still pushes at
/// most one pair per card it spans. Every other holder, every slot-less edge
/// (the instance→loader pin, [`HolderCardDefer::whole_object`]) and every holder on a
/// header-grained table keeps the one `(holder, 0)` push.
///
/// The consumer needs nothing new: `scan_dirty_cards_inner` already reads only
/// the elements inside a dirty element card of an array whose header card is
/// clean, and a young element among the first ~60 lands on the header card,
/// which still means "the whole object" — correct, as before.
pub struct HolderCardDefer {
    pub holder: usize,
    /// The card table's base when `holder` is a reference array on an
    /// element-precise table; `None` otherwise (header-grained).
    pub element_card_base: Option<usize>,
    /// The card index of the last element pair pushed (`usize::MAX` = none).
    pub last_card: usize,
    /// A header mark is owed.
    pub whole: bool,
}

impl HolderCardDefer {
    /// `element_card_base` is `Some(card_table.base_addr())` when the table is
    /// element-precise, `None` otherwise; it is dropped here unless `header`
    /// is a reference array's.
    #[inline]
    pub fn new(
        holder: usize,
        header: &ObjectHeader,
        element_card_base: Option<usize>,
    ) -> Self {
        let ref_array = header.kind() == ObjectKind::Array
            && header.element_type() == ArrayElementType::Reference;
        Self {
            holder,
            element_card_base: element_card_base.filter(|_| ref_array),
            last_card: usize::MAX,
            whole: false,
        }
    }

    /// The slot at byte offset `off` of the holder keeps a referent that
    /// stays young after this cycle.
    #[inline]
    pub fn slot_stays_young(&mut self, off: usize, out: &mut Vec<(usize, usize)>) {
        match self.element_card_base {
            Some(base) => {
                let card =
                    self.holder.wrapping_add(off).wrapping_sub(base) / cratonvm_gc_core::card_table::CARD_SIZE;
                if card != self.last_card {
                    self.last_card = card;
                    out.push((self.holder, off));
                }
            }
            None => self.whole = true,
        }
    }

    /// An edge with no slot of its own stays young (the instance→loader pin):
    /// the header card, "scan the whole object".
    #[inline]
    pub fn whole_object(&mut self) {
        self.whole = true;
    }

    /// Push the header pair if anything owed one.
    #[inline]
    pub fn finish(self, out: &mut Vec<(usize, usize)>) {
        if self.whole {
            out.push((self.holder, 0));
        }
    }
}

/// gcd d5/s (`gengc-r4w3-cards3-precise-array-cards-residuals` item 3,
/// producer (g)): the card marks owed by one holder the non-moving sweep's
/// selective promotion just evacuated to old gen and that still names a young
/// object (`fixup_object_fields`' `points_young`), appended to `out` as the
/// addresses `CardTable::mark_dirty_bulk` takes.
///
/// A reference array on an element-precise table (`element_card_base` is
/// `Some`) marks the card of each young-staying ELEMENT, once per card
/// ([`HolderCardDefer`]); every other holder, and every holder on a
/// header-grained table, marks its header card ("scan the whole object"), as
/// the sweep always did. A holder the caller saw pointing young but whose
/// reference slots name nothing `in_young` accepts (an edge with no slot of
/// its own) marks its header too. `scratch` is a reusable buffer, cleared
/// here.
pub fn push_promoted_holder_cards(
    dst: *mut u8,
    header: &ObjectHeader,
    element_card_base: Option<usize>,
    in_young: &dyn Fn(usize) -> bool,
    scratch: &mut Vec<(usize, usize)>,
    out: &mut Vec<usize>,
) {
    let holder = dst as usize;
    let mut defer = HolderCardDefer::new(holder, header, element_card_base);
    scratch.clear();
    // SAFETY: `dst` is a live old-generation object whose reference slots the
    // caller has just fixed up, read during the pause (no mutator runs); the
    // closure only reads, and returns `None`, so no slot is rewritten.
    unsafe {
        forward_ref_slots_at(dst, header, |off, r| {
            if in_young(r as usize) {
                defer.slot_stays_young(off, scratch);
            }
            None
        });
    }
    defer.finish(scratch);
    if scratch.is_empty() {
        out.push(holder);
    }
    out.extend(scratch.drain(..).map(|(h, o)| h + o));
}

/// gce e1/y: how much of the evacuated semi-space a moving young cycle keeps
/// committed for the NEXT cycle's copy (`uncommit_evacuated_young_keeping`):
/// the bytes this cycle's survivors occupy in young (`survivor_bytes`, the
/// new from-space's cursor) plus a quarter, when the default on
/// `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS` is on; 0 (the historical give-back)
/// otherwise. Pure.
pub fn uncommit_keep_for_survivors(survivor_bytes: usize, on: bool) -> usize {
    if on {
        survivor_bytes.saturating_add(survivor_bytes / 4)
    } else {
        0
    }
}

/// gcd d5/s (cards3 item 3, the consumer): the byte windows, relative to the
/// object's start, of every range of `dirty_ranges` (sorted, disjoint
/// `[lo, hi)` offsets into old gen, as `scan_dirty_cards_inner` coalesces
/// them) that overlaps the object `[obj_off, obj_off + size)`. A range that
/// begins at or below the object's start yields a window starting at 0. Pure.
pub fn array_dirty_windows(
    dirty_ranges: &[(usize, usize)],
    obj_off: usize,
    size: usize,
) -> Vec<(usize, usize)> {
    let end = obj_off.saturating_add(size);
    let first = dirty_ranges.partition_point(|&(_, hi)| hi <= obj_off);
    dirty_ranges[first..]
        .iter()
        .take_while(|&&(lo, _)| lo < end)
        .map(|&(lo, hi)| (lo.max(obj_off) - obj_off, hi.min(end) - obj_off))
        .collect()
}

/// The elements `[first, last)` of a reference array (`elems` of them, `esz`
/// bytes each) whose slot STARTS inside the byte window `[lo, hi)` measured
/// from the array's header -- exactly the slots whose card a precise element
/// mark dirties. The rule `scan_dirty_cards_inner`'s window arm has used since
/// gen r4w3/cards3.
#[inline]
pub fn window_element_span(lo: usize, hi: usize, elems: usize, esz: usize) -> (usize, usize) {
    (
        lo.saturating_sub(ARRAY_DATA_OFFSET)
            .div_ceil(esz)
            .min(elems),
        hi.saturating_sub(ARRAY_DATA_OFFSET)
            .div_ceil(esz)
            .min(elems),
    )
}

/// Forward/remap every reference slot of an object/array: for each non-null
/// referent, calls `forward(referent_ptr)`; on `Some(new_ptr)` the slot is
/// rewritten (8-byte raw for arrays/compact objects, 16-byte `Value::Object`
/// for legacy objects), on `None` the slot is left untouched. Handles all three
/// layouts; flag-off runs only the array + legacy arms (identical to dev, with
/// non-forwarded slots never written).
///
/// # Safety
/// Same contract as [`for_each_ref_slot`]. A returned `Some(ptr)` must be a
/// valid heap pointer.
#[inline]
// SAFETY: callers uphold the `# Safety` contract above (same as `for_each_ref_slot`);
// any returned `Some(ptr)` is a valid heap pointer.
pub unsafe fn forward_ref_slots(
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    mut forward: impl FnMut(*mut u8) -> Option<*mut u8>,
) {
    forward_ref_slots_at(obj_ptr, header, |_, p| forward(p));
}

/// [`forward_ref_slots`], with the slot's BYTE OFFSET from `obj_ptr` handed to
/// `forward` beside the referent: `ARRAY_DATA_OFFSET + i * ref_element_size()`
/// for reference-array element `i`, the oop-map displacement for a compact
/// instance, `HEADER_SIZE + slot_idx * SLOT_SIZE` for a legacy one. A
/// reference array's elements are visited in ascending index order.
///
/// gcd d2/h (2026-09-27): what lets a promoted reference array defer its
/// ELEMENT's card instead of its header's
/// (`gengc-r4w3-cards3-precise-array-cards-residuals`, item 3 producer (c);
/// [`HolderCardDefer`]). `forward_ref_slots` is this function with the offset
/// ignored, so every existing caller is unchanged.
///
/// # Safety
/// Same contract as [`forward_ref_slots`].
#[inline]
pub unsafe fn forward_ref_slots_at(
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    mut forward: impl FnMut(usize, *mut u8) -> Option<*mut u8>,
) {
    if header.kind() == ObjectKind::Array {
        if header.element_type() == ArrayElementType::Reference {
            // HIB-DCAST-LATEPHASE.1: see the matching cap in `for_each_ref_slot`.
            let elems = (header.array_length() as usize).min(1 << 24);
            for i in 0..elems {
                let off = ARRAY_DATA_OFFSET + i * ref_element_size();
                let s = obj_ptr.add(off);
                let raw: u64 = read_ref_slot(s);
                if raw != 0 {
                    if let Some(n) = forward(off, raw as usize as *mut u8) {
                        // gcstress face-1 hunt (no-op unless gated) — a RAW
                        // 8-byte pointer write; on a MISREAD header (walk
                        // overshoot family) this arm would spray pointers
                        // over legacy 16-byte cells.
                        cratonvm_gc_core::heap::cell_watch_check(
                            s as usize,
                            8,
                            "forward_ref_slots-refarray",
                            &(n as usize),
                        );
                        write_ref_slot(s, n as u64);
                    }
                }
            }
        }
    } else if is_compact_object(header) {
        // See the matching note in `for_each_ref_slot`: `compact_oop_scan`
        // returning `None` here means the layout for this genuinely-compact
        // object (`GC_FLAG_COMPACT` set) is not currently registered, NOT
        // that this is a legacy object. Falling through to the legacy arm
        // would forward/rewrite this object's packed compact body under the
        // wrong 16-byte-cell formula, striding past its real extent.
        //
        // gc-common w3-e: borrowing form. This is the copy phase, serial and
        // parallel: with the `Arc` form every evacuation worker scanning a run
        // of same-class objects bumped and dropped the SAME layout refcount,
        // a cache line that ping-ponged between all of them.
        let _ = cratonvm_gc_core::heap::with_compact_oop_scan(header, |layout, body| {
            for &off in &layout.ref_disps {
                let off = off as usize;
                if off + ref_field_size() > body {
                    break;
                }
                let s = obj_ptr.add(off);
                let raw: u64 = read_ref_slot(s);
                if raw != 0 {
                    if let Some(n) = forward(off, raw as usize as *mut u8) {
                        // gcstress face-1 hunt (no-op unless gated).
                        cratonvm_gc_core::heap::cell_watch_check(
                            s as usize,
                            8,
                            "forward_ref_slots-compact",
                            &(n as usize),
                        );
                        write_ref_slot(s, n as u64);
                    }
                }
            }
        });
    } else {
        // HIB-DCAST-LATEPHASE.1: see the matching cap in `for_each_ref_slot`.
        let slots = (header.num_slots() as usize).min(1 << 24);
        for slot_idx in 0..slots {
            let off = HEADER_SIZE + slot_idx * SLOT_SIZE;
            let s = obj_ptr.add(off);
            if let Value::Object(Some(r)) = std::ptr::read(s as *const Value) {
                if let Some(n) = forward(off, r.as_ptr()) {
                    // gcstress face-1 hunt (no-op unless gated).
                    cratonvm_gc_core::heap::cell_watch_check(
                        s as usize,
                        16,
                        "forward_ref_slots-legacy",
                        &(n as usize),
                    );
                    cratonvm_types::write_value_atomic(
                        s as *mut Value,
                        Value::Object(Some(ObjectRef::from_raw(n))),
                    );
                }
            }
        }
    }
}

/// DoHead walk-desync hardening (2026-07-02): robust free-block skip shared by
/// every linear young from-space walk.
///
/// Advances `free_iter` past blocks the cursor has wholly passed; if the
/// cursor sits AT or INSIDE a block, jumps the cursor to the block's end.
/// Returns `(resynced, overshot)`: `resynced` means the cursor moved (the
/// caller must `continue` its loop); `overshot` means the cursor was found
/// STRICTLY INSIDE a block (`cursor > off`) — proof that an earlier stride
/// was mis-sized and everything parsed since the previous anchor is suspect.
///
/// This generalizes the main sweep's robust skip: the old exact-match skip
/// (`cursor == off`) wedged its iterator forever after a single overshoot,
/// after which every later free block was walked as a run of zeroed 40-byte
/// phantom `Object`s (class_id=0, num_slots=0 → size 40) — the A2 / ReflRepro
/// / DoHead walk-desync corruption family.
#[inline]
pub fn skip_free_blocks(
    cursor: &mut usize,
    free_iter: &mut std::iter::Peekable<std::slice::Iter<'_, (usize, usize)>>,
) -> (bool, bool) {
    let mut resynced = false;
    let mut overshot = false;
    while let Some(&&(off, sz)) = free_iter.peek() {
        if *cursor >= off + sz {
            // Walk is already past this entire free block — drop it and
            // re-examine the next one.
            free_iter.next();
            continue;
        }
        if *cursor >= off {
            if *cursor > off {
                overshot = true;
                let n = SWEEP_WALK_OVERSHOOT_HITS.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    tracing::warn!(
                        "young walk: cursor {} overshot into free block [{}, {}) — \
                         resynced at block end (walk grid / free list disagree upstream)",
                        *cursor,
                        off,
                        off + sz,
                    );
                }
            }
            // Cursor is within `[off, off + sz)` — skip the remainder of this
            // free block and resync the walk to a real boundary.
            *cursor = off + sz;
            free_iter.next();
            resynced = true;
        }
        // `cursor < off`: the next free block is still ahead; stop.
        break;
    }
    (resynced, overshot)
}

/// The sequential sweep walk's ANCHOR-VALIDITY probe, as a type.
///
/// The parallel sweep prefix splits from-space at `sweep_anchors` and starts an
/// independent object chain at each one; its soundness rests entirely on those
/// offsets being real walk positions. The sequential walk visits every walk
/// position in ascending order, so an anchor it strides past without landing on
/// is an anchor that is NOT on the object grid — a chunk started there parses
/// phantom objects, and a phantom that reads as unmarked reclaims and zeroes
/// every live object it subsumes. That is what [`SWEEP_ANCHOR_NOT_A_BASE`]
/// counts, and its report calls the counter "exactly zero on a sound anchor
/// list".
///
/// # The two crossings that are NOT findings
///
/// The probe was inline, ran after `skip_free_blocks`, and counted every
/// unconsumed anchor below the cursor. Both of the walk's legitimate jumps
/// therefore registered as findings:
///
/// * **offset 0.** The anchor builder deliberately exempts it from the "drop
///   anchors that land inside a free block" filter — "it is the walk's start,
///   not a split point, and every walker already skip-resyncs from it". When
///   from-space opens with a free block, the first iteration resyncs the cursor
///   past it and `continue`s (skipping the probe), and the next iteration finds
///   anchor 0 below the cursor and counts it. This is the `off_grid=1
///   anchors=2` reading two passing classes produced in the 2026-09-08
///   BindableTests sweep: a two-entry list is `[0, used]`, and `used` cannot be
///   reached from inside a `cursor < used` loop, so the exempt anchor is the
///   only one it can have counted.
/// * **a free block the walk resyncs over.** Anchors inside one are filtered
///   out at construction against `exact_skips`, but the walk resyncs off
///   `existing_free`, and a parallel chunk beginning at such an offset resyncs
///   off exactly the same list — it parses no phantom, so it is not this
///   failure.
///
/// An anchor the walk passed by an OBJECT stride still counts: that is the
/// grid disagreement the probe exists for.
pub struct AnchorGridProbe<'a> {
    pub it: std::iter::Peekable<std::slice::Iter<'a, usize>>,
    pub off_grid: usize,
    /// Only meaningful when the sequential walk covers the whole arena; when
    /// the parallel prefix already consumed the front, anchors below the
    /// prefix end were proven by the prefix, not skipped.
    pub armed: bool,
}

impl<'a> AnchorGridProbe<'a> {
    pub fn new(anchors: &'a [usize], armed: bool) -> Self {
        let mut p = Self {
            it: anchors.iter().peekable(),
            off_grid: 0,
            armed,
        };
        if armed {
            // Consume the exempt walk start before the walk can jump over it.
            while p.it.peek().is_some_and(|&&a| a == 0) {
                p.it.next();
            }
        }
        p
    }

    /// The walk is standing on `cursor`, a position the object grid reaches.
    pub fn at_walk_position(&mut self, cursor: usize) {
        if !self.armed {
            return;
        }
        while self.it.peek().is_some_and(|&&a| a < cursor) {
            self.off_grid += 1;
            SWEEP_ANCHOR_NOT_A_BASE.fetch_add(1, Ordering::Relaxed);
            self.it.next();
        }
        if self.it.peek().is_some_and(|&&a| a == cursor) {
            self.it.next();
        }
    }

    /// The walk jumped from `from` to `to` over a KNOWN free block. Anchors in
    /// `[from, to]` are accounted for, not skipped; anything still below `from`
    /// was passed by an object stride and is a finding.
    pub fn crossed_free_block(&mut self, from: usize, to: usize) {
        if !self.armed {
            return;
        }
        while self.it.peek().is_some_and(|&&a| a < from) {
            self.off_grid += 1;
            SWEEP_ANCHOR_NOT_A_BASE.fetch_add(1, Ordering::Relaxed);
            self.it.next();
        }
        while self.it.peek().is_some_and(|&&a| a <= to) {
            self.it.next();
        }
    }

    pub fn off_grid(&self) -> usize {
        self.off_grid
    }
}

/// The first ALLOCATOR-RECORDED object start strictly after `after`.
///
/// The young sweep's zero-span screen needs somewhere on-grid to resume, and
/// no byte-level stride can supply one. A 24-byte all-zero run is EITHER a
/// 16-byte zero-header object followed by the next object's zero
/// `class_id`/`shape` word (next start = `run_end - 8`) OR a dead zeroed
/// 24-byte object (next start = `run_end`), and the bytes cannot tell those
/// apart -- that ambiguity IS the defect, since `HEADER_SIZE` 24 -> 16 made a
/// JIT-allocated `new Object()` bit-identical to zeroed arena. A stride that
/// guesses lands mid-object and the walk then subsumes a live object under a
/// phantom header (caught by the `gc::guard` "left the object grid" check,
/// observed once per ~10 runs before this).
///
/// `grid_anchors` is built from `Arena::take_alloc_anchors`, so every entry is
/// a real object start, and entries inside free/TLAB-skip blocks are already
/// filtered out. Anchors are TLAB-granular (256 KiB - 1 MiB), so resuming here
/// retains at most one TLAB rather than the rest of the arena.
pub fn next_grid_anchor(anchors: &[usize], after: usize) -> Option<usize> {
    anchors
        .get(anchors.partition_point(|&a| a <= after))
        .copied()
}

/// Why [`zero_run_verdict`] answered as it did.
///
/// A boolean here was not enough: once the parallel chunk walker's seven bail
/// reasons collapsed onto "the zero run", the only remaining question was which
/// of the three conditions had rejected, and every one of them means something
/// different — misaligned is a run that cannot be whole objects, a live base
/// inside is the walk being somewhere it should not be, and an implausible
/// following header is the run not ending where an object starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroRunVerdict {
    /// A run of empty objects ending at `resume`, an on-grid offset. `resume`
    /// is the run's end rounded DOWN to a whole HEADER_SIZE slot, which is not
    /// always the run's end — see the truncation note in `zero_run_verdict`.
    EmptyObjects { resume: usize },
    /// The run does not contain even one whole HEADER_SIZE slot.
    Misaligned,
    /// A marked object BASE starts strictly inside the run.
    LiveInside,
    /// The header at `run_end` does not size plausibly, so `run_end` is not a
    /// believable object start.
    ImplausibleNext,
}

/// Why the selective-promotion evacuation pre-pass unwound its candidates.
/// Index: `0` free-block overshoot, `1` unlisted zero span, `2` implausible
/// header size, `3` extent crosses a free hole. The aggregate
/// [`SWEEP_PROMOTION_ABORT_HITS`] cannot say, and the four mean different
/// things — only `1` is the benign-shape family, the rest are grid evidence.
pub static EVAC_UNWIND_REASONS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Candidates dropped by those unwinds, summed. The count per event is what
/// makes this expensive (37 354 in one, measured), not the number of events.
pub static EVAC_UNWIND_CANDIDATES: AtomicU64 = AtomicU64::new(0);

/// Zero-run anomaly hits at the three walks that still carry the old rule,
/// indexed as [`YOUNG_WALK_ENTRIES`] `2`..`4`: `0` `mark_young_to_old_refs`,
/// `1` `fixup_young_old_refs`, `2` `walk_young_objects`. Separate from the
/// entry counts because "ran and did not see the shape" and "never ran" are
/// different answers and both are worth being able to prove.
pub static LATE_WALK_ZERO_RUNS: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Entries to each young from-space walk that still treats an all-zero run as
/// a desync (the five `zero_run_end` callers left over after the 2026-08-13
/// work). A zero anomaly count means one of two opposite things — the walk ran
/// and the shape was absent, or the walk never ran — and only this separates
/// them. One relaxed increment per WALK, not per object.
///
/// Index: `0` selective-promotion evacuation pre-pass, `1` the (3a) survivor
/// fixup pass, `2` `mark_young_to_old_refs`, `3` `fixup_young_old_refs`,
/// `4` `walk_young_objects`.
pub static YOUNG_WALK_ENTRIES: [AtomicU64; 5] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Refusals by [`ZeroRunVerdict`], index = discriminant order minus the
/// accepting variant: `0` Misaligned, `1` LiveInside, `2` ImplausibleNext.
/// Printed beside the chunk-bail split; see `PAR_CHUNK_BAILS`.
pub static ZERO_RUN_REFUSALS: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Zero runs stepped over by resuming at a PROVED live base inside them,
/// instead of refusing and unwinding the cycle's reclaim decisions. See the
/// long note in `zero_run_verdict`.
///
/// This is an ACCEPTANCE and deliberately not an entry in
/// [`ZERO_RUN_REFUSALS`]: that array prints under a `refusals=` label, and
/// filing an acceptance there would make the fix read as the defect it removes.
/// `live_inside` stays the count of runs that were genuinely refused — an
/// interior mark, or a caller with no `unresolved` set to judge against.
pub static ZERO_RUN_LIVE_RESUMES: AtomicU64 = AtomicU64::new(0);
