use super::{
    keystore_spi_left_to_bytecode, register_keystore_real, DKS_FQN, JCEKS_FQN, JKS_FQN,
};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

const LOAD: &str = "(Ljava/io/InputStream;[C)V";

fn registry(real_jdk: bool, mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_real_jdk(real_jdk);
    register_keystore_real(&mut r);
    r
}

/// JIT round 16 wave 17 (lane shadow): `--compatible` over a real JDK runs
/// the JDK's JCEKS / DKS bodies; the JKS / PKCS12 surfaces stay.
#[test]
fn compatible_on_a_real_jdk_leaves_jceks_and_dks_to_bytecode() {
    if !true {
        return; // CRATONVM_COMPAT_REAL_KEYSTORE_SPI=0 in this environment
    }
    let r = registry(true, CompatibilityMode::Compatible);
    assert!(r.find(JCEKS_FQN, "engineLoad", LOAD).is_none());
    assert!(r.find(DKS_FQN, "engineLoad", LOAD).is_none());
    assert!(r.find(JKS_FQN, "engineLoad", LOAD).is_some());
}

/// The synthetic JDK keeps them, and `--real-jdk` keeps registering them
/// for the retirement table to re-tag.
#[test]
fn synthetic_jdk_and_jdk_only_keep_the_registration() {
    let synthetic = registry(false, CompatibilityMode::Compatible);
    assert!(synthetic.find(JCEKS_FQN, "engineLoad", LOAD).is_some());
    assert!(synthetic.find(DKS_FQN, "engineLoad", LOAD).is_some());
    let mut jdk_only = NativeMethodRegistry::new();
    jdk_only.set_compatibility_mode(CompatibilityMode::JdkOnly);
    jdk_only.set_real_jdk(true);
    assert!(!keystore_spi_left_to_bytecode(&jdk_only));
}
