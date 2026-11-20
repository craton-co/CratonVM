use super::*;

/// The site counters start at zero and are independent, so a run that
/// reports `Long.valueOf=0 Long.longValue=N` is reporting two facts and not
/// one number twice. `the field site cache shipped default-OFF for 13 days`
/// is what this row is for: "the bind did not help" and "the bind never
/// happened" have to be distinguishable from the log line alone.
#[test]
fn the_two_long_site_counters_are_separate() {
    let (before_v, before_l) = long_box_direct_helper_sites();
    LONG_VALUE_OF_SITES.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
    let (after_v, after_l) = long_box_direct_helper_sites();
    assert_eq!(after_v, before_v + 3);
    assert_eq!(
        after_l, before_l,
        "bumping valueOf moved the longValue tally"
    );
    LONG_LONG_VALUE_SITES.fetch_add(5, std::sync::atomic::Ordering::Relaxed);
    let (final_v, final_l) = long_box_direct_helper_sites();
    assert_eq!(final_v, before_v + 3);
    assert_eq!(final_l, before_l + 5);
}
