use super::{kem_left_to_bytecode, register, KEM_CLASS};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

const GET_INSTANCE: &str = "(Ljava/lang/String;)Ljavax/crypto/KEM;";

fn registry(real_jdk: bool, mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_real_jdk(real_jdk);
    register(&mut r);
    r
}

/// JIT round 16 wave 17 (lane shadow): `--compatible` over a real JDK
/// registers no KEM carrier native.
#[test]
fn compatible_on_a_real_jdk_leaves_kem_to_bytecode() {
    if !super::compat_real_kem_enabled()
        || crate::real_jca_mode()
        || !crate::route_pqc_to_real()
    {
        return; // a kill switch is set in this environment
    }
    let r = registry(true, CompatibilityMode::Compatible);
    assert!(r.find(KEM_CLASS, "getInstance", GET_INSTANCE).is_none());
    let synthetic = registry(false, CompatibilityMode::Compatible);
    assert!(synthetic
        .find(KEM_CLASS, "getInstance", GET_INSTANCE)
        .is_some());
    let mut jdk_only = NativeMethodRegistry::new();
    jdk_only.set_compatibility_mode(CompatibilityMode::JdkOnly);
    jdk_only.set_real_jdk(true);
    assert!(!kem_left_to_bytecode(&jdk_only));
}
