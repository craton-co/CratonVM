use super::lambda_static_impl_call_refusal as refusal;

#[test]
fn a_plain_static_site_is_answered() {
    assert_eq!(refusal(false, false, true, false), None);
}

#[test]
fn each_gate_alone_refuses_with_its_own_reason() {
    assert_eq!(refusal(true, false, true, false), Some("a replayed checkcast"));
    assert_eq!(refusal(false, true, true, false), Some("a box / unbox adapter"));
    assert_eq!(refusal(false, false, false, false), Some("an instance-method impl"));
    assert_eq!(refusal(false, false, true, true), Some("the impl owner was redefined"));
}

#[test]
fn the_cast_reason_names_only_a_site_no_other_gate_refuses() {
    // The census counts LAMBDA221-2's remainder: a site that also needs an
    // adapter or an instance receiver would not be answered without the
    // cast either, so it must not be counted as a cast refusal.
    assert_eq!(refusal(true, true, true, false), Some("a box / unbox adapter"));
    assert_eq!(refusal(true, false, false, false), Some("an instance-method impl"));
}

#[test]
fn the_answer_admits_through_the_one_decision() {
    let src = include_str!("../lambda.rs");
    let start = src.find("pub(crate) fn lambda_proxy_static_impl_call(").unwrap_or(0);
    let body = &src[start..];
    let end = body.find("\n}\n").unwrap_or(0);
    assert!(start > 0 && end > 0, "lambda_proxy_static_impl_call moved");
    let body = &body[..end];
    assert!(body.contains("lambda_static_impl_call_refusal("));
    assert!(!body.contains("|| site.rust_only()"), "a second copy of the gate");
}
