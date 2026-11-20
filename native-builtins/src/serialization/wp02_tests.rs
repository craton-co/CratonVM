use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{FieldMetadata, MethodMetadata};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

const ACC_PUBLIC: u16 = 0x0001;
const ACC_PROTECTED: u16 = 0x0004;
const ACC_WP02_PRIVATE: u16 = 0x0002;
const ACC_WP02_TRANSIENT: u16 = 0x0080;

fn fm(name: &str, desc: &str, flags: u16, slot: usize, declaring: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: flags,
        slot_index: slot,
        declaring_class_id: declaring,
        is_static: (flags & 0x0008) != 0,
    }
}

fn mm(name: &str, desc: &str, flags: u16, declaring: ClassId) -> MethodMetadata {
    MethodMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: flags,
        declaring_class_id: declaring,
        exceptions: Vec::new(),
        signature: None,
    }
}

/// Build a Serializable class hierarchy on top of `MockNativeContext`:
///
///   Object (non-Serializable, no fields)
///   └── Base (non-Serializable, has `<init>()V`)
///        └── Foo (implements Serializable) { int a; int b; int c;
///             transient int skip; private void writeObject(...); }
///
/// Returns (ctx, foo_class_id).
fn build_foo_ctx() -> (MockNativeContext, ClassId) {
    let mut ctx = MockNativeContext::new();
    // Register via ensure_class_initialized so they all have ClassIds.
    let object = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let serializable = ctx
        .ensure_class_initialized("java/io/Serializable")
        .unwrap();
    let _externalizable = ctx
        .ensure_class_initialized("java/io/Externalizable")
        .unwrap();
    let _osc = ctx
        .ensure_class_initialized("java/io/ObjectStreamClass")
        .unwrap();
    let _osf = ctx
        .ensure_class_initialized("java/io/ObjectStreamField")
        .unwrap();
    let _method = ctx
        .ensure_class_initialized("java/lang/reflect/Method")
        .unwrap();
    let _ctor = ctx
        .ensure_class_initialized("java/lang/reflect/Constructor")
        .unwrap();
    let base = ctx.ensure_class_initialized("Base").unwrap();
    let foo = ctx.ensure_class_initialized("Foo").unwrap();

    // Hierarchy.
    ctx.set_superclass(base, object);
    ctx.set_superclass(foo, base);
    ctx.set_interfaces(foo, vec![serializable]);

    // Base's no-arg ctor.
    ctx.set_declared_methods(base, vec![mm("<init>", "()V", ACC_PUBLIC, base)]);
    // Foo: fields in declaration order (a, b, c, transient skip).
    ctx.set_declared_fields(
        foo,
        vec![
            fm("a", "I", ACC_PUBLIC, 0, foo),
            fm("b", "I", ACC_PUBLIC, 1, foo),
            fm("c", "I", ACC_PUBLIC, 2, foo),
            fm("skip", "I", ACC_PUBLIC | ACC_WP02_TRANSIENT, 3, foo),
        ],
    );
    // Foo's methods: private writeObject + protected writeReplace.
    ctx.set_declared_methods(
        foo,
        vec![
            mm(
                "writeObject",
                "(Ljava/io/ObjectOutputStream;)V",
                ACC_WP02_PRIVATE,
                foo,
            ),
            mm("writeReplace", "()Ljava/lang/Object;", ACC_PROTECTED, foo),
        ],
    );
    (ctx, foo)
}

fn mh_string_field(
    ctx: &MockNativeContext,
    mh: cratonvm_types::ObjectRef,
    slot: usize,
) -> String {
    match ctx.get_field(mh, slot) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        other => panic!("expected MethodHandle string slot {slot}, got {:?}", other),
    }
}

#[test]
fn reflection_factory_read_object_returns_null_without_private_hook() {
    let mut ctx = MockNativeContext::new();
    let no_hook = ctx.ensure_class_initialized("example/NoHook").unwrap();
    let mirror = ctx.get_class_mirror(no_hook);
    let result = native_reflection_factory_read_object(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(mirror))],
    )
    .expect("native must return normally");
    assert_eq!(result, Some(Value::Object(None)));
}

#[test]
fn reflection_factory_read_object_returns_special_handle_for_private_hook() {
    let mut ctx = MockNativeContext::new();
    let hook_class = ctx.ensure_class_initialized("example/HasHook").unwrap();
    ctx.set_declared_methods(
        hook_class,
        vec![mm(
            "readObject",
            "(Ljava/io/ObjectInputStream;)V",
            ACC_WP02_PRIVATE,
            hook_class,
        )],
    );
    let mirror = ctx.get_class_mirror(hook_class);

    let result = native_reflection_factory_read_object(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(mirror))],
    )
    .expect("native must return normally");
    let mh = match result {
        Some(Value::Object(Some(mh))) => mh,
        other => panic!("expected MethodHandle, got {:?}", other),
    };

    // Keep these in sync with lang_invoke's synthetic MethodHandle tail
    // slots: class, name, descriptor, kind start at slot 16.
    assert_eq!(mh_string_field(&ctx, mh, 16), "example/HasHook");
    assert_eq!(mh_string_field(&ctx, mh, 17), "readObject");
    assert_eq!(
        mh_string_field(&ctx, mh, 18),
        "(Ljava/io/ObjectInputStream;)V"
    );
    assert_eq!(ctx.get_field(mh, 19), Value::Int(2));
}

#[test]
fn reflection_factory_two_arg_constructor_returns_widened_copy() {
    let mut ctx = MockNativeContext::new();
    let target = ctx.ensure_class_initialized("example/CtorTarget").unwrap();
    let _ctor_class = ctx
        .ensure_class_initialized("java/lang/reflect/Constructor")
        .unwrap();
    let incoming =
        create_constructor_object(&mut ctx, &mm("<init>", "()V", ACC_PUBLIC, target)).unwrap();
    let target_mirror = ctx.get_class_mirror(target);

    let result = native_reflection_factory_new_constructor_for_serialization(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(target_mirror)),
            Value::Object(Some(incoming)),
        ],
    )
    .expect("native must return normally");
    let widened = match result {
        Some(Value::Object(Some(ctor))) => ctor,
        other => panic!("expected Constructor copy, got {:?}", other),
    };

    assert_ne!(widened, incoming);
    assert_eq!(
        read_constructor_descriptor(&ctx, widened).as_deref(),
        Some("()V")
    );
}

#[test]
fn reflection_factory_serialization_methods_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);

    for cls in [
        "sun/reflect/ReflectionFactory",
        "jdk/internal/reflect/ReflectionFactory",
    ] {
        assert!(r
            .find(
                cls,
                "readObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "writeObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "newConstructorForSerialization",
                "(Ljava/lang/Class;Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "hasStaticInitializerForSerialization",
                "(Ljava/lang/Class;)Z"
            )
            .is_some());
    }
}

#[test]
fn osc_lookup_returns_non_null_for_serializable() {
    let (mut ctx, foo) = build_foo_ctx();
    let desc = build_object_stream_class(&mut ctx, foo, false)
        .unwrap()
        .expect("lookup of a Serializable class must return a descriptor");
    // Slot 2 is field_count = 3 (a, b, c — skip is transient).
    assert_eq!(ctx.get_field(desc, 2), Value::Int(3));
    // Slot 3 has SC_SERIALIZABLE bit.
    let flags = match ctx.get_field(desc, 3) {
        Value::Int(n) => n,
        other => panic!("expected Int flags, got {:?}", other),
    };
    assert_eq!(flags & OSC_FLAG_SERIALIZABLE, OSC_FLAG_SERIALIZABLE);
}

#[test]
fn osc_lookup_returns_none_for_non_serializable() {
    let (mut ctx, _foo) = build_foo_ctx();
    let base = ctx.class_id_by_name("Base").unwrap();
    let result = build_object_stream_class(&mut ctx, base, false).unwrap();
    assert!(
        result.is_none(),
        "lookup(non-Serializable) must return None"
    );
}

#[test]
fn osc_lookup_any_returns_descriptor_for_non_serializable() {
    let (mut ctx, _foo) = build_foo_ctx();
    let base = ctx.class_id_by_name("Base").unwrap();
    let desc = build_object_stream_class(&mut ctx, base, true)
        .unwrap()
        .expect("lookupAny must return a descriptor even for non-Serializable");
    // field_count == 0 for non-Serializable lookupAny.
    assert_eq!(ctx.get_field(desc, 2), Value::Int(0));
    assert_eq!(ctx.get_field(desc, 3), Value::Int(0));
}

#[test]
fn osc_fields_are_in_declaration_order() {
    let (mut ctx, foo) = build_foo_ctx();
    let desc = build_object_stream_class(&mut ctx, foo, false)
        .unwrap()
        .expect("descriptor");
    let arr = match ctx.get_field(desc, 6) {
        Value::Object(Some(a)) => a,
        other => panic!("expected fields[] array, got {:?}", other),
    };
    assert_eq!(
        ctx.array_length(arr),
        3,
        "transient field must be excluded from fields[]"
    );
    let expected = ["a", "b", "c"];
    for (i, want) in expected.iter().enumerate() {
        let osf = match ctx.get_array_element(arr, i) {
            Value::Object(Some(o)) => o,
            other => panic!("expected ObjectStreamField at index {}, got {:?}", i, other),
        };
        let name = match ctx.get_field(osf, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            other => panic!("expected name string, got {:?}", other),
        };
        assert_eq!(name, *want, "fields[] must be in declaration order");
    }
}

/// Slot 6 is the declared `ObjectStreamField[]`, not `Object[]` (i8-L2).
#[test]
fn i8_l2_osc_fields_array_is_object_stream_field_typed() {
    let (mut ctx, foo) = build_foo_ctx();
    let osf = ctx
        .ensure_class_initialized("java/io/ObjectStreamField")
        .unwrap();
    let desc = build_object_stream_class(&mut ctx, foo, false)
        .unwrap()
        .expect("descriptor");
    let arr = match ctx.get_field(desc, 6) {
        Value::Object(Some(a)) => a,
        other => panic!("expected fields[] array, got {:?}", other),
    };
    assert_eq!(ctx.ref_array_component(arr), Some(osf));    }

#[test]
fn osc_cache_identity_two_lookups_same_ref() {
    let (mut ctx, foo) = build_foo_ctx();
    let d1 = build_object_stream_class(&mut ctx, foo, false)
        .unwrap()
        .expect("descriptor");
    let d2 = build_object_stream_class(&mut ctx, foo, false)
        .unwrap()
        .expect("descriptor");
    assert_eq!(
        d1, d2,
        "two lookup(cls) calls for the same class must return the same ObjectRef"
    );
}

#[test]
fn osc_finds_serializable_constructor_on_base() {
    // Foo's serializableConstructor should come from Base (the
    // closest non-Serializable ancestor's no-arg ctor).
    let (ctx, foo) = build_foo_ctx();
    let base = ctx.class_id_by_name("Base").unwrap();
    let result = find_serializable_constructor(&ctx, foo);
    let (anc, ctor) = result.expect("must find ctor in non-Serializable ancestor");
    assert_eq!(anc, base);
    assert_eq!(ctor.name, "<init>");
    assert_eq!(ctor.descriptor, "()V");
}

#[test]
fn osc_find_write_replace_method_when_present() {
    let (ctx, foo) = build_foo_ctx();
    let found = find_inheritable_method(&ctx, foo, "writeReplace", "()Ljava/lang/Object;");
    assert!(
        found.is_some(),
        "find_inheritable_method must locate writeReplace on Foo"
    );
}

#[test]
fn osc_find_write_replace_absent_returns_none() {
    let (ctx, _foo) = build_foo_ctx();
    let base = ctx.class_id_by_name("Base").unwrap();
    // Base has no writeReplace — walking up from Base also hits
    // `Object` which has none. Must be None.
    assert!(
        find_inheritable_method(&ctx, base, "writeReplace", "()Ljava/lang/Object;").is_none()
    );
}

#[test]
fn osc_has_write_object_slot_set_when_present() {
    let (mut ctx, foo) = build_foo_ctx();
    let desc = build_object_stream_class(&mut ctx, foo, false)
        .unwrap()
        .expect("descriptor");
    assert_eq!(ctx.get_field(desc, 4), Value::Int(1)); // has_write_object
    assert_eq!(ctx.get_field(desc, 5), Value::Int(0)); // has_read_object
}

#[test]
fn osc_for_class_returns_class_mirror() {
    let (mut ctx, foo) = build_foo_ctx();
    let desc = build_object_stream_class(&mut ctx, foo, false)
        .unwrap()
        .expect("descriptor");
    let mirror_val = ctx.get_field(desc, 7);
    assert!(
        matches!(mirror_val, Value::Object(Some(_))),
        "forClass slot (7) must be populated with the Class mirror"
    );
}
