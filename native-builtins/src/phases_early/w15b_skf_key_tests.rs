use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

/// Forget the VMs' keys (and so their rows) even if an assertion fails.
struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for vm in self.0 {
            crate::forget_vm_lock_keys(*vm);
        }
    }
}

fn prf_of(ctx: &dyn NativeContext, f: ObjectRef) -> Option<i32> {
    let key = pbkdf2_key_for(ctx, f);
    pbkdf2_prf_table().lock().unwrap().get(&key).copied()
}

fn algorithm_of(ctx: &mut MockNativeContext, f: ObjectRef) -> Option<String> {
    match pbkdf2_get_algorithm(ctx, &[Value::Object(Some(f))]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// The page's retire test: VM A's `PBKDF2WithHmacSHA1` factory and VM B's
/// `PBKDF2WithHmacSHA512` factory with one identity hash. The old registry
/// handed B A's key, B's `getInstance` overwrote A's PRF, and A derived its
/// keys with SHA-512.
#[test]
fn two_vms_factories_with_one_identity_hash_keep_distinct_prfs() {
    const VM_A: usize = 0xB15F_0001;
    const VM_B: usize = 0xB15F_0002;
    let _forget = Forget(&[VM_A, VM_B]);
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let fa = obj(0x1_B15F_1000);
    let fb = obj(0x2_B15F_1000);
    assert_eq!(a.identity_hash_code(fa), b.identity_hash_code(fb));

    let sha1 = pbkdf2_prf_code("PBKDF2WithHmacSHA1");
    let sha512 = pbkdf2_prf_code("PBKDF2WithHmacSHA512");
    assert!(sha1.is_some() && sha512.is_some() && sha1 != sha512);
    note_skf_factory(&a, fa, sha1, "PBKDF2WithHmacSHA1".to_string());
    note_skf_factory(&b, fb, sha512, "PBKDF2WithHmacSHA512".to_string());

    assert_ne!(pbkdf2_key_for(&a, fa), pbkdf2_key_for(&b, fb));
    assert_eq!(
        prf_of(&a, fa),
        sha1,
        "VM A's factory lost its PRF to VM B's"
    );
    assert_eq!(prf_of(&b, fb), sha512);
    assert_eq!(
        algorithm_of(&mut a, fa).as_deref(),
        Some("PBKDF2WithHmacSHA1")
    );
    assert_eq!(
        algorithm_of(&mut b, fb).as_deref(),
        Some("PBKDF2WithHmacSHA512")
    );
    assert!(skf_receiver_is_ours(&mut a, fa) && skf_receiver_is_ours(&mut b, fb));

    // A PBE factory in A next to them keeps its own rows.
    let pbe = obj(0x3_B15F_2000);
    note_skf_factory(&a, pbe, None, "PBEWithMD5AndDES".to_string());
    let kp = pbkdf2_key_for(&a, pbe);
    assert_eq!(
        pbe_algo_table()
            .lock()
            .unwrap()
            .get(&kp)
            .map(String::as_str),
        Some("PBEWithMD5AndDES")
    );
    assert_eq!(prf_of(&a, pbe), None);

    // Tearing A down takes A's rows only.
    let (ka, kb) = (pbkdf2_key_for(&a, fa), pbkdf2_key_for(&b, fb));
    crate::forget_vm_lock_keys(VM_A);
    assert!(!pbkdf2_prf_table().lock().unwrap().contains_key(&ka));
    assert!(!skf_algo_table().lock().unwrap().contains_key(&ka));
    assert!(!pbe_algo_table().lock().unwrap().contains_key(&kp));
    assert_eq!(pbkdf2_prf_table().lock().unwrap().get(&kb).copied(), sha512);
}

/// A dead factory's three rows go at the sweep that finds it dead, and a
/// later object with its identity hash is not taken for one of ours (the
/// old registry rebound the dead slot to it, rows and all).
#[test]
fn a_dead_factorys_rows_go_with_its_key() {
    const VM: usize = 0xB15F_0003;
    let _forget = Forget(&[VM]);
    let mut c = ctx_for(VM);
    let dead = obj(0x1_B15F_3000);
    let live = obj(0x1_B15F_4000);
    let reborn = obj(0x2_B15F_3000);
    for (f, alg) in [(dead, "PBKDF2WithHmacSHA256"), (live, "PBKDF2WithHmacSHA1")] {
        note_skf_factory(&c, f, pbkdf2_prf_code(alg), alg.to_string());
    }
    let kd = pbkdf2_key_for(&c, dead);
    let live_addr = live.as_ptr() as usize;

    assert_eq!(crate::gc_sweep_lock_keys(VM, &|a| a == live_addr), 1);
    assert!(!pbkdf2_prf_table().lock().unwrap().contains_key(&kd));
    assert!(!skf_algo_table().lock().unwrap().contains_key(&kd));
    assert_eq!(prf_of(&c, live), pbkdf2_prf_code("PBKDF2WithHmacSHA1"));

    assert_ne!(pbkdf2_key_for(&c, reborn), kd);
    assert!(!skf_receiver_is_ours(&mut c, reborn));
    assert_eq!(prf_of(&c, reborn), None);
}

/// `UTF_8.encode(CharBuffer)`, as SunJCE's `PBKDF2KeyImpl` does: a
/// surrogate pair is one 4-byte sequence and an unpaired surrogate is
/// `'?'`. Both used to be dropped.
#[test]
fn pbkdf2_password_bytes_encode_like_sunjce() {
    assert_eq!(pbkdf2_password_bytes(&[0x70, 0x77]), b"pw".to_vec());
    assert_eq!(pbkdf2_password_bytes(&[0xE9]), "\u{e9}".as_bytes().to_vec());
    // U+1F600 as a surrogate pair.
    assert_eq!(
        pbkdf2_password_bytes(&[0x61, 0xD83D, 0xDE00]),
        vec![0x61, 0xF0, 0x9F, 0x98, 0x80]
    );
    // A lone high and a lone low surrogate.
    assert_eq!(
        pbkdf2_password_bytes(&[0xD83D, 0x61, 0xDE00]),
        b"?a?".to_vec()
    );
    assert!(pbkdf2_password_bytes(&[]).is_empty());
}

/// SunJCE's `PBKDF2KeyImpl` checks, in order. `PBEKeySpec(char[])` (no
/// salt, zero counts) reaches the first; a negative count used to become a
/// ~2^32-iteration derivation.
#[test]
fn pbkdf2_spec_errors_follow_sunjce() {
    assert_eq!(pbkdf2_spec_error(false, 0, 0), Some("Salt not found"));
    assert_eq!(
        pbkdf2_spec_error(true, 0, 256),
        Some("Iteration count not found")
    );
    assert_eq!(
        pbkdf2_spec_error(true, -1, 256),
        Some("Iteration count is negative")
    );
    assert_eq!(
        pbkdf2_spec_error(true, 1000, 0),
        Some("Key length not found")
    );
    assert_eq!(
        pbkdf2_spec_error(true, 1000, -8),
        Some("Key length is negative")
    );
    assert_eq!(pbkdf2_spec_error(true, 1000, 256), None);
}
