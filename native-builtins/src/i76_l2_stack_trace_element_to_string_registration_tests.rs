use super::*;

/// Interpreter round i3 wave 76, lane L2
/// (`CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC`):
/// `StackTraceElement.toString()` carries a reviewed `Intrinsic` only
/// under `--real-jdk` over a real JDK with the switch on; every other
/// configuration registers nothing on it here, so the JDK body runs. The
/// switch is set explicitly in each arm, never read from its default.
#[test]
fn the_to_string_intrinsic_registers_only_under_jdk_only_with_the_switch() {
    use cratonvm_native_api::NativeKind;
    use cratonvm_types::compat::CompatibilityMode;
    let ste = "java/lang/StackTraceElement";
    let desc = "()Ljava/lang/String;";
    for (mode, on, want) in [
        (
            CompatibilityMode::JdkOnly,
            true,
            Some(NativeKind::Intrinsic),
        ),
        (CompatibilityMode::JdkOnly, false, None),
        (CompatibilityMode::Compatible, true, None),
        (CompatibilityMode::Compatible, false, None),
    ] {
        let mut registry = NativeMethodRegistry::new();
        registry.set_compatibility_mode(mode);
        registry.set_real_jdk(true);
        registry.set_stack_trace_element_to_string_intrinsic(on);
        // Registered only in a VM without a JIT (the wave-76 triage).
        registry.set_jit_enabled(false);
        register_essential_natives(&mut registry);
        assert_eq!(
            registry.kind_of(ste, "toString", desc),
            want,
            "StackTraceElement.toString under {mode:?} with the switch {on}"
        );
    }
    // With a JIT the switch registers nothing, unless
    // `CRATONVM_INTRINSIC_BODY_COMPILES` (wave 80): then the row registers
    // and is marked body-compilable.
    for body_compiles in [false, true] {
        let mut registry = NativeMethodRegistry::new();
        registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
        registry.set_real_jdk(true);
        registry.set_stack_trace_element_to_string_intrinsic(true);
        registry.set_jit_enabled(true);
        registry.set_intrinsic_body_compiles(body_compiles);
        register_essential_natives(&mut registry);
        let want = if body_compiles {
            Some(NativeKind::Intrinsic)
        } else {
            None
        };
        assert_eq!(
            registry.kind_of(ste, "toString", desc),
            want,
            "body_compiles={body_compiles}"
        );
        assert_eq!(
            registry.is_body_compilable_intrinsic(ste, "toString", desc),
            body_compiles,
            "body_compiles={body_compiles}"
        );
    }
}
