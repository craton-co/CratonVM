use super::{armed_ref_cas_route, ArmedRefCasRoute as R};

/// Elements heal-then-CAS (or lock with their switch off); fields take the
/// plain word CAS (or lock with theirs off); neither switch reaches the
/// other kind of slot.
#[test]
fn armed_slots_route_by_kind_and_switch() {
    assert_eq!(armed_ref_cas_route(true, false, true, true), R::Healed);
    assert_eq!(armed_ref_cas_route(true, false, true, false), R::Healed);
    assert_eq!(armed_ref_cas_route(true, false, false, true), R::Decline);
    assert_eq!(armed_ref_cas_route(false, false, true, true), R::Plain);
    assert_eq!(armed_ref_cas_route(false, false, false, true), R::Plain);
    assert_eq!(armed_ref_cas_route(false, false, true, false), R::Decline);
}

/// zt t31/zc11: a colored-store heap's FIELD holds coloured words, so it
/// takes the elements' heal-then-CAS (and their switch), never the plain
/// word CAS its uncoloured `expected` cannot match.
#[test]
fn zt31zc11_colored_fields_heal_then_cas() {
    assert_eq!(armed_ref_cas_route(false, true, true, true), R::Healed);
    assert_eq!(armed_ref_cas_route(false, true, true, false), R::Healed);
    assert_eq!(armed_ref_cas_route(false, true, false, true), R::Decline);
    assert_eq!(armed_ref_cas_route(false, true, false, false), R::Decline);
    assert_eq!(armed_ref_cas_route(true, true, true, true), R::Healed);
}
