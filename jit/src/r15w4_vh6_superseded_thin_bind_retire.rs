use super::*;
use std::sync::atomic::Ordering::SeqCst;

fn body() -> Option<CompiledMethod> {
    let mut buf = ExecutableBuffer::new(64)?;
    buf.emit(&[0xC3]); // ret
    Some(CompiledMethod::new(buf))
}

/// A thin-bound body a tier-up superseded is in no map, but a raw caller
/// still runs it. The first-mint sweep retires it and withdraws the
/// published raw caller; the body that superseded it (no bind) stays.
#[test]
fn the_first_mint_sweep_retires_a_superseded_thin_bound_body() {
    if !varhandle_thin_bind_retire_enabled() || !varhandle_retire_superseded_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(0x6601);
    let (Some(mut bound), Some(fresh), Some(mut caller)) = (body(), body(), body()) else {
        return;
    };
    add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
    cache.put("R15Vh6".into(), "wrapper".into(), "()V".into(), cid, bound);
    // Held as a raw caller holds it: alive after its supersede.
    let old = cache
        .get("R15Vh6", "wrapper", "()V", cid)
        .expect("the thin-bound body is published");
    cache.put("R15Vh6".into(), "wrapper".into(), "()V".into(), cid, fresh);
    assert!(old.superseded.load(SeqCst), "the tier-up superseded it");
    caller._direct_callee_entries = vec![old.entry_ptr() as usize];
    cache.put("R15Vh6".into(), "caller".into(), "()V".into(), cid, caller);
    assert!(cache.get("R15Vh6", "caller", "()V", cid).is_some());
    let withdrawn = cache.invalidate_for_class(VARHANDLE_THIN_BIND_DEPENDENCY);
    assert!(
        old.retired.load(SeqCst),
        "the superseded thin-bound body is retired"
    );
    assert!(
        cache.get("R15Vh6", "caller", "()V", cid).is_none(),
        "its published raw caller is withdrawn"
    );
    assert!(
        cache.get("R15Vh6", "wrapper", "()V", cid).is_some(),
        "the body that superseded it bound nothing and stays"
    );
    assert_eq!(withdrawn, 1, "only the published caller left a map");
}
