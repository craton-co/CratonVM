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
fn a_dead_keys_rows_are_swept_and_a_live_keys_are_kept() {
    const VM: usize = 0xD14_0101;
    const OTHER_VM: usize = 0xD14_01FF;
    let _forget = ForgetVm(VM);
    let _forget_other = ForgetVm(OTHER_VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let live = ctx.fresh_object_ref();
    let dead = ctx.fresh_object_ref();
    let list = ctx.fresh_object_ref();
    let parent = ctx.fresh_object_ref();
    let manager = ctx.fresh_object_ref();
    let filter = ctx.fresh_object_ref();
    for key in [live, dead] {
        jul_logger_handlers_set(&mut ctx, key, list);
        jul_logger_parent_set(&mut ctx, key, Some(parent));
        jul_handler_error_manager_set(&mut ctx, key, manager);
        jul_logger_filter_set(&mut ctx, key, Some(filter));
        jul_file_handler_set_filename(&mut ctx, key, Some("w14d.log".to_string()));
    }
    let live_addr = live.as_ptr() as usize;

    // Another VM's sweep judges none of this VM's rows.
    assert_eq!(
        crate::logmanager::gc_sweep_logging_rows(OTHER_VM, &|_| false),
        0
    );
    let dropped = crate::logmanager::gc_sweep_logging_rows(VM, &|addr| addr == live_addr);

    assert_eq!(
        dropped, 5,
        "the dead key's handler list, parent, ErrorManager, filter and FileHandler rows"
    );
    assert_eq!(jul_logger_handlers_get(&mut ctx, dead), None);
    assert_eq!(jul_logger_parent_get(&mut ctx, dead), None);
    assert_eq!(jul_handler_error_manager_get(&mut ctx, dead), None);
    assert_eq!(jul_logger_filter_get(&mut ctx, dead), None);
    assert_eq!(jul_file_handler_filename(&mut ctx, dead), None);

    assert_eq!(jul_logger_handlers_get(&mut ctx, live), Some(list));
    assert_eq!(jul_logger_parent_get(&mut ctx, live), Some(parent));
    assert_eq!(jul_handler_error_manager_get(&mut ctx, live), Some(manager));
    assert_eq!(jul_logger_filter_get(&mut ctx, live), Some(filter));
    assert_eq!(
        jul_file_handler_filename(&mut ctx, live).as_deref(),
        Some("w14d.log")
    );
    // Idempotent: nothing more to drop.
    assert_eq!(
        crate::logmanager::gc_sweep_logging_rows(VM, &|addr| addr == live_addr),
        0
    );
}

#[test]
fn values_are_roots_keys_are_not_and_both_are_re_addressed_after_a_move() {
    const VM: usize = 0xD14_0102;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let logger = ctx.fresh_object_ref();
    let list = ctx.fresh_object_ref();
    let moved_logger = ctx.fresh_object_ref();
    let moved_list = ctx.fresh_object_ref();
    jul_logger_handlers_set(&mut ctx, logger, list);
    jul_file_handler_set_filename(&mut ctx, logger, Some("moved.log".to_string()));

    let mut roots = Vec::new();
    crate::logmanager::gc_scan_logmanager_roots(VM, &mut roots);
    assert!(roots.contains(&list), "the value is reported as a root");
    assert!(!roots.contains(&logger), "the key is weak");

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(logger.as_ptr() as usize, moved_logger.as_ptr() as usize);
    moved.insert(list.as_ptr() as usize, moved_list.as_ptr() as usize);
    crate::logmanager::gc_update_logmanager_refs(VM, &moved);

    assert_eq!(
        jul_logger_handlers_get(&mut ctx, moved_logger),
        Some(moved_list),
        "the row follows its key and names the value at its new address"
    );
    assert_eq!(jul_logger_handlers_get(&mut ctx, logger), None);
    assert_eq!(
        jul_file_handler_filename(&mut ctx, moved_logger).as_deref(),
        Some("moved.log")
    );
    assert_eq!(jul_file_handler_filename(&mut ctx, logger), None);
}
