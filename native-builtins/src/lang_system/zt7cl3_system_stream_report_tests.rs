use super::*;
use crate::test_utils::MockNativeContext;

#[test]
fn zt7cl3_the_report_has_a_line_per_stream_even_when_nothing_resolves() {
    let ctx = MockNativeContext::new();
    let lines = system_stream_report(&ctx);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("System.out: "), "{lines:?}");
    assert!(lines[1].starts_with("System.err: "), "{lines:?}");
}
