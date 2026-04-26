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
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
    )
)]

use crate::class::ClassLoaderId;
use crate::class_path::ClassPath;
use rustjvm_types::error::ClassFileError;

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
    fn find_class_bytes(&self, class_name: &str) -> Result<Vec<u8>, ClassFileError>;

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
}

impl ClassFinder for BootstrapClassFinder {
    fn loader_id(&self) -> ClassLoaderId {
        ClassLoaderId::Bootstrap
    }
    fn find_class_bytes(&self, class_name: &str) -> Result<Vec<u8>, ClassFileError> {
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
    fn find_class_bytes(&self, class_name: &str) -> Result<Vec<u8>, ClassFileError> {
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
    fn find_class_bytes(&self, class_name: &str) -> Result<Vec<u8>, ClassFileError> {
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
}
