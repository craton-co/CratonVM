use super::*;

/// `descriptor_param_slots` is the allocation-free form of "sum the slot
/// widths of `param_types_from_descriptor`": same answer on well-formed and
/// malformed descriptors alike (SBR round 5, lane verify).
#[test]
fn descriptor_param_slots_matches_the_parsed_parameter_types() {
    let descriptors = [
        "()V",
        "(I)V",
        "(J)J",
        "(DJ)V",
        "(IJLjava/lang/String;D[I[[J)Ljava/lang/Object;",
        "([J[D)V",
        "(Ljava/util/List;Ljava/util/Map;)V",
        // malformed: the walk stops at the same place for both
        "",
        "I",
        "(",
        "(J",
        "(JQ)V",
        "(Ljava/lang/String)V",
        "(L;)V",
        "(JLjava/lang/Str.ing;D)V",
        "(J[)V",
        "(JD",
        ")V",
        "(Z)",
    ];
    for d in descriptors {
        let by_types: usize = param_types_from_descriptor(d)
            .iter()
            .map(|t| if t.is_category2() { 2 } else { 1 })
            .sum();
        assert_eq!(descriptor_param_slots(d), by_types, "descriptor {d:?}");
    }
}

/// Mock class hierarchy for testing.
struct MockHierarchy;

impl ClassHierarchy for MockHierarchy {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        // Simple hierarchy: String <: Object, ArrayList <: Object, Integer <: Number <: Object
        if child == parent {
            return true;
        }
        // Every reference type (including interfaces) is a subclass of
        // java.lang.Object.  The real ClassStoreHierarchy mirrors this.
        if parent == "java/lang/Object" {
            return true;
        }
        matches!(
            (child, parent),
            ("java/lang/String", "java/lang/Object")
                | ("java/util/ArrayList", "java/lang/Object")
                | ("java/lang/Integer", "java/lang/Number")
                | ("java/lang/Integer", "java/lang/Object")
                | ("java/lang/Number", "java/lang/Object")
        )
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        if a == b {
            return a.to_string();
        }
        if self.is_subclass(a, b) {
            return b.to_string();
        }
        if self.is_subclass(b, a) {
            return a.to_string();
        }
        // For our mock, everything meets at Object
        "java/lang/Object".to_string()
    }

    fn is_interface(&self, name: &str) -> bool {
        name == "java/io/Serializable" || name == "java/lang/Cloneable"
    }
}

// --- from_field_descriptor ---

#[test]
fn from_descriptor_int() {
    assert_eq!(VType::from_field_descriptor("I"), VType::Int);
}

#[test]
fn from_descriptor_byte() {
    assert_eq!(VType::from_field_descriptor("B"), VType::Int);
}

#[test]
fn from_descriptor_char() {
    assert_eq!(VType::from_field_descriptor("C"), VType::Int);
}

#[test]
fn from_descriptor_short() {
    assert_eq!(VType::from_field_descriptor("S"), VType::Int);
}

#[test]
fn from_descriptor_boolean() {
    assert_eq!(VType::from_field_descriptor("Z"), VType::Int);
}

#[test]
fn from_descriptor_long() {
    assert_eq!(VType::from_field_descriptor("J"), VType::Long);
}

#[test]
fn from_descriptor_float() {
    assert_eq!(VType::from_field_descriptor("F"), VType::Float);
}

#[test]
fn from_descriptor_double() {
    assert_eq!(VType::from_field_descriptor("D"), VType::Double);
}

#[test]
fn from_descriptor_object() {
    assert_eq!(
        VType::from_field_descriptor("Ljava/lang/String;"),
        VType::ObjectRef(Arc::from("java/lang/String"))
    );
}

#[test]
fn from_descriptor_int_array() {
    assert_eq!(
        VType::from_field_descriptor("[I"),
        VType::ArrayRef(Arc::from("[I"))
    );
}

#[test]
fn from_descriptor_object_array() {
    assert_eq!(
        VType::from_field_descriptor("[Ljava/lang/Object;"),
        VType::ArrayRef(Arc::from("[Ljava/lang/Object;"))
    );
}

#[test]
fn from_descriptor_2d_array() {
    assert_eq!(
        VType::from_field_descriptor("[[I"),
        VType::ArrayRef(Arc::from("[[I"))
    );
}

// --- is_category2 ---

#[test]
fn category2_long() {
    assert!(VType::Long.is_category2());
}

#[test]
fn category2_double() {
    assert!(VType::Double.is_category2());
}

#[test]
fn category2_int_is_not() {
    assert!(!VType::Int.is_category2());
}

// --- is_reference ---

#[test]
fn is_reference_object() {
    assert!(VType::ObjectRef(Arc::from("Foo")).is_reference());
}

#[test]
fn is_reference_null() {
    assert!(VType::Null.is_reference());
}

#[test]
fn is_reference_array() {
    assert!(VType::ArrayRef(Arc::from("[I")).is_reference());
}

#[test]
fn is_reference_int_is_not() {
    assert!(!VType::Int.is_reference());
}

// --- is_assignable_to ---

#[test]
fn assignable_same_type() {
    let h = MockHierarchy;
    assert!(VType::Int.is_assignable_to(&VType::Int, &h));
    assert!(VType::Long.is_assignable_to(&VType::Long, &h));
}

#[test]
fn null_assignable_to_object() {
    let h = MockHierarchy;
    assert!(VType::Null.is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/Object")), &h));
}

#[test]
fn null_assignable_to_array() {
    let h = MockHierarchy;
    assert!(VType::Null.is_assignable_to(&VType::ArrayRef(Arc::from("[I")), &h));
}

#[test]
fn subclass_assignable_to_superclass() {
    let h = MockHierarchy;
    assert!(VType::ObjectRef(Arc::from("java/lang/String"))
        .is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/Object")), &h));
}

#[test]
fn superclass_not_assignable_to_subclass() {
    let h = MockHierarchy;
    assert!(!VType::ObjectRef(Arc::from("java/lang/Object"))
        .is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/String")), &h));
}

#[test]
fn array_assignable_to_object() {
    let h = MockHierarchy;
    assert!(VType::ArrayRef(Arc::from("[I"))
        .is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/Object")), &h));
}

#[test]
fn array_assignable_to_serializable() {
    let h = MockHierarchy;
    assert!(VType::ArrayRef(Arc::from("[I"))
        .is_assignable_to(&VType::ObjectRef(Arc::from("java/io/Serializable")), &h));
}

#[test]
fn covariant_reference_array() {
    let h = MockHierarchy;
    // [String is assignable to [Object
    assert!(VType::ArrayRef(Arc::from("[Ljava/lang/String;"))
        .is_assignable_to(&VType::ArrayRef(Arc::from("[Ljava/lang/Object;")), &h));
}

#[test]
fn primitive_array_not_covariant() {
    let h = MockHierarchy;
    assert!(!VType::ArrayRef(Arc::from("[I"))
        .is_assignable_to(&VType::ArrayRef(Arc::from("[J")), &h));
}

#[test]
fn int_not_assignable_to_long() {
    let h = MockHierarchy;
    assert!(!VType::Int.is_assignable_to(&VType::Long, &h));
}

#[test]
fn top_not_assignable_to_int() {
    let h = MockHierarchy;
    assert!(!VType::Top.is_assignable_to(&VType::Int, &h));
}

#[test]
fn any_type_assignable_to_top() {
    // Per JVMS 4.10.1.2, Top is the top of the verification type lattice.
    // StackMapTable frames may declare a slot as Top to indicate the value
    // is unused past this merge point; any incoming type must widen to Top.
    let h = MockHierarchy;
    assert!(VType::Int.is_assignable_to(&VType::Top, &h));
    assert!(VType::Long.is_assignable_to(&VType::Top, &h));
    assert!(VType::Null.is_assignable_to(&VType::Top, &h));
    assert!(VType::ObjectRef(Arc::from("java/lang/String")).is_assignable_to(&VType::Top, &h));
    assert!(VType::ArrayRef(Arc::from("[I")).is_assignable_to(&VType::Top, &h));
}

// --- Interface relaxation (JVMS §4.10.1.2) ----------------------------
//
// The verifier does not distinguish between interface types and
// java.lang.Object in its tracking of reference types. Therefore:
//   * Object.is_assignable_to(SomeInterface) → true
//   * SomeClass.is_assignable_to(SomeInterface-it-does-not-implement)
//       → true
//   * SomeInterface.is_assignable_to(Object) → true (always did)
// Runtime checkcast / invokeinterface enforce the actual type.

#[test]
fn object_assignable_to_interface() {
    // Object.is_assignable_to(SomeInterface) → true (relaxation rule).
    let h = MockHierarchy;
    let obj = VType::ObjectRef(Arc::from("java/lang/Object"));
    let iface = VType::ObjectRef(Arc::from("java/io/Serializable"));
    assert!(obj.is_assignable_to(&iface, &h));
}

#[test]
fn interface_assignable_to_object() {
    // SomeInterface.is_assignable_to(Object) → true (always was).
    let h = MockHierarchy;
    let iface = VType::ObjectRef(Arc::from("java/io/Serializable"));
    let obj = VType::ObjectRef(Arc::from("java/lang/Object"));
    assert!(iface.is_assignable_to(&obj, &h));
}

#[test]
fn class_assignable_to_unrelated_interface() {
    // SomeClass.is_assignable_to(Interface it doesn't implement) → true
    // at verify time per JVMS §4.10.1.2 relaxation.
    //
    // In MockHierarchy, "java/lang/String" is NOT declared as
    // implementing "java/lang/Cloneable" via is_subclass, yet verify-
    // time assignment must still succeed because the target is an
    // interface.
    let h = MockHierarchy;
    let s = VType::ObjectRef(Arc::from("java/lang/String"));
    let iface = VType::ObjectRef(Arc::from("java/lang/Cloneable"));
    // Sanity: MockHierarchy does NOT model String <: Cloneable.
    assert!(!h.is_subclass("java/lang/String", "java/lang/Cloneable"));
    // Yet is_assignable_to must accept it (interface relaxation).
    assert!(s.is_assignable_to(&iface, &h));
}

#[test]
fn array_assignable_to_arbitrary_interface() {
    // Arrays are reference types; assignment to ANY interface is
    // accepted at verify time. The existing Serializable/Cloneable
    // paths were already accepted; this ensures generic interface
    // targets also succeed via is_interface().
    struct OnlyInterfaceHierarchy;
    impl ClassHierarchy for OnlyInterfaceHierarchy {
        fn is_subclass(&self, c: &str, p: &str) -> bool {
            c == p || p == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, name: &str) -> bool {
            name == "my/pkg/IFoo"
        }
    }
    let h = OnlyInterfaceHierarchy;
    let arr = VType::ArrayRef(Arc::from("[I"));
    let iface = VType::ObjectRef(Arc::from("my/pkg/IFoo"));
    assert!(arr.is_assignable_to(&iface, &h));
}

// --- RVERIF.2: known-JDK-interface fallback ----------------------------
//
// When a JDK interface is loaded as a synthetic stub (because its real
// .class wasn't yet read at verify time), the stub heuristic can flag
// it as PUBLIC|SUPER instead of INTERFACE — which makes the existing
// `is_interface(parent)` relaxation miss and breaks legitimate
// List → Collection / ArrayList → Iterable assignments. The
// `is_known_jdk_interface` static table is the verifier's safety net
// for those cases.

/// Hierarchy that mimics the failure mode: classes are loaded but
/// `is_subclass` does not see the interface relationship and
/// `is_interface` returns `false` for everything (because the stub
/// access flags are wrong).
struct StubHierarchyMissingInterfaceFlag;
impl ClassHierarchy for StubHierarchyMissingInterfaceFlag {
    fn is_subclass(&self, c: &str, p: &str) -> bool {
        // Same class or anything-to-Object only.
        c == p || p == "java/lang/Object"
    }
    fn common_superclass(&self, _a: &str, _b: &str) -> String {
        "java/lang/Object".to_string()
    }
    fn is_interface(&self, _name: &str) -> bool {
        false
    }
}

#[test]
fn list_assignable_to_collection_via_jdk_table() {
    // RVERIF.2 regression: this is the canonical case from
    // org/jboss/modules/Module.getResources at bytecode offset 294 —
    // `Collections.enumeration(Collection)` invoked with a List on
    // the stack. With a stub-only hierarchy that has neither the
    // subclass relationship nor the INTERFACE flag, the verifier
    // must still accept the assignment because Collection is a
    // well-known JDK interface.
    let h = StubHierarchyMissingInterfaceFlag;
    let list = VType::ObjectRef(Arc::from("java/util/List"));
    let collection = VType::ObjectRef(Arc::from("java/util/Collection"));

    // Sanity: the hierarchy genuinely cannot prove the relationship.
    assert!(!h.is_subclass("java/util/List", "java/util/Collection"));
    assert!(!h.is_interface("java/util/Collection"));

    // Yet the verifier accepts the assignment.
    assert!(
        list.is_assignable_to(&collection, &h),
        "List must be assignable to Collection (JDK-interface fallback)"
    );
}

#[test]
fn arraylist_assignable_to_iterable_via_jdk_table() {
    // Concrete class to interface, both via JDK fallback.
    let h = StubHierarchyMissingInterfaceFlag;
    let al = VType::ObjectRef(Arc::from("java/util/ArrayList"));
    let iter = VType::ObjectRef(Arc::from("java/lang/Iterable"));
    assert!(al.is_assignable_to(&iter, &h));
}

#[test]
fn runtime_permission_assignable_to_permission_via_jdk_table() {
    // WildFly's JBossThread.onExit calls
    // SecurityManager.checkPermission(Permission) with a RuntimePermission
    // static. When RuntimePermission is still represented by sparse
    // bootstrap metadata, the verifier must still accept this real JDK
    // superclass edge.
    let h = StubHierarchyMissingInterfaceFlag;
    let runtime_permission = VType::ObjectRef(Arc::from("java/lang/RuntimePermission"));
    let permission = VType::ObjectRef(Arc::from("java/security/Permission"));

    assert!(!h.is_subclass("java/lang/RuntimePermission", "java/security/Permission"));
    assert!(runtime_permission.is_assignable_to(&permission, &h));
}

#[test]
fn unknown_class_not_assignable_to_unknown_class() {
    // Negative control: the JDK fallback must NOT accept arbitrary
    // class-to-class assignments. Only known JDK interfaces relax
    // the check.
    let h = StubHierarchyMissingInterfaceFlag;
    let foo = VType::ObjectRef(Arc::from("com/example/Foo"));
    let bar = VType::ObjectRef(Arc::from("com/example/Bar"));
    assert!(!foo.is_assignable_to(&bar, &h));
}

// --- RVERIF.3 soundness regressions -----------------------------------

#[test]
fn uninitialized_not_assignable_to_object() {
    // The classic uninitialized-object attack: an uninitialized object
    // must NOT be assignable where an initialized reference is
    // expected (JVMS §4.10.1.2). It becomes an ObjectRef only after
    // the verifier sees the matching invokespecial <init>.
    let h = MockHierarchy;
    let obj = VType::ObjectRef(Arc::from("java/lang/Object"));
    assert!(!VType::UninitializedThis.is_assignable_to(&obj, &h));
    assert!(!VType::Uninitialized(7).is_assignable_to(&obj, &h));
}

#[test]
fn uninitialized_assignable_only_to_itself() {
    // An uninitialized type is compatible with itself (equality) but
    // nothing else among reference types.
    let h = MockHierarchy;
    assert!(VType::UninitializedThis.is_assignable_to(&VType::UninitializedThis, &h));
    assert!(VType::Uninitialized(3).is_assignable_to(&VType::Uninitialized(3), &h));
    assert!(!VType::Uninitialized(3).is_assignable_to(&VType::Uninitialized(4), &h));
}

#[test]
fn unresolved_type_does_not_bypass_assignability() {
    // RVERIF.3: a hierarchy that reports a type as not-yet-resolvable
    // must NOT make an otherwise-unprovable reference assignment pass.
    struct UnresolvedHierarchy;
    impl ClassHierarchy for UnresolvedHierarchy {
        fn is_subclass(&self, c: &str, p: &str) -> bool {
            c == p || p == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
        fn is_resolvable(&self, _name: &str) -> bool {
            false // everything unresolved
        }
    }
    let h = UnresolvedHierarchy;
    let foo = VType::ObjectRef(Arc::from("com/example/Foo"));
    let bar = VType::ObjectRef(Arc::from("com/example/Bar"));
    // Unprovable class-to-class assignment must be rejected even
    // though both types are "unresolved".
    assert!(!foo.is_assignable_to(&bar, &h));
    // Same for reference-array element types.
    let afoo = VType::ArrayRef(Arc::from("[Lcom/example/Foo;"));
    let abar = VType::ArrayRef(Arc::from("[Lcom/example/Bar;"));
    assert!(!afoo.is_assignable_to(&abar, &h));
}

#[test]
fn param_types_from_empty_descriptor_does_not_panic() {
    // RVERIF.3: a zero-length descriptor must not panic on slicing.
    assert!(param_types_from_descriptor("").is_empty());
    assert!(param_types_from_descriptor("(").is_empty());
    assert!(param_types_from_descriptor(")V").is_empty());
}

#[test]
fn known_jdk_interface_recognises_collection_framework() {
    // Spot-check that the table covers the names that
    // Module.getResources / Collections / Stream APIs rely on.
    assert!(is_known_jdk_interface("java/util/Collection"));
    assert!(is_known_jdk_interface("java/util/List"));
    assert!(is_known_jdk_interface("java/util/Map"));
    assert!(is_known_jdk_interface("java/util/Set"));
    assert!(is_known_jdk_interface("java/util/Iterator"));
    assert!(is_known_jdk_interface("java/util/Enumeration"));
    assert!(is_known_jdk_interface("java/lang/Iterable"));
    assert!(is_known_jdk_interface("java/util/function/Function"));
    assert!(is_known_jdk_interface("java/util/stream/Stream"));
    // Negative: a concrete class must NOT be in the table.
    assert!(!is_known_jdk_interface("java/util/ArrayList"));
    assert!(!is_known_jdk_interface("java/lang/Object"));
    assert!(!is_known_jdk_interface("java/lang/String"));
}

// --- merge ---

#[test]
fn merge_same_type() {
    let h = MockHierarchy;
    assert_eq!(VType::Int.merge(&VType::Int, &h), VType::Int);
}

#[test]
fn merge_null_with_object() {
    let h = MockHierarchy;
    let obj = VType::ObjectRef(Arc::from("java/lang/String"));
    assert_eq!(VType::Null.merge(&obj, &h), obj);
    assert_eq!(obj.merge(&VType::Null, &h), obj);
}

#[test]
fn merge_two_objects() {
    let h = MockHierarchy;
    let s = VType::ObjectRef(Arc::from("java/lang/String"));
    let al = VType::ObjectRef(Arc::from("java/util/ArrayList"));
    assert_eq!(
        s.merge(&al, &h),
        VType::ObjectRef(Arc::from("java/lang/Object"))
    );
}

#[test]
fn merge_subclass_with_superclass() {
    let h = MockHierarchy;
    let integer = VType::ObjectRef(Arc::from("java/lang/Integer"));
    let number = VType::ObjectRef(Arc::from("java/lang/Number"));
    assert_eq!(
        integer.merge(&number, &h),
        VType::ObjectRef(Arc::from("java/lang/Number"))
    );
}

#[test]
fn merge_int_with_long_is_top() {
    let h = MockHierarchy;
    assert_eq!(VType::Int.merge(&VType::Long, &h), VType::Top);
}

#[test]
fn merge_array_with_object_is_object() {
    let h = MockHierarchy;
    let arr = VType::ArrayRef(Arc::from("[I"));
    let obj = VType::ObjectRef(Arc::from("java/lang/String"));
    assert_eq!(
        arr.merge(&obj, &h),
        VType::ObjectRef(Arc::from("java/lang/Object"))
    );
}

#[test]
fn merge_two_reference_arrays() {
    let h = MockHierarchy;
    let a = VType::ArrayRef(Arc::from("[Ljava/lang/String;"));
    let b = VType::ArrayRef(Arc::from("[Ljava/util/ArrayList;"));
    assert_eq!(
        a.merge(&b, &h),
        VType::ArrayRef(Arc::from("[Ljava/lang/Object;"))
    );
}

#[test]
fn merge_different_primitive_arrays() {
    let h = MockHierarchy;
    let a = VType::ArrayRef(Arc::from("[I"));
    let b = VType::ArrayRef(Arc::from("[J"));
    assert_eq!(
        a.merge(&b, &h),
        VType::ObjectRef(Arc::from("java/lang/Object"))
    );
}

// --- param_types_from_descriptor ---

#[test]
fn param_types_empty() {
    assert!(param_types_from_descriptor("()V").is_empty());
}

#[test]
fn param_types_single_int() {
    assert_eq!(param_types_from_descriptor("(I)V"), vec![VType::Int]);
}

#[test]
fn param_types_mixed() {
    let types = param_types_from_descriptor("(ILjava/lang/String;[DJ)V");
    assert_eq!(
        types,
        vec![
            VType::Int,
            VType::ObjectRef(Arc::from("java/lang/String")),
            VType::ArrayRef(Arc::from("[D")),
            VType::Long,
        ]
    );
}

#[test]
fn param_types_2d_array() {
    let types = param_types_from_descriptor("([[I)V");
    assert_eq!(types, vec![VType::ArrayRef(Arc::from("[[I"))]);
}

// --- return_type_from_descriptor ---

#[test]
fn return_type_void() {
    assert_eq!(return_type_from_descriptor("()V"), None);
}

#[test]
fn return_type_int() {
    assert_eq!(return_type_from_descriptor("()I"), Some(VType::Int));
}

#[test]
fn return_type_object() {
    assert_eq!(
        return_type_from_descriptor("()Ljava/lang/String;"),
        Some(VType::ObjectRef(Arc::from("java/lang/String")))
    );
}

#[test]
fn return_type_array() {
    assert_eq!(
        return_type_from_descriptor("()[I"),
        Some(VType::ArrayRef(Arc::from("[I")))
    );
}

// --- descriptor well-formedness (JVMS §4.3.2 / §4.3.3) ---

#[test]
fn valid_field_descriptors_accepted() {
    for d in [
        "I",
        "J",
        "D",
        "F",
        "B",
        "C",
        "S",
        "Z",
        "[I",
        "[[J",
        "Ljava/lang/String;",
        "[Ljava/lang/Object;",
        "[[Ljava/util/Map$Entry;",
    ] {
        assert!(is_valid_field_descriptor(d), "{d} should be valid");
    }
}

#[test]
fn malformed_field_descriptors_rejected() {
    for d in [
        "",                    // empty
        "V",                   // void is not a field type
        "Q",                   // unknown tag
        "L",                   // bare L
        "Ljava/lang/String",   // unterminated
        "L;",                  // empty class name
        "Lja.va/Foo;",         // '.' is illegal in an internal name
        "[",                   // dangling array marker
        "II",                  // trailing garbage
        "Ljava/lang/String;X", // trailing garbage
    ] {
        assert!(!is_valid_field_descriptor(d), "{d} should be rejected");
    }
}

#[test]
fn valid_method_descriptors_accepted() {
    for d in [
        "()V",
        "()I",
        "(I)V",
        "(IJ)Ljava/lang/String;",
        "([[Ljava/lang/Object;D)[I",
        "(Ljava/lang/String;Ljava/lang/String;)Z",
    ] {
        assert!(is_valid_method_descriptor(d), "{d} should be valid");
    }
}

#[test]
fn malformed_method_descriptors_rejected() {
    for d in [
        "",
        "V",
        "()",                   // no return type
        "(I",                   // no ')'
        "I)V",                  // no '('
        "()VV",                 // trailing garbage
        "(Ljava/lang/String)V", // unterminated parameter
        "(Q)V",                 // unknown parameter tag
        "()Q",                  // unknown return tag
    ] {
        assert!(!is_valid_method_descriptor(d), "{d} should be rejected");
    }
}

#[test]
fn unterminated_object_parameter_does_not_panic() {
    // REGRESSION: the old walk computed the parameter's end as
    // `i + find(';').unwrap_or(len - i) + 1`, i.e. `len + 1` when the `;`
    // is missing — an out-of-range slice, reachable from any attacker
    // supplied `NameAndType` descriptor. Must return cleanly instead.
    assert!(param_types_from_descriptor("(Ljava/lang/String)V").is_empty());
    assert!(param_types_from_descriptor("([Ljava/lang/String)V").is_empty());
    assert!(param_types_from_descriptor("(L)V").is_empty());
    assert!(param_types_from_descriptor("([").is_empty());
}

#[test]
fn well_formed_parameters_still_parse() {
    assert_eq!(
        param_types_from_descriptor("(IJLjava/lang/String;[D)V"),
        vec![
            VType::Int,
            VType::Long,
            VType::ObjectRef(Arc::from("java/lang/String")),
            VType::ArrayRef(Arc::from("[D")),
        ]
    );
}

#[test]
fn deeply_nested_array_descriptor_is_bounded() {
    // JVMS §4.4.1 caps array descriptors at 255 dimensions; a longer one is
    // malformed rather than an unbounded recursion / allocation source.
    let ok = format!("{}I", "[".repeat(255));
    let too_deep = format!("{}I", "[".repeat(256));
    assert!(is_valid_field_descriptor(&ok));
    assert!(!is_valid_field_descriptor(&too_deep));
}

/// The loads HotSpot's `is_reference_assignable_from` makes, as the
/// define-time hierarchy records them for the VM's link step (the i47
/// page): the target unless it is `Object`, the source only for a class
/// target, array components by the same rule, nothing for an equal pair,
/// a `null` source or a hierarchy that does not record.
#[test]
fn assignability_records_hotspots_verifier_loads() {
    struct Recording(std::cell::RefCell<Vec<String>>, bool);
    impl ClassHierarchy for Recording {
        fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
            true
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, name: &str) -> bool {
            name == "p/Iface"
        }
        fn records_verifier_loads(&self) -> bool {
            self.1
        }
        fn note_verifier_load(&self, name: &str) {
            self.0.borrow_mut().push(name.to_string());
        }
    }
    let obj = |n: &str| VType::ObjectRef(Arc::from(n));
    let arr = |n: &str| VType::ArrayRef(Arc::from(n));
    let loads = |source: VType, target: VType| {
        let h = Recording(std::cell::RefCell::new(Vec::new()), true);
        assert!(source.is_assignable_to(&target, &h));
        h.0.into_inner()
    };
    assert_eq!(loads(obj("p/Impl"), obj("p/Sup")), ["p/Sup", "p/Impl"]);
    assert_eq!(loads(obj("p/Impl"), obj("p/Iface")), ["p/Iface"]);
    assert_eq!(
        loads(obj("p/Impl"), obj("java/util/List")),
        ["java/util/List"],
        "a JDK interface the static table knows"
    );
    assert!(loads(obj("p/Impl"), obj("java/lang/Object")).is_empty());
    assert!(loads(obj("p/Impl"), obj("p/Impl")).is_empty());
    assert!(loads(VType::Null, obj("p/Sup")).is_empty());
    assert_eq!(
        loads(arr("[Lp/Impl;"), obj("java/lang/Cloneable")),
        ["java/lang/Cloneable"]
    );
    assert_eq!(
        loads(arr("[Lp/Impl;"), arr("[Lp/Sup;")),
        ["p/Sup", "p/Impl"]
    );
    assert_eq!(
        loads(arr("[[Lp/Impl;"), arr("[[Lp/Sup;")),
        ["p/Sup", "p/Impl"]
    );
    assert!(loads(arr("[Lp/Impl;"), arr("[Ljava/lang/Object;")).is_empty());
    let quiet = Recording(std::cell::RefCell::new(Vec::new()), false);
    assert!(obj("p/Impl").is_assignable_to(&obj("p/Sup"), &quiet));
    assert!(quiet.0.into_inner().is_empty());
}

/// A target not loaded at define time may be an interface, for which
/// HotSpot never loads the source: the source load is recorded as
/// conditional on the target, not made unconditionally (the Spring
/// `GroovyObject` regression on dev 19dc529626). A loaded class target
/// still records the source outright.
#[test]
fn unloaded_target_defers_the_source_load_to_link() {
    struct Partial(std::cell::RefCell<Vec<String>>);
    impl ClassHierarchy for Partial {
        fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
            true
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
        fn is_resolvable(&self, name: &str) -> bool {
            name == "p/LoadedSup"
        }
        fn records_verifier_loads(&self) -> bool {
            true
        }
        fn note_verifier_load(&self, name: &str) {
            self.0.borrow_mut().push(name.to_string());
        }
        fn note_verifier_load_unless_interface(&self, target: &str, source: &str) {
            self.0.borrow_mut().push(format!("{source}?{target}"));
        }
    }
    let obj = |n: &str| VType::ObjectRef(Arc::from(n));
    let loads = |source: VType, target: VType| {
        let h = Partial(std::cell::RefCell::new(Vec::new()));
        let _ = source.is_assignable_to(&target, &h);
        h.0.into_inner()
    };
    assert_eq!(
        loads(obj("p/GroovyReader"), obj("p/MaybeIface")),
        ["p/MaybeIface", "p/GroovyReader?p/MaybeIface"]
    );
    assert_eq!(
        loads(obj("p/Impl"), obj("p/LoadedSup")),
        ["p/LoadedSup", "p/Impl"]
    );
}
