use super::*;

#[test]
fn a_hidden_self_name_names_the_site_under_the_switch_only() {
    let site = "R21Fz9HU";
    let hidden = Some("R21Fz9HU/0x0000000000000004");
    assert!(typecheck_site_names_class_by(hidden, || true, || true, site));
    assert!(
        !typecheck_site_names_class_by(hidden, || true, || false, site),
        "=0 restores the stored-name-only check"
    );
    assert!(
        !typecheck_site_names_class_by(hidden, || false, || true, site),
        "a hidden class whose class-file name is another name does not name the site"
    );
}

#[test]
fn a_stored_name_match_never_asks_the_hidden_rule_or_the_switch() {
    assert!(typecheck_site_names_class_by(
        Some("p/A"),
        || panic!("hidden rule asked on a stored-name match"),
        || panic!("switch read on a stored-name match"),
        "p/A",
    ));
    assert!(!typecheck_site_names_class_by(
        None,
        || panic!("hidden rule asked for an unknown id"),
        || panic!("switch read for an unknown id"),
        "p/A",
    ));
}

/// Every id validation in the compiled type check goes through the one
/// predicate; a stored-name-only copy is what refused the hidden self
/// cast (`R21Fz9HiddenUnload1`).
#[test]
fn the_type_check_validations_use_the_one_predicate() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let src = src.as_str();
    let start = src
        .find("\npub unsafe fn jit_typecheck_holder_site_target(")
        .expect("holder site target");
    let end = src[start..]
        .find("\n}\n")
        .map(|e| start + e + 2)
        .expect("end of the predicate block");
    let resolve = src
        .find("\npub unsafe fn jit_typecheck_resolve(")
        .expect("jit_typecheck_resolve");
    let resolve_end = src[resolve..]
        .find("// Fast path: target already loaded.")
        .map(|e| resolve + e)
        .expect("by-name tail");
    let stale = concat!("c.name == ", "class_name)");
    for body in [&src[start..end], &src[resolve..resolve_end]] {
        assert!(!body.contains(stale), "a stored-name-only site validation is back");
    }
    assert_eq!(
        src[resolve..resolve_end]
            .matches("typecheck_site_names_class(&cm, ")
            .count(),
        2,
        "the array branch and the recorded-site branch"
    );
}
