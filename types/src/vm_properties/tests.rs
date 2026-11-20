use super::*;

/// The readable keys are HotSpot 25.0.3's measured live-phase set
/// (`L1W61JvmtiAgentStartup`'s `props` row, less the launcher's `-D`
/// properties), in its order.
#[test]
fn the_readable_keys_are_hotspots_measured_set() {
    let keys: Vec<&str> = vm_property_keys().collect();
    assert_eq!(
        keys,
        [
            "java.vm.specification.name",
            "java.vm.specification.vendor",
            "java.vm.specification.version",
            "java.vm.name",
            "java.vm.vendor",
            "java.vm.version",
            "java.vm.info",
            "jdk.debug",
            "java.home",
            "java.class.path",
            "java.library.path",
            "sun.boot.library.path",
            "java.vm.compressedOopsMode",
        ]
    );
    assert!(keys.iter().all(|k| !is_internal_property(k)));
}

/// Wave 118: every identity key is a readable list key, in the list's
/// order, and none of them is writeable or launch-dependent.
#[test]
fn the_identity_values_cover_the_fixed_list_keys() {
    let keys: Vec<&str> = VM_IDENTITY.iter().map(|(k, _)| *k).collect();
    let fixed: Vec<&str> = vm_property_keys()
        .filter(|k| !is_writeable_vm_property(k) && *k != "java.vm.compressedOopsMode")
        .collect();
    assert_eq!(keys, fixed);
    // zt t27/lg1 (`CRATONVM_VM_IDENTITY_HOTSPOT`): HotSpot's name by
    // default, the old one on the `=0` kill.
    let key = concat!("CRATON", "VM_VM_IDENTITY_HOTSPOT");
    crate::flags::with_thread_overrides(&[(key, None)], || {
        assert_eq!(vm_identity_value("java.vm.name"), Some(HOTSPOT_VM_NAME));
        assert_eq!(vm_identity_value("java.vm.vendor"), Some("cratonvm"));
    });
    assert_eq!(vm_identity_value("java.vm.info"), None, "the launch's");
}

/// Wave 135 (lane L1b): HotSpot's order holds the same thirteen keys,
/// and the sort key puts a `-D` key between the before-heap list and
/// `java.vm.compressedOopsMode`, as the measured `vmProperties()` does.
#[test]
fn i135_l1b_hotspot_order_is_a_permutation_and_sorts_as_measured() {
    let mut ordered: Vec<&str> = HOTSPOT_LIST_ORDER.to_vec();
    let mut table: Vec<&str> = vm_property_keys().collect();
    ordered.sort_unstable();
    table.sort_unstable();
    assert_eq!(ordered, table);
    let mut keys = vec![
        "java.vm.compressedOopsMode",
        "zz.user",
        "java.home",
        "java.vm.specification.name",
        "sun.java.launcher",
        "java.vm.vendor",
    ];
    keys.sort_by_key(|k| hotspot_list_sort_key(k));
    assert_eq!(
        keys,
        [
            "java.vm.specification.name",
            "java.home",
            "java.vm.vendor",
            "zz.user",
            "sun.java.launcher",
            "java.vm.compressedOopsMode",
        ]
    );
}

/// Wave 136 (lane L1b): the measured HotSpot 25.0.3 table of
/// `java.vm.compressedOopsMode` (`L1bW136CompressedOopsModeProperty`).
#[test]
fn i136_l1b_compressed_oops_mode_matches_the_measured_table() {
    use CompressedOopsCollector::{Serial, Z, G1};
    const M: u64 = 1 << 20;
    let mode = |mib: u64, c| hotspot_compressed_oops_mode(mib * M, c, true);
    assert_eq!(mode(64, G1), Some("32-bit"));
    assert_eq!(mode(2048, G1), Some("32-bit"));
    assert_eq!(mode(2048, Serial), Some("32-bit"));
    assert_eq!(mode(2050, Serial), Some("Zero based"), "-Xmx2049m aligned to 2 MiB");
    assert_eq!(mode(4096, G1), Some("Zero based"));
    assert_eq!(mode(30 * 1024, G1), Some("Zero based"));
    assert_eq!(mode(30 * 1024, Serial), Some("Zero based"));
    assert_eq!(mode(30 * 1024 + 1, G1), Some("Non-zero disjoint base"));
    assert_eq!(mode(31 * 1024, G1), Some("Non-zero disjoint base"));
    assert_eq!(mode(32736, G1), Some("Non-zero disjoint base"));
    assert_eq!(mode(32752, G1), None, "-Xmx32737m");
    assert_eq!(mode(32766, Serial), Some("Non-zero disjoint base"));
    assert_eq!(mode(32768, Serial), None, "-Xmx32767m");
    assert_eq!(mode(40 * 1024, G1), None);
    assert_eq!(mode(4096, Z), None);
    assert_eq!(hotspot_compressed_oops_mode(4096 * M, G1, false), None);
    assert_eq!(hotspot_narrow_oop_mode_name(0, 0), "32-bit");
    assert_eq!(hotspot_narrow_oop_mode_name(0, 3), "Zero based");
    assert_eq!(hotspot_narrow_oop_mode_name(1 << 35, 3), "Non-zero disjoint base");
    assert_eq!(hotspot_narrow_oop_mode_name((1 << 35) + 4096, 3), "Non-zero based");
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(use_compressed_oops_from_jvm_args(&args(&[])));
    assert!(!use_compressed_oops_from_jvm_args(&args(&["-XX:-UseCompressedOops"])));
    assert!(use_compressed_oops_from_jvm_args(&args(&[
        "-XX:-UseCompressedOops",
        "-Xmx1g",
        "-XX:+UseCompressedOops"
    ])));
}

#[test]
fn the_writeable_keys_are_the_paths_and_the_vm_info() {
    let writeable: Vec<&str> = VM_PROPERTIES
        .iter()
        .filter(|p| p.writeable)
        .map(|p| p.key)
        .collect();
    assert_eq!(
        writeable,
        [
            "java.vm.info",
            "java.home",
            "java.class.path",
            "java.library.path",
            "sun.boot.library.path",
        ]
    );
    assert!(!is_writeable_vm_property("java.vm.version"));
    assert!(is_internal_property("jdk.module.path"));
    assert!(!is_internal_property("jdk.debug"));
}
