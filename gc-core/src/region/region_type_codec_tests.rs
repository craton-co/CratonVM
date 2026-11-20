use super::RegionType;

/// W3-A — the one invariant `G1Region`'s `AtomicU8` representation rests on:
/// every variant `to_u8` can produce, `from_u8` returns.
///
/// A variant added without a `from_u8` arm fails to compile at
/// `every_variant_decodes`; a variant given the WRONG arm (a copy-paste of
/// its neighbour's discriminant) compiles fine and is what this catches.
#[test]
fn region_type_round_trip() {
    assert_eq!(
        RegionType::ALL.len(),
        6,
        "a variant was added to RegionType without being added to ALL, so the round-trip below no longer covers the whole enum"
    );
    for t in RegionType::ALL {
        assert!(t.every_variant_decodes());
        assert_eq!(
            RegionType::from_u8(t.to_u8()),
            t,
            "{t:?} does not survive the byte encoding G1Region stores it in"
        );
    }
    // Distinct discriminants — two variants sharing a byte would round-trip
    // individually and still make the collector unable to tell them apart.
    for (i, a) in RegionType::ALL.iter().enumerate() {
        for b in RegionType::ALL.iter().skip(i + 1) {
            assert_ne!(a.to_u8(), b.to_u8(), "{a:?} and {b:?} share a byte");
        }
    }
    // Everything outside the discriminants decodes to the fail-safe.
    for raw in 6u8..=255 {
        assert_eq!(RegionType::from_u8(raw), RegionType::Old);
    }
}
