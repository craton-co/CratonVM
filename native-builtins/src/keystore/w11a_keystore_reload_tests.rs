use super::*;

const VM_A: usize = 0xA11C_0001;
const VM_B: usize = 0xA11C_0002;

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_keystores(vm);
        }
    }
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

#[test]
fn a_reload_reuses_its_own_vms_store_id_only() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let id = keystore_register_in_vm(VM_A, store_with("first"));

    // Another VM, a VM-less id and id 0 are refused, and hand the store back.
    let back = keystore_replace_in_vm(VM_B, id, store_with("b"));
    assert!(back.is_err(), "VM B must not overwrite VM A's store");
    assert!(keystore_replace_in_vm(VM_A, 0, store_with("zero")).is_err());
    let unowned = keystore_register(store_with("unowned"));
    assert!(keystore_replace_in_vm(VM_A, unowned, store_with("x")).is_err());
    {
        let mut g = registry().write();
        g.stores.remove(&unowned);
    }

    // VM A's own reload replaces the contents under the same id.
    assert!(keystore_replace_in_vm(VM_A, id, store_with("second")).is_ok());
    let store = keystore_lookup(id).expect("still registered");
    assert!(store.entries.contains_key("second"));
    assert!(!store.entries.contains_key("first"));

    // And the store still goes with its VM.
    forget_vm_keystores(VM_A);
    assert!(keystore_lookup(id).is_none());
}
