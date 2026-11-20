use super::DispatchCache;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn body() -> Arc<cratonvm_jit::CompiledMethod> {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]); // RET
    Arc::new(cratonvm_jit::CompiledMethod::new(buf))
}

fn memo_on(owner: &Arc<cratonvm_jit::CompiledMethod>) -> DispatchCache {
    DispatchCache {
        entry: owner.entry_ptr() as usize,
        needs_context: false,
        owner: Some(std::rc::Rc::new(cratonvm_jit::RetainedCode::new(
            Arc::clone(owner),
        ))),
        rerun_pins: false,
    }
}

/// A fresh body is served; a superseded one and an invalidated one are
/// both dropped at the hit. An unowned entry (tests only) names nothing.
#[test]
fn a_memo_is_stale_once_its_body_is_superseded_or_retired() {
    let fresh = body();
    assert!(!memo_on(&fresh).names_a_stale_body());

    let replaced = body();
    replaced.superseded.store(true, Ordering::Release);
    assert!(memo_on(&replaced).names_a_stale_body());

    let invalidated = body();
    invalidated.retired.store(true, Ordering::Release);
    assert!(memo_on(&invalidated).names_a_stale_body());

    let unowned = DispatchCache {
        entry: 0x1000,
        needs_context: false,
        owner: None,
        rerun_pins: false,
    };
    assert!(!unowned.names_a_stale_body());
}

/// The whole contract end to end: a replacing `put` marks the body it
/// displaced, so a memo that still names it goes stale while a memo on the
/// new body does not, and the displaced body is NOT `retired` (which six
/// readers take to mean "withdrawn by an invalidation").
#[test]
fn a_replacing_put_makes_the_old_bodys_memo_stale_and_leaves_it_unretired() {
    let cache = cratonvm_jit::JitCache::new();
    let cid = cratonvm_types::ClassId::new(7);
    let publish = |cache: &cratonvm_jit::JitCache| {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
        buf.emit(&[0xC3]);
        cache.put(
            "r11w11/Memo".into(),
            "hot".into(),
            "()V".into(),
            cid,
            cratonvm_jit::CompiledMethod::new(buf),
        );
        cache
            .get("r11w11/Memo", "hot", "()V", cid)
            .expect("the publication is reachable")
    };
    let first = publish(&cache);
    let old_memo = memo_on(&first);
    assert!(!old_memo.names_a_stale_body());
    let second = publish(&cache);
    assert!(
        !Arc::ptr_eq(&first, &second),
        "the second put replaced the first"
    );
    assert!(first.superseded.load(Ordering::Acquire));
    assert!(!first.retired.load(Ordering::Acquire));
    assert!(old_memo.names_a_stale_body());
    assert!(!memo_on(&second).names_a_stale_body());
}
