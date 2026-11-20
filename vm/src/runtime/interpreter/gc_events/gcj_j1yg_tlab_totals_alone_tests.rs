use super::*;

/// A `gc+tlab=debug` rule arms the collection's events on its own only on
/// a Generational heap and only with the kill switch unset.
#[test]
fn gcj_j1yg_a_tlab_only_spec_arms_events_on_generational_only() {
    assert!(tlab_totals_alone_arms_events(true, true, true));
    assert!(!tlab_totals_alone_arms_events(false, true, true), "G1 / ZGC unchanged");
    assert!(!tlab_totals_alone_arms_events(true, false, true), "no gc+tlab rule");
    assert!(!tlab_totals_alone_arms_events(true, true, false), "the =0 kill");
}


/// The collection's start asks the new arm beside `gc_events_wanted`, and
/// before the `TLAB totals` stash reads `events`.
#[test]
fn gcj_j1yg_gc_event_start_ors_the_tlab_only_arm_before_the_stash() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let start = src
        .find("pub(super) fn gc_event_start(")
        .expect("gc_event_start");
    let body = &src[start..];
    let arm = body
        .find("(gc_events_wanted() || xlog_tlab_totals_alone_wanted(shared))")
        .expect("the tlab-only arm");
    let stash = body
        .find("stash_tlab_totals(shared, events)")
        .expect("the stash");
    assert!(arm < stash);
}
