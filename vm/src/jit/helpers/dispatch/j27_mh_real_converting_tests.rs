use super::*;
use cratonvm_native_builtins::lang_invoke::{
    MhRealBox, MH_REAL_UNBOX_BYTE, MH_REAL_UNBOX_CHAR, MH_REAL_UNBOX_INT, MH_REAL_UNBOX_LONG,
    MH_REAL_UNBOX_SHORT,
};

fn conv(unbox0: u8, box_return: MhRealBox) -> MhRealConv {
    let mut unbox = [0u8; MH_BIND_MAX_SPLICED_ARGS];
    unbox[0] = unbox0;
    MhRealConv {
        unbox,
        cast: [0u32; MH_BIND_MAX_SPLICED_ARGS],
        wrappers: [(11, 0), (12, 0), (13, 0), (14, 0), (15, 0)],
        box_return,
    }
}

/// j27 wave 5 (lane MHASTYPE): a wrapper's value becomes the word an
/// `int` / `long` widening of its primitive gives, whatever the slot
/// carries above the primitive's width; a value of another variant is
/// the door's.
#[test]
fn j27_real_unbox_widens_each_wrapper_as_its_primitive() {
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_LONG, Value::Long(i64::MIN)), Some(i64::MIN));
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_INT, Value::Int(-7)), Some(-7));
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_SHORT, Value::Int(-2)), Some(-2));
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_SHORT, Value::Int(0x1_8000)), Some(-32768));
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_BYTE, Value::Int(0x80)), Some(-128));
    // `char` is unsigned: 'A' is 65, and U+FFFF is 65535, never -1.
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_CHAR, Value::Int(65)), Some(65));
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_CHAR, Value::Int(-1)), Some(65535));
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_LONG, Value::Int(5)), None);
    assert_eq!(mh_real_widen(MH_REAL_UNBOX_INT, Value::Long(5)), None);
    assert_eq!(mh_real_widen(5, Value::Int(1)), None, "no sixth wrapper");
}

/// The converted words: marked words replaced by the unbox's answer, the
/// others passed; a refused unbox (null, a wrapper the JDK's unboxer
/// rejects) or a call wider than the buffer is the door's; the return box
/// follows the admission's answer.
#[test]
fn j27_real_convert_args_unboxes_marked_words_only() {
    let boxed = 0x7_0000_0040i64;
    let mask = 1u8 << MH_REAL_UNBOX_INT;
    let unbox = |m: u8, w: i64| (m == mask && w == boxed).then_some(-42i64);
    let mut two = conv(0, MhRealBox::Long);
    two.unbox[1] = mask;
    let no_cast = |_, _| false;
    let out = mh_real_convert_args(&two, &[5, boxed], unbox, no_cast).expect("word 1 unboxes");
    assert_eq!(&out.words[..2], &[5, -42]);
    assert!(out.box_long_return && !out.box_int_return);
    assert!(mh_real_convert_args(&conv(mask, MhRealBox::Keep), &[0], unbox, no_cast).is_none());
    let keep = mh_real_convert_args(&conv(0, MhRealBox::Int), &[1, 2], unbox, no_cast).expect("plain");
    assert_eq!(&keep.words[..2], &[1, 2]);
    assert!(keep.box_int_return && !keep.box_long_return);
    let wide = [boxed; MH_BIND_MAX_SPLICED_ARGS + 1];
    assert!(mh_real_convert_args(&conv(mask, MhRealBox::Keep), &wide, unbox, no_cast).is_none());
    let mut with_cast = conv(0, MhRealBox::Keep);
    with_cast.cast[0] = 42;
    assert!(mh_real_convert_args(&with_cast, &[99], unbox, |c, w| c == 42 && w == 99).is_some());
    assert!(mh_real_convert_args(&with_cast, &[99], unbox, |_, _| false).is_none());
}

/// The carrier's converting words are unchanged by the shared loop, and
/// never box a `long`.
#[test]
fn j27_carrier_convert_args_never_box_a_long() {
    let carrier = MhBindConv {
        type_slot: 0,
        type_word: 0,
        unbox_int: 0b1,
        box_int_return: true,
        integer_class_id: 1,
        value_slot: 0,
    };
    let out = mh_bind_convert_args(&carrier, &[9, 3], |w| Some(w * 2)).expect("converts");
    assert_eq!(&out.words[..2], &[18, 3]);
    assert!(out.box_int_return && !out.box_long_return);
}
