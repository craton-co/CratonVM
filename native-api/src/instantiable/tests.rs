use super::*;

/// The invariant the callers rely on: whatever comes back, `new` could
/// have produced it. Written as an invariant rather than as "an
/// unresolvable name answers None" because a mock — and the real VM in
/// synthetic-JDK mode — is allowed to FABRICATE a class for an unknown
/// name, and a fabricated class is concrete and therefore a legitimate
/// answer here.
#[test]
fn whatever_is_returned_is_something_new_could_produce() {
    let mut ctx = crate::test_mock::MockNativeContext::new();
    if let Some((_, cid)) = first_instantiable(&mut ctx, &["does/not/Exist"]) {
        assert!(class_is_instantiable(&ctx, cid));
    }
}
