use super::resolved_flags;
use crate::config::{GcAlgorithm, VmConfig};

fn row(config: &VmConfig, name: &str) -> Option<(String, &'static str)> {
    resolved_flags(config)
        .into_iter()
        .find(|r| r.name == name)
        .map(|r| (r.value, r.origin))
}

/// Interpreter round i7 wave 136, lane L1b: `UseCompressedOops` and
/// `ObjectAlignmentInBytes` as HotSpot 25.0.3's `PrintFlagsFinal` measured
/// them (`L1bW136DiagnosticVmOptions`), and no rows with the parity switch
/// off.
#[test]
fn the_compressed_oops_rows_follow_hotspots_ergonomics() {
    let mut config = VmConfig::default();
    config.vm_compressed_oops_mode_hotspot = true;
    config.gc_algorithm = GcAlgorithm::Generational;
    config.max_heap_size = 256 << 20;
    config.jvm_args = vec!["-XX:+UseGenerationalGC".to_string(), "-Xmx256m".to_string()];
    assert_eq!(row(&config, "UseCompressedOops"), Some(("true".to_string(), "{ergonomic}")));
    assert_eq!(row(&config, "ObjectAlignmentInBytes"), Some(("8".to_string(), "{default}")));
    config.jvm_args.push("-XX:-UseCompressedOops".to_string());
    assert_eq!(
        row(&config, "UseCompressedOops"),
        Some(("false".to_string(), "{command line}"))
    );
    config.jvm_args.pop();
    if !cratonvm_types::narrow_oop::narrow_oops_enabled() {
        config.max_heap_size = 40 << 30;
        assert_eq!(
            row(&config, "UseCompressedOops"),
            Some(("false".to_string(), "{default}"))
        );
    }
    config.vm_compressed_oops_mode_hotspot = false;
    assert_eq!(row(&config, "UseCompressedOops"), None);
    assert_eq!(row(&config, "ObjectAlignmentInBytes"), None);
}
