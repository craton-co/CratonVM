// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP8.10.10 -- `System.getenv()` regression: ensure the returned Map is the
//! OpenJDK-shaped unmodifiable wrapper whose private backing field `m` points
//! at a real `java/util/HashMap`.
//!
//! The wrapper preserves the prior HashMap-dispatch fix because all map reads
//! delegate to the backing map, while also matching libraries such as System
//! Rules that reflect on `System.getenv().getClass().getDeclaredField("m")`.

use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::{NativeContextImpl, Vm};

#[test]
fn system_getenv_returns_unmodifiable_map_with_hashmap_backing() {
    let mut vm = Vm::new(VmConfig::default());

    let getenv = vm
        .shared
        .natives
        .native_methods
        .find("java/lang/System", "getenv", "()Ljava/util/Map;")
        .expect("System.getenv()Map must be registered");
    let object_get_class = vm
        .shared
        .natives
        .native_methods
        .find("java/lang/Object", "getClass", "()Ljava/lang/Class;")
        .expect("Object.getClass must be registered");
    let class_get_name = vm
        .shared
        .natives
        .native_methods
        .find("java/lang/Class", "getName", "()Ljava/lang/String;")
        .expect("Class.getName must be registered");

    let mut ctx = NativeContextImpl {
        shared: &vm.shared,
        thread: &mut vm.main_thread,
    };

    let map_ref = match getenv(&mut ctx, &[]).expect("getenv must not error") {
        Some(Value::Object(Some(o))) => o,
        other => panic!("System.getenv() must return non-null Map, got {other:?}"),
    };

    let map_class = match object_get_class(&mut ctx, &[Value::Object(Some(map_ref))])
        .expect("Object.getClass must not error")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("System.getenv().getClass() returned {other:?}"),
    };
    let class_name_obj = match class_get_name(&mut ctx, &[Value::Object(Some(map_class))])
        .expect("Class.getName must not error")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Class.getName returned {other:?}"),
    };
    assert_eq!(
        ctx.read_string(class_name_obj).as_deref(),
        Some("java.util.Collections$UnmodifiableMap"),
        "System.getenv() should report the same wrapper class shape as HotSpot"
    );

    let m_name = ctx.create_string("m");
    let m_field = match ctx
        .invoke_virtual(
            map_class,
            "getDeclaredField",
            "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
            &[Value::Object(Some(m_name))],
        )
        .expect("getDeclaredField(\"m\") must not throw")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("getDeclaredField(\"m\") returned {other:?}"),
    };
    let field_name_obj = match ctx
        .invoke_virtual(m_field, "getName", "()Ljava/lang/String;", &[])
        .expect("Field.getName must not error")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Field.getName returned {other:?}"),
    };
    assert_eq!(ctx.read_string(field_name_obj).as_deref(), Some("m"));

    ctx.invoke_virtual(m_field, "setAccessible", "(Z)V", &[Value::Int(1)])
        .expect("Field.setAccessible(true) must not throw");
    let backing_ref = match ctx
        .invoke_virtual(
            m_field,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(map_ref))],
        )
        .expect("Field.get(System.getenv()) must not throw")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Field.get(System.getenv()) returned {other:?}"),
    };

    let backing_class_id = ctx.class_id_of_object(backing_ref);
    let backing_class_name = ctx
        .class_name_of_id(backing_class_id)
        .unwrap_or_else(|| "<unknown>".to_string());
    assert_eq!(
        backing_class_name, "java/util/HashMap",
        "the OpenJDK-compatible `m` field must expose the real HashMap backing"
    );

    let map_get = vm
        .shared
        .natives
        .native_methods
        .find(
            "cratonvm/internal/UnmodifiableMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .expect("UnmodifiableMap.get must be registered");
    let path_key = ctx.create_string(if cfg!(windows) { "Path" } else { "PATH" });
    let _ = match map_get(
        &mut ctx,
        &[Value::Object(Some(map_ref)), Value::Object(Some(path_key))],
    )
    .expect("UnmodifiableMap.get must delegate to the backing map")
    {
        Some(Value::Object(_)) => (),
        other => panic!("UnmodifiableMap.get returned unexpected value {other:?}"),
    };
}
