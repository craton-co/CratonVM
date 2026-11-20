use super::indy_target_class_admission as admit;

fn production() -> String {
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
    whole.to_string()
}

fn flat(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

#[test]
fn a_name_that_answers_the_target_is_admitted_by_name_without_the_id_terms() {
    let mut asked = false;
    assert_eq!(
        admit(Some(42), 42, true, || {
            asked = true;
            false
        }),
        Ok("by-name")
    );
    assert!(!asked, "the by-id terms are asked only for an unknown name");
    assert_eq!(admit(Some(42), 42, false, || false), Ok("by-name"));
}

#[test]
fn a_name_that_answers_another_class_is_refused_in_both_arms() {
    for by_id_on in [true, false] {
        let mut asked = false;
        assert!(admit(Some(7), 42, by_id_on, || {
            asked = true;
            true
        })
        .is_err());
        assert!(!asked);
    }
}

#[test]
fn an_unknown_name_is_admitted_by_id_only_with_the_switch_and_the_terms() {
    assert_eq!(admit(None, 42, true, || true), Ok("by-id"));
    assert!(admit(None, 42, true, || false).is_err());
    let mut asked = false;
    assert!(admit(None, 42, false, || {
        asked = true;
        true
    })
    .is_err());
    assert!(!asked, "with the switch off the terms are not asked");
}

/// Both indy plans ask the one admission, and every optimizing door hands
/// the jit crate the class-context resolvers under the switch.
#[test]
fn both_plans_share_the_admission_and_the_doors_pass_the_context_resolvers() {
    let src = flat(&production());
    let admissions = src.matches("letadmission=self.indy_target_admission(");
    assert_eq!(admissions.count(), 2);
    assert!(!src.contains("fnindy_direct_target_callable("));
    assert_eq!(src.matches("indy_context_callee_compiler:if").count(), 3);
    let inline_rows = "indy_context_ir_inline_resolver:ifcratonvm_jit::ir_inline_enabled()";
    assert_eq!(src.matches(inline_rows).count(), 3);
    assert!(
        src.contains("c_direct_lookup_in(ClassId::new(context),site_class,site_method,site_desc)")
    );
    assert!(src.contains(
        "callee_compiler_in(ClassId::new(context),callee_class,callee_method,callee_desc,)"
    ));
    assert!(src.contains(
        "direct_callee_lookup_in(ClassId::new(context),callee_class,callee_method,callee_desc,)"
    ));
    assert!(src.contains("fnjit_indy_admit_by_id_enabled()->bool{true}"));
    assert!(src.matches("jit_indy_admit_by_id_enabled()").count() >= 4);
}
