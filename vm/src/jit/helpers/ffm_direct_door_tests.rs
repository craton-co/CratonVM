fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

/// The door is the first thing the virtual-MIC helper asks, before the
/// lean native door and the panic scope.
#[test]
fn the_door_is_asked_first_in_the_virtual_mic_helper() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
    let door = body
        .find("try_ffm_direct_door(")
        .expect("the door is asked");
    let lean = body
        .find("try_lean_native_door(")
        .expect("the lean door is asked");
    assert!(door < lean);
}

/// A hit needs the bind's own identity test (the same handle address in
/// the same collection epoch) and every generation the fill saw, before
/// anything runs; the call itself is the bound row's
/// `ffm_downcall_direct_call`, which re-asks the per-call admission.
#[test]
fn a_hit_is_the_bound_handle_in_the_fill_epoch() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe fn try_ffm_direct_door(");
    let call = body
        .find("ffm_downcall_direct_call_with(")
        .expect("the bound row's call");
    for guard in [
        "args[0] as u64 != site.handle",
        "site.epoch != vm.mem.address_epoch()",
        "site.jit_gen != cratonvm_jit::jit_cache_generation()",
        "site.registry_gen != vm.natives.native_methods.generation()",
        "any_class_redefined()",
        "jvmti_requires_interpreter(vm)",
        "check_native_dispatch_capability_for_slot",
    ] {
        let at = body
            .find(guard)
            .unwrap_or_else(|| panic!("missing guard {guard}"));
        assert!(at < call, "{guard} runs before the call");
    }
    // The arguments are forwarded before the handle is compared and before
    // the call reads any of them (a relocation in progress moves them
    // without moving the epoch).
    let forward = body
        .find("forward_jit_reference_args(vm, info, args)")
        .expect("the arguments are forwarded");
    let compare = body
        .rfind("args[0] as u64 != site.handle")
        .expect("the forwarded compare");
    assert!(forward < compare && compare < call);
    // The fill records the epoch read before the C call.
    let bound = body_of(&src, "\npub unsafe fn try_jit_ffm_downcall_bound(");
    let epoch = bound
        .find("let epoch = vm.mem.address_epoch();")
        .expect("epoch");
    let call = bound.find("ffm_downcall_direct_call(").expect("call");
    let fill = bound.find("ffm_direct_door_fill(").expect("fill");
    assert!(epoch < call && call < fill);
}

#[test]
fn every_slot_index_is_in_range() {
    for addr in [0usize, 8, 0x1234_5670, usize::MAX] {
        assert!(super::ffm_direct_door_slot(addr) < super::FFM_DIRECT_DOOR_SLOTS);
    }
}

/// JIT round 22 (FFM22-1 step (b)): the thin helper widens the staged
/// arguments and reads `rax` by the row's codes exactly as the compiled
/// call's own `MOVSX` / `MOVZX` bytes do.
#[test]
fn the_lazy_helper_widens_by_the_row_codes() {
    use cratonvm_jit::ir_lower as jl;
    assert_eq!(super::ffm_const_widen(0x1FF, jl::FFM_CONST_S8), u64::MAX);
    assert_eq!(super::ffm_const_widen(0x7F, jl::FFM_CONST_S8), 0x7F);
    assert_eq!(
        super::ffm_const_widen(0x1_FFFF, jl::FFM_CONST_S16),
        u64::MAX
    );
    assert_eq!(super::ffm_const_widen(0x1_FFFF, jl::FFM_CONST_U16), 0xFFFF);
    assert_eq!(
        super::ffm_const_widen(0x1_8000_0000, jl::FFM_CONST_S32),
        0xFFFF_FFFF_8000_0000
    );
    assert_eq!(
        super::ffm_const_widen(u64::MAX - 1, jl::FFM_CONST_W64),
        u64::MAX - 1
    );
    assert_eq!(super::ffm_const_widen(42, jl::FFM_CONST_VOID), 0);
}
