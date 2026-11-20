use super::tl_trace_property_value as trace_on;

#[test]
fn tl21_the_bare_define_and_true_trace_and_everything_else_does_not() {
    assert!(trace_on(Some("")));
    assert!(trace_on(Some("true")));
    assert!(trace_on(Some("TRUE")));
    assert!(!trace_on(None));
    assert!(!trace_on(Some("false")));
    assert!(!trace_on(Some("1")));
    assert!(!trace_on(Some("yes")));
}
