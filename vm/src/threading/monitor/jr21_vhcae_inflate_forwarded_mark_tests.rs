//! JIT round 21, lane vhcae (correctness; page
//! `docs/feature-designs/generative-gc-architecture.md`):
//! `inflate_locked` never CASes a FORWARDED word; it inflates the
//! forwardee.
use super::tests::compact_object;
use super::{header_of, MonitorTable};
use crate::memory::heap::Heap;
use cratonvm_types::ObjectHeader;
use std::sync::atomic::Ordering;

#[test]
fn a_forwarded_object_keeps_its_forwarding_and_its_forwardee_is_inflated() {
    let t = MonitorTable::new();
    let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(1 << 16)));
    let stale = compact_object(heap);
    let moved = compact_object(heap);
    let stale_header = header_of(stale);
    stale_header.set_forwarding_address(moved.as_ptr());
    let before = stale_header.mark_word.load(Ordering::Acquire);
    assert!(ObjectHeader::is_forwarded_mark(before));

    let h = t.identity_hash_via_monitor(stale, || 0x2468_1357);
    assert_ne!(h, 0);

    // The stale copy still says where the object went, bit for bit.
    assert_eq!(stale_header.mark_word.load(Ordering::Acquire), before);
    assert_eq!(stale_header.forwarding_address(), moved.as_ptr());
    assert!(
        t.lookup_indexed(stale).is_none(),
        "nothing indexed at the stale address"
    );
    // The forwardee carries the monitor and the hash.
    assert_eq!(
        ObjectHeader::mark_state(header_of(moved).mark_word.load(Ordering::Acquire)),
        cratonvm_types::MARK_INFLATED
    );
    let m = t.lookup_indexed(moved).expect("the forwardee is indexed");
    assert_eq!(m.displaced_hash(), h);
    // Later requests through either reference answer the same.
    assert_eq!(t.identity_hash_via_monitor(stale, || 7), h);
    assert_eq!(t.identity_hash_via_monitor(moved, || 9), h);
}

/// A self-forward is a relocation in progress: inflation waits it out and
/// then inflates the restored word in place.
#[test]
fn a_self_forward_is_waited_out_then_inflated_in_place() {
    let t = MonitorTable::new();
    let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(1 << 16)));
    let obj = compact_object(heap);
    let header = header_of(obj);
    let before = header.mark_word.load(Ordering::Acquire);
    header.set_forwarding_address(obj.as_ptr());
    assert!(ObjectHeader::is_forwarded_mark(
        header.mark_word.load(Ordering::Acquire)
    ));

    let restorer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        // The collector's restore: the pre-pause word comes back.
        header.mark_word.store(before, Ordering::Release);
    });
    let h = t.identity_hash_via_monitor(obj, || 0x1357_2468);
    restorer.join().unwrap();

    assert_ne!(h, 0);
    assert_eq!(
        ObjectHeader::mark_state(header.mark_word.load(Ordering::Acquire)),
        cratonvm_types::MARK_INFLATED
    );
    assert_eq!(
        t.lookup_indexed(obj)
            .expect("indexed in place")
            .displaced_hash(),
        h
    );
}

/// A self-forward nobody restores (the waiter would be the collector
/// itself) stops the VM naming the address instead of hanging it, and
/// leaves the word untouched.
#[test]
#[should_panic(expected = "self-forwarded mark")]
fn a_self_forward_never_restored_panics_instead_of_hanging() {
    let t = MonitorTable::new();
    let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(1 << 16)));
    let obj = compact_object(heap);
    header_of(obj).set_forwarding_address(obj.as_ptr());
    t.identity_hash_via_monitor(obj, || 1);
}
