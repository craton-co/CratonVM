//! JIT tier2 wave 5, lane nat21 (`NATIVE21-2`): `holds_without_locking`, the
//! leaf `Thread.holdsLock`'s read, answers exactly `holds`'s value whenever
//! it answers at all, and declines (`None`) rather than lock.
use super::{header_of, MonitorTable};
use crate::classloading::ClassId;
use crate::memory::heap::Heap;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_types::ObjectHeader;
use std::sync::atomic::Ordering;

fn object() -> crate::types::ObjectRef {
    let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(4096)));
    heap.alloc_object(ClassId::new(0), 0)
}

fn state(obj: crate::types::ObjectRef) -> u32 {
    ObjectHeader::mark_state(header_of(obj).mark_word.load(Ordering::Acquire))
}

/// Whenever the lock-free read answers, it equals the full read.
fn agrees(t: &MonitorTable, obj: crate::types::ObjectRef, tid: ThreadId) -> Option<bool> {
    let quick = t.holds_without_locking(obj, tid);
    if let Some(answer) = quick {
        assert_eq!(answer, t.holds(obj, tid), "the leaf read must equal `holds`");
    }
    quick
}

#[test]
fn nat21_neutral_and_thin_locked_words() {
    let t = MonitorTable::new();
    let obj = object();
    let me = ThreadId(7_101);
    let other = ThreadId(7_102);
    assert_eq!(state(obj), cratonvm_types::MARK_NEUTRAL);
    assert_eq!(agrees(&t, obj, me), Some(false), "an unlocked word is not held");

    t.enter(obj, me);
    assert_eq!(state(obj), cratonvm_types::MARK_THIN_LOCKED);
    // `enter` leased and cached this thread's slot: the read answers.
    assert_eq!(agrees(&t, obj, me), Some(true));
    // A thread this OS thread has not cached declines or says no, never yes.
    assert_ne!(agrees(&t, obj, other), Some(true));
    // Re-entered, then released once: still held.
    t.enter(obj, me);
    t.exit(obj, me).unwrap();
    assert_eq!(agrees(&t, obj, me), Some(true));
    t.exit(obj, me).unwrap();
    assert_eq!(agrees(&t, obj, me), Some(false));
}

#[test]
fn nat21_inflated_word_answers_from_the_cache_and_declines_without_it() {
    let t = MonitorTable::new();
    let obj = object();
    let me = ThreadId(7_103);
    let other = ThreadId(7_104);
    let (_monitor, contended) = t.enter_inflated_or_contend(obj, me).unwrap();
    assert!(!contended);
    assert_eq!(state(obj), cratonvm_types::MARK_INFLATED);
    // `lookup_indexed` records the monitor in this thread's cache.
    assert!(t.lookup_indexed(obj).is_some());
    assert_eq!(agrees(&t, obj, me), Some(true));
    assert_eq!(agrees(&t, obj, other), Some(false));
    // An index mutation invalidates the cache: no answer, no lock taken;
    // the full read still says held.
    t.note_index_mutation();
    assert_eq!(t.holds_without_locking(obj, me), None);
    assert!(t.holds(obj, me));
    // The full read refilled the cache from the index.
    assert_eq!(agrees(&t, obj, me), Some(true));
    t.exit(obj, me).unwrap();
    assert_ne!(agrees(&t, obj, me), Some(true));
}
