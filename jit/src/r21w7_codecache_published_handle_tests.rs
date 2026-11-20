use super::*;
use std::sync::atomic::Ordering;

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

/// `(strong before the handle drops, strong after, holders after,
/// superseded)` for a reader handle on the cache's body, dropped while this
/// thread is inside compiled code (so a queued reference stays queued).
/// `supersede` republishes the key first, so the handle is the last holder.
fn drop_a_reader_handle(supersede: bool) -> (usize, usize, usize, bool) {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("r21w7/PublishedHandle");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    let bare = cache.get(&class, &method, &desc, cid).expect("published");
    let handle = RetainedCode::new(Arc::clone(&bare));
    // Entered BEFORE the supersede, so the reference the supersede itself
    // queues cannot be released by the drain the handle's drop runs (which
    // would read as a plain decrement).
    let token = jit_execution_enter();
    if supersede {
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    }
    let before = Arc::strong_count(&bare);
    drop(handle);
    let after = Arc::strong_count(&bare);
    jit_execution_leave(token);
    let holders = bare.retained_holders.load(Ordering::SeqCst);
    let superseded = bare.superseded.load(Ordering::SeqCst);
    cache.clear_all();
    drop(RetainedCode::new(bare));
    (before, after, holders, superseded)
}

/// The common invoke-cache eviction: the body is still published, the
/// map's handle keeps the count above zero, and the reader's reference is
/// released on the spot -- nothing waits in the queue for it.
#[test]
fn a_handle_on_a_published_body_drops_plainly() {
    let (before, after, holders, superseded) = drop_a_reader_handle(false);
    assert!(!superseded);
    assert_eq!(holders, 1, "the shard map's own handle is counted");
    assert_eq!(after, before - 1, "a plain decrement, not a queue entry");
}

/// The race CODECACHE21-2 closes is still closed: once the cache has let
/// go, the reader's handle is the last holder and its reference moves into
/// the queue instead of being released.
#[test]
fn the_last_handle_on_a_superseded_body_still_routes() {
    let (before, after, holders, superseded) = drop_a_reader_handle(true);
    assert!(superseded);
    assert_eq!(holders, 0);
    assert_eq!(after, before, "the reference moved into the queue");
}
