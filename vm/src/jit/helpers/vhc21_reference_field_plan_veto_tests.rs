/// The veto sits in `vh_const_file_plan` after the kind screen and before
/// the plan word is built (a plan, once filed, is read by both tiers), and
/// asks the same class walk the `Unsafe` cell publishers ask (UPD21-R1).
#[test]
fn vhc21_the_plan_filer_refuses_the_reference_class_own_fields() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let at = src.find("\npub fn vh_const_file_plan(").expect("the filer");
    let rest = &src[at + 1..];
    let body = &rest[..rest.find("\n}\n").expect("end of the filer")];
    let kind = body
        .find("matches!(plan.value_desc, b'I' | b'J' | b'L')")
        .expect("kind screen");
    let veto = body
        .find("crate::vm::vm_exec::class_is_java_lang_ref_reference(")
        .expect("the Reference class walk");
    let slots = body.find("plan.field_index < 4").expect("the slot screen");
    let word = body.find(".to_word()").expect("the plan word");
    assert!(kind < slots && slots < veto && veto < word);
    assert!(
        body[..veto].contains("plan.value_desc == b'L'"),
        "only a reference field can be the referent"
    );
}
