use super::{initial_java_home, vm_property_list, SharedVm};
use crate::config::VmConfig;

#[test]
fn the_vm_property_list_is_the_tables_and_reaches_sys_props() {
    let config = VmConfig::default();
    let java_home = initial_java_home(&config);
    let list = vm_property_list(&config, &java_home);
    let keys: Vec<&str> = list.iter().map(|(k, _)| *k).collect();
    // Wave 136 (lane L1b): `java.vm.compressedOopsMode` is in the list
    // only when HotSpot would set it (absent under ZGC, the default
    // build's collector).
    let has_mode = super::vm_compressed_oops_mode(&config).is_some();
    let table: Vec<&str> = cratonvm_types::vm_properties::vm_property_keys()
        .filter(|k| has_mode || *k != "java.vm.compressedOopsMode")
        .collect();
    assert_eq!(keys, table);
    for (key, value) in &list {
        // zt t27/lg1 (`CRATONVM_VM_IDENTITY_HOTSPOT`): the vendor and
        // version are the image's own when its `release` file says them.
        let from_release = match *key {
            "java.vm.vendor" => super::jdk_image_release_value(&java_home, "IMPLEMENTOR"),
            "java.vm.version" => {
                super::jdk_image_release_value(&java_home, "JAVA_RUNTIME_VERSION")
                    .or_else(|| super::jdk_image_release_value(&java_home, "JAVA_VERSION"))
            }
            _ => None,
        };
        if let Some(release) = from_release {
            if cratonvm_types::vm_properties::vm_identity_hotspot_enabled() {
                assert_eq!(value, &release, "{key}");
                continue;
            }
        }
        if let Some(fixed) = cratonvm_types::vm_properties::vm_identity_value(key) {
            assert_eq!(value, fixed, "{key}");
        }
    }
    let shared = SharedVm::new(VmConfig::default());
    let props = shared.system_properties.read();
    for (key, value) in &list {
        assert_eq!(props.get(*key), Some(value), "{key}");
    }
}
