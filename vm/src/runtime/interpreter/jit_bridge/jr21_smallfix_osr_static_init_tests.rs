use super::static_init_pending_any_initialized;
use crate::vm::SharedVm;

/// JIT round 21, lane smallfix: nothing pending (the switch off) and ids
/// the class manager does not know are never "initialized".
#[test]
fn jr21_an_empty_or_unknown_pending_list_never_fires() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    assert!(!static_init_pending_any_initialized(&shared, &[]));
    assert!(!static_init_pending_any_initialized(
        &shared,
        &[0, 0x00ff_fff0]
    ));
}

/// The trigger asks `init_state` (as the recorder does), not only the
/// lazily filled `class_init_bits` memo.
#[test]
fn jr21_the_trigger_and_the_recorder_ask_init_state() {
    let whole = concat!(
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
    let src = &whole;
    let start = src
        .find("fn static_init_pending_any_initialized(")
        .expect("the trigger helper");
    let helper = &src[start..];
    let helper = &helper[..helper.find("\n\u{7d}\n").unwrap_or(helper.len())];
    assert!(helper.contains("is_class_initialized_fast"));
    assert!(helper.contains("try_read_untracked()"));
    let start = src.find("fn osr_optimizing_cached(").expect("the lookup");
    let lookup = &src[start..];
    let lookup = &lookup[..lookup.find("\n\u{7d}\n").unwrap_or(lookup.len())];
    assert!(lookup.contains(
        "static_init_pending_any_initialized(shared, &entry.static_init_pending_at_build)"
    ));
    assert!(!lookup.contains("class_init_bits"));
}
