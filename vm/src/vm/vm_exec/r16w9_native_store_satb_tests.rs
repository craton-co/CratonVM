use super::{
    descriptor_may_hold_reference, publish_overwritten_array_element,
    publish_overwritten_field, NativeStoreSatb,
};
use crate::types::{ObjectRef, Value};
use std::cell::{Cell, RefCell};

/// A heap that says whether it is marking, answers every old-value read
/// with `old`, and records each read and each barrier call.
struct MockHeap {
    marking: bool,
    old: Value,
    reads: Cell<usize>,
    published: RefCell<Vec<Value>>,
    last_desc: Cell<Option<Option<u8>>>,
}

impl MockHeap {
    fn new(marking: bool, old: Value) -> Self {
        Self {
            marking,
            old,
            reads: Cell::new(0),
            published: RefCell::new(Vec::new()),
            last_desc: Cell::new(None),
        }
    }
}

impl NativeStoreSatb for MockHeap {
    fn native_store_must_publish(&self) -> bool {
        self.marking
    }
    fn overwritten_array_element(&self, _: ObjectRef, _: usize) -> Value {
        self.reads.set(self.reads.get() + 1);
        self.old
    }
    fn overwritten_field(&self, _: ObjectRef, _: usize, desc: Option<u8>) -> Value {
        self.reads.set(self.reads.get() + 1);
        self.last_desc.set(Some(desc));
        self.old
    }
    fn publish_overwritten(&self, old: Value) {
        self.published.borrow_mut().push(old);
    }
}

fn obj(word: usize) -> ObjectRef {
    // SAFETY: never dereferenced -- the mock answers every read itself.
    unsafe { ObjectRef::from_raw(word as *mut u8) }
}

fn addr(value: Value) -> usize {
    match value {
        Value::Object(Some(r)) => r.as_ptr() as usize,
        _ => 0,
    }
}

#[test]
fn a_marking_heap_gets_the_overwritten_array_element() {
    let heap = MockHeap::new(true, Value::Object(Some(obj(0x1000))));
    assert!(publish_overwritten_array_element(
        &heap,
        || true,
        obj(0x2000),
        3
    ));
    assert_eq!(heap.reads.get(), 1);
    let published = heap.published.borrow();
    assert_eq!(published.len(), 1);
    assert_eq!(addr(published[0]), 0x1000);
}

#[test]
fn an_idle_heap_reads_nothing_and_never_asks_the_switch() {
    let heap = MockHeap::new(false, Value::Object(Some(obj(0x1000))));
    let asked = Cell::new(false);
    let enabled = || {
        asked.set(true);
        true
    };
    assert!(!publish_overwritten_array_element(
        &heap,
        enabled,
        obj(0x2000),
        0
    ));
    assert!(!publish_overwritten_field(
        &heap,
        || true,
        obj(0x2000),
        0,
        None
    ));
    assert!(!asked.get(), "the kill switch is read only while marking");
    assert_eq!(heap.reads.get(), 0, "no old-value read while nothing marks");
    assert!(heap.published.borrow().is_empty());
}

#[test]
fn the_kill_switch_stops_the_read_and_the_publish() {
    let heap = MockHeap::new(true, Value::Object(Some(obj(0x1000))));
    assert!(!publish_overwritten_array_element(
        &heap,
        || false,
        obj(0x2000),
        0
    ));
    assert!(!publish_overwritten_field(
        &heap,
        || false,
        obj(0x2000),
        0,
        Some(b'L')
    ));
    assert_eq!(heap.reads.get(), 0);
    assert!(heap.published.borrow().is_empty());
}

#[test]
fn null_and_primitive_old_values_are_not_published() {
    for old in [Value::Object(None), Value::Int(7), Value::Long(-1)] {
        let heap = MockHeap::new(true, old);
        assert!(!publish_overwritten_array_element(
            &heap,
            || true,
            obj(0x2000),
            1
        ));
        assert!(!publish_overwritten_field(
            &heap,
            || true,
            obj(0x2000),
            1,
            None
        ));
        assert_eq!(heap.reads.get(), 2);
        assert!(heap.published.borrow().is_empty());
    }
}

#[test]
fn field_stores_read_through_the_store_descriptor_and_skip_primitive_slots() {
    for desc in [None, Some(0u8), Some(b'L'), Some(b'[')] {
        let heap = MockHeap::new(true, Value::Object(Some(obj(0x3000))));
        assert!(publish_overwritten_field(
            &heap,
            || true,
            obj(0x2000),
            2,
            desc
        ));
        assert_eq!(
            heap.last_desc.get(),
            Some(desc),
            "read with the store's descriptor"
        );
        assert_eq!(heap.published.borrow().len(), 1);
        assert_eq!(addr(heap.published.borrow()[0]), 0x3000);
    }
    for prim in *b"BCDFIJSZ" {
        let heap = MockHeap::new(true, Value::Object(Some(obj(0x3000))));
        assert!(!publish_overwritten_field(
            &heap,
            || true,
            obj(0x2000),
            2,
            Some(prim)
        ));
        assert_eq!(heap.reads.get(), 0, "a primitive slot is never read");
    }
    assert!(descriptor_may_hold_reference(None));
    assert!(descriptor_may_hold_reference(Some(0)));
    assert!(!descriptor_may_hold_reference(Some(b'I')));
}
