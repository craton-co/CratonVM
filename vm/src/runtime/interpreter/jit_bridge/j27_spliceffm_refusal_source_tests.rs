/// j27 wave 4 (lane SPLICEFFM): the `ffm-accessor-keeps-its-own-fast-path`
/// refusal of `resolve_inline_site_from` lifts only through the admission
/// the planner asks (`spliced_ffm_element_admission`, behaviour pinned in
/// `cratonvm_jit::ffm_element_inline` `j27_spliceffm_tests`), never on the
/// switch alone. Source check: the whole file is searched (the resolver
/// lies past the first test module), and this module's own literals
/// cannot match (their quotes and newlines are escaped here).
#[test]
fn j27_spliceffm_the_ffm_refusal_lifts_only_through_the_shared_admission() {
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
    let at = whole
        .find("no_live!(dead, \"ffm-accessor-keeps-its-own-fast-path\")")
        .expect("the refusal");
    let cond = &whole[whole[..at].rfind("if !(ir_mode").expect("its condition")..at];
    assert!(
        cond.contains("spliced_ffm_element_site_admitted("),
        "{cond}"
    );
    assert!(
        !cond.contains("ir_splice_ffm_element_enabled()"),
        "not the switch alone"
    );
    let helper = &whole[whole
        .find("\npub(in crate::runtime::interpreter::jit_bridge) fn spliced_ffm_element_site_admitted(")
        .expect("the helper")..];
    let helper = &helper[..helper.find("\n}\n").expect("fn end")];
    assert!(helper.contains("fei::spliced_ffm_element_admission("));
    assert!(helper.contains("fei::ffm_segment_class_served("));
    assert!(helper.contains(".receivers"), "the callee's own receiver row");
    // j27 SPLICEFFM2: the builder declines the resolver can see are
    // admission inputs -- the callee body (layout operand) and the arena
    // table (native segments).
    assert!(cond.contains("code,"), "the callee body: {cond}");
    assert!(helper.contains("fei::SplicedFfmCall {"));
    assert!(helper.contains("spliced_ffm_arena_table_published()"));
    assert!(helper.contains("fei::ffm_native_segment_class("));
}
