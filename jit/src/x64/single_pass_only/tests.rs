use super::*;

/// The module header's spelled-out count is the real one.
///
/// The header said "The seven below" while [`SinglePassOnly::ALL`] held
/// six, because loop unswitching was dropped from the list (2026-09-12)
/// and the sentence above it was not. That is precisely the failure this
/// file's own header warns about — "the comment written to prevent the
/// drift was the drift" — so the number is read out of the source and
/// compared, not maintained by hand.
///
/// Only the English numerals this header could plausibly use are
/// recognised; a count past `ten` fails loudly rather than silently
/// passing, which is the right direction for a list that is supposed to
/// stay small enough to enumerate by reading.
#[test]
fn the_prose_count_matches_all() {
    const NUMERALS: [&str; 11] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    let src = include_str!("../single_pass_only.rs");
    // Split the needle so this literal is not itself a match: the file
    // contains `" below were "` and `"found by reading every list"` as two
    // adjacent tokens, never as the contiguous sentence the header has.
    let needle = concat!(" below were ", "found by reading every list");
    let (before, _) = src.split_once(needle).expect(
        "the header sentence that counts the variants is gone; either \
         restore it or delete this test deliberately",
    );
    let spelled = before
        .rsplit(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
        .expect("a word precedes ` below were found`")
        .to_ascii_lowercase();
    let n = SinglePassOnly::ALL.len();
    let expected = NUMERALS.get(n).unwrap_or_else(|| {
        panic!(
            "ALL now has {n} variants, past the numerals this test knows; \
             add one to NUMERALS and update the header"
        )
    });
    assert_eq!(
        &spelled, expected,
        "the module header says \"{spelled}\" but `SinglePassOnly::ALL` has \
         {n} variants — update the header sentence"
    );
}

/// A variant that exists but is missing from `ALL` would be silently
/// exempt from the veto — the exact shape of the bug this module is for,
/// one level down. `ordinal` is exhaustive, so a new variant does not
/// compile until it has an index; this makes that index mean something.
#[test]
fn all_variants_are_registered() {
    for (i, k) in SinglePassOnly::ALL.iter().enumerate() {
        assert_eq!(
            k.ordinal(),
            i,
            "{k:?} is at index {i} of ALL but its ordinal is {}; ALL and \
             ordinal() have drifted",
            k.ordinal()
        );
    }
    assert_eq!(
        SinglePassOnly::ALL.len(),
        SinglePassOnly::ALL
            .iter()
            .map(|k| k.ordinal())
            .max()
            .map_or(0, |m| m + 1),
        "ALL has a gap: some variant's ordinal is not covered"
    );
}

#[test]
fn every_variant_has_a_distinct_label() {
    let mut labels: Vec<&str> = SinglePassOnly::ALL.iter().map(|k| k.label()).collect();
    labels.sort_unstable();
    let before = labels.len();
    labels.dedup();
    assert_eq!(before, labels.len(), "two variants share a label");
}

/// The regression this whole module exists for. If the veto stops firing
/// on `CratonBench.sieve`, the IR tier takes the method back and the phase
/// goes 2,462 ms -> 15,823 ms with no test failing and no checksum
/// changing, which is precisely how it shipped the first time.
#[test]
fn the_sieve_that_cost_6x_is_vetoed_and_named() {
    // The exact javac shape of `CratonBench.sieve(boolean[], int)`.
    let sieve = [
        0x03, 0x3d, // 0: int i/count = 0
        0x1c, 0x1b, 0xa3, 0x00, 0x0d, // 2: clear-loop header -> 17
        0x2a, 0x1c, 0x03, 0x54, // 7: a[i] = 0
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xf4, // 11: i++; goto 2
        0x03, 0x3d, 0x05, 0x3e, // 17: count=0; outer i=2
        0x1d, 0x1b, 0xa3, 0x00, 0x2b, // 21: outer header -> 66
        0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 26: if (a[i]) -> 60
        0x84, 0x02, 0x01, // 32: count++
        0x1d, 0x1d, 0x60, 0x36, 0x04, // 35: j=i+i
        0x15, 0x04, 0x1b, 0xa3, 0x00, 0x11, // 40: inner header -> 60
        0x2a, 0x15, 0x04, 0x04, 0x54, // 46: a[j] = 1
        0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 51: j += i
        0xa7, 0xff, 0xef, // 57: goto 40
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xd6, // 60: i++; goto 21
        0x1c, 0xac, // 66: return count
        0x00, 0x00,
    ];
    let hit = single_pass_only_lowering(&sieve, 68, &[]);
    assert!(
        hit.is_some(),
        "the veto must fire on CratonBench.sieve — without it the IR tier \
         takes this method and emits a scalar loop 6.4x slower than the \
         vectorised one the single-pass backend already emits"
    );
    // Naming it is not decoration: the admission line is what a reader of
    // a perf-gate results directory sees, and "declined" without "why" is
    // what made the original regression take a bisect to find.
    assert!(
        !hit.unwrap().label().is_empty(),
        "a veto that cannot say which lowering it protected is a veto \
         nobody can audit"
    );
}

/// The other half, and the one that would catch a veto gone greedy: a
/// method with no loop at all keeps its IR body. Without this the module
/// would still pass every other test while refusing everything.
#[test]
fn a_loopless_method_is_not_vetoed() {
    // iconst_0; ireturn
    assert_eq!(single_pass_only_lowering(&[0x03, 0xac], 2, &[]), None);
}

/// And a method that loops but over nothing the single-pass backend
/// specialises: `for (int i = 0; i < n; i++) {}` — a bare counted loop
/// with an empty body.
#[test]
fn a_plain_counted_loop_is_not_vetoed() {
    let code = [
        0x03, 0x3c, // 0: int i = 0
        0x1b, 0x1a, 0xa2, 0x00, 0x08, // 2: if (i >= n) -> 10
        0x84, 0x01, 0x01, // 7: i++
        0xa7, 0xff, 0xf8, // 10 (back edge): goto 2
        0xb1, // 13: return
    ];
    assert_eq!(single_pass_only_lowering(&code, 14, &[]), None);
}

/// `static long s(int[] a) { long s = 0; int n = a.length;
/// for (int i = 0; i < n; i++) s = a[i] + s; return s; }` (javac; the
/// historical sum form, detected with or without the vectorize switch).
/// Locals: a=0, (unused 1), s=2..3, n=4, i=5.
const SUM_ONLY: [u8; 32] = [
    0x09, 0x41, // 0: s = 0L
    0x2a, 0xbe, 0x36, 0x04, // 2: n = a.length
    0x03, 0x36, 0x05, // 6: i = 0
    0x15, 0x05, 0x15, 0x04, 0xa2, 0x00, 0x11, // 9: if (i >= n) -> 30
    0x2a, 0x15, 0x05, 0x2e, 0x85, 0x20, 0x61, 0x41, // 16: s = a[i] + s
    0x84, 0x05, 0x01, // 24: i++
    0xa7, 0xff, 0xee, // 27: goto 9
    0x20, 0xad, // 30: return s
];

/// [`SUM_ONLY`] followed by a sibling scalar loop,
/// `for (int j = 0; j < k; j++) s = s * 31 + j;` (k = local 1, j = 6).
const SUM_AND_SIBLING: [u8; 57] = [
    0x09, 0x41, // 0: s = 0L
    0x2a, 0xbe, 0x36, 0x04, // 2: n = a.length
    0x03, 0x36, 0x05, // 6: i = 0
    0x15, 0x05, 0x15, 0x04, 0xa2, 0x00, 0x11, // 9: if (i >= n) -> 30
    0x2a, 0x15, 0x05, 0x2e, 0x85, 0x20, 0x61, 0x41, // 16: s = a[i] + s
    0x84, 0x05, 0x01, // 24: i++
    0xa7, 0xff, 0xee, // 27: goto 9
    0x03, 0x36, 0x06, // 30: j = 0
    0x15, 0x06, 0x1b, 0xa2, 0x00, 0x13, // 33: if (j >= k) -> 55
    0x20, 0x10, 0x1f, 0x85, 0x69, // 39: s * 31L
    0x15, 0x06, 0x85, 0x61, 0x41, // 44: s = .. + j
    0x84, 0x06, 0x01, // 49: j++
    0xa7, 0xff, 0xed, // 52: goto 33
    0x20, 0xad, // 55: return s
];

/// Round 9 wave 4: the SIMD families veto the IR tier only when the
/// vectorised loops carry the method's loop work (`dominate`). A sibling
/// scalar loop keeps the IR body (measured: `LoopProbe mixed`, 2,032 ms
/// with its IR body against 2,402 ms vetoed).
#[test]
fn a_sibling_scalar_loop_keeps_the_ir_body() {
    if !has_avx2() || !c1_vector_veto_enabled() || no_bce() {
        return;
    }
    assert_eq!(
        detect_loops(&SUM_ONLY, SUM_ONLY.len()),
        vec![(9, 27)],
        "fixture: one loop"
    );
    assert_eq!(
        single_pass_only_lowering(&SUM_ONLY, SUM_ONLY.len(), &[]),
        Some(SinglePassOnly::SimdIntArraySum),
        "a method that is its vectorised sum keeps the single-pass body"
    );
    assert_eq!(
        detect_loops(&SUM_AND_SIBLING, SUM_AND_SIBLING.len()),
        vec![(9, 27), (33, 52)],
        "fixture: two sibling loops"
    );
    assert_eq!(
        single_pass_only_lowering(&SUM_AND_SIBLING, SUM_AND_SIBLING.len(), &[]),
        None,
        "a vectorised sum next to a scalar loop must not veto the IR tier"
    );
}

/// The dominance rule itself, host-independent.
#[test]
fn vectorised_loops_dominate_only_their_own_nest() {
    let simd = CoveredSimdLoops {
        sums: vec![(9, 27)],
        element_wise: Vec::new(),
    };
    assert!(simd.dominate(&[(9, 27)]), "the loop itself");
    assert!(
        simd.dominate(&[(2, 40), (9, 27)]),
        "an outer loop that only repeats it"
    );
    assert!(
        !simd.dominate(&[(9, 27), (33, 52)]),
        "a sibling scalar loop"
    );
    let ewise = CoveredSimdLoops {
        sums: Vec::new(),
        element_wise: vec![(33, 52)],
    };
    assert!(
        ewise.dominate(&[(33, 52)]) && !ewise.dominate(&[(9, 27), (33, 52)]),
        "the element-wise family follows the same rule"
    );
}
