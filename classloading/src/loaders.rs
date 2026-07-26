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
