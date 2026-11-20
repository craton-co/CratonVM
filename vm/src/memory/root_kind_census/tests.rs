use super::*;

/// A toy heap: `addr -> (size, class, children)`.
fn toy<'a>(
    objects: &'a [(usize, usize, u32, &'a [usize])],
) -> impl FnMut(usize, &mut dyn FnMut(usize)) -> Option<(usize, u32)> + 'a {
    move |addr, visit| {
        let &(_, size, cid, kids) = objects.iter().find(|o| o.0 == addr)?;
        for &k in kids {
            visit(k);
        }
        Some((size, cid))
    }
}

fn kind(label: &str, roots: &[usize]) -> Kind {
    Kind {
        label: label.to_string(),
        roots: roots.to_vec(),
    }
}

/// First reach wins, in kind order; a shared subgraph is the first kind's;
/// a non-object word counts as a root but resolves nothing; cycles end.
#[test]
fn the_first_kind_to_reach_an_object_is_credited() {
    let heap: &[(usize, usize, u32, &[usize])] = &[
        (0x100, 16, 1, &[0x200, 0x300]),
        (0x200, 1000, 2, &[0x100]),
        (0x300, 50, 3, &[]),
        (0x400, 24, 1, &[0x300, 0x500]),
        (0x500, 2000, 2, &[]),
    ];
    let kinds = vec![
        kind("a", &[0x100, 0x999]),
        kind("b", &[0x400]),
        kind("c", &[0x300]),
    ];
    let mut obj = toy(heap);
    let r = attribute(&kinds, &mut obj, 12);
    assert_eq!(r.total_bytes, 16 + 1000 + 50 + 24 + 2000);
    assert_eq!(r.total_objects, 5);
    assert_eq!((r.rows[0].roots, r.rows[0].resolved), (2, 1));
    assert_eq!(r.rows[0].first_bytes, 1066);
    assert_eq!(r.rows[1].first_bytes, 2024);
    assert_eq!(r.rows[2].first_bytes, 0);
    // Exclusive: a keeps 0x100 and 0x200 alone; 0x300 is shared with b and c.
    assert_eq!(r.rows[0].exclusive_bytes, Some(1016));
    assert_eq!(r.rows[1].exclusive_bytes, Some(2024));
    // c reached nothing first, so it gets no exclusive figure.
    assert_eq!(r.rows[2].exclusive_bytes, None);
    // Top classes by bytes, largest first.
    assert_eq!(r.rows[0].top_classes[0], (2, 1, 1000));
    // Top roots, largest first.
    assert_eq!(r.top_roots[0].addr, 0x400);
    assert_eq!(r.top_roots[0].bytes, 2024);
    assert_eq!(r.top_roots[1].addr, 0x100);
    assert!(r.reached.contains_key(&0x500));
}

#[test]
fn exclusive_is_never_above_first_reach_and_respects_the_cap() {
    let heap: &[(usize, usize, u32, &[usize])] = &[
        (0x10, 100, 1, &[0x20]),
        (0x20, 100, 1, &[]),
        (0x30, 100, 1, &[0x20]),
    ];
    let kinds = vec![kind("x", &[0x10]), kind("y", &[0x30])];
    let mut obj = toy(heap);
    let r = attribute(&kinds, &mut obj, 1);
    // Only the largest kind is computed under a cap of one.
    assert_eq!(r.rows[0].exclusive_bytes, Some(100));
    assert_eq!(r.rows[1].exclusive_bytes, None);
    for row in &r.rows {
        if let Some(e) = row.exclusive_bytes {
            assert!(e <= row.first_bytes);
        }
    }
}

#[test]
fn the_root_vector_splits_by_section_and_by_snapshot_thread() {
    let roots = [1, 2, 3, 4, 5, 6];
    let sections: &[(usize, &'static str)] = &[(0, "s0"), (2, "snaps"), (5, "xt")];
    let label = |w: usize| (w == 3).then(|| "snapshot T1".to_string());
    let kinds = split_kinds(
        &roots,
        sections,
        Some((2, 5)),
        &label,
        vec![("extra".to_string(), vec![9])],
    );
    let names: Vec<&str> = kinds.iter().map(|k| k.label.as_str()).collect();
    // Kinds appear in the order their first word does.
    assert_eq!(names, vec!["s0", "snapshot T1", "snaps", "xt", "extra"]);
    assert_eq!(kinds[0].roots, vec![1, 2]);
    assert_eq!(kinds[1].roots, vec![3]);
    assert_eq!(kinds[2].roots, vec![4, 5]);
    assert_eq!(kinds[3].roots, vec![6]);
    assert_eq!(kinds[4].roots, vec![9]);
}

#[test]
fn the_region_summary_counts_unreached_regions_and_deciles() {
    // addr-keyed: not an address (a region index), test-local.
    let mut m: FxHashMap<usize, (u64, u64)> = FxHashMap::default();
    m.insert(0, (1000, 0));
    m.insert(1, (1000, 1000));
    m.insert(2, (1000, 450));
    let (regions, used, reached, zero, d) = region_summary(&m);
    assert_eq!((regions, used, reached, zero), (3, 3000, 1450, 1));
    assert_eq!(d[0], 1);
    assert_eq!(d[4], 1);
    assert_eq!(d[9], 1);
}

#[test]
fn the_switch_is_off_by_default() {
    if cratonvm_types::flags::runtime_var_os(KEY).is_none() {
        assert_eq!(mode(), CensusMode::Off);
    }
}
