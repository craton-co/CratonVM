use super::const_int_return_of;

/// `CompletionStages.alwaysTrue` is these two bytes, and the probe's whole
/// premise is recognising them.
#[test]
fn iconst_1_ireturn_is_the_constant_one() {
    assert_eq!(const_int_return_of(&[0x04, 0xAC], false), Some(1));
    assert_eq!(const_int_return_of(&[0x03, 0xAC], false), Some(0));
    assert_eq!(const_int_return_of(&[0x02, 0xAC], false), Some(-1));
    assert_eq!(const_int_return_of(&[0x08, 0xAC], false), Some(5));
}

#[test]
fn pushed_literals_decode_with_their_sign() {
    assert_eq!(const_int_return_of(&[0x10, 0xFF, 0xAC], false), Some(-1));
    assert_eq!(const_int_return_of(&[0x10, 0x7F, 0xAC], false), Some(127));
    assert_eq!(
        const_int_return_of(&[0x11, 0xFF, 0x00, 0xAC], false),
        Some(-256)
    );
}

/// The regression that made the first version of this probe screen NOTHING:
/// this VM hands out a zero-padded `code` slice, so `alwaysTrue` arrives as
/// four bytes and an exact-length pattern missed the one method the probe
/// exists for. `site_const_screened=0` was the only thing that showed it.
#[test]
fn trailing_padding_does_not_hide_a_constant_body() {
    assert_eq!(
        const_int_return_of(&[0x04, 0xAC, 0x00, 0x00], false),
        Some(1)
    );
    assert_eq!(
        const_int_return_of(&[0x10, 0x2A, 0xAC, 0x00], false),
        Some(42)
    );
}

/// With a handler in the table, a bci past the `ireturn` is reachable, so
/// the prefix no longer describes every path and the body is not a
/// candidate.
#[test]
fn a_handler_makes_the_prefix_argument_invalid() {
    assert_eq!(const_int_return_of(&[0x04, 0xAC, 0x00, 0x00], true), None);
}

/// Anything that is not a push and an `ireturn` is not a candidate: a body
/// the probe cannot predict must not be screened against a guess.
#[test]
fn a_body_that_does_anything_else_is_not_a_constant() {
    assert_eq!(const_int_return_of(&[0x04], false), None);
    // iload_0; ireturn — returns an argument, not a constant.
    assert_eq!(const_int_return_of(&[0x1A, 0xAC], false), None);
    // iconst_1; areturn — right constant, wrong return category.
    assert_eq!(const_int_return_of(&[0x04, 0xB0], false), None);
    assert_eq!(const_int_return_of(&[], false), None);
}
