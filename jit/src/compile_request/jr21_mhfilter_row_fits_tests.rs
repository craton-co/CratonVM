use super::*;

fn row(position: Option<u16>, main_kind: u8, pre: (&str, &str, u8)) -> MhConstantFilterRow {
    MhConstantFilterRow {
        position,
        main_kind,
        pre_class: "p/F".into(),
        pre_name: pre.0.into(),
        pre_desc: pre.1.into(),
        pre_kind: pre.2,
        pre_class_id: 0x71,
        target_slot: 4,
    }
}

#[test]
fn jr21_desc_parts_split_every_kind_of_parameter() {
    assert_eq!(
        mh_filter_desc_parts("(I[[JLp/A;D)Lq/B;"),
        Some((vec!["I", "[[J", "Lp/A;", "D"], "Lq/B;"))
    );
    assert_eq!(mh_filter_desc_parts("()V"), Some((vec![], "V")));
    assert_eq!(mh_filter_desc_parts("(L;)V"), None);
    assert_eq!(mh_filter_desc_parts("(I)"), None);
    assert_eq!(mh_filter_desc_parts("(Q)V"), None);
    assert_eq!(mh_filter_desc_parts("I)V"), None);
}

#[test]
fn jr21_an_argument_filter_of_a_virtual_target() {
    // filterArguments(Acc.plus (I)I, 0, swap (Lp/Acc;)Lp/Acc;): (Lp/Acc;I)I.
    let swap = row(Some(0), 0, ("swap", "(Lp/Acc;)Lp/Acc;", 3));
    assert!(mh_filter_row_fits("(Lp/Acc;I)I", "p/Acc", "(I)I", &swap));
    // The receiver filtered needs the argL0 slot, and a filter taking
    // the receiver's class.
    let no_slot = MhConstantFilterRow {
        target_slot: 0,
        ..swap.clone()
    };
    assert!(!mh_filter_row_fits(
        "(Lp/Acc;I)I",
        "p/Acc",
        "(I)I",
        &no_slot
    ));
    let from_obj = row(Some(0), 0, ("make", "(Ljava/lang/Object;)Lp/Acc;", 3));
    assert!(!mh_filter_row_fits(
        "(Ljava/lang/Object;I)I",
        "p/Acc",
        "(I)I",
        &from_obj
    ));
    // A filter at the int argument: (Lp/Acc;J)I with narrow (J)I.
    let narrow = row(Some(1), 0, ("narrow", "(J)I", 3));
    assert!(mh_filter_row_fits("(Lp/Acc;J)I", "p/Acc", "(I)I", &narrow));
    assert!(!mh_filter_row_fits("(Lp/Acc;I)I", "p/Acc", "(I)I", &narrow));
    assert!(!mh_filter_row_fits("(Lp/Acc;J)J", "p/Acc", "(I)I", &narrow));
    // Past the last argument, a virtual filter, a void filter: no.
    assert!(!mh_filter_row_fits(
        "(Lp/Acc;I)I",
        "p/Acc",
        "(I)I",
        &row(Some(2), 0, ("narrow", "(J)I", 3))
    ));
    assert!(!mh_filter_row_fits(
        "(Lp/Acc;J)I",
        "p/Acc",
        "(I)I",
        &row(Some(1), 0, ("narrow", "(J)I", 0))
    ));
    assert!(!mh_filter_row_fits(
        "(Lp/Acc;J)I",
        "p/Acc",
        "(I)I",
        &row(Some(1), 0, ("narrow", "(J)V", 3))
    ));
}

#[test]
fn jr21_an_argument_filter_of_a_static_target() {
    // filterArguments(add (II)I, 1, len (Ljava/lang/String;)I).
    let len = row(Some(1), 3, ("len", "(Ljava/lang/String;)I", 3));
    assert!(mh_filter_row_fits(
        "(ILjava/lang/String;)I",
        "p/H",
        "(II)I",
        &len
    ));
    assert!(!mh_filter_row_fits("(II)I", "p/H", "(II)I", &len));
}

#[test]
fn jr21_a_return_filter() {
    // filterReturnValue(Acc.plus (I)I, box (I)Ljava/lang/Integer;).
    let plus = MhConstantFilterRow {
        pre_class: "p/Acc".into(),
        ..row(None, 3, ("plus", "(I)I", 0))
    };
    assert!(mh_filter_row_fits(
        "(Lp/Acc;I)Ljava/lang/Integer;",
        "p/F",
        "(I)Ljava/lang/Integer;",
        &plus
    ));
    // The filter must take the target's return, be static and one-ary.
    assert!(!mh_filter_row_fits(
        "(Lp/Acc;I)Ljava/lang/Integer;",
        "p/F",
        "(J)Ljava/lang/Integer;",
        &plus
    ));
    let virtual_filter = MhConstantFilterRow {
        main_kind: 0,
        ..plus.clone()
    };
    assert!(!mh_filter_row_fits(
        "(Lp/Acc;I)Ljava/lang/Integer;",
        "p/F",
        "(I)Ljava/lang/Integer;",
        &virtual_filter
    ));
    // A static target: (II)J with twice (J)J over add (II)J.
    let add = row(None, 3, ("add", "(II)J", 3));
    assert!(mh_filter_row_fits("(II)J", "p/F", "(J)J", &add));
    assert!(!mh_filter_row_fits("(II)I", "p/F", "(J)J", &add));
    let void_target = row(None, 3, ("run", "(II)V", 3));
    assert!(!mh_filter_row_fits("(II)J", "p/F", "(J)J", &void_target));
}
