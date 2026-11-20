use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

pub fn key(n: u32) -> (usize, usize, usize, u32, bool) {
    (7, 0x1000, 3, n, false)
}

/// A memo proven under one epoch is never served under another, and the
/// probe that notices the move empties it.
#[test]
pub fn a_negative_is_served_only_under_the_epoch_it_was_proven_in() {
    jit_typecheck_negative_cache_put(key(1), 40);
    assert!(jit_typecheck_negative_cached(key(1), 40));
    assert!(
        !jit_typecheck_negative_cached(key(2), 40),
        "a different receiver"
    );
    assert!(
        !jit_typecheck_negative_cached(key(1), 41),
        "a class was defined since: the false must be re-proven"
    );
    assert!(
        !jit_typecheck_negative_cached(key(1), 40),
        "the stale memo was emptied, not merely skipped"
    );
}

/// Bounded, least-recently-used eviction; a hit is moved to the front.
#[test]
pub fn the_memo_is_bounded_and_keeps_the_hot_entry() {
    let epoch = 9_000;
    jit_typecheck_negative_cache_put(key(0), epoch);
    for n in 1..JIT_TYPECHECK_NEGATIVE_CACHE_CAP as u32 {
        jit_typecheck_negative_cache_put(key(n), epoch);
        // Touch entry 0 every time so it is never the LRU victim.
        assert!(jit_typecheck_negative_cached(key(0), epoch));
    }
    jit_typecheck_negative_cache_put(key(10_000), epoch);
    assert!(
        jit_typecheck_negative_cached(key(0), epoch),
        "hot entry survived"
    );
    assert!(jit_typecheck_negative_cached(key(10_000), epoch));
    let len = JIT_TYPECHECK_NEGATIVE_CACHE.with(|c| c.borrow().1.len());
    assert_eq!(len, JIT_TYPECHECK_NEGATIVE_CACHE_CAP);
}

/// End to end through the entry point compiled code calls: an unrelated
/// receiver at a RECORDED site answers `0`, and that `false` is memoized;
/// an annotation-proxy receiver's `false` is not (its answer depends on the
/// instance, not the class id).
#[test]
pub fn a_recorded_site_false_is_memoized_but_an_annotation_proxy_false_is_not() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let (target, plain, anno) = {
        let mut cm = shared.classes.class_manager.write();
        let mut mk = |name: &str, fields: usize| {
            cm.try_ensure_synthetic_class(name, fields)
                .expect("Compatible mode fabricates; this fixture never runs under --real-jdk")
        };
        (
            mk("cratonvm/test/R9w3NegTarget", 0),
            mk("cratonvm/test/R9w3NegPlain", 0),
            // One slot: `annotation_proxy_satisfies_target` reads slot 0
            // (the annotation's type descriptor); null here, so the proxy
            // is an instance of nothing but `Annotation`.
            mk("java/lang/annotation/AnnotationProxy", 1),
        )
    };
    let (name_ptr, name_len) = cratonvm_jit::intern_typecheck_target(
        "cratonvm/test/R9w3NegTarget",
        Some(target.as_u32()),
    );
    let plain_obj = shared.mem.heap.alloc_object(plain, 0);
    let anno_obj = shared.mem.heap.alloc_object(anno, 1);
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let key_for = |cid: ClassId| {
        (
            shared.vm_identity,
            name_ptr as usize,
            name_len,
            cid.as_u32(),
            false,
        )
    };
    // The epoch is this VM's store slot's (wave 21), which a concurrent
    // test's store may share, and the redefine latch is process-wide;
    // retry until one call runs in a quiet window.
    let mut checked = false;
    for _ in 0..200 {
        if crate::classloading::any_class_redefined() {
            // Sticky, set by some other test: the memo is bypassed by
            // design, so there is nothing to assert about it here.
            return;
        }
        let e0 = typecheck_memo_epoch(&shared);
        // SAFETY: `vm_ptr` names the live VM above; the objects are live
        // heap objects of its heap; the name is an interned `&'static str`.
        let (plain_ans, anno_ans) = unsafe {
            (
                jit_instanceof(vm_ptr, plain_obj.as_ptr() as i64, name_ptr, name_len as i64),
                jit_instanceof(vm_ptr, anno_obj.as_ptr() as i64, name_ptr, name_len as i64),
            )
        };
        assert_eq!(plain_ans, 0, "an unrelated class is not an instance");
        assert_eq!(
            anno_ans, 0,
            "an annotation proxy of no type is not an instance"
        );
        if typecheck_memo_epoch(&shared) != e0 {
            continue;
        }
        assert!(
            jit_typecheck_negative_cached(key_for(plain), e0),
            "a recorded-site false for an ordinary class must be memoized"
        );
        assert!(
            !jit_typecheck_negative_cached(key_for(anno), e0),
            "an annotation proxy's false depends on the instance and must not be"
        );
        // And a memoized false still answers false.
        // SAFETY: as above.
        let again = unsafe {
            jit_instanceof(vm_ptr, plain_obj.as_ptr() as i64, name_ptr, name_len as i64)
        };
        assert_eq!(again, 0);
        checked = true;
        break;
    }
    assert!(checked, "no quiet epoch window in 200 tries");
}
