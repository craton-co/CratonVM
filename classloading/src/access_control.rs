//! Access control enforcement (JVM spec 5.4.4).
//!
//! Checks whether a class, field, or method is accessible from a given context.
//! Returns `IllegalAccessError` when access is denied.

use rustjvm_reader::class_access_flags::{FieldAccessFlags, MethodAccessFlags};
#[cfg(test)]
use std::sync::Arc;

use super::class::{Class, ClassStore};
use crate::module::{package_of as module_pkg_of, ModuleRegistry, UNNAMED_MODULE};
use rustjvm_types::error::LinkageError;

/// Check whether `accessor` can access `target` class.
///
/// Per JVM spec 5.4.4:
/// - Public classes are accessible from any class.
/// - Non-public (package-private) classes are only accessible from the same runtime package.
#[inline]
pub fn check_class_access(accessor: &Class, target: &Class) -> Result<(), LinkageError> {
    if target.is_public() {
        return Ok(());
    }

    // Package-private: same runtime package required
    if same_runtime_package(&accessor.name, &target.name) {
        return Ok(());
    }

    Err(LinkageError::IllegalAccessError {
        message: format!(
            "class {} cannot access class {} (not public, different package)",
            accessor.name, target.name
        ),
    })
}

/// Check whether `accessor` can access a field in `declaring` class with given flags.
///
/// Per JVM spec 5.4.4:
/// - `PUBLIC` в†’ accessible from anywhere
/// - `PRIVATE` в†’ accessible only from declaring class
/// - `PROTECTED` в†’ accessible from same package OR subclasses
/// - Package-private (no access modifier) в†’ accessible from same package only
#[inline]
pub fn check_field_access(
    accessor: &Class,
    declaring: &Class,
    flags: FieldAccessFlags,
    store: &ClassStore,
) -> Result<(), LinkageError> {
    // Public fields are always accessible
    if flags.contains(FieldAccessFlags::PUBLIC) {
        return Ok(());
    }

    // Private: only from the declaring class itself or a nestmate (JEP 181)
    if flags.contains(FieldAccessFlags::PRIVATE) {
        if accessor.id == declaring.id || are_nestmates(accessor, declaring) {
            return Ok(());
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access private field in {}",
                accessor.name, declaring.name
            ),
        });
    }

    // Protected: same package OR subclass
    if flags.contains(FieldAccessFlags::PROTECTED) {
        if same_runtime_package(&accessor.name, &declaring.name) {
            return Ok(());
        }
        if accessor.is_subclass_of(declaring.id, store) {
            return Ok(());
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access protected field in {} (different package, not subclass)",
                accessor.name, declaring.name
            ),
        });
    }

    // Package-private (no access modifier): same package only
    if same_runtime_package(&accessor.name, &declaring.name) {
        return Ok(());
    }

    Err(LinkageError::IllegalAccessError {
        message: format!(
            "class {} cannot access package-private field in {} (different package)",
            accessor.name, declaring.name
        ),
    })
}

/// Check whether `accessor` can access a method in `declaring` class with given flags.
///
/// Same rules as field access (JVM spec 5.4.4).
#[inline]
pub fn check_method_access(
    accessor: &Class,
    declaring: &Class,
    flags: MethodAccessFlags,
    store: &ClassStore,
) -> Result<(), LinkageError> {
    // Public methods are always accessible
    if flags.contains(MethodAccessFlags::PUBLIC) {
        return Ok(());
    }

    // Private: only from the declaring class itself or a nestmate (JEP 181)
    if flags.contains(MethodAccessFlags::PRIVATE) {
        if accessor.id == declaring.id || are_nestmates(accessor, declaring) {
            return Ok(());
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access private method in {}",
                accessor.name, declaring.name
            ),
        });
    }

    // Protected: same package OR subclass
    if flags.contains(MethodAccessFlags::PROTECTED) {
        if same_runtime_package(&accessor.name, &declaring.name) {
            return Ok(());
        }
        if accessor.is_subclass_of(declaring.id, store) {
            return Ok(());
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access protected method in {} (different package, not subclass)",
                accessor.name, declaring.name
            ),
        });
    }

    // Package-private: same package only
    if same_runtime_package(&accessor.name, &declaring.name) {
        return Ok(());
    }

    Err(LinkageError::IllegalAccessError {
        message: format!(
            "class {} cannot access package-private method in {} (different package)",
            accessor.name, declaring.name
        ),
    })
}

/// Check if two classes are nestmates (JEP 181, Java 11+).
///
/// Two classes are nestmates if they have the same nest host. The nest host is:
/// - The class named by the `NestHost` attribute, if present.
/// - Otherwise, the class itself (it is its own nest host).
#[inline]
pub fn are_nestmates(a: &Class, b: &Class) -> bool {
    if a.id == b.id {
        return true;
    }
    let host_a = a.nest_host.as_deref().unwrap_or(&a.name);
    let host_b = b.nest_host.as_deref().unwrap_or(&b.name);
    host_a == host_b
}

/// Check if two classes are in the same runtime package.
///
/// The runtime package is determined by the package prefix of the fully-qualified
/// internal name. For example:
/// - `"java/lang/Object"` в†’ package `"java/lang"`
/// - `"java/lang/String"` в†’ package `"java/lang"` (same)
/// - `"java/util/List"` в†’ package `"java/util"` (different)
/// - `"Foo"` в†’ default package `""` (no `/`)
#[inline]
pub fn same_runtime_package(name_a: &str, name_b: &str) -> bool {
    package_of(name_a) == package_of(name_b)
}

/// Extract the package prefix from a fully-qualified internal name.
fn package_of(class_name: &str) -> &str {
    match class_name.rfind('/') {
        Some(pos) => &class_name[..pos],
        None => "", // default package
    }
}

// ---------------------------------------------------------------------------
// N2: Module-boundary access checks (JPMS, Java 9+)
// ---------------------------------------------------------------------------

/// Check JPMS module boundary rules when `accessor` accesses a public member
/// in `target`.
///
/// This is called **in addition** to the standard JVM 5.4.4 checks above.
/// It only fires when both classes belong to distinct *named* modules.
///
/// Rules (simplified from JVMS В§5.4.4 with JPMS overlay):
/// 1. Same module в†’ allowed.
/// 2. Either module is the unnamed module в†’ allowed (classpath compat).
/// 3. `accessor_module` must *read* `target_module`.
/// 4. `target_module` must *export* the target package to `accessor_module`.
///
/// If the `ModuleRegistry` is empty (no modules registered), the check is
/// a no-op to preserve backward compatibility with classpath-only runs.
pub fn check_module_access(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
) -> Result<(), LinkageError> {
    // No modules registered в†’ classpath-only mode, skip enforcement.
    if registry.is_empty() {
        return Ok(());
    }

    let accessor_mod = accessor.module_name.as_deref().unwrap_or(UNNAMED_MODULE);
    let target_mod = target.module_name.as_deref().unwrap_or(UNNAMED_MODULE);

    // Same module or unnamed module involved в†’ always allowed.
    if accessor_mod == target_mod || accessor_mod == UNNAMED_MODULE || target_mod == UNNAMED_MODULE {
        return Ok(());
    }

    let target_pkg = module_pkg_of(&target.name);

    registry
        .check_module_access(accessor_mod, target_mod, target_pkg)
        .map_err(|reason| LinkageError::IllegalAccessError { message: reason })
}

/// Module-aware variant of [`check_class_access`].
///
/// Performs both the standard 5.4.4 check and the JPMS module-boundary check.
pub fn check_class_access_with_modules(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
) -> Result<(), LinkageError> {
    check_class_access(accessor, target)?;
    check_module_access(accessor, target, registry)
}

/// Module-aware variant of [`check_field_access`].
pub fn check_field_access_with_modules(
    accessor: &Class,
    declaring: &Class,
    flags: FieldAccessFlags,
    store: &ClassStore,
    registry: &ModuleRegistry,
) -> Result<(), LinkageError> {
    check_field_access(accessor, declaring, flags, store)?;
    check_module_access(accessor, declaring, registry)
}

/// Module-aware variant of [`check_method_access`].
pub fn check_method_access_with_modules(
    accessor: &Class,
    declaring: &Class,
    flags: MethodAccessFlags,
    store: &ClassStore,
    registry: &ModuleRegistry,
) -> Result<(), LinkageError> {
    check_method_access(accessor, declaring, flags, store)?;
    check_module_access(accessor, declaring, registry)
}

/// Convenience: check JPMS module access between two classes identified by
/// ClassId, using a ClassManager reference (which holds both the class store
/// and the module registry).
///
/// Returns Ok(()) silently if either class is not found (defensive вЂ” the
/// missing class will be caught later by a more specific error path).
pub fn check_module_access_by_id(
    accessor_id: super::class::ClassId,
    target_id: super::class::ClassId,
    cm: &super::class_manager::ClassManager,
) -> Result<(), LinkageError> {
    if cm.module_registry.is_empty() {
        return Ok(());
    }
    let (accessor, target) = match (cm.get_class(accessor_id), cm.get_class(target_id)) {
        (Some(a), Some(t)) => (a, t),
        _ => return Ok(()),
    };
    check_module_access(accessor, target, &cm.module_registry)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class::{Class, ClassId, ClassLoaderId, ClassState, ClassStore};
    use rustjvm_reader::class_access_flags::ClassAccessFlags;
    use rustjvm_reader::class_file_version::ClassFileVersion;
    use rustjvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    fn empty_cp() -> ConstantPool {
        ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
    }

    fn make_class(
        store: &mut ClassStore,
        name: &str,
        superclass: Option<ClassId>,
        flags: ClassAccessFlags,
    ) -> ClassId {
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_cp(),
            access_flags: flags,
            superclass,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            array_info: None,
        });
        id
    }

    // --- package_of ---

    #[test]
    fn package_of_standard_class() {
        assert_eq!(package_of("java/lang/Object"), "java/lang");
    }

    #[test]
    fn package_of_nested() {
        assert_eq!(package_of("com/example/foo/Bar"), "com/example/foo");
    }

    #[test]
    fn package_of_default_package() {
        assert_eq!(package_of("Foo"), "");
    }

    // --- same_runtime_package ---

    #[test]
    fn same_package_java_lang() {
        assert!(same_runtime_package("java/lang/Object", "java/lang/String"));
    }

    #[test]
    fn different_packages() {
        assert!(!same_runtime_package("java/lang/Object", "java/util/List"));
    }

    #[test]
    fn same_default_package() {
        assert!(same_runtime_package("Foo", "Bar"));
    }

    #[test]
    fn default_vs_named_package() {
        assert!(!same_runtime_package("Foo", "com/example/Bar"));
    }

    // --- check_class_access ---

    #[test]
    fn public_class_always_accessible() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/Accessor",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let target_id = make_class(
            &mut store,
            "com/bar/Target",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let accessor = store.get(accessor_id).unwrap();
        let target = store.get(target_id).unwrap();
        assert!(check_class_access(accessor, target).is_ok());
    }

    #[test]
    fn package_private_class_same_package() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/Accessor",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let target_id = make_class(
            &mut store,
            "com/foo/Target",
            None,
            ClassAccessFlags::SUPER, // no PUBLIC
        );

        let accessor = store.get(accessor_id).unwrap();
        let target = store.get(target_id).unwrap();
        assert!(check_class_access(accessor, target).is_ok());
    }

    #[test]
    fn package_private_class_different_package() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/Accessor",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let target_id = make_class(
            &mut store,
            "com/bar/Target",
            None,
            ClassAccessFlags::SUPER, // no PUBLIC
        );

        let accessor = store.get(accessor_id).unwrap();
        let target = store.get(target_id).unwrap();
        assert!(check_class_access(accessor, target).is_err());
    }

    // --- check_field_access ---

    #[test]
    fn public_field_always_accessible() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let declaring_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let accessor = store.get(accessor_id).unwrap();
        let declaring = store.get(declaring_id).unwrap();
        assert!(check_field_access(accessor, declaring, FieldAccessFlags::PUBLIC, &store).is_ok());
    }

    #[test]
    fn private_field_same_class() {
        let mut store = ClassStore::new();
        let class_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let class = store.get(class_id).unwrap();
        assert!(check_field_access(class, class, FieldAccessFlags::PRIVATE, &store).is_ok());
    }

    #[test]
    fn private_field_different_class() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/foo/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store).is_err());
    }

    #[test]
    fn protected_field_subclass() {
        let mut store = ClassStore::new();
        let parent_id = make_class(
            &mut store,
            "com/foo/Parent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let child_id = make_class(
            &mut store,
            "com/bar/Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let child = store.get(child_id).unwrap();
        let parent = store.get(parent_id).unwrap();
        assert!(check_field_access(child, parent, FieldAccessFlags::PROTECTED, &store).is_ok());
    }

    #[test]
    fn protected_field_non_subclass_different_package() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::PROTECTED, &store).is_err());
    }

    #[test]
    fn package_private_field_same_package() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/foo/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::empty(), &store).is_ok());
    }

    #[test]
    fn package_private_field_different_package() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::empty(), &store).is_err());
    }

    // --- check_method_access ---

    #[test]
    fn public_method_always_accessible() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_method_access(a, b, MethodAccessFlags::PUBLIC, &store).is_ok());
    }

    #[test]
    fn private_method_same_class() {
        let mut store = ClassStore::new();
        let class_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let class = store.get(class_id).unwrap();
        assert!(check_method_access(class, class, MethodAccessFlags::PRIVATE, &store).is_ok());
    }

    #[test]
    fn private_method_different_class() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/foo/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_method_access(a, b, MethodAccessFlags::PRIVATE, &store).is_err());
    }

    #[test]
    fn protected_method_subclass_different_package() {
        let mut store = ClassStore::new();
        let parent_id = make_class(
            &mut store,
            "com/foo/Parent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let child_id = make_class(
            &mut store,
            "com/bar/Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let child = store.get(child_id).unwrap();
        let parent = store.get(parent_id).unwrap();
        assert!(check_method_access(child, parent, MethodAccessFlags::PROTECTED, &store).is_ok());
    }

    // --- Nest-based access control (JEP 181) ---

    fn make_nest_class(
        store: &mut ClassStore,
        name: &str,
        nest_host: Option<&str>,
        nest_members: &[&str],
    ) -> ClassId {
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_11,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_cp(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: nest_host.map(|s| s.to_string()),
            nest_members: nest_members.iter().map(|s| s.to_string()).collect(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            array_info: None,
        });
        id
    }

    #[test]
    fn nestmates_same_host_can_access_private() {
        let mut store = ClassStore::new();
        // Outer is the nest host, Inner has NestHost pointing to Outer
        let _outer_id =
            make_nest_class(&mut store, "com/foo/Outer", None, &["com/foo/Outer$Inner"]);
        let inner_id = make_nest_class(
            &mut store,
            "com/foo/Outer$Inner",
            Some("com/foo/Outer"),
            &[],
        );

        let outer = store.get(_outer_id).unwrap();
        let inner = store.get(inner_id).unwrap();

        // Inner can access Outer's private fields
        assert!(check_field_access(inner, outer, FieldAccessFlags::PRIVATE, &store).is_ok());
        // Outer can access Inner's private methods
        assert!(check_method_access(outer, inner, MethodAccessFlags::PRIVATE, &store).is_ok());
    }

    #[test]
    fn nestmates_both_inner_classes() {
        let mut store = ClassStore::new();
        // Two inner classes with the same nest host
        let a_id = make_nest_class(&mut store, "com/foo/Outer$A", Some("com/foo/Outer"), &[]);
        let b_id = make_nest_class(&mut store, "com/foo/Outer$B", Some("com/foo/Outer"), &[]);

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();

        // A and B are nestmates, can access each other's private members
        assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store).is_ok());
        assert!(check_field_access(b, a, FieldAccessFlags::PRIVATE, &store).is_ok());
    }

    #[test]
    fn non_nestmates_cannot_access_private() {
        let mut store = ClassStore::new();
        let a_id = make_nest_class(
            &mut store,
            "com/foo/A",
            None, // its own host
            &[],
        );
        let b_id = make_nest_class(
            &mut store,
            "com/foo/B",
            None, // its own host (different nest)
            &[],
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();

        assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store).is_err());
        assert!(check_method_access(a, b, MethodAccessFlags::PRIVATE, &store).is_err());
    }

    #[test]
    fn are_nestmates_same_class() {
        let mut store = ClassStore::new();
        let id = make_nest_class(&mut store, "com/foo/A", None, &[]);
        let class = store.get(id).unwrap();
        assert!(are_nestmates(class, class));
    }
}
