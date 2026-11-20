// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Class and class-loader identity types.
//!
//! Defines [`ClassLoaderId`], which names the class loader (bootstrap,
//! extension, application, or a user-defined loader) that forms half of a
//! class's JVM-spec runtime identity — the pair (defining loader, fully
//! qualified name).

use std::fmt;

/// Identifies which class loader loaded a class.
///
/// In the JVM spec, a class's runtime identity is (defining loader, fully qualified name).
/// This enum covers the three built-in loaders plus user-defined loaders
/// (Phase 23.2: ClassLoader.defineClass support).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClassLoaderId {
    /// The bootstrap class loader (null in Java). Loads from rt.jar / boot classpath.
    Bootstrap,
    /// The extension/platform class loader. Loads from $JAVA_HOME/lib/ext.
    Extension,
    /// The application/system class loader. Loads from the -classpath.
    Application,
    /// A user-defined class loader. The `u32` is a unique loader id assigned at
    /// runtime. Classes defined via `ClassLoader.defineClass(byte[])` use this.
    UserDefined(u32),
}

impl ClassLoaderId {
    /// Flat `u32` wire value for the bootstrap loader.
    ///
    /// Doubles as the "caller did not specify a loader" sentinel on the
    /// native-API boundary — see [`ClassLoaderId::from_native_id_or_default`].
    pub const NATIVE_BOOTSTRAP: u32 = 0;
    /// Flat `u32` wire value for the extension/platform loader.
    pub const NATIVE_EXTENSION: u32 = 1;
    /// Flat `u32` wire value for the application/system loader.
    pub const NATIVE_APPLICATION: u32 = 2;
    /// Lowest `u32` wire value that names a genuine user-defined loader
    /// namespace. `allocate_loader_id` starts its counter here so no allocated
    /// namespace can ever alias a built-in loader's reserved id.
    pub const NATIVE_FIRST_USER_DEFINED: u32 = 3;

    /// Encode this loader id into the flat `u32` used across the
    /// `NativeContext` boundary (`NativeContext::loader_id_of_class` returns
    /// this value widened to `i32`).
    ///
    /// `Bootstrap=0, Extension=1, Application=2, UserDefined(id)=id`.
    ///
    /// [`ClassLoaderId::from_native_id`] is the exact inverse. Keep the two in
    /// lockstep: they used to be written out by hand at six separate call
    /// sites and drifted apart, which silently mistagged every CGLIB-enhanced
    /// `@Configuration` subclass as `UserDefined(2)` instead of `Application`
    /// and broke package-private override detection (see
    /// `configproxy-cglib-loaderid-fixed-20260727.md`).
    pub const fn to_native_id(self) -> u32 {
        match self {
            ClassLoaderId::Bootstrap => Self::NATIVE_BOOTSTRAP,
            ClassLoaderId::Extension => Self::NATIVE_EXTENSION,
            ClassLoaderId::Application => Self::NATIVE_APPLICATION,
            ClassLoaderId::UserDefined(id) => id,
        }
    }

    /// Exact inverse of [`ClassLoaderId::to_native_id`]: decode a flat `u32`
    /// that is known to have come from an `to_native_id` round-trip.
    ///
    /// Use this only where `0` genuinely means *bootstrap*. Most native-API
    /// entry points instead treat `0` as "unspecified — use the default
    /// loader"; those must use [`ClassLoaderId::from_native_id_or_default`].
    pub const fn from_native_id(id: u32) -> Self {
        match id {
            Self::NATIVE_BOOTSTRAP => ClassLoaderId::Bootstrap,
            Self::NATIVE_EXTENSION => ClassLoaderId::Extension,
            Self::NATIVE_APPLICATION => ClassLoaderId::Application,
            other => ClassLoaderId::UserDefined(other),
        }
    }

    /// Decode a flat `u32` under the native-API boundary's *default-sentinel*
    /// convention: identical to [`ClassLoaderId::from_native_id`] except that
    /// `0` decodes to [`ClassLoaderId::Application`] rather than
    /// [`ClassLoaderId::Bootstrap`].
    ///
    /// This is the convention the `NativeContext` class-definition and
    /// class-lookup methods document and that their callers rely on: a caller
    /// with no particular loader in mind (`define_class_full(name, bytes, 0,
    /// ..)`) passes `0` meaning "the
    /// ordinary application/global namespace", and several such call sites
    /// exist (`native-builtins`'s hidden-class, JBoss-module and
    /// Spring-bootstrap defines, among others). Bootstrap is not otherwise
    /// reachable through those entry points, so collapsing `0` onto
    /// `Application` loses nothing in practice; what it must NOT do is what
    /// the pre-fix code did and decode `2` — a real, round-tripped
    /// `Application` id — to `UserDefined(2)`.
    pub const fn from_native_id_or_default(id: u32) -> Self {
        match id {
            // NOTE: deliberately NOT `Bootstrap` — see the doc comment.
            Self::NATIVE_BOOTSTRAP => ClassLoaderId::Application,
            other => Self::from_native_id(other),
        }
    }

    /// True when this loader is one of the three built-in loaders, i.e. its
    /// wire id is one of the reserved values below
    /// [`ClassLoaderId::NATIVE_FIRST_USER_DEFINED`].
    pub const fn is_builtin(self) -> bool {
        !matches!(self, ClassLoaderId::UserDefined(_))
    }
}

impl fmt::Display for ClassLoaderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClassLoaderId::Bootstrap => write!(f, "bootstrap"),
            ClassLoaderId::Extension => write!(f, "extension"),
            ClassLoaderId::Application => write!(f, "application"),
            ClassLoaderId::UserDefined(id) => write!(f, "user-defined({id})"),
        }
    }
}

/// A unique identifier for a loaded class within this VM.
///
/// Each class loaded by the class manager receives a unique `ClassId`.
/// Multiple class loaders may load the "same" class name and receive
/// different `ClassId`s — they are considered distinct classes.
///
/// Using a simple `u32` index avoids the lifetime complexity of holding
/// `&Class` references everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ClassId(u32);

impl ClassId {
    /// Create a new `ClassId` from a raw index.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// Return the raw u32 value.
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ClassId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests;
