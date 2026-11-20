use super::zgc_explicit_commit_refusal_lines as lines;

/// zki: the halving loop's refusals against an empty pool. One granule:
/// the whole request only; two: the whole, then one (host row `zki_f3`
/// printed two lines for its refused page).
#[test]
fn zki_refused_requests_count_hotspots_halving_steps() {
    assert_eq!(lines(0), 0);
    assert_eq!(lines(1), 1);
    assert_eq!(lines(2), 2);
    assert_eq!(lines(3), 3);
    assert_eq!(lines(4), 3);
    assert_eq!(lines(63), 7);
}
