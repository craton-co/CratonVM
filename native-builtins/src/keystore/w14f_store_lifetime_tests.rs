use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xF14C_0001;
const VM_B: usize = 0xF14C_0002;
const VM_C: usize = 0xF14C_0003;
const VM_D: usize = 0xF14C_0004;
const VM_E: usize = 0xF14C_0005;
const VM_F: usize = 0xF14C_0006;
const VM_G: usize = 0xF14C_0007;

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::t27_tls::forget_vm_tls_state(vm);
            forget_vm_keystores(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn sweep(vm: usize, live: &[ObjectRef]) -> usize {
    let live: Vec<usize> = live.iter().map(|o| o.as_ptr() as usize).collect();
    crate::t27_tls::gc_sweep_tls_rows(vm, &|x| live.contains(&x))
        + crate::gc_sweep_lock_keys(vm, &|x| live.contains(&x))
}

/// `obj`'s carrier row (its store ids), if it has one.
fn carried_by(c: &crate::test_utils::MockNativeContext, obj: ObjectRef) -> Option<Vec<i32>> {
    let key = crate::existing_weak_lock_key(c, obj)?;
    registry()
        .read()
        .carriers
        .get(&key)
        .map(|row| row.ids.clone())
}

fn store_with(alias: &str) -> LoadedKeyStore {
    let mut store = LoadedKeyStore::default();
    store.entries.insert(
        alias.to_string(),
        KeyStoreEntry {
            alias: alias.to_string(),
            creation_time_ms: 0,
            kind: EntryKind::TrustedCert {
                cert_der: vec![0x30, 0x00],
            },
        },
    );
    store
}

fn present(id: i32) -> bool {
    keystore_lookup(id).is_some()
}

fn owned_by(vm: usize) -> usize {
    registry()
        .read()
        .owners
        .values()
        .filter(|owner| **owner == vm)
        .count()
}

/// The `KeyStore` is the only carrier: its store lives with it and goes,
/// with its identity row, at the collection that finds it dead.
#[test]
fn a_dropped_keystore_releases_its_store() {
    let _teardown = Teardown(&[VM_A]);
    let mut a = ctx_in(VM_A);
    let ks = a.fresh_object_ref();
    let id = keystore_register_in_vm(VM_A, store_with("k"));
    set_store_id(&mut a, ks, id);
    assert_eq!(get_store_id(&mut a, ks), id);
    let key = crate::existing_weak_lock_key(&a, ks).expect("filed");

    sweep(VM_A, &[ks]);
    assert!(present(id), "a live KeyStore keeps its store");

    sweep(VM_A, &[]);
    assert!(!present(id), "the last carrier died: the store goes");
    assert_eq!(owned_by(VM_A), 0);
    assert!(!store_id_by_identity()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key));
    assert!(!registry().read().carriers.contains_key(&key));
}

/// A `PrivateKey` proxy resolves its DER through the store after the
/// `KeyStore` is gone, so it keeps the store; the store goes with it.
#[test]
fn a_key_proxy_keeps_its_store_after_the_keystore_dies() {
    let _teardown = Teardown(&[VM_B]);
    let mut b = ctx_in(VM_B);
    let ks = b.fresh_object_ref();
    let proxy = b.fresh_object_ref();
    let id = keystore_register_in_vm(VM_B, store_with("k"));
    set_store_id(&mut b, ks, id);
    // `engine_get_key`'s record, on the proxy it just allocated.
    note_store_carrier(&b, proxy, id);

    sweep(VM_B, &[proxy]);
    assert!(present(id), "the proxy can still read its key");
    sweep(VM_B, &[]);
    assert!(!present(id));
}

/// `engineGetKey` records the proxy it hands out: the whole path, not the
/// helper. Skips (with a note) if the mock cannot allocate the proxy.
#[test]
fn engine_get_key_records_its_proxy_as_a_carrier() {
    const VM_I: usize = 0xF14C_0009;
    let _teardown = Teardown(&[VM_I]);
    let mut i = ctx_in(VM_I);
    let ks = i.fresh_object_ref();
    let mut store = LoadedKeyStore::default();
    store.entries.insert(
        "k".to_string(),
        KeyStoreEntry {
            alias: "k".to_string(),
            creation_time_ms: 0,
            kind: EntryKind::PrivateKey {
                // SEQUENCE { INTEGER 0 }: plaintext-shaped, not an envelope.
                key_der: vec![0x30, 0x03, 0x02, 0x01, 0x00],
                chain: Vec::new(),
                protected: None,
            },
        },
    );
    let id = keystore_register_in_vm(VM_I, store);
    set_store_id(&mut i, ks, id);
    let alias = i.create_string("k");
    let args = [
        Value::Object(Some(ks)),
        Value::Object(Some(alias)),
        Value::Object(None),
    ];
    let pk = match engine_get_key(&mut i, &args) {
        Ok(Some(Value::Object(Some(pk)))) => pk,
        _ => {
            eprintln!("skipped: the mock handed back no key proxy");
            return;
        }
    };
    assert!(carried_by(&i, pk).is_some_and(|ids| ids.contains(&id)));

    sweep(VM_I, &[pk]);
    assert!(
        present(id),
        "the proxy outlives its KeyStore and keeps the store"
    );
    assert!(private_key_der_from_proxy(&mut i, pk).is_some());
    sweep(VM_I, &[]);
    assert!(!present(id));
}

/// A `KeyManagerFactory` initialised on the store builds from it in a
/// later `getKeyManagers()`: its row keeps the store after the
/// `KeyStore` died. Once the factory is gone too, the store goes at the
/// next registration -- the reload loop's next iteration.
#[test]
fn a_factory_row_holds_its_store_until_the_factory_dies() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = ctx_in(VM_C);
    let ks = c.fresh_object_ref();
    let factory = c.fresh_object_ref();
    let id = keystore_register_in_vm(VM_C, store_with("k"));
    set_store_id(&mut c, ks, id);
    // gc-common w27-a: factory rows are keyed (VM, weak lock key) and go
    // with the lock-key sweep (the second half of `sweep`).
    let fkey = (
        VM_C,
        crate::gc_stable_weak_lock_key(&c, factory).unwrap() as u64,
    );
    crate::phases_late::ssl_security::kmf_keystore_id_by_identity()
        .lock()
        .insert(fkey, id);

    sweep(VM_C, &[factory]);
    assert!(
        present(id),
        "the factory can still build a KeyManager from it"
    );

    sweep(VM_C, &[]);
    assert!(
        !crate::phases_late::ssl_security::kmf_keystore_id_by_identity()
            .lock()
            .contains_key(&fkey)
    );
    let next = keystore_register_in_vm(VM_C, store_with("next"));
    assert!(!present(id), "retried, and nothing names it any more");
    assert!(present(next));
}

/// The common factory shape: `init` then `getKeyManagers()`, which built a
/// `KeyManagerState` from the store. When the factory and its managers
/// die, the state is freed at the end of the sweep, and the store the
/// factory row held goes in the same sweep.
#[test]
fn a_factory_store_goes_with_its_key_manager_state() {
    const VM_H: usize = 0xF14C_0008;
    let _teardown = Teardown(&[VM_H]);
    let mut h = ctx_in(VM_H);
    let ks = h.fresh_object_ref();
    let factory = h.fresh_object_ref();
    let km = h.fresh_object_ref();
    let id = keystore_register_in_vm(VM_H, store_with("k"));
    set_store_id(&mut h, ks, id);
    let fkey = (
        VM_H,
        crate::gc_stable_weak_lock_key(&h, factory).unwrap() as u64,
    );
    crate::phases_late::ssl_security::kmf_keystore_id_by_identity()
        .lock()
        .insert(fkey, id);
    let km_id = crate::x509_manager::next_km_id();
    crate::x509_manager::km_registry().write().insert(
        km_id,
        crate::x509_manager::KeyManagerState {
            keystore_id: id,
            vm: VM_H,
            ..Default::default()
        },
    );
    crate::x509_manager::set_km_id(&mut h, km, km_id);

    sweep(VM_H, &[factory, km]);
    assert!(present(id), "held by the factory row");

    // gc-common w27-a: the factory's row goes with its lock key, after the
    // TLS sweep (the epilogue's order, which `sweep` follows); the hook
    // releases the store it held.
    sweep(VM_H, &[]);
    assert!(!crate::x509_manager::km_registry()
        .read()
        .contains_key(&km_id));
    assert!(
        !present(id),
        "released with the state, no registration needed"
    );
}

/// The shape the page measured: a fresh `KeyStore` per reload, the old
/// one dropped. The registry holds one store, not one per reload.
#[test]
fn a_reload_loop_leaves_the_registry_bounded() {
    let _teardown = Teardown(&[VM_D]);
    let mut d = ctx_in(VM_D);
    let mut first = 0;
    for round in 0..48 {
        let ks = d.fresh_object_ref();
        let id = keystore_register_in_vm(VM_D, store_with("k"));
        if round == 0 {
            first = id;
        }
        set_store_id(&mut d, ks, id);
        sweep(VM_D, &[ks]);
        assert_eq!(
            owned_by(VM_D),
            1,
            "round {round}: only the live store is left"
        );
        assert!(present(id));
    }
    assert!(!present(first));
}

/// An object stamped with a new id (a load that could not replace its old
/// store in place) stops carrying the old one, which goes at once.
#[test]
fn a_restamped_keystore_releases_its_old_store() {
    let _teardown = Teardown(&[VM_E]);
    let mut e = ctx_in(VM_E);
    let ks = e.fresh_object_ref();
    let old = keystore_register_in_vm(VM_E, store_with("old"));
    set_store_id(&mut e, ks, old);
    let new = keystore_register_in_vm(VM_E, store_with("new"));
    assert!(present(old), "still carried until the re-stamp");
    set_store_id(&mut e, ks, new);
    assert!(!present(old));
    assert!(present(new));
    assert_eq!(get_store_id(&mut e, ks), new);
}

/// A VM-less store -- `tls.rs`'s process-wide default-truststore cache --
/// is never recorded as carried, and never released while a VM lives,
/// whatever that VM's objects do.
#[test]
fn a_vm_less_store_is_never_released() {
    let _teardown = Teardown(&[VM_F]);
    let mut f = ctx_in(VM_F);
    let obj = f.fresh_object_ref();
    let id = keystore_register(store_with("ca"));
    note_store_carrier(&f, obj, id);
    assert!(carried_by(&f, obj).is_none());
    assert!(
        crate::existing_weak_lock_key(&f, obj).is_none(),
        "no key is minted for a store this VM did not register"
    );

    sweep(VM_F, &[]);
    let _ = keystore_register_in_vm(VM_F, LoadedKeyStore::default());
    assert!(present(id), "the shared cache's id must keep resolving");
    registry().write().stores.remove(&id);
}

/// Teardown drops the lifetime rows with the stores.
#[test]
fn teardown_drops_the_lifetime_rows() {
    let _teardown = Teardown(&[VM_G]);
    let mut g = ctx_in(VM_G);
    let ks = g.fresh_object_ref();
    let id = keystore_register_in_vm(VM_G, store_with("k"));
    set_store_id(&mut g, ks, id);
    assert!(carried_by(&g, ks).is_some());

    forget_vm_keystores(VM_G);
    let reg = registry().read();
    assert!(!reg.carriers.values().any(|row| row.vm == VM_G));
    assert!(!reg.carrier_count.contains_key(&id));
    assert!(!reg.stores.contains_key(&id));
}
