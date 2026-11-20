use super::*;

/// Registered rows only: not null, not a key of the map (moved, identity
/// survivor, resurrected copy), not waiting in the queue; ascending, once.
#[test]
fn gci_i31_gq_finalizable_candidates_skip_mapped_queued_and_null() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000, 0x9000); // moved (or resurrected from the CSet)
    map.insert(0x3000, 0x3000); // identity survivor
    let registered = [0x5000, 0x2000, 0, 0x3000, 0x4000, 0x1000, 0x4000];
    let queued = [0x1000, 0x8000];
    assert_eq!(
        g1_kept_young_finalizable_candidates(&registered, &queued, &map),
        vec![0x4000, 0x5000]
    );
    assert!(g1_kept_young_finalizable_candidates(&[], &queued, &map).is_empty());
}

/// An offered finalizable found unheld or finalizer-only is dead: it
/// leaves both sets (no in-place consumer may read it dead) and is
/// returned; a held one stays registered; the weak candidates' answers are
/// untouched.
#[test]
fn gci_i31_gq_dead_finalizables_leave_the_verdict_sets() {
    let mut v = cratonvm_gc::g1::G1PinnedReferentVerdict {
        unheld: [0x1000, 0x4000, 0x7000].into_iter().collect(),
        finalizer_only: [0x5000, 0x8000].into_iter().collect(),
        ..Default::default()
    };
    // 0x4000 unheld, 0x5000 finalizer-only, 0x6000 held.
    let dead = g1_kept_young_dead_finalizables(&[0x4000, 0x5000, 0x6000], &mut v);
    assert_eq!(dead, vec![0x4000, 0x5000]);
    let mut unheld: Vec<usize> = v.unheld.iter().copied().collect();
    unheld.sort_unstable();
    assert_eq!(unheld, vec![0x1000, 0x7000]);
    let fin_only: Vec<usize> = v.finalizer_only.iter().copied().collect();
    assert_eq!(fin_only, vec![0x8000]);
    assert!(g1_kept_young_dead_finalizables(&[], &mut v).is_empty());
}

/// The helper offers the finalizables behind their kill, before the
/// verdict, splits them out after it and hands them back; the door joins
/// them to `dead_finalizers` after the helper and before the round and
/// the enqueue.
#[test]
fn gci_i31_gq_the_dead_finalizables_reach_the_door_s_queue() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let body = |start: &str| -> String {
        let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let rest = &src[at..];
        rest[..rest.find("\n\u{7d}\n").expect("end")].to_string()
    };
    let pos = |b: &str, needle: &str| b.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let helper = body("fn g1_pinned_young_referents_unheld(");
    let gate = pos(&helper, "let finalizable_candidates =");
    let verdict = pos(
        &helper,
        ".g1_pinned_young_referents_unheld(&candidates, &roots);",
    );
    let split = pos(
        &helper,
        "g1_kept_young_dead_finalizables(&finalizable_candidates, &mut v);",
    );
    let handed = pos(&helper, "kept_dead_finalizables.extend(dead);");
    let noted = pos(&helper, ".note_resurrected_finalizables(&noted);");
    let unheld = pos(&helper, "            v.unheld\n");
    assert!(gate < verdict && verdict < split && split < handed && handed < noted);
    assert!(noted < unheld);
    let door = body("fn run_collection_pause(");
    let call = pos(&door, "&mut g1_kept_dead_finalizables,");
    let join = pos(&door, "dead_finalizers.extend(g1_kept_dead_finalizables);");
    let round = pos(&door, "    process_references_after_gc(\n");
    let enqueue = pos(
        &door,
        "enqueue_resurrected_finalizers(shared, &dead_finalizers);",
    );
    assert!(call < join && join < round && round < enqueue);
}
