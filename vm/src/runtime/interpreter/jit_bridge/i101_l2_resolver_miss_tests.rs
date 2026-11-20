use super::bail_reason_is_resolver_miss;

/// A resolver that answered `None` is a miss, with or without the site's
/// coordinates; a missing resolver, an unwired helper and a backend
/// refusal are not.
#[test]
fn only_resolver_none_sites_read_as_a_miss() {
    assert!(bail_reason_is_resolver_miss("invoke_resolve"));
    assert!(bail_reason_is_resolver_miss("static_field_resolve(pc=12,op=0xb2)"));
    assert!(bail_reason_is_resolver_miss("typecheck_class(pc=3,op=0xc0)"));
    assert!(bail_reason_is_resolver_miss("arm64_cold_field_site(pc=9,op=0xb4)"));
    assert!(!bail_reason_is_resolver_miss("cp_invoke_resolver"));
    assert!(!bail_reason_is_resolver_miss("new_resolve_deferred_unwired"));
    assert!(!bail_reason_is_resolver_miss("unrecorded"));
    assert!(!bail_reason_is_resolver_miss("code_buffer_too_small"));
}
