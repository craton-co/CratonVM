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
fn remove_handler_on_the_juli_root_takes_it_out_of_the_root_registry() {
    const VM: usize = 0xD14_0201;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.apache.juli.ClassLoaderLogManager",
    );
    let root = allocate_logger(&mut ctx, "").unwrap();
    let handler =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/logging/Handler", 0).unwrap();
    let args = [Value::Object(Some(root)), Value::Object(Some(handler))];

    native_jul_logger_add_handler(&mut ctx, &args).unwrap();
    // The mock has no context loader, so the row is keyed 0.
    assert_eq!(
        tomcat_juli_root_handler_registry(VM)
            .lock()
            .unwrap()
            .get(&0)
            .cloned(),
        Some(vec![addr(handler)]),
        "addHandler on the JULI root files the handler under its loader"
    );

    native_jul_logger_remove_handler(&mut ctx, &args).unwrap();
    assert!(
        tomcat_juli_root_handler_registry(VM)
            .lock()
            .unwrap()
            .get(&0)
            .is_none(),
        "removeHandler on the JULI root must take the handler out of the registry \
         (and drop the emptied row), or the next JULI child logger copies it back"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin is released");
}

#[test]
fn a_dead_loaders_juli_rows_are_swept_and_a_moved_loaders_re_keyed() {
    const VM: usize = 0xD14_0202;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    let dead_loader = ctx.fresh_object_ref();
    let live_loader = ctx.fresh_object_ref();
    let moved_loader = ctx.fresh_object_ref();
    let logger = ctx.fresh_object_ref();
    let moved_logger = ctx.fresh_object_ref();
    let handler = ctx.fresh_object_ref();
    let (dead, live) = (addr(dead_loader) as usize, addr(live_loader) as usize);
    {
        let mut loggers = tomcat_juli_logger_registry(VM).lock().unwrap();
        loggers.insert((dead, "a".to_string()), addr(logger));
        loggers.insert((live, "a".to_string()), addr(logger));
        loggers.insert((0, "a".to_string()), addr(logger));
        let mut roots = tomcat_juli_root_handler_registry(VM).lock().unwrap();
        roots.insert(dead, vec![addr(handler)]);
        roots.insert(live, vec![addr(handler)]);
        roots.insert(0, vec![addr(handler)]);
    }

    // The loggers and handlers stay roots; the loaders are not.
    let mut scanned = Vec::new();
    gc_scan_logmanager_roots(VM, &mut scanned);
    assert!(scanned.contains(&logger) && scanned.contains(&handler));
    assert!(!scanned.contains(&dead_loader) && !scanned.contains(&live_loader));

    let dropped = gc_sweep_logging_rows(VM, &|a| a == live);
    assert_eq!(
        dropped, 2,
        "the dead loader's logger row and root-handler row"
    );
    {
        let loggers = tomcat_juli_logger_registry(VM).lock().unwrap();
        assert!(!loggers.contains_key(&(dead, "a".to_string())));
        assert!(loggers.contains_key(&(live, "a".to_string())));
        assert!(
            loggers.contains_key(&(0, "a".to_string())),
            "the no-loader row (key 0) is never judged"
        );
        let roots = tomcat_juli_root_handler_registry(VM).lock().unwrap();
        assert!(!roots.contains_key(&dead));
        assert!(roots.contains_key(&live) && roots.contains_key(&0));
    }

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(live, addr(moved_loader) as usize);
    moved.insert(addr(logger) as usize, addr(moved_logger) as usize);
    gc_update_logmanager_refs(VM, &moved);
    let new = addr(moved_loader) as usize;
    assert_eq!(
        tomcat_juli_logger_registry(VM)
            .lock()
            .unwrap()
            .get(&(new, "a".to_string()))
            .copied(),
        Some(addr(moved_logger)),
        "a moved loader's row is re-keyed and its logger re-addressed"
    );
    assert!(!tomcat_juli_logger_registry(VM)
        .lock()
        .unwrap()
        .contains_key(&(live, "a".to_string())));
    assert_eq!(
        tomcat_juli_root_handler_registry(VM)
            .lock()
            .unwrap()
            .get(&new)
            .cloned(),
        Some(vec![addr(handler)])
    );
}
