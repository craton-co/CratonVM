use super::*;

/// Resolve the same two backend inputs as the VM's processor constructor,
/// including ZGC's separate default and the explicit overrides.
#[test]
fn gci_i5g_g1o_g1_soft_lru_takes_hotspots_inputs_by_default() {
    assert!(soft_lru_takes_hotspot_inputs_by_default(GcBackend::Generational));
    assert!(soft_lru_takes_hotspot_inputs_by_default(GcBackend::G1));
    let lru = |v: Option<&str>, backend: GcBackend| {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SOFTREF_HOTSPOT_LRU", v)],
            || {
                cratonvm_types::flags()
                    .gc
                    .softref_hotspot_lru_for_backend(
                        soft_lru_takes_hotspot_inputs_by_default(backend),
                        backend == GcBackend::Zgc,
                    )                },
        )
    };
    assert!(lru(None, GcBackend::G1), "unset, G1: HotSpot's inputs");
    assert!(!lru(Some("0"), GcBackend::G1), "`=0` kills it on G1");
    assert!(lru(None, GcBackend::Generational));
    assert!(!lru(Some("0"), GcBackend::Generational));
    assert!(lru(None, GcBackend::Zgc), "unset, ZGC: HotSpot's inputs");
    assert!(!lru(Some("0"), GcBackend::Zgc), "`=0` kills it on ZGC");
    for backend in [GcBackend::Generational, GcBackend::G1, GcBackend::Zgc] {
        assert!(lru(Some("1"), backend));
    }
}
