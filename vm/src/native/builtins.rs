// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Re-exported from cratonvm-native-builtins. All crate::native::builtins::* paths continue to work.
pub use cratonvm_native_builtins::*;

// ---------------------------------------------------------------------------
// NEW-4: no-op shims for when the `synthetic-jdk` feature is OFF.
//
// `cratonvm-native-builtins` gates `register_builtins` and
// `register_synthetic_overrides` behind `#[cfg(feature = "synthetic-jdk")]`,
// so they simply do not exist in non-synthetic builds. The vm crate has
// inline-test call sites that reference these symbols via the
// `crate::native::builtins::*` path. Rather than sprinkle `#[cfg]` gates
// over every call site, we provide local no-op shims that keep the path
// resolvable.
//
// Tests that genuinely depend on synthetic natives must be gated with
// `#[cfg(feature = "synthetic-jdk")]` at the test level — the shims
// here exist only to keep compilation working, not to silence failing
// expectations.

#[cfg(not(feature = "synthetic-jdk"))]
pub fn register_builtins(_registry: &mut cratonvm_native_api::NativeMethodRegistry) {
    // No-op shim. See module documentation above.
}

#[cfg(not(feature = "synthetic-jdk"))]
pub fn register_synthetic_overrides(_registry: &mut cratonvm_native_api::NativeMethodRegistry) {
    // No-op shim. See module documentation above.
}
