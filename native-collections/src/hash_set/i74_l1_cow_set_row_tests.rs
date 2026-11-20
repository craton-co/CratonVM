use super::*;
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};

const COWAS: &str = "java/util/concurrent/CopyOnWriteArraySet";

fn rows_on(class: &str, real_jdk: bool, on: bool) -> usize {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(CompatibilityMode::Compatible);
    // From NONE, not the defaults: `JdkStreams` (default on) drops the
    // `stream()` row of every `SET_CLASSES` name by itself.
    r.set_hotspot_alignments(HotSpotAlignments::NONE.with(Row::JdkCopyOnWriteArraySet, on));
    r.set_real_jdk(real_jdk);
    register_hashset_natives(&mut r);
    r.dump_registrations()
        .into_iter()
        .filter(|(c, _, _, _)| *c == class)
        .count()
}

#[test]
fn the_set_surface_follows_the_copy_on_write_set_row() {
    if cratonvm_native_api::unretire::is_excluded(COWAS, "isEmpty", "()Z") {
        return; // a retired row was un-retired in this environment
    }
    assert_eq!(rows_on(COWAS, true, false), 24);
    assert_eq!(rows_on(COWAS, true, true), 0);
    // The synthetic library: the row needs a real JDK.
    assert_eq!(rows_on(COWAS, false, true), 24);
    for c in ["java/util/HashSet", "java/util/LinkedHashSet"] {
        assert_eq!(rows_on(c, true, true), rows_on(c, true, false), "{c}");
    }
}
