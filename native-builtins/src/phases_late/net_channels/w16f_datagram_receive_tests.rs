//! gc-common w16-f, the retire criterion of
//! `common-w15f-synthetic-datagram-receive-blocks-outside-a-blocking-region-FIXED-20260923`:
//! a blocking `DatagramChannel.receive` parks in a GC-blocking region and
//! WITHOUT the process-wide `s2_registry()` lock, so another thread can
//! take that lock (for a send, or a `close()`) while it waits.
//!
//! Every wait is bounded: the registry socket carries a 3 s read timeout,
//! so on a regression (the receive holding the lock) the receive gives up
//! on its own, the other thread gets the lock late, and the assertions
//! fail instead of the test hanging.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::time::Duration;

const DC: &str = "java/nio/channels/DatagramChannel";
const RECEIVE: &str = "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;";

/// A blocking channel over a fresh loopback socket, and a 16-byte
/// `ByteBuffer` (slot 0 = `byte[]`, 1 = position, 2 = limit).
fn channel_and_buffer(
    ctx: &mut MockNativeContext,
    vm: usize,
) -> (ObjectRef, ObjectRef, i32, std::net::SocketAddr) {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind loopback");
    sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let addr = sock.local_addr().unwrap();
    let sid = crate::servlet::s2_alloc_dgram_for_vm(vm, sock);
    let ch = ctx.fresh_object_ref();
    ctx.set_field(ch, 0, Value::Int(i32::from(addr.port())));
    ctx.set_field(ch, 1, Value::Int(1));
    ctx.set_field(ch, 2, Value::Int(0));
    ctx.set_field(ch, 3, Value::Int(1)); // blocking
    ctx.set_field(ch, 4, Value::Int(sid));
    let bb = ctx.fresh_object_ref();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    ctx.set_field(bb, 0, Value::Object(Some(arr)));
    ctx.set_field(bb, 1, Value::Int(0));
    ctx.set_field(bb, 2, Value::Int(16));
    (ch, bb, sid, addr)
}

fn receive_native() -> cratonvm_native_api::NativeCallback {
    let mut r = NativeMethodRegistry::new();
    register_datagram_channel(&mut r);
    r.find(DC, "receive", RECEIVE)
        .expect("receive is registered")
}

#[test]
fn w16f_blocking_receive_parks_gc_blocked_and_without_the_registry_lock() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF16_0201);
    let (ch, bb, sid, addr) = channel_and_buffer(&mut ctx, 0xF16_0201);
    let receive = receive_native();

    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        // The lock a parked receive used to hold. Taken and released
        // before the datagram goes out, so the send can only happen once
        // the receiver has let go of it.
        drop(crate::servlet::s2_registry().lock());
        let out = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        out.send_to(b"w16f", addr).unwrap();
    });
    let result = receive(
        &mut ctx,
        &[Value::Object(Some(ch)), Value::Object(Some(bb))],
    );
    sender.join().unwrap();
    crate::servlet::s2_registry().lock().dgrams.remove(&sid);

    assert!(
        matches!(result, Ok(Some(Value::Object(Some(_))))),
        "the datagram sent after taking the registry lock must be received"
    );
    assert_eq!(ctx.get_field(bb, 1).as_int(), Some(4), "position advanced");
    assert_eq!(
        ctx.blocking_region_counts(),
        (1, 1),
        "a blocking receive must park GC-blocked"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn w16f_closing_the_channel_ends_a_parked_receive() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF16_0202);
    let (ch, bb, sid, _addr) = channel_and_buffer(&mut ctx, 0xF16_0202);
    let receive = receive_native();

    let closer = std::thread::spawn(move || {
        // Long enough for the receive to have taken its duplicate and
        // parked; the close then has to reach it through the poll.
        std::thread::sleep(Duration::from_millis(250));
        // What `DatagramChannel.close()` does to the registry.
        crate::servlet::s2_registry().lock().dgrams.remove(&sid);
    });
    let result = receive(
        &mut ctx,
        &[Value::Object(Some(ch)), Value::Object(Some(bb))],
    );
    closer.join().unwrap();

    match result {
        Err(e) => assert!(
            e.to_string().contains("closed"),
            "a close must end the wait as a closed channel, not a timeout: {e}"
        ),
        Ok(v) => panic!("a close must end the wait with an IOException, got {v:?}"),
    }
    assert_eq!(ctx.blocking_region_counts(), (1, 1));
}
