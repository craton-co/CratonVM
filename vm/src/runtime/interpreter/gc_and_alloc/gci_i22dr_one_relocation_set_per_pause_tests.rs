fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

#[test]
fn gci_i22dr_the_door_builds_one_set_and_lends_it_to_three_verdicts() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let door = body(&src, "fn run_collection_pause(");
    // The pause ends at the release. zt t31/zc11's after-release reference
    // round builds its own set over an EMPTY map (no pointer moved) with
    // the mutators running: not an in-pause verdict.
    let release_at = door
        .find("complete_gc(result.pointer_map);")
        .expect("release");
    let (in_pause, after_release) = door.split_at(release_at);
    assert_eq!(
        in_pause.matches("RelocationTargets::for_pause(").count(),
        1,
        "one build per pause"
    );
    assert_eq!(
        after_release
            .matches("RelocationTargets::for_pause(")
            .count(),
        after_release
            .matches("RelocationTargets::for_pause(&shared.mem.heap, &no_moves)")
            .count(),
        "after the release, only the empty-map round's set"
    );
    assert!(after_release.contains("let no_moves = cratonvm_types::PointerMap::default();"));
    assert!(!door.contains("InPlaceVerdict::capture_after("));
    let build = door.find("RelocationTargets::for_pause(").expect("build");
    let refs = door
        .find("    process_references_after_gc(\n")
        .expect("refs");
    let jni = door
        .find(concat!(
            "InPlaceVerdict::capture_with(\n",
            "            &shared.mem.heap,\n",
            "            &relocation_targets,"
        ))
        .expect("jni sweep");
    let update = door
        .find("update_all_roots(shared, thread, &result.pointer_map, &relocation_targets);")
        .expect("update_all_roots");
    let dropped = door.find("drop(relocation_targets);").expect("drop");
    let release = door
        .find("complete_gc(result.pointer_map);")
        .expect("release");
    assert!(
        build < refs && refs < jni && jni < update && update < dropped && dropped < release,
        "{build} {refs} {jni} {update} {dropped} {release}"
    );
    let refs_fn = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    assert!(refs_fn.contains(
        "InPlaceVerdict::capture_with(\n        &shared.mem.heap,\n        pause_relocations,"
    ));
    assert!(!refs_fn.contains("InPlaceVerdict::capture_after("));
    assert!(
        !refs_fn.contains(concat!(
            "let relocation_targets: rustc_hash::FxHashSet<",
            "usize> ="
        )),
        "process_references_after_gc reuses pause_relocations instead of building a private set"
    );
    assert!(
        refs_fn.contains("pause_relocations.claims(referent_old)"),
        "referent restore screen uses pause_relocations.claims"
    );
    let gc = include_str!("../../../memory/gc.rs").replace("\r\n", "\n");
    let update_fn = body(&gc, "\npub fn update_all_roots(");
    assert!(update_fn.contains("InPlaceVerdict::capture_with("));
    assert!(!update_fn.contains("InPlaceVerdict::capture_after("));
}
