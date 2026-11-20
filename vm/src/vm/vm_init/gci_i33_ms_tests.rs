use super::*;

const K: u64 = 1024;
const M: u64 = 1024 * 1024;

#[test]
fn gci_i33ms_arena_footprint_granules_and_class_space_size() {
    let nc = &HS_STANDARD_NON_CLASS_CHUNKS;
    assert_eq!(hs_arena_footprint(0, nc), 0, "no arena, no chunk");
    assert_eq!(hs_arena_footprint(1, nc), 4 * K);
    assert_eq!(hs_arena_footprint(4 * K, nc), 4 * K);
    assert_eq!(hs_arena_footprint(4 * K + 1, nc), 8 * K);
    assert_eq!(hs_arena_footprint(12 * K + 1, nc), 20 * K);
    // Past the sequence the last chunk (16K) repeats.
    assert_eq!(hs_arena_footprint(36 * K + 1, nc), 52 * K);
    assert_eq!(hs_arena_footprint(1, &HS_STANDARD_CLASS_CHUNKS), 2 * K);
    assert_eq!(hs_round_up(0, HS_COMMIT_GRANULE_BYTES), 0);
    assert_eq!(hs_round_up(1, HS_COMMIT_GRANULE_BYTES), 64 * K);
    assert_eq!(hs_round_up(64 * K, HS_COMMIT_GRANULE_BYTES), 64 * K);
    assert_eq!(hs_compressed_class_space_bytes(None), 1024 * M);
    // 80% of 32m (26,843,544) aligned up to 4M.
    assert_eq!(hs_compressed_class_space_bytes(Some(32 * M)), 28 * M);
    // At least one root chunk.
    assert_eq!(hs_compressed_class_space_bytes(Some(M)), 4 * M);
    assert_eq!(hs_compressed_class_space_bytes(Some(u64::MAX)), 1024 * M);
}

#[test]
fn gci_i33ms_stats_split_out_the_class_space() {
    let s = hs_stats_from_sums([100_000, 30_000], [5_000, 1_000], [8 * K, 2 * K], None);
    assert_eq!(s.used, 136_000);
    // Non-class: the boot arena by granule (128K) + the others' chunks
    // by granule (64K); class: 64K + 64K.
    assert_eq!(s.committed, 192 * K + 128 * K);
    assert_eq!(s.reserved, 64 * M + 1024 * M, "1114112K, HotSpot's figure");
    assert_eq!(s.class_space, Some((31_000, 128 * K, 1024 * M)));
    assert_eq!(s.non_class(), (105_000, 192 * K, 64 * M));
    assert_eq!(s.triple(), (136_000, 320 * K, 1088 * M));
    assert_eq!(MetaspaceStats::from_triples(s.triple(), s.class_space), s);
    // A non-class space past one node reserves whole nodes.
    let big = hs_stats_from_sums([65 * M, 0], [0, 0], [0, 0], None);
    assert_eq!(big.non_class().2, 128 * M);
    // The pre-i33 shape.
    let flat = MetaspaceStats::flat(5);
    assert_eq!((flat.triple(), flat.class_space), ((5, 5, 5), None));
    assert_eq!(flat.non_class(), (5, 5, 5));
}

/// One walk: the stats' `used` is the figure every consumer reported, and
/// committed covers it.
#[test]
fn gci_i33ms_stats_are_the_metaspace_figure() {
    let shared = SharedVm::new(VmConfig::default());
    let cm = shared.classes.class_manager.read();
    let stats = metaspace_stats(&cm);
    assert_eq!(stats.used, metaspace_used_bytes(&cm) as u64);
    assert!(stats.committed >= stats.used);
    if metaspace_hotspot_figure_enabled() && metaspace_hotspot_spaces_enabled() {
        let (class_used, class_committed, class_reserved) =
            stats.class_space.expect("a class space");
        assert!(class_used <= stats.used && class_committed <= stats.committed);
        assert_eq!(class_reserved, 1024 * M);
        assert_eq!(stats.committed % HS_COMMIT_GRANULE_BYTES, 0);
    } else {
        assert_eq!(stats, MetaspaceStats::flat(stats.used));
    }
}

/// The `-XX:MaxMetaspaceSize` check: no limit, never; the model's
/// committed bytes against the limit, the tally caught up with the store.
#[test]
fn gci_i33ms_the_limit_checks_the_committed_model() {
    use cratonvm_types::ClassLoaderId;
    let shared = SharedVm::new(VmConfig::default());
    {
        let cm = shared.classes.class_manager.read();
        assert!(!metaspace_limit_would_exceed(&cm, ClassLoaderId::Application, u64::MAX));
    }
    shared.classes.class_manager_write().set_max_metaspace_bytes(Some(1));
    {
        let cm = shared.classes.class_manager.read();
        // Two bytes more are at least a granule committed, over 1 byte;
        // the pre-i33 check (non-boot class-file bytes) agrees here.
        assert!(metaspace_limit_would_exceed(&cm, ClassLoaderId::Application, 2));
        if metaspace_limit_hotspot_check_enabled() && metaspace_hotspot_figure_enabled() {
            let tally = cm.metaspace_arena_tally().lock();
            assert_eq!(tally.classes, cm.class_store().len());
            assert_eq!(tally.scanned_slots, cm.class_store().slot_count());
            assert_eq!(*tally, hs_metaspace_arenas(&cm), "the incremental sums are the walk's");
        }
    }
    shared.classes.class_manager_write().set_max_metaspace_bytes(Some(u64::MAX));
    let cm = shared.classes.class_manager.read();
    assert!(!metaspace_limit_would_exceed(&cm, ClassLoaderId::UserDefined(7), 4096));
}

/// gci i36/ms (vi33-ms S1 hand-back): the class-space census by `Klass`
/// kind -- every kind has its `Klass` in the class space (HotSpot 25 has
/// no JDK-8338526), and the census is printed for the record
/// (`cargo test -j 5 -p cratonvm-vm --lib gci_i36ms_ -- --nocapture`:
/// `[ms36] census kind=<k> count=<n> class=<bytes> nonclass=<bytes>
/// perklass=<class/n>`, then the per-class class-space figure the S1 rule
/// divides by the instance-class count).
#[test]
fn gci_i36ms_class_space_census_by_klass_kind() {
    const KINDS: [&str; 6] =
        ["concrete", "abstract", "interface", "hidden", "obj-array", "type-array"];
    let shared = SharedVm::new(VmConfig::default());
    let cm = shared.classes.class_manager.read();
    let store = cm.class_store();
    let mut shapes = HsKlassShapes::new(store);
    let mut census = [(0usize, 0usize, 0usize); 6];
    let (mut class_total, mut instances) = (0usize, 0usize);
    for c in store.iter() {
        let (non_class, class) = shapes.class_parts(c, hs_class_type_annotations_len(&cm, c.id));
        let kind = if hs_is_array_class(c) {
            if c.name.len() == 2 {
                5
            } else {
                4
            }
        } else if c.hidden {
            3
        } else if c.is_interface() {
            2
        } else if c.is_abstract() {
            1
        } else {
            0
        };
        assert!(class >= HS_ARRAY_KLASS_BYTES, "every Klass is in the class space: {}", c.name);
        census[kind].0 += 1;
        census[kind].1 += class;
        census[kind].2 += non_class;
        class_total += class;
        if kind < 4 {
            instances += 1;
        }
    }
    for (name, (count, class, non_class)) in KINDS.iter().zip(census) {
        eprintln!(
            "[ms36] census kind={name} count={count} class={class} nonclass={non_class} perklass={}",
            if count > 0 { class / count } else { 0 }
        );
    }
    eprintln!(
        "[ms36] census instances={instances} class_total={class_total} per_instance_class={}",
        if instances > 0 { class_total / instances } else { 0 }
    );
    // The split is the model's: class + non-class is the used figure.
    let tally = hs_metaspace_arenas(&cm);
    assert_eq!(
        (tally.boot[1] + tally.others_used[1]) as usize,
        class_total,
        "the census sums the class space"
    );
}
