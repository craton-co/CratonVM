use super::*;

#[test]
fn i139_l1b_vm_info_feature_list_is_this_vms() {
    let props = vec![("java.vm.compressedOopsMode".to_string(), "Zero based".to_string())];
    let g1 = vec!["-XX:MaxHeapSize=1".to_string(), "-XX:+UseG1GC".to_string()];
    assert_eq!(
        vm_info_feature_list("mixed mode", &props, &g1),
        "mixed mode, tiered, compressed oops, compressed class ptrs, g1 gc"
    );
    let serial = vec!["-XX:+UseSerialGC".to_string()];
    assert_eq!(
        vm_info_feature_list("interpreted mode, jdk-only", &[], &serial),
        "interpreted mode, jdk-only, serial gc"
    );
    let summary = HeapSummary {
        young_gen_used: 1 << 20,
        young_gen_capacity: 4 << 20,
        old_gen_used: 1 << 20,
        old_gen_capacity: 8 << 20,
        metaspace_used: 1 << 20,
        metaspace_capacity: 2 << 20,
        total_used: 2 << 20,
        total_capacity: 12 << 20,
    };
    let text = default_vm_info_text(&props, "Main", 1.5, None, &summary, &g1);
    assert!(text.contains("\n# JRE version: "), "{text}");
    assert!(text.contains("\n CPUs: "), "{text}");
    assert!(!text.contains("Polling page: 0x0000000000000000"), "{text}");
    assert!(!text.contains("GC Heap History (0 events)"), "{text}");
}
