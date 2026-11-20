use super::*;

/// gcf f3/o: one compact entry per explained marker -- its object and its
/// FIRST holder; a marker with no holder line says so.
#[test]
fn gcf_f3o_compact_holder_summary_names_each_markers_first_holder() {
    let lines: Vec<String> = [
        "[holder-census] major #3 markers staged=2 resolved=2 reached_from_roots=2 explained=2",
        "[holder-census]   marker#1 0x1000 long[] old reach_bytes=8192 (auto: big)",
        "[holder-census]     holder#1 root[7]=0x2000 cat=jit/sp path: Chain -> long[]",
        "[holder-census]     holder#2 root[9]=0x3000 cat=static/x path: X",
        "[holder-census]     marker#1: 2 holder(s); with their objects blocked no root reaches it",
        "[holder-census]   marker#2 0x4000 Chain old reach_bytes=64 (staged)",
        "[holder-census]     marker#2: 0 holder(s); with their objects blocked no root reaches it",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let s = compact_holder_summary(&lines);
    assert_eq!(
        s,
        vec![
            "marker#1 0x1000 long[] old reach_bytes=8192 | holder#1 root[7]=0x2000 cat=jit/sp path: Chain -> long[]"
                .to_string(),
            "marker#2 0x4000 Chain old reach_bytes=64 | holder#1 none".to_string(),
        ]
    );
    assert!(compact_holder_summary(&[]).is_empty());
}

/// gce e2/f: a primitive array is named by its element tag; a reference
/// array, an instance and a bad tag fall back to the class-id name.
#[test]
fn gce_e2f_a_primitive_array_is_named_by_its_element_tag() {
    use cratonvm_types::{ArrayElementType as E, ObjectKind as K};
    let arr = K::Array as u8;
    assert_eq!(primitive_array_descriptor(arr, E::Char as u8), Some("[C"));
    assert_eq!(primitive_array_descriptor(arr, E::Byte as u8), Some("[B"));
    assert_eq!(primitive_array_descriptor(arr, E::Long as u8), Some("[J"));
    assert_eq!(
        primitive_array_descriptor(arr, E::Boolean as u8),
        Some("[Z")
    );
    assert_eq!(primitive_array_descriptor(arr, E::Reference as u8), None);
    assert_eq!(primitive_array_descriptor(arr, 0xEE), None, "bad tag");
    assert_eq!(
        primitive_array_descriptor(K::Object as u8, E::Char as u8),
        None,
        "an instance's element bits are hash bits"
    );
}

/// gcf f2/f: a reference array is named `[L<element>;` when its header
/// class id names the element class, and kept when it names an array.
#[test]
fn gcf_f2f_a_reference_array_is_named_as_an_array() {
    use cratonvm_types::{ArrayElementType as E, ObjectKind as K};
    let arr = K::Array as u8;
    assert!(is_reference_array(arr, E::Reference as u8));
    assert!(!is_reference_array(arr, E::Char as u8));
    assert!(!is_reference_array(K::Object as u8, E::Reference as u8));
    assert!(!is_reference_array(arr, 0xEE), "bad tag");
    assert_eq!(
        reference_array_name("java/lang/Object".to_string()),
        "[Ljava/lang/Object;"
    );
    assert_eq!(
        reference_array_name("[Ljava/lang/String;".to_string()),
        "[Ljava/lang/String;"
    );
}

#[test]
fn containing_resolves_bases_and_interiors_and_rejects_gaps() {
    let grid = vec![(0x1000usize, 0x40usize), (0x2000, 0x10)];
    assert_eq!(containing(&grid, 0x1000), Some((0x1000, 0x40)));
    assert_eq!(containing(&grid, 0x1038), Some((0x1000, 0x40)));
    assert_eq!(containing(&grid, 0x1040), None, "one past the end");
    assert_eq!(containing(&grid, 0x0fff), None, "below the first object");
    assert_eq!(containing(&grid, 0x200f), Some((0x2000, 0x10)));
    assert_eq!(containing(&grid, 0x3000), None);
}

/// gcf f7/f: the holder line names an interior root's offset and whether
/// the word it points at is a stamped header.
#[test]
fn gcf_f7f_interior_root_suffix_names_offset_and_stamp() {
    // A 48-byte object: word 0 a stamped header, word 2 a `long` field
    // (`v = 7`, upper half zero, so its mark half carries no flags).
    // The flags nibble is mark bits 24..27; the mark is the word's upper
    // half (little-endian, `MARK_WORD_OFFSET` 4), so word bits 56..59.
    let stamp = (cratonvm_types::GC_FLAG_HEADER as u64) << 56;
    let mut obj: Box<[u64; 6]> = Box::new([0; 6]);
    obj[2] = 7;
    obj[4] = stamp;
    let base = obj.as_ptr() as usize;
    assert_eq!(interior_root_suffix(base, base, 48), "");
    assert_eq!(
        interior_root_suffix(base + 16, base, 48),
        " interior=+0x10 stamped=false"
    );
    assert_eq!(
        interior_root_suffix(base + 32, base, 48),
        " interior=+0x20 stamped=true"
    );
    // The mark half would run past the object's end: not read.
    assert_eq!(
        interior_root_suffix(base + 46, base, 48),
        " interior=+0x2e stamped=?"
    );
}

/// gen r5w6/oomjit10: with no staged marker the census explains the
/// largest ROOT-REACHED old objects of at least a mebibyte, largest first.
/// Nothing is dereferenced, so the grid may name made-up addresses.
#[test]
fn auto_markers_are_the_largest_root_reached_old_objects() {
    let at = std::ptr::without_provenance_mut::<u8>;
    let old: Vec<(*mut u8, usize)> = vec![
        (at(0x10_0000), 2 << 20),
        (at(0x40_0000), 8 << 20),
        (at(0x100_0000), 64),
        (at(0x200_0000), 4 << 20),
    ];
    let young: Vec<(usize, usize)> = Vec::new();
    let mut walk = Walk {
        old: &old,
        young: &young,
        visited: FxHashSet::default(),
        stack: Vec::new(),
        scratch: Vec::new(),
    };
    for base in [0x10_0000usize, 0x40_0000, 0x100_0000] {
        walk.visited.insert(base);
    }
    let picked: Vec<usize> = auto_holder_markers(&walk).iter().map(|m| m.addr).collect();
    assert_eq!(
        picked,
        vec![0x40_0000, 0x10_0000],
        "an unreached object and one below the floor are not explained"
    );
}

#[test]
fn the_tail_scope_labels_appended_seeds_and_restores() {
    let empty_vm: FxHashMap<usize, &'static str> = FxHashMap::default();
    let empty_jit: FxHashSet<usize> = FxHashSet::default();
    assert_eq!(TAIL.with(Cell::get), None);
    {
        let _outer = TailScope::new(3, "promotion-dest");
        let tail = TAIL.with(Cell::get);
        assert_eq!(
            label_for(2, 0x5000, tail, &[], &empty_vm, &empty_jit),
            ("vm", "unattributed")
        );
        assert_eq!(
            label_for(3, 0x5000, tail, &[], &empty_vm, &empty_jit),
            ("seed", "promotion-dest")
        );
        // The tail outranks a staged section: the gen heap appended those
        // entries after the door staged its map.
        assert_eq!(
            label_for(3, 0x5000, tail, &[(0, "initiator")], &empty_vm, &empty_jit),
            ("seed", "promotion-dest")
        );
        {
            let _inner = TailScope::new(1, "finalizer-old");
            assert_eq!(TAIL.with(Cell::get), Some((1, "finalizer-old")));
        }
        assert_eq!(TAIL.with(Cell::get), Some((3, "promotion-dest")));
    }
    assert_eq!(TAIL.with(Cell::get), None);
}

#[test]
fn a_vm_label_outranks_the_jit_scan_registry() {
    let mut vm: FxHashMap<usize, &'static str> = FxHashMap::default();
    vm.insert(0x7000, "deopt-stash");
    let mut jit: FxHashSet<usize> = FxHashSet::default();
    jit.insert(0x7000);
    jit.insert(0x8000);
    assert_eq!(
        label_for(0, 0x7000, None, &[], &vm, &jit),
        ("vm", "deopt-stash")
    );
    assert_eq!(
        label_for(0, 0x8000, None, &[], &vm, &jit),
        ("jit", "frame-scan")
    );
}

/// gen r4w6/oomjit6: a staged section names every root by the part of
/// the VM that pushed it, refined by the registry that names its address,
/// and `-` when none does -- never `vm/unattributed`.
#[test]
fn a_staged_section_names_the_root_and_the_address_refines_it() {
    let mut vm: FxHashMap<usize, &'static str> = FxHashMap::default();
    vm.insert(0x7000, "interp-local-dead-by-liveness");
    let mut jit: FxHashSet<usize> = FxHashSet::default();
    jit.insert(0x8000);
    let sections = [
        (0usize, "initiator"),
        (10, "registry-snapshots"),
        (20, "xt-frozen-peer"),
    ];
    assert_eq!(
        label_for(3, 0x7000, None, &sections, &vm, &jit),
        ("initiator", "interp-local-dead-by-liveness")
    );
    assert_eq!(
        label_for(10, 0x8000, None, &sections, &vm, &jit),
        ("registry-snapshots", "jit-frame-scan")
    );
    assert_eq!(
        label_for(25, 0x9000, None, &sections, &vm, &jit),
        ("xt-frozen-peer", "-")
    );
    // Below the first staged start: the address-only label.
    assert_eq!(
        label_for(0, 0x9000, None, &[(5, "late")], &vm, &jit),
        ("vm", "unattributed")
    );
}

#[test]
fn section_of_picks_the_last_start_at_or_below_the_index() {
    let s = [(0usize, "a"), (4, "b"), (4, "c"), (9, "d")];
    assert_eq!(section_of(&s, 0), Some("a"));
    assert_eq!(section_of(&s, 3), Some("a"));
    assert_eq!(
        section_of(&s, 4),
        Some("c"),
        "equal starts: the last one wins"
    );
    assert_eq!(section_of(&s, 8), Some("c"));
    assert_eq!(section_of(&s, 1_000), Some("d"));
    assert_eq!(section_of(&[(2, "x")], 1), None);
    assert_eq!(section_of(&[], 0), None);
}

#[test]
fn staged_sections_sort_and_restore_on_drop() {
    let read = || SECTIONS.with(|s| s.borrow().clone());
    assert!(read().is_empty());
    {
        let _outer = stage_root_sections(vec![(7, "b"), (0, "a")]);
        assert_eq!(read(), vec![(0, "a"), (7, "b")], "sorted by start");
        {
            let _inner = stage_root_sections(vec![(0, "nested")]);
            assert_eq!(read(), vec![(0, "nested")]);
        }
        assert_eq!(
            read(),
            vec![(0, "a"), (7, "b")],
            "the outer staging is back"
        );
    }
    assert!(read().is_empty());
}

/// gen r5w4/jit8: a holder path is read back from the search's `Via` map
/// root-object first, and ends at the target.
#[test]
fn holder_path_walks_back_to_the_seed() {
    let mut via: FxHashMap<usize, Via> = FxHashMap::default();
    via.insert(0x10, Via::Root(3));
    via.insert(0x20, Via::Parent(0x10));
    via.insert(0x30, Via::Parent(0x20));
    via.insert(0x40, Via::YoungSeed);
    via.insert(0x50, Via::Parent(0x40));
    assert_eq!(
        holder_path(&via, 0x30),
        (Some(Via::Root(3)), vec![0x10, 0x20, 0x30])
    );
    assert_eq!(holder_path(&via, 0x10), (Some(Via::Root(3)), vec![0x10]));
    assert_eq!(
        holder_path(&via, 0x50),
        (Some(Via::YoungSeed), vec![0x40, 0x50])
    );
    assert_eq!(holder_path(&via, 0x99), (None, vec![0x99]), "not reached");
}

#[test]
fn a_long_holder_path_keeps_both_ends() {
    let name = |b: usize| format!("C{b}");
    assert_eq!(render_chain(&[1, 2], name), "C1@0x1 -> C2@0x2");
    let long: Vec<usize> = (1..=25).collect();
    let s = render_chain(&long, name);
    assert!(s.starts_with("C1@0x1 -> "), "{s}");
    assert!(s.ends_with(" -> C25@0x19"), "{s}");
    assert!(
        s.contains(&format!("... {} more ...", 25 - HOLDER_PATH_MAX)),
        "{s}"
    );
}

#[test]
fn staged_holder_markers_restore_on_drop() {
    let read = || MARKERS.with(|m| m.borrow().iter().map(|h| h.addr).collect::<Vec<_>>());
    assert!(read().is_empty());
    {
        let _outer = stage_holder_markers(vec![HolderMarker {
            addr: 0x1000,
            why: "outer".into(),
        }]);
        assert_eq!(read(), vec![0x1000]);
        {
            let _inner = stage_holder_markers(Vec::new());
            assert!(read().is_empty());
        }
        assert_eq!(read(), vec![0x1000], "the outer staging is back");
    }
    assert!(read().is_empty());
}

#[test]
fn flush_without_a_report_prints_nothing_and_takes_it_once() {
    PENDING.with(|p| *p.borrow_mut() = None);
    flush(0);
    PENDING.with(|p| *p.borrow_mut() = Some(String::from("x\n")));
    flush(1);
    assert!(PENDING.with(|p| p.borrow().is_none()));
}

/// gch h1/jo: the unreached-marker line names the object, its generation
/// and why it is watched, and the compact summary does not take it for an
/// explained marker.
#[test]
fn gch_h1jo_unreached_marker_line_is_named_and_not_summarised() {
    let line = unreached_marker_line(0x1000, "Box", false, "weak referent");
    assert!(
        line.starts_with("[holder-census]   unreached marker 0x1000 Box young (weak referent)"),
        "{line}"
    );
    assert!(compact_holder_summary(&[line]).is_empty());
    let old = unreached_marker_line(0x2000, "[J", true, "w");
    assert!(old.contains(" [J old (w)"), "{old}");
}

/// gch h1/jo: a staged marker no root entry reaches gets a line of its own
/// (it used to be only `resolved=1 reached_from_roots=0`). The old object
/// is a zeroed buffer: the census reads its header words and nothing else
/// (no root and no young object, so no search visits its fields).
#[test]
fn gch_h1jo_holder_census_names_a_marker_no_root_reaches() {
    let mut buf = vec![0u64; 8];
    let base = buf.as_mut_ptr() as usize;
    let old: Vec<(*mut u8, usize)> = vec![(buf.as_mut_ptr().cast::<u8>(), 64)];
    let young: Vec<(usize, usize)> = Vec::new();
    let walk = Walk {
        old: &old,
        young: &young,
        visited: FxHashSet::default(),
        stack: Vec::new(),
        scratch: Vec::new(),
    };
    let markers = vec![HolderMarker {
        addr: base + 8,
        why: "test referent".to_string(),
    }];
    let empty_vm: FxHashMap<usize, &'static str> = FxHashMap::default();
    let empty_jit: FxHashSet<usize> = FxHashSet::default();
    let lines = holder_census(1, &[], &walk, None, &[], &empty_vm, &empty_jit, &markers);
    assert!(
        lines[0].contains("resolved=1 reached_from_roots=0"),
        "{lines:?}"
    );
    let needle = format!("unreached marker 0x{base:x} ");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&needle) && l.contains(" old (test referent)")),
        "{lines:?}"
    );
    drop(walk);
    drop(buf);
}
