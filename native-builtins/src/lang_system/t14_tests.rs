use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// -----------------------------------------------------------------------
// T14.1 вЂ” initPhase1
// -----------------------------------------------------------------------

#[test]
fn init_phase1_succeeds() {
    let mut ctx = mock_ctx();
    // Ensure System class exists so ensure_class_initialized works
    let _ = ctx.ensure_class_initialized("java/lang/System").unwrap();
    let r = native_system_init_phase1(&mut ctx, &[]);
    assert!(r.is_ok());
    assert_eq!(r.unwrap(), None); // void return
}

// -----------------------------------------------------------------------
// T14.2 вЂ” initPhase2
// -----------------------------------------------------------------------

#[test]
fn init_phase2_returns_zero() {
    let mut ctx = mock_ctx();
    let r = native_system_init_phase2(&mut ctx, &[Value::Int(0), Value::Int(0)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

// -----------------------------------------------------------------------
// T14.3 вЂ” initPhase3
// -----------------------------------------------------------------------

#[test]
fn init_phase3_succeeds() {
    let mut ctx = mock_ctx();
    let r = native_system_init_phase3(&mut ctx, &[]);
    assert!(r.is_ok());
    assert_eq!(r.unwrap(), None); // void return
}

// -----------------------------------------------------------------------
// T14.4 вЂ” VM.getSavedProperty
// -----------------------------------------------------------------------

#[test]
fn vm_get_saved_property_returns_null_for_missing() {
    let mut ctx = mock_ctx();
    let key = ctx.create_string("nonexistent.property");
    let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(Some(key))]);
    assert_eq!(r.unwrap(), Some(Value::Object(None)));
}

#[test]
fn vm_get_saved_property_returns_value() {
    let mut ctx = mock_ctx();
    ctx.set_system_property("test.key", "test.value");
    let key = ctx.create_string("test.key");
    let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(Some(key))]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected string Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "test.value");
}

#[test]
fn vm_get_saved_property_null_key() {
    let mut ctx = mock_ctx();
    let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(None)]);
    assert_eq!(r.unwrap(), Some(Value::Object(None)));
}

// `saveProperties` stores into `VM`'s static block, and a native does not get
// the JVMS 5.5 class-initialisation an interpreted `invokestatic` does: on
// `--real-jdk` the store landed in the pre-`<clinit>` block and `VM.<clinit>`
// then reset it, so `ClassLoaders.<clinit>` read `savedProps == null` and
// every `ServiceLoader`/`java.util.logging` user died with
// `NoClassDefFoundError: ClassLoaders`. The native must initialise `VM` first.
#[test]
fn vm_save_properties_initialises_vm_before_storing() {
    let mut ctx = mock_ctx();
    assert!(
        ctx.class_id_by_name("jdk/internal/misc/VM").is_none(),
        "fixture: VM must start uninitialised"
    );
    let props = ctx.create_string("stand-in for the Map");
    let r = native_vm_save_properties(&mut ctx, &[Value::Object(Some(props))]);
    assert_eq!(r.unwrap(), None);
    assert!(
        ctx.class_id_by_name("jdk/internal/misc/VM").is_some(),
        "saveProperties stored into an uninitialised VM"
    );
}

#[test]
fn vm_save_properties_null_argument_is_a_noop() {
    let mut ctx = mock_ctx();
    let r = native_vm_save_properties(&mut ctx, &[Value::Object(None)]);
    assert_eq!(r.unwrap(), None);
}

// -----------------------------------------------------------------------
// T14.5 вЂ” VM.getRuntimeArguments
// -----------------------------------------------------------------------

#[test]
fn vm_get_runtime_arguments_returns_empty_array() {
    let mut ctx = mock_ctx();
    let r = native_vm_get_runtime_arguments(&mut ctx, &[]);
    let arr = match r.unwrap() {
        Some(Value::Object(Some(a))) => a,
        other => panic!("expected array, got {other:?}"),
    };
    assert_eq!(ctx.array_length(arr), 0);
}
