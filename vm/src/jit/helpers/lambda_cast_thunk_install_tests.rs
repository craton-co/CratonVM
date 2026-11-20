fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

/// A cast site installs only with every guard proved (asked BEFORE the
/// per-site latch is claimed, so a call whose arguments had not proved a
/// class yet does not spend the claim), with a MIC to bake.
#[test]
fn a_cast_site_is_guarded_before_the_claim() {
    let src =
        include_str!("fast_native/jit_concurrent_hashmap_get_direct.rs").replace("\r\n", "\n");
    let body = body_of(&src, "\npub(crate) unsafe fn install_lambda_inline_cache(");
    let guards = body
        .find("site.cast_thunk_guards()")
        .expect("the guards are asked");
    let claim = body.find("site.claim_adapter_install(").expect("the latch");
    assert!(guards < claim);
    assert!(body.contains("mic_ptr == 0"), "no MIC, no miss edge");
    assert!(body.contains("lambda_cast_adapter_entry("));
}

/// The miss edge names THIS site, so a cast thunk never reaches the
/// shared megamorphic table, where other sites would take it.
#[test]
fn a_cast_thunk_is_never_published_to_the_shared_table() {
    let src =
        include_str!("fast_native/jit_concurrent_hashmap_get_direct.rs").replace("\r\n", "\n");
    let body = body_of(&src, "\npub(crate) unsafe fn install_lambda_inline_cache(");
    let publish = body
        .find("install_mega_dispatch_way(")
        .expect("the shared publish");
    let gate = body[..publish].rfind("cast_guards.is_none()");
    assert!(
        gate.is_some(),
        "the shared publish is gated on a plain thunk"
    );
    assert!(body.contains("helper: jit_invoke_virtual_mic as *const () as usize"));
}

/// nio21 w19 L2: the MIC tail asks for the selected method's body under
/// its DECLARING class before the interpreted template and the by-name
/// tail, only when the receiver-keyed probe found none, and the arm
/// refuses wrapped / indy-trapping bodies, checks native shadows through
/// the template build, and publishes into the per-thread entry cache.
#[test]
fn the_mic_tail_asks_for_the_declaring_class_body_first() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body_inner(");
    let arm = body
        .find("mic_tail_call_selected_declaring_body(")
        .expect("the arm is asked");
    let template = body
        .find("try_jit_virtual_bytecode_callee(")
        .expect("the interpreted template");
    let tail = body.find("\"jit_mic_tail\"").expect("the by-name tail");
    assert!(arm < template && template < tail);
    assert!(body[..arm].contains("!callee_has_compiled_code && !compiled_entry_forbidden"));
    let f = body_of(
        &src,
        "\npub unsafe fn mic_tail_call_selected_declaring_body(",
    );
    for needle in [
        "select_for_jit_receiver(",
        "compiled.requires_wrapped_entry || compiled.has_indy_trap",
        "build_bytecode_callee_cached(",
        "template.declaring_class_id != decl_id",
        "receiver_class_id.as_u32() == 0",
        "call_body_the_mic_probe_just_found(",
        "publish_mic_rust_cached_entry(",
        // nio21 w20 L2: a table-free callee also reaches the machine-code IC.
        "if !has_table && !rerun_pins",
        "mic_tail_publish_declaring_body(",
    ] {
        assert!(f.contains(needle), "{needle}");
    }
    let p = body_of(&src, "\npub unsafe fn mic_tail_publish_declaring_body(");
    for needle in [
        "mic_ptr == 0",
        "mic_publication_skipped(pic_ptr)",
        "publish_mega_dispatch_entry(",
        "withdraw_ic_publication_if_redefined(",
    ] {
        assert!(p.contains(needle), "{needle}");
    }
}

/// JIT r20 w19 yLadder (DISP20-7): the MIC miss arm's probe takes the
/// declaring-class body as its answer before it records a refusal, and the
/// fallback keeps the entryless arm's rules behind its own switch.
#[test]
fn the_mic_miss_probe_falls_back_to_the_declaring_class_body() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body_inner(");
    let miss = &body[body
        .find("// --- Cache miss: full resolution + update cache ---")
        .expect("the miss arm")..];
    let fallback = miss
        .find("probe_selected_declaring_body(vm, info, receiver_class_id)")
        .expect("the miss arm asks the fallback");
    let declined = miss
        .find("if probed.is_none()")
        .expect("the refusal record");
    let published = miss
        .find("mark_site_rerun_pins_before_publish(")
        .expect("publication");
    assert!(fallback < declined && declined < published);
    let f = body_of(&src, "\npub fn probe_selected_declaring_body(");
    for needle in [
        "select_for_jit_receiver(",
        "decl_id == receiver_class_id",
        "method.is_synchronized()",
        "compiled.requires_wrapped_entry || compiled.has_indy_trap",
        "build_bytecode_callee_cached(",
        "template.declaring_class_id != decl_id",
        "receiver_class_id.as_u32() == 0",
    ] {
        assert!(f.contains(needle), "{needle}");
    }
}
