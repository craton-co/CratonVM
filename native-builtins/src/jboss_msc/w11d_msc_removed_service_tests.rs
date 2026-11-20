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

/// A controller mirror bound to a fresh controller of `mode` in `vm`'s
/// container.
fn install(ctx: &mut MockNativeContext, vm: usize, name: &str, mode: Mode) -> (u64, ObjectRef) {
    let id = container_for(vm)
        .add_service(ServiceName::parse(name), vec![], mode, 0)
        .expect("install");
    let mirror = ctx.fresh_object_ref();
    ctx.set_field(mirror, SC_FIELD_ID, Value::Long(id as i64));
    (id, mirror)
}

#[test]
fn a_removed_service_is_no_longer_rooted_and_its_mirror_keeps_it() {
    const VM: usize = 0x11D0_0B01;
    let _t = Teardown(VM);
    let mut ctx = vm_ctx(VM);
    let name = "w11d.msc.removed.svc";
    let (id, mirror) = install(&mut ctx, VM, name, Mode::Never);
    let service = ctx.fresh_object_ref();
    let target = ctx.fresh_object_ref();
    let start_ctx = ctx.fresh_object_ref();
    let listener = ctx.fresh_object_ref();
    let (reg, inj) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    roots_row(VM, id, |r| {
        r.service = Some(service);
        r.controller_mirror = Some(mirror);
        r.child_target = Some(target);
        r.start_context = Some(start_ctx);
        r.listeners.push(listener);
        r.dep_injections.push((reg, inj));
    });
    assert_eq!(scan(VM).len(), 7, "a live controller roots all seven");

    // A listener added to a LIVE controller is still retained.
    let live_listener = ctx.fresh_object_ref();
    assert!(native_service_controller_add_listener(
        &mut ctx,
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(live_listener))
        ],
    )
    .is_ok());
    assert!(scan(VM).contains(&addr(live_listener)));

    // setMode(REMOVE).
    assert!(native_service_controller_set_mode(
        &mut ctx,
        &[Value::Object(Some(mirror)), Value::Int(0)],
    )
    .is_ok());
    assert_eq!(
        container_for(VM).get_state(&ServiceName::parse(name)),
        Some(ServiceState::Removed)
    );
    assert!(
        scan(VM).is_empty(),
        "a removed controller must root nothing (its Service pins its deployment's loader)"
    );

    // The mirror holds its service now, as a real controller does.
    let got = native_service_controller_get_service(&mut ctx, &[Value::Object(Some(mirror))]);
    assert_eq!(got.ok().flatten(), Some(Value::Object(Some(service))));

    // A late listener gets its replay and is not retained.
    let late = ctx.fresh_object_ref();
    assert!(native_service_controller_add_listener(
        &mut ctx,
        &[Value::Object(Some(mirror)), Value::Object(Some(late))],
    )
    .is_ok());
    assert!(
        scan(VM).is_empty(),
        "a listener on a removed controller is not a root"
    );

    // A second REMOVE is harmless.
    assert!(native_service_controller_set_mode(
        &mut ctx,
        &[Value::Object(Some(mirror)), Value::Int(0)],
    )
    .is_ok());
    assert!(scan(VM).is_empty());
}

/// A live controller's mirror never answers the parked slot: it is null
/// until removal, and a foreign receiver (an id this VM never minted) is
/// never read.
#[test]
fn a_live_or_foreign_mirror_has_no_parked_service() {
    const VM: usize = 0x11D0_0B02;
    let _t = Teardown(VM);
    let mut ctx = vm_ctx(VM);
    let (id, mirror) = install(&mut ctx, VM, "w11d.msc.live.svc", Mode::Never);
    assert_eq!(parked_service(&ctx, mirror, id), None);

    let foreign = ctx.fresh_object_ref();
    let stray = ctx.fresh_object_ref();
    ctx.set_field(foreign, SC_FIELD_VALUE, Value::Object(Some(stray)));
    assert_eq!(parked_service(&ctx, foreign, u64::MAX - 0x11D0), None);
}

#[test]
fn a_finished_start_releases_its_start_context() {
    const VM: usize = 0x11D0_0B03;
    let _t = Teardown(VM);
    let mut ctx = vm_ctx(VM);
    // `Never`, so the drive loop `complete()` runs picks neither up.
    let (done_id, done_mirror) = install(&mut ctx, VM, "w11d.msc.async.done", Mode::Never);
    let (failed_id, failed_mirror) =
        install(&mut ctx, VM, "w11d.msc.async.failed", Mode::Never);
    let done_ctx = ctx.fresh_object_ref();
    let failed_ctx = ctx.fresh_object_ref();
    for (id, mirror, start_ctx) in [
        (done_id, done_mirror, done_ctx),
        (failed_id, failed_mirror, failed_ctx),
    ] {
        ctx.set_field(start_ctx, CTX_FIELD_CONTROLLER_ID, Value::Long(id as i64));
        roots_row(VM, id, |r| {
            r.controller_mirror = Some(mirror);
            r.start_context = Some(start_ctx);
        });
    }
    assert!(native_start_context_complete(&mut ctx, &[Value::Object(Some(done_ctx))]).is_ok());
    assert!(native_start_context_failed(&mut ctx, &[Value::Object(Some(failed_ctx))]).is_ok());
    let mut expected = vec![addr(done_mirror), addr(failed_mirror)];
    expected.sort_unstable();
    assert_eq!(
        scan(VM),
        expected,
        "only the mirrors stay rooted once complete() / failed() ended the start"
    );
}
