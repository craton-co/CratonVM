use super::*;

#[test]
fn as_str_is_the_documented_wire_spelling() {
    assert_eq!(CompatibilityMode::Compatible.as_str(), "compatible");
    assert_eq!(CompatibilityMode::JdkOnly.as_str(), "jdk-only");
}

/// The report JSON round-trips through `as_str`, so the mapping has to be
/// injective — two modes sharing a spelling would make the report unable to
/// say which run produced it.
#[test]
fn as_str_round_trips() {
    for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
        let parsed = match mode.as_str() {
            "compatible" => CompatibilityMode::Compatible,
            "jdk-only" => CompatibilityMode::JdkOnly,
            other => panic!("unmapped spelling {other}"),
        };
        assert_eq!(parsed, mode);
    }
    assert_ne!(
        CompatibilityMode::Compatible.as_str(),
        CompatibilityMode::JdkOnly.as_str()
    );
}

/// Strictness must never be the default: it is opted into by `--real-jdk`
/// and never inferred from a build feature or an unrelated env var.
#[test]
fn default_is_compatible() {
    assert_eq!(CompatibilityMode::default(), CompatibilityMode::Compatible);
    assert!(!CompatibilityMode::default().is_jdk_only());
}

#[test]
fn is_jdk_only_matches_the_mode() {
    assert!(CompatibilityMode::JdkOnly.is_jdk_only());
    assert!(!CompatibilityMode::Compatible.is_jdk_only());
}

#[test]
fn execution_policy_constructors() {
    let compat = ExecutionPolicy::compatible(true);
    assert_eq!(compat.compatibility_mode, CompatibilityMode::Compatible);
    assert!(compat.real_jdk);
    assert!(!compat.is_jdk_only());

    let synthetic = ExecutionPolicy::compatible(false);
    assert!(!synthetic.real_jdk);
    assert!(!synthetic.is_jdk_only());

    let strict = ExecutionPolicy::jdk_only();
    assert_eq!(strict.compatibility_mode, CompatibilityMode::JdkOnly);
    assert!(
        strict.real_jdk,
        "--real-jdk always implies a real JDK image"
    );
    assert!(strict.is_jdk_only());
}

/// The default policy is the *current* VM: real JDK, every substitution
/// permitted. An embedded `VmConfig::default()` must not silently become
/// strict.
#[test]
fn execution_policy_default_is_compatible_real_jdk() {
    assert_eq!(
        ExecutionPolicy::default(),
        ExecutionPolicy::compatible(true)
    );
    assert!(!ExecutionPolicy::default().is_jdk_only());
    assert!(ExecutionPolicy::default().real_jdk);
}
