use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};

/// Every JDK this VM is gated against installs a `constructorAccessor` on
/// the `Constructor` that `ReflectionFactory.newConstructorForSerialization`
/// returns -- but NOT the same one, and the difference is not configurable:
///
///     JDK 21.0.12+8  ->  jdk/internal/reflect/GeneratedSerializationConstructorAccessorN
///     JDK 25.0.4+7   ->  jdk/internal/reflect/DirectConstructorHandleAccessor
///
/// The 25-shaped object carries the target type in `target.instanceClass`.
/// The 21-shaped one carries NOTHING -- it declares no instance fields at
/// all -- so the only way to honour the serialization contract on 21 is to
/// run the accessor's own `newInstance` bytecode.
///
/// Before the delegating arm existed, the 21 shape matched no arm and fell
/// through to allocating the constructor's DECLARING class (the first
/// non-serializable ancestor), which is the whole JDK 21 serialization
/// defect. It was silent whenever that ancestor was concrete: `Integer`
/// deserialized to a bare `java.lang.Object` and threw nothing.
///
/// These tests are written against the mock precisely because the defect is
/// invisible to any test run on a JDK 25 image -- and a JDK 25 image is what
/// CI has.
fn constructor_with_accessor_named(
    ctx: &mut MockNativeContext,
    accessor_class: &str,
) -> ObjectRef {
    let ctor_cid = ctx
        .ensure_class_initialized("java/lang/reflect/Constructor")
        .expect("declare Constructor");
    // `constructorAccessor` is slot 0 and `clazz` slot 1 for this test only;
    // the mock resolves names through `set_declared_fields`, which outranks
    // its built-in tables.
    ctx.set_declared_fields(
        ctor_cid,
        vec![
            FieldMetadata {
                name: "constructorAccessor".to_string(),
                descriptor: "Ljdk/internal/reflect/ConstructorAccessor;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: ctor_cid,
                is_static: false,
            },
            FieldMetadata {
                name: "clazz".to_string(),
                descriptor: "Ljava/lang/Class;".to_string(),
                access_flags: 0,
                slot_index: 1,
                declaring_class_id: ctor_cid,
                is_static: false,
            },
        ],
    );
    let ctor = ctx.alloc_object(ctor_cid, 4);

    let acc_cid = ctx
        .ensure_class_initialized(accessor_class)
        .expect("declare accessor class");
    let acc = ctx.alloc_object(acc_cid, 0);
    ctx.set_field_by_name(ctor, "constructorAccessor", Value::Object(Some(acc)));
    ctor
}

/// The delegation must be a call to the accessor's OWN `newInstance`, with
/// the JDK signature. Asserting only "something was returned" would pass
/// for a fallthrough that happened to produce an object.
fn record_call(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "newInstance" && descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;" {
        // A sentinel Int is not a legal `newInstance` return in production;
        // it is used here BECAUSE it cannot be confused with anything the
        // fallthrough path could construct.
        Some(Ok(Some(Value::Int(0x5E21))))
    } else {
        None
    }
}

#[test]
fn a_jdk21_generated_serialization_accessor_is_delegated_to() {
    let mut ctx = MockNativeContext::new();
    let ctor = constructor_with_accessor_named(
        &mut ctx,
        "jdk/internal/reflect/GeneratedSerializationConstructorAccessor1",
    );
    ctx.set_invoke_virtual_hook(record_call);

    let got = native_constructor_new_instance(
        &mut ctx,
        &[Value::Object(Some(ctor)), Value::Object(None)],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Int(0x5E21)),
        "the JDK 21 generated serialization accessor was NOT delegated to; \
         control fell through to the generic path, which allocates the \
         DECLARING class -- that is the JDK 21 serialization defect"
    );
}

/// Same accessor family, a higher generation counter. The JDK numbers these
/// per generated class (`...Accessor1`, `...Accessor2`, ...), so an
/// equality test against a single name would pass the first case in a
/// process and fail every one after it.
#[test]
fn the_generation_counter_in_the_accessor_name_is_not_part_of_the_match() {
    let mut ctx = MockNativeContext::new();
    let ctor = constructor_with_accessor_named(
        &mut ctx,
        "jdk/internal/reflect/GeneratedSerializationConstructorAccessor47",
    );
    ctx.set_invoke_virtual_hook(record_call);

    let got = native_constructor_new_instance(
        &mut ctx,
        &[Value::Object(Some(ctor)), Value::Object(None)],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Int(0x5E21)),
        "only the FIRST generated accessor in a process would be handled"
    );
}

/// The control that makes the two tests above mean something: an accessor
/// this arm must NOT claim. Without it, a `starts_with` that matched
/// everything would pass both tests above and silently divert every
/// ordinary reflective construction into the accessor.
/// The control that matters most, because it is one word away from a match.
/// The JDK ALSO generates `GeneratedConstructorAccessorN` -- no
/// `Serialization` in the name -- for ordinary reflective construction under
/// the old inflation scheme. Those must keep taking the generic path;
/// delegating them would route every ordinary
/// `Constructor.newInstance` through an accessor and skip the JPMS and
/// argument-arity checks below. A match loosened to, say,
/// `contains("ConstructorAccessor")` passes every other test in this module
/// and fails this one.
#[test]
fn the_ordinary_generated_constructor_accessor_is_not_delegated_to() {
    let mut ctx = MockNativeContext::new();
    let ctor = constructor_with_accessor_named(
        &mut ctx,
        "jdk/internal/reflect/GeneratedConstructorAccessor1",
    );
    ctx.set_invoke_virtual_hook(record_call);

    let got = native_constructor_new_instance(
        &mut ctx,
        &[Value::Object(Some(ctor)), Value::Object(None)],
    );

    assert_ne!(
        got.ok().flatten(),
        Some(Value::Int(0x5E21)),
        "an ORDINARY generated constructor accessor was routed into the serialization arm -- the match is too loose"
    );
}

#[test]
fn an_unrelated_accessor_class_is_not_delegated_to() {
    let mut ctx = MockNativeContext::new();
    let ctor = constructor_with_accessor_named(
        &mut ctx,
        "jdk/internal/reflect/NativeConstructorAccessorImpl",
    );
    ctx.set_invoke_virtual_hook(record_call);

    let got = native_constructor_new_instance(
        &mut ctx,
        &[Value::Object(Some(ctor)), Value::Object(None)],
    );

    assert_ne!(
        got.ok().flatten(),
        Some(Value::Int(0x5E21)),
        "an unrelated accessor was routed into the serialization arm"
    );
}
