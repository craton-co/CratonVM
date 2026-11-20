use super::{proxy_rows_below, ClassId, ROW_KEY_IS_HASH, ROW_KEY_IS_PROXY};

#[test]
fn only_proxy_rows_entered_below_the_depth_and_listed_count() {
    let proxy = ClassId::new(7);
    let rows = vec![
        // A proxy row entered at depth 2 (between frames 1 and 2).
        (2usize, proxy, ROW_KEY_IS_PROXY | (1 << 16) | 3, 0u32),
        // A JNI native's row (a function pointer key).
        (1, ClassId::new(9), 0x7ff0_1234, 0),
        // A method-event post's row (a hashed key).
        (1, ClassId::new(9), ROW_KEY_IS_HASH | 5, 0),
        // A proxy row on top of three frames (the proxy's own events).
        (3, proxy, ROW_KEY_IS_PROXY | 3, 0),
    ];
    assert_eq!(proxy_rows_below(&rows, 3, |_, _| true), 1);
    assert_eq!(proxy_rows_below(&rows, 4, |_, _| true), 2);
    assert_eq!(proxy_rows_below(&rows, 2, |_, _| true), 0);
    // A row whose listing facts were never noted is not listed.
    assert_eq!(proxy_rows_below(&rows, 4, |_, index| index != 3), 0);
}
