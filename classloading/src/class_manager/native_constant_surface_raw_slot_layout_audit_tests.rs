use super::*;

#[test]
fn native_constant_surface_raw_slot_layout_audit() {
    // These issue-surface classes are constructed through bytecode `new` and
    // handled through positional native fields. Explicit
    // `alloc_concurrent_synthetic(..., n)` call sites reserve their own
    // capacity. Keep this manifest beside the fallback allocator so a short
    // layout cannot silently reappear while these native surfaces evolve.
    //
    // A hand-written manifest catches these eight and only these eight. The
    // tree-wide form is `t9c_synthetic_field_tables_cover_their_factories`
    // (`vm/tests/tier1_tests.rs`), which derives the minimum from every literal
    // `alloc_concurrent_synthetic(ctx, "cls", n)` site rather than a list, and
    // reads the declared width by CALLING this table. Adding a class here is
    // still worth doing when its factory count is NOT a literal, which is the
    // one case the tree-wide gate cannot see.
    for (class, minimum_slots) in [
        ("java/net/DatagramSocket", 4),
        ("java/net/DatagramPacket", 5),
        ("java/util/prefs/Preferences", 6),
        ("java/util/prefs/AbstractPreferences", 6),
        ("com/sun/net/httpserver/HttpServer", 6),
        ("com/sun/net/httpserver/HttpServerImpl", 6),
        ("sun/net/httpserver/HttpServerImpl", 6),
        ("com/sun/net/httpserver/HttpExchange", 13),
        ("java/lang/StackWalker$StackFrame", 8),
    ] {
        assert!(
            synthetic_stub_fields(class).len() >= minimum_slots,
            "{class} needs at least {minimum_slots} synthetic fields for its native raw-slot surface",
        );
    }
}
