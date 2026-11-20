use super::*;

const VM_A: usize = 0x7A80_0401;
const VM_B: usize = 0x7A80_0402;
const VM_HTTP_ROOTS: usize = 0x7A80_0403;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: used only as a map key / `as_ptr()` operand, never read.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn moved(from: usize, to: usize) -> cratonvm_types::PointerMap {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(from, to);
    map
}

#[test]
fn http_handler_roots_belong_to_the_creating_vm() {
    let (handler, context) = (at(0x7A80_1000), at(0x7A80_2000));
    let id = next_server_id();
    let state = std::sync::Arc::new(ServerState {
        listener: Mutex::new(None),
        running: AtomicBool::new(false),
        handlers: Mutex::new(vec![HttpHandlerEntry {
            path_prefix: "/".to_string(),
            handler,
            context,
        }]),
        bound_port: AtomicI32::new(-1),
        conns: OrderedPlMutex::new(HashMap::new(), LockLevel::Scratch),
        next_conn: AtomicU64::new(0),
        // gc-common w16-a: its own VM. `forget_vm_socket_side_tables` now
        // tears down a VM's `HttpServer` rows too, and the datagram test
        // below runs it for `VM_A` in parallel with this one.
        vm: VM_HTTP_ROOTS,
    });
    server_registry().lock().insert(id, state.clone());
    let scan = |vm: usize| {
        let mut out = Vec::new();
        gc_scan_re10_handler_roots(vm, &mut out);
        out
    };
    assert!(scan(VM_HTTP_ROOTS).contains(&handler) && scan(VM_HTTP_ROOTS).contains(&context));
    assert!(!scan(VM_B).contains(&handler) && !scan(VM_B).contains(&context));

    let map = moved(0x7A80_1000, 0x7A80_3000);
    gc_update_re10_handler_refs(VM_B, &map);
    assert_eq!(
        state.handlers.lock()[0].handler,
        handler,
        "B's map must not rewrite A's handler"
    );
    gc_update_re10_handler_refs(VM_HTTP_ROOTS, &map);
    assert_eq!(state.handlers.lock()[0].handler, at(0x7A80_3000));

    server_registry().lock().remove(&id);
}

#[test]
fn datagram_socket_and_inet_address_rows_belong_to_the_writing_vm() {
    let (sock, addr) = (at(0x7A80_4000), at(0x7A80_5000));
    ds_set(VM_A, sock, |d| d.fd = 21);
    ds_set_peer(VM_A, sock, "10.1.2.3", 4242);
    inet_addr_set(VM_A, addr, "w8a.example", "10.9.8.7");
    let scan = |vm: usize| {
        let mut out = Vec::new();
        gc_scan_ds_roots(vm, &mut out);
        gc_scan_inet_addr_roots(vm, &mut out);
        out
    };
    // gc-common w17-a: the rows are weak, so neither VM's collection roots
    // them (the owner-death sweep drops a dead key's rows instead).
    assert!(scan(VM_A).is_empty(), "A's rows root nothing");
    assert!(
        scan(VM_B).is_empty(),
        "VM B's collection must not root VM A's socket or address mirrors"
    );

    // B's pointer map names both keys; neither row is re-keyed by it.
    gc_update_ds_refs(VM_B, &moved(0x7A80_4000, 0x7A80_6000));
    gc_update_inet_addr_refs(VM_B, &moved(0x7A80_5000, 0x7A80_7000));
    assert_eq!(ds_get(sock).fd, 21);
    assert_eq!(ds_peer(sock), Some(("10.1.2.3".to_string(), 4242)));
    assert_eq!(
        inet_addr_get_raw(addr),
        Some(("w8a.example".to_string(), "10.9.8.7".to_string()))
    );

    // A's own map does.
    gc_update_ds_refs(VM_A, &moved(0x7A80_4000, 0x7A80_6000));
    gc_update_inet_addr_refs(VM_A, &moved(0x7A80_5000, 0x7A80_7000));
    assert_eq!(ds_get(at(0x7A80_6000)).fd, 21);
    assert!(inet_addr_get_raw(at(0x7A80_7000)).is_some());
    assert!(inet_addr_get_raw(addr).is_none());

    forget_vm_socket_side_tables(VM_A);
    assert_eq!(ds_get(at(0x7A80_6000)).fd, -1, "teardown drops A's rows");
    assert_eq!(ds_peer(at(0x7A80_6000)), None);
    assert!(inet_addr_get_raw(at(0x7A80_7000)).is_none());
}
