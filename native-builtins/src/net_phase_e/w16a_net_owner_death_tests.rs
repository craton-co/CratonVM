use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;
use cratonvm_types::PointerMap;

const VM_DROP: usize = 0x16A0_0401;
const VM_MOVE: usize = 0x16A0_0402;
const VM_SSC: usize = 0x16A0_0403;
const VM_ORPHAN: usize = 0x16A0_0404;
const VM_SSS: usize = 0x16A0_0405;
const VM_HTTP: usize = 0x16A0_0406;
const VM_TEARDOWN: usize = 0x16A0_0407;
const VM_CLIENT: usize = 0x16A0_0408;
const VM_DROP_OTHER: usize = VM_DROP + 0x100;
const VM_HTTP_OTHER: usize = VM_HTTP + 0x100;
const VM_TEARDOWN_OTHER: usize = VM_TEARDOWN + 0x100;

/// Drops the test's VMs' rows even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_socket_side_tables(vm);
            crate::servlet::forget_vm_s2_sockets(vm);
        }
    }
}

fn obj(v: MethodCallResult) -> ObjectRef {
    match v {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    }
}

/// A fabricated, 8-aligned address; never dereferenced (the mock's
/// identity hash is the address itself).
fn at(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// The page's retire criterion, on a real loopback connection: a socket
/// dropped without `close()` has its descriptor closed (the peer reads
/// EOF, as it does on HotSpot once the `Socket` is collected) and its row
/// dropped. Another VM's collection judges none of it, and a collection
/// that keeps the socket keeps everything.
#[test]
fn a_dropped_unclosed_socket_has_its_stream_closed_and_its_row_dropped() {
    let _teardown = Teardown(&[VM_DROP, VM_DROP_OTHER]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_DROP);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let sid = crate::servlet::s2_alloc_stream_for_vm(VM_DROP, client);
    let sock = obj(ctx.new_object("java/net/Socket"));
    sock_set_owned(&ctx, sock, |s| {
        s.stream_id = sid;
        s.owns_stream = true;
    });
    let key = mint_native_obj_key(&ctx, sock);
    let empty = PointerMap::default();

    assert_eq!(
        gc_sweep_net_socket_rows(VM_DROP_OTHER, &empty, &|_| false),
        0,
        "another VM's collection judges no row of this VM"
    );
    let sock_addr = addr(sock);
    assert_eq!(
        gc_sweep_net_socket_rows(VM_DROP, &empty, &|a| a == sock_addr),
        0
    );
    assert!(sock_side_table().lock().contains_key(&key));
    assert!(crate::servlet::s2_registry()
        .lock()
        .streams
        .contains_key(&sid));

    // The socket is dropped unclosed.
    assert_eq!(gc_sweep_net_socket_rows(VM_DROP, &empty, &|_| false), 1);
    assert!(
        !sock_side_table().lock().contains_key(&key),
        "the dead socket's row is gone"
    );
    // Closes the stream now if the sweep found the socket registry busy.
    net_release_deferred(&mut ctx);
    let mut buf = [0u8; 1];
    let n = peer
        .read(&mut buf)
        .expect("the peer must see the close, not time out");
    assert_eq!(
        n, 0,
        "the peer reads EOF: the dropped socket's fd was closed"
    );
    assert!(!crate::servlet::s2_registry()
        .lock()
        .streams
        .contains_key(&sid));
}

/// A moved socket's row follows it and is not judged; a dead socket whose
/// stream still lives keeps its row (HotSpot's stream references its
/// socket); once the stream dies too, both rows go.
#[test]
fn a_moved_socket_is_followed_and_a_live_stream_keeps_a_dead_socket() {
    let _teardown = Teardown(&[VM_MOVE]);
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_MOVE);
    let (sock, moved_to, stream) = (at(0x16A0_1000), 0x16A0_2000usize, at(0x16A0_3000));
    // A plain stream id no s2 socket of a concurrently running test holds.
    sock_set_owned(&ctx, sock, |s| {
        s.stream_id = 0x3FFF_16A2;
        s.owns_stream = true;
    });
    let key = mint_native_obj_key(&ctx, sock);
    let stream_key = mint_native_obj_key(&ctx, stream);
    stream_owner_set_at(stream_key, key, addr(stream));
    let stream_addr = addr(stream);
    let empty = PointerMap::default();

    let mut map = PointerMap::default();
    map.insert(addr(sock), moved_to);
    assert_eq!(
        gc_sweep_net_socket_rows(VM_MOVE, &map, &|a| a == stream_addr),
        0
    );
    assert_eq!(
        sock_side_table().lock().get(&key).map(|s| s.owner),
        Some(moved_to),
        "the row follows its moved socket"
    );

    // The socket dies; its stream does not.
    assert_eq!(
        gc_sweep_net_socket_rows(VM_MOVE, &empty, &|a| a == stream_addr),
        0
    );
    {
        let t = sock_side_table().lock();
        let row = t.get(&key).expect("a live stream keeps its socket's row");
        assert_eq!(row.owner, SOCK_OWNER_GONE);
        assert_eq!(row.stream_id, 0x3FFF_16A2, "and its connection");
    }
    assert_eq!(stream_owner_get(&ctx, stream), Some(key));

    // The stream dies too.
    assert_eq!(gc_sweep_net_socket_rows(VM_MOVE, &empty, &|_| false), 2);
    assert!(!sock_side_table().lock().contains_key(&key));
    assert_eq!(stream_owner_get(&ctx, stream), None);
    let t = stream_owner_table().lock();
    assert!(!t.by_owner.contains_key(&key));
    assert!(!t.addrs.contains_key(&stream_key));
}

/// The carrier root goes with its `SSLContext`; the carrier then lives as
/// long as the application holds it (its tuning still reads back), and
/// its binding and tuning row go with it.
#[test]
fn a_dead_contexts_carrier_root_is_released_and_its_rows_go_with_the_carrier() {
    let _teardown = Teardown(&[VM_SSC]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_SSC);
    let sslctx = obj(ctx.new_object("javax/net/ssl/SSLContext"));
    let roots_before = ctx.global_root_count();
    let carrier = ssc_carrier(&mut ctx, sslctx, SSC_TAG_SERVER).expect("minted");
    assert_eq!(
        ssc_carrier(&mut ctx, sslctx, SSC_TAG_SERVER).expect("again"),
        carrier,
        "one carrier per (SSLContext, side)"
    );
    assert_eq!(ctx.global_root_count(), roots_before + 1);
    ssc_set(&ctx, carrier, |s| s.cache_size = 7);
    let carrier_addr = addr(carrier);
    let empty = PointerMap::default();

    // The context dies. Its carrier is still rooted, so it is live.
    assert_eq!(
        gc_sweep_net_socket_rows(VM_SSC, &empty, &|a| a == carrier_addr),
        1
    );
    assert!(!ssc_carrier_roots()
        .lock()
        .keys()
        .any(|(k, _)| k.vm == VM_SSC));
    net_release_deferred(&mut ctx);
    assert_eq!(
        ctx.global_root_count(),
        roots_before,
        "the VM's next native call released the carrier's root"
    );
    assert_eq!(ssc_get(&ctx, carrier).cache_size, 7);

    // The carrier dies: its binding and its tuning row go.
    assert_eq!(gc_sweep_net_socket_rows(VM_SSC, &empty, &|_| false), 2);
    assert!(!ssc_owner_table().lock().keys().any(|k| k.vm == VM_SSC));
    assert!(!ssc_side_table().lock().keys().any(|(k, _)| k.vm == VM_SSC));
}

/// A carrier `get{Client,Server}SessionContext` never bound keys its
/// tuning off itself; that row now lives exactly as long as it does.
#[test]
fn an_orphan_carriers_tuning_goes_with_it() {
    let _teardown = Teardown(&[VM_ORPHAN]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_ORPHAN);
    let orphan = obj(ctx.new_object("javax/net/ssl/SSLSessionContext"));
    ssc_set(&ctx, orphan, |s| s.timeout_secs = 5);
    assert_eq!(ssc_key(&ctx, orphan).map(|k| k.1), Some(SSC_TAG_ORPHAN));
    assert_eq!(ssc_get(&ctx, orphan).timeout_secs, 5);
    let orphan_addr = addr(orphan);
    let empty = PointerMap::default();
    assert_eq!(
        gc_sweep_net_socket_rows(VM_ORPHAN, &empty, &|a| a == orphan_addr),
        0
    );
    assert_eq!(ssc_get(&ctx, orphan).timeout_secs, 5);
    assert_eq!(gc_sweep_net_socket_rows(VM_ORPHAN, &empty, &|_| false), 2);
    assert!(!ssc_side_table()
        .lock()
        .keys()
        .any(|(k, _)| k.vm == VM_ORPHAN));
}

thread_local! {
    /// What the SSL server socket under test answers to `isClosed()`.
    static SSS_CLOSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn sss_hook(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match name {
        "isClosed" => Some(Ok(Some(Value::Int(i32::from(
            SSS_CLOSED.with(std::cell::Cell::get),
        ))))),
        "close" => Some(Ok(None)),
        "getSoTimeout" => Some(Ok(Some(Value::Int(0)))),
        _ => None,
    }
}

/// An `SSLServerSocket` dropped open has its delegate root released by the
/// sweep; a closed one's forwarded call is answered and leaves no root.
#[test]
fn ssl_server_socket_delegates_are_released_on_close_and_on_death() {
    let _teardown = Teardown(&[VM_SSS]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_SSS);
    ctx.set_invoke_virtual_hook(sss_hook);
    let roots_before = ctx.global_root_count();

    SSS_CLOSED.with(|c| c.set(false));
    let open = obj(ctx.new_object("javax/net/ssl/SSLServerSocket"));
    let got = sss_option_call(
        &mut ctx,
        &[Value::Object(Some(open))],
        "getSoTimeout",
        "()I",
    )
    .expect("forwarded to the delegate");
    assert!(matches!(got, Some(Value::Int(0))));
    let open_key = mint_native_obj_key(&ctx, open);
    assert!(sss_option_delegates().lock().contains_key(&open_key));
    assert_eq!(ctx.global_root_count(), roots_before + 1);

    // Dropped without close().
    assert_eq!(
        gc_sweep_net_socket_rows(VM_SSS, &PointerMap::default(), &|_| false),
        1
    );
    assert!(!sss_option_delegates().lock().contains_key(&open_key));
    net_release_deferred(&mut ctx);
    assert_eq!(ctx.global_root_count(), roots_before);

    // A closed socket: answered, and nothing stays rooted or remembered.
    SSS_CLOSED.with(|c| c.set(true));
    let closed = obj(ctx.new_object("javax/net/ssl/SSLServerSocket"));
    sss_option_call(
        &mut ctx,
        &[Value::Object(Some(closed))],
        "getSoTimeout",
        "()I",
    )
    .expect("forwarded to a closed delegate");
    let closed_key = mint_native_obj_key(&ctx, closed);
    assert!(!sss_option_delegates().lock().contains_key(&closed_key));
    assert_eq!(
        ctx.global_root_count(),
        roots_before,
        "a closed SSLServerSocket leaves no delegate root"
    );
}

fn http_server_state(vm: usize, listener: Option<TcpListener>, port: i32) -> Arc<ServerState> {
    Arc::new(ServerState {
        listener: Mutex::new(listener),
        running: AtomicBool::new(false),
        handlers: Mutex::new(vec![HttpHandlerEntry {
            path_prefix: "/".to_string(),
            handler: at(0x16A0_5000),
            context: at(0x16A0_5100),
        }]),
        bound_port: AtomicI32::new(port),
        conns: OrderedPlMutex::new(HashMap::new(), LockLevel::Scratch),
        next_conn: AtomicU64::new(0),
        vm,
    })
}

/// The page's retire criterion: destroying a VM with a started, unstopped
/// `HttpServer` frees its port (a second VM in the process can bind it)
/// and leaves no registry row. Another VM's server is left alone.
#[test]
fn a_torn_down_vms_http_server_frees_its_port_and_its_row() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mine = next_server_id();
    let other = next_server_id();
    server_registry().lock().insert(
        mine,
        http_server_state(VM_HTTP, Some(listener), i32::from(port)),
    );
    server_registry()
        .lock()
        .insert(other, http_server_state(VM_HTTP_OTHER, None, -1));
    re10_start_server(mine).expect("the accept thread starts");
    request_queue().lock().insert(mine, Vec::new());

    assert_eq!(forget_vm_http_servers(VM_HTTP), 1);
    assert!(!server_registry().lock().contains_key(&mine));
    assert!(
        server_registry().lock().contains_key(&other),
        "another VM's server stays"
    );
    assert!(!request_queue().lock().contains_key(&mine));
    let rebound = TcpListener::bind(("127.0.0.1", port))
        .expect("the torn-down server's port must be free again");
    drop(rebound);
    assert_eq!(forget_vm_http_servers(VM_HTTP), 0, "idempotent");

    server_registry().lock().remove(&other);
}

/// A closed `HttpClient`'s shutdown row goes with the client, so a later
/// client that is handed its identity hash does not report
/// `isTerminated()` before anyone closed it.
#[test]
fn a_closed_http_clients_shutdown_row_goes_with_the_client() {
    let _teardown = Teardown(&[VM_CLIENT]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_CLIENT);
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    let hc = "java/net/http/HttpClient";
    let close = registry
        .find(hc, "close", "()V")
        .expect("HttpClient.close is registered");
    let is_terminated = registry
        .find(hc, "isTerminated", "()Z")
        .expect("HttpClient.isTerminated is registered");
    let client = obj(ctx.new_object(hc));
    let args = [Value::Object(Some(client))];
    assert!(matches!(
        is_terminated(&mut ctx, &args),
        Ok(Some(Value::Int(0)))
    ));
    close(&mut ctx, &args).expect("close succeeds");
    assert!(matches!(
        is_terminated(&mut ctx, &args),
        Ok(Some(Value::Int(1)))
    ));

    let client_addr = addr(client);
    let empty = PointerMap::default();
    assert_eq!(
        gc_sweep_net_socket_rows(VM_CLIENT, &empty, &|a| a == client_addr),
        0
    );
    assert!(matches!(
        is_terminated(&mut ctx, &args),
        Ok(Some(Value::Int(1)))
    ));
    assert_eq!(gc_sweep_net_socket_rows(VM_CLIENT, &empty, &|_| false), 1);
    let client_key = mint_native_obj_key(&ctx, client);
    assert!(!re5_client_shutdown_table().lock().contains_key(&client_key));
}

/// Teardown drops the VM's queued sweep work and stream addresses, and no
/// other VM's.
#[test]
fn teardown_drops_the_vms_deferred_work_and_stream_addresses() {
    let _teardown = Teardown(&[VM_TEARDOWN, VM_TEARDOWN_OTHER]);
    let stream = NativeObjKey {
        vm: VM_TEARDOWN,
        key: 0x16A_0010,
    };
    let owner = NativeObjKey {
        vm: VM_TEARDOWN,
        key: 0x16A_0020,
    };
    stream_owner_set_at(stream, owner, 0x16A0_7000);
    {
        let mut deferred = net_deferred().lock();
        deferred.roots.entry(VM_TEARDOWN).or_default().push(42);
        deferred
            .roots
            .entry(VM_TEARDOWN_OTHER)
            .or_default()
            .push(43);
        deferred.streams.push((VM_TEARDOWN, 0x3FFF_16A3));
    }
    forget_vm_socket_side_tables(VM_TEARDOWN);
    assert!(!stream_owner_table().lock().addrs.contains_key(&stream));
    let deferred = net_deferred().lock();
    assert!(!deferred.roots.contains_key(&VM_TEARDOWN));
    assert!(deferred.roots.contains_key(&VM_TEARDOWN_OTHER));
    assert!(!deferred.streams.iter().any(|&(vm, _)| vm == VM_TEARDOWN));
    drop(deferred);
}
