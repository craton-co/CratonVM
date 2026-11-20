use super::*;
#[test]
fn an_expired_inner_soft_edge_keeps_its_owner_but_clears_its_referent() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x20, None);
    proc.discover_reference(ReferenceType::Soft, 0x21, 0x30, None);
    proc.touch_soft_reference(0x10, 2000);
    let marked = |a| a == 0x10;
    let trace = |roots: &[usize]| {
        roots
            .iter()
            .flat_map(|a| match *a {
                0x20 => vec![0x20, 0x21],
                _ => vec![*a],
            })
            .collect()
    };
    let kept = proc.soft_retention_closure(&marked, &trace, 1, 2000);
    assert_eq!(kept, [0x20, 0x21].into_iter().collect());
    proc.remove_collected_reference_objects(&|a| marked(a) || kept.contains(&a));
    proc.process_references_with_finalizer_trace(&marked, Some(&trace), 1, 2000);
    assert_eq!(proc.take_newly_cleared(), vec![0x21]);
    assert_eq!(proc.soft_survivor_referents(), vec![0x20]);
}

#[test]
fn retained_soft_edges_admit_nested_owners_but_never_disconnected_rows() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x20, None);
    proc.discover_reference(ReferenceType::Soft, 0x21, 0x30, None);
    proc.discover_reference(ReferenceType::Soft, 0x90, 0xa0, None);
    let marked = |a| a == 0x10;
    let trace = |roots: &[usize]| {
        roots
            .iter()
            .flat_map(|a| match *a {
                0x20 => vec![0x20, 0x21],
                0x30 => vec![0x30, 0x31],
                _ => vec![*a],
            })
            .collect()
    };
    let kept = proc.soft_retention_closure(&marked, &trace, 1000, 100);
    assert_eq!(kept, [0x20, 0x21, 0x30, 0x31].into_iter().collect());
    proc.remove_collected_reference_objects(&|a| marked(a) || kept.contains(&a));
    assert_eq!(proc.soft_survivor_referents().len(), 2);
    proc.condemn_all_soft_refs();
    assert!(proc
        .soft_retention_closure(&marked, &trace, 1000, 100)
        .is_empty());
}
