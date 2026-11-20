use super::*;

#[test]
fn ways_are_in_range_for_extreme_keys() {
    for key in [0u128, 1, u128::MAX, 1u128 << 64, (1u128 << 64) - 1] {
        assert!(compiled_neg_way(key) < COMPILED_NEG_MEMO_WAYS);
    }
}

/// The memo is only sound while a miss is stamped with the name-keyed
/// publish stamp, read before the probe.
#[test]
fn miss_is_stamped_before_the_probe_and_keyed_by_vm_and_method() {
    let src = include_str!("../invoke_fast.rs");
    let body = src
        .split("pub(super) fn jit_cache_get_memoized")
        .nth(1)
        .and_then(|s| s.split("#[cfg(test)]").next())
        .expect("memo fn present");
    let stamp = body.find("callee_publish_stamp").expect("stamp read");
    let probe = body.find("jit_cache.read().get(").expect("probe");
    assert!(stamp < probe, "stamp must be read before the probe");
    assert!(body.contains("shared.vm_identity") && body.contains("invoc_key"));
}
