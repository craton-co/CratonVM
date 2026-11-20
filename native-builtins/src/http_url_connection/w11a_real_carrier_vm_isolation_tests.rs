use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xA11B_0001;
const VM_B: usize = 0xA11B_0002;
// `a_live_upload_belongs_to_its_vm` owns its own pair: the tests run in
// parallel, and each `Teardown` forgets its VMs wholesale, so a shared
// identity let one test wipe the other's rows mid-run.
const VM_C: usize = 0xA11B_0003;
const VM_D: usize = 0xA11B_0004;

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

#[test]
fn a_mint_in_one_vm_leaves_another_vms_carrier_alone() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);

    // One identity hash in both VMs (the mock's hash is the address
    // truncated to `i32`).
    let (conn_a, conn_b) = (at(0x1_A11B_1000), at(0x2_A11B_1000));
    assert_eq!(a.identity_hash_code(conn_a), b.identity_hash_code(conn_b));

    with_real_req(&a, conn_a, |req| {
        req.method = "PUT".to_string();
        req.read_timeout_ms = Some(1234);
    });
    // The carrier's weak lock key (gc-common w27-b), minted just above.
    let key = huc_existing_obj_key(&a, conn_a).expect("filed");
    real_results_with(VM_A, |t| {
        t.insert(
            key,
            RealResult {
                status: 201,
                headers: vec![("X-Vm".to_string(), "a".to_string())],
                body: b"from-a".to_vec(),
                reason: String::new(),
                truncated: false,
            },
        );
    });

    // VM B reads none of it...
    assert!(
        !real_is_connected(&b, conn_b),
        "B must not see A's response"
    );
    assert!(huc_real_body(&b, conn_b).is_empty());
    assert!(huc_real_headers(&b, conn_b).is_empty());

    // ...and B's `URL.openConnection()` mint of a carrier with that hash
    // (`real_forget`) no longer deletes A's in-flight request.
    real_forget(&mut b, conn_b);
    let method = real_reqs_peek(VM_A, |t| t.get(&key).map(|r| r.method.clone())).flatten();
    assert_eq!(method.as_deref(), Some("PUT"));
    assert!(real_is_connected(&a, conn_a));
    assert_eq!(huc_real_body(&a, conn_a), b"from-a".to_vec());
    assert!(!REAL_CARRIERS.has_row(VM_B), "a forget creates no row");

    // A's own mint does drop them.
    real_forget(&mut a, conn_a);
    assert!(!real_is_connected(&a, conn_a));
    assert_eq!(real_reqs_peek(VM_A, |t| t.contains_key(&key)), Some(false));
}

/// A live upload is found, taken and forgotten only through its own VM.
#[test]
fn a_live_upload_belongs_to_its_vm() {
    let _teardown = Teardown(&[VM_C, VM_D]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let tcp = TcpStream::connect(listener.local_addr().unwrap()).expect("connect loopback");
    let key = 0x0A11_B200;
    let baos_key = 0x0A11_B201;
    REAL_CARRIERS.with(VM_C, |t| {
        t.fixed_streams.insert(
            key,
            Arc::new(Mutex::new(LiveFixedStream {
                tcp,
                expected: 4,
                written: 0,
                closed: false,
                jdk_counted: false,
            })),
        );
        t.fixed_owners.insert(baos_key, key);
    });
    // Another VM's forget or take of the same key is a no-op for A.
    forget_live_fixed_stream(VM_D, key);
    assert!(take_live_fixed_stream(VM_D, key).is_none());
    let cell = take_live_fixed_stream(VM_C, key).expect("A's upload is still there");
    assert_eq!(lock_live_fixed_stream(&cell).expected, 4);
    assert_eq!(
        REAL_CARRIERS.peek(VM_C, |t| t.fixed_owners.contains_key(&baos_key)),
        Some(false),
        "taking the stream drops its owner row"
    );
}
