//! JIT round 21 wave 8, lane reflect2 (`CRATONVM_REFLECT_NATIVE_ACCESSOR_ARG_TEXTS`,
//! `CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_ONLY`; page
//! `docs/feature-designs/native-dispatch-architecture.md`):
//! HotSpot 25's `Reflection::invoke` argument refusals and the
//! `-Djdk.reflect.useNativeAccessorOnly` test, as `R21Fz2ReflNative1` measured them.
use super::{
    illegal_arg_exc, native_accessor_argument_refusal, native_accessor_null_to_primitive,
    jdk_config_point_reached, native_accessor_only_cached, native_accessor_only_property,
    INVOKE0_ARGUMENT_TEXT,
};
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{NativeClassAccess, NativeContext, NativeHeapAccess};
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::Value;

fn iae_message(e: &MethodCallFailed) -> Option<&str> {
    match e {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        )) => Some(message.as_str()),
        _ => None,
    }
}

#[test]
fn the_property_is_exactly_true() {
    // `ReflectionFactory.loadConfig`: `"true".equals(...)`.
    assert!(native_accessor_only_property(Some("true")));
    for other in [None, Some(""), Some("TRUE"), Some("True"), Some(" true"), Some("false"), Some("1")] {
        assert!(!native_accessor_only_property(other), "{other:?}");
    }
}

/// r21 review rev-w68-native item 3 (JIT round 21 wave 10, lane walk):
/// the answer is fixed once per VM, as `ReflectionFactory.config()`
/// caches it; a later `System.setProperty` changes nothing, either way.
/// Wave 13 (review rev-native item 4): it is fixed only once the JDK would
/// have fixed it; before that point a refusal answers `false` (the
/// uncached `DEFAULT_CONFIG`) and leaves the cell empty.
#[test]
fn the_answer_is_fixed_once_per_vm_at_the_jdk_point() {
    let set = std::sync::OnceLock::new();
    assert!(native_accessor_only_cached(&set, || Some(true)));
    assert!(native_accessor_only_cached(&set, || panic!("read twice")));
    let unset = std::sync::OnceLock::new();
    assert!(!native_accessor_only_cached(&unset, || Some(false)));
    assert!(!native_accessor_only_cached(&unset, || Some(true)));
    // Before the point: false, nothing kept; the first answer at the
    // point is the one kept.
    let early = std::sync::OnceLock::new();
    assert!(!native_accessor_only_cached(&early, || None));
    assert!(early.get().is_none());
    assert!(native_accessor_only_cached(&early, || Some(true)));
    assert!(native_accessor_only_cached(&early, || Some(false)));
    // `isModuleSystemInited() && isJavaLangInvokeInited()`.
    assert!(!jdk_config_point_reached(1, true));
    assert!(!jdk_config_point_reached(2, false));
    assert!(jdk_config_point_reached(2, true));
    assert!(jdk_config_point_reached(4, true));
    // The door keeps the answer in the VM's door caches, not a static,
    // and both bodies offer the fill before any refusal can ask.
    let src = concat!(include_str!("../lang_class/mod.rs"), "\n", include_str!("../lang_class/dbg_bb_enabled.rs"), "\n", include_str!("../lang_class/wf7_synthesise_entry_class_if.rs"), "\n", include_str!("../lang_class/descriptor_to_class_mirror_via.rs"), "\n", include_str!("../lang_class/native_field_set.rs"), "\n", include_str!("../lang_class/executable_access_refusal_chain.rs"), "\n", include_str!("../lang_class/forget_lang_class_keys.rs"), "\n", include_str!("../lang_class/gc_update_annotation_proxy_refs.rs"), "\n", include_str!("../lang_class/native_class_is_annotation_present.rs"), "\n", include_str!("../lang_class/classloader_packages_synced.rs"), "\n", include_str!("../lang_class/declaring_class_loader_aware.rs")).replace("\r\n", "\n");
    let door = &src[src
        .find("\npub fn door_honours_native_accessor_only(")
        .expect("the door's test")..];
    let door = &door[..door.find("\n}\n").expect("its end")];
    assert!(door.contains("native_accessor_only_cached(&caches.native_accessor_only, || {"));
    assert!(door.contains("jdk_reflection_config_native_accessor_only(ctx)"));
    for body in ["\npub fn native_method_invoke_body(", "\npub fn native_constructor_new_instance_body("] {
        let b = &src[src.find(body).expect("the body")..];
        let b = &b[..b.find("\n}\n").expect("its end")];
        let offer = b
            .find("door_notes_reflection_config_point(&*ctx, gate_access);")
            .expect("the offer");
        let ask = b.find("door_honours_native_accessor_only(").expect("the refusal's ask");
        assert!(offer < ask, "{body}");
    }
}

#[test]
fn only_a_null_for_a_primitive_is_the_message_less_arm() {
    for p in ["I", "J", "F", "D", "Z", "B", "S", "C"] {
        assert!(native_accessor_null_to_primitive(Value::Object(None), p), "{p}");
        assert!(!native_accessor_null_to_primitive(Value::Int(1), p), "{p}");
    }
    assert!(!native_accessor_null_to_primitive(Value::Object(None), "Ljava/lang/String;"));
    assert!(!native_accessor_null_to_primitive(Value::Object(None), "[I"));
}

#[test]
fn a_mismatch_is_the_cause_less_argument_type_mismatch() {
    let mut ctx = mock_ctx();
    let out = native_accessor_argument_refusal(
        &mut ctx,
        Value::Double(1.5),
        "I",
        illegal_arg_exc("argument type mismatch".to_string()),
    );
    assert_eq!(iae_message(&out), Some(INVOKE0_ARGUMENT_TEXT));
    assert_eq!(INVOKE0_ARGUMENT_TEXT, "argument type mismatch");
}

#[test]
fn a_null_for_a_primitive_is_an_iae_without_a_message() {
    let mut ctx = mock_ctx();
    let out = native_accessor_argument_refusal(
        &mut ctx,
        Value::Object(None),
        "I",
        illegal_arg_exc("argument type mismatch".to_string()),
    );
    match out {
        // Built by its no-arg constructor: `getMessage()` is null.
        MethodCallFailed::ExceptionThrown(obj) => {
            let cid = ctx.class_id_of_object(obj);
            assert_eq!(
                ctx.class_name_of_id(cid).as_deref(),
                Some("java/lang/IllegalArgumentException")
            );
        }
        // A throwable that cannot be built keeps the coercion's refusal.
        other => assert_eq!(iae_message(&other), Some("argument type mismatch")),
    }
}

#[test]
fn a_failure_that_is_not_a_plain_iae_passes_through() {
    let mut ctx = mock_ctx();
    let e: MethodCallFailed = RuntimeError::NullPointerException { message: None }.into();
    let out = native_accessor_argument_refusal(&mut ctx, Value::Object(None), "I", e);
    assert!(matches!(
        out,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message: None }
        ))
    ));
}

#[test]
fn both_bodies_take_the_native_accessor_texts() {
    let src = concat!(include_str!("../lang_class/mod.rs"), "\n", include_str!("../lang_class/dbg_bb_enabled.rs"), "\n", include_str!("../lang_class/wf7_synthesise_entry_class_if.rs"), "\n", include_str!("../lang_class/descriptor_to_class_mirror_via.rs"), "\n", include_str!("../lang_class/native_field_set.rs"), "\n", include_str!("../lang_class/executable_access_refusal_chain.rs"), "\n", include_str!("../lang_class/forget_lang_class_keys.rs"), "\n", include_str!("../lang_class/gc_update_annotation_proxy_refs.rs"), "\n", include_str!("../lang_class/native_class_is_annotation_present.rs"), "\n", include_str!("../lang_class/classloader_packages_synced.rs"), "\n", include_str!("../lang_class/declaring_class_loader_aware.rs"));
    let method = &src[src.find("\npub fn native_method_invoke_body(").expect("the method body")..];
    let method = &method[..method.find("\n}\n").expect("its end")];
    // Both argument loops, and the property on a refusal only.
    assert_eq!(method.matches("native_accessor_argument_refusal(").count(), 2);
    assert!(method.contains("door_honours_native_accessor_only(ctx, gate_access)"));
    let ctor =
        &src[src.find("\npub fn native_constructor_new_instance_body(").expect("the ctor body")..];
    let ctor = &ctor[..ctor.find("\n}\n").expect("its end")];
    assert!(ctor.contains("return Err(illegal_arg_exc(INVOKE0_ARITY_TEXT.to_string()));"));
    assert!(ctor.contains("native_accessor_argument_refusal(ctx, arg_val, pdesc, e)"));
    assert!(ctor.contains("door_honours_native_accessor_only(ctx, gate_access)"));
}
