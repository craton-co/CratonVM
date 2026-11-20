use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xA13A_0001;
const VM_B: usize = 0xA13A_0002;
const VM_C: usize = 0xA13A_0003;
const VM_D: usize = 0xA13A_0004;
const VM_E: usize = 0xA13A_0005;
const VM_F: usize = 0xA13A_0006;

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

/// What `SSLContext.init` leaves behind for context `sslctx`: the owner
/// note (as the three attaches make it), a `TrustManager` (rooted), an
/// identity, trust roots, a client session store, a cached session
/// (rooted) and the deferred-client-auth marker. Answers the key.
fn init_context(
    c: &mut crate::test_utils::MockNativeContext,
    sslctx: ObjectRef,
    tm: ObjectRef,
    ses: ObjectRef,
) -> u64 {
    let key = ctx_obj_key(c, sslctx).unwrap();
    note_ssl_context_owner(&*c, key, sslctx);
    ctx_trust_managers_table().lock().insert(key, vec![tm]);
    ctx_identity_table()
        .lock()
        .insert(key, ("cert".to_string(), "key".to_string()));
    ctx_trust_roots_table()
        .lock()
        .insert(key, TlsTrustRoots::default());
    let _store = ctx_client_session_store(key);
    client_session_cache()
        .lock()
        .insert((key, "peer.example".to_string(), 443), ses);
    mark_deferred_client_auth(key);
    key
}

/// Whether any of `key`'s rows is still present.
fn has_rows(key: u64) -> bool {
    ctx_trust_managers_table().lock().contains_key(&key)
        || ctx_identity_table().lock().contains_key(&key)
        || ctx_trust_roots_table().lock().contains_key(&key)
        || ctx_client_session_store_table().lock().contains_key(&key)
        || client_session_cache()
            .lock()
            .keys()
            .any(|(k, _, _)| *k == key)
        || deferred_client_auth_contexts().lock().contains(&key)
}

fn roots_of(vm: usize) -> Vec<ObjectRef> {
    let mut roots = Vec::new();
    gc_scan_tls_ctx_trust_manager_roots(vm, &mut roots);
    roots
}

/// The page's core case: the context dies while an engine it made lives.
/// The engine still finds its `TrustManager`s (so its trust check is not
/// silently switched to "no managers"); once the engine dies too, every
/// row goes and the managers stop being roots.
#[test]
fn a_dead_contexts_rows_serve_its_live_engine_then_go_with_it() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = ctx_in(VM_A);
    let (sslctx, tm, ses) = (at(0xA13A_1000), at(0xA13A_1100), at(0xA13A_1200));
    let key = init_context(&mut a, sslctx, tm, ses);
    let engine = at(0xA13A_2000);
    set_engine_trust_ctx_key(&mut a, engine, sslctx).unwrap();
    let id = engine_id_or_alloc(&a, engine);
    assert_eq!(
        with_engine(id, |s| s.trust_managers_ctx_key).flatten(),
        Some(key)
    );

    // Another VM's collection judges none of it.
    gc_sweep_tls_rows(VM_B, &|_| false);
    assert!(has_rows(key));

    // The context died; the engine did not.
    gc_sweep_tls_rows(VM_A, &|x| x == addr(engine));
    assert!(
        has_rows(key),
        "a live engine still reads its context's rows"
    );
    assert_eq!(
        ctx_trust_managers_table().lock().get(&key).cloned(),
        Some(vec![tm]),
        "the engine's post-handshake trust check still finds its managers"
    );
    assert!(roots_of(VM_A).contains(&tm));
    assert!(roots_of(VM_A).contains(&ses));

    // A collection that keeps the engine again changes nothing.
    gc_sweep_tls_rows(VM_A, &|x| x == addr(engine));
    assert!(has_rows(key));

    // The engine dies: the rows go in the same collection.
    let dropped = gc_sweep_tls_rows(VM_A, &|_| false);
    assert!(dropped >= 6, "engine rows + six context rows: {dropped}");
    assert!(!has_rows(key));
    assert!(!roots_of(VM_A).contains(&tm));
    assert!(!roots_of(VM_A).contains(&ses));
    let holders = ctx_key_holders().lock();
    assert_eq!(holders.holder_count(key), 0);
    assert!(!holders.dead.contains_key(&key), "no mark left behind");
}

/// A LIVE context keeps its rows whatever happens to its engines; setting
/// an engine's key twice is one hold, not two.
#[test]
fn a_live_context_keeps_its_rows_after_its_engines_die() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = ctx_in(VM_C);
    let sslctx = at(0xA13B_1000);
    let key = init_context(&mut c, sslctx, at(0xA13B_1100), at(0xA13B_1200));
    for i in 0..8usize {
        set_engine_trust_ctx_key(&mut c, at(0xA13B_2000 + i * 16), sslctx).unwrap();
    }
    assert_eq!(ctx_key_holders().lock().holder_count(key), 8);
    set_engine_trust_ctx_key(&mut c, at(0xA13B_2000), sslctx).unwrap();
    assert_eq!(ctx_key_holders().lock().holder_count(key), 8);

    gc_sweep_tls_rows(VM_C, &|x| x == addr(sslctx));
    assert!(has_rows(key));
    assert_eq!(ctx_key_holders().lock().holder_count(key), 0);

    // A later collection that finds the context dead drops them.
    gc_sweep_tls_rows(VM_C, &|_| false);
    assert!(!has_rows(key));
}

/// The retire criterion: build, use and drop N contexts (their engines
/// too) and the per-VM tables stay bounded.
#[test]
fn a_loop_of_dropped_contexts_leaves_the_tables_bounded() {
    let _teardown = Teardown(&[VM_D]);
    let mut d = ctx_in(VM_D);
    let mut keys = Vec::new();
    for i in 0..400usize {
        let base = 0xA13C_0000 + i * 0x100;
        let sslctx = at(base);
        let key = init_context(&mut d, sslctx, at(base + 0x10), at(base + 0x20));
        set_engine_trust_ctx_key(&mut d, at(base + 0x30), sslctx).unwrap();
        set_engine_trust_ctx_key(&mut d, at(base + 0x40), sslctx).unwrap();
        keys.push(key);
        if i % 50 == 49 {
            // Nothing of this VM survives its collection.
            gc_sweep_tls_rows(VM_D, &|_| false);
        }
    }
    assert!(keys.iter().all(|k| !has_rows(*k)));
    let mine = 0xA13C_0000usize..0xA13C_0000 + 400 * 0x100;
    assert!(!roots_of(VM_D).iter().any(|r| mine.contains(&addr(*r))));
    let holders = ctx_key_holders().lock();
    assert!(keys.iter().all(|k| holders.holder_count(*k) == 0));
    assert!(!holders.dead.values().any(|vm| *vm == VM_D));
    assert!(!holders
        .engine_ctx
        .keys()
        .any(|k| objref_key_owned_by(*k, VM_D)));
}

/// An `HttpsURLConnection` capture leases its context's keys: a request
/// still in flight after `setDefaultSSLSocketFactory` replaced the slot
/// keeps the managers its pass-through verifier defers to, and the rows go
/// once the last clone does.
#[test]
fn an_in_flight_https_capture_holds_a_dead_contexts_rows() {
    let _teardown = Teardown(&[VM_E]);
    let mut e = ctx_in(VM_E);
    let mut sslctx = at(0xA13D_1000);
    let key = init_context(&mut e, sslctx, at(0xA13D_1100), at(0xA13D_1200));
    capture_huc_ssl_context(&mut e, &mut sslctx).unwrap();
    let in_flight = huc_default_tls_in_vm(VM_E).expect("captured");
    assert_eq!(in_flight.tm_ctx_key, Some(key));
    assert_eq!(ctx_key_holders().lock().holder_count(key), 1);

    // The default is replaced by another context's capture.
    let mut other = at(0xA13D_3000);
    let other_key = ctx_obj_key(&mut e, other).unwrap();
    note_ssl_context_owner(&e, other_key, other);
    capture_huc_ssl_context(&mut e, &mut other).unwrap();
    assert_eq!(ctx_key_holders().lock().holder_count(key), 1, "the clone");

    gc_sweep_tls_rows(VM_E, &|x| x == addr(other));
    assert!(has_rows(key), "the in-flight request still reads them");
    drop(in_flight);
    assert_eq!(ctx_key_holders().lock().holder_count(key), 0);
    gc_sweep_tls_rows(VM_E, &|x| x == addr(other));
    assert!(!has_rows(key));
}

/// A socket whose handshake was parked with the key keeps the rows until
/// it is consumed; a new context the lock-key registry hands the dead
/// one's key cancels the release.
#[test]
fn parked_sockets_and_a_revived_key_keep_the_rows() {
    let _teardown = Teardown(&[VM_F]);
    let mut f = ctx_in(VM_F);
    let sslctx = at(0xA13E_1000);
    let key = init_context(&mut f, sslctx, at(0xA13E_1100), at(0xA13E_1200));
    let sock = at(0xA13E_2000);
    crate::phases_late::ssl_security::stash_pending_ssl_socket_connect_ctx(
        &f,
        sock,
        Vec::new(),
        Some(key),
        None,
    );
    // The SOCKET survives this collection (only the context dies): since
    // gc-common w16-d the staged row is weakly owned by its socket, so a
    // sweep that declared the socket dead too would rightly drop the row
    // -- and the parked handshake with it.
    gc_sweep_tls_rows(VM_F, &|x| x == addr(sock));
    assert!(
        has_rows(key),
        "the parked handshake will run these managers"
    );
    let _ = crate::phases_late::ssl_security::take_pending_ssl_socket_connect_ctx(&f, sock);
    gc_sweep_tls_rows(VM_F, &|_| false);
    assert!(!has_rows(key));

    // Revival: the context dies while an engine holds the key, then a
    // new context is noted under the same key before the engine goes.
    let key = init_context(&mut f, sslctx, at(0xA13E_1300), at(0xA13E_1400));
    let engine = at(0xA13E_3000);
    set_engine_trust_ctx_key(&mut f, engine, sslctx).unwrap();
    gc_sweep_tls_rows(VM_F, &|x| x == addr(engine));
    assert!(ctx_key_holders().lock().dead.contains_key(&key));
    let successor = at(0xA13E_4000);
    note_ssl_context_owner(&f, key, successor);
    assert!(!ctx_key_holders().lock().dead.contains_key(&key));
    gc_sweep_tls_rows(VM_F, &|x| x == addr(successor));
    assert!(has_rows(key), "the successor context owns them now");
}
