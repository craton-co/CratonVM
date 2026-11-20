use super::*;

#[test]
fn tableswitch_single_case_is_one() {
    // `high == low`: exactly one case, JVMS allows it.
    assert_eq!(checked_tableswitch_count(0, 0), Some(1));
    assert_eq!(checked_tableswitch_count(5, 5), Some(1));
}

#[test]
fn tableswitch_high_below_low_is_refused() {
    // `count = high - low + 1 <= 0` — an empty or negative span, which a
    // well-formed tableswitch never has. Must refuse, not wrap or emit a
    // zero/negative-size table.
    assert_eq!(checked_tableswitch_count(5, 3), None);
    assert_eq!(checked_tableswitch_count(5, 4), None); // count == 0
}

#[test]
fn tableswitch_full_i32_span_exceeds_the_cap() {
    // The widest legal `low`/`high` pair: span is ~2^32, far past
    // `MAX_TABLESWITCH_COUNT` (2^24). Must refuse rather than allocate a
    // multi-GiB jump table for adversarial bytecode.
    assert_eq!(checked_tableswitch_count(i32::MIN, i32::MAX), None);
}

#[test]
fn tableswitch_cap_boundary() {
    let low = 0;
    let at_cap = i32::try_from(MAX_TABLESWITCH_COUNT - 1).expect("cap fits i32");
    let over_cap = i32::try_from(MAX_TABLESWITCH_COUNT).expect("cap fits i32");
    assert_eq!(
        checked_tableswitch_count(low, at_cap),
        Some(MAX_TABLESWITCH_COUNT)
    );
    assert_eq!(checked_tableswitch_count(low, over_cap), None);
}

#[test]
fn lookupswitch_negative_npairs_is_refused() {
    assert_eq!(checked_lookupswitch_npairs(-1), None);
    assert_eq!(checked_lookupswitch_npairs(i32::MIN), None);
}

#[test]
fn lookupswitch_zero_npairs_is_a_default_only_switch() {
    assert_eq!(checked_lookupswitch_npairs(0), Some(0));
}

#[test]
fn lookupswitch_cap_boundary() {
    let at_cap = i32::try_from(MAX_LOOKUPSWITCH_NPAIRS).expect("cap fits i32");
    let over_cap = at_cap + 1;
    assert_eq!(
        checked_lookupswitch_npairs(at_cap),
        Some(MAX_LOOKUPSWITCH_NPAIRS)
    );
    assert_eq!(checked_lookupswitch_npairs(over_cap), None);
}

#[test]
fn lookupswitch_i32_max_exceeds_the_cap() {
    assert_eq!(checked_lookupswitch_npairs(i32::MAX), None);
}
