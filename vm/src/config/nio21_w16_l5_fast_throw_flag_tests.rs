use super::*;

#[test]
fn nio21_w16_l5_explicit_xx_flag_beats_the_switch() {
    assert!(!resolve_omit_stack_trace_in_fast_throw(Some(false), || {
        true
    }));
    assert!(resolve_omit_stack_trace_in_fast_throw(Some(true), || false));
    assert!(resolve_omit_stack_trace_in_fast_throw(None, || true));
    assert!(!resolve_omit_stack_trace_in_fast_throw(None, || false));
    assert_eq!(VmConfig::default().omit_stack_trace_in_fast_throw, None);
}
