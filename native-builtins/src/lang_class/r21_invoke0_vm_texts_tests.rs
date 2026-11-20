//! JIT round 21 wave 1, lane reflect (`CRATONVM_REFLECT_INVOKE0_VM_TEXTS`):
//! the texts HotSpot 25's `JVM_InvokeMethod` raised in `R21ReflectInvoke`.
use super::{illegal_arg_exc, INVOKE0_ARITY_TEXT, INVOKE0_RECEIVER_TEXT};
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};

fn message(e: MethodCallFailed) -> String {
    match e {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        )) => message,
        _ => panic!("not an IllegalArgumentException"),
    }
}

#[test]
fn invoke0_raises_the_vm_texts() {
    assert_eq!(
        message(illegal_arg_exc(INVOKE0_RECEIVER_TEXT.to_string())),
        "object is not an instance of declaring class"
    );
    assert_eq!(
        message(illegal_arg_exc(INVOKE0_ARITY_TEXT.to_string())),
        "wrong number of arguments"
    );
}

#[test]
fn the_door_takes_them_only_for_native_accessor_methods() {
    // `invoke0` always; the `Method.invoke` door (`gate_access == true`)
    // only for a method JDK 25 sends to `NativeAccessor`, and the null
    // receiver only past the access check (`accessible`).
    let src = concat!(include_str!("../lang_class/mod.rs"), "\n", include_str!("../lang_class/dbg_bb_enabled.rs"), "\n", include_str!("../lang_class/wf7_synthesise_entry_class_if.rs"), "\n", include_str!("../lang_class/descriptor_to_class_mirror_via.rs"), "\n", include_str!("../lang_class/native_field_set.rs"), "\n", include_str!("../lang_class/executable_access_refusal_chain.rs"), "\n", include_str!("../lang_class/forget_lang_class_keys.rs"), "\n", include_str!("../lang_class/gc_update_annotation_proxy_refs.rs"), "\n", include_str!("../lang_class/native_class_is_annotation_present.rs"), "\n", include_str!("../lang_class/classloader_packages_synced.rs"), "\n", include_str!("../lang_class/declaring_class_loader_aware.rs"));
    let body = &src[src.find("\npub fn native_method_invoke_body(").expect("the body")..];
    assert!(body.contains("(!gate_access\n            || (door_native_accessor_texts_enabled()"));
    assert!(body.contains("if accessible && vm_texts_now(&*ctx) {"));
    assert!(body.contains("RuntimeError::NullPointerException { message: None }"));
}

fn descs(ps: &[&str]) -> Vec<String> {
    ps.iter().map(|p| p.to_string()).collect()
}

#[test]
fn native_accessor_choice_follows_use_native_accessor() {
    use super::method_takes_native_accessor as native;
    const PUBLIC: i32 = 0x0001;
    const VARARGS: i32 = 0x0080;
    const NATIVE: i32 = 0x0100;
    let mh = "java/lang/invoke/MethodHandle";
    let objs = descs(&["[Ljava/lang/Object;"]);
    // Signature-polymorphic: `MethodHandle.invokeExact`, `VarHandle.get`.
    assert!(native(mh, PUBLIC | VARARGS | NATIVE, false, &objs));
    assert!(native("java/lang/invoke/VarHandle", PUBLIC | VARARGS | NATIVE, false, &objs));
    // `invokeWithArguments(Object...)`: a plain varargs method, MH accessor.
    assert!(!native(mh, PUBLIC | VARARGS, false, &objs));
    // A varargs method whose last parameter is not an array (class files
    // only; javac never writes one), or with no parameter at all.
    assert!(native("p/C", PUBLIC | VARARGS, true, &descs(&["I"])));
    assert!(native("p/C", PUBLIC | VARARGS, true, &[]));
    // Ordinary methods.
    assert!(!native("p/C", PUBLIC, true, &descs(&["I", "I"])));
    assert!(!native("p/C", PUBLIC | NATIVE, false, &descs(&[])));
    // 255 slots: longs and doubles count twice, the receiver once.
    assert!(native("p/C", PUBLIC, true, &descs(&["J"; 128])));
    assert!(!native("p/C", PUBLIC, true, &descs(&["J"; 127])));
    assert!(native("p/C", PUBLIC, false, &descs(&["I"; 254])));
    assert!(!native("p/C", PUBLIC, true, &descs(&["I"; 254])));
}
