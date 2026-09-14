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
    /// `jdk.internal.access.JavaIOFileDescriptorAccess` — `set`, `get`,
    /// `setAppend`, `getAppend`, `close`, `registerCleanup`,
    /// `unregisterCleanup`, `setHandle`, `getHandle`.
    ///
    /// F24-1 (2026-08-13): ADDED. This variant was missing while
    /// `shared_secrets_bridge.rs`'s `FACTORIES` carried
    /// `getJavaIOFileDescriptorAccess` — the concrete half of the
    /// two-lists-of-equal-length divergence described on [`all`]. Verified
    /// present on Microsoft 25.0.3+9-LTS, all three parts:
    ///
    /// ```text
    /// $ javap -p jdk.internal.access.JavaIOFileDescriptorAccess
    /// public interface jdk.internal.access.JavaIOFileDescriptorAccess { ... }
    /// $ javap -p 'java.io.FileDescriptor$1'
    /// class java.io.FileDescriptor$1 implements jdk.internal.access.JavaIOFileDescriptorAccess
    /// $ javap -p jdk.internal.access.SharedSecrets | grep getJavaIOFileDescriptorAccess
    ///   public static ...JavaIOFileDescriptorAccess getJavaIOFileDescriptorAccess();
    /// ```
    ///
    /// Note the contrast with the deleted `JavaSecurity`: this owner class is a
    /// REAL JDK anonymous class that really implements the interface, so the
    /// bridge's stand-ins here shadow live bytecode rather than fabricating a
    /// receiver.
    JavaIOFileDescriptor,
    /// `jdk.internal.access.JavaNetInetAddressAccess` —
    /// `getHostFromNameService`, `getOriginalHostName`.
    JavaNetInetAddress,
    /// `jdk.internal.access.JavaNetUriAccess` — `create`.
    JavaNetUri,
    /// `jdk.internal.access.JavaNioAccess` — `getBufferPool`,
    /// `newDirectByteBuffer`, `acquireSession`.
    JavaNio,
    // F24-1 (2026-08-13): `JavaSecurity` USED TO BE HERE AND IS GONE. Do not
    // re-add it. JEP 486 removed the Security Manager and took
    // `jdk.internal.access.JavaSecurityAccess` with it, so there is no
    // differently-spelled member to correct this to — the whole interface is
    // absent, not just the getter. Measured on Microsoft 25.0.3+9-LTS:
    //
    //     $ javap -p jdk.internal.access.JavaSecurityAccess
    //     Error: class not found: jdk.internal.access.JavaSecurityAccess
    //     $ javap -p jdk.internal.access.SharedSecrets | grep -c getJavaSecurityAccess
    //     0
    //
    // Its `owner_class` was fabricated too: `javap -p java.security.AccessController$1`
    // answers `class not found` on the same image.
    //
    // `getJavaxSecurityAccess`, `getJavaSecuritySpecAccess`,
    // `getJavaSecuritySignatureAccess` and `getJavaSecurityPropertiesAccess` ARE
    // all on the JDK 25 surface and are near-misses in a name-keyed search. None
    // is a rename of this one.
    //
    // The matching deletion in `native-builtins/src/shared_secrets_bridge.rs`
    // is F17-1, landed 2026-08-13.
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
            Self::JavaIORandomAccessFile => "jdk/internal/access/JavaIORandomAccessFileAccess",
            Self::JavaIOFileDescriptor => "jdk/internal/access/JavaIOFileDescriptorAccess",
            Self::JavaNetInetAddress => "jdk/internal/access/JavaNetInetAddressAccess",
            Self::JavaNetUri => "jdk/internal/access/JavaNetUriAccess",
            Self::JavaNio => "jdk/internal/access/JavaNioAccess",
            Self::JavaUtilJar => "jdk/internal/access/JavaUtilJarAccess",
            Self::JavaUtilZipFile => "jdk/internal/access/JavaUtilZipFileAccess",
            Self::JavaNetHttpCookie => "jdk/internal/access/JavaNetHttpCookieAccess",
            Self::JavaObjectInputStream => "jdk/internal/access/JavaObjectInputStreamAccess",
            Self::JavaUtilResourceBundle => "jdk/internal/access/JavaUtilResourceBundleAccess",
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
    ///
    /// # F33-1 (2026-08-13): this column had never been checked, and now is
    ///
    /// F24-1 verified all fifteen [`factory_method`](Self::factory_method)
    /// spellings against `javap -p jdk.internal.access.SharedSecrets`. It did
    /// not check the pairing this map encodes, which is a *different* claim and
    /// a stronger one: not "the owner class exists" but "the owner class
    /// implements THE INTERFACE this variant names". A `$N` index is exactly the
    /// kind of thing that is right by luck — `java.nio.Buffer$1` is the IOOBE
    /// formatter and `Buffer$2` is the access impl, which
    /// [`tests::java_nio_owner_is_access_impl_not_formatter`] records as a
    /// mistake already made once here.
    ///
    /// `javap -p` prints the `implements` clause, so the pairing is directly
    /// readable rather than inferred. All eleven JDK-namespaced owners in this
    /// map, on Microsoft **25.0.3+9-LTS** (`javap -p '<name>' | sed -n 2p`):
    ///
    /// ```text
    ///   java.lang.System$1                  implements JavaLangAccess              OK
    ///   java.lang.invoke.MethodHandleImpl$1 implements JavaLangInvokeAccess        OK
    ///   java.lang.ref.Reference$1           implements JavaLangRefAccess           OK
    ///   java.lang.reflect.ReflectAccess     implements JavaLangReflectAccess       OK  (final class, not a $N)
    ///   java.io.Console$1                   implements JavaIOAccess                OK
    ///   java.io.FileDescriptor$1            implements JavaIOFileDescriptorAccess  OK
    ///   java.net.InetAddress$1              implements JavaNetInetAddressAccess    OK
    ///   java.nio.Buffer$2                   implements JavaNioAccess               OK
    ///   java.util.zip.ZipFile$1             implements JavaUtilZipFileAccess       OK
    ///   java.util.ResourceBundle$1          implements JavaUtilResourceBundleAccess OK
    ///   java.io.ObjectInputStream$1         class not found                        ABSENT
    /// ```
    ///
    /// Ten of eleven pair correctly, and the eleventh is the documented
    /// asymmetry: JDK 25 builds `JavaObjectInputStreamAccess` with an
    /// `invokedynamic` (`ObjectInputStream.java:4039`,
    /// `setJavaObjectInputStreamAccess(ObjectInputStream::checkArray)`), so there
    /// is no anonymous class and the bridge deliberately does not intercept the
    /// getter. Run with a negative control
    /// (`javap -p java.lang.NoSuchClassAtAllXyz` → `class not found`) so that
    /// "absent" is distinguishable from "javap could not see the module"; all
    /// fifteen `jdk.internal.access.Java*Access` interfaces are PRESENT on the
    /// same image, which is the positive control for the same question.
    ///
    /// **The four `cratonvm/internal/ss/…$1` names cannot be checked this way
    /// and are not a gap in this sweep — they are the finding.** No JDK declares
    /// a `cratonvm/` class, so the pairing question becomes "do the natives
    /// registered on the stand-in implement the interface the factory's return
    /// descriptor promises", and measured against `javap -p` the answer is 0/1
    /// for `JavaIORandomAccessFileAccess` (registered `open`/`openAsChannel`;
    /// declared `openAndDelete`), 0/2 for `JavaNetHttpCookieAccess` (registered
    /// `parseCookie`; declared `parse`, `header`), 2/5 for `JavaUtilJarAccess`
    /// and 1/1 for `JavaNetUriAccess`. `--jdk-only` now refuses those four
    /// accessors outright rather than returning a carrier no `invokeinterface`
    /// can hit; see `register_factories` in
    /// `native-builtins/src/shared_secrets_bridge.rs` and
    /// docs/known-issues/jdk-only/F33-1-a-factory-and-its-owner-must-share-one-kind-20260813.md
    pub fn owner_class(&self) -> &'static str {
        match self {
            Self::JavaLang => "java/lang/System$1",
            Self::JavaLangInvoke => "java/lang/invoke/MethodHandleImpl$1",
            Self::JavaLangRef => "java/lang/ref/Reference$1",
            Self::JavaLangReflect => "java/lang/reflect/ReflectAccess",
            Self::JavaIO => "java/io/Console$1",
            Self::JavaIORandomAccessFile => "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1",
            Self::JavaIOFileDescriptor => "java/io/FileDescriptor$1",
            Self::JavaNetInetAddress => "java/net/InetAddress$1",
            Self::JavaNetUri => "cratonvm/internal/ss/JavaNetUriAccess$1",
            Self::JavaNio => "java/nio/Buffer$2",
            Self::JavaUtilJar => "cratonvm/internal/ss/JavaUtilJarAccess$1",
            Self::JavaUtilZipFile => "java/util/zip/ZipFile$1",
            Self::JavaNetHttpCookie => "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
            Self::JavaObjectInputStream => "java/io/ObjectInputStream$1",
            Self::JavaUtilResourceBundle => "java/util/ResourceBundle$1",
        }
    }

    /// The `SharedSecrets` static that hands out this interface's singleton —
    /// the method name used when we register the native.
    ///
    /// **Not uniformly `getJavaXxxAccess`.** `JavaUtilJar`'s accessor is spelled
    /// `javaUtilJarAccess`, with no `get` prefix; the *setter* is
    /// `setJavaUtilJarAccess`, which is how the `get` form got invented here and
    /// in the bridge. Measured on Microsoft 25.0.3+9-LTS:
    ///
    /// ```text
    /// $ javap -p jdk.internal.access.SharedSecrets | grep JarAccess
    ///   private static jdk.internal.access.JavaUtilJarAccess javaUtilJarAccess;
    ///   public static jdk.internal.access.JavaUtilJarAccess javaUtilJarAccess();
    ///   public static void setJavaUtilJarAccess(jdk.internal.access.JavaUtilJarAccess);
    /// ```
    ///
    /// So do not "restore consistency" here, and do not add a
    /// `starts_with("getJava")` assertion over this map: an earlier one existed,
    /// and because every name had been written to that shape it checked the list
    /// against itself while actively punishing the one correct spelling. See
    /// [`factory_method_matches_the_jdk25_surface`].
    pub fn factory_method(&self) -> &'static str {
        match self {
            Self::JavaLang => "getJavaLangAccess",
            Self::JavaLangInvoke => "getJavaLangInvokeAccess",
            Self::JavaLangRef => "getJavaLangRefAccess",
            Self::JavaLangReflect => "getJavaLangReflectAccess",
            Self::JavaIO => "getJavaIOAccess",
            Self::JavaIORandomAccessFile => "getJavaIORandomAccessFileAccess",
            Self::JavaIOFileDescriptor => "getJavaIOFileDescriptorAccess",
            Self::JavaNetInetAddress => "getJavaNetInetAddressAccess",
            Self::JavaNetUri => "getJavaNetUriAccess",
            Self::JavaNio => "getJavaNioAccess",
            // No `get` prefix — see this method's doc comment.
            Self::JavaUtilJar => "javaUtilJarAccess",
            Self::JavaUtilZipFile => "getJavaUtilZipFileAccess",
            Self::JavaNetHttpCookie => "getJavaNetHttpCookieAccess",
            Self::JavaObjectInputStream => "getJavaObjectInputStreamAccess",
            Self::JavaUtilResourceBundle => "getJavaUtilResourceBundleAccess",
        }
    }

    /// Full set of WP1.4-covered interfaces.
    ///
    /// # This list has no runtime consumer
    ///
    /// F24-1 (2026-08-13) corrected the previous sentence here, which read
    /// "Used by the native-builtins registration loop and by the
    /// `apps/sharedsecrets_probe` integration test." **Both halves were false.**
    /// `apps/sharedsecrets_probe` does not exist anywhere in the tree, and the
    /// native-builtins registration loop iterates its own private `FACTORIES`
    /// table, not this one — nothing outside this file names
    /// `SharedSecretsInterface`, `SHARED_SECRETS_OWNERS` or
    /// `SharedSecretsRegistry`:
    ///
    /// ```text
    /// $ grep -rn 'SharedSecretsInterface\|SHARED_SECRETS_OWNERS\|SharedSecretsRegistry' \
    ///       --include=*.rs . | grep -v vm/src/runtime/shared_secrets.rs
    /// (no output)
    /// ```
    ///
    /// So this module is documentation and a mirror, and its value is entirely
    /// in being *checkable* against the thing that does run. That is what
    /// [`tests::owner_classes_mirror_the_native_builtins_bridge`] does, and why
    /// the wrong spellings here mattered even though no dispatch reads them: the
    /// bridge's own comment points a future author at this file as the canonical
    /// list.
    pub fn all() -> &'static [Self] {
        &[
            Self::JavaLang,
            Self::JavaLangInvoke,
            Self::JavaLangRef,
            Self::JavaLangReflect,
            Self::JavaIO,
            Self::JavaIORandomAccessFile,
            Self::JavaIOFileDescriptor,
            Self::JavaNetInetAddress,
            Self::JavaNetUri,
            Self::JavaNio,
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
/// Derived from [`SharedSecretsInterface::all`] and asserted equal to it by
/// [`tests::owners_slice_equals_all`] — it is a second spelling of the same
/// list, and two hand-maintained copies is how this file drifted in the first
/// place.
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
    SharedSecretsInterface::JavaIOFileDescriptor,
    SharedSecretsInterface::JavaNetInetAddress,
    SharedSecretsInterface::JavaNetUri,
    SharedSecretsInterface::JavaNio,
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
        // Derived, not restated: a literal 15 here was one of the numbers that
        // agreed for the wrong reason while the lists' CONTENTS diverged.
        assert_eq!(seen.len(), SharedSecretsInterface::all().len());
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

    /// F24-1 (2026-08-13) — REPLACES `factory_method_matches_canonical_shape`,
    /// a guard that could not fail and that punished the one correct answer.
    ///
    /// Its body was, in full:
    ///
    /// ```ignore
    /// for iface in SharedSecretsInterface::all() {
    ///     let m = iface.factory_method();
    ///     assert!(m.starts_with("getJava") && m.ends_with("Access"));
    /// }
    /// ```
    ///
    /// Every name in the map had been *written* to that shape, so the assertion
    /// compared the list against itself. It passed on `getJavaSecurityAccess`
    /// (a member JDK 25 does not declare at all) and on `getJavaUtilJarAccess`
    /// (a misspelling of `javaUtilJarAccess`) for exactly as long as both were
    /// present, and it would have gone RED on the repair — the correct spelling
    /// has no `get` prefix.
    ///
    /// The replacement inverts it: the JDK's departure from its own convention
    /// is the thing pinned, by name. It fails if someone "restores consistency"
    /// (the exception list empties) and it fails if a new non-conforming name
    /// appears unreviewed (the list grows).
    #[test]
    fn factory_method_matches_the_jdk25_surface() {
        let departures: Vec<&str> = SharedSecretsInterface::all()
            .iter()
            .map(|i| i.factory_method())
            .filter(|m| !(m.starts_with("getJava") && m.ends_with("Access")))
            .collect();
        assert_eq!(
            departures,
            vec!["javaUtilJarAccess"],
            "exactly one JDK 25 SharedSecrets accessor departs from the \
             getJava*Access convention. If this list is EMPTY someone has \
             re-broken `javaUtilJarAccess` by adding a `get` prefix the JDK \
             does not have (`javap -p jdk.internal.access.SharedSecrets | \
             grep JarAccess`). If it has GROWN, verify the new name against \
             `javap -p` before widening this assertion."
        );
    }

    /// The count is derived, not restated: [`SHARED_SECRETS_OWNERS`] is a second
    /// hand-written spelling of [`SharedSecretsInterface::all`], and the two
    /// drifting is the failure this file already had once.
    ///
    /// F24-1 replaces `all_contains_exactly_fifteen_entries`, which asserted
    /// `all().len() == 15` and `SHARED_SECRETS_OWNERS.len() == 15`
    /// *independently*. Two length checks against a literal cannot see contents,
    /// which is precisely how a 15-entry `all()` containing
    /// `JavaObjectInputStream` coexisted with a 15-entry bridge table containing
    /// `JavaIOFileDescriptor` instead.
    #[test]
    fn owners_slice_equals_all() {
        assert_eq!(
            SHARED_SECRETS_OWNERS,
            SharedSecretsInterface::all(),
            "SHARED_SECRETS_OWNERS and all() are the same list written twice \
             and have diverged"
        );
    }

    /// **Task-2 check: the one that crosses the crate boundary.**
    ///
    /// The `native-builtins` doc comment above its `FACTORIES` table has claimed
    /// since WP1.4 that "a compile-time `#[test]` in the vm crate asserts the
    /// two lists stay in sync". No such test existed. The two lists had drifted
    /// in *both* directions while both held fifteen entries, so every
    /// length-based or count-based guard on either side passed:
    ///
    /// | | `all()` (this file) | `FACTORIES` (bridge) |
    /// |---|---|---|
    /// | `java/io/FileDescriptor$1` | absent | present |
    /// | `java/io/ObjectInputStream$1` | present | absent |
    ///
    /// F24-1 adds the missing `JavaIOFileDescriptor` variant, so one row of that
    /// table is closed. The other is a deliberate, documented asymmetry and is
    /// pinned here by name rather than papered over.
    ///
    /// # Why this can be written at all
    ///
    /// `jdk_baseline` — the external JDK-surface oracle the bridge's own guards
    /// use — is `pub(crate)` to `native-builtins`, so this crate cannot reach
    /// it. But `shared_secrets_bridge::owner_classes()` is `pub`, and `vm`
    /// already depends on `cratonvm-native-builtins`, so the bridge's live owner
    /// set IS observable from here. That projection is enough: owner class is
    /// the field the two tables disagreed on.
    #[test]
    fn owner_classes_mirror_the_native_builtins_bridge() {
        use std::collections::BTreeSet;

        let ours: BTreeSet<&'static str> = SharedSecretsInterface::all()
            .iter()
            .map(|i| i.owner_class())
            .collect();
        let bridge: BTreeSet<&'static str> =
            cratonvm_native_builtins::shared_secrets_bridge::owner_classes().collect();

        let only_bridge: Vec<&str> = bridge.difference(&ours).copied().collect();
        assert!(
            only_bridge.is_empty(),
            "native-builtins registers SharedSecrets factories handing out owner \
             classes this file does not list: {only_bridge:?}. Add a \
             SharedSecretsInterface variant for each, verifying the interface, \
             the owner class and the accessor spelling with `javap -p` first."
        );

        let only_ours: Vec<&str> = ours.difference(&bridge).copied().collect();
        assert_eq!(
            only_ours,
            vec!["java/io/ObjectInputStream$1"],
            "the ONLY interface this file may list without a matching bridge \
             factory is JavaObjectInputStream. JDK 25 builds that access object \
             with an `invokedynamic` in `ObjectInputStream.<clinit>` and has no \
             `ObjectInputStream$1`, so the bridge deliberately does not \
             intercept the getter — see the NOTE at that entry in \
             `native-builtins/src/shared_secrets_bridge.rs`. Any other name \
             here is drift, which is what this test exists to catch."
        );
    }

    #[test]
    fn java_nio_owner_is_access_impl_not_formatter() {
        // JDK 25's `java.nio.Buffer$1` is the IOOBE formatter Function.
        // `JavaNioAccess` is implemented by `Buffer$2`; using the formatter as
        // the SharedSecrets singleton makes calls such as scaleShifts(Buffer)
        // dispatch to the wrong receiver class.
        assert_eq!(
            SharedSecretsInterface::JavaNio.owner_class(),
            "java/nio/Buffer$2"
        );
    }
}
