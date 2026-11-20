use super::bound_site_build as build;

#[test]
fn a_bound_static_site_is_an_unguarded_invokestatic() {
    assert_eq!(build(3, true, 2, false), Some((0xb8, false)));
    assert_eq!(build(3, true, 1, false), Some((0xb8, false)), "only the bound value");
}

#[test]
fn a_bound_virtual_site_keeps_its_guarded_invokevirtual() {
    assert_eq!(build(0, false, 2, false), Some((0xb6, true)));
    assert_eq!(build(1, false, 1, false), Some((0xb6, true)), "after a CHA re-mark");
}

#[test]
fn a_kind_the_plan_did_not_name_is_refused() {
    // A static row the plan took for a bound virtual site would get the
    // bound value as a receiver, and the other way round.
    assert_eq!(build(3, false, 2, false), None);
    assert_eq!(build(0, true, 2, false), None);
    assert_eq!(build(1, true, 2, false), None);
    assert_eq!(build(2, false, 2, false), None);
    assert_eq!(build(2, true, 2, false), None);
}

#[test]
fn no_argument_or_an_interface_site_is_refused() {
    assert_eq!(build(3, true, 0, false), None);
    assert_eq!(build(0, false, 0, false), None);
    assert_eq!(build(3, true, 2, true), None);
    assert_eq!(build(0, false, 2, true), None);
}
