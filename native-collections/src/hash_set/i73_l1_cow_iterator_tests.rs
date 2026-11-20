use super::*;
use cratonvm_native_api::test_mock::MockNativeContext;

/// A class library with no real `COWIterator` constructor (the mock's
/// `method_exists` answers false) mints nothing: the caller keeps its
/// `ArrayList$Itr` wrapper rather than fabricating a stub iterator.
#[test]
fn no_real_cow_iterator_mints_nothing() {
    let mut mock = MockNativeContext::new();
    let snap = mock.fresh_object_ref();
    let ctx: &mut dyn NativeContext = &mut mock;
    let pin = ctx.pin_native_root(snap);
    let got = real_cowal_iterator(ctx, pin, snap).unwrap();
    assert!(got.is_none());
}
