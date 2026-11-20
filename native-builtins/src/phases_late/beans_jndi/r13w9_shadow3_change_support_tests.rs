use super::*;
use cratonvm_types::compat::CompatibilityMode;

const PCS: &str = "java/beans/PropertyChangeSupport";
const PCS_INIT: &str = "(Ljava/lang/Object;)V";
const INTRO: &str = "java/beans/Introspector";
const GET_BEAN_INFO: &str = "(Ljava/lang/Class;)Ljava/beans/BeanInfo;";

fn registry(real_jdk: bool, mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_real_jdk(real_jdk);
    register_p72_beans(&mut r);
    r
}

/// `--compatible` on a real JDK: the real classes run their own bytecode;
/// the introspection half Spring needs is still there.
#[test]
fn compatible_on_a_real_jdk_leaves_change_support_to_bytecode() {
    if !beans_real_change_support_enabled() {
        return; // CRATONVM_BEANS_REAL_CHANGE_SUPPORT=0 in this environment
    }
    let r = registry(true, CompatibilityMode::Compatible);
    assert!(r.find(PCS, "<init>", PCS_INIT).is_none());
    assert!(r.find(INTRO, "getBeanInfo", GET_BEAN_INFO).is_some());
}

/// The synthetic JDK has no bytecode for the three classes and keeps the
/// natives; `--real-jdk` keeps going through the wave-8 retirement table.
#[test]
fn synthetic_jdk_and_jdk_only_keep_the_registration() {
    let synthetic = registry(false, CompatibilityMode::Compatible);
    assert!(synthetic.find(PCS, "<init>", PCS_INIT).is_some());
    assert!(synthetic
        .find(INTRO, "getBeanInfo", GET_BEAN_INFO)
        .is_some());
    let mut jdk_only = NativeMethodRegistry::new();
    jdk_only.set_compatibility_mode(CompatibilityMode::JdkOnly);
    jdk_only.set_real_jdk(true);
    assert!(!beans_change_support_left_to_bytecode(&jdk_only));
}
