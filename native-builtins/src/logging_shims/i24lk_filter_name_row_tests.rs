use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Forgets the VM's logging rows however the test ends.
struct ForgetVm(usize);
impl Drop for ForgetVm {
    fn drop(&mut self) {
        crate::logmanager::forget_vm_logging_state(self.0);
    }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

#[test]
fn gci_i24lk_a_name_keyed_filter_row_goes_with_its_owner_and_moves_with_it() {
    const VM: usize = 0xE24_1201;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    let owner = ctx.fresh_object_ref();
    let other_owner = ctx.fresh_object_ref();
    let filter = ctx.fresh_object_ref();
    let other_filter = ctx.fresh_object_ref();
    let moved_owner = ctx.fresh_object_ref();
    let moved_filter = ctx.fresh_object_ref();
    {
        let mut names = jul_rows_lock(jul_logger_filter_names_table(VM));
        names.insert("a".to_string(), (addr(owner), addr(filter)));
        names.insert("b".to_string(), (addr(other_owner), addr(other_filter)));
    }
    let mut plain = Vec::new();
    gc_scan_jul_side_rows(VM, &mut plain);
    assert!(plain.contains(&filter) && plain.contains(&other_filter));
    assert!(
        !plain.contains(&owner) && !plain.contains(&other_owner),
        "the owner is weak: a row never reports it"
    );

    let dead = addr(other_owner);
    assert_eq!(
        gc_sweep_jul_side_rows(VM, &|a| a != dead),
        1,
        "only the dead owner's filter row goes (a name no longer pins it)"
    );
    let mut plain = Vec::new();
    gc_scan_jul_side_rows(VM, &mut plain);
    assert_eq!(plain, vec![filter]);

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(addr(owner), addr(moved_owner));
    moved.insert(addr(filter), addr(moved_filter));
    gc_update_jul_side_rows(VM, &moved);
    assert_eq!(
        jul_rows_lock(jul_logger_filter_names_table(VM))
            .get("a")
            .copied(),
        Some((addr(moved_owner), addr(moved_filter))),
        "both halves of the row are re-addressed"
    );
    let survivor = addr(moved_owner);
    assert_eq!(gc_sweep_jul_side_rows(VM, &|a| a == survivor), 0);
}

#[test]
fn gci_i24lk_the_ephemeron_scan_pins_a_name_keyed_filter_to_its_owner() {
    const VM: usize = 0xE24_1202;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    let owner = ctx.fresh_object_ref();
    let filter = ctx.fresh_object_ref();
    jul_rows_lock(jul_logger_filter_names_table(VM))
        .insert("a".to_string(), (addr(owner), addr(filter)));

    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    gc_scan_jul_side_rows_ephemeron(VM, &mut out, &mut rooted, &|_| true, &mut pins);
    assert!(out.is_empty() && rooted.is_empty());
    assert_eq!(pins, vec![(addr(owner), addr(filter))]);

    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    gc_scan_jul_side_rows_ephemeron(VM, &mut out, &mut rooted, &|_| false, &mut pins);
    assert!(out.is_empty() && pins.is_empty());
    assert_eq!(rooted, vec![filter]);
}
