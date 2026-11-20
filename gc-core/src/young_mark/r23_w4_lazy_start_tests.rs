use super::*;

const BASE: usize = 0x10_0000;
const SPAN: usize = 0x4000;

/// A synthetic chunk walker: one start every 64 bytes outside the skip
/// blocks; the chunk holding offset 0x3000 records one start and fails.
fn every_64(
    base: usize,
    lo: usize,
    hi: usize,
    skips: &[(usize, usize)],
    bits: &ObjectStartBits,
) -> bool {
    if lo <= 0x3000 && 0x3000 < hi {
        bits.insert(base + lo);
        return false;
    }
    let mut o = lo;
    while o < hi {
        if !skips.iter().any(|&(s, l)| o >= s && o < s + l) {
            bits.insert(base + o);
        }
        o += 64;
    }
    true
}

fn grid(skips: Vec<(usize, usize)>) -> ObjectStartBits {
    ObjectStartBits::new_lazy(
        BASE,
        SPAN,
        vec![0, 0x1000, 0x2000, 0x3000, SPAN],
        skips,
        every_64,
    )
    .expect("a usable grid")
}

#[test]
fn r23_w4_contains_walks_only_the_chunk_it_asks_about() {
    let bits = grid(Vec::new());
    assert!(bits.is_lazy());
    assert_eq!(bits.lazy_census(), Some((4, 0, 0, 0)));
    assert!(bits.contains(BASE + 0x1040));
    assert!(
        !bits.contains(BASE + 0x1048),
        "an interior word is not a start"
    );
    // Certified anchors (the default): chunk 1 answers on its own.
    assert_eq!(
        bits.lazy_census(),
        Some((4, 1, 0, 0x1000)),
        "one chunk, walked once"
    );
    assert!(!bits.contains(BASE + SPAN), "past the span");
    assert_eq!(bits.lazy_census(), Some((4, 1, 0, 0x1000)));
}

#[test]
fn r23_w4_a_failed_chunk_answers_not_a_start_and_keeps_no_bits() {
    let bits = grid(Vec::new());
    assert!(
        !bits.contains(BASE + 0x3000),
        "the refused chain's own bit is withdrawn"
    );
    assert!(!bits.ensure_addr(BASE + 0x3040));
    assert!(!bits.ensure_span(BASE, BASE + SPAN));
    assert!(
        bits.ensure_span(BASE, BASE + 0x3000),
        "the chunks below are fine"
    );
    let (_, walked, failed, _) = bits.lazy_census().unwrap();
    assert_eq!((walked, failed), (3, 1));
    assert_eq!(
        bits.next_start_at_or_above(BASE + 0x2fc0),
        Some(BASE + 0x2fc0)
    );
    assert_eq!(
        bits.next_start_at_or_above(BASE + 0x2fc8),
        None,
        "nothing above in a failed chunk"
    );
    assert_eq!(
        bits.nearest_start_at_or_below(BASE + 0x3010),
        Some(BASE + 0x2fc0)
    );
    assert!(
        bits.ensure_addr(BASE + SPAN),
        "no chunk holds an address past the span"
    );
}

#[test]
fn r23_w4_lazy_scans_cross_unwalked_chunks() {
    let bits = grid(vec![(0x1000, 0x1000)]);
    // From chunk 0's last start up across the all-skip chunk 1 into chunk 2.
    assert_eq!(
        bits.next_start_at_or_above(BASE + 0xfc8),
        Some(BASE + 0x2000)
    );
    assert_eq!(
        bits.nearest_start_at_or_below(BASE + 0x1800),
        Some(BASE + 0xfc0)
    );
    assert!(!bits.contains(BASE + 0x1040), "a skip block holds no start");
}

#[test]
fn r23_w4_one_walk_per_chunk_under_concurrent_readers() {
    let bits = grid(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for i in 0..64 {
                    assert!(bits.contains(BASE + i * 64));
                }
            });
        }
    });
    assert_eq!(bits.lazy_census(), Some((4, 1, 0, 0x1000)));
}

#[test]
fn r23_w4_unusable_grids_are_refused_and_duplicates_are_eager() {
    assert!(
        ObjectStartBits::new_lazy(BASE, SPAN, vec![0, SPAN - 8], Vec::new(), every_64)
            .is_none()
    );
    assert!(
        ObjectStartBits::new_lazy(BASE, SPAN, vec![8, SPAN], Vec::new(), every_64).is_none()
    );
    assert!(ObjectStartBits::new_lazy(
        BASE,
        SPAN,
        vec![0, 0x1000, 0x1000, SPAN],
        Vec::new(),
        every_64
    )
    .is_none());
    assert!(ObjectStartBits::new_lazy(BASE, SPAN, vec![0], Vec::new(), every_64).is_none());
    let bits = grid(Vec::new());
    let copy = bits.duplicate();
    assert!(!copy.is_lazy());
    assert!(copy.contains(BASE + 0x2040) && !copy.contains(BASE + 0x3000));
    assert_eq!(
        copy.len(),
        3 * 64,
        "three walked chunks of 64 starts; the failed one has none"
    );
    assert!(
        ObjectStartBits::new(BASE, SPAN).ensure_span(BASE, BASE + SPAN),
        "eager bitmaps need nothing"
    );
}

/// Wave 4c: chunk 1 fails; chunk 2 alone would walk and land.
fn fails_chunk_1(
    base: usize,
    lo: usize,
    hi: usize,
    _skips: &[(usize, usize)],
    bits: &ObjectStartBits,
) -> bool {
    if lo == 0x1000 {
        return false;
    }
    let mut o = lo;
    while o < hi {
        bits.insert(base + o);
        o += 64;
    }
    true
}

/// Wave 4c (review: the lost anchor induction): a chunk whose own chain
/// lands is still refused when a chunk below it cannot be proven -- its
/// anchor is then not known to be an object start -- and a speculative
/// walk's bits are never answered nor copied.
#[test]
fn r23_w4c_a_chunk_above_an_unprovable_one_never_answers() {
    let mut bits = ObjectStartBits::new_lazy(
        BASE,
        SPAN,
        vec![0, 0x1000, 0x2000, 0x3000, SPAN],
        Vec::new(),
        fails_chunk_1,
    )
    .expect("a usable grid");
    bits.set_lazy_induction(true);
    assert!(bits.walk_chunk_index(2), "chunk 2's own chain lands");
    assert!(!bits.contains(BASE + 0x2040), "but its anchor is unproven");
    assert!(bits.contains(BASE + 0x40), "chunk 0 is proven from offset 0");
    assert!(!bits.ensure_chunk_index(3));
    assert_eq!(bits.next_start_at_or_above(BASE + 0xfc8), None);
    assert_eq!(bits.nearest_start_at_or_below(BASE + 0x2040), Some(BASE + 0xfc0));
    assert_eq!(bits.lazy_chunk_of_addr(BASE + 0x2040), Some(2));
    let copy = bits.duplicate();
    assert!(!copy.contains(BASE + 0x2040), "speculative bits are withdrawn");
    assert_eq!(copy.len(), 0x1000 / 64, "chunk 0 only");
}

/// Wave 4c: a walker whose chain from 0x1000 misses the bound 0x2000 (a
/// bound that is not a boundary) but lands on anything else.
fn misses_bound_0x2000(
    base: usize,
    lo: usize,
    hi: usize,
    _skips: &[(usize, usize)],
    bits: &ObjectStartBits,
) -> bool {
    if lo == 0x1000 && hi == 0x2000 {
        return false;
    }
    let mut o = lo;
    while o < hi {
        bits.insert(base + o);
        o += 64;
    }
    true
}

fn missing_bound_grid() -> ObjectStartBits {
    ObjectStartBits::new_lazy(
        BASE,
        SPAN,
        vec![0, 0x1000, 0x2000, 0x3000, SPAN],
        Vec::new(),
        misses_bound_0x2000,
    )
    .expect("a usable grid")
}

/// Wave 4c: a chain that misses its end bound is walked on to the span's
/// end as one merged chain (the eager walk's sequential fallback), and
/// every chunk it covered answers from it.
#[test]
fn r23_w4c_a_missed_end_bound_is_walked_through() {
    let bits = missing_bound_grid();
    assert!(bits.contains(BASE + 0x1040));
    assert_eq!(bits.lazy_merged_walks(), 1);
    assert!(bits.contains(BASE + 0x2040) && bits.contains(BASE + 0x3fc0));
    assert_eq!(bits.lazy_merged_walks(), 1, "chunks 2 and 3 came with the merge");
    assert_eq!(bits.lazy_census().map(|c| c.2), Some(1), "chunk 1's own chain failed once");
}

/// Wave 4c: the merge stops at the first DONE chunk above it and must land
/// there; here it cannot (the walker refuses `[0x1000, 0x2000)` however it
/// is asked), so chunk 1 is unprovable, chunk 2 keeps its answer.
#[test]
fn r23_w4c_a_merge_never_rewalks_a_done_chunk() {
    let bits = missing_bound_grid();
    assert!(bits.contains(BASE + 0x2040), "chunk 2 first, on its own");
    assert!(!bits.contains(BASE + 0x1040));
    assert!(!bits.ensure_addr(BASE + 0x1040), "DEAD: not retried");
    assert_eq!(bits.lazy_merged_walks(), 1);
    assert!(bits.contains(BASE + 0x2040), "chunk 2 still answers");
}
