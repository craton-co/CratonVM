use super::*;
use cratonvm_types::ClassId;

fn g1_vm() -> SharedVm {
    SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::G1,
        ..crate::config::VmConfig::default()
    })
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// A ring of unmarked objects longer than the hash set holds, entered
/// twice, plus a marked object hanging off it with an unmarked one behind:
/// every ring member and the marked stop are reported exactly once, and
/// nothing behind the stop is.
#[test]
fn a_walk_past_the_promotion_point_reports_each_object_once() {
    let shared = g1_vm();
    let heap = &shared.mem.heap;
    assert!(heap.reserved_address_envelope().is_some(), "G1 publishes its span");
    let n = RemarkVisited::PROMOTE_AT + 1_000;
    let ring: Vec<ObjectRef> = (0..n).map(|_| heap.alloc_object(ClassId::new(0), 2)).collect();
    for (i, o) in ring.iter().enumerate() {
        heap.set_field(*o, 0, Value::Object(Some(ring[(i + 1) % n])));
    }
    let stop = heap.alloc_object(ClassId::new(0), 1);
    let behind = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(stop, 0, Value::Object(Some(behind)));
    heap.set_field(ring[n / 2], 1, Value::Object(Some(stop)));

    let is_marked = |a: usize| a == addr(stop);
    let not_a_reference = |_: ObjectRef| false;
    let closure = g1_remark_reference_closure(
        &shared,
        &is_marked,
        &not_a_reference,
        &[addr(ring[0]), addr(ring[n - 1])],
    );

    let mut sorted = closure.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), closure.len(), "no object is reported twice");
    let mut want: Vec<usize> = ring.iter().map(|o| addr(*o)).collect();
    want.push(addr(stop));
    want.sort_unstable();
    assert_eq!(sorted, want, "the ring and the marked stop, nothing behind it");
}

/// Promotion carries the hash set's entries into the bitmap: an address
/// recorded before it is still refused after it.
#[test]
fn promotion_keeps_what_was_recorded_before_it() {
    let base = 0x1000_0000usize;
    let mut seen = RemarkVisited::new(Some((base, base + (64 << 20))));
    for i in 0..RemarkVisited::PROMOTE_AT {
        assert!(seen.insert(base + 8 * i));
    }
    assert!(matches!(seen, RemarkVisited::Big(_)), "promoted at the threshold");
    assert!(!seen.insert(base), "the first entry survived the move");
    assert!(!seen.insert(base + 8 * (RemarkVisited::PROMOTE_AT - 1)));
    assert!(seen.insert(base + 8 * RemarkVisited::PROMOTE_AT), "a new one is new");
    assert!(!seen.insert(base + 8 * RemarkVisited::PROMOTE_AT));
}

/// Without a published span the hash set serves the whole walk.
#[test]
fn without_a_span_the_set_never_promotes() {
    let mut seen = RemarkVisited::new(None);
    for i in 0..RemarkVisited::PROMOTE_AT + 10 {
        assert!(seen.insert(8 * (i + 1)));
    }
    assert!(matches!(seen, RemarkVisited::Small { .. }));
    assert!(!seen.insert(8));
}

/// The memo traces a root list once and answers it again unchanged; a
/// list that differs only in order is traced on its own.
#[test]
fn the_memo_traces_an_identical_root_list_once() {
    let memo = RemarkClosureMemo::default();
    let calls = std::cell::Cell::new(0);
    let walk = |roots: &[usize]| {
        calls.set(calls.get() + 1);
        RemarkWalk {
            closure: roots.iter().map(|r| r * 10).collect(),
            references: Vec::new(),
        }
    };
    assert_eq!(memo.walk_of(&[1, 2], &walk).closure, vec![10, 20]);
    assert_eq!(memo.walk_of(&[1, 2], &walk).closure, vec![10, 20]);
    assert_eq!(calls.get(), 1, "the second answer is remembered");
    assert_eq!(memo.walk_of(&[2, 1], &walk).closure, vec![20, 10]);
    assert_eq!(calls.get(), 2, "another order is another list");
}

/// The walk's `references` are exactly the closure's unmarked,
/// `Reference`-shaped instances of two or more fields -- the screen
/// `g1_remark_undiscovered_references` used to run over the closure -- and
/// a `Reference`'s slot 0 is not followed.
#[test]
fn the_walk_names_the_unmarked_reference_shaped_instances() {
    let shared = g1_vm();
    let heap = &shared.mem.heap;
    let reference_class = ClassId::new(7);
    let root = heap.alloc_object(ClassId::new(0), 3);
    let reference = heap.alloc_object(reference_class, 2);
    let marked_reference = heap.alloc_object(reference_class, 2);
    let one_field_reference = heap.alloc_object(reference_class, 1);
    let referent = heap.alloc_object(ClassId::new(0), 1);
    let queue = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(root, 0, Value::Object(Some(reference)));
    heap.set_field(root, 1, Value::Object(Some(marked_reference)));
    heap.set_field(root, 2, Value::Object(Some(one_field_reference)));
    heap.set_field(reference, 0, Value::Object(Some(referent)));
    heap.set_field(reference, 1, Value::Object(Some(queue)));

    let is_marked = |a: usize| a == addr(marked_reference);
    let shaped = |o: ObjectRef| heap.class_id_of(o) == reference_class;
    let walk = g1_remark_reference_walk(&shared, &is_marked, &shaped, &[addr(root)]);

    assert_eq!(walk.references, vec![addr(reference)]);
    let mut closure = walk.closure.clone();
    closure.sort_unstable();
    let mut want = vec![
        addr(root),
        addr(reference),
        addr(marked_reference),
        addr(one_field_reference),
        addr(queue),
    ];
    want.sort_unstable();
    assert_eq!(closure, want, "the referent behind slot 0 is not reached");
}
