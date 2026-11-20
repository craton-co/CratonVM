use super::*;

unsafe extern "C" fn fill(ctx: i64, out: i64, n: i64) -> i64 {
    if ctx != 0x1000 || n < THREAD_LOCAL_IR_LAYOUT_WORDS as i64 {
        return 0;
    }
    for i in 0..THREAD_LOCAL_IR_LAYOUT_WORDS {
        (out as *mut u64).add(i).write(100 + i as u64);
    }
    THREAD_LOCAL_IR_LAYOUT_WORDS as i64
}

unsafe extern "C" fn decline(_ctx: i64, _out: i64, _n: i64) -> i64 {
    0
}

/// The reader asks with the published context and takes only a full
/// answer; no word, no context or a decline answer `None`.
#[test]
fn nio21w19_the_layout_reader_takes_only_a_full_answer() {
    let t = |f: DirectHelperFnThreadLocalIrLayout, ctx: usize| DirectHelperTable {
        thread_local_ir_layout: f as *const () as usize,
        static_base_resolver_ctx: ctx,
        ..DirectHelperTable::EMPTY
    };
    let got = t(fill, 0x1000)
        .thread_local_ir_layout_words()
        .expect("full answer");
    assert_eq!(got[0], 100);
    assert_eq!(got[9], 109);
    assert_eq!(got[14], 114);
    assert!(t(fill, 0).thread_local_ir_layout_words().is_none());
    assert!(t(decline, 0x1000).thread_local_ir_layout_words().is_none());
    assert!(DirectHelperTable::EMPTY
        .thread_local_ir_layout_words()
        .is_none());
}
