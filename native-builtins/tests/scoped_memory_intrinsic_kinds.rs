// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;

#[test]
fn only_scalar_internal_accessors_replace_real_java_wrappers_in_both_modes() {
    let class = "jdk/internal/misc/ScopedMemoryAccess";
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_compatibility_mode(mode);
        cratonvm_native_builtins::register_essential_natives(&mut registry);
        for (suffix, tag) in [
            ("Byte", 'B'),
            ("Short", 'S'),
            ("Char", 'C'),
            ("Int", 'I'),
            ("Long", 'J'),
        ] {
            for unaligned in [false, true] {
                if suffix == "Byte" && unaligned {
                    continue;
                }
                for write in [false, true] {
                    let name = format!(
                        "{}{}{}",
                        if write { "put" } else { "get" },
                        suffix,
                        if unaligned { "Unaligned" } else { "" }
                    );
                    let descriptor = format!(
                        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J{}{}){}",
                        if write {
                            tag.to_string()
                        } else {
                            String::new()
                        },
                        if unaligned { "Z" } else { "" },
                        if write { 'V' } else { tag }
                    );
                    // Existing retired-shadow filtering may omit a public wrapper
                    // entirely; it must never gain authority over real bytecode.
                    assert_ne!(registry.kind_of(class, &name, &descriptor), Some(NativeKind::Intrinsic),
                        "{mode:?}: the Java wrapper/finally must remain authoritative for {name}{descriptor}");
                    assert_eq!(registry.kind_of(class, &format!("{name}Internal"), &descriptor), Some(NativeKind::Intrinsic),
                        "{mode:?}: the checked scalar operation must survive strict native dispatch for {name}{descriptor}");
                }
            }
        }
        // Round 19: the bulk copy's checked operation too (its wrapper stays Java).
        let copy = "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V";
        assert_ne!(
            registry.kind_of(class, "copyMemory", copy),
            Some(NativeKind::Intrinsic),
            "{mode:?}: copyMemory's Java wrapper must remain authoritative"
        );
        assert_eq!(
            registry.kind_of(class, "copyMemoryInternal", copy),
            Some(NativeKind::Intrinsic),
            "{mode:?}: copyMemoryInternal must survive strict native dispatch"
        );
    }
}

/// Round nio20 wave 3, lane L5: the four BULK Internal bodies are windowed
/// reviewed intrinsics in both modes (so a shared close's handshake waits for
/// them before the session's Java cleanup actions run), and the `copyMemory`
/// wrapper never gains authority over its real Java `catch`.
#[test]
fn bulk_internal_bodies_are_windowed_intrinsics_in_both_modes() {
    let class = "jdk/internal/misc/ScopedMemoryAccess";
    let copy = "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V";
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_compatibility_mode(mode);
        cratonvm_native_builtins::register_essential_natives(&mut registry);
        for (name, descriptor) in [
            ("copyMemoryInternal", copy),
            (
                "copySwapMemoryInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJJ)V",
            ),
            (
                "setMemoryInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJB)V",
            ),
            (
                "vectorizedMismatchInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JII)I",
            ),
        ] {
            assert_eq!(
                registry.kind_of(class, name, descriptor),
                Some(NativeKind::Intrinsic),
                "{mode:?}: {name}{descriptor} must be the windowed intrinsic"
            );
        }
        assert_ne!(
            registry.kind_of(class, "copyMemory", copy),
            Some(NativeKind::Intrinsic),
            "{mode:?}: the copyMemory wrapper's Java catch stays authoritative"
        );
    }
}

/// Round nio20 wave 5, lane L5: the windowed twins are default ON, so a
/// sample of each family (volatile, CAS, getAndOp, plain boolean, mapped)
/// is a reviewed `Intrinsic` in both modes, and the public wrapper of each
/// keeps its Java `catch (ScopedAccessError)`. Skipped when the kill switch is
/// set in the test environment.
#[test]
fn windowed_twins_are_default_intrinsics_in_both_modes() {
    if std::env::var("CRATONVM_FFM_WINDOWED_SCOPED_TWINS").is_ok_and(|v| v.trim() == "0") {
        return;
    }
    let class = "jdk/internal/misc/ScopedMemoryAccess";
    let rows = [
        ("getIntVolatile", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)I"),
        ("compareAndSetLong", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJJ)Z"),
        ("getAndAddInt", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)I"),
        ("getFloatVolatile", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)F"),
        ("putDoubleRelease", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)V"),
        ("getBoolean", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)Z"),
        (
            "load",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/access/foreign/MappedMemoryUtilsProxy;JZJ)V",
        ),
    ];
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_compatibility_mode(mode);
        cratonvm_native_builtins::register_essential_natives(&mut registry);
        for (name, descriptor) in rows {
            assert_eq!(
                registry.kind_of(class, &format!("{name}Internal"), descriptor),
                Some(NativeKind::Intrinsic),
                "{mode:?}: {name}Internal{descriptor} must be the windowed twin"
            );
            assert_ne!(
                registry.kind_of(class, name, descriptor),
                Some(NativeKind::Intrinsic),
                "{mode:?}: the {name} wrapper's Java catch stays authoritative"
            );
        }
        // nio20 w5 fix-back: the four plain float / double rows stay the
        // compiled JDK body (a native there measured ~1.8x slower).
        for (name, descriptor) in [
            (
                "getFloatInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)F",
            ),
            (
                "putFloatInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JF)V",
            ),
            (
                "getDoubleInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)D",
            ),
            (
                "putDoubleInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JD)V",
            ),
        ] {
            assert_eq!(
                registry.kind_of(class, name, descriptor),
                None,
                "{mode:?}: {name}{descriptor} must stay unregistered"
            );
        }
    }
}
