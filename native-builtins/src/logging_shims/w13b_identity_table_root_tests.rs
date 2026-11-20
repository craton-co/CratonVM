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

#[test]
fn replacing_a_loggers_handler_list_releases_the_old_root() {
    const VM: usize = 0xB13_0101;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let logger = ctx.fresh_object_ref();
    let first = ctx.fresh_object_ref();
    let second = ctx.fresh_object_ref();

    let roots = ctx.global_root_count();
    jul_logger_handlers_set(&mut ctx, logger, first);
    jul_logger_handlers_set(&mut ctx, logger, second);
    assert_eq!(
        ctx.global_root_count(),
        roots,
        "a row takes no global root, so a replaced list cannot leak one"
    );
    assert_eq!(jul_logger_handlers_get(&mut ctx, logger), Some(second));
    let mut scanned = Vec::new();
    gc_scan_jul_side_rows(VM, &mut scanned);
    assert_eq!(scanned, vec![second], "only the current list is rooted");
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn replacing_a_loggers_parent_releases_the_old_root() {
    const VM: usize = 0xB13_0102;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let logger = ctx.fresh_object_ref();
    let first = ctx.fresh_object_ref();
    let second = ctx.fresh_object_ref();

    let roots = ctx.global_root_count();
    jul_logger_parent_set(&mut ctx, logger, Some(first));
    jul_logger_parent_set(&mut ctx, logger, Some(second));
    assert_eq!(ctx.global_root_count(), roots);
    assert_eq!(jul_logger_parent_get(&mut ctx, logger), Some(second));
    jul_logger_parent_set(&mut ctx, logger, None);
    assert_eq!(ctx.global_root_count(), roots);
    assert_eq!(jul_logger_parent_get(&mut ctx, logger), None);
    let mut scanned = Vec::new();
    gc_scan_jul_side_rows(VM, &mut scanned);
    assert!(scanned.is_empty(), "a cleared parent is no longer rooted");
}

#[test]
fn teardown_forgets_the_identity_keyed_rows() {
    const VM: usize = 0xB13_0103;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let logger = ctx.fresh_object_ref();
    let list = ctx.fresh_object_ref();
    jul_logger_handlers_set(&mut ctx, logger, list);
    assert_eq!(jul_logger_handlers_get(&mut ctx, logger), Some(list));

    crate::logmanager::forget_vm_logging_state(VM);

    assert_eq!(jul_logger_handlers_get(&mut ctx, logger), None);
}
