use super::*;

/// A run-stored snapshot reads like the full-length one: the same word at
/// every index, zero outside the runs, and the same bases (2026-10-07).
#[test]
fn a_run_snapshot_reads_like_the_full_copy() {
    const BASE: usize = 0x4000_0000;
    let span = 4 * 1024 * 1024;
    let starts = ZObjectStarts::with_bitmap(BASE, span, true);
    for off in [0usize, 8, 4096, 64 * 1024] {
        starts.insert(BASE + off);
    }
    starts.insert(BASE + span - 64);
    let ranges = [(BASE, BASE + 128 * 1024), (BASE + span - 4096, BASE + span)];
    let bounded = starts.snapshot_within(&ranges);
    let full = starts.snapshot();
    assert_eq!(bounded.words.len(), full.words.len());
    for w in 0..full.words.len() {
        assert_eq!(bounded.words[w], full.words[w], "word {w}");
        assert_eq!(bounded.words.get(w), full.words.get(w));
    }
    assert_eq!(bounded.words.get(full.words.len()), None);
    assert_eq!(bounded.bases(), full.bases());
    assert_eq!(bounded.base_count(), 5);
}

/// zt t26/pc1: the direct walk names the same list as the snapshot route it
/// replaces on the slide's live list -- covered words ascending, a base in
/// a partially covered edge word included, the spill appended and the
/// whole list sorted -- on both arms, and allocates it at its exact length.
#[test]
fn the_direct_live_list_is_the_snapshot_list() {
    const BASE: usize = 0x4000_0000;
    let span = 4 * 1024 * 1024;
    for bitmap in [true, false] {
        let starts = ZObjectStarts::with_bitmap(BASE, span, bitmap);
        // 128 KiB + 8 is past the second range's end but in the word that
        // covers its last byte (a word is 512 arena bytes), so both routes
        // must include it. Nothing goes in the uncovered middle: debug
        // builds assert it is clear (`debug_assert_clear_outside`).
        for off in [0usize, 8, 4096, 64 * 1024, 128 * 1024 + 8] {
            starts.insert(BASE + off);
        }
        starts.insert(BASE + span - 64);
        // Off the 8-byte grid: the bitmap arm spills it.
        starts.insert(BASE + 4100);
        let ranges = [
            (BASE + span - 4096, BASE + span),
            (BASE, BASE + 128 * 1024 + 1),
        ];
        let via_snapshot = starts.snapshot_within(&ranges).bases();
        let direct = starts.bases_within(&ranges);
        assert_eq!(direct, via_snapshot, "bitmap arm = {bitmap}");
        assert!(direct.is_sorted(), "bitmap arm = {bitmap}");
        // Seven bases on both arms (on the bitmap arm the off-grid one
        // comes from the spill), and the bitmap arm sized its list exactly.
        assert_eq!(direct.len(), 7, "bitmap arm = {bitmap}");
        if bitmap {
            assert_eq!(direct.capacity(), direct.len());
        }
    }
}

fn snap(covered: Option<Vec<(usize, usize)>>) -> ZObjectStartsSnapshot {
    let mut words = vec![0u64; 16];
    words[3] = 0b101;
    words[11] = 0b1;
    ZObjectStartsSnapshot {
        words: words.into(),
        base: 0x10_0000,
        extra: FxHashSet::default(),
        covered,
        cached_base_count: None,
    }
}

/// r16 zgcarray (ZA3): the per-cycle passes stream only the covered
/// words, and the sweep's shards split the covered words, not the raw
/// range whose middle is all zero.
#[test]
fn covered_segments_bound_every_pass_and_split_evenly() {
    let s = snap(Some(vec![(2, 4), (10, 12)]));
    assert_eq!(s.segments(3, 11), vec![(3, 4), (10, 11)]);
    assert_eq!(s.segments(0, 99), vec![(2, 4), (10, 12)]);
    assert_eq!(s.covered_word_count(), 4);
    assert_eq!(s.split_covered(2), vec![(2, 4), (10, 12)]);
    assert_eq!(s.split_covered(1), vec![(2, 12)]);
    assert_eq!(s.split_covered(8).len(), 4);
    assert_eq!(s.base_count(), 3);
    let mut seen = Vec::new();
    s.for_each_base(|b| seen.push(b));
    assert_eq!(seen, s.bases());
    assert_eq!(seen.len(), 3);

    // Unbounded: every word, as before.
    let full = snap(None);
    assert_eq!(full.segments(0, 16), vec![(0, 16)]);
    assert_eq!(full.base_count(), 3);
    assert_eq!(
        full.split_covered(4),
        vec![(0, 4), (4, 8), (8, 12), (12, 16)]
    );
}
