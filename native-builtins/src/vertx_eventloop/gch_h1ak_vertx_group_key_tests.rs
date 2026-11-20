//! gch h1/ak: the `VertxImpl` group row is filed under the object's weak
//! lock key, so `close()` finds it after a collection moved the object,
//! and a stranger later placed at a dead instance's address finds nothing.
use super::*;
use crate::test_utils::mock_ctx;

#[test]
fn gch_h1ak_vertx_group_follows_a_moved_vertx_and_not_its_address() {
    const VM: usize = 0x7E87_0001;
    let ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only; nothing on these paths dereferences them.
    let obj = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let (old, new) = (0x1_7E87_0010usize, 0x3_7E87_0010usize);
    let key = crate::gc_stable_weak_lock_key(&ctx, obj(old)).expect("minted");
    store_group(key, vec![901, 902]);

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    // `close()`'s lookup at the vacated address: nothing.
    assert_eq!(
        crate::existing_weak_lock_key(&ctx, obj(old)).and_then(take_group),
        None
    );
    // `close()`'s lookup at the new address: the group.
    assert_eq!(
        crate::existing_weak_lock_key(&ctx, obj(new)).and_then(take_group),
        Some(vec![901, 902])
    );
    crate::forget_vm_lock_keys(VM);
}
