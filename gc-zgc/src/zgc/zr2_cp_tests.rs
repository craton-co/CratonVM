use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

#[test]
fn the_pressure_tier_follows_the_mode_the_latch_and_the_tail() {
    let tight = ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE - 1;
    let roomy = ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE;
    // Kill switch: never, whatever the heap says.
    assert_eq!(relocate_pressure_tier(0, true, tight), None);
    // Default: only after a refusal, and the tail does not matter.
    assert_eq!(
        relocate_pressure_tier(1, true, roomy),
        Some(RelocatePressure::AllocFailure)
    );
    assert_eq!(relocate_pressure_tier(1, false, tight), None);
    // Opt-in: a tight heap widens too, a roomy one does not.
    assert_eq!(
        relocate_pressure_tier(2, false, tight),
        Some(RelocatePressure::Tight)
    );
    assert_eq!(relocate_pressure_tier(2, false, roomy), None);
    assert_eq!(
        relocate_pressure_tier(2, true, tight),
        Some(RelocatePressure::AllocFailure)
    );
    // Both widened cutoffs admit pages the default refuses.
    for tier in [RelocatePressure::AllocFailure, RelocatePressure::Tight] {
        assert!(
            relocate_pressure_occupancy(tier) > forwarding::ZFWD_DEFAULT_MAX_LIVE_OCCUPANCY
        );
        assert!(relocate_pressure_occupancy(tier) < 1.0);        }    }


#[derive(Debug)]
struct TestOom(usize, &'static str);

fn test_hook(length: usize, what: &'static str) {
    std::panic::panic_any(TestOom(length, what));    }