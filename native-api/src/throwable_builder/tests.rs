use super::*;
use crate::test_mock::MockNativeContext;

/// The mock's constructor path succeeds: the constructed object is the
/// answer and the fallback never runs.
#[test]
fn a_constructed_throwable_is_thrown_and_the_fallback_is_not_asked() {
    let mut ctx = MockNativeContext::new();
    let failed = throw_new(
        &mut ctx,
        "java/lang/IllegalStateException",
        "()V",
        &[],
        || panic!("the constructor built the object; the fallback must not run"),
    );
    assert!(matches!(failed, MethodCallFailed::ExceptionThrown(_)));
    let built = construct_new(&mut ctx, "java/lang/IllegalStateException", "()V", &[]);
    assert!(matches!(built, Ok(Some(_))));
}
