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

fn addrs(v: &[ObjectRef]) -> Vec<usize> {
    let mut out: Vec<usize> = v.iter().map(|o| addr(*o)).collect();
    out.sort_unstable();
    out
}

#[test]
fn gci_i23ep_a_deferring_scan_pins_each_value_to_its_own_key_and_roots_none() {
    const VM: usize = 0xE23_0101;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let logger = ctx.fresh_object_ref();
    let handler = ctx.fresh_object_ref();
    let list = ctx.fresh_object_ref();
    let parent = ctx.fresh_object_ref();
    let manager = ctx.fresh_object_ref();
    jul_logger_handlers_set(&mut ctx, logger, list);
    jul_logger_parent_set(&mut ctx, logger, Some(parent));
    jul_handler_error_manager_set(&mut ctx, handler, manager);

    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    gc_scan_jul_side_rows_ephemeron(VM, &mut out, &mut rooted, &|_| true, &mut pins);
    assert!(out.is_empty(), "no name-keyed filter row: {out:?}");
    assert!(rooted.is_empty(), "every value deferred: {rooted:?}");
    pins.sort_unstable();
    let mut want = vec![
        (addr(logger), addr(list)),
        (addr(logger), addr(parent)),
        (addr(handler), addr(manager)),
    ];
    want.sort_unstable();
    assert_eq!(pins, want, "each value is pinned to the key it stands in a field of");
    assert!(
        !pins.iter().any(|&(_, v)| v == addr(logger) || v == addr(handler)),
        "a key is never reported, as a root or as a value"
    );

    // A value the predicate refuses (a young value on a moving cycle) is
    // rooted; the others stay pinned.
    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    let refused = addr(parent);
    gc_scan_jul_side_rows_ephemeron(VM, &mut out, &mut rooted, &|a| a != refused, &mut pins);
    assert_eq!(rooted, vec![parent]);
    assert_eq!(pins.len(), 2);
    assert!(!pins.iter().any(|&(_, v)| v == refused));
}

#[test]
fn gci_i23ep_a_scan_that_defers_nothing_reports_the_plain_scans_roots() {
    const VM: usize = 0xE23_0102;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let logger = ctx.fresh_object_ref();
    let handler = ctx.fresh_object_ref();
    let list = ctx.fresh_object_ref();
    let parent = ctx.fresh_object_ref();
    let manager = ctx.fresh_object_ref();
    let filter = ctx.fresh_object_ref();
    jul_logger_handlers_set(&mut ctx, logger, list);
    jul_logger_parent_set(&mut ctx, logger, Some(parent));
    jul_handler_error_manager_set(&mut ctx, handler, manager);
    jul_logger_filter_set(&mut ctx, logger, Some(filter));

    let mut plain = Vec::new();
    gc_scan_jul_side_rows(VM, &mut plain);
    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    gc_scan_jul_side_rows_ephemeron(VM, &mut out, &mut rooted, &|_| false, &mut pins);
    assert!(pins.is_empty());
    let mut both = out.clone();
    both.extend(rooted.iter().copied());
    assert_eq!(addrs(&both), addrs(&plain), "same roots as the plain JUL scan");

    // The whole `logmanager` row: the registry roots are shared unchanged.
    let mut plain = Vec::new();
    crate::logmanager::gc_scan_logmanager_roots(VM, &mut plain);
    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    crate::logmanager::gc_scan_logmanager_roots_ephemeron(
        VM,
        &mut out,
        &mut rooted,
        &|_| false,
        &mut pins,
    );
    assert!(pins.is_empty());
    let mut both = out.clone();
    both.extend(rooted.iter().copied());
    assert_eq!(addrs(&both), addrs(&plain), "same roots as the plain logmanager scan");
    assert!(!addrs(&both).contains(&addr(logger)), "the key stays weak");
}
