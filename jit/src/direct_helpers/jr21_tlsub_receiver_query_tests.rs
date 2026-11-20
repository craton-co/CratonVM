use super::*;

unsafe extern "C" fn answer(ctx: i64, out: i64, n: i64) -> i64 {
    if ctx != 0x1000 || n != THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64 {
        return 0;
    }
    let io = out as *mut u64;
    // Class 77 reads the plain map and keeps `set`; 78 reads the plain
    // map but declares its own `set`; anything else: its bytecode.
    let (kind, set_ok) = match io.read() {
        77 => (u64::from(TL_RECEIVER_KIND_PLAIN), 1),
        78 => (u64::from(TL_RECEIVER_KIND_PLAIN), 0),
        _ => (2, 0),
    };
    io.write(kind);
    io.add(1).write(set_ok);
    2
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
fn jr21_tlsub_the_receiver_query() {
    let t = table(answer, 0x1000);
    assert_eq!(
        t.thread_local_receiver_kind(77),
        Some((TL_RECEIVER_KIND_PLAIN, true))
    );
    assert_eq!(
        t.thread_local_receiver_kind(78),
        Some((TL_RECEIVER_KIND_PLAIN, false))
    );
    assert_eq!(t.thread_local_receiver_kind(5), Some((2, false)));
    assert_eq!(
        t.thread_local_receiver_kind(0),
        None,
        "no class, no question"
    );
    assert_eq!(table(decline, 0x1000).thread_local_receiver_kind(77), None);
    assert_eq!(table(answer, 0).thread_local_receiver_kind(77), None);
    assert_eq!(
        DirectHelperTable::default().thread_local_receiver_kind(77),
        None
    );
}
