// Round-10 fix [STUB/jit-api]: this module currently has zero
// consumers in-workspace (the `gpu-lowering` feature is off by
// default and no other crate enables it — see the crate-level
// "`gpu-lowering` feature status" note in `lib.rs`). Rather than
// delete the public seam blindly, exercise the error type's
// `Display`/`Error` impls so the code is at least covered when the
// feature *is* built, and a regression in the human-facing messages
// is caught. These tests only compile/run under `--features
// gpu-lowering` because the whole module is gated on it.
use super::*;

#[test]
fn lowering_error_display_unsupported() {
    let e = LoweringError::Unsupported("invokedynamic");
    assert_eq!(e.to_string(), "unsupported: invokedynamic");
}

#[test]
fn lowering_error_display_internal() {
    let e = LoweringError::Internal("ptx emit failed".to_string());
    assert_eq!(e.to_string(), "internal error: ptx emit failed");
}

#[test]
fn lowering_error_is_std_error() {
    // Confirm the `std::error::Error` impl is wired (usable as a
    // boxed trait object, and `source()` is the default `None`).
    fn assert_error<E: std::error::Error>(_: &E) {}
    let e = LoweringError::Unsupported("athrow");
    assert_error(&e);
    let boxed: Box<dyn std::error::Error> = Box::new(LoweringError::Internal("x".into()));
    assert_eq!(boxed.to_string(), "internal error: x");
    assert!(std::error::Error::source(&e).is_none());
}

#[test]
fn lowering_error_debug_is_nonempty() {
    // `#[derive(Debug)]` must keep producing something useful for
    // log lines / panics.
    let e = LoweringError::Unsupported("monitorenter");
    assert!(format!("{e:?}").contains("Unsupported"));
}
