use super::*;

/// A triple is evaluated once per thread; a different method name,
/// descriptor, class or VM is a different key and evaluates again.
#[test]
fn name_override_memo_answers_a_repeat_without_evaluating() {
    let calls = std::cell::Cell::new(0u32);
    let eval = |answer: bool| {
        calls.set(calls.get() + 1);
        answer
    };
    let cid = ClassId::new(0x4_1234);
    let mut memo = NameOverrideMemo::default();
    let memo = &mut memo;
    assert!(name_override_memoized(
        memo,
        7,
        cid,
        "lock",
        "()V",
        || eval(true)
    ));
    assert!(name_override_memoized(
        memo,
        7,
        cid,
        "lock",
        "()V",
        || eval(false)
    ));
    assert_eq!(calls.get(), 1, "the repeat is answered from the memo");
    assert!(!name_override_memoized(
        memo,
        7,
        cid,
        "lock",
        "(I)V",
        || eval(false)
    ));
    assert!(!name_override_memoized(
        memo,
        7,
        cid,
        "unlock",
        "()V",
        || eval(false)
    ));
    assert!(!name_override_memoized(memo, 8, cid, "lock", "()V", || {
        eval(false)
    }));
    assert!(!name_override_memoized(
        memo,
        7,
        ClassId::new(0x4_1235),
        "lock",
        "()V",
        || { eval(false) }
    ));
    assert_eq!(calls.get(), 5, "every other key evaluates");
}

/// The row index reads only lengths and a few bytes, and stays in range
/// for empty strings.
#[test]
fn name_override_memo_index_is_in_range_for_degenerate_names() {
    for (m, d) in [
        ("", ""),
        ("a", ""),
        ("", "()V"),
        ("x", "(Ljava/lang/String;)V"),
    ] {
        assert!(name_override_memo_index(u32::MAX, m, d) < NAME_OVERRIDE_MEMO_ROWS);
    }
}
