use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj_of(v: MethodCallResult) -> ObjectRef {
    match v {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    }
}

/// JIT round 21 wave 1, lane updaters: an updater whose `cclass` is not
/// its `tclass` (a protected field reached from a foreign-package
/// subclass) admits only `cclass` instances: a bare `tclass` instance is
/// a thrown `RuntimeException(IllegalAccessException)`, a null target the
/// helpful NPE of `obj.getClass()`; an updater whose `cclass` word is its
/// `tclass` keeps the old `ClassCastException`.
#[test]
fn r21_updaters_protected_cclass_narrows_the_receiver() {
    let mut ctx = mock_ctx();
    let base = ctx.ensure_class_initialized("a/Base").unwrap();
    let caller = ctx.ensure_class_initialized("b/Caller").unwrap();
    let below = ctx.ensure_class_initialized("b/Below").unwrap();
    ctx.set_superclass(caller, base);
    ctx.set_superclass(below, caller);
    let imp = ctx.ensure_class_initialized("x/Impl").unwrap();
    let updater = ctx.alloc_object(imp, FU_NUM_SLOTS);
    // Casts: small mock class ids.
    ctx.set_field(updater, FU_SLOT_TCLASS_ID, Value::Int(base.as_u32() as i32));
    ctx.set_field(updater, FU_SLOT_CCLASS_ID, Value::Int(caller.as_u32() as i32));
    let base_obj = obj_of(ctx.new_object("a/Base"));
    let below_obj = obj_of(ctx.new_object("b/Below"));
    let ok = require_target(&mut ctx, updater, &[Value::Object(None), Value::Object(Some(below_obj))], 1);
    assert_eq!(ok.ok(), Some(below_obj));
    let err = require_target(&mut ctx, updater, &[Value::Object(None), Value::Object(Some(base_obj))], 1)
        .unwrap_err();
    assert!(
        matches!(err, MethodCallFailed::ExceptionThrown(_)),
        "a bare tclass instance must be RuntimeException(IllegalAccessException), got {err:?}"
    );
    let err = require_target(&mut ctx, updater, &[Value::Object(None), Value::Object(None)], 1)
        .unwrap_err();
    assert!(
        matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::NullPointerException { message: Some(ref m) }
            )) if m == "Cannot invoke \"Object.getClass()\" because \"obj\" is null"
        ),
        "a null target must be the getClass() NPE, got {err:?}"
    );
    // `cclass == tclass`: the old door.
    ctx.set_field(updater, FU_SLOT_CCLASS_ID, Value::Int(base.as_u32() as i32));
    let other = obj_of(ctx.new_object("c/Other"));
    let err = require_target(&mut ctx, updater, &[Value::Object(None), Value::Object(Some(other))], 1)
        .unwrap_err();
    assert!(matches!(
        err,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::ClassCastException { .. }
        ))
    ));
    assert_eq!(
        require_target(&mut ctx, updater, &[Value::Object(None), Value::Object(Some(base_obj))], 1).ok(),
        Some(base_obj)
    );
}

/// JIT round 21 wave 1, lane updaters: the reference updater's
/// `valueCheck` -- null, the declared class and a subclass pass; another
/// class and an array (for a non-`Object` vclass) are a
/// `ClassCastException`; an updater with no vclass word checks nothing.
#[test]
fn r21_updaters_reference_updater_value_check_matches_the_jdk() {
    let mut ctx = mock_ctx();
    let foo = ctx.ensure_class_initialized("p/Foo").unwrap();
    let sub = ctx.ensure_class_initialized("p/SubFoo").unwrap();
    ctx.set_superclass(sub, foo);
    let foo_obj = obj_of(ctx.new_object("p/Foo"));
    let sub_obj = obj_of(ctx.new_object("p/SubFoo"));
    let other_obj = obj_of(ctx.new_object("p/Other"));
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    assert!(foo.as_u32() > 0, "the mock hands out non-zero class ids");
    // Cast: a small mock class id.
    let word = Value::Int(foo.as_u32() as i32);
    assert!(require_value_of(&mut ctx, word, Value::Object(None)).is_ok());
    assert!(require_value_of(&mut ctx, word, Value::Object(Some(foo_obj))).is_ok());
    assert!(require_value_of(&mut ctx, word, Value::Object(Some(sub_obj))).is_ok());
    assert!(require_value_of(&mut ctx, Value::Int(0), Value::Object(Some(other_obj))).is_ok());
    for bad in [other_obj, arr] {
        let err = require_value_of(&mut ctx, word, Value::Object(Some(bad))).unwrap_err();
        assert!(
            matches!(
                err,
                MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                    RuntimeError::ClassCastException { ref message }
                )) if message.is_empty()
            ),
            "a wrong-typed value must be a message-less ClassCastException, got {err:?}"
        );
    }
}
