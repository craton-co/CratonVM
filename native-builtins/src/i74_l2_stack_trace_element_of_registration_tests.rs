use super::*;

/// Interpreter round i3 wave 74, lane L2
/// (`CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC`):
/// `StackTraceElement.of(Object, int)` carries a reviewed `Intrinsic` only
/// under `--real-jdk` over a real JDK with the switch on; every other
/// configuration registers nothing on it, so the JDK body runs as before.
/// `initStackTraceElements` keeps its `Bridge` in every arm. The switch is
/// set explicitly in each arm, never read from its default.
#[test]
fn the_of_intrinsic_registers_only_under_jdk_only_with_the_switch() {
    use cratonvm_native_api::NativeKind;
    use cratonvm_types::compat::CompatibilityMode;
    let ste = "java/lang/StackTraceElement";
    let of_desc = "(Ljava/lang/Object;I)[Ljava/lang/StackTraceElement;";
    let init_desc = "([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V";
    for (mode, real_jdk, on, want) in [
        (
            CompatibilityMode::JdkOnly,
            true,
            true,
            Some(NativeKind::Intrinsic),
        ),
        (CompatibilityMode::JdkOnly, true, false, None),
        (CompatibilityMode::Compatible, true, true, None),
        (CompatibilityMode::Compatible, true, false, None),
    ] {
        let mut registry = NativeMethodRegistry::new();
        registry.set_compatibility_mode(mode);
        registry.set_real_jdk(real_jdk);
        registry.set_stack_trace_element_of_intrinsic(on);
        register_essential_natives(&mut registry);
        assert_eq!(
            registry.kind_of(ste, "of", of_desc),
            want,
            "StackTraceElement.of under {mode:?} (real JDK {real_jdk}) with the switch {on}"
        );
        assert_eq!(
            registry.kind_of(ste, "initStackTraceElements", init_desc),
            Some(NativeKind::Bridge),
            "initStackTraceElements under {mode:?} (real JDK {real_jdk}) with the switch {on}"
        );
    }
}
