use super::*;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;

const CL: &str = "java/lang/ClassLoader";
const M: &str = "getDefinedPackage";
const D: &str = "(Ljava/lang/String;)Ljava/lang/Package;";

fn registry(mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_real_jdk(true);
    i2_register_classloader_package_natives(&mut r);
    r
}

#[test]
fn registration_leaves_the_jdk_body_unshadowed_in_both_modes() {
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        let r = registry(mode);
        assert_eq!(r.kind_of(CL, M, D), Some(cratonvm_native_api::NativeKind::Bridge));
        assert!(!r.reentry_only_bodies().contains(&(CL, M, D)));
    }
}
