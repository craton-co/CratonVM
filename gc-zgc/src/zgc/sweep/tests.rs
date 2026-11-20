use super::*;


/// Set difference decided per byte: the slowest possible implementation,
/// which is the point -- it shares no structure with the one under test.
///
/// Byte granularity keeps the oracle obviously correct, so every extent
/// here is small. Zero-length spans are deliberately never constructed:
/// the producers (`sweep_bitmap_range`, `join_complement`) both push only
/// under a strict `>`, so a zero-length span is not in the domain.
fn oracle(
    spans: &[(usize, usize)],
    skips: &[(usize, usize)],
    base: usize,
) -> Vec<(usize, usize)> {
    let cuts: Vec<(usize, usize)> = skips
        .iter()
        .map(|&(s, e)| (s.saturating_sub(base), e.saturating_sub(base)))
        .filter(|&(s, e)| e > s)
        .collect();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &(off, len) in spans.iter() {
        let mut run: Option<(usize, usize)> = None;
        for b in off..off + len {
            if cuts.iter().any(|&(s, e)| b >= s && b < e) {
                if let Some(r) = run.take() {
                    out.push(r);
                }
            } else {
                run = match run {
                    Some((ro, rl)) => Some((ro, rl + 1)),
                    None => Some((b, 1)),
                };
            }
        }
        if let Some(r) = run.take() {
            out.push(r);
        }
    }
    out
}

/// Run the real function and the oracle over the same input and demand they
/// agree exactly, element for element and in order.
#[track_caller]
fn agree(spans: &[(usize, usize)], skips: &[(usize, usize)], base: usize) {
    let expected = oracle(spans, skips, base);
    let mut got = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut got, skips, base);
    assert_eq!(
        got, expected,
        "withhold_skip_regions disagreed with the per-byte oracle\n  spans: \
         {spans:?}\n  skips: {skips:?}\n  base:  {base}"
    );
    // Ascending and non-overlapping, because the arena's coalescer only
    // sees two spans as adjacent if they arrive in order.
    for w in got.windows(2) {
        let (a, b) = (w[0], w[1]);
        assert!(
            a.0 + a.1 <= b.0,
            "output must stay ascending and disjoint: {a:?} then {b:?}"
        );
    }
    // No empty spans: an empty free block is a free-list entry nobody can
    // allocate out of and the producers never make one.
    assert!(
        got.iter().all(|&(_, l)| l > 0),
        "withhold_skip_regions must not emit a zero-length span: {got:?}"
    );
}

const BASE: usize = 0x1_0000;

/// The shape the function exists for: one big low-region free span with a
/// frozen peer's published TLAB tail sitting inside it.
///
/// The tail must come out and **both flanks must survive**. An
/// implementation that dropped the whole span would be "safe" against the
/// double-allocation bug and would leak the rest of the region every cycle
/// a peer is frozen, so the flanks are asserted explicitly and not only
/// through the oracle.
#[test]
fn a_frozen_peers_published_tail_is_carved_out_and_the_flanks_survive() {
    let spans = [(0usize, 400usize)];
    let tail = (BASE + 120, BASE + 180);
    agree(&spans, &[tail], BASE);

    let mut got = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut got, &[tail], BASE);
    assert_eq!(
        got,
        vec![(0, 120), (180, 220)],
        "the tail is withheld and the bytes either side are still free"
    );
}

/// A tail that covers a whole span removes it, and a tail that covers every
/// span empties the list. This is the case where "the free list is now
/// shorter" must not be mistaken for a bug by a future reader.
#[test]
fn a_tail_covering_a_span_removes_it_entirely() {
    let spans = [(0usize, 64usize), (64, 64), (200, 40)];
    let skips = [(BASE + 64, BASE + 128)];
    agree(&spans, &skips, BASE);

    let mut got = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut got, &skips, BASE);
    assert_eq!(got, vec![(0, 64), (200, 40)]);

    let whole = [(BASE, BASE + 1024)];
    agree(&spans, &whole, BASE);
    let mut all = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut all, &whole, BASE);
    assert!(all.is_empty(), "every byte was withheld: {all:?}");
}

/// A tail flush against the start or the end of a span leaves exactly one
/// piece. These are the `cur == lo` and `cur >= hi` arms of the carve, and
/// an off-by-one in either produces a one-byte span or a one-byte leak --
/// neither of which a coarse assertion would notice.
#[test]
fn a_tail_flush_with_either_edge_leaves_one_piece() {
    let spans = [(100usize, 100usize)];
    agree(&spans, &[(BASE + 100, BASE + 140)], BASE);
    agree(&spans, &[(BASE + 160, BASE + 200)], BASE);
    // ...and one that abuts without overlapping touches nothing.
    agree(&spans, &[(BASE + 60, BASE + 100)], BASE);
    agree(&spans, &[(BASE + 200, BASE + 260)], BASE);

    let mut got = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut got, &[(BASE + 200, BASE + 260)], BASE);
    assert_eq!(got, vec![(100, 100)], "an abutting cut removes nothing");
}

/// **Unsorted** cuts. The rewrite sorts once up front; the old loop did not
/// need to because it restarted per cut. If the sort were dropped, the
/// forward walk would stop at the first cut whose start is past `hi` and
/// silently keep the bytes a later, lower cut covers -- which is the
/// silent-leak direction.
#[test]
fn cuts_are_normalised_regardless_of_the_order_they_arrive_in() {
    let spans = [(0usize, 500usize)];
    let ascending = [
        (BASE + 40, BASE + 60),
        (BASE + 100, BASE + 130),
        (BASE + 300, BASE + 310),
    ];
    let shuffled = [
        (BASE + 300, BASE + 310),
        (BASE + 40, BASE + 60),
        (BASE + 100, BASE + 130),
    ];
    agree(&spans, &ascending, BASE);
    agree(&spans, &shuffled, BASE);

    let mut a = spans.to_vec();
    let mut b = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut a, &ascending, BASE);
    ZgcRealHeap::withhold_skip_regions(&mut b, &shuffled, BASE);
    assert_eq!(a, b, "the answer cannot depend on the skip list's order");
}

/// **Overlapping and touching** cuts. The merge step is what lets the carve
/// assume the next cut starts strictly above the previous one's end; without
/// it `cur` can move backwards and emit an overlapping -- i.e. double-freed
/// -- output span.
#[test]
fn overlapping_and_touching_cuts_merge_before_the_carve() {
    let spans = [(0usize, 400usize)];
    agree(
        &spans,
        &[(BASE + 50, BASE + 150), (BASE + 100, BASE + 200)],
        BASE,
    );
    agree(
        &spans,
        &[(BASE + 50, BASE + 150), (BASE + 150, BASE + 200)],
        BASE,
    );
    // One cut wholly inside another.
    agree(
        &spans,
        &[(BASE + 40, BASE + 300), (BASE + 100, BASE + 120)],
        BASE,
    );
    // Three identical cuts.
    agree(
        &spans,
        &[
            (BASE + 10, BASE + 20),
            (BASE + 10, BASE + 20),
            (BASE + 10, BASE + 20),
        ],
        BASE,
    );
}

/// One cut straddling several spans, which is the shape a published tail
/// takes when the complement pass found survivors inside it. Each span is
/// entered independently through `partition_point`, so this is the case
/// where a stale `i` carried between spans would show.
#[test]
fn one_cut_across_several_spans() {
    let spans = [(0usize, 50usize), (60, 50), (120, 50), (200, 50)];
    agree(&spans, &[(BASE + 30, BASE + 210)], BASE);

    let mut got = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut got, &[(BASE + 30, BASE + 210)], BASE);
    assert_eq!(got, vec![(0, 30), (210, 40)]);
}

/// Several cuts inside one span, including two with a survivor between
/// them: the `while i < cuts.len() && cuts[i].0 < hi` loop must advance
/// through both.
#[test]
fn several_cuts_inside_one_span() {
    let spans = [(0usize, 300usize)];
    let skips = [
        (BASE + 20, BASE + 40),
        (BASE + 60, BASE + 80),
        (BASE + 250, BASE + 400),
    ];
    agree(&spans, &skips, BASE);

    let mut got = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut got, &skips, BASE);
    assert_eq!(got, vec![(0, 20), (40, 20), (80, 170)]);
}

/// Degenerate skips: empty ranges, inverted ranges, and ranges below the
/// arena base. `saturating_sub` collapses the last of these onto offset 0,
/// which is the behaviour the clip has always had -- a cut reaching below
/// the arena clips to its start rather than wrapping.
#[test]
fn degenerate_skips_are_dropped_or_clipped_but_never_wrap() {
    let spans = [(0usize, 200usize)];
    // Empty and inverted: no effect at all.
    agree(&spans, &[(BASE + 50, BASE + 50)], BASE);
    agree(&spans, &[(BASE + 90, BASE + 50)], BASE);
    let mut got = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut got, &[(BASE + 90, BASE + 50)], BASE);
    assert_eq!(got, vec![(0, 200)], "an inverted range carves nothing");

    // Wholly below the base: both ends saturate to 0, so it is empty and
    // dropped rather than becoming a huge wrapped cut.
    agree(&spans, &[(BASE - 400, BASE - 200)], BASE);
    let mut low = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut low, &[(BASE - 400, BASE - 200)], BASE);
    assert_eq!(low, vec![(0, 200)]);

    // Straddling the base: clipped to [0, e - base).
    agree(&spans, &[(BASE - 100, BASE + 30)], BASE);
    let mut straddle = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut straddle, &[(BASE - 100, BASE + 30)], BASE);
    assert_eq!(straddle, vec![(30, 170)]);
}

/// An empty skip list is the overwhelmingly common case and must be exactly
/// a no-op.
#[test]
fn an_empty_skip_list_changes_nothing() {
    let spans = vec![(0usize, 64usize), (128, 32), (4096, 8)];
    let mut got = spans.clone();
    ZgcRealHeap::withhold_skip_regions(&mut got, &[], BASE);
    assert_eq!(got, spans);
    // ...and so does a list of nothing but degenerate ranges.
    let mut got2 = spans.clone();
    ZgcRealHeap::withhold_skip_regions(&mut got2, &[(BASE + 8, BASE + 8)], BASE);
    assert_eq!(got2, spans);
}

/// Applying the same cuts twice must be a no-op the second time. The pass
/// runs once per cycle today, but idempotence is what says the output is a
/// genuine set difference rather than an operation with internal state --
/// and it is the property a future "clip the cursor too" caller will assume.
#[test]
fn carving_is_idempotent() {
    let spans = [(0usize, 300usize), (400, 100)];
    let skips = [(BASE + 20, BASE + 60), (BASE + 280, BASE + 420)];
    let mut once = spans.to_vec();
    ZgcRealHeap::withhold_skip_regions(&mut once, &skips, BASE);
    let mut twice = once.clone();
    ZgcRealHeap::withhold_skip_regions(&mut twice, &skips, BASE);
    assert_eq!(once, twice);
}

/// A deterministic pseudo-random sweep over many span/skip layouts.
///
/// No `rand` dependency and no seed from the clock: an xorshift with a
/// fixed seed, so a failure here reproduces exactly. This is what covers
/// the interleavings the named cases above do not enumerate -- in
/// particular the `partition_point` boundary, which is only interesting
/// when a cut ends exactly at a span's start or begins exactly at its end.
#[test]
fn randomised_layouts_agree_with_the_oracle() {
    let mut state: u64 = 0x5A47_4352_4C43_0001;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for _ in 0..600 {
        // Ascending, disjoint spans -- the shape the complement pass emits.
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut cursor = 0usize;
        let n = (next() % 6) as usize + 1;
        for _ in 0..n {
            cursor += (next() % 12) as usize;
            let len = (next() % 24) as usize + 1;
            spans.push((cursor, len));
            cursor += len;
        }
        // Skips in ABSOLUTE addresses, unsorted and free to overlap, which
        // is what `set_jit_tlab_skip_regions` allows.
        let mut skips: Vec<(usize, usize)> = Vec::new();
        let m = (next() % 5) as usize;
        for _ in 0..m {
            let s = (next() % 160) as usize;
            let e = s + (next() % 30) as usize;
            skips.push((BASE + s, BASE + e));
        }
        agree(&spans, &skips, BASE);
    }
}

// -- the page census ---------------------------------------------------

/// **A page with one survivor is not wholly dead, and a page with a
/// thousand corpses and no survivor is.**
///
/// The census is the measurement
/// `proposal-b-page-based-evacuation.md`'s O(pages) sweep argument rests
/// on, so the classification has to be exact at the boundary that matters:
/// ONE live object is the difference between a page a page-based heap
/// returns for free and a page it must still walk. The test therefore
/// builds a page whose only difference from a wholly-dead one is a single
/// mark bit.
#[test]
fn the_page_census_classifies_each_logical_page_by_its_survivors() {
    use super::super::starts::ZObjectStartBits;

    const BASE: usize = 0x1000_0000;
    let page = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    let pages = 3usize;
    let span = page * pages;
    // 512 arena bytes per bitmap word.
    let words_per_page = page >> 9;

    let marks = ZObjectStartBits::new(BASE, span);
    let mut words = vec![0u64; words_per_page * pages];
    let mut set_start = |addr: usize, words: &mut Vec<u64>| {
        let idx = (addr - BASE) >> 3;
        words[idx >> 6] |= 1u64 << (idx & 63);
    };

    // Page 0: two survivors and three corpses -> OCCUPIED.
    for k in 0..5usize {
        let addr = BASE + k * 64;
        set_start(addr, &mut words);
        if k < 2 {
            marks.insert(addr);
        }
    }
    // Page 1: five corpses, no survivor -> WHOLLY DEAD. Spread across the
    // page so the classification cannot come from one word being zero.
    for k in 0..5usize {
        set_start(BASE + page + k * 4096, &mut words);
    }
    // Page 2: nothing at all -> EMPTY.

    let snapshot = ZObjectStartsSnapshot {
        words: words.into(),
        base: BASE,
        extra: rustc_hash::FxHashSet::default(),
        covered: None,
        cached_base_count: None,
    };
    let n = snapshot.words.len().min(marks.word_count());
    let c = ZgcRealHeap::sweep_page_census(&snapshot, &marks, BASE, BASE + span, n);

    assert_eq!(c.pages, pages);
    assert_eq!(c.occupied, 1, "the page with survivors");
    assert_eq!(c.wholly_dead, 1, "the page with corpses and no survivor");
    assert_eq!(c.empty, 1, "the page nothing was ever allocated in");
    assert_eq!(c.occupied + c.wholly_dead + c.empty, c.pages, "exhaustive");
    assert_eq!(c.live_objects, 2);
    assert_eq!(c.dead_objects, 3 + 5);
    assert_eq!(
        c.dead_on_wholly_dead_pages, 5,
        "only page 1's corpses are on a page a page-based reclaim could return"
    );

    // THE BOUNDARY: mark ONE object on the wholly-dead page and it stops
    // being wholly dead. Nothing else about the heap changed.
    marks.insert(BASE + page);
    let c2 = ZgcRealHeap::sweep_page_census(&snapshot, &marks, BASE, BASE + span, n);
    assert_eq!(c2.wholly_dead, 0, "one survivor is enough to keep the page");
    assert_eq!(c2.occupied, 2);
    assert_eq!(c2.dead_on_wholly_dead_pages, 0);
    assert_eq!(c2.live_objects, 3);
    assert_eq!(c2.dead_objects, 3 + 4);
}

/// The census refuses a snapshot whose base is not the arena base rather
/// than attributing bases to the wrong page. Instrumentation that guesses
/// is worse than instrumentation that is absent -- the round's own record
/// is a census that was 42% high and carried the authority of having been
/// machine-checked.
#[test]
fn the_page_census_declines_a_snapshot_it_cannot_place() {
    use super::super::starts::ZObjectStartBits;

    const BASE: usize = 0x1000_0000;
    let page = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    let marks = ZObjectStartBits::new(BASE, page);
    let snapshot = ZObjectStartsSnapshot {
        words: vec![0u64; page >> 9].into(),
        base: BASE + 4096,
        extra: rustc_hash::FxHashSet::default(),
        covered: None,
        cached_base_count: None,
    };
    let c = ZgcRealHeap::sweep_page_census(
        &snapshot,
        &marks,
        BASE,
        BASE + page,
        snapshot.words.len(),
    );
    assert_eq!(c, ZSweepPageCensus::default());
}

/// Everything the complement pass reports that the region step could
/// change: the low free spans, the new cursor, the high-end holes, and
/// every counter.
#[derive(Debug, PartialEq, Eq)]
struct ComplementOutcome {
    spans: Vec<(usize, usize)>,
    cursor: usize,
    high_spans: Vec<(usize, usize)>,
    swept: usize,
    dead_count: usize,
    dead_headers_skipped: usize,
    bytes_copied: usize,
    objects_copied: usize,
    bytes_freed: usize,
    unsizable: usize,
    unsizable_high: usize,
}

/// Build a heap from `seed` (same seed, same layout: no TLABs, so every
/// allocation is a deterministic bump), mark a live set chosen in phases --
/// long all-dead stretches (whole 32 KiB blocks with no survivor), sparse
/// stretches (mostly-dead words), dense stretches -- plus huge arrays at
/// the large-object end, some live, and run the complement sweep with the
/// region step on or off. Returns the outcome (arena-relative) and the
/// region step's engagement.
fn complement_outcome(seed: u64, region: bool, workers: usize) -> (ComplementOutcome, usize) {
    use cratonvm_gc_core::collector::GarbageCollector;
    use cratonvm_types::{ArrayElementType, ClassId};
    let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
    heap.set_tlab_enabled(false);
    let marks = heap
        .mark_bits
        .as_ref()
        .expect("side mark bits are the default");
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let big = super::super::ZGC_LARGE_OBJECT_MIN;
    let mut phase_left = 0u64;
    let mut live_one_in = 0u64; // 0 = none live
    for i in 0..24_000u64 {
        if phase_left == 0 {
            phase_left = 50 + next() % 600;
            live_one_in = match next() % 4 {
                0 | 1 => 0, // an all-dead stretch
                2 => 40 + next() % 60,
                _ => 1 + next() % 3,
            };
        }
        phase_left -= 1;
        let o = if i % 4_001 == 4_000 {
            heap.alloc_array(
                ClassId::new(1),
                ArrayElementType::Byte,
                big + (next() % 4096) as usize,
            )
        } else {
            heap.alloc_object(ClassId::new(11), (next() % 40) as usize)
        };
        let live = live_one_in != 0 && next() % live_one_in == 0;
        if live {
            assert!(marks.claim(o.as_ptr() as usize));
        }
    }
    let cfg = {
        let arena = heap.arena.lock();
        ZSweepCfg {
            arena_base: arena.base_ptr() as usize,
            high_floor: arena.high_cursor(),
            arena_used: arena.used(),
            zero_header_only: true,
            skip_dead_headers: true,
            merge_dead_runs: true,
            bulk_clearable: true,
            want_dead: false,
            collect_dead_hashes: false,
            region_skip: region,
            gen_on: false,
            promo_age: 3,
            record_live_cells: false,
            reserve_live_hints: 0,
            size_memo: None,
            time_walk: false,
        }
    };
    let registered = heap.registry.snapshot();
    let (mut sh, spans, cursor) = heap
        .sweep_bitmap(&registered, &cfg, workers)
        .expect("the complement arm accepts this cycle");
    sh.flush();
    let skipped = sh.region_words_skipped;
    (
        ComplementOutcome {
            spans,
            cursor,
            high_spans: std::mem::take(&mut sh.spans),
            swept: sh.swept,
            dead_count: sh.dead_count,
            dead_headers_skipped: sh.dead_headers_skipped,
            bytes_copied: sh.bytes_copied,
            objects_copied: sh.objects_copied,
            bytes_freed: sh.bytes_freed,
            unsizable: sh.unsizable,
            unsizable_high: sh.unsizable_high.len(),
        },
        skipped,
    )
}

/// zr z5/sw: **the region step frees exactly what the word walk frees.**
/// On randomized heaps, serial and sharded (shard seams land inside
/// blocks and inside dead stretches), the free-span set, the cursor, the
/// large-object end's holes and every counter are identical with the step
/// on and off -- and the step really engaged (positive control), so the
/// equality is not two runs of the same code.
#[test]
fn sweep_region_step_matches_the_word_walk() {
    let mut engaged = 0usize;
    for seed in [0x9E37_79B9_7F4A_7C15u64, 7, 0xDEAD_BEEF, 12_345_678_901] {
        for workers in [1usize, 3] {
            let (off, off_skipped) = complement_outcome(seed, false, workers);
            let (on, on_skipped) = complement_outcome(seed, true, workers);
            assert_eq!(off_skipped, 0, "the step must not run when it is off");
            assert_eq!(
                on, off,
                "seed {seed:#x}, {workers} worker(s): the region step changed the sweep"
            );
            assert!(
                !on.spans.is_empty() && on.dead_count > 0,
                "a heap with garbage"
            );
            engaged += on_skipped;
        }
    }
    assert!(
        engaged > 0,
        "positive control: some block must have been skipped"
    );
}

/// The step must stand down whenever a per-dead-object obligation is
/// owed: with the header store on, or a dead list wanted, the outcome is
/// the word walk's and the counter stays 0.
#[test]
fn the_region_step_stands_down_when_dead_objects_are_owed_work() {
    use cratonvm_gc_core::collector::GarbageCollector;
    use cratonvm_types::ClassId;
    let heap = ZgcRealHeap::with_capacity(4 * 1024 * 1024);
    heap.set_tlab_enabled(false);
    for _ in 0..2_000 {
        heap.alloc_object(ClassId::new(11), 30); // all dead: whole empty blocks
    }
    for (skip_headers, want_dead) in [(false, false), (true, true)] {
        let cfg = {
            let arena = heap.arena.lock();
            ZSweepCfg {
                arena_base: arena.base_ptr() as usize,
                high_floor: arena.high_cursor(),
                arena_used: arena.used(),
                zero_header_only: true,
                skip_dead_headers: skip_headers,
                merge_dead_runs: true,
                bulk_clearable: true,
                want_dead,
                collect_dead_hashes: false,
                region_skip: true,
                gen_on: false,
                promo_age: 3,
                record_live_cells: false,
                reserve_live_hints: 0,
                size_memo: None,
                time_walk: false,
            }
        };
        let registered = heap.registry.snapshot();
        let marks = heap.mark_bits.as_ref().expect("side mark bits");
        let words = registered.words.len().min(marks.word_count());
        let base = cfg.arena_base;
        let shard = heap.sweep_bitmap_range(
            &registered,
            &cfg,
            marks,
            0,
            words,
            base,
            base.saturating_add(cfg.high_floor),
        );
        assert_eq!(
            shard.sh.region_words_skipped, 0,
            "skip_headers={skip_headers} want_dead={want_dead}: dead objects are owed \
             per-object work, so no block may be skipped"
        );
        assert_eq!(shard.sh.dead_count, 2_000);
        if want_dead {
            assert_eq!(shard.sh.dead.len(), 2_000, "the list is complete");
        }
    }
}
