use super::drop_swept_reference_rows;
use cratonvm_gc::ReferenceType;

/// `gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`:
/// a weak row whose old `Reference` a sweep slice freed is gone after the
/// slice's prune; a row between two freed spans, one registered after the
/// sweep's snapshot on a freed block, and an empty slice change nothing.
#[test]
fn a_sweep_slice_drops_the_rows_of_the_references_it_freed() {
    let mut proc = cratonvm_gc::ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Weak, 0x10_000, 0x90_000, None);
    proc.discover_reference(ReferenceType::Weak, 0x10_100, 0x90_100, None);
    proc.discover_reference(ReferenceType::Phantom, 0x10_200, 0x90_200, None);
    let before = proc.registration_seq();
    proc.discover_reference(ReferenceType::Weak, 0x10_200, 0x90_300, None);
    let spans = [(0x10_000usize, 0x40usize), (0x10_200, 0x40)];

    assert_eq!(drop_swept_reference_rows(&mut proc, before, &[]), 0);
    assert_eq!(drop_swept_reference_rows(&mut proc, before, &spans), 2);
    let mut left = proc.reference_object_addresses();
    left.sort_unstable();
    assert_eq!(
        left,
        vec![0x10_100, 0x10_200],
        "the in-between row and the new one"
    );
    assert_eq!(proc.weak_ref_count(), 2);
    assert_eq!(proc.phantom_ref_count(), 0);
}
