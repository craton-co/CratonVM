use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn alloc_random(ctx: &mut dyn NativeContext) -> ObjectRef {
    // Random's synthetic layout: field 0 = seed (Long), field 1 =
    // haveNextGaussian (Int). Same layout as SecureRandom so the
    // shared `sr_next_seed` helper works on both.
    let r = try_alloc_concurrent_synthetic(ctx, "java/util/Random", 2).unwrap();
    ctx.set_field(r, 0, Value::Long(0));
    ctx.set_field(r, 1, Value::Int(0));
    r
}

#[test]
fn t2_random_next_long_returns_long_value() {
    let mut ctx = mock_ctx();
    let r = alloc_random(&mut ctx);
    let v = native_sr_next_long(&mut ctx, &[Value::Object(Some(r))]);
    assert!(matches!(v.unwrap(), Some(Value::Long(_))));
}

#[test]
fn t2_random_next_double_in_unit_interval() {
    let mut ctx = mock_ctx();
    let r = alloc_random(&mut ctx);
    for _ in 0..32 {
        let v = native_sr_next_double(&mut ctx, &[Value::Object(Some(r))]);
        match v.unwrap() {
            Some(Value::Double(d)) => {
                assert!(
                    (0.0..=1.0).contains(&d),
                    "nextDouble must lie in [0, 1], got {d}"
                );
            }
            other => panic!("expected Double, got {other:?}"),
        }
    }
}

#[test]
fn t2_random_next_float_in_unit_interval() {
    let mut ctx = mock_ctx();
    let r = alloc_random(&mut ctx);
    for _ in 0..32 {
        let v = native_random_next_float(&mut ctx, &[Value::Object(Some(r))]);
        match v.unwrap() {
            Some(Value::Float(f)) => {
                assert!(
                    (0.0..1.0).contains(&f),
                    "nextFloat must lie in [0, 1), got {f}"
                );
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }
}

#[test]
fn t2_random_next_boolean_is_binary() {
    let mut ctx = mock_ctx();
    let r = alloc_random(&mut ctx);
    for _ in 0..16 {
        let v = native_sr_next_boolean(&mut ctx, &[Value::Object(Some(r))]);
        match v.unwrap() {
            Some(Value::Int(b)) => assert!(b == 0 || b == 1),
            other => panic!("expected Int, got {other:?}"),
        }
    }
}

#[test]
fn t2_random_next_gaussian_is_finite() {
    let mut ctx = mock_ctx();
    let r = alloc_random(&mut ctx);
    // 32 samples — all must be finite doubles. We do not assert a
    // specific mean/stddev because that would be flaky with only 32
    // samples from the CSPRNG; the shape of the distribution is
    // validated by the polar algorithm's mathematical correctness.
    let mut all_zero = true;
    for _ in 0..32 {
        let v = native_random_next_gaussian(&mut ctx, &[Value::Object(Some(r))]);
        match v.unwrap() {
            Some(Value::Double(d)) => {
                assert!(d.is_finite(), "nextGaussian must be finite, got {d}");
                if d != 0.0 {
                    all_zero = false;
                }
            }
            other => panic!("expected Double, got {other:?}"),
        }
    }
    assert!(
        !all_zero,
        "nextGaussian produced 32 zeros in a row — CSPRNG stuck?"
    );
}
