/// gcg g6/oo (door 4): both `StackTraceElement[]` builders allocate their
/// elements through the reclaiming door, and the placeholder backfill too;
/// none keeps the one-attempt allocator.
#[test]
fn gcg_g6oo_stack_trace_elements_are_allocated_through_the_reclaiming_door() {
    let src = include_str!("../lang_misc.rs");
    let body = |head: &str| -> String {
        src.split(head)
            .nth(1)
            .and_then(|rest| rest.split("\npub(crate) fn ").next())
            .expect("function body")
            .to_string()
    };
    for head in [
        "pub fn native_init_stack_trace_elements(",
        "pub fn native_throwable_get_stack_trace_array(",
    ] {
        let b = body(head);
        assert!(b.contains("allocator.alloc_reclaiming("));
        assert!(!b.contains("allocator.alloc(&mut"));
    }
    let init = body("pub fn native_init_stack_trace_elements(");
    assert!(init.contains("try_alloc_concurrent_synthetic_reclaiming("));
}
