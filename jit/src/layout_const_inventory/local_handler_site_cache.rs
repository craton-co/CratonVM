use super::*;

/// The whole reason the cache is ONE `u64` and not two `u32`s: a reader
/// must never be able to pair one throwable's class id with another
/// throwable's answer. Round-tripping every field together is what says
/// the packing has that property.
#[test]
fn the_cache_round_trips_class_and_index_together() {
    for (class_id, index) in [(1u32, 0i32), (0, 3), (0x7fff_ffff, -1), (1163, 2)] {
        let raw = JitLocalHandlerSite::encode_cache(class_id, index);
        assert_ne!(
            raw,
            JitLocalHandlerSite::CACHE_EMPTY,
            "a populated cache must be distinguishable from an empty one, \
             including for class id 0 and index -1"
        );
        assert_eq!(
            JitLocalHandlerSite::decode_cache(raw),
            Some((class_id, index)),
            "class {class_id} / index {index} did not survive the packing"
        );
    }
}

/// `-1` — "this frame does not catch it" — is worth caching: a site that
/// keeps propagating must not re-resolve its catch types on every throw.
/// So the empty state cannot be "index is negative"; it is its own bit.
#[test]
fn an_empty_cache_is_not_a_cached_propagate() {
    assert_eq!(
        JitLocalHandlerSite::decode_cache(JitLocalHandlerSite::CACHE_EMPTY),
        None
    );
    let cached_propagate = JitLocalHandlerSite::encode_cache(7, -1);
    assert_eq!(
        JitLocalHandlerSite::decode_cache(cached_propagate),
        Some((7, -1))
    );
}
