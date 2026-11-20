use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

struct Teardown(usize);
impl Drop for Teardown {
    fn drop(&mut self) {
        forget_vm_msc_service_roots(self.0);
        forget_vm_msc_container(self.0);
    }
}

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn scan(vm: usize) -> Vec<usize> {
    let mut out = Vec::new();
    gc_scan_msc_service_roots(vm, &mut out);
    let mut addrs: Vec<usize> = out.iter().map(|r| r.as_ptr() as usize).collect();
    addrs.sort_unstable();
    addrs
}

fn addr(r: ObjectRef) -> usize {
    r.as_ptr() as usize
}

/// A synthetic controller mirror bound to controller `id`.
fn mirror_for(ctx: &mut MockNativeContext, id: u64) -> ObjectRef {
    let mirror = ctx.fresh_object_ref();
    ctx.set_field(mirror, SC_FIELD_ID, Value::Long(id as i64));
    mirror
}

/// `getState()` on `mirror`. The mock has no `ServiceController$State`
/// enum, so the native answers the legacy ordinal.
fn java_state(ctx: &mut MockNativeContext, mirror: ObjectRef) -> Option<Value> {
    native_service_controller_get_state(ctx, &[Value::Object(Some(mirror))])
        .ok()
        .flatten()
}

#[test]
fn a_duplicate_install_of_a_live_name_fails_and_registers_nothing() {
    let c = ServiceContainer::new();
    let n = ServiceName::parse("w12d.msc.duplicate.live");
    let first = c
        .add_service(n.clone(), vec![], Mode::Never, 0)
        .expect("first install");
    // Installs `n` again; true when that is refused as a duplicate.
    let refused_again = |mode: Mode| {
        let r = c.add_service(n.clone(), vec![], mode, 0);
        matches!(r, Err(ref msg) if msg.starts_with(DUPLICATE_SERVICE_PREFIX))
    };

    // Down, Up and Failed all hold the name.
    assert!(
        refused_again(Mode::Active),
        "a Down controller holds its name"
    );
    c.start_without_callback(first);
    assert_eq!(c.get_state(&n), Some(ServiceState::Up));
    assert!(
        refused_again(Mode::Active),
        "an Up controller holds its name"
    );
    c.record_failure(first, "boom".into());
    assert!(
        refused_again(Mode::Active),
        "a Failed controller holds its name"
    );

    // Nothing was registered by the refused installs, and the first
    // controller is untouched.
    assert_eq!(c.get_id(&n), Some(first));
    assert_eq!(c.size(), 1);
    {
        let st = c.inner.lock().unwrap();
        assert_eq!(st.by_id.len(), 1, "a refused install mints no id row");
        assert!(st.task_queue.is_empty(), "and queues no start");
    }
    assert_eq!(c.get_state(&n), Some(ServiceState::Failed));

    // A REMOVED controller's name may be installed again (a redeploy).
    assert!(c.mark_removed(first));
    let second = c
        .add_service(n.clone(), vec![], Mode::Never, 0)
        .expect("reinstall after removal");
    assert_ne!(second, first);
    assert_eq!(c.get_id(&n), Some(second));
    assert!(
        refused_again(Mode::Never),
        "the reinstalled controller holds it"
    );
}

/// The reinstalled controller keeps the name's recorded dependents, so a
/// dependent installed before the redeploy is still scheduled by it.
#[test]
fn a_reinstall_inherits_the_names_dependents() {
    let c = ServiceContainer::new();
    let dep = ServiceName::parse("w12d.msc.inherit.dependency");
    let user = ServiceName::parse("w12d.msc.inherit.user");
    let old = c.add_service(dep.clone(), vec![], Mode::Never, 0).unwrap();
    c.add_service(user.clone(), vec![dep.clone()], Mode::Never, 0)
        .unwrap();
    assert!(c.mark_removed(old));
    let new = c.add_service(dep.clone(), vec![], Mode::Never, 0).unwrap();
    let st = c.inner.lock().unwrap();
    assert_eq!(
        st.controller(new).map(|ctrl| ctrl.dependents.clone()),
        Some(vec![user.clone()])
    );
}

#[test]
fn an_old_id_never_resolves_to_the_controller_that_took_its_name() {
    const VM: usize = 0x12D0_0D01;
    let _t = Teardown(VM);
    let mut ctx = vm_ctx(VM);
    let name = ServiceName::parse("w12d.msc.redeploy.svc");
    let container = container_for(VM);

    // Deploy, then undeploy (setMode(REMOVE) on the first mirror).
    let old_id = container
        .add_service(name.clone(), vec![], Mode::Never, 0)
        .unwrap();
    let old_mirror = mirror_for(&mut ctx, old_id);
    assert!(native_service_controller_set_mode(
        &mut ctx,
        &[Value::Object(Some(old_mirror)), Value::Int(0)],
    )
    .is_ok());

    // Redeploy under the same name and bring it up.
    let new_id = container
        .add_service(name.clone(), vec![], Mode::Never, 0)
        .unwrap();
    let new_mirror = mirror_for(&mut ctx, new_id);
    let new_service = ctx.fresh_object_ref();
    roots_row(VM, new_id, |r| {
        r.service = Some(new_service);
        r.controller_mirror = Some(new_mirror);
    });
    container.start_without_callback(new_id);
    assert_eq!(container.get_state(&name), Some(ServiceState::Up));
    let rooted = {
        let mut v = vec![addr(new_mirror), addr(new_service)];
        v.sort_unstable();
        v
    };
    assert_eq!(scan(VM), rooted);

    // The old mirror answers its own state, not its successor's.
    assert_eq!(
        java_state(&mut ctx, old_mirror),
        Some(Value::Int(ServiceState::Removed.ordinal()))
    );
    assert_eq!(
        java_state(&mut ctx, new_mirror),
        Some(Value::Int(ServiceState::Up.ordinal()))
    );

    // A stray setMode on the old mirror leaves the new controller alone:
    // ACTIVE does not change its mode, REMOVE does not remove it or
    // retire its row.
    assert!(native_service_controller_set_mode(
        &mut ctx,
        &[Value::Object(Some(old_mirror)), Value::Int(5)],
    )
    .is_ok());
    assert!(native_service_controller_set_mode(
        &mut ctx,
        &[Value::Object(Some(old_mirror)), Value::Int(0)],
    )
    .is_ok());
    assert_eq!(container.get_state(&name), Some(ServiceState::Up));
    assert_eq!(container.get_id(&name), Some(new_id));
    {
        let st = container.inner.lock().unwrap();
        assert_eq!(st.controller(new_id).map(|c| c.mode), Some(Mode::Never));
        assert!(st.controller(old_id).is_none());
        assert_eq!(st.state_of(old_id), Some(ServiceState::Removed));
    }
    assert_eq!(scan(VM), rooted, "the new controller's row survives");

    // Bookkeeping by the old id is a no-op for the new controller.
    container.record_failure(old_id, "stale".into());
    container.mark_async(old_id);
    container.complete_async(old_id);
    assert_eq!(container.get_state(&name), Some(ServiceState::Up));
    assert!(!container.mark_removed(old_id));

    // A listener added through the old mirror gets REMOVED replayed and
    // is not retained on either controller's row.
    let late = ctx.fresh_object_ref();
    assert!(native_service_controller_add_listener(
        &mut ctx,
        &[Value::Object(Some(old_mirror)), Value::Object(Some(late))],
    )
    .is_ok());
    assert!(!scan(VM).contains(&addr(late)));
    assert_eq!(
        ctx.get_field(old_mirror, SC_FIELD_STATE),
        Value::Int(ServiceState::Removed.ordinal()),
        "the old mirror is reflected as removed"
    );

    // The new controller's own mirror still works.
    assert!(native_service_controller_set_mode(
        &mut ctx,
        &[Value::Object(Some(new_mirror)), Value::Int(0)],
    )
    .is_ok());
    assert_eq!(container.get_state(&name), Some(ServiceState::Removed));
    assert!(scan(VM).is_empty());
}
