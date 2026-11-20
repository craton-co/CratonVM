use super::register_linked_hashmap_natives;
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

const LHM: &str = "java/util/LinkedHashMap";
const REVERSED: &str = "()Ljava/util/SequencedMap;";

/// The `jdk-hash-collections` row off, as in `lhs_reversed_registration_tests`.
fn registry(real_jdk: bool, mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_hotspot_alignments(
        cratonvm_types::HotSpotAlignments::defaults()
            .with(cratonvm_types::HotSpotAlignment::JdkHashCollections, false),
    );
    r.set_real_jdk(real_jdk);
    register_linked_hashmap_natives(&mut r);
    r
}

fn switched_on() -> bool {
    true
        && !cratonvm_native_api::unretire::is_excluded(LHM, "reversed", REVERSED)    }

/// The snapshot is not registered, so the real view runs; the members it
/// delegates to, and the view carriers' own `reversed()`, stay native.
#[test]
fn compatible_on_a_real_jdk_leaves_reversed_to_the_jdk_view() {
    if !switched_on() {
        return; // switched off in this environment
    }
    let r = registry(true, CompatibilityMode::Compatible);
    assert!(r.find(LHM, "reversed", REVERSED).is_none());
    for (c, m, d) in [
        (LHM, "sequencedKeySet", "()Ljava/util/SequencedSet;"),
        (LHM, "sequencedEntrySet", "()Ljava/util/SequencedSet;"),
        (
            LHM,
            "putFirst",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (LHM, "pollLastEntry", "()Ljava/util/Map$Entry;"),
        (
            "java/util/LinkedHashMap$LinkedKeySet",
            "reversed",
            "()Ljava/util/SequencedSet;",
        ),
        (
            "java/util/LinkedHashMap$LinkedEntrySet",
            "reversed",
            "()Ljava/util/SequencedSet;",
        ),
    ] {
        assert!(r.find(c, m, d).is_some(), "{c}.{m}{d} is missing");
    }
}

/// The synthetic JDK has no bytecode for the view and keeps the snapshot.
#[test]
fn the_synthetic_jdk_keeps_the_reversed_map_snapshot() {
    let r = registry(false, CompatibilityMode::Compatible);
    assert!(r.find(LHM, "reversed", REVERSED).is_some());
}
