use super::*;

#[test]
fn invoke_exact_and_invoke_name_their_checks() {
    assert_eq!(
        null_invoker_check(METHOD_HANDLE, "invokeExact").map(|c| (c.0, c.2)),
        Some(("checkExactType", "type"))
    );
    assert_eq!(
        null_invoker_check(METHOD_HANDLE, "invoke").map(|c| (c.0, c.2)),
        Some(("checkGenericType", "asType"))
    );
    // Not an invoker: the call-site answer stands.
    assert_eq!(null_invoker_check(METHOD_HANDLE, "invokeWithArguments"), None);
    assert_eq!(null_invoker_check(METHOD_HANDLE, "type"), None);
    assert_eq!(null_invoker_check("java/lang/invoke/VarHandle", "invokeExact"), None);
}

#[test]
fn the_level_label_is_a_compiled_frame_label() {
    let (method, descriptor, _) = null_invoker_check(METHOD_HANDLE, "invokeExact").unwrap();
    assert_eq!(
        &*invokers_label(method, descriptor),
        "java/lang/invoke/Invokers.checkExactType:(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)V"
    );
}
