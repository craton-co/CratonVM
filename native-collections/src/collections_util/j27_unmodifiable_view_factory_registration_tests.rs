use super::register_collections_utility_natives;
use crate::register_collections_extras_natives;
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::{HotSpotAlignment, HotSpotAlignments};

const COLLECTIONS: &str = "java/util/Collections";

/// Every `Collections.unmodifiable*` factory the two native-collections
/// registrars install (the `unmodifiableList` one is `collections_util`'s,
/// the other five `register_collections_extras_natives`').
const FACTORIES: [(&str, &str); 6] = [
    ("unmodifiableList", "(Ljava/util/List;)Ljava/util/List;"),
    ("unmodifiableCollection", "(Ljava/util/Collection;)Ljava/util/Collection;"),
    ("unmodifiableSet", "(Ljava/util/Set;)Ljava/util/Set;"),
    ("unmodifiableSortedSet", "(Ljava/util/SortedSet;)Ljava/util/SortedSet;"),
    (
        "unmodifiableNavigableSet",
        "(Ljava/util/NavigableSet;)Ljava/util/NavigableSet;",
    ),
    ("unmodifiableMap", "(Ljava/util/Map;)Ljava/util/Map;"),
];

fn registry(mode: CompatibilityMode, real_jdk: bool, row: bool) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_hotspot_alignments(
        HotSpotAlignments::defaults().with(HotSpotAlignment::JdkUnmodifiableViews, row),
    );
    r.set_real_jdk(real_jdk);
    register_collections_utility_natives(&mut r);
    register_collections_extras_natives(&mut r);
    r
}

/// The factories that would hand out a `cratonvm/internal/Unmodifiable*`.
fn stand_in_factories(r: &NativeMethodRegistry) -> Vec<&'static str> {
    FACTORIES
        .iter()
        .filter(|(m, d)| r.find(COLLECTIONS, m, d).is_some())
        .map(|(m, _)| *m)
        .collect()
}

fn all_factories() -> Vec<&'static str> {
    FACTORIES.iter().map(|(m, _)| *m).collect()
}

#[test]
fn compatible_over_a_real_jdk_runs_every_factory_as_jdk_bytecode() {
    let r = registry(CompatibilityMode::Compatible, true, true);
    assert_eq!(stand_in_factories(&r), Vec::<&str>::new());
}

#[test]
fn jdk_only_runs_every_factory_as_jdk_bytecode() {
    let r = registry(CompatibilityMode::JdkOnly, true, true);
    assert_eq!(stand_in_factories(&r), Vec::<&str>::new());
}

/// The row's revert (`CRATONVM_COMPAT_ALIGN=-jdk-unmodifiable-views`) puts
/// every stand-in back: the snapshot list iterator is reachable again.
#[test]
fn the_row_off_puts_the_stand_in_factories_back() {
    let r = registry(CompatibilityMode::Compatible, true, false);
    assert_eq!(stand_in_factories(&r), all_factories());
}

/// No real JDK: there is no bytecode to defer to, so the stand-ins stay.
#[test]
fn an_image_without_the_jdk_keeps_the_stand_in_factories() {
    let r = registry(CompatibilityMode::Compatible, false, true);
    assert_eq!(stand_in_factories(&r), all_factories());
}
