use super::{
    scoped_session_never_closing_class_ids, WELL_KNOWN_SESSION_GLOBAL, WELL_KNOWN_SESSION_HEAP,
    WELL_KNOWN_SESSION_IMPLICIT,
};

unsafe extern "C" fn three_sessions(ctx: i64, which: i64) -> i64 {
    if ctx != 0x1000 {
        return 0;
    }
    match which {
        WELL_KNOWN_SESSION_HEAP => 71,
        WELL_KNOWN_SESSION_GLOBAL => 70,
        WELL_KNOWN_SESSION_IMPLICIT => 72,
        // Any other selector (the Integer / String / cache ones) must not
        // leak into the session list.
        _ => 5,
    }
}

unsafe extern "C" fn only_heap_loaded(_ctx: i64, which: i64) -> i64 {
    match which {
        WELL_KNOWN_SESSION_HEAP => 71,
        _ => 0,
    }
}

unsafe extern "C" fn out_of_range(_ctx: i64, _which: i64) -> i64 {
    i64::from(u32::MAX) + 1
}

fn table(f: unsafe extern "C" fn(i64, i64) -> i64, ctx: usize) -> crate::DirectHelperTable {
    crate::DirectHelperTable {
        well_known_class_id: f as *const () as usize,
        static_base_resolver_ctx: ctx,
        ..crate::DirectHelperTable::EMPTY
    }
}

