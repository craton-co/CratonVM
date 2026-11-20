use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess as _, NativeHeapAccess as _, NativeThreadAccess as _,
};
use cratonvm_types::ClassId;

struct ForgetVm(usize);
impl Drop for ForgetVm {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
    }
}

fn field(
    cid: ClassId,
    name: &str,
    desc: &str,
    slot: usize,
) -> cratonvm_native_api::FieldMetadata {
    cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    }
}

/// A real-layout exact `Random` whose `seed` holds a fresh `AtomicLong`.
fn real_random(c: &mut crate::test_utils::MockNativeContext) -> (ObjectRef, ObjectRef) {
    let random = c.ensure_class_initialized(RANDOM_CLASS).unwrap();
    let atomic = c
        .ensure_class_initialized("java/util/concurrent/atomic/AtomicLong")
        .unwrap();
    c.set_declared_fields(
        random,
        vec![
            field(
                random,
                "seed",
                "Ljava/util/concurrent/atomic/AtomicLong;",
                0,
            ),
            field(random, "nextNextGaussian", "D", 1),
            field(random, "haveNextNextGaussian", "Z", 2),
        ],
    );
    c.set_declared_fields(atomic, vec![field(atomic, "value", "J", 0)]);
    let r = c.alloc_object(random, 3);
    let al = c.alloc_object(atomic, 1);
    c.set_field(al, 0, Value::Long(0));
    c.set_field(r, 0, Value::Object(Some(al)));
    c.set_field(r, 1, Value::Double(0.0));
    (r, al)
}

fn next_long(c: &mut crate::test_utils::MockNativeContext, r: ObjectRef) -> i64 {
    match native_random_next_long(c, &[Value::Object(Some(r))]) {
        Ok(Some(Value::Long(v))) => v,
        other => panic!("nextLong answered {other:?}"),
    }
}

fn next_gaussian(c: &mut crate::test_utils::MockNativeContext, r: ObjectRef) -> f64 {
    match native_random_next_gaussian(c, &[Value::Object(Some(r))]) {
        Ok(Some(Value::Double(v))) => v,
        other => panic!("nextGaussian answered {other:?}"),
    }
}

/// What `readObject` leaves behind (real fields, no row) continues the
/// serialized stream, cached Gaussian first. Before wave 10 the first draw
/// installed an entropy seed.
#[test]
fn a_deserialized_random_continues_its_stream() {
    const VM: usize = 0xB2A_0001;
    let _g = ForgetVm(VM);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    let (r, al) = real_random(&mut c);
    c.set_field(al, 0, Value::Long(scramble_seed(42) as i64));
    c.set_field(r, 1, Value::Double(0.625));
    c.set_field(r, 2, Value::Int(1));
    assert_eq!(
        next_gaussian(&mut c, r).to_bits(),
        0.625f64.to_bits(),
        "the cached deviate"
    );

    // The reference: a native `new Random(42)`.
    let random = c.class_id_by_name(RANDOM_CLASS).unwrap();
    let reference = c.alloc_object(random, 3);
    native_random_init_seed(&mut c, &[Value::Object(Some(reference)), Value::Long(42)])
        .unwrap();
    for i in 0..3 {
        assert_eq!(
            next_long(&mut c, r),
            next_long(&mut c, reference),
            "draw {i}"
        );
    }
}

/// `writeObject` on a natively constructed instance finds the row in the
/// real fields (the mock's `invoke_special` stands in for the body).
///
/// Round 14 wave 3 (SH3-2): the instance is served from the side table
/// only while its `seed` holds no `AtomicLong` (the mock has no
/// constructor bytecode to build one), so it gets its `AtomicLong` just
/// before `writeObject`, as `resetSeed` would build it. With
/// `CRATONVM_RANDOM_REAL_SEED_FIELD` on the row is then HANDED OVER to the
/// fields: dropped, and the next draw continues from the fields.
#[test]
fn write_object_materializes_the_row_into_the_real_fields() {
    const VM: usize = 0xB2A_0011;
    let _g = ForgetVm(VM);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    let (r, al) = real_random(&mut c);
    c.set_field(r, 0, Value::Object(None));
    native_random_init_seed(&mut c, &[Value::Object(Some(r)), Value::Long(7)]).unwrap();
    next_long(&mut c, r);
    let first = next_gaussian(&mut c, r); // caches the partner
    let key = crate::existing_weak_lock_key(&c, r).expect("r's key");
    let row = SEED_TABLE
        .read()
        .as_ref()
        .and_then(|t| t.get(&key).copied())
        .unwrap();
    let cached = GAUSSIAN_TABLE
        .read()
        .as_ref()
        .and_then(|t| t.get(&key).copied())
        .unwrap();
    assert!(first.is_finite());

    c.set_field(r, 0, Value::Object(Some(al)));
    native_random_write_object(&mut c, &[Value::Object(Some(r)), Value::Object(None)]).unwrap();
    assert_eq!(c.get_field(al, 0), Value::Long(row as i64));
    assert_eq!(c.get_field(r, 2), Value::Int(1));
    assert_eq!(c.get_field(r, 1), Value::Double(cached));
    {
        assert_eq!(
            SEED_TABLE
                .read()
                .as_ref()
                .and_then(|t| t.get(&key).copied()),
            None,
            "the row is handed over to the fields"
        );
        assert_eq!(
            next_gaussian(&mut c, r).to_bits(),
            cached.to_bits(),
            "the cached deviate"
        );
        assert_eq!(c.get_field(r, 2), Value::Int(0));
        // The stream continues from the materialized state.
        let mut s = row;
        let mut step = |bits: u32| -> i64 {
            s = s.wrapping_mul(LCG_MULTIPLIER).wrapping_add(LCG_INCREMENT) & LCG_MASK;
            ((s >> (48 - bits)) as i32) as i64
        };
        let (hi, lo) = (step(32), step(32));
        assert_eq!(next_long(&mut c, r), (hi << 32).wrapping_add(lo));
    }
}

/// Round 14 wave 3 (SH3-2): an exact `Random` whose `seed` holds an
/// `AtomicLong` draws from, and advances, THAT field -- no lock key, no
/// side-table row -- and `setSeed` / `nextGaussian` use the real
/// `haveNextNextGaussian` / `nextNextGaussian`. The stream is bit-equal to
/// the side-table road's for the same seed.
#[test]
fn the_real_seed_field_is_the_generator_state() {
    const VM: usize = 0xB2A_0021;
    let _g = ForgetVm(VM);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    let (r, al) = real_random(&mut c);
    native_random_set_seed(&mut c, &[Value::Object(Some(r)), Value::Long(42)]).unwrap();
    assert_eq!(c.get_field(al, 0), Value::Long(scramble_seed(42) as i64));
    assert_eq!(c.get_field(r, 2), Value::Int(0));

    // The reference: a side-table `new Random(42)` (no `AtomicLong`).
    let random = c.class_id_by_name(RANDOM_CLASS).unwrap();
    let reference = c.alloc_object(random, 3);
    c.set_field(reference, 0, Value::Object(None));
    native_random_init_seed(&mut c, &[Value::Object(Some(reference)), Value::Long(42)])
        .unwrap();
    for i in 0..3 {
        assert_eq!(
            next_long(&mut c, r),
            next_long(&mut c, reference),
            "long {i}"
        );
        assert_eq!(
            next_gaussian(&mut c, r).to_bits(),
            next_gaussian(&mut c, reference).to_bits(),
            "gaussian {i}"
        );
    }
    // An odd number of Gaussians left the partner in the real fields.
    assert_eq!(c.get_field(r, 2), Value::Int(1));
    assert!(
        crate::existing_weak_lock_key(&c, r).is_none(),
        "a field-road Random minted a lock key"
    );
    // `setSeed` clears the cached partner, as the JDK's does.
    native_random_set_seed(&mut c, &[Value::Object(Some(r)), Value::Long(42)]).unwrap();
    assert_eq!(c.get_field(r, 2), Value::Int(0));
    assert_eq!(c.get_field(al, 0), Value::Long(scramble_seed(42) as i64));
}

/// Round 14 wave 5 (lane fixup2, RV4-2): the first derivation memoises the
/// layout for its VM, later draws are served from it with the same cell,
/// another VM (with another layout) is never served that row, a receiver
/// the memo does not fit falls back to the full derivation, and a unit
/// mock's identity `0` never memoises.
#[test]
fn the_random_layout_memo_serves_only_its_own_vm() {
    const VM_A: usize = 0xB2A_0051;
    const VM_B: usize = 0xB2A_0052;
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let (r, al) = real_random(&mut a);
    let derived = exact_random_seed_cell(&a, r).expect("real layout");
    assert!(derived.atomic == al && derived.value == 0);
    let memo = memoized_random_seed_cell(&a, VM_A, r).expect("memoised for VM A");
    assert!(memo.atomic == al && memo.value == 0 && memo.random == derived.random);
    let before = scramble_seed(7) as i64;
    a.set_field(al, 0, Value::Long(before));
    let mut reference = before as u64;
    for i in 0..3 {
        reference = reference
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT)
            & LCG_MASK;
        let cell = exact_random_seed_cell(&a, r).expect("served");
        assert_eq!(
            field_lcg_next(&mut a, cell, 32),
            (reference >> 16) as i32,
            "draw {i}"
        );
    }
    // A seed that is not an `AtomicLong` any more: not served, not derived.
    a.set_field(r, 0, Value::Object(None));
    assert!(memoized_random_seed_cell(&a, VM_A, r).is_none());
    assert!(exact_random_seed_cell(&a, r).is_none());

    // VM B keeps `seed` in slot 1: A's row (slot 0) must not answer for it.
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    let random = b.ensure_class_initialized(RANDOM_CLASS).unwrap();
    let atomic = b
        .ensure_class_initialized("java/util/concurrent/atomic/AtomicLong")
        .unwrap();
    b.set_declared_fields(
        random,
        vec![
            field(random, "nextNextGaussian", "D", 0),
            field(
                random,
                "seed",
                "Ljava/util/concurrent/atomic/AtomicLong;",
                1,
            ),
            field(random, "haveNextNextGaussian", "Z", 2),
        ],
    );
    b.set_declared_fields(atomic, vec![field(atomic, "value", "J", 0)]);
    let rb = b.alloc_object(random, 3);
    let alb = b.alloc_object(atomic, 1);
    b.set_field(alb, 0, Value::Long(0));
    b.set_field(rb, 0, Value::Double(0.0));
    b.set_field(rb, 1, Value::Object(Some(alb)));
    assert!(memoized_random_seed_cell(&b, VM_B, rb).is_none(), "A's row");
    let cell = exact_random_seed_cell(&b, rb).expect("B's own derivation");
    assert!(cell.atomic == alb && cell.value == 0);
    assert!(memoized_random_seed_cell(&b, VM_B, rb).is_some_and(|m| m.atomic == alb));

    // Identity 0 (a unit mock that set none) never reads nor fills a row.
    let mut z = crate::test_utils::mock_ctx();
    let (rz, _) = real_random(&mut z);
    assert!(exact_random_seed_cell(&z, rz).is_some());
    assert!(memoized_random_seed_cell(&z, 0, rz).is_none());
}
