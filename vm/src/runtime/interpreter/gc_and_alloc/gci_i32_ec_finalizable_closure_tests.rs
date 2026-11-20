use super::*;

/// A copy maps back to its key, an identity entry to itself, an unmapped
/// (kept-region) address to itself; a map entry whose value is not in the
/// set contributes nothing.
#[test]
fn gci_i32_ec_closure_addresses_map_back_to_pre_collection_addresses() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000, 0x9000); // copied
    map.insert(0x3000, 0x3000); // identity
    map.insert(0x4000, 0xa000); // copied, not in the closure
                                // addr-keyed: test fixture.
    let post: rustc_hash::FxHashSet<usize> = [0x9000, 0x3000, 0x5000].into_iter().collect();
    let mut pre: Vec<usize> = g1_closure_pre_collection_addrs(&post, &map)
        .into_iter()
        .collect();
    pre.sort_unstable();
    assert_eq!(pre, vec![0x2000, 0x3000, 0x5000]);
    // addr-keyed: test fixture.
    let none: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    assert!(g1_closure_pre_collection_addrs(&none, &map).is_empty());
}

/// The helper asks for the closure only for the dead finalizables it
/// splits out, inside the `no_refproc` guard and behind the kill, and
/// before it notes the set.
#[test]
fn gci_i32_ec_the_closure_is_noted_with_the_dead_finalizables() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let at = src
        .find("fn g1_pinned_young_referents_unheld(")
        .expect("helper");
    let rest = &src[at + 1..];
    let helper = &rest[..rest.find("fn ").expect("next fn")];
    let pos = |needle: &str| helper.find(needle).expect(needle);
    let split = pos("g1_kept_young_dead_finalizables(&finalizable_candidates, &mut v);");
    let guard = pos("if !dead.is_empty()");
    let call = pos("noted.extend(g1_kept_finalizable_private_weak_referents(");
    let handed = pos("kept_dead_finalizables.extend(dead);");
    let noted = pos(".note_resurrected_finalizables(&noted);");
    assert!(split < guard && guard < call && call < handed && handed < noted);
}
