//! JIT round 21 lane osrsplice: the OSR door's int-seed canonicalisation
//! (`jr21-osr-int-seed-zero-extended-20261015.md`).
use super::sign_extend_int_seeds;
use cratonvm_types::{VTAG_INT, VTAG_LONG, VTAG_OBJECT};

#[test]
fn a_zero_extended_negative_int_seed_is_sign_extended() {
    // `Frame::get_local_raw` shape: int -1 as `u32 as u64`, a long whose
    // low word looks like a negative int, a reference.
    let locals = [0xFFFF_FFFFi64, 0x0000_0000_8000_0000, 0x7000_0000_1000];
    let tags = [VTAG_INT, VTAG_LONG, VTAG_OBJECT];
    let seeded = sign_extend_int_seeds(&locals, &tags);
    assert_eq!(&*seeded, &[-1i64, 0x0000_0000_8000_0000, 0x7000_0000_1000]);
}

#[test]
fn non_negative_ints_borrow_the_offer_unchanged() {
    let locals = [5i64, 0x7FFF_FFFF, -9];
    let tags = [VTAG_INT, VTAG_INT, VTAG_LONG];
    let seeded = sign_extend_int_seeds(&locals, &tags);
    assert!(matches!(seeded, std::borrow::Cow::Borrowed(_)));
    assert_eq!(&*seeded, &locals);
}

/// t1 wave 8 lane OSRSPLICE: on the rewriting (`Owned`) path, only
/// `VTAG_INT` words change. A negative float's bits, a double, a long and
/// a reference whose low halves would read as negative ints, a null, an
/// uninitialised slot and a return address all pass through bit-exact
/// (sign-extending a float's `to_bits() as u64` would hand the body
/// 0xFFFF_FFFF_BF80_0000 for -1.0f).
#[test]
fn only_int_tagged_words_are_rewritten() {
    use cratonvm_types::{VTAG_DOUBLE, VTAG_FLOAT, VTAG_NULL, VTAG_RETADDR, VTAG_UNINIT};
    let locals = [
        0xFFFF_FFFFi64,           // int -1, zero-extended: rewritten
        0xBF80_0000,              // float -1.0f bits
        0x0000_0000_8000_0001,    // long
        0x0000_0000_C000_0000,    // double bits with a "negative" low half
        0x0000_7FF0_8000_0010,    // reference
        0,                        // null
        0,                        // uninit
        0x8000_0004,              // return address
    ];
    let tags = [
        VTAG_INT,
        VTAG_FLOAT,
        VTAG_LONG,
        VTAG_DOUBLE,
        VTAG_OBJECT,
        VTAG_NULL,
        VTAG_UNINIT,
        VTAG_RETADDR,
    ];
    let seeded = sign_extend_int_seeds(&locals, &tags);
    assert!(matches!(seeded, std::borrow::Cow::Owned(_)));
    let mut want = locals;
    want[0] = -1;
    assert_eq!(&*seeded, &want);
}

/// The interpreter tags every int-category local (`byte`, `short`,
/// `char`, `boolean`, `int`) `VTAG_INT` and hands it as `i32 as u32 as
/// u64`; each extreme comes out as the sign-extended i64 of its int value,
/// and `char`'s top value (a non-negative int) is left alone.
#[test]
fn int_category_extremes_are_sign_extended() {
    let locals = [
        i64::from(i8::MIN as i32 as u32),  // byte -128
        i64::from(i16::MIN as i32 as u32), // short -32768
        i64::from(u16::MAX),               // char 0xFFFF
        1,                                 // boolean true
        i64::from(i32::MIN as u32),        // int MIN
        i64::from(i32::MAX as u32),        // int MAX
    ];
    let tags = [VTAG_INT; 6];
    let seeded = sign_extend_int_seeds(&locals, &tags);
    assert_eq!(
        &*seeded,
        &[-128i64, -32768, 0xFFFF, 1, i64::from(i32::MIN), i64::from(i32::MAX)]
    );
}

/// An offer whose negative ints are already canonical (the shape a
/// re-entry after a single-pass exit would produce if a future door
/// handed the compiled words back verbatim) is borrowed, and the
/// rewrite is idempotent.
#[test]
fn already_sign_extended_ints_are_borrowed_and_the_rewrite_is_idempotent() {
    let tags = [VTAG_INT, VTAG_INT];
    let canonical = [-5i64, i64::from(i32::MIN)];
    let seeded = sign_extend_int_seeds(&canonical, &tags);
    assert!(matches!(seeded, std::borrow::Cow::Borrowed(_)));
    let raw = [0xFFFF_FFFBi64, 0x8000_0000];
    let once = sign_extend_int_seeds(&raw, &tags).into_owned();
    assert_eq!(once, canonical);
    let twice = sign_extend_int_seeds(&once, &tags);
    assert!(matches!(twice, std::borrow::Cow::Borrowed(_)));
}

/// A tag slice shorter than the locals (validation refuses it before an
/// entry is planned) leaves the untagged tail as offered, even a word that
/// would read as a zero-extended negative int.
#[test]
fn the_untagged_tail_is_left_as_offered() {
    let locals = [0xFFFF_FFFEi64, 0xFFFF_FFFF];
    let tags = [VTAG_INT];
    let seeded = sign_extend_int_seeds(&locals, &tags);
    assert_eq!(&*seeded, &[-2i64, 0xFFFF_FFFF]);
}
