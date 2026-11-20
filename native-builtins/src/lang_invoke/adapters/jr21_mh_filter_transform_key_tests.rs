use super::mh_filter_transform_position;

#[test]
fn jr21_filter_keys_decode_to_their_position() {
    // TransformKey.of(FILTER_ARG, 1 + 0, L_TYPE): 0x15.
    assert_eq!(mh_filter_transform_position(0x15), Some(Some(0)));
    // FILTER_ARG at LF position 3 (site argument 2), I_TYPE: 0x135.
    assert_eq!(mh_filter_transform_position(0x135), Some(Some(2)));
    // FILTER_RETURN, not constant-zero, I_TYPE: 0x106.
    assert_eq!(mh_filter_transform_position(0x106), Some(None));
    // FILTER_RETURN with constantZero: no filter call.
    assert_eq!(mh_filter_transform_position(0x116), None);
    // FILTER_ARG at LF position 0 (the handle itself), BIND_ARG(1), a
    // fourth byte: none.
    assert_eq!(mh_filter_transform_position(0x05), None);
    assert_eq!(mh_filter_transform_position(0x11), None);
    assert_eq!(mh_filter_transform_position(0x1115), None);
}
