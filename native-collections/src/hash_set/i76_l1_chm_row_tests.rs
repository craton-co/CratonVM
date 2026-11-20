use super::*;
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};

const CM: &str = "java/util/concurrent/ConcurrentMap";

/// `(registrations on a CHM class, registrations on ConcurrentMap)`.
fn counts(mode: CompatibilityMode, real_jdk: bool, on: bool) -> (usize, usize) {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_hotspot_alignments(HotSpotAlignments::NONE.with(Row::JdkConcurrentHashMap, on));
    r.set_real_jdk(real_jdk);
    register_concurrent_hashmap_natives(&mut r);
    register_map_view_carrier_natives(&mut r);
    register_set_view_carrier_natives(&mut r);
    let regs = r.dump_registrations();
    let chm = regs
        .iter()
        .filter(|(c, _, _, _)| c.starts_with(CHM_CLASS))
        .count();
    let cm = regs.iter().filter(|(c, _, _, _)| *c == CM).count();
    (chm, cm)
}

#[test]
fn the_concurrent_hash_map_cluster_follows_the_row() {
    let compat = CompatibilityMode::Compatible;
    let (off_chm, off_cm) = counts(compat, true, false);
    assert!(off_chm > 0, "no CHM registration without the row");
    assert_eq!(off_cm, 5);
    let (on_chm, on_cm) = counts(compat, true, true);
    assert_eq!(on_chm, 0, "a CHM registration survives the row");
    assert_eq!(on_cm, 5, "the ConcurrentMap interface rows stay");
    // The synthetic library: the row needs a real JDK.
    assert_eq!(counts(compat, false, true), counts(compat, false, false));
    // `--real-jdk`: its registrations are the retired-shadow tables'.
    assert_eq!(
        counts(CompatibilityMode::JdkOnly, true, true),
        counts(CompatibilityMode::JdkOnly, true, false)
    );
}

/// A `ConcurrentMap` door with the row in force but a receiver that is not a
/// CHM (the mock classifies nothing as one) keeps the segmented native; the
/// mock context answers `aligned` with `is_jdk_only` (false), so the door
/// helper declines whatever the receiver.
#[test]
fn the_door_helper_declines_without_the_row() {
    let mut mock = cratonvm_native_api::test_mock::MockNativeContext::new();
    let this = mock.fresh_object_ref();
    let ctx: &mut dyn NativeContext = &mut mock;
    assert!(!chm_jdk_bodies(ctx));
    assert!(chm_door_runs_jdk_body(ctx, &[Value::Object(Some(this))], "size", "()I").is_none());
    assert!(chm_jdk_table_entries(ctx, this).is_empty());
}
