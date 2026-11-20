use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;

// One VM identity per test: each test's teardown drops every row of its
// VM, and the tests run in parallel.
const VM: usize = 0x15A0_0401;
const VM_REUSED_KEY: usize = 0x15A0_0402;
const VM_TEARDOWN: usize = 0x15A0_0403;

fn key(vm: usize, key: usize) -> NativeObjKey {
    NativeObjKey { vm, key }
}

fn obj(v: MethodCallResult) -> ObjectRef {
    match v {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    }
}

#[test]
fn socket_streams_take_no_global_root_and_close_drops_their_rows() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let sock = obj(ctx.new_object("java/net/Socket"));
    // A plain (non-TLS) stream id that no s2 socket of a concurrently
    // running test can hold: ids are minted upward from 1.
    sock_set(&ctx, sock, |s| s.stream_id = 0x3FFF_15A1);
    let owner = mint_native_obj_key(&ctx, sock);

    let mut registry = NativeMethodRegistry::new();
    // As a bridge: the surface states no kind of its own, so it would be
    // registered under the default `SyntheticStub` and dropped whenever
    // the process runs with synthetic stubs disabled.
            // And the registry's own real-sockets filter drops EVERY
    // `java/net/Socket` native, bridge or not, unless told this is a test
    // registry that wants the synthetic surface (gc-common w17
    // verification: the bridge tag alone still found nothing).
    registry.allow_synthetic_net_sockets(true);
    // gc-common w16-a: the ungated surface. `register_re1_socket` returns
    // early under the default `real_net_sockets`, so this test used to
    // find nothing registered and fail at the `expect` below.
    register_re1_socket_surface(&mut registry);
    let get_in = registry
        .find(
            "java/net/Socket",
            "getInputStream",
            "()Ljava/io/InputStream;",
        )
        .expect("Socket.getInputStream is registered");
    let get_out = registry
        .find(
            "java/net/Socket",
            "getOutputStream",
            "()Ljava/io/OutputStream;",
        )
        .expect("Socket.getOutputStream is registered");
    let in_close = registry
        .find("java/net/Socket$SocketInputStream", "close", "()V")
        .expect("SocketInputStream.close is registered");

    let roots_before = ctx.global_root_count();
    let args = [Value::Object(Some(sock))];
    let in1 = obj(get_in(&mut ctx, &args));
    let in2 = obj(get_in(&mut ctx, &args));
    let out = obj(get_out(&mut ctx, &args));
    assert_eq!(
        ctx.global_root_count(),
        roots_before,
        "a socket stream must not root its Socket (it used to, until the VM exited)"
    );
    for s in [in1, in2, out] {
        assert_eq!(stream_owner_get(&ctx, s), Some(owner));
    }

    // Closing either stream closes the socket and drops every row.
    in_close(&mut ctx, &[Value::Object(Some(in1))]).expect("close succeeds");
    for s in [in1, in2, out] {
        assert_eq!(
            stream_owner_get(&ctx, s),
            None,
            "a closed socket keeps no stream row"
        );
    }
    assert!(!stream_owner_table().lock().by_owner.contains_key(&owner));
    assert_eq!(sock_get(&ctx, sock).closed, 1);
    assert_eq!(sock_get(&ctx, sock).stream_id, -1);

    // A read through a stream of the closed socket fails, as on HotSpot
    // (`SocketException: Socket closed`), rather than finding a row.
    assert!(
        native_socket_input_stream_read_one(&mut ctx, &[Value::Object(Some(in2))]).is_err()
    );
    // A second close is a no-op.
    in_close(&mut ctx, &[Value::Object(Some(in1))]).expect("a second close is a no-op");

    forget_vm_socket_side_tables(VM);
}

/// A dead stream's identity hash reused by a stream of another socket:
/// the first socket's close must not drop the second socket's row.
#[test]
fn a_reused_stream_key_follows_its_new_owner() {
    let (stream, first, second) = (
        key(VM_REUSED_KEY, 0x15A_0010),
        key(VM_REUSED_KEY, 0x15A_0020),
        key(VM_REUSED_KEY, 0x15A_0030),
    );
    stream_owner_set(stream, first);
    stream_owner_set(stream, second);
    {
        let t = stream_owner_table().lock();
        assert_eq!(t.by_stream.get(&stream), Some(&second));
        assert!(
            !t.by_owner.contains_key(&first),
            "the old owner no longer lists it"
        );
    }
    stream_owners_forget_owner(first);
    assert_eq!(
        stream_owner_table().lock().by_stream.get(&stream),
        Some(&second)
    );
    stream_owners_forget_owner(second);
    let t = stream_owner_table().lock();
    assert!(!t.by_stream.contains_key(&stream));
    assert!(!t.by_owner.contains_key(&second));
}

/// `forget_vm_socket_side_tables` now also drops the VM's recorded
/// `HttpsURLConnection` handshakes and probe-socket restrictions, and
/// leaves another VM's alone.
#[test]
fn teardown_drops_https_carrier_sessions_and_probe_restrictions() {
    let mine = key(VM_TEARDOWN, 0x15A_0040);
    let other = key(VM_TEARDOWN + 0x100, 0x15A_0040);
    for k in [mine, other] {
        https_carrier_sessions().lock().insert(
            k,
            HttpsCarrierSession {
                peer_chain_der: vec![vec![0x30, 0x03, 0x02, 0x01, 0x01]],
                ..HttpsCarrierSession::default()
            },
        );
        probe_restrictions()
            .lock()
            .insert(k, (vec!["TLS_AES_128_GCM_SHA256".to_string()], Vec::new()));
    }
    forget_vm_socket_side_tables(VM_TEARDOWN);
    assert!(!https_carrier_sessions().lock().contains_key(&mine));
    assert!(!probe_restrictions().lock().contains_key(&mine));
    assert!(https_carrier_sessions().lock().contains_key(&other));
    assert!(probe_restrictions().lock().contains_key(&other));
    forget_vm_socket_side_tables(VM_TEARDOWN + 0x100);
    assert!(!https_carrier_sessions().lock().contains_key(&other));
}
