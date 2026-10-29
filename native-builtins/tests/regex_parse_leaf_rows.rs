// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

/// The Matcher rows exist only under `CRATONVM_REGEX_MATCHER_BYTECODE=0` (JIT
/// r23 wave 2c, lane L5: by default the Matcher natives yield to the JDK
/// bytecode), so this asks under that override.
#[test]
fn real_regex_and_long_parse_leaf_claims_survive_last_registration() {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_REGEX_MATCHER_BYTECODE", Some("0"))],
        real_regex_and_long_parse_leaf_claims,
    );
}

/// JIT r23 wave 2c, lane L5: by default no real-layout Matcher native is
/// registered in either mode -- `find` / `group` / `hitEnd` are the JDK
/// bytecode -- while the `Long.parseLong` leaves stay.
#[test]
fn matcher_natives_yield_to_bytecode_by_default() {
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_REGEX_MATCHER_BYTECODE", None),
            ("CRATONVM_NATIVE_MATCHER_FIND", None),
        ],
        || {
            for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
                let mut registry = NativeMethodRegistry::new();
                registry.set_compatibility_mode(mode);
                registry.set_drop_real_layout_synthetic(true);
                boot_path::vm_init_real_jdk_boot_path(&mut registry);
                for (method, descriptor) in [
                    ("find", "()Z"),
                    ("find", "(I)Z"),
                    ("group", "(I)Ljava/lang/String;"),
                    ("hitEnd", "()Z"),
                    ("requireEnd", "()Z"),
                ] {
                    assert!(
                        registry
                            .resolve_id("java/util/regex/Matcher", method, descriptor)
                            .is_none(),
                        "Matcher.{method}{descriptor} must be the JDK bytecode"
                    );
                }
                assert!(registry
                    .resolve_id("java/lang/Long", "parseLong", "(Ljava/lang/String;)J")
                    .is_some());
            }
        },
    );
}

/// JIT r23 wave 4, lane L5 (`CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE`,
/// default ON since wave 5, R23-W5-FLIP): with the compile cache off neither
/// `Pattern.compile` row is registered in either mode and dispatch reaches the
/// JDK bodies; `=0` keeps both rows (an `Intrinsic`, so the regex drop keeps
/// them), and so does the cache, since it is their other job.
#[test]
fn pattern_compile_rows_yield_to_bytecode_only_when_asked() {
    const ROWS: [&str; 2] = [
        "(Ljava/lang/String;)Ljava/util/regex/Pattern;",
        "(Ljava/lang/String;I)Ljava/util/regex/Pattern;",
    ];
    let present = |edits: &[(&str, Option<&str>)]| {
        cratonvm_types::flags::with_thread_overrides(edits, || {
            [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly].map(|mode| {
                let mut registry = NativeMethodRegistry::new();
                registry.set_compatibility_mode(mode);
                registry.set_drop_real_layout_synthetic(true);
                boot_path::vm_init_real_jdk_boot_path(&mut registry);
                ROWS.map(|d| {
                    registry
                        .resolve_id("java/util/regex/Pattern", "compile", d)
                        .is_some()
                })
            })
        })
    };
    let on = "CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE";
    let cache = "CRATONVM_REGEX_PATTERN_COMPILE_CACHE";
    assert_eq!(present(&[(on, None), (cache, None)]), [[false; 2]; 2], "default");
    assert_eq!(present(&[(on, Some("0")), (cache, None)]), [[true; 2]; 2], "switched off");
    assert_eq!(present(&[(on, Some("1")), (cache, None)]), [[false; 2]; 2], "opted in");
    assert_eq!(present(&[(on, Some("1")), (cache, Some("1"))]), [[true; 2]; 2], "cache on");
}

fn real_regex_and_long_parse_leaf_claims() {
    for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
        let mut registry = NativeMethodRegistry::new();
        registry.set_compatibility_mode(mode);
        registry.set_drop_real_layout_synthetic(true);
        boot_path::vm_init_real_jdk_boot_path(&mut registry);
        for (class, method, descriptor) in [
            ("java/util/regex/Matcher", "find", "()Z"),
            ("java/lang/Long", "parseLong", "(Ljava/lang/String;)J"),
            ("java/lang/Long", "parseLong", "(Ljava/lang/String;I)J"),
        ] {
            let id = registry
                .resolve_id(class, method, descriptor)
                .expect("registered intrinsic");
            assert!(registry.is_leaf_id(id), "{class}.{method}{descriptor}");
        }
        for (method, descriptor) in [
            ("find", "(I)Z"),
            ("group", "()Ljava/lang/String;"),
            ("group", "(I)Ljava/lang/String;"),
        ] {
            let id = registry
                .resolve_id("java/util/regex/Matcher", method, descriptor)
                .unwrap();
            assert!(
                !registry.is_leaf_id(id),
                "allocating or reentrant callback was classified leaf"
            );
        }
    }
}
