use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;

struct ForgetVm(usize);
impl Drop for ForgetVm {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
        forget_vm_rwl_state(self.0);
    }
}

#[test]
fn rwl_states_of_hash_colliders_stay_apart_and_die_with_their_lock() {
    const VM: usize = 0xB29_3001;
    let _g = ForgetVm(VM);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    // SAFETY: keys only, never dereferenced.
    let (a, b) = unsafe {
        (
            ObjectRef::from_raw(0x1_B29F_0000usize as *mut u8),
            ObjectRef::from_raw(0x2_B29F_0000usize as *mut u8),
        )
    };
    assert_eq!(c.identity_hash_code(a), c.identity_hash_code(b), "premise");
    let ka = rl_key(&mut c, a);
    rwl_with(ka, |st| st.readers = 3);
    assert_eq!(rwl_get(rl_key(&mut c, b)).readers, 0, "b saw a's readers");
    assert_eq!(rwl_get(ka).readers, 3);

    let b_addr = b.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|x| x == b_addr);
    assert!(
        !rwl_state_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&ka),
        "the dead lock's read holds survived"
    );
}

/// The `toString()` renderings carry the identity hash `Object.toString`
/// prints, not the heap address.
#[test]
fn to_string_renders_the_identity_hash_not_the_address() {
    const VM: usize = 0xB29_3002;
    let _g = ForgetVm(VM);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    let lock = c.fresh_object_ref();
    let hash = c.identity_hash_code(lock) as u32;
    let Ok(Some(Value::Object(Some(s)))) =
        native_rl_to_string(&mut c, &[Value::Object(Some(lock))])
    else {
        panic!("toString answered no string");
    };
    let text = c.read_string(s).unwrap_or_default();
    assert!(
        text.starts_with(&format!(
            "java.util.concurrent.locks.ReentrantLock@{hash:x}["
        )),
        "rendered {text:?}"
    );
}
