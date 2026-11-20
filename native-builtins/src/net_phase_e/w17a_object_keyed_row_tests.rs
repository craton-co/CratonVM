use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;
use cratonvm_types::PointerMap;

const VM_DEAD: usize = 0x17A0_0401;
const VM_TWO_A: usize = 0x17A0_0402;
const VM_TWO_B: usize = 0x17A0_0403;
const VM_MOVE: usize = 0x17A0_0404;
const VM_CLOSED: usize = 0x17A0_0405;
const VM_EPOCH: usize = 0x17A0_0406;
const VM_CARRIER: usize = 0x17A0_0407;
const VM_PROBE: usize = 0x17A0_0408;

/// Drops the test's VMs' rows even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_socket_side_tables(vm);
        }
    }
}

/// A fabricated, 8-aligned address; used only as a key, never read.
fn at(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

// Through `dyn NativeContext`, so no supertrait import is needed.
fn new_obj(ctx: &mut dyn NativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    }
}

fn add_root(ctx: &mut dyn NativeContext, o: ObjectRef) -> usize {
    ctx.add_global_root(o)
}

fn fds(ctx: &dyn NativeContext) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
    ctx.fd_table()
}

/// Sweep until `want` rows have gone in total. The object-keyed tables
/// are shared with every test in this binary, and a write by a parallel
/// test between a sweep's collection and its drop defers that table's
/// drop to the next sweep (the write-epoch check), so one sweep is not
/// guaranteed to finish. Bounded; the window is a few instructions wide.
fn sweep_expecting(
    vm: usize,
    map: &PointerMap,
    is_live: &dyn Fn(usize) -> bool,
    want: usize,
) -> usize {
    let mut total = 0;
    for _ in 0..256 {
        total += gc_sweep_net_socket_rows(vm, map, is_live);
        if total >= want {
            break;
        }
    }
    total
}

/// The page's retire criterion: a collected `DatagramSocket` and
/// `InetAddress` mirror leave no row (and are not rooted), and a socket
/// dropped OPEN has its descriptor closed by the VM's next native call,
/// as HotSpot's cleaner closes a collected socket's.
#[test]
fn a_dead_socket_and_address_leave_no_row_and_the_open_fd_is_closed() {
    let _teardown = Teardown(&[VM_DEAD]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_DEAD);
    let fd = fds(&ctx)
        .open_udp(Some("127.0.0.1:0"))
        .expect("a loopback UDP socket");
    let (sock, mirror) = (at(0x17A0_1000), at(0x17A0_2000));
    ds_set(VM_DEAD, sock, |d| {
        d.fd = fd as i32;
        d.connected = 1;
    });
    ds_set_peer(VM_DEAD, sock, "127.0.0.1", 9);
    inet_addr_set(VM_DEAD, mirror, "w17a.example", "10.17.0.1");

    let mut roots = Vec::new();
    gc_scan_ds_roots(VM_DEAD, &mut roots);
    gc_scan_inet_addr_roots(VM_DEAD, &mut roots);
    assert!(roots.is_empty(), "neither table roots its key any more");

    let empty = PointerMap::default();
    let (s, m) = (addr(sock), addr(mirror));
    assert_eq!(
        gc_sweep_net_socket_rows(VM_DEAD, &empty, &|a| a == s || a == m),
        0,
        "a collection that keeps both keeps every row"
    );
    assert_eq!(ds_get(sock).fd, fd as i32);
    assert!(inet_addr_get_raw(mirror).is_some());

    assert_eq!(sweep_expecting(VM_DEAD, &empty, &|_| false, 3), 3);
    assert_eq!(ds_get(sock).fd, -1, "the dead socket's row is gone");
    assert_eq!(ds_peer(sock), None, "and its peer row");
    assert!(
        inet_addr_get_raw(mirror).is_none(),
        "the dead mirror's row is gone"
    );
    assert!(net_deferred()
        .lock()
        .udp_fds
        .get(&VM_DEAD)
        .is_some_and(|q| q.contains(&(fd as i32))));
    assert!(
        fds(&ctx).udp_local_addr(fd).is_ok(),
        "the sweep has no NativeContext: the descriptor waits for one"
    );
    net_release_deferred(&mut ctx);
    assert!(
        fds(&ctx).udp_local_addr(fd).is_err(),
        "the dropped-open socket's descriptor is closed"
    );
    assert!(!net_deferred().lock().udp_fds.contains_key(&VM_DEAD));
}

/// Two VMs: a collection judges only its own VM's rows.
#[test]
fn another_vms_collection_judges_none_of_this_vms_rows() {
    let _teardown = Teardown(&[VM_TWO_A, VM_TWO_B]);
    let (sock_a, addr_a, addr_b) = (at(0x17A0_3000), at(0x17A0_3100), at(0x17A0_3200));
    ds_set(VM_TWO_A, sock_a, |d| d.port = 4242);
    inet_addr_set(VM_TWO_A, addr_a, "a.example", "10.17.0.2");
    inet_addr_set(VM_TWO_B, addr_b, "b.example", "10.17.0.3");
    let empty = PointerMap::default();

    assert_eq!(sweep_expecting(VM_TWO_B, &empty, &|_| false, 1), 1);
    assert!(inet_addr_get_raw(addr_b).is_none(), "B's dead mirror goes");
    assert_eq!(
        ds_get(sock_a).port,
        4242,
        "B's collection must not judge A's socket"
    );
    assert!(
        inet_addr_get_raw(addr_a).is_some(),
        "B's collection must not judge A's mirror"
    );

    assert_eq!(sweep_expecting(VM_TWO_A, &empty, &|_| false, 2), 2);
    assert!(inet_addr_get_raw(addr_a).is_none());
    assert_eq!(ds_get(sock_a).port, 0, "A's socket row is gone");
}

/// At the pause site the keys are PRE-remap: a moved key is kept whatever
/// the predicate says, the remap re-keys it, and it is judged at its new
/// address from then on.
#[test]
fn a_moved_key_is_not_judged_and_its_rows_follow_it() {
    let _teardown = Teardown(&[VM_MOVE]);
    let (old_sock, new_sock) = (0x17A0_4000usize, 0x17A0_5000usize);
    let (old_ia, new_ia) = (0x17A0_4100usize, 0x17A0_5100usize);
    ds_set(VM_MOVE, at(old_sock), |d| d.timeout = 77);
    ds_set_peer(VM_MOVE, at(old_sock), "10.17.0.4", 5353);
    inet_addr_set(VM_MOVE, at(old_ia), "moved.example", "10.17.0.5");
    let mut map = PointerMap::default();
    map.insert(old_sock, new_sock);
    map.insert(old_ia, new_ia);

    assert_eq!(
        gc_sweep_net_socket_rows(VM_MOVE, &map, &|_| false),
        0,
        "a moved key survived; the vacated address is not asked about"
    );
    gc_update_ds_refs(VM_MOVE, &map);
    gc_update_inet_addr_refs(VM_MOVE, &map);
    assert_eq!(ds_get(at(new_sock)).timeout, 77);
    assert_eq!(ds_peer(at(new_sock)), Some(("10.17.0.4".to_string(), 5353)));
    assert_eq!(
        inet_addr_get_raw(at(new_ia)),
        Some(("moved.example".to_string(), "10.17.0.5".to_string()))
    );

    let empty = PointerMap::default();
    assert_eq!(
        gc_sweep_net_socket_rows(VM_MOVE, &empty, &|a| a == new_sock || a == new_ia),
        0,
        "live at the new address: kept"
    );
    assert_eq!(sweep_expecting(VM_MOVE, &empty, &|_| false, 3), 3);
    assert_eq!(ds_get(at(new_sock)).timeout, 0);
    assert!(inet_addr_get_raw(at(new_ia)).is_none());
}

/// The JDK requires `getPort()` / `getInetAddress()` / `isConnected()` to
/// keep answering on a CLOSED socket, so a reachable closed socket keeps
/// its rows; once it dies they go, with no descriptor to close.
#[test]
fn a_closed_reachable_socket_keeps_its_answers() {
    let _teardown = Teardown(&[VM_CLOSED]);
    let sock = at(0x17A0_6000);
    ds_set(VM_CLOSED, sock, |d| {
        d.closed = 1;
        d.fd = -1;
        d.connected = 1;
    });
    ds_set_peer(VM_CLOSED, sock, "10.17.0.6", 6000);
    let s = addr(sock);
    let empty = PointerMap::default();

    assert_eq!(gc_sweep_net_socket_rows(VM_CLOSED, &empty, &|a| a == s), 0);
    assert_eq!(ds_peer(sock), Some(("10.17.0.6".to_string(), 6000)));
    assert_eq!(ds_get(sock).connected, 1);
    assert_eq!(ds_get(sock).closed, 1);

    assert_eq!(sweep_expecting(VM_CLOSED, &empty, &|_| false, 2), 2);
    assert_eq!(ds_peer(sock), None);
    assert!(
        !net_deferred().lock().udp_fds.contains_key(&VM_CLOSED),
        "a closed socket already released its descriptor"
    );
}

/// Outside a pause a mutator can file a row while the sweep asks the heap
/// (a new object on a just-freed address). The drop is then deferred to
/// the next sweep rather than risked, and the new row is never touched.
#[test]
fn a_row_written_while_the_sweep_judges_defers_the_drop() {
    let _teardown = Teardown(&[VM_EPOCH]);
    let (dead, fresh) = (at(0x17A0_7000), at(0x17A0_7100));
    inet_addr_set(VM_EPOCH, dead, "dead.example", "10.17.0.7");
    let empty = PointerMap::default();

    let dropped = gc_sweep_net_socket_rows(VM_EPOCH, &empty, &|_| {
        inet_addr_set(VM_EPOCH, fresh, "fresh.example", "10.17.0.8");
        false
    });
    assert_eq!(
        dropped, 0,
        "the table was written since the keys were collected"
    );
    assert!(inet_addr_get_raw(dead).is_some());
    assert!(inet_addr_get_raw(fresh).is_some());

    let f = addr(fresh);
    assert_eq!(sweep_expecting(VM_EPOCH, &empty, &|a| a == f, 1), 1);
    assert!(inet_addr_get_raw(dead).is_none(), "the next sweep drops it");
    assert_eq!(
        inet_addr_get_raw(fresh),
        Some(("fresh.example".to_string(), "10.17.0.8".to_string()))
    );
}

/// A dead `HttpsURLConnection` carrier's recorded handshake goes with it,
/// and the global root on the `SSLSession` it cached is released by the
/// VM's next native call. A moved carrier is followed, a live one kept.
#[test]
fn a_dead_https_carriers_handshake_goes_and_its_session_root_is_released() {
    let _teardown = Teardown(&[VM_CARRIER]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_CARRIER);
    let carrier = new_obj(&mut ctx, "javax/net/ssl/HttpsURLConnection");
    record_https_carrier_session(
        &mut ctx,
        carrier,
        "TLSv1.3",
        "TLS_AES_128_GCM_SHA256",
        &[vec![0x30, 0x03, 0x02, 0x01, 0x01]],
        "127.0.0.1",
        8443,
    );
    let key = mint_native_obj_key(&ctx, carrier);
    let session = new_obj(&mut ctx, "javax/net/ssl/SSLSession");
    let roots_before = ctx.global_root_count();
    let handle = add_root(&mut ctx, session);
    https_carrier_sessions()
        .lock()
        .get_mut(&key)
        .expect("the handshake was recorded")
        .session_root = handle;
    let c = addr(carrier);
    let moved_to = c + 0x10_0000;
    let empty = PointerMap::default();

    let mut map = PointerMap::default();
    map.insert(c, moved_to);
    assert_eq!(gc_sweep_net_socket_rows(VM_CARRIER, &map, &|_| false), 0);
    assert_eq!(
        https_carrier_sessions().lock().get(&key).map(|s| s.owner),
        Some(moved_to),
        "the row follows its moved carrier"
    );
    assert_eq!(
        gc_sweep_net_socket_rows(VM_CARRIER, &empty, &|a| a == moved_to),
        0
    );
    assert!(https_carrier_session(&ctx, carrier).is_some());

    assert_eq!(gc_sweep_net_socket_rows(VM_CARRIER, &empty, &|_| false), 1);
    assert!(!https_carrier_sessions().lock().contains_key(&key));
    assert_eq!(
        ctx.global_root_count(),
        roots_before + 1,
        "the sweep has no NativeContext: the root waits for one"
    );
    net_release_deferred(&mut ctx);
    assert_eq!(ctx.global_root_count(), roots_before);
}

/// A probe socket the factory never handed back (so
/// `take_probe_restrictions` never claimed its entry) loses the entry
/// with its socket row; a live probe keeps it.
#[test]
fn an_unclaimed_dead_probe_sockets_restrictions_go_with_it() {
    let _teardown = Teardown(&[VM_PROBE]);
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_PROBE);
    let sock = at(0x17A0_8000);
    sock_set_owned(&ctx, sock, |s| {
        s.host = "w17a.example".to_string();
        s.port = 443;
        s.stream_id = -1;
    });
    let key = mint_native_obj_key(&ctx, sock);
    probe_restrictions().lock().insert(
        key,
        (vec!["TLS_AES_128_GCM_SHA256".to_string()], Vec::new()),
    );
    let s = addr(sock);
    let empty = PointerMap::default();

    assert_eq!(gc_sweep_net_socket_rows(VM_PROBE, &empty, &|a| a == s), 0);
    assert!(probe_restrictions().lock().contains_key(&key));

    assert_eq!(gc_sweep_net_socket_rows(VM_PROBE, &empty, &|_| false), 1);
    assert!(!sock_side_table().lock().contains_key(&key));
    assert!(
        !probe_restrictions().lock().contains_key(&key),
        "the dead probe's restrictions go with its row"
    );
}
