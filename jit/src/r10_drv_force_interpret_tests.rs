use super::force_interpret_matches;

fn list(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn a_deny_entry_matches_a_substring_of_class_dot_method() {
    let deny = list(&["org/h2/mvstore/MVStore.commit"]);
    assert!(force_interpret_matches(
        Some(deny.as_slice()),
        None,
        "org/h2/mvstore/MVStore",
        "commit"
    ));
    assert!(!force_interpret_matches(
        Some(deny.as_slice()),
        None,
        "org/h2/mvstore/MVStore",
        "rollback"
    ));
    // A bare package pins every method in it.
    let pkg = list(&["org/keycloak/"]);
    assert!(force_interpret_matches(
        Some(pkg.as_slice()),
        None,
        "org/keycloak/Realm",
        "id"
    ));
    assert!(!force_interpret_matches(
        Some(pkg.as_slice()),
        None,
        "org/other/Realm",
        "id"
    ));
}

#[test]
fn an_empty_deny_entry_would_deny_everything() {
    // The hazard `jit_deny_filter` now filters out at parse time: a
    // substring test against "" is true for every method, so one stray
    // separator in `CRATONVM_JIT_DENY` is `--nojit` for the whole run.
    // This pins the MATCHER's behaviour so the parser's filter stays the
    // thing that prevents it.
    let with_empty = list(&["org/h2/", ""]);
    assert!(force_interpret_matches(
        Some(with_empty.as_slice()),
        None,
        "com/entirely/Unrelated",
        "m"
    ));
    let parsed_as_the_filter_now_does = list(&["org/h2/"]);
    assert!(!force_interpret_matches(
        Some(parsed_as_the_filter_now_does.as_slice()),
        None,
        "com/entirely/Unrelated",
        "m"
    ));
}

#[test]
fn bisect_only_is_a_class_name_prefix_allowlist() {
    let only = list(&["org/h2/"]);
    assert!(!force_interpret_matches(
        None,
        Some(only.as_slice()),
        "org/h2/Store",
        "m"
    ));
    assert!(force_interpret_matches(
        None,
        Some(only.as_slice()),
        "org/other/Store",
        "m"
    ));
    // A prefix matching nothing force-interprets everything — which is
    // what the 2026-08-04 measurement in `jit_force_interpret`'s doc
    // expected and did not get from the doors that skipped this predicate.
    let nothing = list(&["zzzNoSuchPrefix"]);
    assert!(force_interpret_matches(
        None,
        Some(nothing.as_slice()),
        "org/h2/Store",
        "m"
    ));
}

#[test]
fn both_levers_unset_never_force_interprets() {
    assert!(!force_interpret_matches(None, None, "any/Class", "any"));
}

#[test]
fn deny_wins_over_an_allowing_bisect_only() {
    let deny = list(&["org/h2/Store.commit"]);
    let only = list(&["org/h2/"]);
    assert!(force_interpret_matches(
        Some(deny.as_slice()),
        Some(only.as_slice()),
        "org/h2/Store",
        "commit"
    ));
}
