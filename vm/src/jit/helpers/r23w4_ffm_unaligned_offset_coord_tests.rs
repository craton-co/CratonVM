use super::*;

/// r23 wave 4: an index scales by the width (overflow declines), a byte
/// offset is taken as is, and the marker never collides with a converted
/// index.
#[test]
fn coords_name_the_byte_offset_the_road_checks() {
    assert_eq!(FfmCoord::Index(5).byte_offset(4), Some(20));
    assert_eq!(FfmCoord::Index(i64::MAX).byte_offset(4), None);
    assert_eq!(FfmCoord::Byte(1).byte_offset(4), Some(1));
    assert_eq!(FfmCoord::Byte(-3).byte_offset(8), Some(-3), "negative: the bounds check declines");
    for shift in 1..=3 {
        assert!(i64::MIN >> shift != FFM_BYTE_OFFSET_MARK);
    }
}

/// r23 wave 5: the one kind-to-carrier table both door entries share.
#[test]
fn site_kinds_map_every_ffm_kind() {
    let all: Vec<Option<u8>> = (0..=7).map(ffm_site_kind).collect();
    assert_eq!(
        all,
        vec![Some(b'B'), Some(b'S'), Some(b'C'), Some(b'I'), Some(b'J'), Some(b'F'), Some(b'D'), None]
    );
}

/// An unaligned offset takes a byte coordinate only when the site word
/// says so; an aligned one keeps its element index; an index site never
/// takes a byte coordinate.
#[test]
fn unaligned_offsets_need_the_site_bit() {
    use cratonvm_jit::{
        FFM_ELEMENT_SITE_BYTE_OFFSET as OFF, FFM_ELEMENT_SITE_UNALIGNED_OK as UN,
        FFM_ELEMENT_SITE_VALID as V,
    };
    let int = FFM_KIND_INT as u32; // Cast: a 0..=6 kind code
    assert_eq!(ffm_element_word_coord(V | OFF | UN | int, 8), Some((FFM_KIND_INT, FfmCoord::Index(2))));
    assert_eq!(ffm_element_word_coord(V | OFF | UN | int, 9), Some((FFM_KIND_INT, FfmCoord::Byte(9))));
    assert_eq!(ffm_element_word_coord(V | OFF | int, 9), None, "switch off at bind: decline");
    assert_eq!(ffm_element_word_coord(V | int, 9), Some((FFM_KIND_INT, FfmCoord::Index(9))));
    assert_eq!(ffm_element_word_coord(OFF | UN | int, 9), None, "not a site");
}
