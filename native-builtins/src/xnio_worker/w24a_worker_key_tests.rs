use super::*;
use crate::test_utils::MockNativeContext;
use crate::xnio_conduits::{drop_sink_channel, drop_source_channel, Pipe};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test uses them.
const W24A_VM_A: usize = 0x0A24_9A08;
const W24A_VM_B: usize = 0x0A24_9B08;
const W24A_VM_C: usize = 0x0A24_9C08;
const W24A_VM_D: usize = 0x0A24_9D08;
const W24A_VM_E: usize = 0x0A24_9E08;

struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_xnio_worker_rows(vm);
            crate::xnio_conduits::forget_vm_xnio_conduit_rows(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the key registry and the mock hash the address
    // and never dereference it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn peer(port: u16) -> Option<SocketAddr> {
    Some(SocketAddr::from(([127, 0, 0, 1], port)))
}

fn address_rows_of(vm: usize) -> usize {
    stream_connection_address_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|(owner, _)| *owner == vm)
        .count()
}

/// A connection's address row outlives its close -- `getPeerAddress`
/// keeps answering while the `StreamConnection` lives -- and goes at the
/// first sweep that finds the connection object dead. It used to stay
/// until VM teardown: one row per connection ever accepted.
#[test]
fn w24a_an_address_row_outlives_the_close_and_goes_with_the_connection() {
    let _forget = Forget(&[W24A_VM_A]);
    let mut a = ctx_for(W24A_VM_A);
    let conn = a.fresh_object_ref();
    remember_stream_connection_addresses(&a, conn, peer(8080), peer(40001));

    // The connection's two halves close (the shutdown natives' path).
    let source_id = crate::xnio_conduits::register_source_channel(
        crate::xnio_conduits::ConduitTransport::Pipe(Arc::new(Pipe::new(16))),
    );
    let sink_id = crate::xnio_conduits::register_sink_channel(
        crate::xnio_conduits::ConduitTransport::Pipe(Arc::new(Pipe::new(16))),
    );
    drop_source_channel(&mut a, source_id);
    drop_sink_channel(&mut a, sink_id);
    let after_close = stream_connection_addresses(&a, conn).and_then(|x| x.peer);
    assert_eq!(
        after_close,
        peer(40001),
        "a closed connection keeps its peer"
    );
    assert_eq!(address_rows_of(W24A_VM_A), 1);

    let conn_addr = conn.as_ptr() as usize;
    assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_A, &|x| x != conn_addr), 1);
    assert_eq!(
        address_rows_of(W24A_VM_A),
        0,
        "the row goes with the object"
    );
    assert!(stream_connection_addresses(&a, conn).is_none());
}

/// A dead connection's addresses are not answered for a later object
/// carrying its identity hash (the old `(vm, identity hash)` key did).
#[test]
fn w24a_a_dead_connections_addresses_are_not_found_by_a_collider() {
    let _forget = Forget(&[W24A_VM_B]);
    let b = ctx_for(W24A_VM_B);
    let dead = 0x1_A249_1000usize;
    let newcomer = 0x2_A249_1000usize;
    assert_eq!(
        b.identity_hash_code(obj(dead)),
        b.identity_hash_code(obj(newcomer)),
        "premise: the two objects share an identity hash"
    );
    remember_stream_connection_addresses(&b, obj(dead), None, peer(40002));
    assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_B, &|x| x != dead), 1);
    assert!(stream_connection_addresses(&b, obj(newcomer)).is_none());

    remember_stream_connection_addresses(&b, obj(newcomer), None, peer(40003));
    let answered = stream_connection_addresses(&b, obj(newcomer)).and_then(|x| x.peer);
    assert_eq!(answered, peer(40003));
}

/// Two VMs filing one address keep their own rows; one VM's sweep costs
/// the other nothing.
#[test]
fn w24a_two_vms_keep_their_own_address_rows() {
    let _forget = Forget(&[W24A_VM_C, W24A_VM_D]);
    let c = ctx_for(W24A_VM_C);
    let d = ctx_for(W24A_VM_D);
    let addr = 0x1_A24A_1000usize;
    remember_stream_connection_addresses(&c, obj(addr), None, peer(40004));
    remember_stream_connection_addresses(&d, obj(addr), None, peer(40005));
    let c_peer = stream_connection_addresses(&c, obj(addr)).and_then(|x| x.peer);
    let d_peer = stream_connection_addresses(&d, obj(addr)).and_then(|x| x.peer);
    assert_eq!(c_peer, peer(40004));
    assert_eq!(d_peer, peer(40005));

    crate::gc_sweep_lock_keys(W24A_VM_C, &|x| x != addr);
    assert!(stream_connection_addresses(&c, obj(addr)).is_none());
    let d_peer = stream_connection_addresses(&d, obj(addr)).and_then(|x| x.peer);
    assert_eq!(d_peer, peer(40005));
    assert_eq!(address_rows_of(W24A_VM_D), 1);
}

/// A worker mirror's row goes with the mirror, and the worker is still
/// shut down at its VM's teardown (teardown used to find it through that
/// row alone).
#[test]
fn w24a_a_dead_mirrors_worker_is_still_shut_down_with_its_vm() {
    let _forget = Forget(&[W24A_VM_E]);
    let e = ctx_for(W24A_VM_E);
    let mirror = 0x1_A24B_1000usize;
    let worker = XnioWorker::new(
        "w24a_dead_mirror",
        OptionMap {
            worker_io_threads: Some(1),
            worker_task_core_threads: Some(1),
        },
    );
    register_worker(worker.clone());
    remember_worker_object(&e, obj(mirror), &worker);
    assert!(lookup_worker_object(&e, obj(mirror)).is_some());

    assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_E, &|x| x != mirror), 1);
    assert!(lookup_worker_object(&e, obj(mirror)).is_none());
    let rows = worker_object_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_key
        .values()
        .filter(|(owner, _)| *owner == W24A_VM_E)
        .count();
    assert_eq!(rows, 0, "the mirror's row goes with the mirror");
    assert!(lookup_worker(worker.id).is_some());

    forget_vm_xnio_worker_rows(W24A_VM_E);
    assert!(lookup_worker(worker.id).is_none());
    assert!(worker.is_shutdown(), "the VM's teardown still finds it");
    assert!(worker.await_termination(Duration::from_secs(2)));
}
