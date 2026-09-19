// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Built-in class loaders: bootstrap, extension, and application.
//!
//! Each loader wraps a [`ClassPath`] and is identified by a [`ClassLoaderId`].
//! The [`ClassFinder`] trait provides a uniform interface for locating class
//! bytecode; the [`ClassManager`](super::ClassManager) orchestrates parsing,
//! superclass loading, and registration.
//
// T1.8.2 — production-code panic gate. Class loading is on the
// startup-critical path; a panic here would terminate the entire VM.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,)
)]

use crate::class::ClassLoaderId;
use crate::class_path::ClassPath;
use cratonvm_reader::SharedBytes;
use cratonvm_types::error::ClassFileError;

/// Round 5 audit fix (LOW #11) / Round 7 carry-over: the built-in
/// loader hierarchy is fixed at process start
/// (`Bootstrap → Extension → Application`). On every class-load miss
/// the previous implementation walked the chain via trait dispatch
/// (`bootstrap.find_class_bytes` → `extension.find_class_bytes` →
/// `application.find_class_bytes`), which on a deep miss is 3 vtable
/// calls + 3 inner `ClassPath::find_class` probes. Flattening into a
/// pre-computed `&[ClassLoaderId]` lets callers walk a slice instead
/// of chasing pointers through a parent chain.
///
/// For the built-in loaders this is purely a code-clarity win — the
/// `find_class_bytes_delegated` path in `class_manager.rs` is already
/// flat (4 sequential `if let Ok(...)` arms). User-defined loaders
/// (`ClassLoaderId::UserDefined`) have their parent chains modelled
/// on the **Java side** (the `parent` field of `java.lang.ClassLoader`)
/// — the Rust side never observes a deep parent walk for those
/// because `ClassLoader.loadClass` is implemented in Java and the
/// native code only sees the bottom-most `defineClass` invocation.
///
/// Returned in delegation order: callers should probe each loader in
/// order and return the first hit (standard parent-delegation model).
pub const BUILTIN_LOADER_DELEGATION_CHAIN: &[ClassLoaderId] = &[
    ClassLoaderId::Bootstrap,
    ClassLoaderId::Extension,
    ClassLoaderId::Application,
];

/// Round 5 audit fix (LOW #11): the maximum parent-chain depth the
/// VM's built-in loader hierarchy ever walks. Used by callers that
/// pre-size scratch buffers (`SmallVec<[ClassLoaderId; 4]>`) for the
/// flat walk.
pub const MAX_BUILTIN_LOADER_DEPTH: usize = 3;

// ---------------------------------------------------------------------------
// User-defined loader parent chains
// ---------------------------------------------------------------------------

/// Parent namespace for each user-defined loader namespace id.
///
/// [`BUILTIN_LOADER_DELEGATION_CHAIN`]'s doc comment above says user loaders'
/// parent chains "are modelled on the Java side ... the Rust side never
/// observes a deep parent walk for those". That is true for *finding bytes* —
/// `ClassLoader.loadClass` is Java and the native code only sees the
/// bottom-most `defineClass`. It was never true for **resolution against
/// already-defined classes**, which is entirely a Rust-side operation over the
/// flat `loaded_classes` map: with no parent link recorded, a class defined by
/// `UserDefined(7)` could only resolve a supertype or constant-pool name
/// against its own namespace or the built-in chain (Bootstrap -> Extension ->
/// Application). A name defined by its *parent* `UserDefined(3)` was invisible,
/// so resolution fell through to the loader-blind global path — and for a name
/// that also exists on the application classpath that path DEFINES A SECOND
/// COPY in the application namespace.
///
/// That is the mechanism behind the Spring AOT `argument type mismatch`
/// family: Spring's `TestCompiler` `DynamicClassLoader` (a child of
/// `@CompileWithForkedClassLoader`'s fork loader) defines a CGLIB proxy, whose
/// superclass name resolved to a freshly-minted application-loader copy instead
/// of the fork's, so `Field.set` correctly rejected the proxy as unrelated to
/// the field's declared, fork-loaded type.
///
/// Written by `native-builtins`' loader-namespace allocator — the only place
/// that can see the Java `ClassLoader.parent` field — and read by the
/// resolution paths in `class_manager`. A parent of `0` is stored too, and
/// means "delegates to the built-in chain"; it is what stops the writer from
/// re-walking the Java parent field on every call.
static USER_LOADER_PARENTS: std::sync::OnceLock<
    std::sync::RwLock<crate::fx_hash::FxHashMap<u32, u32>>,
> = std::sync::OnceLock::new();

/// Whether [`USER_LOADER_PARENTS`] has any entry at all, so the read path can
/// skip the `RwLock` in a process with no user-defined loaders.
static USER_LOADER_PARENTS_NONEMPTY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The deepest parent chain [`user_loader_ancestors`] reports. Real chains are
/// short (Tomcat's is 3, Spring's AOT fork 2); the cap keeps the walk
/// allocation-free and bounds a cycle introduced by a mis-registration.
pub const MAX_USER_LOADER_DEPTH: usize = 8;

fn user_loader_parents() -> &'static std::sync::RwLock<crate::fx_hash::FxHashMap<u32, u32>> {
    USER_LOADER_PARENTS.get_or_init(|| std::sync::RwLock::new(Default::default()))
}

/// Record `child_ns`'s delegation parent (`0` = the built-in chain).
///
/// Idempotent, and deliberately last-writer-wins: a `ClassLoader`'s parent is
/// fixed at construction, so a second call can only be re-registering the same
/// link or upgrading a `0` recorded before the parent had a namespace of its
/// own.
pub fn register_user_loader_parent(child_ns: u32, parent_ns: u32) {
    if child_ns < 3 || child_ns == parent_ns {
        return;
    }
    let mut map = match user_loader_parents().write() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    map.insert(child_ns, parent_ns);
    USER_LOADER_PARENTS_NONEMPTY.store(true, std::sync::atomic::Ordering::Release);
}

/// True once any user-loader parent link has been registered.
#[inline]
pub fn has_user_loader_parents() -> bool {
    USER_LOADER_PARENTS_NONEMPTY.load(std::sync::atomic::Ordering::Acquire)
}

/// True when `child_ns`'s parent has already been resolved and recorded (even
/// as `0`). The writer uses this to avoid re-walking the Java `parent` field on
/// every namespace-id lookup.
pub fn user_loader_parent_known(child_ns: u32) -> bool {
    if !has_user_loader_parents() || child_ns < 3 {
        return false;
    }
    match user_loader_parents().read() {
        Ok(g) => g.contains_key(&child_ns),
        Err(e) => e.into_inner().contains_key(&child_ns),
    }
}

/// Write `ns`'s delegation ancestors into `out`, nearest parent first,
/// excluding `ns` itself and the built-in chain, and return how many were
/// written. Stops at a namespace with no registered (or a built-in) parent, on
/// a repeat (cycle), or at [`MAX_USER_LOADER_DEPTH`].
pub fn user_loader_ancestors(ns: u32, out: &mut [u32; MAX_USER_LOADER_DEPTH]) -> usize {
    if !has_user_loader_parents() || ns < 3 {
        return 0;
    }
    let map = match user_loader_parents().read() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let mut cur = ns;
    let mut n = 0;
    while n < MAX_USER_LOADER_DEPTH {
        let Some(&parent) = map.get(&cur) else { break };
        if parent < 3 || parent == ns || out[..n].contains(&parent) {
            break;
        }
        out[n] = parent;
        n += 1;
        cur = parent;
    }
    n
}

/// The built-in loader (`0`=Bootstrap, `1`=Extension/Platform, `2`=Application)
/// `ns`'s delegation chain terminates at, if that chain has been fully
/// recorded.
///
/// `user_loader_ancestors` walks the same `USER_LOADER_PARENTS` chain but
/// discards exactly this fact once it reaches a value `< 3` — it just stops.
/// A caller that then falls back to "try the whole built-in chain" loses the
/// distinction a registered `Extension`/platform parent draws: a
/// `ModifiedClassPathClassLoader` (parent = platform, specifically to exclude
/// Application from delegation, e.g. Spring's `@ClassPathExclusions`) has a
/// terminal parent of `1`, and probing `Application` anyway resolves a
/// same-named class through the wrong loader — the same defect class as the
/// `PropertySource`/`EnumerablePropertySource` cross-loader
/// `ClassCastException` family this function was added to close.
///
/// `None` means "unrecorded, or the chain did not bottom out within
/// `MAX_USER_LOADER_DEPTH`" — callers must keep probing the full built-in
/// chain in that case; only a POSITIVELY recorded terminal may narrow it.
pub fn user_loader_builtin_parent(ns: u32) -> Option<u32> {
    if !has_user_loader_parents() || ns < 3 {
        return None;
    }
    let map = match user_loader_parents().read() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let mut cur = ns;
    let mut seen = 0usize;
    while seen < MAX_USER_LOADER_DEPTH {
        let &parent = map.get(&cur)?;
        if parent < 3 {
            return Some(parent);
        }
        if parent == ns {
            return None;
        }
        cur = parent;
        seen += 1;
    }
    None
}

/// `CRATONVM_LOADER_PARENT_CHAIN` gate (default ON). Off (`0` or empty)
/// restores the pre-2026-07-30 behaviour where a user loader's resolution saw
/// only its own namespace and the built-in chain. Kept as an escape hatch for
/// bisecting a regression to this change; read once and cached.
pub fn loader_parent_chain_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_LOADER_PARENT_CHAIN") {
            Ok(v) => !(v.is_empty() || v == "0"),
            Err(_) => true,
        },
    )
}

/// `CRATONVM_DBG_LOADER_CHAIN=1` — print one line per supertype a user loader
/// resolved through, or failed to resolve through, its parent chain. The
/// `argument type mismatch` family is otherwise invisible until it surfaces as
/// two ClassIds inside a `Field.set` coercion guard.
pub fn dbg_loader_chain() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_LOADER_CHAIN")
            .map(|v| !(v.is_empty() || v == "0"))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod user_loader_parent_tests {
    use super::*;

    // The registry is process-global, so these use ids far above anything a
    // real run allocates and assert only about their own keys.
    #[test]
    fn ancestors_walk_nearest_first_and_stop_at_the_builtin_chain() {
        register_user_loader_parent(9001, 9002);
        register_user_loader_parent(9002, 9003);
        register_user_loader_parent(9003, 0);
        let mut out = [0u32; MAX_USER_LOADER_DEPTH];
        let n = user_loader_ancestors(9001, &mut out);
        assert_eq!(&out[..n], &[9002, 9003]);
        assert!(user_loader_parent_known(9003));
        assert!(!user_loader_parent_known(9004));
    }

    #[test]
    fn a_cycle_terminates() {
        register_user_loader_parent(9101, 9102);
        register_user_loader_parent(9102, 9101);
        let mut out = [0u32; MAX_USER_LOADER_DEPTH];
        let n = user_loader_ancestors(9101, &mut out);
        assert_eq!(&out[..n], &[9102]);
    }

    #[test]
    fn builtin_and_self_parents_are_not_walked() {
        register_user_loader_parent(9201, 9201);
        let mut out = [0u32; MAX_USER_LOADER_DEPTH];
        assert_eq!(user_loader_ancestors(9201, &mut out), 0);
        assert_eq!(user_loader_ancestors(2, &mut out), 0);
    }
}

/// Trait for class loaders that can locate class bytecode.
///
/// This trait is deliberately simpler than `java.lang.ClassLoader`: it only
/// finds raw bytes. The `ClassManager` orchestrates parsing, superclass
/// loading, verification, and registration. The name `ClassFinder` is used
/// to reserve `ClassLoader` for the Java-side class (Phase 4.5 / Phase 8).
pub trait ClassFinder: std::fmt::Debug {
    /// The loader identity used to tag loaded classes.
    fn loader_id(&self) -> ClassLoaderId;

    /// Attempt to find the bytecode for a class by its binary name
    /// (e.g. `"java/lang/Object"`).
    ///
    /// Returns `Ok(bytes)` if found, `Err(ClassNotFound)` if not in this
    /// loader's search space.
    fn find_class_bytes(&self, class_name: &str) -> Result<SharedBytes, ClassFileError>;

    /// Human-readable name for logging.
    fn name(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Bootstrap class finder
// ---------------------------------------------------------------------------

/// Bootstrap class loader — loads from rt.jar and the boot classpath.
///
/// In the JVM spec, the bootstrap class loader is represented by `null`.
/// It has no parent.
#[derive(Debug)]
pub struct BootstrapClassFinder {
    class_path: ClassPath,
}

impl BootstrapClassFinder {
    pub fn new(boot_classpath: &[String]) -> Self {
        Self {
            class_path: ClassPath::new(boot_classpath),
        }
    }
    pub fn class_path(&self) -> &ClassPath {
        &self.class_path
    }
    /// Append a path (jar or directory) to the bootstrap search path.
    /// Used by `Instrumentation.appendToBootstrapClassLoaderSearch` so a
    /// dynamically-attached agent (e.g. Mockito's inline mock maker) can
    /// inject helper classes that MUST be visible to the bootstrap loader.
    pub fn add_path(&mut self, path: &str) {
        self.class_path.add_path(path);
    }
}

impl ClassFinder for BootstrapClassFinder {
    fn loader_id(&self) -> ClassLoaderId {
        ClassLoaderId::Bootstrap
    }
    fn find_class_bytes(&self, class_name: &str) -> Result<SharedBytes, ClassFileError> {
        self.class_path.find_class(class_name)
    }
    fn name(&self) -> &str {
        "bootstrap"
    }
}

// ---------------------------------------------------------------------------
// Extension class finder
// ---------------------------------------------------------------------------

/// Extension class loader — loads from `$JAVA_HOME/lib/ext`.
///
/// Parent: bootstrap.
#[derive(Debug)]
pub struct ExtensionClassFinder {
    class_path: ClassPath,
}

impl ExtensionClassFinder {
    pub fn new(ext_classpath: &[String]) -> Self {
        Self {
            class_path: ClassPath::new(ext_classpath),
        }
    }
    pub fn class_path(&self) -> &ClassPath {
        &self.class_path
    }
}

impl ClassFinder for ExtensionClassFinder {
    fn loader_id(&self) -> ClassLoaderId {
        ClassLoaderId::Extension
    }
    fn find_class_bytes(&self, class_name: &str) -> Result<SharedBytes, ClassFileError> {
        self.class_path.find_class(class_name)
    }
    fn name(&self) -> &str {
        "extension"
    }
}

// ---------------------------------------------------------------------------
// Application class finder
// ---------------------------------------------------------------------------

/// Application class loader — loads from the user classpath (`-cp`).
///
/// Parent: extension.
#[derive(Debug)]
pub struct ApplicationClassFinder {
    class_path: ClassPath,
}

impl ApplicationClassFinder {
    pub fn new(classpath: &[String]) -> Self {
        Self {
            class_path: ClassPath::new(classpath),
        }
    }
    pub fn class_path(&self) -> &ClassPath {
        &self.class_path
    }
    /// Add a runtime path (for URLClassLoader / dynamic class loading).
    pub fn add_path(&mut self, path: &str) {
        self.class_path.add_path(path);
    }
    /// Retract a runtime path added by [`Self::add_path`] — `URLClassLoader
    /// .close()`. See [`ClassPath::remove_path`] for the use-count rule that
    /// keeps one loader's close from breaking another's still-open loader.
    pub fn remove_path(&mut self, path: &str) -> usize {
        self.class_path.remove_path(path)
    }
}

impl ClassFinder for ApplicationClassFinder {
    fn loader_id(&self) -> ClassLoaderId {
        ClassLoaderId::Application
    }
    fn find_class_bytes(&self, class_name: &str) -> Result<SharedBytes, ClassFileError> {
        self.class_path.find_class(class_name)
    }
    fn name(&self) -> &str {
        "application"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_loader_id() {
        let loader = BootstrapClassFinder::new(&[]);
        assert_eq!(loader.loader_id(), ClassLoaderId::Bootstrap);
        assert_eq!(loader.name(), "bootstrap");
    }

    #[test]
    fn extension_loader_id() {
        let loader = ExtensionClassFinder::new(&[]);
        assert_eq!(loader.loader_id(), ClassLoaderId::Extension);
        assert_eq!(loader.name(), "extension");
    }

    #[test]
    fn application_loader_id() {
        let loader = ApplicationClassFinder::new(&[]);
        assert_eq!(loader.loader_id(), ClassLoaderId::Application);
        assert_eq!(loader.name(), "application");
    }

    #[test]
    fn empty_loader_finds_nothing() {
        let loader = BootstrapClassFinder::new(&[]);
        assert!(loader.find_class_bytes("java/lang/Object").is_err());
    }

    /// Round 5 audit fix (LOW #11): the delegation chain is the
    /// canonical parent-delegation order; callers (the class-manager
    /// `find_class_bytes_delegated` path) walk this slice instead of
    /// chasing trait-object parent pointers.
    #[test]
    fn delegation_chain_is_bootstrap_extension_application() {
        assert_eq!(
            BUILTIN_LOADER_DELEGATION_CHAIN,
            &[
                ClassLoaderId::Bootstrap,
                ClassLoaderId::Extension,
                ClassLoaderId::Application,
            ]
        );
        assert_eq!(
            BUILTIN_LOADER_DELEGATION_CHAIN.len(),
            MAX_BUILTIN_LOADER_DEPTH
        );
    }
}
