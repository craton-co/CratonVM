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
        assert_eq!(format!("{}", ClassId::new(u32::MAX)), format!("{}", u32::MAX));
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
}
