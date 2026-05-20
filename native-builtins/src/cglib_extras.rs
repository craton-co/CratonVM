//! cglib_probe — formerly a boot-test shim, now neutered.
//!
//! The cglib probe used to be short-circuited because a `ClassLoader`
//! subclass calling `defineClass` NPE'd with "Cannot invoke getCodeSource
//! on null" inside the real JDK `ClassLoader.preDefineClass` bytecode.
//!
//! Root cause (fixed): the simplified real-JDK `ClassLoader.<init>`
//! natives in `classloader_real.rs` only stored the `parent` field and
//! skipped the real ctor's `defaultDomain = new ProtectionDomain(...)`
//! field initialiser, leaving `defaultDomain` null. `preDefineClass`
//! then read that null field and NPE'd. `classloader_real.rs` now calls
//! `init_classloader_common_fields`, which builds a non-null
//! `defaultDomain` (plus `classes` / `packages` / etc.), so the real
//! cglib `Enhancer.create()` → `defineClass` path runs cleanly.
//!
//! This registration body is intentionally empty — no synthetic stubs.
use rustjvm_native_api::NativeMethodRegistry;

pub fn register_cglib_stubs(_registry: &mut NativeMethodRegistry) {
    // Neutered: the underlying VM bug (null `defaultDomain` on custom
    // ClassLoaders) is fixed in `classloader_real.rs`. No shims here.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_cglib_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_cglib_stubs(&mut r);
    }
}
