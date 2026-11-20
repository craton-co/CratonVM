#[allow(unused_imports)]
use super::*;


/// Is the all-zero run `[cursor, run_end)` a run of EMPTY objects rather than
/// evidence that the walk left the object grid?
///
/// An empty object — `ClassId(0)`, `kind = Object` (tag 0), `num_slots = 0`,
/// identity hash not yet minted — is HEADER_SIZE all-zero bytes, and
/// [`gen_object_total_size`] sizes it at exactly HEADER_SIZE. That has been an
/// ordinary, common allocation ever since HEADER_SIZE shrank 24 -> 16 made the
/// mark word the second header word and the JIT's `new Object()` fast path left
/// it zero (`jit/src/x64/objects.rs`; the interpreter mints the hash eagerly,
/// which is why `--nojit` never saw this). A DEAD one is unmarked, so the
/// sweep's `side_sorted` cannot vouch for it, and it used to fall into the
/// unlisted-zero-span anomaly path — which unwinds every reclaim decision taken
/// since the last grid anchor.
///
/// Measured on `org.hibernate.reactive.BatchingConnectionTest` under
/// `-XX:+UseGenerationalGC` (a live JIT frame forces the non-moving sweep, so
/// every young cycle takes that walk): 147 such spans per cycle, EVERY one
/// exactly 16 bytes, 2352 bytes in total — and they discarded 40 724 dead
/// regions, leaving 22. The sweep reclaimed 789 KB of a 187 MB young
/// generation, `live` never fell back under the collection trigger, and the
/// collector ran continuously until the test's 120 s cap fired. No other
/// desync guard fired on those cycles (phantom extents 0, implausible sizes 0,
/// free-hole overlaps 0) — which is what says these runs were the benign shape
/// and not evidence of anything.
///
/// The three conditions are exactly what makes resuming at `run_end` on-grid:
///
/// 1. the run is a whole number of HEADER_SIZE object slots;
/// 2. no marked object BASE starts strictly inside it (one at `cursor` is the
///    caller's `vouched_live` case, which parses normally instead);
/// 3. the header at `run_end` itself sizes plausibly, or the run ends the
///    arena.
///
/// Anything else — notably a long unlisted zeroed span, the shape that
/// abandoned 233 MB of a 256 MB young generation in
/// `jit-young-heap-exhaustion-after-header-16-FIXED-20260807` — still takes the
/// unwind. This says nothing about whether the run is garbage: the caller
/// STEPS OVER it and never frees it, because the span may equally be a live
/// allocation whose header a stale register-held reference clobbered.
///
/// # Safety contract
///
/// `[base + cursor, base + used)` must be mapped from-space, and `run_end` must
/// be the end of an all-zero run starting at `cursor` (so `run_end <= used`).
pub fn zero_run_verdict(
    base: usize,
    cursor: usize,
    run_end: usize,
    used: usize,
    side_sorted: &[usize],
    unresolved: Option<&[usize]>,
) -> ZeroRunVerdict {
    // Truncate to the last whole slot, and judge THAT boundary.
    //
    // Requiring the run itself to be a multiple of HEADER_SIZE looked like the
    // conservative choice and is wrong about what ends a zero run. The walk is
    // word-granular, so a run can only be ragged by 8 bytes, and that happens
    // whenever the object AFTER the empty ones has a zero first word — i.e. it
    // is itself a `ClassId(0)` empty object (`class_id` and `shape` both zero)
    // whose MARK word is not zero. `MARK_NEUTRAL` is 0 and `ObjectKind::Object`
    // and `ArrayElementType::Reference` are both 0, so a freshly built empty
    // object is wholly zero; something has to have written that second word.
    // A one-off probe over six runs classified every ragged tail as neither
    // side-marked, nor header-marked, nor aged — so in practice it is a minted
    // identity hash or a lock word, i.e. a DEAD empty object that was used
    // before it died. `zero_run_end` stops 8 bytes INSIDE that header, so
    // `run_end` is not an object start — but `cursor + n * HEADER_SIZE` is, and
    // it is exactly that object's base, which is both on-grid and the next
    // thing the walk wants to reclaim.
    //
    // Measured: on `org.hibernate.reactive.BatchingConnectionTest` under
    // `-XX:+UseGenerationalGC`, after the earlier zero-run fixes, EVERY
    // remaining parallel-chunk bail was the zero run and EVERY zero-run refusal
    // was this alignment test.
    //
    // Read that census carefully, though: the alignment test used to run FIRST
    // and return immediately, so `live_inside=0` did not mean no live base was
    // inside — it meant the check never got to ask. Ordering a cheap test in
    // front of an informative one makes the informative one's zero unreadable.
    // Truncating first fixes that too: the live-base and plausible-next-header
    // checks below now judge the truncated end, and a live base AT that end is
    // the expected case rather than a veto (it is where the walk resumes).
    let run_end = cursor + ((run_end - cursor) / HEADER_SIZE) * HEADER_SIZE;
    if run_end - cursor < HEADER_SIZE {
        ZERO_RUN_REFUSALS[0].fetch_add(1, Ordering::Relaxed);
        return ZeroRunVerdict::Misaligned;
    }
    // A marked address strictly inside `(cursor, run_end)`. `side_sorted` is
    // ascending, so the only candidate is the last entry below `run_end`.
    let hi = base + run_end;
    let i = side_sorted.partition_point(|&a| a < hi);
    if i > 0 && side_sorted[i - 1] > base + cursor {
        // RESUME AT A PROVED BASE, REFUSE OTHERWISE.
        //
        // The refusal below is right about its subject and must stay: a zero
        // run casts doubt on every STRIDE since the last anchor, and the
        // caller's unwind is what makes a mis-sized stride cost retention
        // instead of a freed live object.
        //
        // But it is too strong for one shape. A field-less object is WHOLLY
        // zero (`MARK_NEUTRAL`, `ObjectKind::Object` and
        // `ArrayElementType::Reference` all encode as 0 — see the note above),
        // so a run of them containing a LIVE empty object is the ordinary case,
        // not a desync. On `ChurnLoop` (125k `new Object()` a round under
        // `System.gc()`) every refusal was this one — `live_inside=17` against
        // `zero_empty_runs=144` — and each discarded the cycle's whole
        // reclamation: `dead_pushed=294 unwound_entries=295 bytes_swept=0`.
        //
        // What separates the two readings is whether the marked address is an
        // object BASE. It need not be: `SWEEP_PHANTOM_INTERIOR_MARKS` records
        // that conservative candidates can mark an object-INTERIOR word (a
        // field address, a derived pointer, a spilled register mid-object) and
        // the late-resolution pass cannot unmark the raw address, "so
        // `side_sorted` can carry both". Resuming at an interior mark would
        // land the cursor MID-OBJECT — the very desync this guard exists for,
        // and the reason an earlier attempt at this (which argued `side_sorted`
        // entries are "bases by construction") was reverted.
        //
        // `unresolved` is the set that may not be bases; everything else came
        // from a precise ref-slot value or from `resolve_candidate_bases`, and
        // is a base by construction. `None` means the caller cannot answer, and
        // then this stays a refusal — which is what keeps every walk that has
        // no live set, and `a_live_base_inside_the_run_is_still_a_desync`,
        // behaving exactly as before.
        //
        // The FIRST base above `cursor` is the resume point, not the last below
        // `run_end`: resuming at the last would step over the live objects
        // between them, which are precisely the ones the walk must visit.
        if let Some(unresolved) = unresolved {
            let j = side_sorted.partition_point(|&a| a <= base + cursor);
            if j < i && unresolved.binary_search(&side_sorted[j]).is_err() {
                ZERO_RUN_LIVE_RESUMES.fetch_add(1, Ordering::Relaxed);
                return ZeroRunVerdict::EmptyObjects {
                    resume: side_sorted[j] - base,
                };
            }
        }
        ZERO_RUN_REFUSALS[1].fetch_add(1, Ordering::Relaxed);
        return ZeroRunVerdict::LiveInside;
    }
    if run_end >= used {
        return ZeroRunVerdict::EmptyObjects { resume: run_end };
    }
    // SAFETY: `run_end < used` and the run is HEADER_SIZE-aligned from an
    // on-grid `cursor`, so `base + run_end` is a header-aligned address inside
    // the mapped from-space region the caller guarantees.
    let next = unsafe { &*((base + run_end) as *const ObjectHeader) };
    let size = gen_object_total_size(next);
    if size >= HEADER_SIZE && run_end + size <= used {
        ZeroRunVerdict::EmptyObjects { resume: run_end }
    } else {
        ZERO_RUN_REFUSALS[2].fetch_add(1, Ordering::Relaxed);
        ZeroRunVerdict::ImplausibleNext
    }
}

/// `CRATONVM_GC_NO_EMPTY_OBJECT_RUN=1` — treat every all-zero run as a walk
/// desync again, i.e. restore the pre-2026-08-12 behaviour in which a run of
/// EMPTY objects took the unwind-and-resync path.
///
/// This exists because the empty-object-run recovery is the kind of fix whose
/// ABSENCE is invisible: without it the sweep still completes, still reports a
/// walk that ran to `used`, and simply throws away almost every reclaim
/// decision it made — 40 724 of 40 746 in the case it was written for. Two
/// separate investigations (`young-sweep-empty-object-run-unwind-20260812`, and
/// `TestDefaultInstanceManager`'s fourth recurrence) spent rounds on symptoms of
/// exactly that, and neither could A/B the mechanism in one binary because
/// there was no way to turn it off. Now there is, and the cross-run comparison
/// that "prices the box" instead of the change is not the only option.
///
/// NOT `OnceLock`-cached: this is read once per zero RUN, not per object, and
/// caching it would make the flag racy against whichever sweep runs first.
pub fn empty_object_run_recovery_disabled() -> bool {
    false
}

/// `Some(resume)` when the run is a run of EMPTY objects and the walk may step
/// to `resume`; `None` when it is evidence the walk left the object grid.
pub fn zero_run_empty_object_resume(
    base: usize,
    cursor: usize,
    run_end: usize,
    used: usize,
    side_sorted: &[usize],
    unresolved: Option<&[usize]>,
) -> Option<usize> {
    if empty_object_run_recovery_disabled() {
        return None;
    }
    match zero_run_verdict(base, cursor, run_end, used, side_sorted, unresolved) {
        ZeroRunVerdict::EmptyObjects { resume } => Some(resume),
        _ => None,
    }
}

/// DoHead walk-desync hardening: measure the all-zero run starting at
/// `start` (an 8-aligned walk-grid offset), in 8-byte words, capped at
/// `limit`. Returns the run's END offset (always 8-aligned unless the run
/// reaches an unaligned `limit` exactly). A run `>= HEADER_SIZE` at a grid
/// offset can never be a legally allocated object header in place — real
/// headers have a non-zero first word (`class_id | kind | element_type`) or,
/// for the zero-slot `ClassId(0)` ad-hoc container, at most `HEADER_SIZE`
/// zero bytes followed by the next real header.
///
/// The caller must guarantee `[base+start, base+limit)` is mapped arena
/// memory (both offsets within the arena's committed capacity).
#[inline]
pub fn zero_run_end(base: usize, start: usize, limit: usize) -> usize {
    let mut r = start;
    // SAFETY: (caller contract) the scanned range is mapped arena memory.
    while r + 8 <= limit && unsafe { *((base + r) as *const u64) } == 0 {
        r += 8;
    }
    if r < limit && limit - r < 8 {
        // Sub-word tail before `limit`: absorb it only if fully zero, so a
        // run ending exactly at a free-block boundary is reported as such.
        // SAFETY: `[r, limit)` lies within `[start, limit)`, which the caller guarantees is mapped
        // arena memory; the single-byte reads there are in-bounds.
        let all_zero = (r..limit).all(|i| unsafe { *((base + i) as *const u8) } == 0);
        if all_zero {
            r = limit;
        }
    }
    r
}

// ---------------------------------------------------------------------------
// The shared young grid probe (gen r5w1/young5, 2026-09-26)
// ---------------------------------------------------------------------------

/// What the young from-space object grid holds at one on-grid offset.
///
/// The verdict of [`probe_grid_at`], which implements every grid rule the
/// linear young walks share ONCE: the GAP-filler stride, the empty-object-run
/// rule, the header-size bound, the free-hole crossing and (opt-in) the
/// phantom extent. Each walk keeps its own loop, free-block skip and anomaly
/// POLICY (unwind, bail, discard, stretch rewrite, re-anchor) by matching on
/// this; it no longer re-implements the rules. See
/// `docs/feature-designs/generative-gc-architecture.md`
/// for the eleven hand-rolled copies this replaces and the defect families
/// their drift produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridProbe {
    /// A GAP-filler sentinel (a sub-`HEADER_SIZE` retired-TLAB tail) of `len`
    /// bytes: stride over it; it is dead filler and never an object.
    Gap { len: usize },
    /// A GAP-filler sentinel whose length is not a legal gap. Only reported
    /// under [`GridRules::bad_gap_filler_is_anomaly`]; otherwise the sentinel
    /// is parsed as a header and the size / hole checks judge it (the
    /// historical behaviour of the sweep and selective-promotion walks).
    BadGapFiller,
    /// A run of EMPTY objects `[off, resume)` (see
    /// [`zero_run_empty_object_resume`]): step over it, never free it.
    EmptyRun { resume: usize },
    /// An unlisted all-zero span `[off, run_end)` that is NOT a run of empty
    /// objects — evidence the walk left the grid. `limit` is where the run
    /// measurement was capped (the next free block, `used` or the window end).
    ZeroSpan { run_end: usize, limit: usize },
    /// A header that sizes below `HEADER_SIZE` or past `used`.
    BadSize { size: usize },
    /// A header of `size` bytes whose extent crosses the next free / skip
    /// block, which starts at `free_off`. A free block is carved from dead
    /// spans, never from inside a live object, so this header is not one.
    CrossesFreeBlock { size: usize, free_off: usize },
    /// A header of `size` bytes whose extent strictly contains the marked
    /// address `victim` (see [`mark_strictly_inside`]). Only reported under
    /// [`GridRules::phantom`].
    Phantom { size: usize, victim: usize },
    /// A parsed object `[off, off + size)`.
    Object { size: usize },
}

/// How [`probe_grid_at`] treats an all-zero first header word.
#[derive(Debug, Clone, Copy)]
pub enum ZeroRunRule<'a> {
    /// No zero-run rule: an all-zero header is parsed like any other, which
    /// sizes it as one `HEADER_SIZE` empty object. The late base-resolution
    /// walk and the exact-base oracle have always walked this way; both then
    /// resolve a candidate inside a zero run to the empty object containing
    /// it, which is the conservative (retaining) answer for a root.
    Parse,
    /// Step over an unvouched run of empty objects
    /// ([`zero_run_empty_object_resume`] with `unresolved`), and report any
    /// other unvouched zero span of at least `HEADER_SIZE` as
    /// [`GridProbe::ZeroSpan`]. A zero run shorter than a header, or one whose
    /// first address `side_sorted` vouches for, is parsed normally.
    /// `count_empty_runs` bumps `SWEEP_ZERO_SPAN_EMPTY_RUNS` /
    /// `EMPTY_RUN_BYTES_CYCLE` for every run stepped over (every collector
    /// walk does; the read-only diagnostic walks must not).
    StepOver {
        unresolved: Option<&'a [usize]>,
        count_empty_runs: bool,
    },
}

/// The per-walk parameters of [`probe_grid_at`]: the geometry and the few
/// rules whose policy genuinely differs between walks.
#[derive(Debug, Clone, Copy)]
pub struct GridRules<'a> {
    /// The from-space base address.
    pub base: usize,
    /// The arena's allocated extent: every header must size within it, and
    /// the empty-run verdict judges against it.
    pub used: usize,
    /// Where a zero-run measurement must stop even with no free block before
    /// it: `used` for a whole-arena walk, the chunk end for a chunk walk that
    /// must land exactly on its upper anchor.
    pub zero_run_hi: usize,
    /// The cycle's marked bases, ascending and absolute (`&[]` when the walk
    /// has no mark set). Vouches for a zero-headed live object and feeds the
    /// empty-run and phantom checks.
    pub side_sorted: &'a [usize],
    pub zero_runs: ZeroRunRule<'a>,
    /// See [`GridProbe::BadGapFiller`].
    pub bad_gap_filler_is_anomaly: bool,
    /// Report a header whose extent contains a mark ([`GridProbe::Phantom`]).
    /// Only meaningful with a `side_sorted` whose entries are object BASES
    /// (the reclaiming sweep walks, after late resolution).
    pub phantom: bool,
}

impl<'a> GridRules<'a> {
    /// A whole-arena walk with no mark set: GAP fillers strided, malformed
    /// ones reported, empty-object runs stepped over (conservative arm,
    /// counted), no phantom check.
    pub fn unmarked(base: usize, used: usize) -> Self {
        Self {
            base,
            used,
            zero_run_hi: used,
            side_sorted: &[],
            zero_runs: ZeroRunRule::StepOver {
                unresolved: None,
                count_empty_runs: true,
            },
            bad_gap_filler_is_anomaly: true,
            phantom: false,
        }
    }
}

/// gcf f3/q (r15w5/gcpage patch 1): a one-entry size memo for ONE linear young
/// walk (`CRATONVM_GEN_WALK_SIZE_MEMO`, default ON; `=0` restores the
/// per-object lookups).
///
/// For a compact instance (`ObjectHeader::is_short`: kind `Object` AND
/// `GC_FLAG_COMPACT`), [`gen_object_total_size`] is a function of
/// `(class_id, layout_generation())` alone: the kind screen passes, the array
/// arm is not taken, `is_compact_object` holds, and `object_instance_size`
/// reads `with_class_layout(class_id, num_slots())` with `num_slots() ==
/// compact_field_count(class_id)`. Both thread-local caches behind those two
/// calls (`ScanCache`, `COMPACT_COUNT_CACHE`) are keyed and invalidated on
/// exactly that pair, so answering a repeat of the pair from here gives the
/// value the two lookups would have given, without the two TLS-and-RefCell
/// accesses. The memo only ever stores a value `gen_object_total_size` itself
/// returned for that pair (the implausible sentinel included). Arrays and
/// legacy instances are sized from their own header words and never touch it.
/// Not shared: every walk (and every `sweep_chunk` call) owns its own.
#[derive(Debug, Clone, Copy)]
pub struct GridSizeMemo {
    pub armed: bool,
    pub class_id: u32,
    pub generation: u64,
    pub size: usize,
    /// Answers served from the memo (tests and the positive control only;
    /// a plain local add, never published per object).
    pub hits: u64,
}

impl GridSizeMemo {
    /// Never answers: `total_size` is `gen_object_total_size`, byte for byte.
    pub const OFF: Self = Self {
        armed: false,
        class_id: 0,
        // No real generation reaches `u64::MAX`, so an armed memo's first
        // lookup always misses.
        generation: u64::MAX,
        size: 0,
        hits: 0,
    };

    /// A memo for one walk, armed when `armed` (the caller read
    /// [`gen_walk_size_memo_enabled`] once for the whole sweep).
    pub fn new(armed: bool) -> Self {
        Self { armed, ..Self::OFF }
    }

    #[inline]
    pub fn total_size(&mut self, header: &ObjectHeader) -> usize {
        if !self.armed || !header.is_short() {
            return gen_object_total_size(header);
        }
        let class_id = header.class_id.as_u32();
        let generation = cratonvm_types::layout_generation();
        if class_id == self.class_id && generation == self.generation {
            self.hits += 1;
            return self.size;
        }
        let size = gen_object_total_size(header);
        self.class_id = class_id;
        self.generation = generation;
        self.size = size;
        size
    }
}

/// `CRATONVM_GEN_WALK_SIZE_MEMO` (default ON; `=0` sizes every object through
/// the two layout lookups). gcf f3/q. Read once per sweep, never per object.
pub fn gen_walk_size_memo_enabled() -> bool {
    true
}

/// Judge the young object grid at offset `off` under `rules` — the ONE
/// implementation of the grid rules (see [`GridProbe`]).
///
/// The caller has already run [`skip_free_blocks`] (so `off` is not inside a
/// free / skip block) and passes `next_free`, the start of the next free /
/// skip block at or above `off` (`free_iter.peek()`), if any. `live_probe` is
/// the caller's monotone index into `rules.side_sorted` for the phantom check
/// (untouched when `rules.phantom` is off).
///
/// The rules, in the order every walk applied them: GAP-filler stride; the
/// zero-run rule; `HEADER_SIZE <= size <= used - off`; no crossing of the next
/// free block; no mark strictly inside the extent.
///
/// # Safety contract
///
/// `off < rules.used`, and `[rules.base + off, rules.base + rules.used)` must
/// be mapped from-space (the same contract [`zero_run_end`] and
/// [`zero_run_verdict`] state). An implausible header is screened here, never
/// trusted.
#[inline]
pub fn probe_grid_at(
    off: usize,
    next_free: Option<usize>,
    rules: &GridRules<'_>,
    live_probe: &mut usize,
) -> GridProbe {
    let mut memo = GridSizeMemo::OFF;
    probe_grid_at_sized(off, next_free, rules, live_probe, &mut memo)
}

/// [`probe_grid_at`] with the caller's per-walk [`GridSizeMemo`] (gcf f3/q).
/// The one difference: the header is sized through `size_memo`, which returns
/// exactly what [`gen_object_total_size`] returns.
#[inline]
pub fn probe_grid_at_sized(
    off: usize,
    next_free: Option<usize>,
    rules: &GridRules<'_>,
    live_probe: &mut usize,
    size_memo: &mut GridSizeMemo,
) -> GridProbe {
    let ptr = rules.base + off;
    // SAFETY: (caller contract) `off < used` inside mapped from-space.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if header.class_id.as_u32() == cratonvm_gc_core::tlab::GAP_FILLER_CLASS_ID.as_u32() {
        // SAFETY: offset 4 lies within the >= 8-byte gap.
        let gap = unsafe { std::ptr::read((ptr as *const u8).add(4) as *const u32) } as usize;
        if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && off + gap <= rules.used {
            return GridProbe::Gap { len: gap };
        }
        if rules.bad_gap_filler_is_anomaly {
            return GridProbe::BadGapFiller;
        }
        // Parsed as a header below; its class id is not zero, so the zero
        // rule does not apply and the size / hole checks judge it.
    }
    if let ZeroRunRule::StepOver {
        unresolved,
        count_empty_runs,
    } = rules.zero_runs
    {
        // SAFETY: the first header word at `ptr`, in bounds as above.
        let word0 = unsafe { *(ptr as *const u64) };
        if word0 == 0 {
            let limit = next_free
                .unwrap_or(rules.used)
                .min(rules.used)
                .min(rules.zero_run_hi);
            let run_end = zero_run_end(rules.base, off, limit);
            // A marked base AT the run start is a live `ClassId(0)` object
            // whose header legitimately reads zero: parse it normally.
            let vouched = rules.side_sorted.binary_search(&ptr).is_ok();
            if run_end - off >= HEADER_SIZE && !vouched {
                if let Some(resume) = zero_run_empty_object_resume(
                    rules.base,
                    off,
                    run_end,
                    rules.used,
                    rules.side_sorted,
                    unresolved,
                ) {
                    if count_empty_runs {
                        SWEEP_ZERO_SPAN_EMPTY_RUNS.fetch_add(1, Ordering::Relaxed);
                        EMPTY_RUN_BYTES_CYCLE.fetch_add((resume - off) as u64, Ordering::Relaxed);
                    }
                    return GridProbe::EmptyRun { resume };
                }
                return GridProbe::ZeroSpan { run_end, limit };
            }
        }
    }
    let size = size_memo.total_size(header);
    if size < HEADER_SIZE || off.checked_add(size).is_none_or(|end| end > rules.used) {
        return GridProbe::BadSize { size };
    }
    if let Some(free_off) = next_free {
        if free_off > off && free_off < off + size {
            return GridProbe::CrossesFreeBlock { size, free_off };
        }
    }
    if rules.phantom {
        if let Some(victim) = mark_strictly_inside(rules.side_sorted, live_probe, ptr, size) {
            return GridProbe::Phantom { size, victim };
        }
    }
    GridProbe::Object { size }
}

/// Why [`YoungGridCursor`] could not parse an offset as an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridAnomaly {
    /// A `GAP_FILLER_CLASS_ID` sentinel whose length is not a legal gap.
    BadGapFiller,
    /// An all-zero span that is not a run of empty objects.
    ZeroSpan,
    /// A header that sizes below `HEADER_SIZE` or past `used`.
    BadSize,
    /// An extent that crosses a pre-existing free / skip block.
    CrossesFreeBlock,
}

/// One step of a [`YoungGridCursor`] walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridStep {
    /// A parsed object `[off, off + size)` (offsets relative to the arena base).
    Object { off: usize, size: usize },
    /// `off` could not be parsed. The cursor has ALREADY re-anchored — at the
    /// next free/skip block, or at the crossed block for `CrossesFreeBlock` —
    /// or, when nothing is left to re-anchor on, ended the walk
    /// ([`YoungGridCursor::truncated`]).
    Anomaly { off: usize, reason: GridAnomaly },
}

/// A READ-ONLY linear walk of young from-space that applies every grid rule
/// the collector's own walks apply, implemented once.
///
/// gen r4w2/youngmark (2026-09-23), first step of
/// `docs/feature-designs/generative-gc-architecture.md`:
/// eleven hand-rolled walks each carried their own subset of the rules, and the
/// four DIAGNOSTIC ones (`CRATONVM_SP_VERIFY`, `[sweep-edges]`,
/// `[sweep-census]`'s inverted holder scan, `[sweep-trace]`) had none of the
/// GAP-filler, empty-object-run or hole-crossing rules, so they sized the first
/// retired-TLAB tail sentinel as an object and stopped — reporting "0 missed"
/// / "0 holders" for an arena they had walked a prefix of. They now all walk
/// through this.
///
/// The rules, in the order the collector walks apply them: robust free/skip
/// block resync ([`skip_free_blocks`]); GAP-filler stride; an all-zero word at
/// the cursor is an empty-object run ([`zero_run_empty_object_resume`], with no
/// unresolved-mark set — the conservative arm) unless `side_sorted` vouches
/// for the address, else an anomaly; a header must size to
/// `[HEADER_SIZE, used - off]`; an extent may not cross a free block. A run of
/// unvouched empty objects is stepped over and NOT yielded (the collector
/// walks do the same: such objects have no fields and are never freed there).
///
/// The anomaly POLICY stays the caller's: every anomaly is yielded, counted in
/// [`Self::anomalies`], and the walk continues at the next trustworthy anchor.
/// The phantom check (a mark strictly inside an extent) is deliberately not
/// here, since it needs the mark set and only the reclaiming walks act on it.
///
/// gen r5w1/young5: the rules themselves now live in [`probe_grid_at`], which
/// this cursor and every collector walk call (the main sweep walk,
/// `sweep_chunk`, the selective-promotion walks, the late resolution, the
/// exact-base oracle, the object-start walks and the moving path's young
/// walks); walks whose policy is "re-anchor and continue" use this cursor
/// directly (`clear_all_mark_bits_in_arena`, the diagnostic walks,
/// `live_holders_of`). (This paragraph used to say no collector walk was on
/// the cursor; the mark-clear walk had been since gen r4w6.)
///
/// Side effect worth knowing: the zero-run judgement is `zero_run_verdict`,
/// which counts its refusals in `ZERO_RUN_REFUSALS`. The diagnostic walks run
/// only under their debug flags, so a default run's counters are unaffected.
pub struct YoungGridCursor<'a> {
    pub base: usize,
    pub used: usize,
    pub cursor: usize,
    pub free_iter: std::iter::Peekable<std::slice::Iter<'a, (usize, usize)>>,
    pub side_sorted: &'a [usize],
    /// Anomalies yielded so far.
    pub anomalies: usize,
    /// Set when an anomaly left nothing to re-anchor on, so the arena past it
    /// was NOT walked — every count from this walk is then a lower bound.
    pub truncated: bool,
    /// Bump `SWEEP_ZERO_SPAN_EMPTY_RUNS` / `EMPTY_RUN_BYTES_CYCLE` for every
    /// empty-object run stepped over — the side effect the COLLECTOR walk
    /// `clear_all_mark_bits_in_arena` has always had (gen r4w6/young6, when
    /// it moved onto this cursor). `false` for the read-only diagnostic walks,
    /// which must not move those counters.
    pub count_empty_runs: bool,
}

impl<'a> YoungGridCursor<'a> {
    /// Walk `[base, base + used)` skipping `free_blocks` (offsets, ascending,
    /// the merged free-list + TLAB-skip list). `side_sorted` is the cycle's
    /// marked bases (ascending, absolute), or `&[]` when none is available.
    ///
    /// The caller guarantees `[base, base + used)` is mapped from-space.
    pub fn new(
        base: usize,
        used: usize,
        free_blocks: &'a [(usize, usize)],
        side_sorted: &'a [usize],
    ) -> Self {
        Self {
            base,
            used,
            cursor: 0,
            free_iter: free_blocks.iter().peekable(),
            side_sorted,
            anomalies: 0,
            truncated: false,
            count_empty_runs: false,
        }
    }

    pub fn anomaly(&mut self, off: usize, reason: GridAnomaly) -> Option<GridStep> {
        self.anomalies += 1;
        if reason == GridAnomaly::CrossesFreeBlock {
            // `next` already moved the cursor onto the crossed block's start.
        } else if !resync_to_next_free_block(&mut self.cursor, &mut self.free_iter) {
            self.truncated = true;
            self.cursor = self.used;
        }
        Some(GridStep::Anomaly { off, reason })
    }
}

impl Iterator for YoungGridCursor<'_> {
    type Item = GridStep;

    fn next(&mut self) -> Option<GridStep> {
        loop {
            if self.cursor >= self.used {
                return None;
            }
            if skip_free_blocks(&mut self.cursor, &mut self.free_iter).0 {
                continue;
            }
            if self.cursor >= self.used {
                return None;
            }
            let off = self.cursor;
            let next_free = self.free_iter.peek().map(|&&(foff, _)| foff);
            let rules = GridRules {
                base: self.base,
                used: self.used,
                zero_run_hi: self.used,
                side_sorted: self.side_sorted,
                zero_runs: ZeroRunRule::StepOver {
                    unresolved: None,
                    count_empty_runs: self.count_empty_runs,
                },
                bad_gap_filler_is_anomaly: true,
                phantom: false,
            };
            let mut no_phantom_probe = 0usize;
            // `off < used` and every free/skip block has been stepped over, so
            // `off` is inside the mapped from-space the constructor's caller
            // vouched for (`probe_grid_at`'s contract).
            match probe_grid_at(off, next_free, &rules, &mut no_phantom_probe) {
                GridProbe::Gap { len } => {
                    self.cursor = off + len;
                    continue;
                }
                GridProbe::EmptyRun { resume } => {
                    self.cursor = resume;
                    continue;
                }
                GridProbe::BadGapFiller => return self.anomaly(off, GridAnomaly::BadGapFiller),
                GridProbe::ZeroSpan { .. } => return self.anomaly(off, GridAnomaly::ZeroSpan),
                GridProbe::BadSize { .. } => return self.anomaly(off, GridAnomaly::BadSize),
                GridProbe::CrossesFreeBlock { free_off, .. } => {
                    self.cursor = free_off;
                    return self.anomaly(off, GridAnomaly::CrossesFreeBlock);
                }
                // `phantom` is off for this cursor, so a `Phantom` verdict
                // cannot be produced; it is the same parsed object either way.
                GridProbe::Phantom { size, .. } | GridProbe::Object { size } => {
                    self.cursor = off + size;
                    return Some(GridStep::Object { off, size });
                }
            }
        }
    }
}

/// DoHead walk-desync hardening: after a walk anomaly (zero span or
/// implausible header), re-anchor the cursor at the START of the next known
/// free block — the only downstream offsets that are ground truth — leaving
/// `[old cursor, anchor)` unparsed (conservatively retained). Returns `false`
/// when no anchor remains (caller should stop its walk; the remainder of the
/// arena is retained). The byte-pattern re-sync probe this replaces could
/// lock onto arbitrary zeroed words OFF the object grid (its plausibility
/// check accepts an all-zero header) and resume freeing from there.
#[inline]
pub fn resync_to_next_free_block(
    cursor: &mut usize,
    free_iter: &mut std::iter::Peekable<std::slice::Iter<'_, (usize, usize)>>,
) -> bool {
    while let Some(&&(off, sz)) = free_iter.peek() {
        if off + sz <= *cursor {
            free_iter.next();
            continue;
        }
        if *cursor > off {
            // Already inside the block — resume on-grid at its end.
            *cursor = off + sz;
            free_iter.next();
        } else {
            // Land ON the block start; the caller's skip loop consumes it
            // and resumes on-grid at its end.
            *cursor = off;
        }
        return true;
    }
    false
}

/// Walk every object header in `arena` and clear `GC_FLAG_MARKED`.
///
/// Used after `sweep_young_non_moving` to guarantee no stale mark bits
/// survive into the next collection (bug C5). The non-moving sweep's main
/// loop only clears marks on objects it visits as survivors; if the loop
/// breaks early on a corrupt / zero-size header, every still-marked object
/// past the breakout point would be treated as live by the *next* sweep
/// (`header.gc_flags & GC_FLAG_MARKED != 0`), retaining unreachable
/// garbage indefinitely.
///
/// Walks the same layout as the sweep — skipping known free blocks, parsing
/// headers in place — and is equally defensive about corruption: a
/// zero-size or out-of-range header stops the walk (we cannot safely
/// continue past unknown structure), but every header we *did* reach gets
/// its mark cleared. Cheap (one byte per header) and idempotent.
///
/// `side_sorted` is this collection's live set (marked object bases, ascending)
/// — the same slice the sweep walk used. It is what lets this walk tell an
/// ordinary run of dead EMPTY objects apart from a desync; without it every
/// such run costs a re-anchor and leaves stale marks behind (see the note at
/// the zero-run branch).
///
/// # On the shared grid cursor (gen r4w6/young6, 2026-09-24)
///
/// This is the first COLLECTOR walk moved onto [`YoungGridCursor`], the step
/// `docs/feature-designs/generative-gc-architecture.md`
/// named as the lowest-risk port left. Waves 3 and 4 made the hand-rolled
/// walk rule-identical to the cursor (GAP filler, empty-object run, free-hole
/// crossing including the straddle); the one remaining difference was its
/// side effect on `SWEEP_ZERO_SPAN_EMPTY_RUNS` / `EMPTY_RUN_BYTES_CYCLE`,
/// which the cursor now reproduces under `count_empty_runs`. Its anomaly
/// policy is the cursor's: re-anchor at the next free block, else stop — the
/// marks in a skipped stretch are left set (retention only), as before. The
/// one behavioural refinement is a GAP-filler sentinel whose gap length is
/// implausible: the old walk fell through and parsed it as an object header
/// (a mark-clear write at a size read from filler bytes); the cursor reports
/// it as `BadGapFiller` and re-anchors. The hand-rolled body is kept below as
/// `clear_all_mark_bits_in_arena_legacy`, test-only, so the drift tests can
/// still compare the two.
pub fn clear_all_mark_bits_in_arena(arena: &mut Arena, side_sorted: &[usize]) {
    let base = arena.base_ptr() as usize;
    let used = arena.used();
    let free_blocks = arena.free_blocks_sorted();
    let mut grid = YoungGridCursor::new(base, used, &free_blocks, side_sorted);
    grid.count_empty_runs = true;
    for step in grid {
        if let GridStep::Object { off, .. } = step {
            // SAFETY: the cursor yields `off` only for a parsed header whose
            // extent `[off, off + size)` lies inside `[0, used)` of this
            // arena's mapped from-space and crosses no free block; clearing a
            // flag is one atomic RMW on that header's mark word.
            let header = unsafe { &*((base + off) as *const ObjectHeader) };
            header.clear_gc_flags(GC_FLAG_MARKED);
        }
    }
}

// Read a `Value` from a slot pointer.
//
// DECODE RULE (R-niche): the 16-byte `Value` is read raw, returning the
// variant whose discriminant was last *written* into the slot. After
// `Value::Object` gained a `NonNull` niche, the all-zero bit pattern decodes
// as `Value::Int(0)`, NOT `Value::Object(None)` — there is no "zeroed slot
// reads as null" shortcut. Reference/uninitialized slots are therefore
// written an explicit `Value::Object(None)` (see
// `GenerationalHeap::alloc_object_with_descriptors`); this read does not
// synthesize `null` from zero bytes.
//
// # Safety
//
// `ptr` must point to a valid, initialized `Value`-sized region within a
// heap-allocated object. The caller must ensure no concurrent writes to
// the same slot.
// SAFETY: caller guarantees `ptr` points to a valid Value within a heap object.
#[inline]
pub unsafe fn read_slot(ptr: *mut u8) -> Value {
    // Defense-in-depth (HIB-CV-32): validate the discriminant before
    // constructing the `Value`. If a reference-integrity defect left this cell
    // holding bytes that do not form a valid `Value` (e.g. a swept-then-reused
    // live object whose primitive field reads back `{heap-ptr, 6}`), decoding
    // it as a `Value` and then matching on it — as the very next operand-stack
    // push does — is a wild jump-table SIGSEGV with no context. Instead return
    // a benign null (the field-read callers already normalize `Object(None)`
    // for a primitive slot) plus a rate-limited diagnostic, turning an
    // unrecoverable crash into a localizable one. The fast path is one aligned
    // 32-bit load + a predictable compare on top of the read already happening.
    //
    // The body now lives in `heap::read_value_cell_checked` so all four
    // legacy-cell readers share ONE implementation. They did not, and the three
    // that skipped this screen turned the same corrupt cell into a SIGSEGV while
    // this one reported it — see that helper for the measurement.
    cratonvm_gc_core::heap::read_value_cell_checked(ptr as *const Value, "gen_heap::read_slot")
}

// Write a `Value` to a slot pointer.
//
// # Safety
//
// `ptr` must point to a `Value`-sized region within a heap-allocated object.
// The caller must ensure exclusive access to the slot.
// SAFETY: caller guarantees `ptr` points to a valid Value slot within a heap object.
#[inline]
pub unsafe fn write_slot(ptr: *mut u8, value: Value) {
    // gcstress face-1 hunt (no-op unless CRATONVM_DBG_WATCH_CELL is set).
    cratonvm_gc_core::heap::cell_watch_check(ptr as usize, 16, "write_slot", &value);
    // PLAIN-SLOT TEARING FIX (2026-07-06): was a bare `ptr::write::<Value>`,
    // a non-atomic 16-byte copy that could tear against a concurrent plain
    // `get_field` from another mutator thread -- see
    // elasticsearch-lucene-binary-docvalues-range-hangs.md
    // #3 and commit 4e6b560f (the GC-marker-vs-JIT-store counterpart fix).
    cratonvm_types::write_value_atomic(ptr as *mut Value, value);
}

// ---------------------------------------------------------------------------
// GarbageCollector trait impl
// ---------------------------------------------------------------------------

impl GarbageCollector for GenerationalHeap {
    fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        GenerationalHeap::try_alloc_object_full(self, class_id, num_fields)
    }

    fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        GenerationalHeap::try_alloc_array_full(self, class_id, element_type, length)
    }

    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.alloc_object(class_id, num_fields)
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        self.alloc_array(class_id, element_type, length)
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        self.get_header(obj)
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.class_id_of(obj)
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.kind_of(obj)
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.element_type_of(obj)
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        self.identity_hash_code(obj)
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field(obj, index)
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field(obj, index, value)
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field_volatile(obj, index)
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field_volatile(obj, index, value)
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        self.array_length(obj)
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        self.get_array_element(obj, index)
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        self.set_array_element(obj, index, value)
    }

    fn needs_gc(&self) -> bool {
        self.needs_gc()
    }

    fn collect_garbage(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        self.collect_garbage(stw, roots, monitors)
    }

    fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        self.write_barrier(obj, stored_value)
    }

    /// Task #25: route through the inherent `satb_barrier` so the
    /// concurrent old-gen marker sees the about-to-be-overwritten ref.
    /// `satb_barrier` early-outs on the SATB queue's `is_active()` check when
    /// no cycle is marking (gc-common w5-e: the queue, not the phase).
    #[inline]
    fn write_barrier_pre(&self, _slot: *mut ObjectRef, old: ObjectRef) {
        self.satb_barrier(Value::Object(Some(old)));
    }

    fn allocated_bytes(&self) -> usize {
        self.allocated_bytes()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

pub use cratonvm_gc_core::jit_bounds::*;
