use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::server_socket_ports;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;
use cratonvm_types::PointerMap;

const VM_OPEN: usize = 0x18F0_0501;
const VM_CLOSED: usize = 0x18F0_0502;
const VM_MOVE: usize = 0x18F0_0503;
const VM_TWO_A: usize = 0x18F0_0504;
const VM_TWO_B: usize = 0x18F0_0505;
const VM_BUSY: usize = 0x18F0_0506;

/// Drops the test's VMs' rows and listeners even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_socket_side_tables(vm);
            server_socket_ports::forget_vm(vm);
            crate::servlet::forget_vm_s2_sockets(vm);
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

fn arg(o: ObjectRef) -> [Value; 1] {
    [Value::Object(Some(o))]
}

/// What `re2_bind_listener` leaves behind for a bound socket: a real
/// loopback listener in the `s2` registry, the `ss_side_table` row and the
/// `server_socket_ports` row. Returns the listener id and its port.
fn bind_row(ctx: &MockNativeContext, vm: usize, sock: ObjectRef) -> (i32, i32) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener.local_addr().expect("its address").port() as i32;
    let lid = s2_alloc_listener_for_vm(vm, listener);
    ss_set(ctx, sock, |s| {
        s.port = port;
        s.listener_id = lid;
        s.constructed = 1;
        s.bound = 1;
        s.host = "127.0.0.1".to_string();
    });
    let hash = ctx.identity_hash_code(sock);
    server_socket_ports::record_addr(vm, hash, sock, "127.0.0.1", port);
    (lid, port)
}

/// Sweep until `want` rows of `vm` have gone. The port table's epoch is
/// shared with every test in this binary (see `sweep_expecting` in
/// `w17a_object_keyed_row_tests`). Bounded.
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

fn listener_registered(lid: i32) -> bool {
    s2_registry().lock().listeners.contains_key(&lid)
}

/// The page's retire criterion: a collected open `ServerSocket` leaves no
/// row in either table, is not rooted, and its listener is closed, so its
/// port can be bound again. A live one keeps everything.
#[test]
fn a_dead_open_server_socket_leaves_no_row_and_its_port_can_be_bound_again() {
    let _teardown = Teardown(&[VM_OPEN]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_OPEN);
    let sock = at(0x18F0_6000);
    let (lid, port) = bind_row(&ctx, VM_OPEN, sock);
    let key = mint_native_obj_key(&ctx, sock);
    let hash = ctx.identity_hash_code(sock);

    let mut roots = Vec::new();
    server_socket_ports::gc_scan_roots(VM_OPEN, &mut roots);
    assert!(roots.is_empty(), "a bound ServerSocket is no longer a root");

    let s = addr(sock);
    let empty = PointerMap::default();
    assert_eq!(gc_sweep_net_socket_rows(VM_OPEN, &empty, &|a| a == s), 0);
    assert!(listener_registered(lid), "a live socket keeps its listener");
    assert_eq!(server_socket_ports::get(hash, sock), Some(port));

    assert_eq!(sweep_expecting(VM_OPEN, &empty, &|_| false, 2), 2);
    assert!(
        !ss_side_table().lock().contains_key(&key),
        "its row is gone"
    );
    assert_eq!(
        server_socket_ports::get(hash, sock),
        None,
        "and its port row"
    );
    // Closed by the sweep, or (registry busy in a parallel test) by the
    // next drain.
    net_release_deferred(&mut ctx);
    assert!(
        !listener_registered(lid),
        "the dropped-open listener is closed"
    );
    TcpListener::bind(("127.0.0.1", port as u16))
        .expect("the dead socket's port can be bound again");
}

/// `isBound()` and `getLocalPort()` keep answering on a CLOSED but
/// reachable socket; once it dies its row goes, so a later socket with its
/// identity hash no longer inherits `closed = 1`.
#[test]
fn a_closed_reachable_server_socket_keeps_its_answers_and_a_dead_ones_row_goes() {
    let _teardown = Teardown(&[VM_CLOSED]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_CLOSED);
    let sock = at(0x18F0_7000);
    let (lid, port) = bind_row(&ctx, VM_CLOSED, sock);
    re2_server_socket_close(&mut ctx, &arg(sock)).expect("close");
    assert!(!listener_registered(lid), "close released the listener");

    let s = addr(sock);
    let empty = PointerMap::default();
    assert_eq!(gc_sweep_net_socket_rows(VM_CLOSED, &empty, &|a| a == s), 0);
    assert_eq!(
        re2_server_socket_is_bound(&mut ctx, &arg(sock)).expect("isBound"),
        Some(Value::Int(1))
    );
    assert_eq!(
        re2_server_socket_is_closed(&mut ctx, &arg(sock)).expect("isClosed"),
        Some(Value::Int(1))
    );
    assert_eq!(
        re2_server_socket_local_port(&mut ctx, &arg(sock)).expect("getLocalPort"),
        Some(Value::Int(port))
    );

    assert_eq!(sweep_expecting(VM_CLOSED, &empty, &|_| false, 1), 1);
    assert!(
        !ss_tracked(&ctx, sock),
        "the dead closed socket's row is gone"
    );
    let fresh = ss_get(&ctx, sock);
    assert_eq!(
        (fresh.closed, fresh.bound, fresh.port),
        (0, 0, -1),
        "a later socket with the same identity hash starts clean"
    );
}

/// At the pause site owners are PRE-remap: a moved socket is re-addressed,
/// not judged at its vacated address, and judged at its new one after.
#[test]
fn a_moved_server_socket_is_followed() {
    let _teardown = Teardown(&[VM_MOVE]);
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_MOVE);
    let (from, to) = (0x18F0_8000usize, 0x18F0_9000usize);
    ss_set(&ctx, at(from), |s| {
        s.constructed = 1;
        s.so_timeout = 250;
    });
    let key = mint_native_obj_key(&ctx, at(from));
    let mut map = PointerMap::default();
    map.insert(from, to);

    assert_eq!(gc_sweep_net_socket_rows(VM_MOVE, &map, &|_| false), 0);
    assert_eq!(
        ss_side_table()
            .lock()
            .get(&key)
            .map(|r| (r.owner, r.so_timeout)),
        Some((to, 250)),
        "the row follows its moved socket"
    );
    let empty = PointerMap::default();
    assert_eq!(gc_sweep_net_socket_rows(VM_MOVE, &empty, &|a| a == to), 0);
    assert!(ss_side_table().lock().contains_key(&key));
    assert_eq!(gc_sweep_net_socket_rows(VM_MOVE, &empty, &|_| false), 1);
    assert!(!ss_side_table().lock().contains_key(&key));
}

/// Two VMs: a collection judges only its own VM's server sockets, and
/// never closes another VM's listener.
#[test]
fn another_vms_collection_judges_none_of_this_vms_server_sockets() {
    let _teardown = Teardown(&[VM_TWO_A, VM_TWO_B]);
    let a = MockNativeContext::new();
    a.set_vm_identity(VM_TWO_A);
    let b = MockNativeContext::new();
    b.set_vm_identity(VM_TWO_B);
    let sock_a = at(0x18F0_A000);
    let sock_b = at(0x18F0_B000);
    let (lid_a, port_a) = bind_row(&a, VM_TWO_A, sock_a);
    ss_set(&b, sock_b, |s| s.constructed = 1);
    let empty = PointerMap::default();

    assert_eq!(sweep_expecting(VM_TWO_B, &empty, &|_| false, 1), 1);
    assert!(!ss_tracked(&b, sock_b), "B's dead socket goes");
    assert!(
        ss_tracked(&a, sock_a),
        "B's collection must not judge A's socket"
    );
    assert!(listener_registered(lid_a), "nor close A's listener");
    let hash_a = a.identity_hash_code(sock_a);
    assert_eq!(server_socket_ports::get(hash_a, sock_a), Some(port_a));
}

/// A sweep that finds the socket registry busy (it may run inside a pause)
/// never waits for it: the dead socket's rows go, its listener stays
/// registered and queued, and the VM's next `net_release_deferred` closes
/// it.
#[test]
fn a_busy_registry_defers_the_listener_close() {
    let _teardown = Teardown(&[VM_BUSY]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_BUSY);
    let sock = at(0x18F0_C000);
    let (lid, _port) = bind_row(&ctx, VM_BUSY, sock);
    let key = mint_native_obj_key(&ctx, sock);
    let empty = PointerMap::default();
    {
        let reg = s2_registry().lock();
        let gone = sweep_expecting(VM_BUSY, &empty, &|_| false, 2);
        assert_eq!(gone, 2, "the rows go whether or not the registry is free");
        assert!(
            reg.listeners.contains_key(&lid),
            "the sweep did not wait for the registry"
        );
    }
    assert!(!ss_side_table().lock().contains_key(&key));
    net_release_deferred(&mut ctx);
    assert!(!listener_registered(lid), "the next drain closes it");
}
