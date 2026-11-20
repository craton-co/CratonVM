use super::*;
use std::sync::Arc;

fn body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn put(cache: &JitCache) {
    cache.put(
        Arc::from("i40l2/P"),
        Arc::from("v"),
        Arc::from("()I"),
        cratonvm_types::ClassId::new(0x0040_1201),
        body(),
    );
}

fn published(cache: &JitCache) -> bool {
    cache
        .get(
            "i40l2/P",
            "v",
            "()I",
            cratonvm_types::ClassId::new(0x0040_1201),
        )
        .is_some()
}

#[test]
fn a_held_cache_publishes_nothing_until_released() {
    let cache = JitCache::new();
    assert!(!cache.publications_held());
    cache.hold_publications();
    assert!(cache.publications_held());
    put(&cache);
    assert!(!published(&cache), "refused while a call installs");
    cache.release_publications();
    assert!(!cache.publications_held());
    put(&cache);
    assert!(published(&cache), "published once the call is done");
    // An unmatched release does not wrap the count.
    cache.release_publications();
    assert!(!cache.publications_held());
}
