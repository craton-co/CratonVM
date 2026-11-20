//! gch h1/fr: the holder-census door's attribution lines, pure parts.
use super::{oldmark_census_interp_frame_token, oldmark_census_pinned_marker_slots};

#[test]
fn gch_h1fr_pinned_marker_slots_name_each_pin_of_a_marker() {
    assert_eq!(
        oldmark_census_pinned_marker_slots(&[0x10, 0x20, 0x30, 0x20], &[0x20]),
        " 1/4 3/4"
    );
    assert_eq!(
        oldmark_census_pinned_marker_slots(&[0x10, 0x30], &[0x20, 0x30]),
        " 1/2"
    );
    assert_eq!(
        oldmark_census_pinned_marker_slots(&[0x10], &[0x20]),
        " none"
    );
    assert_eq!(oldmark_census_pinned_marker_slots(&[], &[0x20]), " none");
}

#[test]
fn gch_h1fr_interp_frame_token_shape() {
    assert_eq!(
        oldmark_census_interp_frame_token("Gcd1ArgPinSpecialProbe", "virtualCaller", 7),
        "Gcd1ArgPinSpecialProbe.virtualCaller@7"
    );
}
