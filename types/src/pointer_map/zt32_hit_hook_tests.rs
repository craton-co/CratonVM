use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Every hit of `get` / `contains_key` / `get_mut` reports its key, in both
/// forms; misses and iteration do not; clones carry the hook.
#[test]
fn zt32_lookups_that_hit_report_their_key() {
    let hits = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(AtomicUsize::new(0));
    let hook = {
        let (hits, last) = (Arc::clone(&hits), Arc::clone(&last));
        Arc::new(move |k: usize| {
            hits.fetch_add(1, Ordering::Relaxed);
            last.store(k, Ordering::Relaxed);
        })
    };
    let mut sorted = PointerMap::from_pairs_sorted(vec![(0x10, 0x90), (0x20, 0xa0)]);
    sorted.set_hit_hook(Some(hook));
    assert_eq!(sorted.get(&0x10), Some(&0x90));
    assert_eq!(last.load(Ordering::Relaxed), 0x10);
    assert!(sorted.contains_key(&0x20));
    assert_eq!(sorted.get(&0x30), None);
    assert!(!sorted.contains_key(&0x30));
    assert_eq!(hits.load(Ordering::Relaxed), 2, "misses do not report");
    let _ = sorted.iter().count();
    assert_eq!(hits.load(Ordering::Relaxed), 2, "iteration does not report");
    let clone = sorted.clone();
    assert_eq!(clone.get(&0x20), Some(&0xa0));
    assert_eq!(
        hits.load(Ordering::Relaxed),
        3,
        "the clone carries the hook"
    );
    let mut hashed = clone;
    if let Some(v) = hashed.get_mut(&0x10) {
        *v = 0xb0;
    }
    assert_eq!(hits.load(Ordering::Relaxed), 4, "get_mut reports too");
    hashed.set_hit_hook(None);
    assert_eq!(hashed.get(&0x10), Some(&0xb0));
    assert_eq!(hits.load(Ordering::Relaxed), 4, "cleared");
}
