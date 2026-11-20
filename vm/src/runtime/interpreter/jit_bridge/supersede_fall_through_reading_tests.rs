use super::supersede_fall_through_reading as read;

#[test]
fn an_optimizing_body_is_never_a_fall_through() {
    assert_eq!(read(true, true, true, true, true), (false, false));
    assert_eq!(read(true, true, false, true, true), (false, false));
}

#[test]
fn a_single_pass_body_keeps_both_readings() {
    assert_eq!(read(false, true, true, true, true), (true, false));
    assert_eq!(read(false, true, false, true, true), (false, true));
    assert_eq!(read(false, true, false, true, false), (false, false));
    assert_eq!(read(false, false, false, true, true), (false, false));
    assert_eq!(read(false, true, false, false, true), (false, false));
}
