use super::polling_body_id;

#[test]
fn a_poll_names_its_body_only_when_the_mirror_agrees() {
    assert_eq!(polling_body_id(7, 7), Some(7));
    assert_eq!(polling_body_id(0, 0), None, "0 is no body");
    assert_eq!(polling_body_id(7, 0), None, "nothing published");
    assert_eq!(polling_body_id(7, 8), None, "another frame's identity");
    // An undefined register (the IR and aarch64 polls do not load it).
    assert_eq!(polling_body_id(-1, u32::MAX), None);
    assert_eq!(polling_body_id(i64::from(u32::MAX) + 7, 7), None);
}
