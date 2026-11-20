use super::*;

#[test]
fn gci_i30_rp_only_finalizer_only_referents_that_are_not_references_are_noted() {
    let pairs = [
        (0x1000, 0x2000),
        (0x3000, 0x4000),
        (0x5000, 0x1000),
        (0x6000, 0x2000),
    ];
    // addr-keyed: test fixture.
    let fin_only: rustc_hash::FxHashSet<usize> =
        [0x2000, 0x1000, 0x7000, 0x3000].into_iter().collect();
    // 0x2000: a referent only a resurrected object holds -> noted, once.
    // 0x1000: also a `Reference` object of a row -> not noted.
    // 0x4000: not finalizer-only -> not noted; 0x7000: no row names it.
    assert_eq!(
        g1_finalizer_only_weak_referents(&pairs, &fin_only),
        vec![0x2000]
    );
    assert!(g1_finalizer_only_weak_referents(&[], &fin_only).is_empty());
    assert!(g1_finalizer_only_weak_referents(&pairs, &rustc_hash::FxHashSet::default()).is_empty());
}

/// The helper notes them on the processor after the verdict, before the
/// round (`run_collection_pause` calls the helper before
/// `process_references_after_gc`).
#[test]
fn gci_i30_rp_the_helper_notes_the_finalizer_only_referents() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let at = src
        .find("fn g1_pinned_young_referents_unheld(")
        .expect("helper");
    let rest = &src[at..];
    let helper = &rest[..rest.find("\n\u{7d}\n").expect("end")];
    let verdict = helper
        .find(".g1_pinned_young_referents_unheld(&candidates, &roots);")
        .expect("the verdict");
    let picked = helper
        .find("g1_finalizer_only_weak_referents(&pairs, &v.finalizer_only)")
        .expect("the pick");
    let noted = helper
        .find(".note_resurrected_finalizables(&noted);")
        .expect("the note");
    assert!(verdict < picked && picked < noted);
    // The side-table owners join the candidates, behind their own kill,
    // before the verdict is taken.
    let owners = helper
        .find(
            "cratonvm_native_builtins::gc_weak_row_owners(shared.vm_identity, &mut table_owners);",
        )
        .expect("the owner list");
    assert!(owners < verdict);
    assert!(helper.contains("let mut table_owners: Vec<usize> = Vec::new();"));
    // The kept regions' dead finalizables join the noted referents.
    assert!(helper.contains("let mut noted = {"));
}
