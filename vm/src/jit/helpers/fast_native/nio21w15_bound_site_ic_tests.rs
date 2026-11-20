fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

/// A kind-1 info is routed before every virtual door of the MIC helper:
/// none of them may select an override for a statically bound site.
#[test]
fn a_bound_site_leaves_the_virtual_mic_helper_first() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
    let route = body
        .find(
            "jit_invoke_bound_site_mic(vm_ptr, info_ptr, args_ptr, num_args, mic_ptr, pic_ptr)",
        )
        .expect("the bound-site route");
    for door in [
        "try_ffm_direct_door(",
        "try_lean_native_door(",
        "jit_invoke_virtual_mic_body(",
    ] {
        let at = body.find(door).unwrap_or_else(|| panic!("{door} is asked"));
        assert!(route < at, "the bound-site route precedes {door}");
    }
}

/// The call itself is the blind dispatch's, and the publication never
/// reaches the shared megamorphic table (keyed by virtual selection).
#[test]
fn the_bound_site_arm_calls_the_dispatch_door_and_publishes_locally() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let arm = body_of(&src, "\npub unsafe fn jit_invoke_bound_site_mic(");
    assert!(arm.contains("jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args)"));
    let publish = body_of(&src, "\npub unsafe fn publish_bound_site_entry(");
    for banned in [
        "publish_mega_dispatch_entry(",
        "bind_mega_selector_at_resolution(",
    ] {
        assert!(
            !publish.contains(banned),
            "the bound-site publication calls {banned}"
        );
    }
    // Every gate runs before the first publication.
    let update = publish.find("mic.update(").expect("the MIC publication");
    for gate in [
        "jvmti_requires_interpreter(vm)",
        "site_quiesced_for_redefinition(vm, info, info_key)",
        "names_a_stale_body()",
        "!body.has_indy_trap",
        "!body.requires_wrapped_entry",
        "mic_publish_exception_table_callees()",
        "mic.mark_callee_rerun_pins()",
        // Interpreter round i7 wave 137, lane L6: a receiver class the
        // interface-sender receiver check refuses is never published.
        "iface_special_receiver_refusal_message(",
    ] {
        let at = publish
            .find(gate)
            .unwrap_or_else(|| panic!("missing gate {gate}"));
        assert!(at < update, "{gate} runs before the publication");
    }
}

/// Interpreter round i7 wave 138, lane L2: the lean native door answers
/// ahead of the interface-sender receiver guard in `jit_invoke_dispatch`
/// and its kind-1 records carry no receiver guard, so a guarded site is
/// never filed in its table.
#[test]
fn the_lean_native_door_never_files_a_guarded_interface_special_site() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let door = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_dispatch(");
    assert!(door.contains("try_lean_native_door("));
    let fill = body_of(&src, "\npub fn lean_native_site_fill(");
    let gate = fill
        .find("iface_special_site_is_guarded(vm, info)")
        .expect("the guarded-site gate");
    let file = fill.find("lean_native_site_file(").expect("the filing");
    assert!(gate < file, "the gate runs before the filing");
}
