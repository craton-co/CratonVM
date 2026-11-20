use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = mock_ctx();
    ctx.set_vm_identity(vm);
    ctx
}

fn object(v: MethodCallResult, what: &str) -> ObjectRef {
    match v {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("{what} answered {other:?}"),
    }
}

fn native(
    registry: &NativeMethodRegistry,
    class: &str,
    name: &str,
    desc: &str,
) -> cratonvm_native_api::NativeCallback {
    registry
        .find(class, name, desc)
        .unwrap_or_else(|| panic!("{class}.{name}{desc} should be registered"))
}

fn assert_store(ctx: &MockNativeContext, store: ObjectRef, cap: usize) {
    for slot in 0..2 {
        let Value::Object(Some(arr)) = ctx.get_field(store, slot) else {
            panic!("store slot {slot} must hold an array");
        };
        assert_eq!(ctx.array_length(arr), cap);
    }
    assert_eq!(ctx.get_field(store, 2), Value::Int(0));
}

#[test]
fn a_binding_store_is_complete_and_releases_its_root() {
    let mut ctx = vm_ctx(0xC19_0101);
    let store = t3_new_binding_store(&mut ctx, "java/util/HashMap", 16).unwrap();
    assert_store(&ctx, store, 16);
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// `new InitialContext(env)`: the store and the environment land in the
/// receiver, and a bind / lookup round trip works through them.
#[test]
fn an_initial_context_keeps_its_store_and_environment() {
    let mut registry = NativeMethodRegistry::new();
    register_t38_jndi(&mut registry);
    let init = native(
        &registry,
        "javax/naming/InitialContext",
        "<init>",
        "(Ljava/util/Hashtable;)V",
    );
    let bind = native(
        &registry,
        "javax/naming/InitialContext",
        "bind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
    );
    let lookup = native(
        &registry,
        "javax/naming/InitialContext",
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
    );
    let mut ctx = vm_ctx(0xC19_0102);
    let this = ctx.fresh_object_ref();
    let env = ctx.fresh_object_ref();
    init(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(env))],
    )
    .unwrap();
    assert_eq!(ctx.native_pin_count_for_test(), 0);
    let Value::Object(Some(store)) = ctx.get_field(this, 0) else {
        panic!("the context must hold its binding store");
    };
    assert_store(&ctx, store, 16);
    assert_eq!(ctx.get_field(this, 1), Value::Object(Some(env)));

    let name = ctx.create_string("jdbc/ds");
    let value = ctx.fresh_object_ref();
    bind(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(name)),
            Value::Object(Some(value)),
        ],
    )
    .unwrap();
    let again = ctx.create_string("jdbc/ds");
    let args = [Value::Object(Some(this)), Value::Object(Some(again))];
    let found = object(lookup(&mut ctx, &args), "lookup");
    assert_eq!(found, value);
}

/// `LocateRegistry.createRegistry`, `ScriptEngineManager.getEngineByName`,
/// `ScriptEngineManager.<init>` and `JShell.create`: each builder's shape.
#[test]
fn the_registry_engine_manager_and_shell_builders_are_complete() {
    let mut registry = NativeMethodRegistry::new();
    register_t38_jndi(&mut registry);
    register_t310_scripting(&mut registry);
    register_t312_tooling(&mut registry);
    let mut ctx = vm_ctx(0xC19_0103);

    let create = native(
        &registry,
        "java/rmi/registry/LocateRegistry",
        "createRegistry",
        "(I)Ljava/rmi/registry/Registry;",
    );
    let reg = object(create(&mut ctx, &[Value::Int(1234)]), "createRegistry");
    let Value::Object(Some(store)) = ctx.get_field(reg, 0) else {
        panic!("the registry must hold its binding store");
    };
    assert_store(&ctx, store, 16);
    assert_eq!(ctx.get_field(reg, 1), Value::Int(1234));
    assert_eq!(ctx.native_pin_count_for_test(), 0);

    let by_name = native(
        &registry,
        "javax/script/ScriptEngineManager",
        "getEngineByName",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
    );
    let manager = ctx.fresh_object_ref();
    let js = ctx.create_string("js");
    let args = [Value::Object(Some(manager)), Value::Object(Some(js))];
    let engine = object(by_name(&mut ctx, &args), "getEngineByName");
    let Value::Object(Some(bindings)) = ctx.get_field(engine, 0) else {
        panic!("the engine must hold its bindings");
    };
    assert_store(&ctx, bindings, 16);
    let Value::Object(Some(engine_name)) = ctx.get_field(engine, 1) else {
        panic!("the engine must hold its name");
    };
    assert_eq!(ctx.read_string(engine_name).as_deref(), Some("js"));
    assert_eq!(ctx.native_pin_count_for_test(), 0);

    let manager_init = native(
        &registry,
        "javax/script/ScriptEngineManager",
        "<init>",
        "()V",
    );
    manager_init(&mut ctx, &[Value::Object(Some(manager))]).unwrap();
    let Value::Object(Some(engines)) = ctx.get_field(manager, 0) else {
        panic!("the manager must hold its engine list");
    };
    let Value::Object(Some(arr)) = ctx.get_field(engines, 0) else {
        panic!("the engine list must hold its array");
    };
    assert_eq!(ctx.array_length(arr), 4);
    assert_eq!(ctx.get_field(engines, 1), Value::Int(0));
    assert_eq!(ctx.native_pin_count_for_test(), 0);

    let shell_create = native(
        &registry,
        "jdk/jshell/JShell",
        "create",
        "()Ljdk/jshell/JShell;",
    );
    let shell = object(shell_create(&mut ctx, &[]), "JShell.create");
    let Value::Object(Some(history)) = ctx.get_field(shell, 0) else {
        panic!("the shell must hold its history list");
    };
    let Value::Object(Some(arr)) = ctx.get_field(history, 0) else {
        panic!("the history list must hold its array");
    };
    assert_eq!(ctx.array_length(arr), 32);
    assert_eq!(ctx.get_field(shell, 1), Value::Int(0));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// `SnippetEvent.status()` reads the event's code before it allocates.
#[test]
fn a_snippet_status_carries_the_events_code() {
    let mut registry = NativeMethodRegistry::new();
    register_t312_tooling(&mut registry);
    let status = native(
        &registry,
        "jdk/jshell/SnippetEvent",
        "status",
        "()Ljdk/jshell/Snippet$Status;",
    );
    let mut ctx = vm_ctx(0xC19_0104);
    let event = ctx.fresh_object_ref();
    ctx.set_field(event, 2, Value::Int(7));
    let st = object(status(&mut ctx, &[Value::Object(Some(event))]), "status");
    let Value::Object(Some(name)) = ctx.get_field(st, 0) else {
        panic!("the status must carry its name");
    };
    assert_eq!(ctx.read_string(name).as_deref(), Some("VALID"));
    assert_eq!(ctx.get_field(st, 1), Value::Int(7));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}
