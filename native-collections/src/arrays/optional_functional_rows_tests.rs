use super::{register_optional_natives, OPTIONAL_FUNCTIONAL_ROWS};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

const OPT: &str = "java/util/Optional";

/// `row`: the `jdk-optional` HotSpot-alignment row (nio20 wave 1, default
/// on), which drops every retired `Optional` row on a real JDK.
fn registry_with_row(real_jdk: bool, row: bool) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(CompatibilityMode::Compatible);
    r.set_hotspot_alignments(
        cratonvm_types::HotSpotAlignments::defaults()
            .with(cratonvm_types::HotSpotAlignment::JdkOptional, row),
    );
    r.set_real_jdk(real_jdk);
    register_optional_natives(&mut r);
    r
}

/// `--compatible` on a real JDK with `-jdk-optional`: the eight callback
/// rows run the class's bytecode (so `Optional.map` keeps its frame); the
/// leaf rows stay.
#[test]
fn compatible_on_a_real_jdk_leaves_the_callback_rows_to_bytecode() {
    let r = registry_with_row(true, false);
    for (m, d) in OPTIONAL_FUNCTIONAL_ROWS {
        if cratonvm_native_api::unretire::is_excluded(OPT, m, d) {
            continue;
        }
        assert!(r.find(OPT, m, d).is_none(), "{m}{d} is still registered");
    }
    assert!(r.find(OPT, "isPresent", "()Z").is_some());
    assert!(r
        .find(
            OPT,
            "ofNullable",
            "(Ljava/lang/Object;)Ljava/util/Optional;"
        )
        .is_some());
}

/// The default `jdk-optional` row (nio20 wave 1): the whole family is the
/// JDK's, leaf rows included (both are retired rows).
#[test]
fn the_jdk_optional_row_leaves_the_leaf_rows_to_bytecode_too() {
    for (m, d) in [
        ("isPresent", "()Z"),
        ("ofNullable", "(Ljava/lang/Object;)Ljava/util/Optional;"),
    ] {
        if cratonvm_native_api::unretire::is_excluded(OPT, m, d) {
            continue;
        }
        assert!(
            registry_with_row(true, true).find(OPT, m, d).is_none(),
            "{m}{d}"
        );
        // Without a real JDK the row cannot apply.
        assert!(
            registry_with_row(false, true).find(OPT, m, d).is_some(),
            "{m}{d}"
        );
    }
}

/// The synthetic JDK has no bytecode for them and keeps all eight, row or
/// no row.
#[test]
fn the_synthetic_jdk_keeps_the_callback_rows() {
    for row in [false, true] {
        let r = registry_with_row(false, row);
        for (m, d) in OPTIONAL_FUNCTIONAL_ROWS {
            assert!(r.find(OPT, m, d).is_some(), "{m}{d} is missing");
        }
    }
}
