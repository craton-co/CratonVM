use super::*;
use cratonvm_jit::ir_schedule::PairCensus;

/// **Every bucket `PairCensus` counts must appear in the printed line.**
///
/// Asked as arithmetic rather than as a list of field names, because a list
/// of field names in a test is a third hand-maintained enumeration and
/// would have been forgotten alongside the other two. `closes()` is the
/// struct's own statement that the buckets partition `candidates`; this
/// re-derives the same sum from the TEXT, so a bucket that exists in the
/// struct and not in the format string makes the printed numbers add up to
/// less than the printed denominator.
///
/// That is exactly the defect this line shipped with: `memory_order` was
/// counted, asserted and not displayed. Deleting `memory_order={}` (and its
/// argument) from `pair_census_line` fails this test with `48 != 65`.
#[test]
fn the_pairing_line_accounts_for_every_candidate() {
    // Distinct primes so that a transposed pair of arguments — the other
    // way a hand-enumerated format string goes wrong — cannot cancel out.
    let declined = [2usize, 3, 5, 7, 11, 13, 17];
    let paired = 7usize;
    let pc = PairCensus {
        candidates: paired + declined.iter().sum::<usize>(),
        paired,
        multi_use: declined[0],
        producer_arm: declined[1],
        other_block: declined[2],
        after_consumer: declined[3],
        already_adjacent: declined[4],
        deopt_between: declined[5],
        memory_order: declined[6],
        no_node: 0,
        too_far: 0,
    };
    assert!(
        pc.closes(),
        "the fixture itself must satisfy the identity the line reports"
    );

    let line = pair_census_line(&pc);
    // `name=NUMBER`, every occurrence, in the order printed.
    let printed: Vec<(String, usize)> = line
        .split_whitespace()
        .filter_map(|tok| tok.split_once('='))
        .filter_map(|(k, v)| v.parse::<usize>().ok().map(|n| (k.to_string(), n)))
        .collect();
    let get = |name: &str| -> usize {
        printed
            .iter()
            .find(|(k, _)| k == name)
            .unwrap_or_else(|| panic!("`{name}` is not printed by `pair_census_line`: {line}"))
            .1
    };
    let denominator = get("candidates");
    let sum: usize = printed
        .iter()
        .filter(|(k, _)| k != "candidates")
        .map(|(_, n)| n)
        .sum();
    assert_eq!(
        sum, denominator,
        "the printed buckets do not add up to the printed denominator, so \
         the line omits one that `PairCensus::closes` counts: {line}"
    );
    // And the values landed against the right names, which the sum alone
    // cannot show.
    assert_eq!(get("memory_order"), declined[6], "{line}");
    assert_eq!(get("no_node"), 0, "{line}");
    assert_eq!(get("paired"), paired, "{line}");
}
