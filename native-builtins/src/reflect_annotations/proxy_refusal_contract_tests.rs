use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// `Class.getName()` spelling. Arrays keep their DESCRIPTOR form
/// (`[Ljava.lang.String;`), which is what makes the measured
/// `class [Ljava.lang.String;` entry in the incompatible-return-types list
/// reproducible. Getting this wrong would have printed
/// `java.lang.String[]` there.
#[test]
fn desc_class_name_matches_class_get_name() {
    assert_eq!(proxy_desc_class_name("I"), "int");
    assert_eq!(proxy_desc_class_name("J"), "long");
    assert_eq!(proxy_desc_class_name("Z"), "boolean");
    assert_eq!(proxy_desc_class_name("V"), "void");
    assert_eq!(
        proxy_desc_class_name("Ljava/lang/String;"),
        "java.lang.String"
    );
    assert_eq!(
        proxy_desc_class_name("[Ljava/lang/String;"),
        "[Ljava.lang.String;"
    );
    assert_eq!(proxy_desc_class_name("[I"), "[I");
    assert_eq!(proxy_desc_class_name("[[J"), "[[J");
}

/// `Class.getTypeName()` spelling — the one `toShortSignature` uses for
/// parameters, where arrays DO become `long[]`.
#[test]
fn desc_type_name_matches_class_get_type_name() {
    assert_eq!(proxy_desc_type_name("[J"), "long[]");
    assert_eq!(
        proxy_desc_type_name("[Ljava/lang/String;"),
        "java.lang.String[]"
    );
    assert_eq!(proxy_desc_type_name("[[I"), "int[][]");
    assert_eq!(proxy_desc_type_name("Ljava/util/List;"), "java.util.List");
    assert_eq!(proxy_desc_type_name("S"), "short");
}

/// Transcribed from HotSpot 25.0.3+9-LTS:
///   `methods with same signature m(int,java.lang.String,long[]) but ...`
///   `methods with same signature q(java.util.List) but ...`
///   `methods with same signature r() but ...`
#[test]
fn short_signature_matches_the_measured_text() {
    assert_eq!(
        proxy_short_signature("m", "ILjava/lang/String;[J"),
        "m(int,java.lang.String,long[])"
    );
    assert_eq!(
        proxy_short_signature("q", "Ljava/util/List;"),
        "q(java.util.List)"
    );
    assert_eq!(proxy_short_signature("r", ""), "r()");
}

#[test]
fn param_descriptors_split_on_the_right_boundaries() {
    assert_eq!(proxy_iter_param_descs(""), Vec::<String>::new());
    assert_eq!(
        proxy_iter_param_descs("ILjava/lang/String;[J"),
        vec!["I", "Ljava/lang/String;", "[J"]
    );
    assert_eq!(
        proxy_iter_param_descs("[[Ljava/lang/Object;Z"),
        vec!["[[Ljava/lang/Object;", "Z"]
    );
    // A truncated descriptor must terminate, not spin.
    assert_eq!(
        proxy_iter_param_descs("Ljava/lang/String"),
        vec!["Ljava/lang/String"]
    );
}

#[test]
fn method_descriptor_splits_into_params_and_return() {
    assert_eq!(
        proxy_split_method_desc("(ILjava/lang/String;)Ljava/lang/Integer;"),
        Some(("ILjava/lang/String;", "Ljava/lang/Integer;"))
    );
    assert_eq!(proxy_split_method_desc("()V"), Some(("", "V")));
    assert_eq!(proxy_split_method_desc("no-parens"), None);
}

/// Array covariance is the difference between two MEASURED rows that must
/// NOT be collapsed: HotSpot accepts `{Object[] u(), String[] u()}` and
/// refuses `{String[] t(), Integer[] t()}`. Both are decided without any
/// class resolution, so this is testable without a VM.
#[test]
fn array_assignability_separates_the_two_measured_array_rows() {
    let probe = |to: &str, from: &str| proxy_desc_assignable_opt(None, to, from);
    assert_eq!(
        probe("[Ljava/lang/Object;", "[Ljava/lang/String;"),
        Some(true)
    );
    assert_eq!(
        probe("[Ljava/lang/String;", "[Ljava/lang/Integer;"),
        None,
        "element assignability needs the VM; the caller must FAIL OPEN"
    );
    assert_eq!(probe("[I", "[I"), Some(true));
    assert_eq!(probe("[I", "[J"), Some(false));
    assert_eq!(probe("Ljava/lang/Object;", "[I"), Some(true));
    assert_eq!(probe("I", "J"), Some(false));
    assert_eq!(probe("V", "Ljava/lang/String;"), Some(false));
    assert_eq!(probe("Ljava/io/Serializable;", "[I"), Some(true));
}

/// The primitive-return arm of the message, transcribed:
/// `... but incompatible return types: int and others` — note it names the
/// PRIMITIVE regardless of which interface was listed first, and that
/// `void` counts as one (`void and others`).
#[test]
fn primitive_return_is_recognised_by_descriptor_shape() {
    for d in ["I", "J", "S", "B", "C", "F", "D", "Z", "V"] {
        assert!(
            !d.starts_with('L') && !d.starts_with('['),
            "{d} must take the primitive arm"
        );
    }
    for d in ["Ljava/lang/String;", "[I", "[Ljava/lang/String;"] {
        assert!(
            d.starts_with('L') || d.starts_with('['),
            "{d} must NOT take the primitive arm"
        );
    }
}

/// The nine names that may be refused from a `Class` mirror carrying no
/// `ClassId`. Anything else must fail OPEN — inventing "X is not an
/// interface" from an unresolvable mirror is how a working path breaks.
#[test]
fn only_primitives_are_refused_by_name() {
    assert!(PROXY_PRIMITIVE_NAMES.contains(&"int"));
    assert!(PROXY_PRIMITIVE_NAMES.contains(&"void"));
    assert_eq!(PROXY_PRIMITIVE_NAMES.len(), 9);
    assert!(!PROXY_PRIMITIVE_NAMES.contains(&"java.lang.String"));
    assert!(!PROXY_PRIMITIVE_NAMES.contains(&""));
}

/// A null `ClassLoader` argument is what arrives as anything other than
/// `Object(Some(_))` at slot 0 — including a MISSING slot 0, which is how
/// a malformed call would otherwise slip past the visibility arm.
#[test]
fn loader_arg_null_detection() {
    assert!(proxy_loader_arg_is_null(&[]));
    assert!(proxy_loader_arg_is_null(&[Value::Object(None)]));
    assert!(proxy_loader_arg_is_null(&[Value::Int(0)]));
}

/// Interpreter round i7 wave 137, lane L3: `referencedTypes` keeps the
/// element class of an array and drops primitives.
#[test]
fn i137_l3_referenced_type_element_classes() {
    assert_eq!(proxy_desc_element_class("Ljava/lang/String;"), Some("java/lang/String"));
    assert_eq!(proxy_desc_element_class("[[Lp/C;"), Some("p/C"));
    assert_eq!(proxy_desc_element_class("[I"), None);
    assert_eq!(proxy_desc_element_class("J"), None);
    assert_eq!(proxy_desc_element_class("V"), None);
}
