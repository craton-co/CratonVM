/// No stand-in row of any family is named `fillInStackTrace`
/// ([`super::innermost_calls_fill_in_stack_trace`]'s premise). Moved into
/// this test module in JIT round 22 wave 7 (lane review): the test was its
/// only caller.
fn fill_call_is_no_standin_row() -> bool {
    super::STANDIN_TABLE
        .iter()
        .all(|(_, rows)| rows.iter().all(|m| m.name != "fillInStackTrace"))
}

#[test]
fn no_standin_row_is_a_fill_call() {
    assert!(fill_call_is_no_standin_row());
}
