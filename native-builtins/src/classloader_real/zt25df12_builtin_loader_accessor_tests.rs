use super::register_classloader_real_natives;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;

const ACCESSORS: [&str; 2] = ["getBuiltinPlatformClassLoader", "getBuiltinAppClassLoader"];

fn kinds(mode: CompatibilityMode) -> Vec<Option<NativeKind>> {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    register_classloader_real_natives(&mut r);
    ACCESSORS
        .iter()
        .map(|m| r.kind_of("java/lang/ClassLoader", m, "()Ljava/lang/ClassLoader;"))
        .collect()
}

#[test]
fn zt25df12_the_accessors_are_compatible_only_intrinsics() {
    assert_eq!(
        kinds(CompatibilityMode::Compatible),
        vec![Some(NativeKind::Intrinsic); 2],
        "--compatible: both carriers, stated Intrinsic"
    );
    assert_eq!(
        kinds(CompatibilityMode::JdkOnly),
        vec![None, None],
        "--real-jdk: the JDK bodies run"
    );
}
