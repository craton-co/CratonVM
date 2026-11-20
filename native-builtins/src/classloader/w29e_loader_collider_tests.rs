use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

struct Teardown(usize);
impl Drop for Teardown {
    fn drop(&mut self) {
        forget_vm_loader_singletons(self.0);
    }
}

/// Closing one of two same-hash loaders closes only it; reopening the
/// other (never closed) leaves the first closed.
#[test]
fn a_same_hash_open_loader_is_not_closed() {
    const VM: usize = 0x29E0_2001;
    let _t = Teardown(VM);
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let (closed, open) = (at(0x1_29E0_2000), at(0x2_29E0_2000));
    assert_eq!(ctx.identity_hash_code(closed), ctx.identity_hash_code(open));

    ucl_mark_closed(&ctx, closed);
    assert!(ucl_is_closed(&ctx, closed));
    assert!(
        !ucl_is_closed(&ctx, open),
        "the open collider is not closed"
    );
    ucl_mark_open(&ctx, open);
    assert!(ucl_is_closed(&ctx, closed), "reopening B does not reopen A");
    ucl_mark_closed(&ctx, open);
    ucl_mark_open(&ctx, closed);
    assert!(!ucl_is_closed(&ctx, closed));
    assert!(ucl_is_closed(&ctx, open));
}

/// The re-entry guard matches its receiver by pin, not by identity hash:
/// a same-hash loader is not the nested delegation of the first.
#[test]
fn the_single_load_class_guard_tells_same_hash_loaders_apart() {
    let mut ctx = MockNativeContext::new();
    let (outer, collider) = (at(0x1_29E0_2008), at(0x2_29E0_2008));
    assert_eq!(
        ctx.identity_hash_code(outer),
        ctx.identity_hash_code(collider)
    );
    let base = ctx.native_pin_count_for_test();

    let outer_pin = single_load_class_guard_enter(&mut ctx, outer).expect("first entry");
    assert_eq!(
        single_load_class_guard_enter(&mut ctx, outer),
        None,
        "the same loader again is the nested super.loadClass"
    );
    let collider_pin = single_load_class_guard_enter(&mut ctx, collider)
        .expect("a same-hash loader is not a re-entry");
    single_load_class_guard_leave(&mut ctx, collider_pin);
    single_load_class_guard_leave(&mut ctx, outer_pin);
    assert_eq!(ctx.native_pin_count_for_test(), base, "every pin released");
    SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|a| assert!(a.borrow().is_empty()));
}

/// gc-common w33-a (`identity-hash-keyed-side-tables-collide-sooner-with-the-8-byte-header`):
/// with the 8-byte header's 20-bit hash, one bucket routinely holds
/// several loaders. Three live loaders share one hash: A (closed) dies, B
/// (closed) moves, C stays open. The reconcile drops only A's row and
/// re-addresses only B's; C is still open, and a newcomer allocated at A's
/// vacated address -- same address, so same mock hash -- is not closed.
#[test]
fn w33a_a_multi_row_bucket_survives_one_collider_dying_and_another_moving() {
    const VM: usize = 0x33A0_1001;
    let _t = Teardown(VM);
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let (a, b, c) = (at(0x1_33A0_1000), at(0x2_33A0_1000), at(0x3_33A0_1000));
    let b_to: usize = 0x4_33A0_1000;
    let b_now = at(b_to);
    let hash = ctx.identity_hash_code(a);
    for o in [b, c, b_now] {
        assert_eq!(ctx.identity_hash_code(o), hash, "premise: one bucket");
    }

    ucl_mark_closed(&ctx, a);
    ucl_mark_closed(&ctx, b);
    assert!(ucl_is_closed(&ctx, a) && ucl_is_closed(&ctx, b));
    assert!(!ucl_is_closed(&ctx, c), "the open collider is not closed");

    let (a_addr, b_from) = (a.as_ptr() as usize, b.as_ptr() as usize);
    let is_marked = move |addr: usize| addr != a_addr;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(b_from, b_to);
    gc_reconcile_defining_loaders(detached_loader_tables(VM), &is_marked, &pointer_map, None);

    assert!(
        ucl_is_closed(&ctx, b_now),
        "the moved collider stays closed"
    );
    assert!(!ucl_is_closed(&ctx, b), "its vacated address is not closed");
    assert!(!ucl_is_closed(&ctx, c), "the open collider is still open");
    let newcomer = at(a_addr);
    assert!(
        !ucl_is_closed(&ctx, newcomer),
        "a loader at the dead collider's address is not closed"
    );
    let rows: Vec<usize> = closed_url_classloader_ids(detached_loader_tables(VM))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&hash)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        rows,
        vec![b_to],
        "one row left in the bucket, at B's new address"
    );
}
