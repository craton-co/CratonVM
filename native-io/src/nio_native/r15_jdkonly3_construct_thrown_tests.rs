use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const FAEE: &str = "java/nio/file/FileAlreadyExistsException";

/// The constructor is asked for, with the path, and its object is thrown.
#[test]
fn file_already_exists_runs_the_one_string_constructor() {
    let mut ctx = MockNativeContext::new();
    let thrown = file_already_exists(&mut ctx, "a");
    let MethodCallFailed::ExceptionThrown(exc) = &thrown else {
        panic!("expected a thrown FileAlreadyExistsException, got {thrown:?}");
    };
    let exc = *exc;
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(exc)).as_deref(),
        Some(FAEE)
    );
    let inits: Vec<_> = ctx
        .recorded_calls()
        .iter()
        .filter(|c| c.method_name == "<init>")
        .collect();
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0].descriptor, "(Ljava/lang/String;)V");
    assert_eq!(inits[0].args.first(), Some(&Value::Object(Some(exc))));
}

/// A constructor that threw used to be ignored and the half-built object
/// thrown; now the flat fallback is the answer.
#[test]
fn a_constructor_that_threw_is_not_thrown_half_built() {
    let mut ctx = MockNativeContext::new();
    ctx.script_invoke(
        "<init>",
        "(Ljava/lang/String;)V",
        Err(io_error("constructor refused")),
    );
    let thrown = file_already_exists(&mut ctx, "a");
    assert!(
        matches!(
            &thrown,
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
                message
            })) if message == "FileAlreadyExistsException: a"
        ),
        "got {thrown:?}"
    );
}

/// Round 15 wave 4, lane jdkonly4 (JO3-2): a constructor that threw a
/// JAVA throwable hands that throwable back, as HotSpot's `new` does. It
/// used to be read as "cannot construct" and the flat `IOException` was
/// thrown in its place.
#[test]
fn a_constructor_that_threw_a_throwable_propagates_it() {
    let mut ctx = MockNativeContext::new();
    let oome = ctx.alloc_object_with_class(0, "java/lang/OutOfMemoryError");
    ctx.script_invoke(
        "<init>",
        "(Ljava/lang/String;)V",
        Err(MethodCallFailed::ExceptionThrown(oome)),
    );
    let thrown = file_already_exists(&mut ctx, "a");
    assert!(
        matches!(&thrown, MethodCallFailed::ExceptionThrown(t) if *t == oome),
        "got {thrown:?}"
    );
}

/// The fallback runs only when nothing was built, never beside a
/// constructed object.
#[test]
fn the_fallback_is_not_evaluated_when_the_object_is_built() {
    let mut ctx = MockNativeContext::new();
    let thrown = construct_thrown(&mut ctx, FAEE, "()V", &[], || {
        panic!("the constructor built the object; the fallback must not run")
    });
    assert!(
        matches!(thrown, MethodCallFailed::ExceptionThrown(_)),
        "got {thrown:?}"
    );
}
