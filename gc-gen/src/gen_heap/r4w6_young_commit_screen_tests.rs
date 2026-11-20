//! gen r4w6/review6: the young commit screen is one immutable snapshot.
//! Every test uses its own table, never the process-global one other
//! tests' heaps publish into.
use super::*;
use cratonvm_gc_core::reservation::GRANULE;

/// A bitmap of `granules` granules with exactly `committed` set.
fn bits(granules: usize, committed: &[usize]) -> Arc<[AtomicU64]> {
    let words: Vec<AtomicU64> = (0..granules.div_ceil(64))
        .map(|_| AtomicU64::new(0))
        .collect();
    for &g in committed {
        words[g >> 6].fetch_or(1u64 << (g & 63), Ordering::Relaxed);
    }
    words.into()
}

fn screen(base: usize, granules: usize, b: &Arc<[AtomicU64]>) -> Option<CommitScreen> {
    Some(CommitScreen {
        base,
        end: base + granules * GRANULE,
        bits: Some(Arc::clone(b)),
    })
}

/// The tri-state read the movable screen uses (gen r5w1/young5): nothing
/// published and a miss are both "no answer", which is what keeps the
/// band verifier's header read unchanged where no backend publishes.
#[test]
fn lookup_separates_no_answer_from_not_committed() {
    let t: CommitScreenTable<3> = CommitScreenTable::new();
    assert_eq!(t.lookup(A, 16).flatten(), None, "nothing published");
    let ba = bits(4, &[0]);
    t.publish([screen(A, 4, &ba), None, None]);
    assert_eq!(t.lookup(B, 16).flatten(), None, "outside every arena");
    assert_eq!(t.lookup(A, 16).flatten(), Some(true));
    assert_eq!(t.lookup(A + GRANULE, 16).flatten(), Some(false));
    t.clear_if_names(&[A]);
    assert_eq!(
        t.lookup(A + GRANULE, 16).flatten(),
        None,
        "unpublished again"
    );
}

/// The per-heap shape (gen r5w1/young5): three slots, a wholly committed
/// arena (`bits: None`), and a miss answering the caller's `outside`.
#[test]
fn a_heap_screen_answers_by_address_and_refuses_a_miss() {
    let t: CommitScreenTable<3> = CommitScreenTable::new();
    assert!(
        t.range_committed_or(A, 16, false),
        "nothing published: no screen"
    );
    let ba = bits(4, &[0]);
    let bb = bits(4, &[1]);
    let old = CommitScreen {
        base: 0x1_0000_0000,
        end: 0x1_0000_0000 + 4 * GRANULE,
        bits: None,
    };
    t.publish([screen(A, 4, &ba), screen(B, 4, &bb), Some(old)]);
    assert!(t.range_committed_or(A, 16, false));
    assert!(!t.range_committed_or(A + GRANULE, 16, false));
    assert!(
        t.range_committed_or(B + GRANULE, 16, false),
        "slot order does not matter"
    );
    assert!(
        t.range_committed_or(0x1_0000_0000 + 3 * GRANULE, 16, false),
        "a wholly committed arena answers true everywhere inside it"
    );
    assert!(
        !t.range_committed_or(0x2_0000_0000, 16, false),
        "a miss is the caller's `outside` answer"
    );
    assert!(t.range_committed_or(0x2_0000_0000, 16, true));
    // A flip republishes the same arenas swapped: the same answers, and
    // one snapshot per ordering.
    let old2 = CommitScreen {
        base: 0x1_0000_0000,
        end: 0x1_0000_0000 + 4 * GRANULE,
        bits: None,
    };
    t.publish([screen(B, 4, &bb), screen(A, 4, &ba), Some(old2)]);
    assert!(t.range_committed_or(A, 16, false));
    assert!(!t.range_committed_or(B, 16, false));
    assert_eq!(t.registry.lock().len(), 2);
}

const A: usize = 0x4000_0000;
const B: usize = 0x8000_0000;

#[test]
fn nothing_published_answers_true_and_outside_answers_true() {
    let t: CommitScreenTable<2> = CommitScreenTable::new();
    assert!(t.range_committed(A, 16), "no snapshot: nothing is young");
    let ba = bits(4, &[0]);
    t.publish([screen(A, 4, &ba), None]);
    assert!(
        t.range_committed(B, 16),
        "outside every arena: not this screen's question"
    );
    assert!(t.range_committed(A + 8, 16), "granule 0 is committed");
    assert!(!t.range_committed(A + GRANULE, 16), "granule 1 is not");
    assert!(
        !t.range_committed(A + GRANULE - 8, 16),
        "a header straddling into an uncommitted granule is not committed"
    );
}

/// The torn pair of the review5 page: a reader that took the OLD publish
/// keeps a snapshot whose bitmap length is its own, so a smaller republish
/// cannot make it walk off the new bitmap, and the old bitmap stays alive.
#[test]
fn an_old_snapshot_stays_valid_and_self_consistent_after_a_republish() {
    let t: CommitScreenTable<2> = CommitScreenTable::new();
    let big = bits(256, &[200]);
    t.publish([screen(A, 256, &big), None]);
    let old = t.current.load(Ordering::Acquire);
    let small = bits(2, &[0]);
    drop(big);
    t.publish([screen(A, 2, &small), None]);
    assert_ne!(t.current.load(Ordering::Acquire), old);
    // SAFETY: published snapshots are leaked and never freed.
    let old = unsafe { &*old };
    assert_eq!(old.range_committed(A + 200 * GRANULE, 16), Some(true));
    assert_eq!(old.range_committed(A + 201 * GRANULE, 16), Some(false));
    assert!(
        t.range_committed(A + 200 * GRANULE, 16),
        "outside the new, smaller arena"
    );
    assert!(t.range_committed(A, 16));
    assert!(!t.range_committed(A + GRANULE, 16));
}

/// A moving flip swaps the arenas between slots. The answer depends only
/// on the address, never on which slot an arena sits in, and repeated
/// flips reuse the two snapshots instead of leaking new ones.
#[test]
fn a_flip_does_not_change_answers_and_reuses_snapshots() {
    let t: CommitScreenTable<2> = CommitScreenTable::new();
    let ba = bits(4, &[0]);
    let bb = bits(4, &[1]);
    for flip in 0..10 {
        if flip % 2 == 0 {
            t.publish([screen(A, 4, &ba), screen(B, 4, &bb)]);
        } else {
            t.publish([screen(B, 4, &bb), screen(A, 4, &ba)]);
        }
        assert!(t.range_committed(A, 16));
        assert!(!t.range_committed(A + GRANULE, 16));
        assert!(!t.range_committed(B, 16));
        assert!(t.range_committed(B + GRANULE, 16));
    }
    assert_eq!(t.registry.lock().len(), 2, "one snapshot per ordering");
    // A re-backed arena (a new bitmap) is a new snapshot.
    let ba2 = bits(8, &[0, 5]);
    t.publish([screen(A, 8, &ba2), screen(B, 4, &bb)]);
    assert_eq!(t.registry.lock().len(), 3);
    assert!(t.range_committed(A + 5 * GRANULE, 16));
}

#[test]
fn clear_if_names_unpublishes_only_the_owner() {
    let t: CommitScreenTable<2> = CommitScreenTable::new();
    let ba = bits(4, &[]);
    t.publish([screen(A, 4, &ba), None]);
    t.clear_if_names(&[B]);
    assert!(
        !t.range_committed(A, 16),
        "another heap's drop leaves the screen"
    );
    t.clear_if_names(&[A]);
    assert!(t.range_committed(A, 16), "the owner's drop unpublishes it");
    assert!(t.current.load(Ordering::Acquire).is_null());
}

/// The page's verification recipe without Miri: a reader hammering the
/// screen while the writer alternates a large and a small bitmap. Every
/// answer must be one that SOME publish gives; before the fix a torn
/// `(ptr, words)` pair could index past the small bitmap.
#[test]
fn a_reader_racing_republishes_only_sees_published_answers() {
    use std::sync::atomic::AtomicBool;
    let t: &'static CommitScreenTable<2> = Box::leak(Box::new(CommitScreenTable::new()));
    let all: Vec<usize> = (0..128).collect();
    let big = bits(128, &all);
    let small = bits(1, &[]);
    t.publish([screen(A, 128, &big), None]);
    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    const READS: u64 = 100_000;
    let reader = std::thread::spawn(move || {
        let mut reads = 0u64;
        for _ in 0..READS {
            let p = t.current.load(Ordering::Acquire);
            // SAFETY: `p` is non-null (published before the spawn) and
            // published snapshots are leaked and never freed.
            let snapshot = unsafe { &*p };
            // Whatever publish the reader caught, it is self-consistent:
            // granule 100 is committed in the big arena and outside the
            // small one. A torn pair would answer `Some(false)` or fault.
            assert_ne!(
                snapshot.range_committed(A + 100 * GRANULE, 16),
                Some(false),
                "a snapshot mixed two publishes"
            );
            // The table's own answer: never a panic, whichever publish or
            // retry outcome it lands on.
            if t.range_committed(A + 100 * GRANULE, 16) {
                reads += 1;
            }
        }
        stop.store(true, Ordering::Release);
        reads
    });
    let mut i = 0u64;
    // `is_finished` too: a reader that panicked never sets `stop`. `i < 2`:
    // on a loaded box the reader can finish before this loop runs, and the
    // registry check below needs both shapes published once.
    while i < 2 || (!stop.load(Ordering::Acquire) && !reader.is_finished()) {
        if i % 2 == 0 {
            t.publish([screen(A, 1, &small), None]);
        } else {
            t.publish([screen(A, 128, &big), None]);
        }
        i += 1;
    }
    let reads = reader.join().expect("the reader must not panic");
    // Every answer at granule 100 is `true` (committed, or outside the
    // small arena) unless four republishes in a row beat one read, which
    // answers the safe `false`; that cannot be most of them.
    assert!(reads >= READS / 2, "reads={reads} of {READS}");
    assert_eq!(
        t.registry.lock().len(),
        2,
        "two distinct publishes, deduplicated"
    );
}
