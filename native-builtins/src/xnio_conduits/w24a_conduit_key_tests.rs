use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test uses them.
const W24A_VM_A: usize = 0x0A24_7A08;
const W24A_VM_B: usize = 0x0A24_7B08;
const W24A_VM_C: usize = 0x0A24_7C08;
const W24A_VM_D: usize = 0x0A24_7D08;

struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_xnio_conduit_rows(vm);
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

fn new_sink() -> u64 {
    register_sink_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(16))))
}

/// A dead channel's rows are not found by a later object carrying its
/// identity hash: the old `(vm, identity hash)` key answered the dead
/// channel's id here, and every native on the newcomer then read from or
/// wrote to the dead connection's socket.
#[test]
fn w24a_a_dead_channels_rows_are_not_found_by_an_object_with_its_hash() {
    let _forget = Forget(&[W24A_VM_A]);
    let a = ctx_for(W24A_VM_A);
    let dead = 0x1_A240_1000usize;
    let newcomer = 0x2_A240_1000usize;
    assert_eq!(
        a.identity_hash_code(obj(dead)),
        a.identity_hash_code(obj(newcomer)),
        "premise: the two objects share an identity hash"
    );
    let dead_id = new_sink();
    remember_sink_obj(&a, obj(dead), dead_id);
    assert_eq!(sink_id_by_obj(&a, obj(dead)), Some(dead_id));

    assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_A, &|x| x != dead), 1);
    assert_eq!(
        sink_id_by_obj(&a, obj(newcomer)),
        None,
        "the dead channel's row is not the newcomer's"
    );

    let new_id = new_sink();
    remember_sink_obj(&a, obj(newcomer), new_id);
    assert_eq!(sink_id_by_obj(&a, obj(newcomer)), Some(new_id));
}

/// A channel the collector moved keeps its rows (the key registry
/// re-addresses its slot), and a lookup of an object that was never filed
/// leaves no key-registry slot behind.
#[test]
fn w24a_a_moved_channel_keeps_its_rows_and_lookups_do_not_mint() {
    let _forget = Forget(&[W24A_VM_B]);
    let b = ctx_for(W24A_VM_B);
    let old = 0x1_A241_1000usize;
    // Same low 32 bits: a real object keeps its identity hash across a
    // move, and the mock's hash is the address truncated to `i32`.
    let new = 0x3_A241_1000usize;
    let id = new_sink();
    remember_sink_obj(&b, obj(old), id);

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    assert_eq!(
        crate::gc_sweep_and_remap_lock_keys(W24A_VM_B, &|x| x == old, &moved),
        0
    );
    assert_eq!(sink_id_by_obj(&b, obj(new)), Some(id));

    let stranger = 0x1_A241_3000usize;
    assert_eq!(sink_id_by_obj(&b, obj(stranger)), None);
    assert_eq!(source_id_by_obj(&b, obj(stranger)), None);
    assert_eq!(crate::existing_weak_lock_key(&b, obj(stranger)), None);
}

/// Two VMs filing one address keep their own rows, and one VM's sweep
/// finding the object dead costs the other VM nothing.
#[test]
fn w24a_two_vms_keep_their_own_channel_rows() {
    let _forget = Forget(&[W24A_VM_C, W24A_VM_D]);
    let c = ctx_for(W24A_VM_C);
    let d = ctx_for(W24A_VM_D);
    let addr = 0x1_A242_1000usize;
    let c_id = new_sink();
    let d_id = new_sink();
    remember_sink_obj(&c, obj(addr), c_id);
    remember_sink_obj(&d, obj(addr), d_id);
    assert_eq!(sink_id_by_obj(&c, obj(addr)), Some(c_id));
    assert_eq!(sink_id_by_obj(&d, obj(addr)), Some(d_id));

    crate::gc_sweep_lock_keys(W24A_VM_C, &|x| x != addr);
    assert_eq!(sink_id_by_obj(&c, obj(addr)), None);
    assert_eq!(sink_id_by_obj(&d, obj(addr)), Some(d_id));
}
