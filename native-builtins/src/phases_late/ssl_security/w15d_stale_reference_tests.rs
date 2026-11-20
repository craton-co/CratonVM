use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{FieldMetadata, NativeMethodRegistry};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;
use std::cell::RefCell;

const VM_MAC: usize = 0xD15_0101;
const VM_MAC_INTO: usize = 0xD15_0102;
const VM_SOCK: usize = 0xD15_0201;
const VM_A: usize = 0xD15_0301;
const VM_B: usize = 0xD15_0302;
const VM_SWEEP: usize = 0xD15_0401;

thread_local! {
    /// (method, old, new): the addresses each hooked method moves.
    static MOVES: RefCell<Vec<(&'static str, usize, usize)>> =
        const { RefCell::new(Vec::new()) };
    /// (method, receiver address) of every hooked call, in order.
    static SEEN: RefCell<Vec<(String, usize)>> = const { RefCell::new(Vec::new()) };
    /// What `engineDoFinal` answers.
    static PRODUCED: RefCell<Option<ObjectRef>> = const { RefCell::new(None) };
}

/// Drops every process-wide row a test's VM identities left behind (TLS
/// weak owners included) and the hook state, on every exit path.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::t27_tls::forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
        MOVES.with(|m| m.borrow_mut().clear());
        SEEN.with(|s| s.borrow_mut().clear());
        PRODUCED.with(|p| *p.borrow_mut() = None);
    }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn apply_moves_for(ctx: &mut MockNativeContext, method: &str) {
    let moves: Vec<(usize, usize)> = MOVES.with(|m| {
        m.borrow()
            .iter()
            .filter(|(name, _, _)| *name == method)
            .map(|(_, old, new)| (*old, *new))
            .collect()
    });
    for (old, new) in moves {
        ctx.remap_native_pin_addr_for_test(old, new);
    }
}

fn recording_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    SEEN.with(|s| {
        s.borrow_mut()
            .push((method_name.to_string(), addr(receiver)))
    });
    apply_moves_for(ctx, method_name);
    match method_name {
        "engineUpdate" | "engineReset" => Some(Ok(None)),
        "engineDoFinal" => {
            let out = PRODUCED.with(|p| *p.borrow());
            Some(Ok(Some(Value::Object(out))))
        }
        "getCipherSuites" => Some(Ok(Some(Value::Object(None)))),
        "getNeedClientAuth" => Some(Ok(Some(Value::Int(1)))),
        "getWantClientAuth" => Some(Ok(Some(Value::Int(0)))),
        "getEndpointIdentificationAlgorithm" => {
            let s = ctx.create_string("HTTPS");
            Some(Ok(Some(Value::Object(Some(s)))))
        }
        _ => None,
    }
}

fn seen() -> Vec<(String, usize)> {
    SEEN.with(|s| s.borrow().clone())
}

/// A `javax.crypto.Mac` whose `spi` field names `spi` (a delegated Mac).
fn delegated_mac(ctx: &mut MockNativeContext, spi: ObjectRef) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("javax/crypto/Mac").unwrap();
    ctx.set_declared_fields(
        cid,
        vec![FieldMetadata {
            name: "spi".to_string(),
            descriptor: "Ljavax/crypto/MacSpi;".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: cid,
            is_static: false,
        }],
    );
    let mac = ctx.alloc_object(cid, 4);
    ctx.set_field(mac, 0, Value::Object(Some(spi)));
    mac
}

fn mac_native(desc: &str) -> cratonvm_native_api::NativeCallback {
    let mut registry = NativeMethodRegistry::new();
    register_p68_crypto_mac(&mut registry);
    registry
        .find("javax/crypto/Mac", "doFinal", desc)
        .expect("Mac.doFinal native should be registered")
}

/// `doFinal(byte[])` on a delegated Mac: the SPI moves during
/// `engineUpdate` and the result array during `engineReset`. The two
/// later calls must reach the MOVED SPI and the MOVED result must be
/// returned (the old code pinned the SPI only after `engineUpdate`, and
/// returned the result's pre-`engineReset` address).
#[test]
fn w15d_delegated_do_final_follows_the_moved_spi_and_result() {
    let _teardown = Teardown(&[VM_MAC]);
    let do_final = mac_native("([B)[B");
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_MAC);
    let (spi, moved_spi) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let mac = delegated_mac(&mut ctx, spi);
    let input = ctx.new_array(ArrayElementType::Byte, 3);
    let result = ctx.new_array(ArrayElementType::Byte, 2);
    let moved_result = ctx.new_array(ArrayElementType::Byte, 2);
    PRODUCED.with(|p| *p.borrow_mut() = Some(result));
    MOVES.with(|m| {
        *m.borrow_mut() = vec![
            ("engineUpdate", addr(spi), addr(moved_spi)),
            ("engineReset", addr(result), addr(moved_result)),
        ]
    });
    ctx.set_invoke_virtual_hook(recording_hook);

    let out = do_final(
        &mut ctx,
        &[Value::Object(Some(mac)), Value::Object(Some(input))],
    )
    .unwrap();

    assert_eq!(
        seen(),
        vec![
            ("engineUpdate".to_string(), addr(spi)),
            ("engineDoFinal".to_string(), addr(moved_spi)),
            ("engineReset".to_string(), addr(moved_spi)),
        ]
    );
    assert_eq!(
        out,
        Some(Value::Object(Some(moved_result))),
        "the moved result"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `doFinal(byte[], int)` on a delegated Mac: the SPI and the output
/// buffer move during `engineDoFinal`. The bytes must land in the MOVED
/// buffer and `engineReset` must reach the MOVED SPI (it used to be sent
/// to the SPI's pre-`engineDoFinal` address).
#[test]
fn w15d_delegated_do_final_into_resets_the_moved_spi() {
    let _teardown = Teardown(&[VM_MAC_INTO]);
    let do_final = mac_native("([BI)V");
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_MAC_INTO);
    let (spi, moved_spi) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let mac = delegated_mac(&mut ctx, spi);
    let out = ctx.new_array(ArrayElementType::Byte, 4);
    let moved_out = ctx.new_array(ArrayElementType::Byte, 4);
    let produced = ctx.new_array(ArrayElementType::Byte, 2);
    ctx.set_array_element(produced, 0, Value::Int(7));
    ctx.set_array_element(produced, 1, Value::Int(9));
    PRODUCED.with(|p| *p.borrow_mut() = Some(produced));
    MOVES.with(|m| {
        *m.borrow_mut() = vec![
            ("engineDoFinal", addr(spi), addr(moved_spi)),
            ("engineDoFinal", addr(out), addr(moved_out)),
        ]
    });
    ctx.set_invoke_virtual_hook(recording_hook);

    let r = do_final(
        &mut ctx,
        &[
            Value::Object(Some(mac)),
            Value::Object(Some(out)),
            Value::Int(1),
        ],
    )
    .unwrap();

    assert_eq!(r, None);
    assert_eq!(
        seen(),
        vec![
            ("engineDoFinal".to_string(), addr(spi)),
            ("engineReset".to_string(), addr(moved_spi)),
        ]
    );
    assert_eq!(ctx.get_array_element(moved_out, 1), Value::Int(7));
    assert_eq!(ctx.get_array_element(moved_out, 2), Value::Int(9));
    assert_eq!(
        ctx.get_array_element(out, 1),
        Value::Int(0),
        "the vacated buffer must not be written"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `SSLSocket.setSSLParameters` on a socket whose handshake is still
/// pending: the socket and the parameters move during `getCipherSuites`.
/// Every later getter must be asked of the MOVED parameters, and the
/// client-auth / endpoint-algorithm rows must be filed under the MOVED
/// socket (the mock's identity hash is the address). The old code re-read
/// both from `args` after `getCipherSuites` and pinned THOSE addresses.
#[test]
fn w15d_set_ssl_parameters_reads_the_parameters_moved_by_get_cipher_suites() {
    let _teardown = Teardown(&[VM_SOCK]);
    let mut registry = NativeMethodRegistry::new();
    register_p68_ssl(&mut registry);
    let set_params = registry
        .find(
            "javax/net/ssl/SSLSocket",
            "setSSLParameters",
            "(Ljavax/net/ssl/SSLParameters;)V",
        )
        .expect("SSLSocket.setSSLParameters native should be registered");
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_SOCK);
    let sock_cid = ctx
        .ensure_class_initialized("javax/net/ssl/SSLSocket")
        .unwrap();
    let sock = ctx.alloc_object(sock_cid, NEW13_SSL_SOCK_FIELDS);
    let moved_sock = ctx.alloc_object(sock_cid, NEW13_SSL_SOCK_FIELDS);
    // A pending layered socket, so the cipher block (and its Java call)
    // runs first. No pending entry exists for id 7 and the hook answers
    // no suites, so nothing is filed for it.
    let pending = crate::servlet::PENDING_LAYERED_SOCK_ID_BASE + 7;
    sock_set_field(&ctx, sock, NEW13_SOCK_TLSID, Value::Int(pending));
    sock_set_field(&ctx, moved_sock, NEW13_SOCK_TLSID, Value::Int(pending));
    let (params, moved_params) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    MOVES.with(|m| {
        *m.borrow_mut() = vec![
            ("getCipherSuites", addr(sock), addr(moved_sock)),
            ("getCipherSuites", addr(params), addr(moved_params)),
        ]
    });
    ctx.set_invoke_virtual_hook(recording_hook);

    set_params(
        &mut ctx,
        &[Value::Object(Some(sock)), Value::Object(Some(params))],
    )
    .unwrap();

    assert_eq!(
        seen(),
        vec![
            ("getProtocols".to_string(), addr(params)),
            ("getCipherSuites".to_string(), addr(params)),
            ("getNeedClientAuth".to_string(), addr(moved_params)),
            ("getWantClientAuth".to_string(), addr(moved_params)),
            (
                "getEndpointIdentificationAlgorithm".to_string(),
                addr(moved_params)
            ),
        ]
    );
    assert_eq!(ssl_sock_auth_get(&ctx, moved_sock), (0, 1, 0));
    assert_eq!(
        ssl_sock_auth_get(&ctx, sock),
        (0, 0, 0),
        "no row for the vacated socket"
    );
    let moved_key = ssl_sock_key(&ctx, moved_sock);
    let alg = ssl_sock_endpoint_alg().lock().get(&moved_key).cloned();
    assert_eq!(alg.as_deref(), Some("HTTPS"));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// Two VMs whose objects draw the same identity hash keep separate `Mac`
/// state, socket client-auth rows and factory-initialised marks; a dead
/// owner's rows go with it, and a VM's teardown takes only its own.
#[test]
fn w15d_identity_rows_are_per_vm_and_go_with_their_owner() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    // The mock's identity hash is the address: one fake object has the
    // same hash under both VMs.
    let obj = unsafe { ObjectRef::from_raw(0x0d15_0000_1500usize as *mut u8) };
    assert_eq!(a.identity_hash_code(obj), b.identity_hash_code(obj));
    assert_ne!(mac_state_key(&a, obj), mac_state_key(&b, obj));
    assert_ne!(ssl_sock_key(&a, obj), ssl_sock_key(&b, obj));

    let state = |algo: &str| MacState {
        algo: algo.to_string(),
        key: vec![1, 2, 3],
        data: Vec::new(),
        initialized: true,
    };
    let (key_a, key_b) = (mac_state_key(&a, obj), mac_state_key(&b, obj));
    mac_state_table()
        .lock()
        .unwrap()
        .insert(key_a, state("HmacSHA256"));
    mac_state_table()
        .lock()
        .unwrap()
        .insert(key_b, state("HmacSHA1"));
    ssl_sock_auth_update(&a, obj, |s| s.1 = 1);
    assert_eq!(
        ssl_sock_auth_get(&b, obj),
        (0, 0, 0),
        "B read A's client-auth row"
    );
    mark_jsse_factory_initialized(&a, obj);
    assert!(require_jsse_factory_initialized(&a, obj, "KeyManagerFactoryImpl").is_ok());
    assert!(
        require_jsse_factory_initialized(&b, obj, "KeyManagerFactoryImpl").is_err(),
        "B's factory passed the gate on A's init"
    );

    // A's object dies: every A row filed under its key goes, B's stay
    // (gc-common w27-a: through the lock-key sweep, which judges only
    // A's slots).
    crate::gc_sweep_lock_keys(VM_A, &|_| false);
    assert!(!mac_state_table().lock().unwrap().contains_key(&key_a));
    assert_eq!(ssl_sock_auth_get(&a, obj), (0, 0, 0));
    assert!(require_jsse_factory_initialized(&a, obj, "KeyManagerFactoryImpl").is_err());
    let algo_b = mac_state_table()
        .lock()
        .unwrap()
        .get(&key_b)
        .map(|s| s.algo.clone());
    assert_eq!(algo_b.as_deref(), Some("HmacSHA1"));

    // B's teardown takes B's rows.
    forget_vm_factory_identity_rows(VM_B);
    assert!(!mac_state_table().lock().unwrap().contains_key(&key_b));
}

/// The sweep wiring end to end: a `Mac` row and a socket row whose owner
/// the collector reports dead are dropped by the lock-key sweep
/// (gc-common w27-a; it was the TLS weak-owner sweep).
#[test]
fn w15d_the_lock_key_sweep_drops_a_dead_macs_and_sockets_rows() {
    let _teardown = Teardown(&[VM_SWEEP]);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_SWEEP);
    let (mac, sock, live) = (
        c.fresh_object_ref(),
        c.fresh_object_ref(),
        c.fresh_object_ref(),
    );
    for o in [mac, live] {
        let key = mac_state_key(&c, o);
        mac_state_table().lock().unwrap().insert(
            key,
            MacState {
                algo: "HmacSHA256".to_string(),
                key: vec![4],
                data: Vec::new(),
                initialized: true,
            },
        );
    }
    ssl_sock_auth_update(&c, sock, |s| s.2 = 1);
    // Taken before the sweep: minting after it would answer fresh keys.
    let (mac_key, live_key, sock_key) = (
        mac_state_key(&c, mac),
        mac_state_key(&c, live),
        ssl_sock_key(&c, sock),
    );
    let live_addr = addr(live);
    crate::gc_sweep_lock_keys(VM_SWEEP, &|x| x == live_addr);
    let (dead_kept, live_kept) = {
        let table = mac_state_table().lock().unwrap();
        (table.contains_key(&mac_key), table.contains_key(&live_key))
    };
    assert!(!dead_kept, "the dead Mac kept its key");
    assert!(live_kept, "a live Mac lost its key");
    assert!(
        !ssl_sock_auth_state().lock().contains_key(&sock_key),
        "the dead socket kept its row"
    );
    assert_eq!(ssl_sock_auth_get(&c, sock), (0, 0, 0));
}
