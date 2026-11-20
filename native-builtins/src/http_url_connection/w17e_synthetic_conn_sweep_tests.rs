use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xE17_0001;
const VM_B: usize = 0xE17_0002;
const VM_C: usize = 0xE17_0003;
const VM_D: usize = 0xE17_0004;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: an identity-hash and weak-owner source only; the mock hashes
    // the address and nothing here dereferences it. 16-byte steps keep it
    // 8-byte aligned (`ObjectRef::from_raw` debug-asserts that).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Clears the test's VMs even when an assertion fails (the rows, then the
/// lock keys they are filed under since gc-common w27-b).
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_http_url_connection_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn exchange(body_len: usize) -> ConnState {
    ConnState {
        status: 200,
        response_body: vec![0x5A; body_len],
        response_headers: vec![("Content-Length".to_string(), body_len.to_string())],
        body_consumed: false,
        truncated: false,
    }
}

fn rows(vm: usize) -> usize {
    CONN_REGISTRY.peek(vm, |r| r.conns.len()).unwrap_or(0)
}

fn body_len(vm: usize, id: i32) -> Option<usize> {
    CONN_REGISTRY
        .peek(vm, |r| r.get(id).map(|s| s.response_body.len()))
        .flatten()
}

/// N synthetic exchanges read without `disconnect()`: once the carriers
/// die, the VM's registry keeps only the live one's row. Another VM's
/// collection judges none of them.
#[test]
fn w17e_dead_synthetic_carriers_leave_no_response_body_behind() {
    let _teardown = Teardown(&[VM_A]);
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let live = at(0xE17_1000);
    let ids: Vec<i32> = (0..100usize)
        .map(|i| register_synthetic_conn(&a, at(0xE17_1000 + i * 16), exchange(4096)))
        .collect();
    assert_eq!(rows(VM_A), 100);

    // A VM that owns no rows here: its collection judges none of A's.
    assert_eq!(crate::gc_sweep_lock_keys(0xE17_00FF, &|_| false), 0);
    assert_eq!(
        rows(VM_A),
        100,
        "another VM's collection judges none of them"
    );

    // gc-common w27-b: the lock-key sweep frees the dead carriers' keys
    // (it counts slots) and drops their rows.
    let dropped = crate::gc_sweep_lock_keys(VM_A, &|x| x == live.as_ptr() as usize);
    assert!(
        dropped >= 99,
        "every dead carrier's key is freed (got {dropped})"
    );
    assert_eq!(rows(VM_A), 1);
    assert_eq!(
        body_len(VM_A, ids[0]),
        Some(4096),
        "the live carrier keeps its body"
    );
    for id in &ids[1..] {
        assert_eq!(body_len(VM_A, *id), None);
    }
    assert_eq!(
        CONN_REGISTRY.peek(VM_A, |r| r.by_owner.len()),
        Some(1),
        "the owner index shrinks with the rows"
    );
}

/// A row `disconnect()` already dropped leaves nothing behind when its
/// carrier dies; one carrier's two exchanges go together with it; and
/// (gc-common w27-b) two live carriers that share an identity hash keep
/// SEPARATE rows, each going with its own carrier.
#[test]
fn w17e_disconnected_and_colliding_carriers() {
    let _teardown = Teardown(&[VM_C]);
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_C);
    let gone = at(0xE17_2000);
    let id = register_synthetic_conn(&c, gone, exchange(16));
    conn_registry_mut(VM_C, |r| r.remove(id));
    assert_eq!(rows(VM_C), 0);
    crate::gc_sweep_lock_keys(VM_C, &|_| false);
    assert_eq!(rows(VM_C), 0);
    assert_eq!(CONN_REGISTRY.peek(VM_C, |r| r.by_owner.len()), Some(0));

    // One carrier that connected twice: both rows under its key, and
    // both go with it.
    let twice = at(0xE17_3000);
    let first = register_synthetic_conn(&c, twice, exchange(16));
    let second = register_synthetic_conn(&c, twice, exchange(32));
    assert_ne!(first, second);
    crate::gc_sweep_lock_keys(VM_C, &|_| true);
    assert_eq!(rows(VM_C), 2);
    crate::gc_sweep_lock_keys(VM_C, &|_| false);
    assert_eq!(rows(VM_C), 0);

    // Two carriers 4 GiB apart share the mock's identity hash: separate
    // keys, and the dead one's row goes while the live one's stays.
    let (c1, c2) = (at(0x1_0E17_4000), at(0x2_0E17_4000));
    assert_eq!(
        c.identity_hash_code(c1),
        c.identity_hash_code(c2),
        "premise: the two carriers share an identity hash"
    );
    let id1 = register_synthetic_conn(&c, c1, exchange(16));
    let id2 = register_synthetic_conn(&c, c2, exchange(32));
    let live = c2.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM_C, &|x| x == live);
    assert_eq!(body_len(VM_C, id1), None, "the dead carrier's row went");
    assert_eq!(
        body_len(VM_C, id2),
        Some(32),
        "the live collider keeps its row"
    );
}

/// A VM's connection rows are invisible to another VM's carrier holding
/// the same id, and teardown drops only the torn-down VM's rows.
#[test]
fn w17e_rows_are_per_vm_and_dropped_at_teardown() {
    let _teardown = Teardown(&[VM_B, VM_D]);
    let mut b = MockNativeContext::new();
    b.set_vm_identity(VM_B);
    let mut d = MockNativeContext::new();
    d.set_vm_identity(VM_D);
    let cb = b.fresh_object_ref();
    let cd = d.fresh_object_ref();
    let id_b = register_synthetic_conn(&b, cb, exchange(8));
    b.set_field(cb, HUC_CONN_ID, Value::Int(id_b));
    // VM D's carrier names the same id without ever having filed it.
    d.set_field(cd, HUC_CONN_ID, Value::Int(id_b));
    assert_eq!(with_state(&b, cb, |s| s.response_body.len()), Some(8));
    assert_eq!(
        with_state(&d, cd, |s| s.response_body.len()),
        None,
        "another VM's row must not answer this VM's carrier"
    );
    let id_d = register_synthetic_conn(&d, cd, exchange(24));
    d.set_field(cd, HUC_CONN_ID, Value::Int(id_d));
    assert_eq!(with_state(&d, cd, |s| s.response_body.len()), Some(24));

    forget_vm_http_url_connection_state(VM_B);
    assert_eq!(rows(VM_B), 0);
    assert!(
        !CONN_REGISTRY.has_row(VM_B),
        "teardown drops the VM's whole registry"
    );
    assert_eq!(with_state(&b, cb, |s| s.response_body.len()), None);
    assert_eq!(
        with_state(&d, cd, |s| s.response_body.len()),
        Some(24),
        "another VM's rows survive the teardown"
    );
}
