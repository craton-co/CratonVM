use super::{
    method_stamp_token, method_stamp_token_of_label, split_throw_stamp_owner,
    TRAP_SNAPSHOT_DEFER_BIT,
};

#[test]
fn a_packed_stamp_splits_back_into_its_bci_and_owner() {
    for bci in [0i64, 1, 23, 0xFFFF, 0x1234 | TRAP_SNAPSHOT_DEFER_BIT] {
        for id in [1u32, 7, 0x7FFF_FFFF, 0xFFFF_FFFE] {
            let packed = ((u64::from(id) << 32) | bci as u64) as i64;
            assert_eq!(split_throw_stamp_owner(packed), (bci, id));
        }
        assert_eq!(split_throw_stamp_owner(bci), (bci, 0));
    }
    assert_eq!(split_throw_stamp_owner(-1), (-1, 0));
}

#[test]
fn the_method_token_is_a_function_of_name_and_descriptor_only() {
    let t = method_stamp_token("p/Q", "run", "(II)I");
    assert_ne!(t, 0);
    assert_ne!(t, u32::MAX);
    assert_eq!(t, method_stamp_token("p/Q", "run", "(II)I"));
    assert_eq!(t, method_stamp_token("p.Q", "run", "(II)I"));
    assert_ne!(t, method_stamp_token("p/Q", "run", "(I)I"));
    assert_ne!(t, method_stamp_token("p/R", "run", "(II)I"));
    assert_eq!(method_stamp_token_of_label("p/Q.run:(II)I"), t);
    assert_eq!(method_stamp_token_of_label("lambda-adapter->0x7f00"), 0);
    assert_eq!(method_stamp_token_of_label(""), 0);
}
