// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.4 — `jdk.internal.access.SharedSecrets` bridge layer.
//!
//! HotSpot exposes a family of `jdk.internal.access.Java*Access`
//! interfaces through `jdk.internal.access.SharedSecrets`.  Each
//! interface gives privileged (package-internal) access to pieces
//! of the JDK that aren't normally visible from application code.
//! The *singleton* for each interface is installed by the
//! containing class's `<clinit>` — `java.lang.System$1` for
//! `JavaLangAccess`, `java.lang.invoke.MethodHandleImpl$1` for
//! `JavaLangInvokeAccess`, etc.  When the installation never runs
//! (because cratonvm's synthetic boot doesn't execute every
//! clinit end-to-end) `SharedSecrets.getJavaLangAccess()` returns
//! `null`, and a downstream `jla.<method>` invokevirtual throws
//! NullPointerException — most famously "Cannot invoke
//! currentCarrierThread on null" in `ForkJoinPool`.
//!
//! The WP1.4 bridge addresses that by:
//!
//! 1. Lazily installing a synthetic singleton for every Access
//!    interface on first access.  See [`SHARED_SECRETS_OWNERS`]
//!    for the mapping of interface → concrete class name.
//! 2. Registering an opinionated Rust implementation of every
//!    interface method on that concrete class, so invokeinterface
//!    (which dispatches through the receiver's concrete class)
//!    resolves to a cratonvm native rather than the (null) Java
//!    field read.
//!
//! The native-side dispatch lives in
//! `native-builtins/src/shared_secrets_bridge.rs`; this file
//! exists as the canonical home for compile-time metadata shared
//! between the vm crate and the native-builtins crate.
//!
//! # Policy
//!
//! We aim for *structural* fidelity (the right arg/ret types and
//! the right method name on the right class) but *simplified*
//! semantics where the real JDK path pulls in subsystems we
//! don't implement.  For example,
//! `JavaLangRefAccess.waitForReferenceProcessing()` is a no-op in
//! our impl — cratonvm's generational GC doesn't pipeline
//! reference processing — but the method is reachable and returns
//! the expected boolean (`false` = nothing pending).
//!
//! # Extension
//!
//! New Access interfaces are added in three places:
//!
//! * [`SharedSecretsInterface`] — enum variant + metadata.
//! * [`SHARED_SECRETS_OWNERS`] — concrete class mapping.
//! * `native-builtins/src/shared_secrets_bridge.rs` — per-method
//!   registration.

use cratonvm_types::ObjectRef;

/// WP1.4 — tag for each `Java*Access` interface we bridge.
///
/// Kept separate from the concrete class name so multi-interface
/// owners (none in JDK 25, but the shape allows it) don't need to
/// duplicate metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SharedSecretsInterface {
    /// `jdk.internal.access.JavaLangAccess` — misc `java.lang`
    /// hooks: `getDeclaredPublicMethods`, `currentCarrierThread`,
    /// `getReflectionFactory`, `blockedOn`, `setCause`,
    /// `getEnumConstantsShared`, `newStringUtf8NoRepl`,
    /// `getBytesUtf8NoRepl`, `newStackTraceElement`.
    JavaLang,
    /// `jdk.internal.access.JavaLangInvokeAccess` —
    /// `findMethodHandleType`, `linkMethodHandleConstant`,
    /// `makeClassValueMap`.
    JavaLangInvoke,
    /// `jdk.internal.access.JavaLangRefAccess` —
    /// `waitForReferenceProcessing`, `runFinalization`.
    JavaLangRef,
    /// `jdk.internal.access.JavaLangReflectAccess` —
    /// `copyMethod`, `copyField`, `copyConstructor`,
    /// `newParameter`, `newAccessibleObject`,
    /// `getExecutableTypeAnnotationBytes`.
    JavaLangReflect,
    /// `jdk.internal.access.JavaIOAccess` — `console`, `charset`.
    JavaIO,
    /// `jdk.internal.access.JavaIORandomAccessFileAccess` —
    /// `open`, `openAsChannel`.
    JavaIORandomAccessFile,
    /// `jdk.internal.access.JavaNetInetAddressAccess` —
    /// `getHostFromNameService`, `getOriginalHostName`.
    JavaNetInetAddress,
    /// `jdk.internal.access.JavaNetUriAccess` — `create`.
    JavaNetUri,
    /// `jdk.internal.access.JavaNioAccess` — `getBufferPool`,
    /// `newDirectByteBuffer`, `acquireSession`.
    JavaNio,
    /// `jdk.internal.access.JavaSecurityAccess` —
    /// `doIntersectionPrivilege`, `getProtectDomains`.
    JavaSecurity,
    /// `jdk.internal.access.JavaUtilJarAccess` —
    /// `jarFileHasClassPathAttribute`, `ensureInitialization`.
    JavaUtilJar,
    /// `jdk.internal.access.JavaUtilZipFileAccess` — `getEntry`,
    /// `entryLocalNameEncoding`, `getManifestName`.
    JavaUtilZipFile,
    /// `jdk.internal.access.JavaNetHttpCookieAccess` —
    /// `parseCookie`.
    JavaNetHttpCookie,
    /// `jdk.internal.access.JavaObjectInputStreamAccess` —
    /// `checkArray`.
    JavaObjectInputStream,
    /// `jdk.internal.access.JavaUtilResourceBundleAccess` —
    /// `setParent`, `getParent`, `setLocale`.
    JavaUtilResourceBundle,
}

impl SharedSecretsInterface {
    /// Canonical interface class name (`/`-separated).  Used as
    /// the return-type descriptor component for the corresponding
    /// `SharedSecrets.getXxx()` native.
    pub fn interface_class(&self) -> &'static str {
        match self {
            Self::JavaLang => "jdk/internal/access/JavaLangAccess",
            Self::JavaLangInvoke => "jdk/internal/access/JavaLangInvokeAccess",
            Self::JavaLangRef => "jdk/internal/access/JavaLangRefAccess",
            Self::JavaLangReflect => "jdk/internal/access/JavaLangReflectAccess",
            Self::JavaIO => "jdk/internal/access/JavaIOAccess",
            Self::JavaIORandomAccessFile => {
                "jdk/internal/access/JavaIORandomAccessFileAccess"
            }
            Self::JavaNetInetAddress => "jdk/internal/access/JavaNetInetAddressAccess",
            Self::JavaNetUri => "jdk/internal/access/JavaNetUriAccess",
            Self::JavaNio => "jdk/internal/access/JavaNioAccess",
            Self::JavaSecurity => "jdk/internal/access/JavaSecurityAccess",
            Self::JavaUtilJar => "jdk/internal/access/JavaUtilJarAccess",
            Self::JavaUtilZipFile => "jdk/internal/access/JavaUtilZipFileAccess",
            Self::JavaNetHttpCookie => "jdk/internal/access/JavaNetHttpCookieAccess",
            Self::JavaObjectInputStream => {
                "jdk/internal/access/JavaObjectInputStreamAccess"
            }
            Self::JavaUtilResourceBundle => {
                "jdk/internal/access/JavaUtilResourceBundleAccess"
            }
        }
    }

    /// Concrete synthetic-class name that implements the
    /// interface — these match the HotSpot anonymous inner-class
    /// singletons so invokeinterface resolving a receiver of this
    /// class hits the matching cratonvm native.
    ///
    /// For interfaces without a canonical `$N` owner (the newer
    /// Access split-outs), we pick a stable synthetic name prefixed
    /// with `cratonvm/internal/ss/` so it can never collide with a
    /// real JDK class.
    pub fn owner_class(&self) -> &'static str {
        match self {
            Self::JavaLang => "java/lang/System$1",
            Self::JavaLangInvoke => "java/lang/invoke/MethodHandleImpl$1",
            Self::JavaLangRef => "java/lang/ref/Reference$1",
            Self::JavaLangReflect => "java/lang/reflect/ReflectAccess",
            Self::JavaIO => "java/io/Console$1",
            Self::JavaIORandomAccessFile => {
                "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1"
            }
            Self::JavaNetInetAddress => "java/net/InetAddress$1",
            Self::JavaNetUri => "cratonvm/internal/ss/JavaNetUriAccess$1",
            Self::JavaNio => "java/nio/Buffer$1",
            Self::JavaSecurity => "java/security/AccessController$1",
            Self::JavaUtilJar => "cratonvm/internal/ss/JavaUtilJarAccess$1",
            Self::JavaUtilZipFile => "java/util/zip/ZipFile$1",
            Self::JavaNetHttpCookie => "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
            Self::JavaObjectInputStream => "java/io/ObjectInputStream$1",
            Self::JavaUtilResourceBundle => "java/util/ResourceBundle$1",
        }
    }

    /// `SharedSecrets.getJavaXxxAccess()` — the factory method name
    /// used when we register the native.
    pub fn factory_method(&self) -> &'static str {
        match self {
            Self::JavaLang => "getJavaLangAccess",
            Self::JavaLangInvoke => "getJavaLangInvokeAccess",
            Self::JavaLangRef => "getJavaLangRefAccess",
            Self::JavaLangReflect => "getJavaLangReflectAccess",
            Self::JavaIO => "getJavaIOAccess",
            Self::JavaIORandomAccessFile => "getJavaIORandomAccessFileAccess",
            Self::JavaNetInetAddress => "getJavaNetInetAddressAccess",
            Self::JavaNetUri => "getJavaNetUriAccess",
            Self::JavaNio => "getJavaNioAccess",
            Self::JavaSecurity => "getJavaSecurityAccess",
            Self::JavaUtilJar => "getJavaUtilJarAccess",
            Self::JavaUtilZipFile => "getJavaUtilZipFileAccess",
            Self::JavaNetHttpCookie => "getJavaNetHttpCookieAccess",
            Self::JavaObjectInputStream => "getJavaObjectInputStreamAccess",
            Self::JavaUtilResourceBundle => "getJavaUtilResourceBundleAccess",
        }
    }

    /// Full set of WP1.4-covered interfaces.  Used by the
    /// native-builtins registration loop and by the
    /// `apps/sharedsecrets_probe` integration test.
    pub fn all() -> &'static [Self] {
        &[
            Self::JavaLang,
            Self::JavaLangInvoke,
            Self::JavaLangRef,
            Self::JavaLangReflect,
            Self::JavaIO,
            Self::JavaIORandomAccessFile,
            Self::JavaNetInetAddress,
            Self::JavaNetUri,
            Self::JavaNio,
            Self::JavaSecurity,
            Self::JavaUtilJar,
            Self::JavaUtilZipFile,
            Self::JavaNetHttpCookie,
            Self::JavaObjectInputStream,
            Self::JavaUtilResourceBundle,
        ]
    }
}

/// WP1.4 — Concrete-class mapping for the 15 Access interfaces
/// bridged by the SharedSecrets shim.
///
/// A native-builtins startup hook can iterate this slice to
/// (a) pre-register every concrete class with `ClassManager`
///     as a 1-slot synthetic,
/// (b) wire up the `SharedSecrets.getXxxAccess()` factories,
/// (c) confirm the per-interface method natives are registered.
pub static SHARED_SECRETS_OWNERS: &[SharedSecretsInterface] = &[
    SharedSecretsInterface::JavaLang,
    SharedSecretsInterface::JavaLangInvoke,
    SharedSecretsInterface::JavaLangRef,
    SharedSecretsInterface::JavaLangReflect,
    SharedSecretsInterface::JavaIO,
    SharedSecretsInterface::JavaIORandomAccessFile,
    SharedSecretsInterface::JavaNetInetAddress,
    SharedSecretsInterface::JavaNetUri,
    SharedSecretsInterface::JavaNio,
    SharedSecretsInterface::JavaSecurity,
    SharedSecretsInterface::JavaUtilJar,
    SharedSecretsInterface::JavaUtilZipFile,
    SharedSecretsInterface::JavaNetHttpCookie,
    SharedSecretsInterface::JavaObjectInputStream,
    SharedSecretsInterface::JavaUtilResourceBundle,
];

/// Per-singleton cache for the lazily allocated owner objects.
///
/// Keyed by interface enum; the stored `ObjectRef` is a heap
/// allocation into the owner class whose fields are all zero /
/// null.  Because interface dispatch resolves through the
/// receiver's concrete class, any method registered on
/// [`SharedSecretsInterface::owner_class`] is reachable from the
/// singleton.  The cache lives inside the native-builtins crate
/// (process-wide `OnceLock`) so we don't need to thread a cache
/// through `NativeContext`.
pub struct SharedSecretsRegistry {
    cached: parking_lot::Mutex<rustc_hash::FxHashMap<SharedSecretsInterface, ObjectRef>>,
}

impl Default for SharedSecretsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedSecretsRegistry {
    pub fn new() -> Self {
        Self {
            cached: parking_lot::Mutex::new(rustc_hash::FxHashMap::default()),
        }
    }

    /// Return the cached singleton for `iface`, if any.
    pub fn get(&self, iface: SharedSecretsInterface) -> Option<ObjectRef> {
        self.cached.lock().get(&iface).copied()
    }

    /// Install `obj` as the singleton for `iface`.  Returns the
    /// cached reference (which may differ from `obj` if another
    /// thread raced us — use the returned ref as the authoritative
    /// singleton).
    pub fn install(&self, iface: SharedSecretsInterface, obj: ObjectRef) -> ObjectRef {
        let mut guard = self.cached.lock();
        *guard.entry(iface).or_insert(obj)
    }

    /// Reset the cache.  Used only by tests that recreate a VM in
    /// the same process — production code should never call this.
    pub fn reset(&self) {
        self.cached.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_interface_has_distinct_owner_class() {
        let mut seen = std::collections::HashSet::new();
        for iface in SharedSecretsInterface::all() {
            let owner = iface.owner_class();
            assert!(
                seen.insert(owner),
                "{:?} owner class {owner} collides with a prior interface",
                iface
            );
        }
        assert_eq!(seen.len(), 15, "expected 15 unique owner classes");
    }

    #[test]
    fn every_interface_has_distinct_factory_method() {
        let mut seen = std::collections::HashSet::new();
        for iface in SharedSecretsInterface::all() {
            let factory = iface.factory_method();
            assert!(
                seen.insert(factory),
                "{:?} factory method {factory} collides",
                iface
            );
        }
    }

    #[test]
    fn factory_method_matches_canonical_shape() {
        // Every factory name must start with `getJava` and end with
        // `Access`, mirroring the HotSpot convention.
        for iface in SharedSecretsInterface::all() {
            let m = iface.factory_method();
            assert!(
                m.starts_with("getJava") && m.ends_with("Access"),
                "{m} does not match getJava*Access"
            );
        }
    }

    #[test]
    fn all_contains_exactly_fifteen_entries() {
        assert_eq!(SharedSecretsInterface::all().len(), 15);
        assert_eq!(SHARED_SECRETS_OWNERS.len(), 15);
    }
}
