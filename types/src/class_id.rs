// SPDX-License-Identifier: Apache-2.0
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
    /// ..)`, `Instrumentation.getInitiatedClasses`) passes `0` meaning "the
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
mod tests {
    use super::*;
    use std::collections::HashSet;

    // -- ClassId tests --

    #[test]
    fn class_id_new_and_as_u32() {
        let id = ClassId::new(42);
        assert_eq!(id.as_u32(), 42);
    }

    #[test]
    fn class_id_zero() {
        let id = ClassId::new(0);
        assert_eq!(id.as_u32(), 0);
    }

    #[test]
    fn class_id_max() {
        let id = ClassId::new(u32::MAX);
        assert_eq!(id.as_u32(), u32::MAX);
    }

    #[test]
    fn class_id_equality() {
        let a = ClassId::new(10);
        let b = ClassId::new(10);
        let c = ClassId::new(11);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn class_id_copy_semantics() {
        let a = ClassId::new(7);
        let b = a; // Copy
        assert_eq!(a, b); // a is still valid after copy
    }

    #[test]
    fn class_id_hash_consistency() {
        // Equal ClassIds must hash to the same value
        let mut set = HashSet::new();
        set.insert(ClassId::new(100));
        assert!(set.contains(&ClassId::new(100)));
        assert!(!set.contains(&ClassId::new(101)));
    }

    #[test]
    fn class_id_hash_uniqueness() {
        // Different ClassIds should generally produce different hashes
        let mut set = HashSet::new();
        for i in 0..1000 {
            set.insert(ClassId::new(i));
        }
        assert_eq!(set.len(), 1000);
    }

    #[test]
    fn class_id_display() {
        assert_eq!(format!("{}", ClassId::new(0)), "0");
        assert_eq!(format!("{}", ClassId::new(42)), "42");
        assert_eq!(
            format!("{}", ClassId::new(u32::MAX)),
            format!("{}", u32::MAX)
        );
    }

    #[test]
    fn class_id_debug() {
        let id = ClassId::new(5);
        let debug = format!("{:?}", id);
        assert!(debug.contains("ClassId"));
        assert!(debug.contains("5"));
    }

    #[test]
    fn class_id_const_new() {
        // Verify const fn works in const context
        const ID: ClassId = ClassId::new(999);
        assert_eq!(ID.as_u32(), 999);
    }

    // -- ClassLoaderId tests --

    #[test]
    fn class_loader_id_display() {
        assert_eq!(format!("{}", ClassLoaderId::Bootstrap), "bootstrap");
        assert_eq!(format!("{}", ClassLoaderId::Extension), "extension");
        assert_eq!(format!("{}", ClassLoaderId::Application), "application");
    }

    #[test]
    fn class_loader_id_equality() {
        assert_eq!(ClassLoaderId::Bootstrap, ClassLoaderId::Bootstrap);
        assert_ne!(ClassLoaderId::Bootstrap, ClassLoaderId::Extension);
        assert_ne!(ClassLoaderId::Extension, ClassLoaderId::Application);
    }

    #[test]
    fn class_loader_id_hash() {
        let mut set = HashSet::new();
        set.insert(ClassLoaderId::Bootstrap);
        set.insert(ClassLoaderId::Extension);
        set.insert(ClassLoaderId::Application);
        assert_eq!(set.len(), 3);
        assert!(set.contains(&ClassLoaderId::Bootstrap));
    }

    #[test]
    fn class_loader_id_clone() {
        let a = ClassLoaderId::Bootstrap;
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -- native-API `u32` codec ------------------------------------------
    //
    // Regression cover for the CGLIB `@Configuration` singleton bug: the
    // encode (`loader_id_of_class`) and the decode (`define_class_full` and
    // friends) were hand-written at six separate call sites and drifted, so
    // `Application` round-tripped to `UserDefined(2)` and every enhanced
    // subclass landed in a different runtime package than its superclass.

    #[test]
    fn native_id_encoding_matches_the_documented_table() {
        assert_eq!(ClassLoaderId::Bootstrap.to_native_id(), 0);
        assert_eq!(ClassLoaderId::Extension.to_native_id(), 1);
        assert_eq!(ClassLoaderId::Application.to_native_id(), 2);
        assert_eq!(ClassLoaderId::UserDefined(3).to_native_id(), 3);
        assert_eq!(ClassLoaderId::UserDefined(9999).to_native_id(), 9999);
    }

    #[test]
    fn from_native_id_is_the_exact_inverse_of_to_native_id() {
        let mut all = vec![
            ClassLoaderId::Bootstrap,
            ClassLoaderId::Extension,
            ClassLoaderId::Application,
        ];
        // Only ids >= 3 are legal user-defined namespaces (see
        // `NATIVE_FIRST_USER_DEFINED` / `allocate_loader_id`).
        all.extend((ClassLoaderId::NATIVE_FIRST_USER_DEFINED..64).map(ClassLoaderId::UserDefined));
        all.push(ClassLoaderId::UserDefined(u32::MAX));
        for id in all {
            assert_eq!(
                ClassLoaderId::from_native_id(id.to_native_id()),
                id,
                "round-trip must be lossless for {id}"
            );
        }
    }

    #[test]
    fn default_sentinel_decode_differs_from_strict_decode_only_at_zero() {
        assert_eq!(
            ClassLoaderId::from_native_id(0),
            ClassLoaderId::Bootstrap,
            "strict decode: 0 is the bootstrap loader"
        );
        assert_eq!(
            ClassLoaderId::from_native_id_or_default(0),
            ClassLoaderId::Application,
            "sentinel decode: 0 means 'unspecified -> application/global namespace'"
        );
        for id in 1..64u32 {
            assert_eq!(
                ClassLoaderId::from_native_id_or_default(id),
                ClassLoaderId::from_native_id(id),
                "the two decoders may only disagree at 0 (id {id})"
            );
        }
    }

    #[test]
    fn application_never_decodes_to_a_user_defined_namespace() {
        // The exact shape of the fixed bug: `Application` encodes to 2, and 2
        // must NOT come back as `UserDefined(2)` under either decoder.
        let encoded = ClassLoaderId::Application.to_native_id();
        assert_eq!(
            ClassLoaderId::from_native_id(encoded),
            ClassLoaderId::Application
        );
        assert_eq!(
            ClassLoaderId::from_native_id_or_default(encoded),
            ClassLoaderId::Application
        );
        assert_eq!(ClassLoaderId::from_native_id(1), ClassLoaderId::Extension);
    }

    #[test]
    fn reserved_ids_are_below_the_first_user_defined_namespace() {
        assert!(ClassLoaderId::NATIVE_BOOTSTRAP < ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
        assert!(ClassLoaderId::NATIVE_EXTENSION < ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
        assert!(ClassLoaderId::NATIVE_APPLICATION < ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
        // Every id at or above the boundary decodes to a user-defined
        // namespace under both decoders.
        for id in ClassLoaderId::NATIVE_FIRST_USER_DEFINED..16 {
            assert_eq!(
                ClassLoaderId::from_native_id(id),
                ClassLoaderId::UserDefined(id)
            );
            assert_eq!(
                ClassLoaderId::from_native_id_or_default(id),
                ClassLoaderId::UserDefined(id)
            );
        }
    }

    #[test]
    fn is_builtin_matches_the_reserved_id_range() {
        assert!(ClassLoaderId::Bootstrap.is_builtin());
        assert!(ClassLoaderId::Extension.is_builtin());
        assert!(ClassLoaderId::Application.is_builtin());
        assert!(!ClassLoaderId::UserDefined(3).is_builtin());
        assert!(!ClassLoaderId::UserDefined(u32::MAX).is_builtin());
    }
}
