// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

/// The Matcher and Long.parseLong leaf rows yield to JDK bytecode by default
/// (JIT r23 wave 2c, lane L5 for Matcher; String/Regex inline path for
/// `Long.parseLong`), so this asks under their opt-back overrides.
/// JIT r23 wave 2c, lane L5: by default no real-layout Matcher native is
/// registered in either mode -- `find` / `group` / `hitEnd` are the JDK
/// bytecode -- and `Long.parseLong` likewise yields to the JDK bytecode when
/// `CRATONVM_WRAPPER_BOX_BYTECODE` is unset (opt back in with `=0`).
/// JIT r23 wave 4, lane L5 (`CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE`,
/// default ON since wave 5, R23-W5-FLIP): with the compile cache off neither
/// `Pattern.compile` row is registered in either mode and dispatch reaches the
/// JDK bodies; `=0` keeps both rows (an `Intrinsic`, so the regex drop keeps
/// them), and so does the cache, since it is their other job.


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
