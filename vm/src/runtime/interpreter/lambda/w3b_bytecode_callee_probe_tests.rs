#[test]
fn bytecode_callee_probes_use_memoized_jit_cache_get() {
    let src = include_str!("../lambda.rs");
    let nominate_body = src
        .split("fn bytecode_callee_compiled_or_nominate")
        .nth(1)
        .and_then(|s| s.split("fn tiny_ctor_tier_up_threshold").next())
        .expect("nominate fn present");
    assert!(
        nominate_body.contains("super::invoke_fast::jit_cache_get_memoized("),
        "bytecode_callee_compiled_or_nominate must route through jit_cache_get_memoized"
    );
    assert!(
        !nominate_body.contains("shared.jit.jit_cache.read().get("),
        "bytecode_callee_compiled_or_nominate must not use bare jit_cache.read().get"
    );
}
