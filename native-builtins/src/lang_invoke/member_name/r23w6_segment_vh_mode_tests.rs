use super::*;

#[test]
fn mode_bodies_follow_the_jdk_25_form_classes() {
    let h = "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J";
    assert_eq!(segment_vh_mode_kind("getAcquire"), Some(SegModeKind::Get));
    assert_eq!(segment_vh_mode_kind("setRelease"), Some(SegModeKind::Set));
    assert_eq!(segment_vh_mode_kind("weakCompareAndSetPlain"), Some(SegModeKind::Cas));
    assert_eq!(segment_vh_mode_kind("compareAndExchangeAcquire"), Some(SegModeKind::Cae));
    assert_eq!(segment_vh_mode_kind("getAndSetRelease"), Some(SegModeKind::GetAndSet));
    assert_eq!(segment_vh_mode_kind("getAndBitwiseXorAcquire"), Some(SegModeKind::GetAndArith));
    assert_eq!(segment_vh_mode_kind("toMethodHandle"), None);
    assert_eq!(
        segment_vh_mode_descriptor(SegModeKind::Get, "I").as_deref(),
        Some(&*format!("{h})I"))
    );
    assert_eq!(
        segment_vh_mode_descriptor(SegModeKind::Cas, "J").as_deref(),
        Some(&*format!("{h}JJ)Z"))
    );
    assert_eq!(
        segment_vh_mode_descriptor(SegModeKind::Cae, "I").as_deref(),
        Some(&*format!("{h}II)I"))
    );
    assert_eq!(
        segment_vh_mode_descriptor(SegModeKind::GetAndSet, "D").as_deref(),
        Some(&*format!("{h}D)D"))
    );
    assert_eq!(segment_vh_mode_descriptor(SegModeKind::GetAndArith, "F"), None);
    // j27 wave 1 (lane FFMVH): the short / char aligned forms declare
    // the ordered modes only -- no CAS family body to raise through.
    for carrier in ["S", "C", "B", "Z"] {
        for kind in [SegModeKind::Cas, SegModeKind::Cae, SegModeKind::GetAndSet, SegModeKind::GetAndArith] {
            assert_eq!(segment_vh_mode_descriptor(kind, carrier), None, "{kind:?} {carrier}");
        }
        assert!(segment_vh_mode_descriptor(SegModeKind::Get, carrier).is_some());
        assert!(segment_vh_mode_descriptor(SegModeKind::Set, carrier).is_some());
    }
    assert_eq!(
        segment_vh_mode_descriptor(SegModeKind::GetAndSet, "F").as_deref(),
        Some(&*format!("{h}F)F"))
    );
    assert_eq!(segment_vh_mode_values(SegModeKind::Cae), 2);
    assert_eq!(segment_vh_mode_values(SegModeKind::Get), 0);
}

/// j27 wave 1 (lane FFMVH): the raise's re-entrancy flag is held for
/// exactly one raise and cleared on drop.
#[test]
fn the_raise_in_flight_flag_is_scoped() {
    assert!(!SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()));
    {
        let _held = SegVhRaiseInFlight::enter();
        assert!(SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()));
    }
    assert!(!SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()));
}
