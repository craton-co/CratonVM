use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xA12C_0001;
const VM_B: usize = 0xA12C_0002;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: an identity-hash source only; the mock hashes the address
    // and never dereferences it.
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

fn performed(ctx: &dyn NativeContext, conn: ObjectRef) {
    // What `huc_real_perform_inner` files: under the carrier's weak lock
    // key (gc-common w27-b; a `HucCarrier` weak owner before).
    let key = huc_obj_key(ctx, conn);
    let vm = ctx.vm_identity();
    real_reqs_with(vm, |t| {
        t.entry(key).or_default().method = "GET".to_string();
    });
    real_results_with(vm, |t| {
        t.insert(
            key,
            RealResult {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                body: vec![0x5A; 4096],
                reason: String::new(),
                truncated: false,
            },
        );
    });
}

fn result_rows(vm: usize) -> usize {
    real_results_peek(vm, |t| t.len()).unwrap_or(0)
}

/// N requests read without `disconnect()`: once the carriers die, the
/// per-VM `results` map keeps only the live one's row. Another VM's
/// collection judges none of them.
#[test]
fn dead_carriers_leave_no_response_body_behind() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let live = at(0xA12C_1000);
    for i in 0..200usize {
        performed(&a, at(0xA12C_1000 + i * 16));
    }
    assert_eq!(result_rows(VM_A), 200);
    let dead = at(0xA12C_1000 + 16);
    let dead_key = huc_existing_obj_key(&a, dead).expect("filed");

    // The lock-key sweep of a VM that keyed nothing frees nothing.
    assert_eq!(crate::gc_sweep_lock_keys(VM_B, &|_| false), 0);
    assert_eq!(result_rows(VM_A), 200);

    crate::gc_sweep_lock_keys(VM_A, &|x| x == live.as_ptr() as usize);
    assert_eq!(result_rows(VM_A), 1);
    assert!(
        real_is_connected(&a, live),
        "the live carrier keeps its response"
    );
    assert_eq!(huc_real_body(&a, live).len(), 4096);
    assert!(!real_is_connected(&a, dead));
    assert_eq!(
        real_reqs_peek(VM_A, |t| t.contains_key(&dead_key)),
        Some(false)
    );
}
