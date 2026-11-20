use super::*;

fn body(inlined: &[(&str, &str, &str)]) -> CompiledMethod {
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    cm.inlined_methods = inlined
        .iter()
        .map(|(c, m, d)| (c.to_string(), m.to_string(), d.to_string()))
        .collect();
    cm
}

const CID: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0);

#[test]
fn a_class_that_was_inlined_still_evicts() {
    let cache = JitCache::new();
    cache.put("Caller".into(), "a".into(), "()V".into(), CID, body(&[]));
    cache.put(
        "Caller".into(),
        "b".into(),
        "()V".into(),
        CID,
        body(&[("Helper", "getX", "()I")]),
    );
    assert_eq!(
        cache.invalidate_for_class("Helper"),
        1,
        "the early-out swallowed a real CHA invalidation"
    );
}

#[test]
fn a_class_nobody_inlined_returns_zero_without_scanning() {
    let cache = JitCache::new();
    cache.put(
        "Caller".into(),
        "b".into(),
        "()V".into(),
        CID,
        body(&[("Alpha", "foo", "()V")]),
    );
    assert_eq!(cache.invalidate_for_class("Beta"), 0);
    assert_eq!(cache.len(), 1, "a non-matching invalidation evicted a body");
}

/// The set is only ever added to, so a body that is evicted leaves its
/// class name behind. That must degrade to "scan and find nothing", not to
/// a wrong answer.
#[test]
fn a_stale_name_costs_a_scan_and_still_answers_zero() {
    let cache = JitCache::new();
    cache.put(
        "Caller".into(),
        "b".into(),
        "()V".into(),
        CID,
        body(&[("Helper", "getX", "()I")]),
    );
    assert_eq!(cache.invalidate_for_class("Helper"), 1);
    // "Helper" is still in the name set; the cache no longer holds it.
    assert_eq!(cache.invalidate_for_class("Helper"), 0);
}

/// `invalidate_unloaded_class` ALSO matches on `declaring_class_id`, which
/// the name set says nothing about. It deliberately has no early-out, and a
/// future edit that gives it one by symmetry would drop every body whose
/// class was unloaded without ever having been inlined from.
#[test]
fn unloaded_class_eviction_does_not_depend_on_the_inlined_name_set() {
    let cache = JitCache::new();
    let owner = cratonvm_types::ClassId::new(7);
    cache.put("Doomed".into(), "m".into(), "()V".into(), owner, body(&[]));
    assert_eq!(
        cache.invalidate_unloaded_class(owner, "Doomed"),
        1,
        "an unloaded class's own body survived: `invalidate_unloaded_class` \
         matches by declaring_class_id and must not be gated on the \
         inlined-name set"
    );
}

#[test]
fn clear_all_empties_the_name_set_with_the_maps() {
    let cache = JitCache::new();
    cache.put(
        "Caller".into(),
        "b".into(),
        "()V".into(),
        CID,
        body(&[("Helper", "getX", "()I")]),
    );
    cache.clear_all();
    assert!(!cache.any_body_inlined_from("Helper"));
    assert_eq!(cache.invalidate_for_class("Helper"), 0);
}
