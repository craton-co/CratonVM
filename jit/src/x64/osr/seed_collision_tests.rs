use super::*;

/// The shape `mixedInsertionSort` actually had: slot 7 is `long ai`'s dead
/// high half in one region and the live `int i` counter in another, so it
/// keeps a register — and if the mask does not cover it at a PC where it is
/// the dead half, its seed lands on top of the live local sharing that home.
#[test]
fn an_unmasked_shared_home_is_reported_with_both_local_indices() {
    // locals 5 and 7 both homed in r12; nothing masked.
    let gpr = vec![None, None, None, None, None, Some(12u8), None, Some(12u8)];
    let found = seed_collisions_at(0, 8, &gpr, &[]);
    assert_eq!(
        found,
        vec![SeedCollision {
            file: "GPR",
            reg: "r12".into(),
            first: 5,
            second: 7
        }],
        "the detector must NAME both locals, not just count"
    );
}

/// The same metadata with the mask doing its job is silent. This is the
/// arm that makes a field zero meaningful.
#[test]
fn masking_the_dead_one_silences_it() {
    let gpr = vec![None, None, None, None, None, Some(12u8), None, Some(12u8)];
    assert!(seed_collisions_at(1 << 7, 8, &gpr, &[]).is_empty());
    assert!(seed_collisions_at(1 << 5, 8, &gpr, &[]).is_empty());
}

/// GPR r9 and XMM x9 are different register files and must never be
/// reported as sharing a home.
#[test]
fn gpr_and_xmm_with_the_same_number_do_not_alias() {
    let gpr = vec![Some(9u8), None];
    let xmm = vec![None, Some(9u8)];
    assert!(seed_collisions_at(0, 2, &gpr, &xmm).is_empty());
}

/// Two XMM locals sharing a home is the ES-tdigest shape.
#[test]
fn xmm_sharing_is_caught_too() {
    let xmm = vec![Some(8u8), Some(8u8)];
    let found = seed_collisions_at(0, 2, &[], &xmm);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].file, "XMM");
    assert_eq!((found[0].first, found[0].second), (0, 1));
}

/// Three locals on one register report two overwrites, each naming the
/// value that was actually lost — not one aggregate "r12 is crowded".
#[test]
fn each_overwrite_names_the_value_it_destroyed() {
    let gpr = vec![Some(13u8), Some(13u8), Some(13u8)];
    let found = seed_collisions_at(0, 3, &gpr, &[]);
    assert_eq!(
        found
            .iter()
            .map(|c| (c.first, c.second))
            .collect::<Vec<_>>(),
        vec![(0, 1), (1, 2)]
    );
}

/// Locals at index >= 64 never get a register home (`color_graph` caps at
/// 64), and the mask is a u64, so the scan must stop there rather than
/// index past the bitset.
#[test]
fn the_scan_respects_the_64_local_cap() {
    let mut gpr = vec![None; 70];
    gpr[64] = Some(12u8);
    gpr[65] = Some(12u8);
    assert!(seed_collisions_at(0, 70, &gpr, &[]).is_empty());
}
