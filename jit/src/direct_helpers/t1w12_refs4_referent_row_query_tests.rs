use super::*;

unsafe extern "C" fn row_answer(ctx: i64, out: i64, n: i64) -> i64 {
    if ctx != 0x1000 || n != REFERENCE_REFERENT_ROW_QUERY_WORDS as i64 {
        return 0;
    }
    let o = out as *mut u64;
    o.write(0);
    o.add(1).write(9);
    o.add(2).write(77);
    o.add(3).write(11);
    REFERENCE_REFERENT_ROW_ANSWER_WORDS as i64
}

/// j27 wave 5 (lane REFS): the pre-j27 three-word answer.
unsafe extern "C" fn three_word_answer(_ctx: i64, out: i64, _n: i64) -> i64 {
    let o = out as *mut u64;
    o.write(0);
    o.add(1).write(9);
    o.add(2).write(77);
    3
}

unsafe extern "C" fn short_answer(_ctx: i64, out: i64, _n: i64) -> i64 {
    (out as *mut u64).write(0);
    1
}

fn table(f: DirectHelperFnThreadLocalIrLayout, ctx: usize) -> DirectHelperTable {
    DirectHelperTable {
        thread_local_ir_layout: f as *const () as usize,
        static_base_resolver_ctx: ctx,
        ..DirectHelperTable::EMPTY
    }
}

#[test]
fn t1w12_refs4_the_row_query() {
    assert_eq!(
        table(row_answer, 0x1000).reference_referent_row(),
        Some((0, 9))
    );
    // The row producer is not a one-word index answer, and vice versa.
    assert_eq!(table(row_answer, 0x1000).reference_referent_index(), None);
    assert_eq!(table(short_answer, 0x1000).reference_referent_row(), None);
    assert_eq!(table(row_answer, 0).reference_referent_row(), None);
    assert_eq!(DirectHelperTable::EMPTY.reference_referent_row(), None);
    // Every query size the producer tells apart is its own.
    for other in [
        REFERENCE_REFERENT_QUERY_WORDS,
        THREAD_LOCAL_RECEIVER_QUERY_WORDS,
        THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS,
        REFERENCE_GET_LAYOUT_QUERY_WORDS,
        THREAD_LOCAL_IR_LAYOUT_WORDS,
    ] {
        assert_ne!(REFERENCE_REFERENT_ROW_QUERY_WORDS, other);
    }
    assert!(REFERENCE_REFERENT_ROW_ANSWER_WORDS <= REFERENCE_REFERENT_ROW_QUERY_WORDS);
    assert!(REFERENCE_REFERENT_ROW_QUERY_WORDS < THREAD_LOCAL_IR_LAYOUT_WORDS);
}

/// j27 wave 5 (lane REFS): word 3 is the compare-only row, read only from
/// a full answer; a short (three-word) answer is no answer at all.
#[test]
fn j27_refs_the_compare_row_is_word_three() {
    assert_eq!(
        table(row_answer, 0x1000).reference_referent_compare_row(),
        Some(11)
    );
    assert_eq!(table(row_answer, 0x1000).reference_referent_row(), Some((0, 9)));
    assert_eq!(table(three_word_answer, 0x1000).reference_referent_compare_row(), None);
    assert_eq!(table(three_word_answer, 0x1000).reference_referent_row(), None);
    assert_eq!(DirectHelperTable::EMPTY.reference_referent_compare_row(), None);
}
