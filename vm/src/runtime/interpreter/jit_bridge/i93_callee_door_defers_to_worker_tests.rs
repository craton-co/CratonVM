use super::callee_door_defers_now;
use crate::vm::SharedVm;

/// Interpreter round i4 wave 93, lane L7: with the deferral armed on this
/// VM (set directly, so the test pins no default), the callee door defers
/// on a mutator outside any compile exactly when the background compiler
/// is the configured one, and never for an in-place caller (the OSR door).
#[test]
fn i93_the_callee_door_defers_only_for_a_caller_that_may_wait() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let _ = shared
        .jit
        .bridge
        .callee_door_defers_to_worker_switch
        .set(true);
    assert_eq!(cratonvm_jit::jit_active_compile_depth(), 0);
    assert_eq!(
        callee_door_defers_now(&shared, true),
        crate::runtime::env_cache::bg_compile()
    );
    assert!(
        !callee_door_defers_now(&shared, false),
        "the OSR door's binds"
    );
    let off = SharedVm::new(crate::config::VmConfig::default());
    let _ = off
        .jit
        .bridge
        .callee_door_defers_to_worker_switch
        .set(false);
    assert!(!callee_door_defers_now(&off, true), "switch off");
}

/// The C1-first switch demotes only an optimizing deferral.
#[test]
fn the_c1_first_switch_demotes_the_deferred_tier() {
    use super::callee_door_deferred_optimize as f;
    assert!(f(true, false), "off: the door's own tier");
    assert!(!f(true, true), "on: C1");
    assert!(!f(false, true) && !f(false, false), "C1 stays C1");
}

/// W4C: the OSR door's in-place callee compile is single-pass exactly when a
/// worker exists to upgrade it, and both OSR callee binds report that C1
/// body to the tiered manager.
#[test]
fn w4c_osr_callee_compiles_single_pass_only_under_the_background_compiler() {
    use super::osr_callee_optimize_for as f;
    assert!(!f(true, true), "bg + switch on: C1");
    assert!(f(true, false), "switch off: the old optimizing tier");
    assert!(
        f(false, true) && f(false, false),
        "no worker: nothing upgrades a C1 body"
    );
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
    assert_eq!(
        src.matches("note_osr_c1_callee_publish(").count(),
        3,
        "def + 2 binds"
    );
    assert_eq!(
        src.matches("osr_callee_compile_optimize()").count(),
        3,
        "def + 2 binds"
    );
}

/// The OSR door binds its callees in place (its body is built once).
#[test]
fn i93_the_osr_door_binds_its_callees_in_place() {
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
    let start = src.find("fn compile_osr_body(").unwrap_or(0);
    let body = &src[start..];
    let body = &body[..body.find("\n\u{7d}\n").unwrap_or(body.len())];
    assert!(body.contains("try_jit_compile_callee_from_site_in_place("));
    assert!(!body.contains("try_jit_compile_callee_from_site("));
    let start = src.find("fn osr_splice_direct_bind(").unwrap_or(0);
    let bind = &src[start..];
    let bind = &bind[..bind.find("\n\u{7d}\n").unwrap_or(bind.len())];
    assert!(bind.contains("try_jit_compile_callee_from_site_in_place("));
}
