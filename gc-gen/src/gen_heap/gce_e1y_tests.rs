use super::*;

const MIB: usize = 1024 * 1024;

#[test]
fn e1y_the_keep_is_the_survivors_plus_a_quarter_and_only_when_opted_in() {
    assert_eq!(uncommit_keep_for_survivors(8 * MIB, false), 0);
    assert_eq!(uncommit_keep_for_survivors(8 * MIB, true), 10 * MIB);
    assert_eq!(uncommit_keep_for_survivors(0, true), 0);
    assert_eq!(
        uncommit_keep_for_survivors(usize::MAX, true),
        usize::MAX,
        "saturates"
    );
}

/// The give-back keeps `keep` bytes committed above a zero floor, and
/// `keep = 0` releases exactly what the plain give-back releases.
#[test]
fn e1y_the_evacuated_semi_space_keeps_the_survivor_prefix_committed() {
    if Arena::new(16 * MIB).commit_bits().is_none() {
        // The wholly committed fallback store releases nothing.
        return;
    }
    cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_GEN_UNCOMMIT", None)], || {
        let heap = GenerationalHeap::with_capacity(64 * MIB);
        let g = cratonvm_gc_core::reservation::GRANULE;

        let mut plain = Arena::new(16 * MIB);
        let _ = plain.commit_initial_prefix(12 * MIB);
        let released_plain = heap.uncommit_evacuated_young(&mut plain);

        let mut zero_keep = Arena::new(16 * MIB);
        let _ = zero_keep.commit_initial_prefix(12 * MIB);
        let released_zero = heap.uncommit_evacuated_young_keeping(&mut zero_keep, 0);
        assert_eq!(
            released_plain, released_zero,
            "keep 0 is the plain give-back"
        );
        assert_eq!(plain.committed_bytes(), zero_keep.committed_bytes());

        let mut kept = Arena::new(16 * MIB);
        let _ = kept.commit_initial_prefix(12 * MIB);
        let keep = 4 * g;
        let _ = heap.uncommit_evacuated_young_keeping(&mut kept, keep);
        assert!(
            kept.committed_bytes() >= keep,
            "the survivor prefix stays committed: {} < {keep}",
            kept.committed_bytes()
        );
        assert!(
            kept.committed_bytes() < 12 * MIB,
            "the rest still goes back"
        );

        // A keep past the capacity is clamped, never an error.
        let mut huge = Arena::new(4 * MIB);
        let _ = huge.commit_initial_prefix(2 * MIB);
        let _ = heap.uncommit_evacuated_young_keeping(&mut huge, usize::MAX);
        assert!(huge.committed_bytes() <= 4 * MIB);
    });
}

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

// The non-moving sweep tells the arena which bytes it has just zeroed
// and listed: a dead run between two survivors is claimed known-zero,
// reads zero, and a hand-out inside it would be served without a second
// memset once the refill consumer is on. The survivors are not claimed.
