use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn register_essential_includes_reflective_field_accessors() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let field = "java/lang/reflect/Field";
    for (name, desc) in [
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ("set", "(Ljava/lang/Object;Ljava/lang/Object;)V"),
        ("getInt", "(Ljava/lang/Object;)I"),
        ("getLong", "(Ljava/lang/Object;)J"),
        ("getFloat", "(Ljava/lang/Object;)F"),
        ("getDouble", "(Ljava/lang/Object;)D"),
        ("getBoolean", "(Ljava/lang/Object;)Z"),
        ("getByte", "(Ljava/lang/Object;)B"),
        ("getShort", "(Ljava/lang/Object;)S"),
        ("getChar", "(Ljava/lang/Object;)C"),
        ("setInt", "(Ljava/lang/Object;I)V"),
        ("setLong", "(Ljava/lang/Object;J)V"),
        ("setFloat", "(Ljava/lang/Object;F)V"),
        ("setDouble", "(Ljava/lang/Object;D)V"),
        ("setBoolean", "(Ljava/lang/Object;Z)V"),
        ("setByte", "(Ljava/lang/Object;B)V"),
        ("setShort", "(Ljava/lang/Object;S)V"),
        ("setChar", "(Ljava/lang/Object;C)V"),
    ] {
        assert!(
            registry.find(field, name, desc).is_some(),
            "Field.{name}{desc} must be registered in the real-JDK essential native path"
        );
    }
}

/// Interpreter round i3 wave 73, lane L3 (`CRATONVM_FIELD_DOORS_INTRINSIC`):
/// the `Field` value doors are reviewed `Intrinsic`s only under
/// `--real-jdk` with the switch on; every other configuration registers
/// the same natives as `Bridge`s, as before. The switch is set explicitly
/// in each arm, never read from its default.
#[test]
fn jdk_only_field_doors_register_the_field_value_doors_as_intrinsics() {
    use cratonvm_native_api::NativeKind;
    use cratonvm_types::compat::CompatibilityMode;
    let field = "java/lang/reflect/Field";
    let doors = [
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ("set", "(Ljava/lang/Object;Ljava/lang/Object;)V"),
        ("getInt", "(Ljava/lang/Object;)I"),
        ("getChar", "(Ljava/lang/Object;)C"),
        ("setLong", "(Ljava/lang/Object;J)V"),
        ("setBoolean", "(Ljava/lang/Object;Z)V"),
    ];
    for (mode, on, want) in [
        // Only `--real-jdk` with the switch on promotes the doors. Off, they
        // are retired shadows over a real JDK: refused under `--real-jdk`
        // (the JDK's own `Field` bytecode runs) and re-tagged
        // `SyntheticStub` under `--compatible`. `None` below means "any
        // kind but `Intrinsic`".
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
        registry.set_jdk_only_field_doors(on);
        register_essential_natives(&mut registry);
        for (name, desc) in doors {
            let kind = registry.kind_of(field, name, desc);
            match want {
                Some(want) => assert_eq!(
                    kind,
                    Some(want),
                    "Field.{name}{desc} under {mode:?} with the switch {on}"
                ),
                None => assert_ne!(
                    kind,
                    Some(NativeKind::Intrinsic),
                    "Field.{name}{desc} under {mode:?} with the switch {on}"
                ),
            }
        }
        // The switch reaches the value doors only: `setAccessible` and
        // the metadata getters keep their own kinds (and, being retired
        // shadows over a real JDK, may not be registered at all).
        assert_ne!(
            registry.kind_of(field, "getName", "()Ljava/lang/String;"),
            Some(NativeKind::Intrinsic),
            "Field.getName under {mode:?} with the switch {on}"
        );
    }
}
