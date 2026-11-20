use super::*;
use crate::{ExecutableBuffer, JitCache};
use std::sync::atomic::Ordering::SeqCst;

const DEP: &str = "J27Globals/Dep";

fn body() -> Option<CompiledMethod> {
    let mut buf = ExecutableBuffer::new(64)?;
    buf.emit(&[0xC3]); // ret
    Some(CompiledMethod::new(buf))
}

fn inlining_dep() -> Option<CompiledMethod> {
    let mut cm = body()?;
    cm.inlined_methods
        .push((DEP.to_string(), "value".to_string(), "()I".to_string()));
    Some(cm)
}

/// One VM's cache: a body inlining `DEP`, displaced by a tier-up, and a
/// published raw caller of the displaced body. Returns the displaced body.
fn vm_with_superseded_dependent(
    cache: &JitCache,
    cid: cratonvm_types::ClassId,
) -> Option<Arc<CompiledMethod>> {
    cache.put("J27G".into(), "dep".into(), "()V".into(), cid, inlining_dep()?);
    let old = cache.get("J27G", "dep", "()V", cid)?;
    cache.put("J27G".into(), "dep".into(), "()V".into(), cid, body()?);
    let mut caller = body()?;
    caller._direct_callee_entries = vec![old.entry_ptr() as usize];
    cache.put("J27G".into(), "caller".into(), "()V".into(), cid, caller);
    // Both must hold, or every test below would pass vacuously.
    assert!(
        cache.get("J27G", "caller", "()V", cid).is_some(),
        "the raw caller of the displaced body is published"
    );
    assert!(old.superseded.load(SeqCst), "the tier-up superseded it");
    Some(old)
}

fn sweeping_cache() -> JitCache {
    let mut cache = JitCache::new();
    cache.invalidation_retires_superseded = true;
    cache
}

/// The scan of one cache lists its own superseded body, never another
/// cache's.
#[test]
fn a_cache_lists_only_its_own_superseded_bodies() {
    let (a, b) = (sweeping_cache(), sweeping_cache());
    let cid = cratonvm_types::ClassId::new(0x2701);
    let (Some(a_old), Some(b_old)) = (
        vm_with_superseded_dependent(&a, cid),
        vm_with_superseded_dependent(&b, cid),
    ) else {
        return;
    };
    let listed = a.superseded_bodies.live();
    assert!(listed.iter().any(|cm| Arc::ptr_eq(cm, &a_old)));
    assert!(!listed.iter().any(|cm| Arc::ptr_eq(cm, &b_old)));
}

/// VM A's class-change sweep leaves VM B's superseded body and its raw
/// caller alone; VM B's own sweep still retires both. Before lane GLOBALS
/// A's sweep marked B's body `retired` and B's sweep then skipped it,
/// leaving B's caller published on the stale body.
#[test]
fn one_vms_sweep_leaves_the_other_vms_superseded_body() {
    let (a, b) = (sweeping_cache(), sweeping_cache());
    let cid = cratonvm_types::ClassId::new(0x2702);
    let (Some(a_old), Some(b_old)) = (
        vm_with_superseded_dependent(&a, cid),
        vm_with_superseded_dependent(&b, cid),
    ) else {
        return;
    };
    a.invalidate_for_class_change(DEP);
    assert!(a_old.retired.load(SeqCst), "A retires its own body");
    assert!(a.get("J27G", "caller", "()V", cid).is_none());
    assert!(!b_old.retired.load(SeqCst), "A's sweep left B's body alone");
    assert!(b.get("J27G", "caller", "()V", cid).is_some());

    b.invalidate_for_class_change(DEP);
    assert!(b_old.retired.load(SeqCst), "B's own sweep retires it");
    assert!(
        b.get("J27G", "caller", "()V", cid).is_none(),
        "and withdraws B's raw caller"
    );
}

/// Dead records are pruned: the list holds no reference that keeps a
/// body alive, and forgets one that was dropped.
#[test]
fn a_dropped_body_leaves_the_list() {
    let list = SupersededBodies::default();
    let Some(cm) = body() else {
        return;
    };
    let cm = Arc::new(cm);
    cm.superseded.store(true, SeqCst);
    list.note(&cm);
    assert_eq!(list.live().len(), 1);
    drop(cm);
    assert!(list.live().is_empty());
    assert!(list.records.lock().bodies.is_empty());
}
