// `register_phase53_security` is called by path, not imported: the
// registrar drift gate reads a module-level `use` as a shipping root.
use super::{synthetic_enum_values, try_alloc_concurrent_synthetic};
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeMethodRegistry};
use cratonvm_types::Value;

/// A synthetic enum's `values()` is the declared `E[]`, one constant per
/// ordinal, in order (i8-L2).
#[test]
fn i8_l2_synthetic_enum_values_are_enum_typed() {
    let mut ctx = mock_ctx();
    let dow = ctx
        .ensure_class_initialized("java/time/DayOfWeek")
        .expect("mock registers the class");
    let arr = match synthetic_enum_values(&mut ctx, "java/time/DayOfWeek", 7, |ctx, i| {
        let d = try_alloc_concurrent_synthetic(ctx, "java/time/DayOfWeek", 1)?;
        ctx.set_field(d, 0, Value::Int(i as i32 + 1));
        Ok(d)
    }) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("expected an array, got {other:?}"),
    };
    assert_eq!(ctx.ref_array_component(arr), Some(dow));
    assert_eq!(ctx.array_length(arr), 7);
    for i in 0..7 {
        let d = match ctx.get_array_element(arr, i) {
            Value::Object(Some(d)) => d,
            other => panic!("expected a constant at {i}, got {other:?}"),
        };
        assert_eq!(ctx.get_field(d, 0), Value::Int(i as i32 + 1));
    }
}

/// The synthetic `Security.getProviders()` answers `Provider[]` (i8-L2).
#[test]
fn i8_l2_synthetic_get_providers_is_provider_typed() {
    let mut r = NativeMethodRegistry::new();
    super::register_phase53_security(&mut r);
    let get_providers = r
        .find(
            "java/security/Security",
            "getProviders",
            "()[Ljava/security/Provider;",
        )
        .expect("registered");
    let mut ctx = mock_ctx();
    let provider = ctx
        .ensure_class_initialized("java/security/Provider")
        .expect("mock registers the class");
    let arr = match get_providers(&mut ctx, &[]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("expected an array, got {other:?}"),
    };
    assert_eq!(ctx.ref_array_component(arr), Some(provider));
    for i in 0..ctx.array_length(arr) {
        assert!(
            matches!(ctx.get_array_element(arr, i), Value::Object(Some(_))),
            "slot {i} holds a provider"
        );
    }
}

/// The synthetic `Security` statics read their parameters from `args[0]`
/// on (a static native has no receiver): `setProperty(k, v)` then
/// `getProperty(k)` round-trips, and `getProperty` of a built-in key
/// answers it (i8-L2; both used to read `args[1]` and answer null).
#[test]
fn i8_l2_synthetic_security_statics_read_args_from_zero() {
    let mut r = NativeMethodRegistry::new();
    super::register_phase53_security(&mut r);
    let set = r
        .find(
            "java/security/Security",
            "setProperty",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        )
        .expect("registered");
    let get = r
        .find(
            "java/security/Security",
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
        )
        .expect("registered");
    let mut ctx = mock_ctx();
    let key = ctx.create_string("i8.l2.security.args.probe");
    let val = ctx.create_string("seen");
    set(
        &mut ctx,
        &[Value::Object(Some(key)), Value::Object(Some(val))],
    )
    .expect("stores");
    let read = match get(&mut ctx, &[Value::Object(Some(key))]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        other => panic!("expected a string, got {other:?}"),
    };
    assert_eq!(read.as_deref(), Some("seen"));
    let builtin = ctx.create_string("keystore.type");
    let read = match get(&mut ctx, &[Value::Object(Some(builtin))]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        other => panic!("expected a string, got {other:?}"),
    };
    assert_eq!(read.as_deref(), Some("pkcs12"));    }