use super::*;
use cratonvm_native_builtins::panama::IllegalNativeAccess;

/// zt t12/df4: `--illegal-native-access=deny|allow` reaches the JDK as
/// `jdk.module.illegal.native.access` (as the `java` launcher passes it);
/// `warn` (the JDK's default) and the `=0` kill pass nothing.
#[test]
fn zt12df4_the_launcher_mode_reaches_the_jdk_property() {
    let on: Vec<_> = [
        IllegalNativeAccess::Allow,
        IllegalNativeAccess::Warn,
        IllegalNativeAccess::Deny,
    ]
    .into_iter()
    .map(|mode| illegal_native_access_property_for(mode, true))
    .collect();
    assert_eq!(on, vec![Some("allow"), None, Some("deny")]);
    assert_eq!(
        illegal_native_access_property_for(IllegalNativeAccess::Deny, false),
        None
    );
    let src = concat!(include_str!("../vm_init/mod.rs"), "\n", include_str!("../vm_init/missing_native_entry.rs"), "\n", include_str!("../vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../vm_init/wait_site.rs"), "\n", include_str!("../vm_init/class_store_cut_with.rs"));
    let insert = src
        .find("\"jdk.module.illegal.native.access\".to_string(),")
        .expect("the property is published");
    let overrides = src
        .find("// Apply user overrides from config")
        .expect("user overrides");
    assert!(insert < overrides, "a user -D still wins");
}
