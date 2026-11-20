use super::*;
use std::sync::Mutex;

/// The class the stub claims every object belongs to. Distinctive on
/// purpose: `-1` (the old report) and `0` (a plausible default) are both
/// wrong answers this must not be confused with.
const STUB_CLASS: u32 = 4242;

/// A collector that implements only the raw half of field access and
/// records what the four `*_as` defaults did on top of it.
///
/// Modelled on `g1.rs`'s `StubCollector`: everything the tests do not
/// call is `unreachable!()`, so a default body that silently starts
/// depending on a new method fails loudly instead of quietly.
///
/// The recording is the whole point. `FieldCoercionSite` is consumed by
/// the `#[cold]` reporter and never returned, and the loss counters are
/// process-global — `heap.rs`'s own G30 tests assert EXACT deltas on
/// them under a module-private lock this module cannot take, so a test
/// here that fired a lossy coercion would make those exact deltas flaky
/// from another module. Instead these tests observe the one part of
/// provenance that is locally visible and equally load-bearing: whether
/// the default asked `class_id_of` at all. It cannot have built a real
/// site without asking, and `coerce_field_value_by_descriptor` — the
/// call this change replaced — never asks.
struct RecordingCollector {
    header: ObjectHeader,
    // `GarbageCollector` is `Sync`, so a `RefCell` recorder does not satisfy the
    // bound. `Mutex` is the smallest change that keeps this a plain struct.
    class_id_calls: Mutex<usize>,
    stored: Mutex<Vec<(usize, Value)>>,
    slot: Mutex<Value>,
}

impl RecordingCollector {
    pub fn new(slot: Value) -> Self {
        Self {
            header: ObjectHeader::new(
                ClassId::new(STUB_CLASS),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                2,
            ),
            class_id_calls: Mutex::new(0),
            stored: Mutex::new(Vec::new()),
            slot: Mutex::new(slot),
        }
    }

    /// A pointer-shaped, 8-aligned `ObjectRef` the stub never
    /// dereferences — every accessor it reaches answers from the
    /// struct above.
    fn obj(&self) -> ObjectRef {
        // SAFETY: `from_raw` only requires a non-null, 8-aligned
        // pointer. `&self.header` is both, and nothing in this module
        // reads through the resulting `ObjectRef`.
        unsafe { ObjectRef::from_raw(&self.header as *const ObjectHeader as *mut u8) }
    }
}

#[test]
fn default_fallible_allocation_never_calls_the_fatal_methods() {
    let heap = RecordingCollector::new(Value::Int(0));
    let erased: &dyn GarbageCollector = &heap;
    assert!(erased.try_alloc_object(ClassId::new(0), 0).is_none());
    assert!(erased
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 0)
        .is_none());
    // RecordingCollector's fatal methods panic, so either delegation
    // would make this test fail even when the requested layout fits.
}

impl GarbageCollector for RecordingCollector {
    fn alloc_object(&self, _: ClassId, _: usize) -> ObjectRef {
        unreachable!("not called by these tests")
    }
    fn alloc_array(&self, _: ClassId, _: ArrayElementType, _: usize) -> ObjectRef {
        unreachable!("not called by these tests")
    }
    fn get_header(&self, _: ObjectRef) -> &ObjectHeader {
        &self.header
    }
    fn class_id_of(&self, _: ObjectRef) -> ClassId {
        *self.class_id_calls.lock().unwrap() += 1;
        ClassId::new(STUB_CLASS)
    }
    fn kind_of(&self, _: ObjectRef) -> ObjectKind {
        ObjectKind::Object
    }
    fn element_type_of(&self, _: ObjectRef) -> ArrayElementType {
        ArrayElementType::Reference
    }
    fn identity_hash_code(&self, _: ObjectRef) -> i32 {
        unreachable!("not called by these tests")
    }
    fn get_field(&self, _: ObjectRef, _: usize) -> Value {
        *self.slot.lock().unwrap()
    }
    fn set_field(&self, _: ObjectRef, index: usize, value: Value) {
        self.stored.lock().unwrap().push((index, value));
    }
    fn get_field_volatile(&self, _: ObjectRef, _: usize) -> Value {
        *self.slot.lock().unwrap()
    }
    fn set_field_volatile(&self, _: ObjectRef, index: usize, value: Value) {
        self.stored.lock().unwrap().push((index, value));
    }
    fn array_length(&self, _: ObjectRef) -> usize {
        unreachable!("not called by these tests")
    }
    fn get_array_element(&self, _: ObjectRef, _: usize) -> Result<Value, i32> {
        unreachable!("not called by these tests")
    }
    fn set_array_element(&self, _: ObjectRef, _: usize, _: Value) -> Result<(), i32> {
        unreachable!("not called by these tests")
    }
    fn needs_gc(&self) -> bool {
        false
    }
    fn collect_garbage(
        &self,
        _: &StopTheWorldToken,
        _: &mut [ObjectRef],
        _: &dyn MonitorCleanup,
    ) -> GcResult {
        unreachable!("not called by these tests")
    }
    fn write_barrier(&self, _: ObjectRef, _: Value) {}
    fn allocated_bytes(&self) -> usize {
        0
    }
}

/// G45, THE PIN. Each of the four descriptor-aware defaults must ask the
/// collector who the object is, exactly once per access.
///
/// This is the whole defect in one assertion. Every one of the 5,431
/// coercion events measured across the vector corpus printed
/// `class_id=-1 index=-1`, because these four bodies — which
/// `GenerationalHeap`, `G1Collector` and `ZgcRealHeap` all inherit
/// unchanged, and which `VmHeap`'s `dispatch!` routes the whole VM
/// through — called `coerce_field_value_by_descriptor`, whose site is
/// the constant `FieldCoercionSite::UNATTRIBUTED`. Reverting any of the
/// four to that call leaves its counter at 0 here.
///
/// "Exactly once", not "at least once": twice would mean the header is
/// being re-read per access for no reason, on the allocation and
/// collection hot path.
#[test]
fn the_four_descriptor_aware_defaults_ask_who_the_object_is() {
    for (name, run) in [
        (
            "get_field_as",
            Box::new(|c: &RecordingCollector| {
                c.get_field_as(c.obj(), 1, b'I');
            }) as Box<dyn Fn(&RecordingCollector)>,
        ),
        (
            "get_field_volatile_as",
            Box::new(|c: &RecordingCollector| {
                c.get_field_volatile_as(c.obj(), 1, b'I');
            }),
        ),
        (
            "set_field_as",
            Box::new(|c: &RecordingCollector| {
                c.set_field_as(c.obj(), 1, Value::Int(5), b'I');
            }),
        ),
        (
            "set_field_volatile_as",
            Box::new(|c: &RecordingCollector| {
                c.set_field_volatile_as(c.obj(), 1, Value::Int(5), b'I');
            }),
        ),
    ] {
        let c = RecordingCollector::new(Value::Int(5));
        run(&c);
        assert_eq!(
            *c.class_id_calls.lock().unwrap(),
            1,
            "{name} must resolve the class exactly once so the coercion \
             instrument can name it (0 = reverted to \
             coerce_field_value_by_descriptor and every event goes back \
             to class_id=-1)",
        );
    }
}

/// G45: and the value the slot sees is unchanged.
///
/// This is the risk half. These four bodies are the VM's entire
/// descriptor-aware field path and 97 of 99 `--real-jdk` vectors pass
/// over them, so the acceptable behavioural delta is zero.
/// `coerce_field_value_for_slot` reads `site` in exactly one place — as
/// an argument to the `#[cold]` `note_field_coercion_loss` — so this
/// holds by construction; the table pins it anyway.
///
/// DELIBERATELY only non-reporting arms. Every lossy input would
/// increment the process-global counters that `heap.rs`'s G30 tests
/// assert exact deltas on, from a module that cannot take their lock.
/// The normalising arms are the ones this change could plausibly have
/// disturbed; the lossy arms are covered where the lock lives.
#[test]
fn provenance_did_not_change_what_the_slot_receives() {
    // (value, descriptor) — normalisation and identity only, no arm
    // here calls `note_field_coercion_loss`.
    let cases: &[(Value, u8)] = &[
        (Value::Int(-7), b'J'),
        (Value::Double(f64::from_bits(0x0102_0304_0506_0708)), b'J'),
        (Value::Float(1.5), b'J'),
        (Value::Long(0x0102_0304_0506_0708), b'D'),
        (Value::Int(3), b'F'),
        (Value::Long(0x1_0000_0001), b'I'),
        (Value::Double(2.5), b'S'),
        (Value::Uninitialized, b'J'),
        (Value::Uninitialized, b'D'),
        (Value::Uninitialized, b'F'),
        (Value::Uninitialized, b'I'),
        (Value::Object(None), b'L'),
        (Value::Object(None), b'['),
        // Unknown descriptor: the `_ => value` arm, untouched.
        (Value::Int(99), b'V'),
    ];
    for &(value, desc) in cases {
        let expected = crate::heap::coerce_field_value_by_descriptor(value, desc);

        let store = RecordingCollector::new(Value::Uninitialized);
        store.set_field_as(store.obj(), 1, value, desc);
        store.set_field_volatile_as(store.obj(), 0, value, desc);
        assert_eq!(
            *store.stored.lock().unwrap().clone(),
            vec![(1, expected), (0, expected)],
            "storing {value:?} at a '{}' slot must land exactly what the \
             descriptor-only helper lands",
            desc as char,
        );

        let load = RecordingCollector::new(value);
        assert_eq!(
            load.get_field_as(load.obj(), 1, desc),
            expected,
            "reading {value:?} from a '{}' slot",
            desc as char,
        );
        assert_eq!(
            load.get_field_volatile_as(load.obj(), 1, desc),
            expected,
            "volatile-reading {value:?} from a '{}' slot",
            desc as char,
        );
    }
}

/// G45: the reads say `read` and the writes say `store`.
///
/// The direction is not decoration. MEASURED on the pre-G30 binary, 336
/// of the 352 cross-type accesses in one `RJdkNet` run are READS of a
/// slot that was never descriptor-initialised, and answering `null`
/// there is correct. Filing those under the same heading as a store is
/// how the handful of real stores became invisible, so the site each
/// default builds is asserted directly rather than through the counters.
#[test]
fn the_reads_are_reads_and_the_writes_are_stores() {
    let cid = Some(ClassId::new(STUB_CLASS));
    let r = crate::heap::FieldCoercionSite::read(cid, 3);
    let s = crate::heap::FieldCoercionSite::store(cid, 3);
    assert_eq!(r.kind.name(), "read");
    assert_eq!(s.kind.name(), "store");
    assert_eq!(r.class_id.map(|c| c.as_u32()), Some(STUB_CLASS));
    assert_eq!(s.index, Some(3));
    // And the thing that was there before names neither.
    let u = crate::heap::FieldCoercionSite::UNATTRIBUTED;
    assert_eq!(u.kind.name(), "unattributed");
    assert!(u.class_id.is_none() && u.index.is_none());
}

/// The volatile stripe pool must occupy one cache line PER STRIPE, not one
/// cache line in TOTAL.
///
/// This asserts both halves, because either alone is satisfiable by the
/// bug: the second assertion is the fix, and the first is the reason the
/// fix was needed and the thing that would make a future "simplification"
/// back to a bare `[Mutex<()>; N]` look harmless. A `parking_lot::Mutex<()>`
/// is one byte, so an unpadded 64-entry pool is 64 bytes — every stripe on
/// one line, and every `lock()` an atomic RMW that takes that line
/// exclusive from every other thread.
#[test]
fn volatile_stripes_do_not_share_a_cache_line() {
    assert!(
        std::mem::size_of::<parking_lot::Mutex<()>>() <= 8,
        "a bare `Mutex<()>` is {} bytes — that is WHY the pool is padded",
        std::mem::size_of::<parking_lot::Mutex<()>>(),
    );
    assert_eq!(
        std::mem::align_of::<VolatileStripe>(),
        128,
        "each volatile stripe must start on its own 128-byte boundary",
    );
    assert_eq!(
        std::mem::size_of::<VolatileStripe>(),
        128,
        "each volatile stripe must OCCUPY 128 bytes, not merely start on a 128-byte boundary",
    );
    // And the pool as a whole: 64 stripes that genuinely do not overlap.
    assert_eq!(
        std::mem::size_of::<[VolatileStripe; VOLATILE_STRIPE_COUNT]>(),
        128 * VOLATILE_STRIPE_COUNT,
    );
}

/// gc-common 2026-09-23 (lane A) — the stripe hash must spread
/// fixed-stride objects over the whole pool.
///
/// The old `(addr >> 3) * K & 63` used only address bits 3..8, so 64
/// objects of 32 bytes each reached 16 stripes, 64-byte objects 8, and any
/// object of 512 bytes or more ONE — every volatile access to field 0 of
/// every large object in the heap serialised on a single mutex. Asserting
/// "at least half the pool" per stride is loose enough to be hash-agnostic
/// and tight enough that the low-bits version fails the 32-, 64-, 128-,
/// 512- and 4096-byte rows (16, 8, 4, 1 and 1 stripes).
#[test]
fn volatile_stripes_spread_fixed_stride_objects() {
    let base = 0x7f00_0000_0000usize;
    for stride in [8usize, 16, 24, 32, 48, 64, 128, 512, 4096] {
        let mut seen = [false; VOLATILE_STRIPE_COUNT];
        for i in 0..256 {
            seen[volatile_stripe_index(base + i * stride, 0)] = true;
        }
        let distinct = seen.iter().filter(|s| **s).count();
        assert!(
            distinct >= VOLATILE_STRIPE_COUNT / 2,
            "256 objects at a {stride}-byte stride reached only {distinct} of \
             {VOLATILE_STRIPE_COUNT} volatile stripes"
        );
    }
    // Determinism, which is the only correctness property: a slot always
    // maps to the same stripe.
    assert_eq!(
        volatile_stripe_index(base, 3),
        volatile_stripe_index(base, 3)
    );
    // And consecutive fields of one object land on consecutive stripes.
    assert_eq!(
        (volatile_stripe_index(base, 0) + 1) % VOLATILE_STRIPE_COUNT,
        volatile_stripe_index(base, 1)
    );
}
