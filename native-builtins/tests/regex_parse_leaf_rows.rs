// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::compat::CompatibilityMode;

#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

#[test]
fn real_regex_and_long_parse_leaf_claims_survive_last_registration() {
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
