/// W3A: a capturing lambda's private `lambda$..` body (the virtual arm's
/// exact owner) and an `invokespecial` handle are entered through the cached
/// template, keyed by the owner, before the by-name door.
#[test]
fn w3a_exact_owner_and_special_arms_try_the_cached_template_first() {
    let src = include_str!("../lambda.rs");
    let key = ["exact_impl_owner\n", "                    .or_else(|| recv_class_id_opt"].concat();
    assert!(src.contains(&key), "the cached probe is keyed by the exact owner");
    let at_cached = src
        .find(&["} else if let Some(result) = ", "cached_result {"].concat())
        .expect("the cached arm");
    let at_owner = src[at_cached..]
        .find(&["} else if let Some(owner_id) = ", "exact_impl_owner {"].concat())
        .expect("the by-name owner arm follows the cached one");
    assert!(at_owner > 0);
    let special = src
        .split("MethodHandleKind::InvokeSpecial => {")
        .nth(1)
        .and_then(|rest| rest.split("MethodHandleKind::NewInvokeSpecial").next())
        .expect("the special arm");
    let at_try = special.find("try_invoke_cached_lambda_impl(").expect("cached first");
    let at_door = special
        .find("invoke_on_class_shared_no_retarget(")
        .expect("by-name fallback");
    assert!(at_try < at_door);
}
