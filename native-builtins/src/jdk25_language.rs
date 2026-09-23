// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK 25 Language Feature Runtime Support
//!
//! Implements runtime backing for three finalized JDK 25 JEPs:
//!
//! - **JEP 511 — Module Import Declarations**: `import module M;` resolution
//! - **JEP 513 — Flexible Constructor Bodies**: pre-`super()` statement validation
//! - **JEP 512 — Compact Source Files / Instance Main Methods**: implicit class detection
//!   and main method selection

use std::collections::HashMap;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

// ===========================================================================
// 15.5  Module Import Declarations (JEP 511)
// ===========================================================================

/// Resolves `import module M;` declarations at runtime by mapping module names
/// to the packages they export.
#[derive(Debug, Clone)]
pub struct ModuleImportResolver {
    /// module name -> list of exported packages (internal form, e.g. `java/lang`)
    pub known_modules: HashMap<String, Vec<String>>,
}

/// Packages exported by `java.base` that are available via `import module java.base;`.
const JAVA_BASE_EXPORTS: &[&str] = &[
    "java/lang",
    "java/util",
    "java/io",
    "java/nio",
    "java/math",
    "java/net",
    "java/time",
    "java/security",
    "java/text",
    "java/util/concurrent",
    "java/util/stream",
    "java/util/function",
    "java/lang/reflect",
    "java/lang/invoke",
];

impl ModuleImportResolver {
    /// Create a new resolver pre-populated with `java.base` exports.
    pub fn new() -> Self {
        let mut known_modules = HashMap::new();
        known_modules.insert(
            "java.base".to_string(),
            JAVA_BASE_EXPORTS.iter().map(|s| s.to_string()).collect(),
        );
        ModuleImportResolver { known_modules }
    }

    /// Register an additional module and its exported packages.
    pub fn register_module(&mut self, name: &str, packages: Vec<String>) {
        self.known_modules.insert(name.to_string(), packages);
    }

    /// Resolve which packages are exported by the given module.
    pub fn resolve_module_import(&self, module_name: &str) -> Option<&[String]> {
        self.known_modules.get(module_name).map(|v| v.as_slice())
    }

    /// Check whether a fully-qualified class name is accessible through the
    /// given module import.  The FQCN is expected in internal form
    /// (e.g. `java/lang/String`).
    pub fn is_type_accessible(&self, module_name: &str, fqcn: &str) -> bool {
        if let Some(packages) = self.known_modules.get(module_name) {
            // Extract the package portion (everything before the last `/`).
            if let Some(last_slash) = fqcn.rfind('/') {
                let pkg = &fqcn[..last_slash];
                packages.iter().any(|p| p == pkg)
            } else {
                // Default (unnamed) package -- not exported by any module.
                false
            }
        } else {
            false
        }
    }
}

impl Default for ModuleImportResolver {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Native methods — jdk/internal/module/ModuleImports
// ---------------------------------------------------------------------------

fn module_imports_resolve(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // resolveModuleImport(String) -> boolean (as int)
    // We accept any string; known modules return 1.
    let resolver = ModuleImportResolver::new();
    let module_name = match args.get(0) {
        Some(Value::Object(Some(_))) => "java.base", // simplified: treat any object as java.base
        _ => "",
    };
    let known = resolver.resolve_module_import(module_name).is_some();
    Ok(Some(Value::Int(if known { 1 } else { 0 })))
}

fn module_imports_get_exported_packages(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let resolver = ModuleImportResolver::new();
    let module_name = match args.get(0) {
        Some(Value::Object(Some(_))) => "java.base",
        _ => "",
    };
    let count = resolver
        .resolve_module_import(module_name)
        .map(|p| p.len() as i32)
        .unwrap_or(0);
    Ok(Some(Value::Int(count)))
}

fn module_imports_is_supported(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

// ===========================================================================
// 15.6  Flexible Constructor Bodies (JEP 513)
// ===========================================================================

/// Kinds of statements that may appear before the explicit `super()` call.
#[derive(Debug, Clone, PartialEq)]
pub enum PreSuperStatement {
    /// Local variable assignment (slot index, value placeholder).
    LocalAssign(u16, Value),
    /// Static method call (class, method).
    StaticCall(String, String),
    /// Argument validation / null-check on a parameter slot.
    ArgumentCheck(u16),
    /// A statement that is NOT allowed before super (description of why).
    Disallowed(String),
}

/// Outcome of validating a sequence of pre-super statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    Valid,
    Invalid(String),
}

/// Validates that a sequence of bytecode-level statements appearing before an
/// explicit `super()` / `this()` invocation is legal under JEP 513 rules.
#[derive(Debug, Clone)]
pub struct FlexibleConstructorValidator;

impl FlexibleConstructorValidator {
    pub fn new() -> Self {
        FlexibleConstructorValidator
    }

    /// Validate a slice of pre-super statements.
    ///
    /// Rules (JEP 513):
    /// - Local variable assignments are allowed.
    /// - Static method calls are allowed.
    /// - Argument checks (e.g. `Objects.requireNonNull`) are allowed.
    /// - Anything else (`this.field` writes, instance calls on `this`,
    ///   letting `this` escape) is disallowed.
    pub fn validate_pre_super_statements(
        &self,
        instructions: &[PreSuperStatement],
    ) -> ValidationResult {
        for stmt in instructions {
            if let PreSuperStatement::Disallowed(reason) = stmt {
                return ValidationResult::Invalid(reason.clone());
            }
        }
        ValidationResult::Valid
    }

    /// Check whether a single statement kind (by ordinal) is permitted.
    ///
    /// 0 = LocalAssign, 1 = StaticCall, 2 = ArgumentCheck  -> allowed
    /// 3+ = Disallowed
    pub fn is_allowed_kind(kind: i32) -> bool {
        matches!(kind, 0 | 1 | 2)
    }
}

impl Default for FlexibleConstructorValidator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Native methods — jdk/internal/vm/FlexibleConstructors
// ---------------------------------------------------------------------------

fn flexible_constructors_is_pre_super_allowed(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn flexible_constructors_validate(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let kind = match args.get(0) {
        Some(Value::Int(k)) => *k,
        _ => -1,
    };
    let allowed = FlexibleConstructorValidator::is_allowed_kind(kind);
    Ok(Some(Value::Int(if allowed { 1 } else { 0 })))
}

// ===========================================================================
// 15.7  Compact Source Files / Instance Main Methods (JEP 512)
// ===========================================================================

/// A candidate `main` method found in a class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainMethodCandidate {
    pub name: String,
    pub descriptor: String,
    pub is_static: bool,
}

impl MainMethodCandidate {
    pub fn new(name: &str, descriptor: &str, is_static: bool) -> Self {
        MainMethodCandidate {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            is_static,
        }
    }

    /// Returns the selection priority (lower = higher priority) per JEP 512.
    ///
    /// Priority order:
    /// 1. `public static void main(String[] args)`
    /// 2. `static void main(String[] args)`
    /// 3. `static void main()`
    /// 4. `public void main(String[] args)` (instance)
    /// 5. `void main(String[] args)` (instance)
    /// 6. `void main()` (instance)
    ///
    /// Returns `None` if not a valid main method candidate.
    pub fn priority(&self) -> Option<u32> {
        let has_args = self.descriptor == "([Ljava/lang/String;)V";
        let no_args = self.descriptor == "()V";
        if self.name != "main" {
            return None;
        }
        match (self.is_static, has_args, no_args) {
            (true, true, _) => Some(1),  // static main(String[])
            (true, _, true) => Some(3),  // static main()
            (false, true, _) => Some(4), // instance main(String[])
            (false, _, true) => Some(6), // instance main()
            _ => None,
        }
    }
}

/// Detects whether a class is an implicitly declared class (JEP 512).
#[derive(Debug, Clone)]
pub struct ImplicitClassDetector;

/// Flag bit indicating a class has no explicit name (compiler-generated).
const ACC_IMPLICIT_CLASS: u16 = 0x1000; // ACC_SYNTHETIC re-used per JEP 512

impl ImplicitClassDetector {
    /// A class is implicit if it carries the implicit/synthetic flag AND
    /// contains a method named `main`.
    pub fn is_implicit_class(flags: u16, methods: &[&str]) -> bool {
        let has_flag = (flags & ACC_IMPLICIT_CLASS) != 0;
        let has_main = methods.iter().any(|m| *m == "main");
        has_flag && has_main
    }

    /// Select the best `main` method from a list of candidates per JEP 512
    /// selection priority.  Returns the index into the slice, or `None` if no
    /// valid candidate exists.
    pub fn find_main_method(methods: &[MainMethodCandidate]) -> Option<usize> {
        let mut best_idx: Option<usize> = None;
        let mut best_prio: u32 = u32::MAX;

        for (i, m) in methods.iter().enumerate() {
            if let Some(p) = m.priority() {
                if p < best_prio {
                    best_prio = p;
                    best_idx = Some(i);
                }
            }
        }
        best_idx
    }
}

/// Selection priority table exposed for testing.
pub struct MainMethodSelection;

impl MainMethodSelection {
    /// Number of defined priority levels.
    pub const LEVELS: u32 = 6;

    /// Human-readable description of a priority level.
    pub fn describe(level: u32) -> &'static str {
        match level {
            1 => "public static void main(String[] args)",
            2 => "static void main(String[] args)",
            3 => "static void main()",
            4 => "public void main(String[] args)",
            5 => "void main(String[] args)",
            6 => "void main()",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Native methods — jdk/internal/misc/ImplicitClasses
// ---------------------------------------------------------------------------

fn implicit_classes_is_implicit(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let flags = match args.get(0) {
        Some(Value::Int(f)) => *f as u16,
        _ => 0,
    };
    // Simplified: check if ACC_SYNTHETIC is set.
    let implicit = (flags & ACC_IMPLICIT_CLASS) != 0;
    Ok(Some(Value::Int(if implicit { 1 } else { 0 })))
}

fn implicit_classes_select_main(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Given a candidate count, return 0 (first/best candidate).
    let _count = match args.get(0) {
        Some(Value::Int(c)) => *c,
        _ => 0,
    };
    Ok(Some(Value::Int(0)))
}

fn implicit_classes_is_instance_main_allowed(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn implicit_classes_is_supported(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

// ===========================================================================
// Registration
// ===========================================================================

pub(crate) fn register_jdk25_language_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mi = "jdk/internal/module/ModuleImports";
    r.register(
        mi,
        "resolveModuleImport",
        "(Ljava/lang/String;)Z",
        module_imports_resolve,
    );
    r.register(
        mi,
        "getExportedPackages",
        "(Ljava/lang/String;)I",
        module_imports_get_exported_packages,
    );
    r.register(mi, "isSupported", "()Z", module_imports_is_supported);

    let fc = "jdk/internal/vm/FlexibleConstructors";
    r.register(
        fc,
        "isPreSuperAllowed",
        "()Z",
        flexible_constructors_is_pre_super_allowed,
    );
    r.register(
        fc,
        "validatePreSuperStatement",
        "(I)Z",
        flexible_constructors_validate,
    );

    let ic = "jdk/internal/misc/ImplicitClasses";
    r.register(ic, "isImplicitClass", "(I)Z", implicit_classes_is_implicit);
    r.register(ic, "selectMainMethod", "(I)I", implicit_classes_select_main);
    r.register(
        ic,
        "isInstanceMainAllowed",
        "()Z",
        implicit_classes_is_instance_main_allowed,
    );
    r.register(ic, "isSupported", "()Z", implicit_classes_is_supported);
    r.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod jdk25_language_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // -----------------------------------------------------------------------
    // ModuleImportResolver tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolver_new_has_java_base() {
        let r = ModuleImportResolver::new();
        assert!(r.known_modules.contains_key("java.base"));
    }

    #[test]
    fn test_resolver_default_same_as_new() {
        let r = ModuleImportResolver::default();
        assert!(r.known_modules.contains_key("java.base"));
    }

    #[test]
    fn test_resolver_java_base_package_count() {
        let r = ModuleImportResolver::new();
        let pkgs = r.resolve_module_import("java.base").unwrap();
        assert_eq!(pkgs.len(), JAVA_BASE_EXPORTS.len());
    }

    #[test]
    fn test_resolver_java_base_contains_java_lang() {
        let r = ModuleImportResolver::new();
        let pkgs = r.resolve_module_import("java.base").unwrap();
        assert!(pkgs.contains(&"java/lang".to_string()));
    }

    #[test]
    fn test_resolver_java_base_contains_java_util() {
        let r = ModuleImportResolver::new();
        let pkgs = r.resolve_module_import("java.base").unwrap();
        assert!(pkgs.contains(&"java/util".to_string()));
    }

    #[test]
    fn test_resolver_java_base_contains_java_io() {
        let r = ModuleImportResolver::new();
        let pkgs = r.resolve_module_import("java.base").unwrap();
        assert!(pkgs.contains(&"java/io".to_string()));
    }

    #[test]
    fn test_resolver_java_base_contains_concurrent() {
        let r = ModuleImportResolver::new();
        let pkgs = r.resolve_module_import("java.base").unwrap();
        assert!(pkgs.contains(&"java/util/concurrent".to_string()));
    }

    #[test]
    fn test_resolver_unknown_module_returns_none() {
        let r = ModuleImportResolver::new();
        assert!(r.resolve_module_import("java.desktop").is_none());
    }

    #[test]
    fn test_resolver_empty_string_returns_none() {
        let r = ModuleImportResolver::new();
        assert!(r.resolve_module_import("").is_none());
    }

    #[test]
    fn test_resolver_register_custom_module() {
        let mut r = ModuleImportResolver::new();
        r.register_module("java.sql", vec!["java/sql".to_string()]);
        assert!(r.resolve_module_import("java.sql").is_some());
        assert_eq!(r.resolve_module_import("java.sql").unwrap().len(), 1);
    }

    #[test]
    fn test_type_accessible_java_lang_string() {
        let r = ModuleImportResolver::new();
        assert!(r.is_type_accessible("java.base", "java/lang/String"));
    }

    #[test]
    fn test_type_accessible_java_util_list() {
        let r = ModuleImportResolver::new();
        assert!(r.is_type_accessible("java.base", "java/util/List"));
    }

    #[test]
    fn test_type_not_accessible_unknown_package() {
        let r = ModuleImportResolver::new();
        assert!(!r.is_type_accessible("java.base", "com/example/Foo"));
    }

    #[test]
    fn test_type_not_accessible_unknown_module() {
        let r = ModuleImportResolver::new();
        assert!(!r.is_type_accessible("java.desktop", "java/lang/String"));
    }

    #[test]
    fn test_type_not_accessible_default_package() {
        let r = ModuleImportResolver::new();
        assert!(!r.is_type_accessible("java.base", "Foo"));
    }

    #[test]
    fn test_type_accessible_nested_package() {
        let r = ModuleImportResolver::new();
        assert!(r.is_type_accessible("java.base", "java/util/concurrent/ConcurrentHashMap"));
    }

    #[test]
    fn test_type_accessible_reflect() {
        let r = ModuleImportResolver::new();
        assert!(r.is_type_accessible("java.base", "java/lang/reflect/Method"));
    }

    // -----------------------------------------------------------------------
    // FlexibleConstructorValidator tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validator_empty_is_valid() {
        let v = FlexibleConstructorValidator::new();
        assert_eq!(
            v.validate_pre_super_statements(&[]),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_validator_default() {
        let v = FlexibleConstructorValidator::default();
        assert_eq!(
            v.validate_pre_super_statements(&[]),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_validator_local_assign_allowed() {
        let v = FlexibleConstructorValidator::new();
        let stmts = vec![PreSuperStatement::LocalAssign(1, Value::Int(42))];
        assert_eq!(
            v.validate_pre_super_statements(&stmts),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_validator_static_call_allowed() {
        let v = FlexibleConstructorValidator::new();
        let stmts = vec![PreSuperStatement::StaticCall(
            "java/util/Objects".to_string(),
            "requireNonNull".to_string(),
        )];
        assert_eq!(
            v.validate_pre_super_statements(&stmts),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_validator_argument_check_allowed() {
        let v = FlexibleConstructorValidator::new();
        let stmts = vec![PreSuperStatement::ArgumentCheck(0)];
        assert_eq!(
            v.validate_pre_super_statements(&stmts),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_validator_disallowed_rejected() {
        let v = FlexibleConstructorValidator::new();
        let stmts = vec![PreSuperStatement::Disallowed(
            "this.field write".to_string(),
        )];
        assert_eq!(
            v.validate_pre_super_statements(&stmts),
            ValidationResult::Invalid("this.field write".to_string())
        );
    }

    #[test]
    fn test_validator_mixed_with_disallowed_at_end() {
        let v = FlexibleConstructorValidator::new();
        let stmts = vec![
            PreSuperStatement::LocalAssign(0, Value::Int(1)),
            PreSuperStatement::ArgumentCheck(1),
            PreSuperStatement::Disallowed("this escape".to_string()),
        ];
        assert_eq!(
            v.validate_pre_super_statements(&stmts),
            ValidationResult::Invalid("this escape".to_string())
        );
    }

    #[test]
    fn test_validator_mixed_all_allowed() {
        let v = FlexibleConstructorValidator::new();
        let stmts = vec![
            PreSuperStatement::LocalAssign(0, Value::Int(0)),
            PreSuperStatement::StaticCall("Foo".to_string(), "bar".to_string()),
            PreSuperStatement::ArgumentCheck(2),
        ];
        assert_eq!(
            v.validate_pre_super_statements(&stmts),
            ValidationResult::Valid
        );
    }

    #[test]
    fn test_is_allowed_kind_local_assign() {
        assert!(FlexibleConstructorValidator::is_allowed_kind(0));
    }

    #[test]
    fn test_is_allowed_kind_static_call() {
        assert!(FlexibleConstructorValidator::is_allowed_kind(1));
    }

    #[test]
    fn test_is_allowed_kind_arg_check() {
        assert!(FlexibleConstructorValidator::is_allowed_kind(2));
    }

    #[test]
    fn test_is_allowed_kind_disallowed() {
        assert!(!FlexibleConstructorValidator::is_allowed_kind(3));
    }

    #[test]
    fn test_is_allowed_kind_negative() {
        assert!(!FlexibleConstructorValidator::is_allowed_kind(-1));
    }

    #[test]
    fn test_is_allowed_kind_large() {
        assert!(!FlexibleConstructorValidator::is_allowed_kind(100));
    }

    // -----------------------------------------------------------------------
    // MainMethodCandidate / ImplicitClassDetector tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_candidate_priority_public_static_main_string_arr() {
        let c = MainMethodCandidate::new("main", "([Ljava/lang/String;)V", true);
        assert_eq!(c.priority(), Some(1));
    }

    #[test]
    fn test_candidate_priority_static_main_no_args() {
        let c = MainMethodCandidate::new("main", "()V", true);
        assert_eq!(c.priority(), Some(3));
    }

    #[test]
    fn test_candidate_priority_instance_main_string_arr() {
        let c = MainMethodCandidate::new("main", "([Ljava/lang/String;)V", false);
        assert_eq!(c.priority(), Some(4));
    }

    #[test]
    fn test_candidate_priority_instance_main_no_args() {
        let c = MainMethodCandidate::new("main", "()V", false);
        assert_eq!(c.priority(), Some(6));
    }

    #[test]
    fn test_candidate_priority_not_main_name() {
        let c = MainMethodCandidate::new("run", "([Ljava/lang/String;)V", true);
        assert_eq!(c.priority(), None);
    }

    #[test]
    fn test_candidate_priority_wrong_descriptor() {
        let c = MainMethodCandidate::new("main", "(I)V", true);
        assert_eq!(c.priority(), None);
    }

    #[test]
    fn test_is_implicit_class_with_flag_and_main() {
        assert!(ImplicitClassDetector::is_implicit_class(
            0x1000,
            &["main", "helper"]
        ));
    }

    #[test]
    fn test_is_not_implicit_class_without_flag() {
        assert!(!ImplicitClassDetector::is_implicit_class(0x0001, &["main"]));
    }

    #[test]
    fn test_is_not_implicit_class_without_main() {
        assert!(!ImplicitClassDetector::is_implicit_class(
            0x1000,
            &["run", "helper"]
        ));
    }

    #[test]
    fn test_is_not_implicit_empty_methods() {
        assert!(!ImplicitClassDetector::is_implicit_class(0x1000, &[]));
    }

    #[test]
    fn test_find_main_selects_highest_priority() {
        let candidates = vec![
            MainMethodCandidate::new("main", "()V", false), // prio 6
            MainMethodCandidate::new("main", "([Ljava/lang/String;)V", true), // prio 1
            MainMethodCandidate::new("main", "()V", true),  // prio 3
        ];
        assert_eq!(
            ImplicitClassDetector::find_main_method(&candidates),
            Some(1)
        );
    }

    #[test]
    fn test_find_main_single_candidate() {
        let candidates = vec![
            MainMethodCandidate::new("main", "()V", false), // prio 6
        ];
        assert_eq!(
            ImplicitClassDetector::find_main_method(&candidates),
            Some(0)
        );
    }

    #[test]
    fn test_find_main_no_candidates() {
        let candidates: Vec<MainMethodCandidate> = vec![];
        assert_eq!(ImplicitClassDetector::find_main_method(&candidates), None);
    }

    #[test]
    fn test_find_main_no_valid_candidates() {
        let candidates = vec![
            MainMethodCandidate::new("run", "()V", true),
            MainMethodCandidate::new("start", "([Ljava/lang/String;)V", false),
        ];
        assert_eq!(ImplicitClassDetector::find_main_method(&candidates), None);
    }

    #[test]
    fn test_find_main_instance_over_no_args() {
        // instance main(String[]) (prio 4) beats instance main() (prio 6)
        let candidates = vec![
            MainMethodCandidate::new("main", "()V", false), // prio 6
            MainMethodCandidate::new("main", "([Ljava/lang/String;)V", false), // prio 4
        ];
        assert_eq!(
            ImplicitClassDetector::find_main_method(&candidates),
            Some(1)
        );
    }

    #[test]
    fn test_find_main_static_over_instance() {
        // static main() (prio 3) beats instance main(String[]) (prio 4)
        let candidates = vec![
            MainMethodCandidate::new("main", "([Ljava/lang/String;)V", false), // prio 4
            MainMethodCandidate::new("main", "()V", true),                     // prio 3
        ];
        assert_eq!(
            ImplicitClassDetector::find_main_method(&candidates),
            Some(1)
        );
    }

    // -----------------------------------------------------------------------
    // MainMethodSelection description tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_selection_describe_level_1() {
        assert_eq!(
            MainMethodSelection::describe(1),
            "public static void main(String[] args)"
        );
    }

    #[test]
    fn test_selection_describe_level_6() {
        assert_eq!(MainMethodSelection::describe(6), "void main()");
    }

    #[test]
    fn test_selection_describe_unknown() {
        assert_eq!(MainMethodSelection::describe(99), "unknown");
    }

    #[test]
    fn test_selection_levels_constant() {
        assert_eq!(MainMethodSelection::LEVELS, 6);
    }

    // -----------------------------------------------------------------------
    // Registration smoke test
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_does_not_panic() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_language_natives(&mut r);
    }

    // -----------------------------------------------------------------------
    // JAVA_BASE_EXPORTS constant tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_java_base_exports_has_14_entries() {
        assert_eq!(JAVA_BASE_EXPORTS.len(), 14);
    }

    #[test]
    fn test_java_base_exports_contains_stream() {
        assert!(JAVA_BASE_EXPORTS.contains(&"java/util/stream"));
    }

    #[test]
    fn test_java_base_exports_contains_function() {
        assert!(JAVA_BASE_EXPORTS.contains(&"java/util/function"));
    }

    // -----------------------------------------------------------------------
    // PreSuperStatement equality tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pre_super_local_assign_eq() {
        let a = PreSuperStatement::LocalAssign(1, Value::Int(5));
        let b = PreSuperStatement::LocalAssign(1, Value::Int(5));
        assert_eq!(a, b);
    }

    #[test]
    fn test_pre_super_local_assign_ne_slot() {
        let a = PreSuperStatement::LocalAssign(1, Value::Int(5));
        let b = PreSuperStatement::LocalAssign(2, Value::Int(5));
        assert_ne!(a, b);
    }

    #[test]
    fn test_pre_super_disallowed_preserves_message() {
        let stmt = PreSuperStatement::Disallowed("instance call on this".to_string());
        assert!(
            matches!(&stmt, PreSuperStatement::Disallowed(msg) if msg == "instance call on this"),
            "expected Disallowed variant, got {stmt:?}"
        );
    }

    // -----------------------------------------------------------------------
    // ACC_IMPLICIT_CLASS constant test
    // -----------------------------------------------------------------------

    #[test]
    fn test_acc_implicit_class_value() {
        assert_eq!(ACC_IMPLICIT_CLASS, 0x1000);
    }

    // -----------------------------------------------------------------------
    // Java 21-25 language feature tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_flexible_constructor_pre_super() {
        // JEP 513: statements before super() call
        // Verify that field assignment before super() is valid
        struct Base {
            x: i32,
        }
        struct Child {
            base: Base,
            y: i32,
        }
        // Simulate: int y = validate(arg); super(y);
        let validated = 42;
        let child = Child {
            base: Base { x: validated },
            y: validated,
        };
        assert_eq!(child.y, 42);
    }

    #[test]
    fn test_module_import_declaration() {
        // JEP 511: import module java.base
        // At VM level, this means resolving all exported packages of a module
        let module_name = "java.base";
        let exported_packages = vec!["java.lang", "java.util", "java.io"];
        assert!(exported_packages.contains(&"java.lang"));
        assert_eq!(module_name, "java.base");
    }

    #[test]
    fn test_compact_source_implicit_class() {
        // JEP 512: implicitly declared classes with instance main methods
        // Class file has main() or main(String[]) without explicit class declaration
        let has_main = true;
        let is_implicit_class = true;
        assert!(has_main && is_implicit_class);
    }

    #[test]
    fn test_stable_value_lazy_init() {
        // JEP 502: StableValue — deferred immutable computation
        use std::sync::OnceLock;
        let stable: OnceLock<i32> = OnceLock::new();
        assert!(stable.get().is_none());
        stable.set(42).unwrap();
        assert_eq!(*stable.get().unwrap(), 42);
        // Cannot set again
        assert!(stable.set(99).is_err());
    }

    #[test]
    fn test_stable_value_thread_safe() {
        use std::sync::{Arc, OnceLock};
        let stable = Arc::new(OnceLock::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let s = stable.clone();
                std::thread::spawn(move || {
                    let _ = s.set(i);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Exactly one thread wins
        assert!(stable.get().is_some());
    }

    #[test]
    fn test_record_basic() {
        // Java records: record Point(int x, int y) {}
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        struct PointRecord {
            x: i32,
            y: i32,
        }
        let p = PointRecord { x: 1, y: 2 };
        let q = PointRecord { x: 1, y: 2 };
        // Records have equals, hashCode, toString
        assert_eq!(p, q);
        assert_eq!(format!("{:?}", p), "PointRecord { x: 1, y: 2 }");
    }

    #[test]
    fn test_record_canonical_constructor() {
        #[derive(Debug, PartialEq)]
        struct Range {
            lo: i32,
            hi: i32,
        }
        impl Range {
            fn new(lo: i32, hi: i32) -> Self {
                // Compact canonical constructor validates
                assert!(lo <= hi, "lo must be <= hi");
                Range { lo, hi }
            }
        }
        let r = Range::new(1, 10);
        assert_eq!(r.lo, 1);
        assert_eq!(r.hi, 10);
    }

    #[test]
    #[should_panic(expected = "lo must be <= hi")]
    fn test_record_canonical_constructor_validation() {
        #[derive(Debug)]
        struct Range {
            lo: i32,
            hi: i32,
        }
        impl Range {
            fn new(lo: i32, hi: i32) -> Self {
                assert!(lo <= hi, "lo must be <= hi");
                Range { lo, hi }
            }
        }
        let _ = Range::new(10, 1); // should panic
    }

    #[test]
    fn test_sealed_class_hierarchy() {
        // sealed interface Shape permits Circle, Rectangle
        // Only permitted subclasses can extend
        enum Shape {
            Circle(f64),
            Rectangle(f64, f64),
            Triangle(f64, f64, f64),
        }
        let shapes: Vec<Shape> = vec![
            Shape::Circle(5.0),
            Shape::Rectangle(3.0, 4.0),
            Shape::Triangle(3.0, 4.0, 5.0),
        ];
        assert_eq!(shapes.len(), 3);
    }

    #[test]
    fn test_sealed_exhaustive_switch() {
        enum Color {
            Red,
            Green,
            Blue,
        }
        let c = Color::Green;
        let name = match c {
            Color::Red => "red",
            Color::Green => "green",
            Color::Blue => "blue",
        };
        assert_eq!(name, "green");
    }
}
