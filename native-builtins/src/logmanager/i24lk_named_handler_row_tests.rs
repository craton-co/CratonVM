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
        forget_vm_logging_state(self.0);
    }
}

fn addr(obj: ObjectRef) -> u64 {
    obj.as_ptr() as u64
}

#[test]
fn gci_i24lk_add_handler_files_a_row_its_logger_owns_and_it_goes_with_the_logger() {
    const VM: usize = 0xE24_1101;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let logger = allocate_logger(&mut ctx, "gcilk24.app").unwrap();
    let handler =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/logging/Handler", 0).unwrap();
    native_jul_logger_add_handler(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(Some(handler))],
    )
    .unwrap();
    assert_eq!(
        logger_handlers(VM)
            .lock()
            .unwrap()
            .get("gcilk24.app")
            .cloned(),
        Some(vec![(addr(logger), addr(handler))]),
        "the name-keyed row names the Logger that filed it"
    );
    let mut scanned = Vec::new();
    gc_scan_logmanager_roots(VM, &mut scanned);
    assert!(
        scanned.contains(&handler),
        "a handler is strong while its owner lives (a `config.handlers` element)"
    );

    // The owner did not survive: its row goes in the same sweep, so the
    // handler is no longer reported (it used to be a root for good).
    let owner = addr(logger) as usize;
    assert!(gc_sweep_logging_rows(VM, &|a| a != owner) >= 1);
    assert!(
        !logger_handlers(VM)
            .lock()
            .unwrap()
            .contains_key("gcilk24.app"),
        "the dead owner's row, and the emptied name, are dropped"
    );
    let mut scanned = Vec::new();
    gc_scan_logmanager_roots(VM, &mut scanned);
    assert!(
        !scanned.contains(&handler),
        "nothing roots a dead Logger's handler any more"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin is released");
}

#[test]
fn gci_i24lk_a_named_row_lives_while_its_owner_does_and_moves_with_it() {
    const VM: usize = 0xE24_1102;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    let owner_a = ctx.fresh_object_ref();
    let owner_b = ctx.fresh_object_ref();
    let moved_b = ctx.fresh_object_ref();
    let h1 = ctx.fresh_object_ref();
    let h2 = ctx.fresh_object_ref();
    let moved_h2 = ctx.fresh_object_ref();
    logger_handlers(VM).lock().unwrap().insert(
        "n".to_string(),
        vec![(addr(owner_a), addr(h1)), (addr(owner_b), addr(h2))],
    );

    let dead = addr(owner_a) as usize;
    assert_eq!(
        gc_sweep_logging_rows(VM, &|a| a != dead),
        1,
        "only the dead owner's row goes"
    );
    assert_eq!(
        logger_handlers(VM).lock().unwrap().get("n").cloned(),
        Some(vec![(addr(owner_b), addr(h2))])
    );
    let mut scanned = Vec::new();
    gc_scan_logmanager_roots(VM, &mut scanned);
    assert!(scanned.contains(&h2) && !scanned.contains(&h1));
    assert!(
        !scanned.contains(&owner_b),
        "the owner is weak: the row never reports it"
    );

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(addr(owner_b) as usize, addr(moved_b) as usize);
    moved.insert(addr(h2) as usize, addr(moved_h2) as usize);
    gc_update_logmanager_refs(VM, &moved);
    assert_eq!(
        logger_handlers(VM).lock().unwrap().get("n").cloned(),
        Some(vec![(addr(moved_b), addr(moved_h2))]),
        "both halves of a row are re-addressed"
    );
    let survivor = addr(moved_b) as usize;
    assert_eq!(gc_sweep_logging_rows(VM, &|a| a == survivor), 0);
    assert_eq!(gc_sweep_logging_rows(VM, &|_| false), 1);
    assert!(logger_handlers(VM).lock().unwrap().is_empty());
}

#[test]
fn gci_i24lk_the_name_keyed_delivery_skips_the_logged_loggers_own_rows() {
    let rows: Vec<NamedHandlerRow> = vec![(1, 10), (2, 20), (1, 11), (3, 20), (4, 0)];
    assert_eq!(
        named_handlers_to_publish(&rows, 1),
        vec![20],
        "the logged Logger's own handlers are the identity list's to deliver, once"
    );
    assert_eq!(
        named_handlers_to_publish(&rows, 9),
        vec![10, 20, 11],
        "another object of the name: every handler once, in filing order"
    );
    assert!(named_handlers_to_publish(&[], 1).is_empty());
}

#[test]
fn gci_i24lk_the_ephemeron_scan_pins_a_named_row_to_its_owner() {
    const VM: usize = 0xE24_1103;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    let owner = ctx.fresh_object_ref();
    let handler = ctx.fresh_object_ref();
    logger_handlers(VM)
        .lock()
        .unwrap()
        .insert("n".to_string(), vec![(addr(owner), addr(handler))]);

    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    gc_scan_logmanager_roots_ephemeron(VM, &mut out, &mut rooted, &|_| true, &mut pins);
    assert!(!out.contains(&handler) && !rooted.contains(&handler));
    assert!(pins.contains(&(addr(owner) as usize, addr(handler) as usize)));

    let (mut out, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    gc_scan_logmanager_roots_ephemeron(VM, &mut out, &mut rooted, &|_| false, &mut pins);
    assert!(pins.is_empty());
    assert_eq!(rooted, vec![handler], "a value the predicate refuses is rooted");
    assert!(!out.contains(&handler));
}
