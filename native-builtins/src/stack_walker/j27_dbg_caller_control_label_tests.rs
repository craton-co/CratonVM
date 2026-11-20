//! j27 wave 7 (lane T1ACCEPT): a fast-answered ask's control walk is not
//! counted as a miss by `grep '(security walk)'`.
use super::dbg_walk_label;

#[test]
fn a_control_walk_is_labelled_apart_from_a_miss() {
    let miss = format!("({}):", dbg_walk_label(false));
    let control = format!("({}):", dbg_walk_label(true));
    assert_eq!(miss, "(security walk):");
    assert!(!control.contains("(security walk)"), "{control}");
    assert_eq!(control, "(security walk, control):");
}
