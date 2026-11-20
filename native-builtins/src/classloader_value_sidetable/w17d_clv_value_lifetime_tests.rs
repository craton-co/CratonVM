use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key / stored value only; nothing on these paths
    // dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

/// Drops the test's lock keys (and so its rows) and its `metadata_pin`
/// rows even when an assertion fails first.
struct Cleanup(usize);
impl Drop for Cleanup {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(self.0);
    }
}

fn call(
    f: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    ctx: &mut crate::test_utils::MockNativeContext,
    args: &[Value],
) -> MethodCallResult {
    f(ctx, args)
}

fn put(
    ctx: &mut crate::test_utils::MockNativeContext,
    clv: ObjectRef,
    cl: Option<ObjectRef>,
    value: ObjectRef,
) -> Option<ObjectRef> {
    match call(
        native_aclv_put_if_absent,
        ctx,
        &[
            Value::Object(Some(clv)),
            Value::Object(cl),
            Value::Object(Some(value)),
        ],
    ) {
        Ok(Some(Value::Object(o))) => o,
        _ => panic!("AbstractClassLoaderValue.putIfAbsent answered a non-object"),
    }
}

fn get(
    ctx: &mut crate::test_utils::MockNativeContext,
    clv: ObjectRef,
    cl: Option<ObjectRef>,
) -> Option<ObjectRef> {
    match call(
        native_aclv_get,
        ctx,
        &[Value::Object(Some(clv)), Value::Object(cl)],
    ) {
        Ok(Some(Value::Object(o))) => o,
        _ => panic!("AbstractClassLoaderValue.get answered a non-object"),
    }
}

fn remove(
    ctx: &mut crate::test_utils::MockNativeContext,
    clv: ObjectRef,
    cl: Option<ObjectRef>,
    value: Option<ObjectRef>,
) -> bool {
    match call(
        native_aclv_remove,
        ctx,
        &[
            Value::Object(Some(clv)),
            Value::Object(cl),
            Value::Object(value),
        ],
    ) {
        Ok(Some(Value::Int(z))) => z != 0,
        _ => panic!("AbstractClassLoaderValue.remove answered a non-boolean"),
    }
}

/// The roots one scan of `vm` reports, with the licence (the deferral
/// predicate) fixed to `deferrable`. Pins go to `metadata_pin`.
fn scan(vm: usize, deferrable: bool) -> Vec<usize> {
    let mut out = Vec::new();
    gc_scan_clv_value_roots(vm, &mut out, &|_| deferrable);
    out.into_iter().map(addr).collect()
}

fn pinned_to(loader: ObjectRef) -> Vec<usize> {
    cratonvm_types::metadata_pin::roots_for_loader(addr(loader)).unwrap_or_default()
}

/// The page's defect: the value of a loader's row was a permanent root,
/// so a value that reaches its loader kept the loader alive. Under the
/// loader-conditional licence it is now pinned to the loader and not
/// rooted; without the licence it is rooted, as every loader-owned value
/// is; a bootstrap row is always rooted (that loader never dies).
#[test]
fn a_loader_rows_value_is_pinned_to_its_loader_not_rooted() {
    const VM: usize = 0xD170_0001;
    let _cleanup = Cleanup(VM);
    let mut c = ctx_for(VM);
    let (cl, clv, val, boot_val) = (
        obj(0x1_D17A_0010),
        obj(0x1_D17A_0020),
        obj(0x1_D17A_0030),
        obj(0x1_D17A_0040),
    );
    assert_eq!(put(&mut c, clv, Some(cl), val), None);
    assert_eq!(put(&mut c, clv, None, boot_val), None);

    assert_eq!(
        scan(VM, true),
        vec![addr(boot_val)],
        "licensed: only the bootstrap row roots"
    );
    assert_eq!(
        pinned_to(cl),
        vec![addr(val)],
        "licensed: the value hangs off its loader"
    );

    let mut unlicensed = scan(VM, false);
    unlicensed.sort_unstable();
    let mut both = vec![addr(val), addr(boot_val)];
    both.sort_unstable();
    assert_eq!(unlicensed, both, "no licence: every value is a root");

    // Another VM's scan reports none of this VM's rows.
    assert!(scan(0xD170_00FF, false).is_empty());
}

/// The row is the value's only native reference: once the loader dies and
/// the epilogue drops the row, the value is reported by nobody. (The var-
/// handle root it used to be had no release, so it stayed until teardown.)
#[test]
fn a_dead_loader_takes_its_value_out_of_every_scan() {
    const VM: usize = 0xD170_0002;
    let _cleanup = Cleanup(VM);
    let mut c = ctx_for(VM);
    let (cl, clv, val) = (obj(0x1_D17B_0010), obj(0x1_D17B_0020), obj(0x1_D17B_0030));
    put(&mut c, clv, Some(cl), val);
    assert_eq!(scan(VM, false), vec![addr(val)]);
    let loader = addr(cl);
    crate::gc_sweep_lock_keys(VM, &|a| a != loader);
    assert!(
        scan(VM, false).is_empty(),
        "a dropped row's value is still reported"
    );
    assert!(scan(VM, true).is_empty());
}

/// A losing `putIfAbsent` and a null value leave nothing behind.
#[test]
fn a_losing_or_null_put_if_absent_stores_nothing() {
    const VM: usize = 0xD170_0003;
    let _cleanup = Cleanup(VM);
    let mut c = ctx_for(VM);
    let (cl, clv, winner, loser) = (
        obj(0x1_D17C_0010),
        obj(0x1_D17C_0020),
        obj(0x1_D17C_0030),
        obj(0x1_D17C_0040),
    );
    assert_eq!(put(&mut c, clv, Some(cl), winner), None);
    assert_eq!(put(&mut c, clv, Some(cl), loser), Some(winner));
    assert_eq!(scan(VM, false), vec![addr(winner)], "the loser is reported");

    // A null value is the JDK map's NPE, and stores no "present" null.
    let clv2 = obj(0x1_D17C_0050);
    let npe = call(
        native_aclv_put_if_absent,
        &mut c,
        &[
            Value::Object(Some(clv2)),
            Value::Object(Some(cl)),
            Value::Object(None),
        ],
    );
    assert!(npe.is_err(), "putIfAbsent(cl, null) must throw");
    assert_eq!(
        put(&mut c, clv2, Some(cl), winner),
        None,
        "a null row was stored"
    );
}

/// The remap re-addresses the loader and the value, so the value is read
/// at its new address and pinned to the loader's new address.
#[test]
fn a_moved_loader_and_value_are_re_addressed_in_the_row() {
    const VM: usize = 0xD170_0004;
    let _cleanup = Cleanup(VM);
    let mut c = ctx_for(VM);
    let (cl_old, cl_new) = (obj(0x1_D17D_0010), obj(0x3_D17D_0010));
    let (clv_old, clv_new) = (obj(0x1_D17D_0020), obj(0x3_D17D_0020));
    let (val_old, val_new) = (obj(0x1_D17D_0030), obj(0x3_D17D_0030));
    put(&mut c, clv_old, Some(cl_old), val_old);
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(addr(cl_old), addr(cl_new));
    moved.insert(addr(clv_old), addr(clv_new));
    moved.insert(addr(val_old), addr(val_new));
    crate::gc_sweep_and_remap_lock_keys(
        VM,
        &|a| a == addr(cl_old) || a == addr(clv_old),
        &moved,
    );
    gc_update_clv_value_refs(VM, &moved);
    // Another VM's remap leaves this VM's row alone.
    gc_update_clv_value_refs(0xD170_00FF, &moved);
    assert_eq!(get(&mut c, clv_new, Some(cl_new)), Some(val_new));
    assert!(scan(VM, true).is_empty());
    assert_eq!(pinned_to(cl_new), vec![addr(val_new)]);
    assert!(
        pinned_to(cl_old).is_empty(),
        "pinned to the loader's old address"
    );
}

const CIA_CL_OLD: usize = 0x1_D17E_0010;
const CIA_CL_NEW: usize = 0x3_D17E_0010;
const CIA_VAL: usize = 0x1_D17E_0030;

/// `apply` "moves" the loader (the mock's pin slot is re-addressed, as a
/// moving collection inside the call would) and answers `CIA_VAL`.
fn apply_moves_the_loader(
    ctx: &mut crate::test_utils::MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method != "apply" {
        return None;
    }
    ctx.remap_native_pin_addr_for_test(CIA_CL_OLD, CIA_CL_NEW);
    Some(Ok(Some(Value::Object(Some(obj(CIA_VAL))))))
}

/// `computeIfAbsent` stores the loader's POST-`apply` address, so the
/// value is pinned to where the loader is, not where it was.
#[test]
fn compute_if_absent_records_the_loader_where_apply_left_it() {
    const VM: usize = 0xD170_0005;
    let _cleanup = Cleanup(VM);
    let mut c = ctx_for(VM);
    c.set_invoke_virtual_hook(apply_moves_the_loader);
    let (clv, func) = (obj(0x1_D17E_0020), obj(0x1_D17E_0040));
    let pins_before = c.native_pin_count_for_test();
    let got = call(
        native_aclv_compute_if_absent,
        &mut c,
        &[
            Value::Object(Some(clv)),
            Value::Object(Some(obj(CIA_CL_OLD))),
            Value::Object(Some(func)),
        ],
    );
    assert_eq!(got.ok().flatten(), Some(Value::Object(Some(obj(CIA_VAL)))));
    assert_eq!(
        c.native_pin_count_for_test(),
        pins_before,
        "the handle scope leaked"
    );
    assert!(scan(VM, true).is_empty());
    assert_eq!(pinned_to(obj(CIA_CL_NEW)), vec![CIA_VAL]);
    assert!(pinned_to(obj(CIA_CL_OLD)).is_empty());
}

/// `remove(cl, v)`: identity match removes; a null `v` and a missing row
/// answer false, as the JDK map does.
#[test]
fn remove_follows_the_jdk_maps_answers() {
    const VM: usize = 0xD170_0006;
    let _cleanup = Cleanup(VM);
    let mut c = ctx_for(VM);
    let (cl, clv, val, other) = (
        obj(0x1_D17F_0010),
        obj(0x1_D17F_0020),
        obj(0x1_D17F_0030),
        obj(0x1_D17F_0040),
    );
    put(&mut c, clv, Some(cl), val);
    assert!(
        !remove(&mut c, clv, Some(cl), None),
        "remove(cl, null) must be false"
    );
    // The mock's `equals` answers nothing, i.e. "not equal".
    assert!(!remove(&mut c, clv, Some(cl), Some(other)));
    assert_eq!(get(&mut c, clv, Some(cl)), Some(val));
    assert!(remove(&mut c, clv, Some(cl), Some(val)));
    assert_eq!(get(&mut c, clv, Some(cl)), None);
    assert!(
        !remove(&mut c, clv, Some(cl), Some(val)),
        "nothing left to remove"
    );
    assert!(scan(VM, false).is_empty());
}
