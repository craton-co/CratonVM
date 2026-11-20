#[test]
fn r22_memory_a_non_object_value_of_answer_is_internal_not_an_oome() {
    let src = include_str!("../deopt_materialize.rs").replace("\r\n", "\n");
    let at = src
        .find("let Some(Value::Object(Some(obj))) = boxed else {")
        .unwrap_or_else(|| panic!("the non-object arm"));
    let arm = &src[at..(at + 700).min(src.len())];
    assert!(arm.contains("VmError::Internal {"), "{arm}");
    assert!(!arm.contains("OutOfMemoryError"), "{arm}");
    let at = src
        .find("Err(MethodCallFailed::InternalError(other)) => {")
        .unwrap_or_else(|| panic!("the internal arm"));
    let arm = &src[at..(at + 800).min(src.len())];
    assert!(arm.contains("return Err(MethodCallFailed::InternalError(other));"), "{arm}");
}
