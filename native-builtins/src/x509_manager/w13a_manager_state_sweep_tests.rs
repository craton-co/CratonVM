use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xA13F_0001;
const VM_B: usize = 0xA13F_0002;
const VM_C: usize = 0xA13F_0003;
const VM_D: usize = 0xA13F_0004;

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::t27_tls::forget_vm_tls_state(vm);
            forget_vm_key_manager_states(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// A collection's two row sweeps, in the order all three GC sites run
/// them: the TLS weak-owner sweep, then the lock-key sweep (which drops
/// the identity rows of the freed keys since gc-common w27-b).
fn sweep(vm: usize, live: &[ObjectRef]) -> usize {
    let live: Vec<usize> = live.iter().map(|o| addr(*o)).collect();
    crate::t27_tls::gc_sweep_tls_rows(vm, &|x| live.contains(&x))
        + crate::gc_sweep_lock_keys(vm, &|x| live.contains(&x))
}

fn tm_present(id: i32) -> bool {
    tm_registry().read().contains_key(&id)
}

fn km_present(id: i32) -> bool {
    km_registry().read().contains_key(&id)
}

fn tm_rows(vm: usize) -> usize {
    tm_id_by_identity()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|row| row.vm == vm)
        .count()
}

/// The factory and two managers carry one id: the state survives any one
/// of them and goes with the last; each dead carrier's fallback identity
/// row goes with it.
#[test]
fn a_trust_state_lives_while_any_carrier_does() {
    let _teardown = Teardown(&[VM_A]);
    let mut a = ctx_in(VM_A);
    let id = register_trust_manager_state_in_vm(VM_A, TrustManagerState::default());
    let (factory, tm1, tm2) = (
        a.fresh_object_ref(),
        a.fresh_object_ref(),
        a.fresh_object_ref(),
    );
    for o in [factory, tm1, tm2] {
        set_tm_id(&mut a, o, id);
    }
    assert_eq!(get_tm_id(&mut a, tm2), id);

    sweep(VM_A, &[factory, tm2]);
    assert!(tm_present(id));
    sweep(VM_A, &[tm2]);
    assert!(tm_present(id), "one live TrustManager keeps its anchors");
    assert_eq!(tm_rows(VM_A), 1, "only the live carrier's row is left");

    sweep(VM_A, &[]);
    assert!(!tm_present(id));
    assert_eq!(tm_rows(VM_A), 0);
}

/// A factory re-`init`-ed onto a new state stops carrying the old one,
/// which goes at the next collection while the new one stays.
#[test]
fn a_restamped_factory_releases_its_old_state() {
    let _teardown = Teardown(&[VM_B]);
    let mut b = ctx_in(VM_B);
    let old = register_trust_manager_state_in_vm(VM_B, TrustManagerState::default());
    let new = register_trust_manager_state_in_vm(VM_B, TrustManagerState::default());
    let factory = b.fresh_object_ref();
    set_tm_id(&mut b, factory, old);
    set_tm_id(&mut b, factory, new);
    sweep(VM_B, &[factory]);
    assert!(!tm_present(old), "no carrier left for the old state");
    assert!(tm_present(new));
}

/// A `phases_late::ssl_security` factory row keeps its state after every
/// manager it handed out died; the factory's own death drops the row, and
/// the state goes at the next collection (gc-common w27-a).
#[test]
fn a_factory_row_holds_its_state_until_the_factory_dies() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = ctx_in(VM_C);
    let id = register_trust_manager_state_in_vm(VM_C, TrustManagerState::default());
    let factory = c.fresh_object_ref();
    // gc-common w27-a: factory rows are keyed (VM, weak lock key) and go
    // with the lock-key sweep, which runs after the TLS sweep.
    let fkey = (
        VM_C,
        crate::gc_stable_weak_lock_key(&c, factory).unwrap() as u64,
    );
    crate::phases_late::ssl_security::tmf_tm_id_by_identity()
        .lock()
        .insert(fkey, id);
    // `getTrustManagers()`'s shape: a fresh manager stamped with the id.
    let tm = c.fresh_object_ref();
    set_tm_id(&mut c, tm, id);

    sweep(VM_C, &[factory]);
    assert!(tm_present(id), "the factory can still hand the state out");
    // gc-common w27-a: `sweep` runs the lock-key sweep after the TLS one,
    // so the factory's key is freed after this collection's TLS sweep; the
    // row's state is offered then and freed at the next collection.
    sweep(VM_C, &[]);
    assert!(tm_present(id), "never earlier than its last holder's death");
    sweep(VM_C, &[]);
    assert!(!tm_present(id));
    assert!(!crate::phases_late::ssl_security::tmf_tm_id_by_identity()
        .lock()
        .contains_key(&fkey));
}

/// A `PrivateKey` proxy carries its km id: the state -- and the live key
/// it roots -- outlives the `KeyManager`, and stops being a root once
/// the proxy dies too.
#[test]
fn a_private_key_proxy_keeps_its_key_manager_state() {
    let _teardown = Teardown(&[VM_D]);
    let mut d = ctx_in(VM_D);
    let live_key = d.fresh_object_ref();
    let mut state = KeyManagerState {
        vm: VM_D,
        ..Default::default()
    };
    state.aliases_to_live_key.insert("k".to_string(), live_key);
    let id = next_km_id();
    km_registry().write().insert(id, state);
    let km = d.fresh_object_ref();
    set_km_id(&mut d, km, id);
    let pk = d.fresh_object_ref();
    crate::t27_tls::note_manager_id_carrier(
        &d,
        crate::t27_tls::TlsOwnerKind::KeyManagerState,
        id,
        pk,
    );
    let roots = || {
        let mut roots = Vec::new();
        gc_scan_key_manager_roots(VM_D, &mut roots);
        roots
    };

    sweep(VM_D, &[pk]);
    assert!(km_present(id), "the proxy still decodes this id");
    assert!(roots().contains(&live_key));
    sweep(VM_D, &[]);
    assert!(!km_present(id));
    assert!(!roots().contains(&live_key));
}
