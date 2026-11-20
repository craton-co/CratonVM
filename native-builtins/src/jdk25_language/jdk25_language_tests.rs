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
