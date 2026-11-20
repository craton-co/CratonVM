use super::*;

fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\u{7d}\n").map_or(rest.len(), |e| e + 3)].to_string()
}

#[test]
fn jr21_tl2_the_screens_ask_the_retired_row_mask() {
    let src = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let resolver = body_of(
        &src,
        "\npub(in crate::runtime::interpreter::jit_bridge) fn resolve_inline_site_from_body(",
    );
    assert_eq!(
        resolver
            .matches("selected_bridge_masked_by_retired_row(")
            .count(),
        2,
        "the selected-method screen and the receiver chain's declaring step"
    );
    assert!(resolver.contains("(cp_class_id, receiver_class_id),"));
    assert!(resolver.contains("(receiver_id, Some(receiver_id)),"));
    let helper = body_of(&src, "\npub(in crate::runtime::interpreter::jit_bridge) fn selected_bridge_masked_by_retired_row(");
    for needle in [
        "find_with_kind(declaring_class_name, method_name, descriptor)",
        "super::native_override::retired_row_masks_ancestor_bridge(",
    ] {
        assert!(helper.contains(needle), "{needle}");
    }
}

/// The mask itself is `--real-jdk` and `Bridge`-only: a `Compatible` VM
/// answers `false` for any walk.
#[test]
fn jr21_tl2_a_compatible_vm_masks_nothing() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cm = shared.classes.class_manager.read();
    let id = crate::classloading::ClassId::new;
    assert!(!selected_bridge_masked_by_retired_row(
        &shared,
        &cm,
        (id(7), None),
        id(5),
        "java/lang/ref/Reference",
        "refersTo",
        "(Ljava/lang/Object;)Z",
    ));
}
