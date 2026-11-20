fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

/// The direct reflective arm is asked behind its switch, after the native
/// site cache (which keeps every site it serves) and before the MIC
/// resolution and the by-name cascade it exists to skip.
#[test]
fn the_direct_reflect_arm_sits_between_the_site_cache_and_the_mic() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let wrapper = body_of(&src, "\npub unsafe fn jit_invoke_virtual_mic_body(");
    assert!(wrapper.contains("jit_invoke_virtual_mic_body_inner("));
    let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body_inner(");
    let site = body
        .find("try_jit_site_cached_native_dispatch(")
        .expect("site cache");
    let gate = body.find("jit_reflect_door_direct()").expect("the switch");
    let arm = body
        .find("jit_forced_reflect_door_native(")
        .expect("the arm");
    let mic = body
        .find("let mic = &*(mic_ptr as *const JitMICSlot);")
        .expect("the MIC");
    let tail = body
        .find("invoke_or_native_keeping_args_current(")
        .expect("the cascade");
    assert!(site < gate && gate < arm && arm < mic && mic < tail);
}

/// The census stamps the helper's entry before any door and its return
/// after the panic scope, both behind the census switch.
#[test]
fn the_census_stamps_both_ends_of_the_virtual_mic_helper() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
    let gate = body
        .find(".reflect_door_phase_stats()")
        .expect("the census switch");
    let entry = body
        .find("ReflectDoorGatePoint::HelperEntry")
        .expect("entry stamp");
    let door = body.find("try_ffm_direct_door(").expect("the first door");
    // The panic scope after the first door: nio21's profile-seed note
    // (before the census) opens a `contain(` of its own.
    let scope = body[door..]
        .find("contain(")
        .map(|i| i + door)
        .expect("the panic scope");
    let exit = body
        .rfind("ReflectDoorGatePoint::HelperExit")
        .expect("exit stamp");
    assert!(gate < entry && entry < door && door < scope && scope < exit);
    // Wave 83: the lean door's early return stamps its exit too.
    let lean = body.find("try_lean_native_door(").expect("the lean door");
    let lean_exit = body
        .find("ReflectDoorGatePoint::HelperExit")
        .expect("lean exit stamp");
    assert!(lean < lean_exit && lean_exit < scope);
}

/// Interpreter round i3 wave 83, lane L3: the direct arm files its site in
/// the lean table behind its own switch, before it serves the call, and
/// the record is a plain callback of no capability (the arm's own call has
/// none).
#[test]
fn the_direct_arm_files_a_lean_record_behind_its_switch() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body_inner(");
    let arm = body
        .find("jit_forced_reflect_door_native(")
        .expect("the arm");
    let gate = body.find("jit_reflect_door_lean_site").expect("the switch");
    let fill = body.find("reflect_door_lean_site_fill(").expect("the fill");
    let call = body[arm..]
        .find("safe_native_call(")
        .map(|i| i + arm)
        .expect("the call");
    assert!(arm < gate && gate < fill && fill < call);
    let fill_fn = body_of(&src, "\npub fn reflect_door_lean_site_fill(");
    assert!(fill_fn.contains("capability_kind: None"));
    assert!(fill_fn.contains("kind: LeafNativeKind::Callback"));
    assert!(fill_fn.contains("lean_native_site_diagnostics_on()"));
}
