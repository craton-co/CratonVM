use super::{
    compare_and_exchange_static_shared, create_java_string_uninterned, get_static_shared,
    set_static_shared, SharedVm,
};
use crate::classloading::ClassId;
use crate::types::Value;
use std::sync::Arc;

fn vm() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(crate::config::VmConfig::default()))
}

#[test]
fn jr21_static_cas_compares_by_bits_and_reports_the_witness() {
    let shared = vm();
    let c = ClassId::new(90_211);
    set_static_shared(&shared, c, 0, Value::Int(5));
    set_static_shared(&shared, c, 1, Value::Long(7));
    set_static_shared(&shared, c, 2, Value::Float(f32::NAN));
    set_static_shared(&shared, c, 3, Value::Object(None));
    set_static_shared(&shared, c, 4, Value::Uninitialized);
    let cae =
        |i: usize, e: Value, n: Value| compare_and_exchange_static_shared(&shared, c, i, e, n);

    // int: a failed CAS leaves the static and names what it held.
    assert_eq!(
        cae(0, Value::Int(4), Value::Int(9)),
        Some((Value::Int(5), false))
    );
    assert_eq!(get_static_shared(&shared, c, 0), Value::Int(5));
    assert_eq!(
        cae(0, Value::Int(5), Value::Int(9)),
        Some((Value::Int(5), true))
    );
    assert_eq!(get_static_shared(&shared, c, 0), Value::Int(9));

    // long
    assert_eq!(
        cae(1, Value::Long(7), Value::Long(-1)),
        Some((Value::Long(7), true))
    );
    assert_eq!(get_static_shared(&shared, c, 1), Value::Long(-1));

    // float: by bits, so a NaN matches itself, and an `int` carrying the
    // raw bits (`compareAndSetFloat` -> `compareAndSetInt`) matches too
    // and keeps the static a `float`.
    let nan_bits = f32::NAN.to_bits();
    let (w, ok) = cae(2, Value::Float(f32::NAN), Value::Float(1.5)).unwrap();
    assert!(ok);
    assert!(matches!(w, Value::Float(f) if f.to_bits() == nan_bits));
    let one_five = 1.5f32.to_bits() as i32;
    let two = 2.0f32.to_bits() as i32;
    assert!(cae(2, Value::Int(one_five), Value::Int(two)).unwrap().1);
    assert_eq!(get_static_shared(&shared, c, 2), Value::Float(2.0));

    // reference: identity, and the JVM null equal to a typed zero.
    let a = create_java_string_uninterned(&shared, "jr21-a");
    let b = create_java_string_uninterned(&shared, "jr21-b");
    assert_eq!(
        cae(3, Value::Int(0), Value::Object(Some(a))),
        Some((Value::Object(None), true))
    );
    assert_eq!(
        cae(3, Value::Object(Some(b)), Value::Object(None)),
        Some((Value::Object(Some(a)), false))
    );
    assert_eq!(
        cae(3, Value::Object(Some(a)), Value::Object(Some(b))),
        Some((Value::Object(Some(a)), true))
    );
    assert_eq!(get_static_shared(&shared, c, 3), Value::Object(Some(b)));

    // Declines: an untyped cell, a kind the cell cannot hold, no block.
    assert_eq!(cae(4, Value::Int(0), Value::Int(1)), None);
    assert_eq!(cae(0, Value::Long(9), Value::Long(1)), None);
    assert_eq!(
        compare_and_exchange_static_shared(
            &shared,
            ClassId::new(90_212),
            0,
            Value::Int(0),
            Value::Int(1)
        ),
        None
    );
}

/// Four threads of CAS increments on a static `int` and a static `long`
/// lose nothing (the read-then-write this replaced lost updates here).
#[test]
fn jr21_static_cas_increments_are_exact_across_threads() {
    let shared = vm();
    let c = ClassId::new(90_213);
    set_static_shared(&shared, c, 0, Value::Int(0));
    set_static_shared(&shared, c, 1, Value::Long(0));
    let vm_ref: &SharedVm = &shared;
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(move || {
                for _ in 0..20_000 {
                    loop {
                        let cur = get_static_shared(vm_ref, c, 0);
                        let Value::Int(v) = cur else { panic!() };
                        let r = compare_and_exchange_static_shared(
                            vm_ref,
                            c,
                            0,
                            cur,
                            Value::Int(v + 1),
                        );
                        if r.expect("typed int static").1 {
                            break;
                        }
                    }
                    loop {
                        let cur = get_static_shared(vm_ref, c, 1);
                        let Value::Long(v) = cur else { panic!() };
                        let r = compare_and_exchange_static_shared(
                            vm_ref,
                            c,
                            1,
                            cur,
                            Value::Long(v + 1),
                        );
                        if r.expect("typed long static").1 {
                            break;
                        }
                    }
                }
            });
        }
    });
    assert_eq!(get_static_shared(&shared, c, 0), Value::Int(80_000));
    assert_eq!(get_static_shared(&shared, c, 1), Value::Long(80_000));
}
