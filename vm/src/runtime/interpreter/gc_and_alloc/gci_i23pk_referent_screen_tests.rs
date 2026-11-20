//! gci i23/pk (i22/dr report section 7, finding F1): the reference
//! processor's referent identity screen builds its relocation-target set
//! only where a survivor can land on a freed base, i.e. never on G1.

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

#[test]
fn gci_i23pk_the_referent_screen_set_is_not_built_on_g1() {
    let src = source();
    let body = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    let body = body.as_str();
    let gate = body
        .find("let screen_can_hit = pause_relocations.is_armed();")
        .expect("the armed gate");
    assert!(
        !body.contains(concat!("let relocation_targets: rustc_hash::FxHashSet<", "usize> =")),
        "process_references_after_gc must not build a private relocation set"
    );
    let claim = body[gate..]
        .find("pause_relocations.claims(referent_old)")
        .map(|i| gate + i)
        .expect("the shared relocation claims check");
    assert!(gate < claim);
}
