use super::*;

#[test]
fn external_method_matches_hotspots_spelling() {
    assert_eq!(
        external_method("p.Api", "take", "(Lp/Shared;)I"),
        "int p.Api.take(p.Shared)"
    );
    assert_eq!(
        external_method(
            "PT2$Api",
            "multi",
            "(ILPT2$Shared;[JLjava/lang/String;)[LPT2$Shared;"
        ),
        "PT2$Shared[] PT2$Api.multi(int, PT2$Shared, long[], java.lang.String)"
    );
    assert_eq!(external_method("p.A", "m", "()V"), "void p.A.m()");
}

#[test]
fn the_override_message_is_hotspots_vtable_wording() {
    let finding = MemberConstraintFinding {
        what: "override",
        accessor_id: ClassId::new(1),
        declaring_id: ClassId::new(2),
        member: "take".to_string(),
        descriptor: "(Lp/Shared;)I".to_string(),
        violated: Some("p/Shared".to_string()),
        pending: Vec::new(),
        itable: None,
    };
    assert_eq!(
        resolution_message(
            &finding,
            "p/Shared",
            "p.Sub",
            "'a' @1",
            "p.Base",
            "'b' @2",
            "p.Sub is in unnamed module of loader 'a' @1, parent loader 'b' @2",
            "p.Base is in unnamed module of loader 'b' @2, parent loader 'app'",
        ),
        "loader constraint violation for class p.Sub: when selecting overriding method \
         'int p.Sub.take(p.Shared)' the class loader 'a' @1 of the selected method's type \
         p.Sub, and the class loader 'b' @2 for its super type p.Base have different Class \
         objects for the type p.Shared used in the signature (p.Sub is in unnamed module of \
         loader 'a' @1, parent loader 'b' @2; p.Base is in unnamed module of loader 'b' @2, \
         parent loader 'app')"
    );
}

#[test]
fn the_itable_message_is_hotspots_wording() {
    let finding = MemberConstraintFinding {
        what: "itable",
        accessor_id: ClassId::new(1),
        declaring_id: ClassId::new(2),
        member: "take".to_string(),
        descriptor: "(Lp/Shared;)I".to_string(),
        violated: Some("p/Shared".to_string()),
        pending: Vec::new(),
        itable: Some(ItableSite {
            linked_name: "p.Impl2".to_string(),
            selected_kind: "class",
        }),
    };
    assert_eq!(
        resolution_message(
            &finding,
            "p/Shared",
            "p.Mid",
            "'a' @1",
            "p.Api",
            "'b' @2",
            "p.Mid is in unnamed module of loader 'a' @1, parent loader 'b' @2",
            "p.Api is in unnamed module of loader 'b' @2, parent loader 'app'",
        ),
        "loader constraint violation in interface itable initialization for class p.Impl2: \
         when selecting method 'int p.Api.take(p.Shared)' the class loader 'b' @2 for super \
         interface p.Api, and the class loader 'a' @1 of the selected method's class, p.Mid \
         have different Class objects for the type p.Shared used in the signature (p.Api is \
         in unnamed module of loader 'b' @2, parent loader 'app'; p.Mid is in unnamed module \
         of loader 'a' @1, parent loader 'b' @2)"
    );
}

#[test]
fn the_method_message_prints_the_type_in_internal_form() {
    let finding = MemberConstraintFinding {
        what: "method",
        accessor_id: ClassId::new(1),
        declaring_id: ClassId::new(2),
        member: "take".to_string(),
        descriptor: "(Lq/Shared;)I".to_string(),
        violated: Some("q/Shared".to_string()),
        pending: Vec::new(),
        itable: None,
    };
    assert_eq!(
        resolution_message(
            &finding,
            "q/Shared",
            "p.User",
            "'a' @1",
            "p.Api",
            "'b' @2",
            "p.User is in unnamed module of loader 'a' @1, parent loader 'b' @2",
            "p.Api is in unnamed module of loader 'b' @2, parent loader 'app'",
        ),
        "loader constraint violation: when resolving method 'int p.Api.take(q.Shared)' the \
         class loader 'a' @1 of the current class, p.User, and the class loader 'b' @2 for \
         the method's defining class, p.Api, have different Class objects for the type \
         q/Shared used in the signature (p.User is in unnamed module of loader 'a' @1, parent \
         loader 'b' @2; p.Api is in unnamed module of loader 'b' @2, parent loader 'app')"
    );
}

#[test]
fn external_field_type_keeps_an_array_descriptor() {
    assert_eq!(external_field_type("Lp/Shared;"), "p.Shared");
    assert_eq!(external_field_type("[Lp/Shared;"), "[Lp.Shared;");
}
