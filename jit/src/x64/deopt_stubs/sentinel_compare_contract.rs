use super::{JCC_NOT_SENTINEL, JCC_SENTINEL};

/// `emit_cmp_rax_sentinel` is `CMP RAX, 1`, and its consumers branch on
/// OF. That is only "RAX == i64::MIN" if `x - 1` overflows (signed) for
/// exactly that one value — which is what this pins, over the boundary
/// values and a sweep, so a future "simplification" to a different
/// immediate or a different flag is caught here rather than as a silently
/// swallowed exception.
#[test]
fn cmp_rax_1_overflows_for_exactly_the_sentinel() {
    assert!(i64::MIN.overflowing_sub(1).1, "the sentinel must set OF");
    for x in [
        i64::MIN + 1,
        -1,
        0,
        1,
        i64::MAX,
        i64::MAX - 1,
        0x7FFF_FFFF,
        -0x8000_0000,
    ] {
        assert!(
            !x.overflowing_sub(1).1,
            "{x:#x} must not read as the sentinel"
        );
    }
    let mut x = i64::MIN + 1;
    for _ in 0..4096 {
        assert_eq!(x.overflowing_sub(1).1, x == i64::MIN);
        x = x.wrapping_add(0x0010_0000_0000_0001);
    }
}

/// `0F 80` is JO and `0F 81` is JNO; the pair must be a condition and its
/// negation (they differ only in the low bit of the condition code).
#[test]
fn the_branch_pair_is_jo_and_jno() {
    assert_eq!(JCC_SENTINEL, 0x80);
    assert_eq!(JCC_NOT_SENTINEL, 0x81);
    assert_eq!(JCC_SENTINEL ^ JCC_NOT_SENTINEL, 1);
}
