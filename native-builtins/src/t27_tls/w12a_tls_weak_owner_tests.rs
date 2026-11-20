use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xA12A_0001;
const VM_B: usize = 0xA12A_0002;
const VM_C: usize = 0xA12A_0003;
const VM_D: usize = 0xA12A_0004;
const VM_E: usize = 0xA12A_0005;
const VM_F: usize = 0xA12A_0006;
const VM_G: usize = 0xA12A_0007;
const VM_H: usize = 0xA12A_0008;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key / address only, never dereferenced (the mock hashes
    // the address).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// Clears the test's VMs even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn owner_rows(vm: usize) -> usize {
    tls_weak_owners()
        .lock()
        .values()
        .filter(|row| row.vm == vm)
        .count()
}

fn roots_of(vm: usize) -> Vec<ObjectRef> {
    let mut roots = Vec::new();
    gc_scan_tls_ctx_trust_manager_roots(vm, &mut roots);
    roots
}

/// The engine's death drops its whole row family -- rustls state,
/// sessions, selector, matchers -- and a live engine keeps its own.
/// Another VM's sweep judges none of them.
#[test]
fn a_dead_engine_loses_every_row_and_a_live_one_keeps_them() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let a = ctx_in(VM_A);
    let (dead, live) = (at(0xA12A_1000), at(0xA12A_2000));
    let (id_dead, id_live) = (engine_id_or_alloc(&a, dead), engine_id_or_alloc(&a, live));
    let (k_dead, k_live) = (engine_objref_key(&a, dead), engine_objref_key(&a, live));
    for (k, base) in [(k_dead, 0xA12A_1100usize), (k_live, 0xA12A_2100)] {
        engine_session_table()
            .lock()
            .insert((k, SessionPhase::Negotiated), at(base));
        engine_alpn_selector_table()
            .lock()
            .insert(k, at(base + 0x10));
        engine_sni_matchers_table()
            .lock()
            .insert(k, vec![at(base + 0x20)]);
    }
    assert_eq!(owner_rows(VM_A), 2);

    // VM B's collection says everything is dead: none of it is B's.
    assert_eq!(gc_sweep_tls_rows(VM_B, &|_| false), 0);
    assert!(engine_registry().read().contains_key(&id_dead));

    // VM A kept only `live`.
    let dropped = gc_sweep_tls_rows(VM_A, &|x| x == addr(live));
    assert!(
        dropped >= 5,
        "table + registry + session + selector + matchers: {dropped}"
    );
    assert!(!engine_registry().read().contains_key(&id_dead));
    assert!(!engine_table().lock().contains_key(&k_dead));
    assert!(!engine_session_table()
        .lock()
        .keys()
        .any(|(k, _)| *k == k_dead));
    assert!(!engine_alpn_selector_table().lock().contains_key(&k_dead));
    assert!(!engine_sni_matchers_table().lock().contains_key(&k_dead));
    assert!(engine_registry().read().contains_key(&id_live));
    assert!(engine_session_table()
        .lock()
        .contains_key(&(k_live, SessionPhase::Negotiated)));
    assert!(engine_alpn_selector_table().lock().contains_key(&k_live));
    assert!(engine_sni_matchers_table().lock().contains_key(&k_live));
    assert_eq!(owner_rows(VM_A), 1);
    // The dead engine's session is no longer a root; the live one's is.
    let roots = roots_of(VM_A);
    assert!(!roots.contains(&at(0xA12A_1100)));
    assert!(roots.contains(&at(0xA12A_2100)));
}

/// `common-w10e-sslengine-state-is-never-evicted`'s retire criterion: a
/// loop of 10 000 engines that die leaves the registry bounded.
#[test]
fn ten_thousand_dead_engines_leave_the_registry_bounded() {
    let _teardown = Teardown(&[VM_C]);
    let c = ctx_in(VM_C);
    let mut ids = Vec::with_capacity(10_000);
    for i in 0..10_000usize {
        let engine = at(0xA12B_0000 + i * 16);
        ids.push(engine_id_or_alloc(&c, engine));
        if i % 1000 == 999 {
            // Nothing survives this VM's collection.
            gc_sweep_tls_rows(VM_C, &|_| false);
        }
    }
    {
        let registry = engine_registry().read();
        assert!(ids.iter().all(|id| !registry.contains_key(id)));
    }
    assert!(!engine_table()
        .lock()
        .keys()
        .any(|k| objref_key_owned_by(*k, VM_C)));
    assert_eq!(owner_rows(VM_C), 0);
}

/// A moved engine is re-addressed with the `tls` row, not dropped.
#[test]
fn a_moved_engine_is_followed_not_dropped() {
    let _teardown = Teardown(&[VM_D]);
    let d = ctx_in(VM_D);
    let (old, new) = (at(0xA12C_1000), at(0xA12C_2000));
    let id = engine_id_or_alloc(&d, old);
    // Inside the epilogue: the pre-remap address is in the pointer map.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(addr(old), addr(new));
    gc_sweep_tls_rows(VM_D, &|x| map.contains_key(&x));
    assert!(engine_registry().read().contains_key(&id));
    gc_update_tls_ctx_trust_manager_refs(VM_D, &map);
    // The next collection finds it at its new address.
    gc_sweep_tls_rows(VM_D, &|x| x == addr(new));
    assert!(engine_registry().read().contains_key(&id));
    gc_sweep_tls_rows(VM_D, &|_| false);
    assert!(!engine_registry().read().contains_key(&id));
}

/// A consumed selector (or a closed engine's) is not a root, is still
/// answered while alive, and goes when it dies; a re-installed selector
/// is a root again.
#[test]
fn a_consumed_selector_is_not_a_root_and_goes_when_it_dies() {
    let _teardown = Teardown(&[VM_E]);
    let e = ctx_in(VM_E);
    let engine = at(0xA12D_1000);
    let (sel, sel2) = (at(0xA12D_2000), at(0xA12D_3000));
    let _id = engine_id_or_alloc(&e, engine);
    let key = engine_objref_key(&e, engine);
    engine_alpn_selector_table().lock().insert(key, sel);
    assert!(roots_of(VM_E).contains(&sel), "unconsumed: rooted");

    alpn_selector_release_root(key);
    assert!(!alpn_selector_is_rooted(key));
    assert!(!roots_of(VM_E).contains(&sel), "consumed: not a root");
    // Alive (held elsewhere): kept.
    gc_sweep_tls_rows(VM_E, &|x| x == addr(engine) || x == addr(sel));
    assert_eq!(
        engine_alpn_selector_table().lock().get(&key).copied(),
        Some(sel)
    );
    // Died: its row and its mark go; the engine's own rows stay.
    assert_eq!(gc_sweep_tls_rows(VM_E, &|x| x == addr(engine)), 1);
    assert!(!engine_alpn_selector_table().lock().contains_key(&key));
    assert!(alpn_selector_is_rooted(key), "no stale mark left");
    assert_eq!(owner_rows(VM_E), 1);

    // The setter's shape: a new selector is rooted until consumed.
    engine_alpn_selector_table().lock().insert(key, sel2);
    alpn_selector_unrooted().lock().remove(&key);
    assert!(roots_of(VM_E).contains(&sel2));
    // A closed engine never consults it: released; then a dead engine
    // takes its selector and the mark with it.
    alpn_selector_release_root(key);
    assert!(!roots_of(VM_E).contains(&sel2));
    gc_sweep_tls_rows(VM_E, &|x| x == addr(sel2));
    assert!(!engine_alpn_selector_table().lock().contains_key(&key));
    assert!(!alpn_selector_unrooted().lock().contains(&key));
}

/// `common-w11a-sslparameters-alpn-side-table-grows-and-serves-reused-
/// hashes`: one row per `getSSLParameters()` no longer outlives its
/// object, and a later object with the dead one's hash reads no list.
#[test]
fn sslparameters_rows_are_bounded_by_their_live_objects() {
    let _teardown = Teardown(&[VM_F]);
    let f = ctx_in(VM_F);
    let live = at(0xA12E_0000);
    for i in 0..1000usize {
        let p = at(0xA12E_0000 + i * 16);
        let key = engine_objref_key(&f, p);
        sslparams_alpn_table()
            .lock()
            .insert(key, vec!["h2".to_string(), "http/1.1".to_string()]);
        note_tls_weak_owner(&f, TlsOwnerKind::SslParams, key, p);
    }
    gc_sweep_tls_rows(VM_F, &|x| x == addr(live));
    let rows = sslparams_alpn_table()
        .lock()
        .keys()
        .filter(|k| objref_key_owned_by(**k, VM_F))
        .count();
    assert_eq!(rows, 1, "only the live parameters object keeps its list");
    // A fresh object that inherits a dead one's hash (the mock's hash is
    // the address) finds nothing: `getApplicationProtocols()` answers the
    // empty array.
    let reused = at(0xA12E_0000 + 7 * 16);
    let key = engine_objref_key(&f, reused);
    assert!(sslparams_alpn_table().lock().get(&key).is_none());
}

/// A session's byte-only rows go with the session; `forget_session_rows`
/// (every session constructor's first call) is what records the owner.
#[test]
fn session_rows_go_with_their_session() {
    let _teardown = Teardown(&[VM_G]);
    let g = ctx_in(VM_G);
    let (dead, live) = (at(0xA12F_1000), at(0xA12F_2000));
    for s in [dead, live] {
        forget_session_rows(&g, s);
        let key = gc_stable_objref_key(&g, s);
        session_peer_certs_table()
            .lock()
            .insert(key, vec![vec![1, 2, 3]]);
        negotiated_session_keys().lock().insert(key);
    }
    let (k_dead, k_live) = (
        gc_stable_objref_key(&g, dead),
        gc_stable_objref_key(&g, live),
    );
    assert_eq!(gc_sweep_tls_rows(VM_G, &|x| x == addr(live)), 2);
    assert!(!session_peer_certs_table().lock().contains_key(&k_dead));
    assert!(!negotiated_session_keys().lock().contains(&k_dead));
    assert!(session_peer_certs_table().lock().contains_key(&k_live));
    assert!(negotiated_session_keys().lock().contains(&k_live));
}

/// The session cache is bounded oldest-first, and an invalidated id does
/// not count against the bound.
#[test]
fn the_session_cache_is_bounded_oldest_first() {
    let _teardown = Teardown(&[VM_H]);
    let id = |n: u8| vec![n; 32];
    for n in 1..=4u8 {
        let ses = at(0xA130_0000 + n as usize * 16);
        session_id_registry_insert_bounded(VM_H, id(n), ses, 3);
    }
    assert_eq!(session_by_id(VM_H, &id(1)), None, "oldest evicted");
    for n in 2..=4u8 {
        assert!(session_by_id(VM_H, &id(n)).is_some(), "id {n} kept");
    }
    session_id_registry_remove(VM_H, id(3));
    session_id_registry_insert_bounded(VM_H, id(5), at(0xA130_0050), 3);
    assert!(
        session_by_id(VM_H, &id(2)).is_some(),
        "3 live rows: no eviction"
    );
    session_id_registry_insert_bounded(VM_H, id(6), at(0xA130_0060), 3);
    assert_eq!(session_by_id(VM_H, &id(2)), None);
    for n in [4u8, 5, 6] {
        assert!(session_by_id(VM_H, &id(n)).is_some(), "id {n} kept");
    }
    assert_eq!(session_id_registry_ids(VM_H).len(), 3);
    assert_eq!(
        SESSION_ID_REGISTRY_PER_VM, 20480,
        "JSSE's default cache size"
    );
}
