use super::*;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for vm in self.0 {
            crate::forget_vm_lock_keys(*vm);
        }
    }
}

fn row(key: usize) -> Option<Vec<u8>> {
    let t = match x500_der_table().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    t.get(&key).cloned()
}

/// Two VMs' principals with one identity hash keep their own DER, a dead
/// principal's row goes with its key (the table used to be capped and
/// evicted LIVE rows instead), and a moved one keeps its row.
#[test]
fn der_rows_are_per_vm_die_with_the_principal_and_survive_a_move() {
    const VM_A: usize = 0xE18F_0001;
    const VM_B: usize = 0xE18F_0002;
    let _forget = Forget(&[VM_A, VM_B]);
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let (pa, pb) = (obj(0x1_E198_0010), obj(0x2_E198_0010));
    remember_der(&mut a, pa, b"\x30\x00A");
    remember_der(&mut b, pb, b"\x30\x00B");
    let (ka, kb) = (x500_der_key(&a, pa), x500_der_key(&b, pb));
    assert_ne!(ka, kb);
    assert_eq!(row(ka).as_deref(), Some(&b"\x30\x00A"[..]));
    assert_eq!(row(kb).as_deref(), Some(&b"\x30\x00B"[..]));

    let dead = pa.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM_A, &|x| x != dead) >= 1);
    assert_eq!(row(ka), None, "the dead principal's DER outlived it");
    assert!(row(kb).is_some(), "A's sweep took B's row");

    let (old, new) = (pb.as_ptr() as usize, 0x4_E198_0010usize);
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM_B, &|x| x == old, &moved);
    assert_eq!(
        x500_der_key(&b, obj(new)),
        kb,
        "the moved principal lost its key"
    );
    assert_eq!(row(kb).as_deref(), Some(&b"\x30\x00B"[..]));
}
