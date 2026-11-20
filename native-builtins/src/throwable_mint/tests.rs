use super::*;
use cratonvm_native_api::test_mock::MockNativeContext;
// The mock implements `create_string` through the trait.
use cratonvm_native_api::NativeHeapAccess;

fn unreachable_mint(_ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    panic!("the constructor could build the object; the bare mint must not run")
}

/// The mock's `new_object_initialized` (the trait default: `new_object` +
/// `<init>`) succeeds, so the constructed object is the answer and the
/// mint -- the pre-wave-3 behaviour of every site -- is never reached.
#[test]
fn the_constructor_is_asked_before_the_mint() {
    let mut ctx = MockNativeContext::new();
    let file = ctx.create_string("f");
    let file = Value::Object(Some(file));
    let spec = ThrowableCtor {
        class: "java/nio/file/NoSuchFileException",
        init_desc: "(Ljava/lang/String;)V",
        init_args: &[file],
        mint: unreachable_mint,
        message: Value::Object(None),
        fields: &[("file", file)],
        null_cause: false,
    };
    let exc = construct_or_mint(&mut ctx, &spec);
    assert!(exc.is_ok());
}
