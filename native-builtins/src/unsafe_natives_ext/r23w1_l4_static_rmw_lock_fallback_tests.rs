use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{NativeHeapAccess, NativeSystemAccess};

// A class id and slots no other test in this crate registers a static for.
const CID: u32 = 0x7_A4C1;

#[test]
fn r23w1_l4_declined_hardware_update_runs_under_the_class_monitor() {
    if !unsafe_static_hw_rmw_enabled() {
        return; // the A/B control's own arm: the old unlocked path
    }
    let mut ctx = MockNativeContext::new();
    let cid = ClassId::new(CID);
    ctx.set_static_field(cid, 2, Value::Int(5));

    // getAndAdd: the witness is the old value, the sum is stored.
    let add2 = |cur: Value, _: &[Value]| match cur {
        Value::Int(v) => Some(Value::Int(v.wrapping_add(2))),
        _ => None,
    };
    let got = unsafe_static_rmw(&mut ctx, cid, 2, &[], &add2);
    assert_eq!(got, Some((Value::Int(5), true)));
    assert_eq!(ctx.get_static_field(cid, 2), Value::Int(7));
    assert_eq!(ctx.monitor_depth.get(), 0, "the class monitor was released");

    // A CAS whose witness is stale stores nothing and still reports the value.
    let cas = |cur: Value, live: &[Value]| (cur == live[0]).then_some(live[1]);
    let stale = [Value::Int(9), Value::Int(1)];
    let got = unsafe_static_rmw(&mut ctx, cid, 2, &stale, &cas);
    assert_eq!(got, Some((Value::Int(7), false)));
    assert_eq!(ctx.get_static_field(cid, 2), Value::Int(7), "nothing stored");
    let fresh = [Value::Int(7), Value::Int(1)];
    let got = unsafe_static_rmw(&mut ctx, cid, 2, &fresh, &cas);
    assert_eq!(got, Some((Value::Int(7), true)));
    assert_eq!(ctx.get_static_field(cid, 2), Value::Int(1));
    assert_eq!(ctx.monitor_depth.get(), 0);

    // A value whose kind is not the cell's: `f` declines, nothing stored.
    let got = unsafe_static_rmw(&mut ctx, cid, 2, &[], &|_, _| None);
    assert_eq!(got, Some((Value::Int(1), false)));
    assert_eq!(ctx.get_static_field(cid, 2), Value::Int(1));
}

#[test]
fn r23w1_l4_fallback_carries_a_reference_operand_through_the_lock_entry() {
    if !unsafe_static_hw_rmw_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let cid = ClassId::new(CID);
    ctx.set_static_field(cid, 3, Value::Object(None));
    let obj = ctx.alloc_object(ClassId::new(1), 1);
    // getAndSet(obj): the old (null) is the witness, `obj` is stored.
    let live = [Value::Object(Some(obj))];
    let got = unsafe_static_rmw(&mut ctx, cid, 3, &live, &|_, live| Some(live[0]));
    assert_eq!(got, Some((Value::Object(None), true)));
    match ctx.get_static_field(cid, 3) {
        Value::Object(Some(stored)) => assert_eq!(stored.as_ptr(), obj.as_ptr()),
        other => panic!("the reference was not stored: {other:?}"),
    }
    assert_eq!(ctx.monitor_depth.get(), 0, "the class monitor was released");
}
