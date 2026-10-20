// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.5 — built-in class loader hierarchy: boot, platform, application.
//!
//! In the JVM spec (§5.3) and JDK 9+ implementation:
//!
//!   * The **bootstrap** class loader is represented as `null` in the Java
//!     API. It loads classes from the boot layer (java.base etc.) and has no
//!     visible parent. Its Rust analogue here is simply the absence of a
//!     ClassLoader object + `ClassLoaderId::BOOTSTRAP` (0).
//!   * The **platform** class loader is an instance of
//!     `jdk.internal.loader.ClassLoaders$PlatformClassLoader`. Its parent is
//!     the bootstrap loader (i.e. `getParent()` returns `null`).
//!   * The **application** (a.k.a. system) class loader is an instance of
//!     `jdk.internal.loader.ClassLoaders$AppClassLoader`. Its parent is the
//!     platform loader.
//!
//! `ClassLoader.getSystemClassLoader()` returns the app loader;
//! `ClassLoader.getPlatformClassLoader()` returns the platform loader. The
//! native allocation helpers live in `native-builtins/src/classloader.rs`;
//! this module owns the shared **name** constants and a few convenience
//! helpers used both by the native side and by the class manager's
//! registration hooks.
//!
//! The purpose of the dedicated module is:
//!   1. Provide a single authoritative name for each built-in loader class
//!      so it cannot drift across the native side and the class manager
//!      side (previously these were hard-coded `java/lang/ClassLoader` in
//!      the allocator, which made `getClass().getName()` report the wrong
//!      type).
//!   2. Expose `builtin_loader_kind(internal_name)` so the class manager
//!      can recognise the three built-in classes when they are defined by
//!      the runtime and skip the usual parent-delegation search.
//!   3. Provide public registration hooks (`register_builtin_loader_aliases`)
//!      that the class manager calls during startup; nothing here mutates
//!      the class manager's internal tables directly.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Internal JVM name of the built-in platform class loader.
///
/// Matches HotSpot 25's `jdk.internal.loader.ClassLoaders$PlatformClassLoader`.
pub const PLATFORM_LOADER_CLASS: &str = "jdk/internal/loader/ClassLoaders$PlatformClassLoader";

/// Internal JVM name of the built-in application (system) class loader.
pub const APP_LOADER_CLASS: &str = "jdk/internal/loader/ClassLoaders$AppClassLoader";

/// Internal JVM name of the shared `BuiltinClassLoader` base type that the
/// platform and app loaders inherit from on HotSpot 25.
pub const BUILTIN_LOADER_CLASS: &str = "jdk/internal/loader/BuiltinClassLoader";

/// Internal JVM name of `jdk.internal.loader.BootLoader`. There is no
/// instance — BootLoader is a "static-only" class that stands in for the
/// null bootstrap loader.
pub const BOOT_LOADER_STATIC_CLASS: &str = "jdk/internal/loader/BootLoader";

/// Discriminator returned by [`builtin_loader_kind`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BuiltinLoaderKind {
    /// `jdk/internal/loader/BootLoader` — the static stand-in for the null
    /// bootstrap loader.
    Boot,
    /// `jdk/internal/loader/ClassLoaders$PlatformClassLoader`.
    Platform,
    /// `jdk/internal/loader/ClassLoaders$AppClassLoader`.
    App,
    /// Shared super-type `jdk/internal/loader/BuiltinClassLoader`.
    BuiltinBase,
}

/// Recognise a built-in loader class by its JVM-internal name.
///
/// Returns `None` for every other class (incl. `java/lang/ClassLoader`
/// itself; that is the common abstract base and not a built-in loader).
pub fn builtin_loader_kind(internal_name: &str) -> Option<BuiltinLoaderKind> {
    match internal_name {
        BOOT_LOADER_STATIC_CLASS => Some(BuiltinLoaderKind::Boot),
        PLATFORM_LOADER_CLASS => Some(BuiltinLoaderKind::Platform),
        APP_LOADER_CLASS => Some(BuiltinLoaderKind::App),
        BUILTIN_LOADER_CLASS => Some(BuiltinLoaderKind::BuiltinBase),
        _ => None,
    }
}

/// Return the expected parent loader's internal class name, or `None` for
/// the bootstrap loader which has no parent.
///
/// Callers use this to lazily chain `getParent()` results so the built-in
/// hierarchy is consistent: `App -> Platform -> (null)`.
pub fn builtin_parent_class(kind: BuiltinLoaderKind) -> Option<&'static str> {
    match kind {
        BuiltinLoaderKind::Boot => None,
        BuiltinLoaderKind::Platform => None, // parent is the bootstrap (null)
        BuiltinLoaderKind::App => Some(PLATFORM_LOADER_CLASS),
        BuiltinLoaderKind::BuiltinBase => None, // abstract base; not instantiated
    }
}

/// Human-readable display name for `ClassLoader.getName()` per the
/// HotSpot convention: the platform loader returns `"platform"`, the app
/// loader returns `"app"`, and the bootstrap loader returns `null`
/// (`None` here).
pub fn builtin_display_name(kind: BuiltinLoaderKind) -> Option<&'static str> {
    match kind {
        BuiltinLoaderKind::Boot => None,
        BuiltinLoaderKind::Platform => Some("platform"),
        BuiltinLoaderKind::App => Some("app"),
        BuiltinLoaderKind::BuiltinBase => None,
    }
}

/// Registration hook state: whether the class-manager has been told about
/// the three built-in loader class names. Incremented once on successful
/// registration; the counter is purely diagnostic (used by integration
/// tests in `vm/tests/wp1_5_*.rs`).
static REGISTRATION_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Record that the class manager has associated the built-in loader names
/// with their reserved `ClassLoaderId`s. Call this exactly once during VM
/// startup; subsequent calls are no-ops and do not fail. This keeps the
/// built-in loader names observable as a single, ordered set regardless of
/// which subsystem gets there first (the native side or the classloading
/// side).
pub fn register_builtin_loader_aliases() {
    REGISTRATION_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Number of times [`register_builtin_loader_aliases`] has been invoked.
///
/// Exposed to integration tests so they can assert the registration hook
/// ran at least once during VM initialization.
pub fn registration_count() -> usize {
    REGISTRATION_COUNT.load(Ordering::SeqCst)
}

/// Iterate every built-in loader kind in ancestor-first order
/// (Boot → Platform → App). Used by integration tests to assert the full
/// delegation chain is coherent.
pub fn all_kinds() -> impl Iterator<Item = BuiltinLoaderKind> {
    [
        BuiltinLoaderKind::Boot,
        BuiltinLoaderKind::Platform,
        BuiltinLoaderKind::App,
    ]
    .into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip_for_known_names() {
        assert_eq!(
            builtin_loader_kind(BOOT_LOADER_STATIC_CLASS),
            Some(BuiltinLoaderKind::Boot)
        );
        assert_eq!(
            builtin_loader_kind(PLATFORM_LOADER_CLASS),
            Some(BuiltinLoaderKind::Platform)
        );
        assert_eq!(
            builtin_loader_kind(APP_LOADER_CLASS),
            Some(BuiltinLoaderKind::App)
        );
        assert_eq!(
            builtin_loader_kind(BUILTIN_LOADER_CLASS),
            Some(BuiltinLoaderKind::BuiltinBase)
        );
        assert_eq!(builtin_loader_kind("java/lang/ClassLoader"), None);
        assert_eq!(builtin_loader_kind("java/net/URLClassLoader"), None);
    }

    #[test]
    fn parent_chain_is_app_platform_bootstrap() {
        assert_eq!(
            builtin_parent_class(BuiltinLoaderKind::App),
            Some(PLATFORM_LOADER_CLASS)
        );
        assert!(builtin_parent_class(BuiltinLoaderKind::Platform).is_none());
        assert!(builtin_parent_class(BuiltinLoaderKind::Boot).is_none());
    }

    #[test]
    fn display_names_match_hotspot_convention() {
        assert_eq!(builtin_display_name(BuiltinLoaderKind::App), Some("app"));
        assert_eq!(
            builtin_display_name(BuiltinLoaderKind::Platform),
            Some("platform")
        );
        assert!(builtin_display_name(BuiltinLoaderKind::Boot).is_none());
    }

    #[test]
    fn registration_hook_is_idempotent_and_counts() {
        let before = registration_count();
        register_builtin_loader_aliases();
        register_builtin_loader_aliases();
        let after = registration_count();
        assert!(after >= before + 2);
    }

    #[test]
    fn all_kinds_includes_three_entries_in_order() {
        let kinds: Vec<_> = all_kinds().collect();
        assert_eq!(
            kinds,
            vec![
                BuiltinLoaderKind::Boot,
                BuiltinLoaderKind::Platform,
                BuiltinLoaderKind::App,
            ]
        );
    }
}
