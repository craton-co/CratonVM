//! gch h4/jm: the `[sp-band]` census classes (`CRATONVM_DBG_SP_BAND_OWNERS`).
use super::{sp_band_owner, sp_band_owner_words};

/// Each word gets exactly one class, in the order named > primitive
/// operand > live splice local > unowned.
#[test]
fn gch_h4jm_each_band_word_gets_one_owner_class() {
    let slots = [64i16, 96];
    let prims = [72i16, 96];
    let inline = [(104i32, 2u16, 0b01u64)];
    assert_eq!(sp_band_owner(64, &slots, &prims, &inline), "oop");
    assert_eq!(sp_band_owner(72, &slots, &prims, &inline), "prim");
    assert_eq!(sp_band_owner(80, &slots, &prims, &inline), "unowned");
    assert_eq!(
        sp_band_owner(96, &slots, &prims, &inline),
        "oop",
        "named wins"
    );
    assert_eq!(sp_band_owner(104, &slots, &prims, &inline), "inline-local");
    assert_eq!(sp_band_owner(112, &slots, &prims, &inline), "inline-local");
    assert_eq!(
        sp_band_owner(120, &slots, &prims, &inline),
        "unowned",
        "past the scope"
    );
    assert_eq!(sp_band_owner(i32::MAX, &slots, &prims, &[]), "unowned");
}

/// The `words=` field walks `[lo, hi)` in 8-byte steps.
#[test]
fn gch_h4jm_the_words_field_covers_lo_to_live_hi() {
    let w = sp_band_owner_words(64, 88, &[64], &[80], &[]);
    assert_eq!(w, "64:oop,72:unowned,80:prim");
    assert_eq!(sp_band_owner_words(64, 64, &[64], &[], &[]), "");
    assert_eq!(
        sp_band_owner_words(64, 0, &[], &[], &[]),
        "",
        "an unknown cursor"
    );
}
