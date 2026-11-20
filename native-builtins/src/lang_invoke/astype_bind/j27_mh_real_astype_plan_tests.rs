use super::*;

const LONG: u8 = 1 << MH_REAL_UNBOX_LONG;
const INT: u8 = 1 << MH_REAL_UNBOX_INT;
const SHORT: u8 = 1 << MH_REAL_UNBOX_SHORT;
const BYTE: u8 = 1 << MH_REAL_UNBOX_BYTE;
const CHAR: u8 = 1 << MH_REAL_UNBOX_CHAR;

/// `twiceJ(long)long.asType((Object)Object)` (`J27MhRows` `conv-long-*`):
/// `unboxWiden(LONG)` accepts Long, Integer, Short, Byte and Character
/// (Boolean, Float, Double and null are the door's CCE / NPE), and the
/// result is boxed through `Long.valueOf`.
#[test]
fn j27_object_into_long_widens_every_convertible_wrapper() {
    let (unbox, ret) = mh_real_conversion_plan(
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(J)J",
    )
    .expect("served");
    assert_eq!(unbox[0], LONG | INT | SHORT | BYTE | CHAR);
    assert!(unbox[1..].iter().all(|m| *m == 0));
    assert_eq!(ret, MhRealBox::Long);
}

/// `int` takes no `Long` (`Wrapper.INT.isConvertibleFrom(LONG)` is false:
/// the JDK raises `Cannot cast java.lang.Long to java.lang.Integer`), and
/// the wrapper's own type is `unboxExact`: that class only.
#[test]
fn j27_int_refuses_long_and_the_exact_wrapper_takes_only_itself() {
    assert_eq!(
        mh_real_arg_unbox("Ljava/lang/Number;", "I"),
        Some(INT | SHORT | BYTE | CHAR)
    );
    assert_eq!(mh_real_arg_unbox("Ljava/lang/Integer;", "I"), Some(INT));
    assert_eq!(mh_real_arg_unbox("Ljava/lang/Long;", "J"), Some(LONG));
    assert_eq!(mh_real_arg_unbox("Ljava/lang/Integer;", "J"), Some(LONG_SOURCES));
}

const LONG_SOURCES: u8 = LONG | INT | SHORT | BYTE | CHAR;

/// Positions this bind does not serve refuse the whole handle: an unbox
/// into `short` / `char` / `boolean` / `double`, a box of an argument, a
/// primitive conversion, a reference cast.
#[test]
fn j27_unserved_positions_refuse() {
    for to in ["S", "C", "Z", "B", "F", "D"] {
        assert_eq!(mh_real_arg_unbox("Ljava/lang/Object;", to), None, "{to}");
    }
    assert_eq!(mh_real_arg_unbox("I", "Ljava/lang/Object;"), None, "argument box");
    assert_eq!(mh_real_arg_unbox("I", "J"), None, "primitive widening");
    assert_eq!(mh_real_arg_unbox("Ljava/lang/Object;", "Ljava/lang/String;"), None);
    // Null conversions pass the word through.
    assert_eq!(mh_real_arg_unbox("Ljava/lang/String;", "Ljava/lang/Object;"), Some(0));
    assert_eq!(mh_real_arg_unbox("[I", "Ljava/lang/Object;"), Some(0));
    assert_eq!(mh_real_arg_unbox("Lp/Acc;", "Lp/Acc;"), Some(0));
    assert!(mh_real_conversion_plan("(Ljava/lang/Object;)Ljava/lang/Object;", "(S)S").is_none());
    assert!(mh_real_conversion_plan("(Ljava/lang/Object;)V", "(J)J").is_none(), "void drop");
    assert!(mh_real_conversion_plan("(Ljava/lang/Object;)J", "(JJ)J").is_none(), "arity");
}

/// The return box: into the wrapper or a supertype of it; never into
/// another wrapper (`int` into `Long` is a CCE in the JDK's adapter).
#[test]
fn j27_return_boxes_only_into_assignable_types() {
    assert_eq!(mh_real_return_box("Ljava/lang/Integer;", "I"), Some(MhRealBox::Int));
    assert_eq!(mh_real_return_box("Ljava/lang/Number;", "J"), Some(MhRealBox::Long));
    assert_eq!(mh_real_return_box("Ljava/lang/Comparable;", "I"), Some(MhRealBox::Int));
    assert_eq!(mh_real_return_box("Ljava/lang/Long;", "I"), None);
    assert_eq!(mh_real_return_box("Ljava/lang/Object;", "S"), None, "short box");
    assert_eq!(mh_real_return_box("J", "J"), Some(MhRealBox::Keep));
    assert_eq!(mh_real_return_box("Ljava/lang/Object;", "Ljava/lang/String;"), Some(MhRealBox::Keep));
    assert_eq!(mh_real_return_box("Ljava/lang/String;", "Ljava/lang/Object;"), None, "cast");
}

/// An unbound virtual member's receiver is word 0 and keeps its token;
/// the `int` parameter after it unboxes (`J27MhRows` `conv-virtual-*`).
#[test]
fn j27_virtual_receiver_passes_and_the_next_word_unboxes() {
    let (unbox, ret) = mh_real_conversion_plan(
        "(Lp/Acc;Ljava/lang/Object;)Ljava/lang/Object;",
        "(Lp/Acc;I)I",
    )
    .expect("served");
    assert_eq!(&unbox[..2], &[0, INT | SHORT | BYTE | CHAR]);
    assert_eq!(ret, MhRealBox::Int);
}

/// j27 wave 8 (lane W8CLI): a position whose every wrapper is unloaded
/// would read as "pass the word" (mask 0) and hand the member a reference
/// word as its `long`: the handle is refused instead. Positions the plan
/// never unboxed, and positions that keep a bit, are fine.
#[test]
fn j27_w8cli_an_unbox_position_emptied_by_unloaded_wrappers_refuses() {
    let (planned, _) = mh_real_conversion_plan(
        "(Ljava/lang/Long;Lp/Acc;)J",
        "(JLp/Acc;)J",
    )
    .expect("served");
    assert_eq!(&planned[..2], &[LONG, 0]);
    let mut emptied = planned;
    emptied[0] &= !LONG;
    assert!(!mh_real_unbox_positions_survive(&planned, &emptied));
    assert!(mh_real_unbox_positions_survive(&planned, &planned));
    let mut narrowed = [0u8; MH_BIND_CONVERSION_MAX_WORDS];
    narrowed[0] = LONG_SOURCES & !CHAR;
    let mut wide = [0u8; MH_BIND_CONVERSION_MAX_WORDS];
    wide[0] = LONG_SOURCES;
    assert!(mh_real_unbox_positions_survive(&wide, &narrowed));
}

