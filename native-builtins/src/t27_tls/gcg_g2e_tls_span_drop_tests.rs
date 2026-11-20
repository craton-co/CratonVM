use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xE2E0_0001;
const VM_B: usize = 0xE2E0_0002;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key / address only, never dereferenced (the mock hashes
    // the address).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

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

/// An engine whose address lies in a freed span loses its rows; one
/// outside every span keeps them; another VM's span drop judges neither.
#[test]
fn gcg_g2e_a_span_drop_forgets_only_the_engines_inside_a_freed_span() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let a = ctx_in(VM_A);
    let (dead, live) = (at(0xE2E0_1000), at(0xE2E0_2000));
    let (id_dead, id_live) = (engine_id_or_alloc(&a, dead), engine_id_or_alloc(&a, live));
    let k_dead = engine_objref_key(&a, dead);
    engine_alpn_selector_table()
        .lock()
        .insert(k_dead, at(0xE2E0_1110));
    assert_eq!(owner_rows(VM_A), 2);
    let spans = [(0xE2E0_0F00usize, 0x200usize), (0xE2E0_3000, 0x100)];

    assert_eq!(
        gc_drop_tls_rows_in_spans(VM_B, &spans),
        0,
        "not VM B's rows"
    );
    assert_eq!(
        gc_drop_tls_rows_in_spans(VM_A, &[]),
        0,
        "no spans, nothing judged"
    );
    assert_eq!(owner_rows(VM_A), 2);

    let dropped = gc_drop_tls_rows_in_spans(VM_A, &spans);
    assert!(dropped >= 2, "table + registry (+ selector): {dropped}");
    assert!(!engine_registry().read().contains_key(&id_dead));
    assert!(!engine_alpn_selector_table().lock().contains_key(&k_dead));
    assert!(engine_registry().read().contains_key(&id_live));
    assert_eq!(owner_rows(VM_A), 1);
    // Idempotent.
    assert_eq!(gc_drop_tls_rows_in_spans(VM_A, &spans), 0);
}
