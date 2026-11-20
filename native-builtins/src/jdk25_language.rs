// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    Ok(Some(Value::Int(0)))}

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

pub fn register_jdk25_language_natives(r: &mut NativeMethodRegistry) {
            let mi = "jdk/internal/module/ModuleImports";
    r.register(
        mi,
        "resolveModuleImport",
        "(Ljava/lang/String;)Z",
        module_imports_resolve, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mi,
        "getExportedPackages",
        "(Ljava/lang/String;)I",
        module_imports_get_exported_packages, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(mi, "isSupported", "()Z", module_imports_is_supported, cratonvm_native_api::NativeKind::Bridge);

    let fc = "jdk/internal/vm/FlexibleConstructors";
    r.register(
        fc,
        "isPreSuperAllowed",
        "()Z",
        flexible_constructors_is_pre_super_allowed, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fc,
        "validatePreSuperStatement",
        "(I)Z",
        flexible_constructors_validate, cratonvm_native_api::NativeKind::Bridge,
    );

    let ic = "jdk/internal/misc/ImplicitClasses";
    r.register(ic, "isImplicitClass", "(I)Z", implicit_classes_is_implicit, cratonvm_native_api::NativeKind::Bridge);
    r.register(ic, "selectMainMethod", "(I)I", implicit_classes_select_main, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ic,
        "isInstanceMainAllowed",
        "()Z",
        implicit_classes_is_instance_main_allowed, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ic, "isSupported", "()Z", implicit_classes_is_supported, cratonvm_native_api::NativeKind::Bridge);
    }

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod jdk25_language_tests;
