fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

/// Both routes the redirect serves (a `bindTo` handle and the plain one)
/// ask the count, for a real handle only, right before they return.
#[test]
fn both_served_routes_count_a_real_handle_first() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe fn mh_constant_bind_redirect(");
    let ask = "&& !mh_bind_customization_serves(vm, info_ptr as usize, args[0])";
    assert_eq!(body.matches(ask).count(), 2, "both routes count");
    let bound_ask = body.find(ask).expect("the bound route's count");
    let bound_ret = body.find("bound: Some(receiver as i64),").expect("the bound route");
    assert!(bound_ask < bound_ret);
    let plain_ask = body.rfind(ask).expect("the plain route's count");
    let plain_ret = body.rfind("bound: None,").expect("the plain route");
    assert!(plain_ask < plain_ret);
    let helper = body_of(&src, "\npub unsafe fn mh_bind_customization_serves(");
    assert!(helper.contains("lang_invoke::lane_check_customized("));
    assert!(helper.contains(".is_some()"));
    // JIT r22 retirement: the customized steady state is answered first,
    // from the raw slots, before the context is built.
    let fast = helper.find("mh_bind_handle_is_customized(").expect("the raw steady-state test");
    let ctx = helper.find("NativeContextImpl {").expect("the counting path");
    assert!(fast < ctx);
}

/// Item 4 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`: the bound
/// site's publication re-checks the redefinition epoch AFTER it installs.
#[test]
fn the_bound_site_publication_rechecks_the_epoch_after_installing() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe fn publish_bound_site_entry(");
    let update = body.find("mic.update(").expect("the MIC install");
    let install = body.find("pic.install(").expect("the PIC install");
    let recheck = body
        .find("withdraw_ic_publication_if_redefined(vm, epoch_now, mic, pic)")
        .expect("the re-check");
    assert!(update < recheck && install < recheck);
}
