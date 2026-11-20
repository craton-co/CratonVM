use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xE10B_0001;
const VM_B: usize = 0xE10B_0002;
const VM_C: usize = 0xE10B_0003;

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_http_url_connection_state(vm);
        }
    }
}

fn rows(vm: usize) -> usize {
    REAL_BODY_STREAMS.peek(vm, |t| t.rows.len()).unwrap_or(0)
}

#[test]
fn body_streams_are_per_vm_and_release_their_root() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    let baos_a = a.fresh_object_ref();

    // One carrier key in both VMs' tables.
    let key: usize = 0x0E10_B100;
    real_body_stream_insert(&mut a, key, baos_a);
    assert_eq!(real_body_stream_get(&a, key), Some(baos_a));
    assert_eq!(
        real_body_stream_get(&b, key),
        None,
        "VM B must not see A's body"
    );

    // B minting a carrier with that key no longer evicts A's row.
    real_body_stream_forget(&mut b, Some(key));
    assert_eq!(real_body_stream_get(&a, key), Some(baos_a));
    assert_eq!(rows(VM_B), 0, "a forget creates no row");

    // A's own forget drops the row and releases its global root.
    let root = REAL_BODY_STREAMS
        .peek(VM_A, |t| t.rows.get(&key).map(|r| r.root))
        .flatten()
        .unwrap();
    real_body_stream_forget(&mut a, Some(key));
    assert_eq!(real_body_stream_get(&a, key), None);
    if root != 0 {
        assert_eq!(a.resolve_global_root(root), None, "root released");
    }
}

#[test]
fn the_bound_evicts_only_sent_bodies() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_C);
    let baos = c.fresh_object_ref();
    for key in 0..REAL_BODY_STREAMS_PER_VM {
        real_body_stream_insert(&mut c, key, baos);
    }
    // Nothing sent yet: the table grows past the bound rather than lose
    // a body that still has to go out.
    let over = REAL_BODY_STREAMS_PER_VM;
    real_body_stream_insert(&mut c, over, baos);
    assert_eq!(rows(VM_C), REAL_BODY_STREAMS_PER_VM + 1);
    // Once row 5 has been sent it is the one that goes.
    REAL_BODY_STREAMS.with(VM_C, |t| t.rows.get_mut(&5).unwrap().sent = true);
    real_body_stream_insert(&mut c, over + 1, baos);
    assert_eq!(rows(VM_C), REAL_BODY_STREAMS_PER_VM + 1);
    assert_eq!(real_body_stream_get(&c, 5), None);
    assert_eq!(real_body_stream_get(&c, 4), Some(baos));
}
