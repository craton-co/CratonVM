use super::*;

/// Compile-time pin that the curated surface re-exports the supported types.
/// (A live round-trip would require the full VM bootstrap; the `libcratonvm`
/// crate's `flat_api_live_vm` test covers the runtime path.)
#[test]
fn facade_reexports_exist() {
    fn _assert_sized<T>() {}
    _assert_sized::<VmConfig>();
    _assert_sized::<Vm>();
    _assert_sized::<SharedVm>();
    _assert_sized::<JvmThread>();
    _assert_sized::<ThreadId>();
    _assert_sized::<ClassId>();
    _assert_sized::<Value>();
    _assert_sized::<ObjectRef>();
    _assert_sized::<VmError>();
    _assert_sized::<MethodCallFailed>();
    _assert_sized::<StackTraceFrame>();
    _assert_sized::<CompatibilityMode>();
    _assert_sized::<ExecutionPolicy>();
    _assert_sized::<JdkMode>();
}

/// An embedded VM is `Compatible` unless the host says otherwise — on
/// **both** constructors, and regardless of which class library they boot.
///
/// This is the asymmetry pinned: `default()` and `with_host_jdk_default()`
/// disagree about [`JdkMode`] (hermetic library vs. host JDK) and agree
/// about [`CompatibilityMode`]. Strictness rejects work that `Compatible`
/// accepts, so it may only ever come from an explicit request; a Cargo
/// feature, an environment variable, or **another entry point's default**
/// must never supply it — and since 2026-09-20 there is such an entry
/// point to not inherit from: `VmConfig::for_launcher` is `JdkOnly`.
#[test]
fn embedded_default_is_compatible() {
    for cfg in [VmConfig::default(), VmConfig::with_host_jdk_default()] {
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
        assert!(!cfg.is_jdk_only());
        assert!(!cfg.execution_policy().is_jdk_only());
        cfg.validate_compatibility()
            .expect("a default config is always coherent");
    }
    // The JDK mode is the half that *does* differ between the two.
    assert_eq!(VmConfig::default().jdk_mode(), JdkMode::Synthetic);
    assert_eq!(
        VmConfig::with_host_jdk_default().jdk_mode(),
        JdkMode::Real,
        "the host-JDK base config boots the host JDK"
    );
    // And the launcher's config, which this crate does not hand out, is
    // the one that is strict. Pinned here so an "alignment" cleanup of
    // `with_host_jdk_default` back into an alias fails in this crate too.
    assert_eq!(
        VmConfig::for_launcher().compatibility_mode,
        CompatibilityMode::JdkOnly
    );
}

/// An explicit strict request round-trips through the facade's own types:
/// what the host set is what `is_jdk_only` / `execution_policy` report.
#[test]
fn explicit_jdk_only_round_trips() {
    let cfg =
        VmConfig::with_host_jdk_default().with_compatibility_mode(CompatibilityMode::JdkOnly);
    assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
    assert!(cfg.is_jdk_only());
    assert_eq!(cfg.compatibility_mode.as_str(), "jdk-only");

    let policy: ExecutionPolicy = cfg.execution_policy();
    assert_eq!(policy.compatibility_mode, CompatibilityMode::JdkOnly);
    assert!(policy.is_jdk_only());
    assert!(policy.real_jdk, "the launcher base config is real-JDK");
    assert_eq!(policy, ExecutionPolicy::jdk_only());

    // Setting the compatibility mode does not rewrite the JDK mode: this
    // config was already real, and quietly forcing `JdkMode::Real` would
    // erase the conflict `validate_compatibility` exists to report.
    assert_eq!(cfg.jdk_mode(), JdkMode::Real);
    cfg.validate_compatibility()
        .expect("real JDK + Real-JDK is coherent");
}

/// Strict mode plus the synthetic class library is a configuration error,
/// reported before the VM is built. The message names both flags and both
/// ways out, because which correction is right depends on what the caller
/// meant.
#[test]
fn jdk_only_plus_synthetic_is_rejected() {
    let cfg = VmConfig::default() // synthetic library
        .with_compatibility_mode(CompatibilityMode::JdkOnly);
    assert_eq!(cfg.jdk_mode(), JdkMode::Synthetic);
    assert!(cfg.is_jdk_only());

    match cfg.validate_compatibility() {
        Err(VmError::InvalidConfiguration(msg)) => {
            assert!(msg.contains("--real-jdk"), "{msg}");
            assert!(msg.contains("--synthetic-jdk"), "{msg}");
        }
        other => panic!("expected VmError::InvalidConfiguration, got {other:?}"),
    }
}

/// Compile-time pin for the facade helper signatures documented as the
/// stable embedding surface. These do not bootstrap a VM, so they stay
/// cheap and avoid depending on host JDK availability.
#[test]
fn facade_helper_signatures_are_pinned() {
    type MakeStringArrayFn = for<'vm, 'items, 'item> fn(
        &'vm mut Vm,
        &'items [&'item str],
    ) -> Result<ObjectRef, VmError>;
    type ReadStringFn = for<'vm> fn(&'vm Vm, ObjectRef) -> Option<String>;
    type ObjectClassNameFn = for<'vm> fn(&'vm Vm, ObjectRef) -> Option<String>;
    type DescribeFailureFn = for<'vm, 'err> fn(&'vm Vm, &'err MethodCallFailed) -> String;
    type FieldIndexFn = for<'vm, 'name> fn(&'vm Vm, ClassId, &'name str) -> Option<usize>;
    type FieldIndexDescFn = for<'vm, 'name, 'desc> fn(
        &'vm Vm,
        ClassId,
        &'name str,
        Option<&'desc str>,
    ) -> Option<usize>;
    type GetFieldByNameFn = for<'vm, 'name> fn(&'vm Vm, ObjectRef, &'name str) -> Option<Value>;
    type SetFieldByNameFn = for<'vm, 'name> fn(&'vm Vm, ObjectRef, &'name str, Value) -> bool;
    // gcg g4/oo: the OOME preallocation step of the lifecycle contract.
    type PreallocateOomeFn = for<'vm> fn(&'vm mut Vm);

    let _make: MakeStringArrayFn = make_string_array;
    let _read: ReadStringFn = read_string;
    let _object_class: ObjectClassNameFn = object_class_name;
    let _describe: DescribeFailureFn = describe_failure;
    let _field_index: FieldIndexFn = field_index;
    let _field_index_desc: FieldIndexDescFn = field_index_desc;
    let _get_field: GetFieldByNameFn = get_field_by_name;
    let _set_field: SetFieldByNameFn = set_field_by_name;
    let _preallocate: PreallocateOomeFn = preallocate_out_of_memory_error;
}
