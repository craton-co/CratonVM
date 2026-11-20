use super::*;

const VM_A: usize = 0xA11D_0001;
const VM_B: usize = 0xA11D_0002;

#[test]
fn the_key_manager_cache_is_torn_down_with_its_vm() {
    let ih = 0x0A11_D100;
    kmf_key_manager_cache().lock().insert((VM_A, ih), (7, 70));
    kmf_key_manager_cache().lock().insert((VM_B, ih), (8, 80));
    forget_vm_factory_identity_rows(VM_A);
    assert!(!kmf_key_manager_cache().lock().contains_key(&(VM_A, ih)));
    assert_eq!(
        kmf_key_manager_cache().lock().get(&(VM_B, ih)).copied(),
        Some((8, 80)),
        "another VM's row with the same identity hash is left alone"
    );
    forget_vm_factory_identity_rows(VM_B);
    assert!(!kmf_key_manager_cache().lock().contains_key(&(VM_B, ih)));
}
