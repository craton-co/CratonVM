use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;
use cratonvm_types::PointerMap;

const VM_PAIR: usize = 0x28A0_0401;
const VM_FREED: usize = 0x28A0_0402;
const VM_STREAM: usize = 0x28A0_0403;
const VM_TWO_A: usize = 0x28A0_0404;
const VM_TWO_B: usize = 0x28A0_0405;
const VM_READ: usize = 0x28A0_0406;

/// Drops this test's VMs' rows and lock keys, even when an assertion
/// fails. Only the VMs named: the tables are process-wide.
struct Guard(&'static [usize]);
impl Drop for Guard {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_socket_side_tables(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the registry and the mock hash the address and
    // never dereference it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> MockNativeContext {
    let c = MockNativeContext::new();
    c.set_vm_identity(vm);
    c
}

/// Two live sockets, and two live `https` carriers, sharing one identity
/// hash keep separate rows: a write to one is not read through the other,
/// and forgetting one leaves the other's.
#[test]
fn two_live_same_hash_sockets_and_carriers_keep_separate_rows() {
    let _guard = Guard(&[VM_PAIR]);
    let mut ctx = ctx_for(VM_PAIR);
    let (a, b) = (at(0x1_28A0_1000), at(0x2_28A0_1000));
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: one identity hash"
    );
    sock_set(&ctx, a, |s| {
        s.port = 1111;
        s.stream_id = 0x3FFF_28A1;
    });
    sock_set(&ctx, b, |s| {
        s.port = 2222;
        s.stream_id = 0x3FFF_28A2;
    });
    assert_ne!(mint_native_obj_key(&ctx, a), mint_native_obj_key(&ctx, b));
    assert_eq!(
        (sock_get(&ctx, a).port, sock_stream_id_for_upcall(&ctx, a)),
        (1111, 0x3FFF_28A1)
    );
    assert_eq!(
        (sock_get(&ctx, b).port, sock_stream_id_for_upcall(&ctx, b)),
        (2222, 0x3FFF_28A2)
    );

    // Server sockets.
    ss_set(&ctx, a, |s| s.so_timeout = 11);
    ss_set(&ctx, b, |s| s.so_timeout = 22);
    assert_eq!(
        (ss_get(&ctx, a).so_timeout, ss_get(&ctx, b).so_timeout),
        (11, 22)
    );

    // `HttpsURLConnection` carriers: B's handshake must not replace A's.
    for (carrier, protocol) in [(a, "TLSv1.3"), (b, "TLSv1.2")] {
        record_https_carrier_session(&mut ctx, carrier, protocol, "TLS_X", &[], "h.test", 443);
    }
    assert_eq!(
        https_carrier_session(&ctx, a).map(|s| s.protocol),
        Some("TLSv1.3".to_string())
    );
    assert_eq!(
        https_carrier_session(&ctx, b).map(|s| s.protocol),
        Some("TLSv1.2".to_string())
    );
    forget_https_carrier_session(&mut ctx, b);
    assert!(https_carrier_session(&ctx, b).is_none());
    assert!(
        https_carrier_session(&ctx, a).is_some(),
        "forgetting one carrier must not evict its hash twin's handshake"
    );

    // `SSLSessionContext` tuning and `HttpClient` shutdown state.
    ssc_set(&ctx, a, |s| s.cache_size = 5);
    assert_eq!(ssc_get(&ctx, a).cache_size, 5);
    assert_eq!(ssc_get(&ctx, b).cache_size, SSC_DEFAULT_CACHE_SIZE);
    let ka = mint_native_obj_key(&ctx, a);
    re5_client_shutdown_table().lock().insert(ka, 0);
    let kb = mint_native_obj_key(&ctx, b);
    assert!(!re5_client_shutdown_table().lock().contains_key(&kb));
}

/// A freed key (its object died) drops every row filed under it -- rows
/// written with no owner address included -- and queues the roots they
/// held; a live object's rows stay.
#[test]
fn a_freed_key_drops_the_rows_and_queues_their_roots() {
    let _guard = Guard(&[VM_FREED]);
    let mut ctx = ctx_for(VM_FREED);
    let (dead, live) = (at(0x1_28A0_2000), at(0x2_28A0_2000));
    let session = at(0x3_28A0_2000);
    for o in [dead, live] {
        // `sock_set` records no owner: the owner-death sweep never judged
        // such a row, which used to stay until VM teardown.
        sock_set(&ctx, o, |s| s.port = 7);
        ss_set(&ctx, o, |s| s.so_timeout = 9);
        ssc_set(&ctx, o, |s| s.timeout_secs = 3);
        record_https_carrier_session(&mut ctx, o, "TLSv1.3", "TLS_X", &[], "h.test", 443);
        let k = mint_native_obj_key(&ctx, o);
        re5_client_shutdown_table().lock().insert(k, 0);
        probe_restrictions()
            .lock()
            .insert(k, (Vec::new(), Vec::new()));
    }
    let (kd, kl) = (
        mint_native_obj_key(&ctx, dead),
        mint_native_obj_key(&ctx, live),
    );
    let roots_before = ctx.global_root_count();
    let handle = ctx.add_global_root(session);
    https_carrier_sessions()
        .lock()
        .get_mut(&kd)
        .expect("recorded")
        .session_root = handle;

    let live_addr = live.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM_FREED, &|x| x == live_addr) >= 1);

    assert!(!sock_side_table().lock().contains_key(&kd));
    assert!(!ss_side_table().lock().contains_key(&kd));
    assert!(!https_carrier_sessions().lock().contains_key(&kd));
    assert!(!re5_client_shutdown_table().lock().contains_key(&kd));
    assert!(!probe_restrictions().lock().contains_key(&kd));
    assert!(!ssc_owner_table().lock().contains_key(&kd));
    assert!(!ssc_side_table().lock().contains_key(&(kd, SSC_TAG_ORPHAN)));
    // One statement per lock (lesson dd).
    let live_sock = sock_side_table().lock().contains_key(&kl);
    let live_ss = ss_side_table().lock().contains_key(&kl);
    let live_https = https_carrier_sessions().lock().contains_key(&kl);
    let live_client = re5_client_shutdown_table().lock().contains_key(&kl);
    let live_probe = probe_restrictions().lock().contains_key(&kl);
    let live_binding = ssc_owner_table().lock().contains_key(&kl);
    assert!(
        live_sock && live_ss && live_https && live_client && live_probe && live_binding,
        "a live object's rows stay"
    );
    assert_eq!(ssc_get(&ctx, live).timeout_secs, 3);
    assert_eq!(
        ctx.global_root_count(),
        roots_before + 1,
        "the sweep has no NativeContext: the session root waits for one"
    );
    net_release_deferred(&mut ctx);
    assert_eq!(ctx.global_root_count(), roots_before);
}

/// A dead socket whose stream still lives keeps its row (HotSpot's stream
/// references its socket), marked for the owner-death sweep; once the
/// stream's key is freed too, both go.
#[test]
fn a_live_stream_keeps_its_dead_sockets_row() {
    let _guard = Guard(&[VM_STREAM]);
    let ctx = ctx_for(VM_STREAM);
    let (sock, stream) = (at(0x1_28A0_3000), at(0x1_28A0_3100));
    sock_set(&ctx, sock, |s| s.stream_id = 0x3FFF_28A3);
    let (ks, kst) = (
        mint_native_obj_key(&ctx, sock),
        mint_native_obj_key(&ctx, stream),
    );
    stream_owner_set_at(kst, ks, stream.as_ptr() as usize);
    let stream_addr = stream.as_ptr() as usize;

    crate::gc_sweep_lock_keys(VM_STREAM, &|x| x == stream_addr);
    assert_eq!(
        sock_side_table()
            .lock()
            .get(&ks)
            .map(|s| (s.owner, s.stream_id)),
        Some((SOCK_OWNER_GONE, 0x3FFF_28A3)),
        "the live stream keeps its socket's connection"
    );
    assert_eq!(stream_owner_get(&ctx, stream), Some(ks));

    crate::gc_sweep_lock_keys(VM_STREAM, &|_| false);
    let stream_row = stream_owner_table().lock().by_stream.contains_key(&kst);
    let owner_row = stream_owner_table().lock().by_owner.contains_key(&ks);
    assert!(!stream_row && !owner_row, "the dead stream's rows go");
    assert_eq!(
        gc_sweep_net_socket_rows(VM_STREAM, &PointerMap::default(), &|_| false),
        1,
        "the owner-death sweep drops the socket once its last stream is gone"
    );
    assert!(!sock_side_table().lock().contains_key(&ks));
}

/// One address in two VMs: separate keys and rows, and one VM's freed keys
/// drop none of the other's.
#[test]
fn two_vms_stay_separate() {
    let _guard = Guard(&[VM_TWO_A, VM_TWO_B]);
    let (a, b) = (ctx_for(VM_TWO_A), ctx_for(VM_TWO_B));
    let s = at(0x1_28A0_4000);
    sock_set(&a, s, |r| r.port = 1);
    sock_set(&b, s, |r| r.port = 2);
    let (ka, kb) = (mint_native_obj_key(&a, s), mint_native_obj_key(&b, s));
    assert_ne!(ka, kb);
    assert_eq!((sock_get(&a, s).port, sock_get(&b, s).port), (1, 2));

    crate::gc_sweep_lock_keys(VM_TWO_A, &|_| false);
    assert!(!sock_side_table().lock().contains_key(&ka));
    assert_eq!(
        sock_get(&b, s).port,
        2,
        "VM A's collection drops none of B's rows"
    );
}

/// Read and remove paths never mint: asking about an object this file
/// never wrote leaves no lock-key slot behind.
#[test]
fn read_and_remove_paths_do_not_mint() {
    let _guard = Guard(&[VM_READ]);
    let mut ctx = ctx_for(VM_READ);
    let o = at(0x1_28A0_5000);
    assert_eq!(sock_get(&ctx, o).stream_id, -1);
    assert_eq!(sock_stream_id_for_upcall(&ctx, o), -1);
    assert!(!ss_tracked(&ctx, o));
    assert_eq!(ss_get(&ctx, o).port, -1);
    assert!(!is_probe_socket(&ctx, o));
    assert!(take_probe_restrictions(&ctx, o).is_none());
    assert!(ssc_key(&ctx, o).is_none());
    assert_eq!(ssc_get(&ctx, o).cache_size, SSC_DEFAULT_CACHE_SIZE);
    assert!(stream_owner_get(&ctx, o).is_none());
    assert!(https_carrier_session(&ctx, o).is_none());
    forget_https_carrier_session(&mut ctx, o);
    assert!(
        crate::existing_weak_lock_key(&ctx, o).is_none(),
        "no read or remove path minted a key"
    );
}
