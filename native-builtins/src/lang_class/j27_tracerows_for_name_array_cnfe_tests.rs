//! j27 wave 4 (lane TRACEROWS, TR2W2-6): `Class.forName` of an array
//! descriptor whose element is missing rethrows the LOADER's
//! `ClassNotFoundException` (renamed to the element), so its backtrace --
//! the loader frames HotSpot lists -- survives; a fresh exception had none.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::NativeHeapAccess;

fn instance_of(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(obj)))) => obj,
        other => panic!("no instance of {class}: {other:?}"),
    }
}

#[test]
fn the_loaders_cnfe_is_rethrown_for_an_array_name() {
    let mut ctx = mock_ctx();
    let thrown = instance_of(&mut ctx, "java/lang/ClassNotFoundException");
    let out = for_name_rename_array_cnfe(
        &mut ctx,
        "[Lr15.Missing;",
        MethodCallFailed::ExceptionThrown(thrown),
    );
    match out {
        MethodCallFailed::ExceptionThrown(o) => assert_eq!(o.as_ptr(), thrown.as_ptr()),
        other => panic!("a fresh throwable loses the loader frames: {other:?}"),
    }
}

#[test]
fn other_throwables_and_plain_names_are_untouched() {
    let mut ctx = mock_ctx();
    let ncdfe = instance_of(&mut ctx, "java/lang/NoClassDefFoundError");
    let out = for_name_rename_array_cnfe(
        &mut ctx,
        "[Lr15.Missing;",
        MethodCallFailed::ExceptionThrown(ncdfe),
    );
    assert!(matches!(out, MethodCallFailed::ExceptionThrown(o) if o.as_ptr() == ncdfe.as_ptr()));
    let cnfe = instance_of(&mut ctx, "java/lang/ClassNotFoundException");
    let out = for_name_rename_array_cnfe(
        &mut ctx,
        "r15.Missing",
        MethodCallFailed::ExceptionThrown(cnfe),
    );
    assert!(matches!(out, MethodCallFailed::ExceptionThrown(o) if o.as_ptr() == cnfe.as_ptr()));
}
