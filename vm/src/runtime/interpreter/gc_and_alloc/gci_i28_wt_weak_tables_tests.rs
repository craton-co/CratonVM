use super::*;

fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

/// The JNI referents join the reference rows' candidates with the same
/// screen: no null, no map key, no finalizable; ascending, each once.
#[test]
fn gci_i28_wt_jni_weak_referents_join_the_candidates_with_the_same_screen() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000, 0x9000); // moved
    map.insert(0x3000, 0x3000); // identity survivor
    let finalizable = [0x6000];
    let pairs = [(0x1000, 0x4000)];
    let mut candidates = g1_pinned_referent_candidates(&pairs, &finalizable, &map);
    g1_pinned_referent_extra_candidates(
        &mut candidates,
        &[0x8000, 0, 0x2000, 0x3000, 0x6000, 0x4000, 0x0800],
        &finalizable,
        &map,
    );
    assert_eq!(candidates, vec![0x0800, 0x1000, 0x4000, 0x8000]);

    let mut none: Vec<usize> = Vec::new();
    g1_pinned_referent_extra_candidates(&mut none, &[], &finalizable, &map);
    assert!(none.is_empty());
    g1_pinned_referent_extra_candidates(&mut none, &[0x7000], &finalizable, &map);
    assert_eq!(none, vec![0x7000], "JNI referents alone are candidates");
}

/// The door takes the verdict once, hands it to the pause's one relocation
/// set before the reference round, and the JNI sweep and
/// `update_all_roots` borrow that same set; the helper offers the JNI weak
/// referents, and gates the reference rows (not the whole verdict) on
/// `CRATONVM_WEAKREF_CLEAR` and `CRATONVM_DBG_NO_REFPROC`.
#[test]
fn gci_i28_wt_the_door_takes_the_verdict_once_before_every_weak_consumer() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let door = body(&src, "fn run_collection_pause(");
    assert_eq!(
        door.matches("g1_pinned_young_referents_unheld(").count(),
        1,
        "one verdict per pause"
    );
    let build = door.find("RelocationTargets::for_pause(").expect("build");
    let carried = door
        // gci i31/gq: the helper also hands back the kept regions' dead
        // registered finalizables (the out-parameter).
        .find(concat!(
            ".with_g1_unheld(g1_pinned_young_referents_unheld(\n",
            "        shared,\n",
            "        &result.pointer_map,\n",
            "        &roots,\n",
            "        &mut g1_kept_dead_finalizables,\n",
            "    ));"
        ))
        .expect("carried on the set");
    let refs = door
        .find("    process_references_after_gc(\n")
        .expect("refs");
    let jni = door
        .find("crate::native::jni::sweep_weak_global_refs_with(")
        .expect("jni sweep");
    let update = door
        .find("update_all_roots(shared, thread, &result.pointer_map, &relocation_targets);")
        .expect("update_all_roots");
    assert!(build < carried && carried < refs && refs < jni && jni < update);

    let refs_fn = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    assert!(
        !refs_fn.contains("g1_pinned_young_referents_unheld("),
        "the round no longer takes its own verdict"
    );

    let helper = body(&src, "fn g1_pinned_young_referents_unheld(");
    assert!(helper.contains("crate::native::jni::weak_global_referent_addrs(shared)"));
    assert!(helper.contains("g1_pinned_referent_extra_candidates(&mut candidates, &jni_weak,"));
    assert!(helper.contains("let pairs = if weakref_clear_enabled() && !no_refproc() \u{7b}"));
    let gate = &helper[..helper.find("let pairs = ").expect("pairs")];
    assert!(
        !gate.contains("weakref_clear_enabled()"),
        "CRATONVM_WEAKREF_CLEAR=0 no longer turns the JNI half off"
    );
}

/// `InPlaceVerdict::survived` asks the pause's unheld set before anything
/// else, so every side-table sweep handed `in_place.survived` and the JNI
/// sweep refuse an unheld address.
#[test]
fn gci_i28_wt_the_in_place_verdict_screens_the_unheld_set_first() {
    let src = include_str!("../../../memory/addr_keyed.rs").replace("\r\n", "\n");
    let survived = body(
        &src,
        "    pub(crate) fn survived(&self, addr: usize) -> bool \u{7b}",
    );
    let screen = survived
        .find("if self.targets.get().g1_unheld(addr) \u{7b}")
        .expect("the screen");
    let parse = survived
        .find("if self.heap.is_object_address(addr).is_none() \u{7b}")
        .expect("the parse test");
    assert!(screen < parse);
}
