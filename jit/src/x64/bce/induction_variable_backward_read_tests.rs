use super::*;

/// A `sipush` whose low operand byte is `0x60` is not an `iadd`.
///
/// `find_induction_variable`'s `iadd; istore X` recogniser asked
/// `code[pc - 1] == 0x60`. This walk lands on real instruction starts, so
/// `pc` is one — but `pc - 1` is the previous instruction's LAST byte, and
/// that is its opcode only when it was one byte long. `sipush 0x0160`
/// encodes as `11 01 60` and ends in a `0x60` that reads as `iadd`.
///
/// The consequence is not a missed optimization: a spurious
/// `iadd_store_locals` entry can make this function name a local as the
/// loop's induction variable when it is not one, and that answer is what
/// guarded bounds-check elision reasons about.
#[test]
fn a_sipush_operand_byte_is_not_an_iadd() {
    // 0: sipush 0x0160   11 01 60
    // 3: istore_1        3C
    // 4: goto 0          A7 FF FC   (back edge; the walk stops before it)
    let code = [0x11, 0x01, 0x60, 0x3C, 0xA7, 0xFF, 0xFC];
    assert_eq!(
        find_induction_variable(&code, 0, 4),
        None,
        "local 1 is stored from a `sipush`, not incremented by an `iadd`; \
         naming it the induction variable would let BCE reason about the \
         wrong value"
    );
}

/// The positive control: a REAL `iadd; istore_1` is still recognised, so
/// the fix did not turn the recogniser off.
#[test]
fn a_real_iadd_before_istore_is_still_recognised() {
    // 0: iload_1   1B
    // 1: iload_2   1C
    // 2: iadd      60
    // 3: istore_1  3C
    // 4: goto 0
    let code = [0x1B, 0x1C, 0x60, 0x3C, 0xA7, 0xFF, 0xFC];
    assert_eq!(
        find_induction_variable(&code, 0, 4),
        Some(1),
        "`iload_1; iload_2; iadd; istore_1` is the non-unit-stride \
         induction variable this recogniser exists for"
    );
}

/// And the ordinary `iinc` form is untouched — it takes priority 1 and
/// never consulted the backward read at all.
#[test]
fn the_iinc_form_is_unaffected() {
    // 0: iinc 1, 1   84 01 01
    // 3: goto 0
    let code = [0x84, 0x01, 0x01, 0xA7, 0xFF, 0xFD];
    assert_eq!(find_induction_variable(&code, 0, 3), Some(1));
}

/// `wide iinc` is a write. A local advanced by `iinc i, 1` AND by
/// `wide iinc i, 1000` is not a unit-stride induction variable; the walk
/// used to step over the wide form without seeing it.
#[test]
fn a_wide_iinc_refuses_the_local_as_an_induction_variable() {
    // 0: iinc 1, 1           84 01 01
    // 3: wide iinc 1, 1000   C4 84 00 01 03 E8
    // 9: goto 0              A7 FF F7
    let code = [
        0x84, 0x01, 0x01, 0xC4, 0x84, 0x00, 0x01, 0x03, 0xE8, 0xA7, 0xFF, 0xF7,
    ];
    assert_eq!(find_induction_variable(&code, 0, 9), None);
    // The twin with the wide write replaced by six `nop`s keeps its IV, so
    // the refusal is about the wide write and not the fixture.
    let plain = [
        0x84, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xA7, 0xFF, 0xF7,
    ];
    assert_eq!(find_induction_variable(&plain, 0, 9), Some(1));
}

/// `dstore_0` writes slots 0 and 1. It used to share the `fstore` arm and
/// mark slot 4 instead, refusing an unrelated IV there.
#[test]
fn a_dstore_marks_its_own_two_slots_not_slot_plus_four() {
    // 0: iinc 4, 1   84 04 01
    // 3: dconst_0    0E
    // 4: dstore_0    47
    // 5: goto 0      A7 FF FB
    let code = [0x84, 0x04, 0x01, 0x0E, 0x47, 0xA7, 0xFF, 0xFB];
    assert_eq!(find_induction_variable(&code, 0, 5), Some(4));
    // And the high half it DOES write refuses an IV living there.
    let clobber = [0x84, 0x01, 0x01, 0x0E, 0x47, 0xA7, 0xFF, 0xFB];
    assert_eq!(find_induction_variable(&clobber, 0, 5), None);
}

/// Priority 2 (`iadd; istore X`) used to trust "exactly one `iadd`-preceded
/// `istore`" as "written nowhere else". A local also written by a second,
/// unrelated plain `istore` in the same body — `j = 5; ...; j = j + i;` —
/// passed that check and was still returned as the induction variable, with
/// the plain store never having been asked about at all.
#[test]
fn a_second_plain_store_to_the_iadd_local_refuses_it_as_an_induction_variable() {
    // 0: iconst_5   08          (an unrelated plain store of local 1)
    // 1: istore_1   3C
    // 2: iload_1    1B
    // 3: iload_2    1C
    // 4: iadd       60
    // 5: istore_1   3C          (the `iadd; istore_1` this recogniser wants)
    // 6: goto -6 -> 0  A7 FF FA (back edge; the walk stops before it)
    let code = [0x08, 0x3C, 0x1B, 0x1C, 0x60, 0x3C, 0xA7, 0xFF, 0xFA];
    assert_eq!(
        find_induction_variable(&code, 0, 6),
        None,
        "local 1 is also reset by a plain `istore` elsewhere in the body, so \
         it is not an affine induction variable — naming it one would let \
         BCE reason about a value that can jump outside the recognised step"
    );
    // MUST ACCEPT twin: with the extra store gone (replaced by `nop`s), the
    // `iadd; istore_1` pattern alone is still recognised.
    let plain = [0x00, 0x00, 0x1B, 0x1C, 0x60, 0x3C, 0xA7, 0xFF, 0xFA];
    assert_eq!(find_induction_variable(&plain, 0, 6), Some(1));
}
