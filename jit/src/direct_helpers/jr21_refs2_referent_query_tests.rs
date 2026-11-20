use super::*;

unsafe extern "C" fn referent_only(ctx: i64, out: i64, n: i64) -> i64 {
    if ctx != 0x1000 || n != REFERENCE_REFERENT_QUERY_WORDS as i64 {
        return 0;
    }
    (out as *mut u64).write(0);
    1
}

unsafe extern "C" fn decline(_ctx: i64, _out: i64, _n: i64) -> i64 {
    0
}

fn table(f: DirectHelperFnThreadLocalIrLayout, ctx: usize) -> DirectHelperTable {
    DirectHelperTable {
        thread_local_ir_layout: f as *const () as usize,
        static_base_resolver_ctx: ctx,
        ..DirectHelperTable::default()
    }
}

#[test]
fn jr21_refs2_the_referent_query() {
    assert_eq!(
        table(referent_only, 0x1000).reference_referent_index(),
        Some(0)
    );
    assert_eq!(table(decline, 0x1000).reference_referent_index(), None);
    assert_eq!(table(referent_only, 0).reference_referent_index(), None);
    // The full layout is not the referent query's answer.
    assert_eq!(
        table(referent_only, 0x1000).thread_local_ir_layout_words(),
        None
    );
}
