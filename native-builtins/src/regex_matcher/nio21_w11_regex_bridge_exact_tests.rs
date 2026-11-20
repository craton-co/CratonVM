use super::*;

/// A search may not start on the LOW half of a pair (the Rust engine has
/// no such position); every other index, both ends included, is fine.
#[test]
fn inside_pair_is_exactly_the_low_half() {
    let (_, utf16_to_byte) = matcher_realjdk_build_offset_tables("a\u{10400}b");
    // UTF-16: a=0, high=1, low=2, b=3, end=4.
    let inside: Vec<i32> = (0..=4)
        .filter(|&i| utf16_index_inside_pair(&utf16_to_byte, i))
        .collect();
    assert_eq!(inside, vec![2]);
    assert!(!utf16_index_inside_pair(&utf16_to_byte, -1));
    assert!(!utf16_index_inside_pair(&utf16_to_byte, 99));
}
