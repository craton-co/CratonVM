use super::register_hashset_natives;
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

const LHS: &str = "java/util/LinkedHashSet";
const REVERSED: &str = "()Ljava/util/SequencedSet;";

/// The `jdk-hash-collections` row off: these tests are about the one row
/// `--compatible` left to bytecode before that row adopted the family.
fn registry(real_jdk: bool) -> NativeMethodRegistry {
    registry_with_row(real_jdk, false)
}

fn registry_with_row(real_jdk: bool, row: bool) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(CompatibilityMode::Compatible);
    r.set_hotspot_alignments(
        cratonvm_types::HotSpotAlignments::defaults()
            .with(cratonvm_types::HotSpotAlignment::JdkHashCollections, row),
    );
    r.set_real_jdk(real_jdk);
    register_hashset_natives(&mut r);
    r
}

/// hcb: with the `jdk-hash-collections` row the source members go too
/// (the whole family runs the JDK's bodies, as under `--real-jdk`).
#[test]
fn the_hash_collection_row_leaves_the_members_to_the_jdk_too() {
    if cratonvm_native_api::unretire::is_excluded(LHS, "addFirst", "(Ljava/lang/Object;)V") {
        return; // switched off in this environment
    }
    let r = registry_with_row(true, true);
    assert!(r.find(LHS, "reversed", REVERSED).is_none());
    assert!(r.find(LHS, "addFirst", "(Ljava/lang/Object;)V").is_none());
    // Without a real JDK the row cannot apply.
    let r = registry_with_row(false, true);
    assert!(r.find(LHS, "reversed", REVERSED).is_some());
}

/// The snapshot is not registered, so the real `ReverseLinkedHashSetView`
/// runs; the source natives it delegates to stay registered.
#[test]
fn compatible_on_a_real_jdk_leaves_reversed_to_the_jdk_view() {
    if !true
        || cratonvm_native_api::unretire::is_excluded(LHS, "reversed", REVERSED)
    {
        return; // switched off in this environment
    }
    let r = registry(true);
    assert!(r.find(LHS, "reversed", REVERSED).is_none());
    for (m, d) in [
        ("addFirst", "(Ljava/lang/Object;)V"),
        ("getLast", "()Ljava/lang/Object;"),
        ("removeFirst", "()Ljava/lang/Object;"),
        ("size", "()I"),
    ] {
        assert!(r.find(LHS, m, d).is_some(), "{m}{d} is missing");
    }
}

/// The synthetic JDK has no bytecode for it and keeps the snapshot.
#[test]
fn the_synthetic_jdk_keeps_the_reversed_snapshot() {
    let r = registry(false);
    assert!(r.find(LHS, "reversed", REVERSED).is_some());
}
