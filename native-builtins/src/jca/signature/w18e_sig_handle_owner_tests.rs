use super::*;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

struct Forget(usize);
impl Drop for Forget {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
    }
}

fn placeholder_pair() -> crypto_impl::RsaKeyPairData {
    let one = || crypto_impl::BigUint::from_bytes_be(&[1]);
    crypto_impl::RsaKeyPairData {
        public_key: crypto_impl::RsaPublicKey { n: one(), e: one() },
        private_key: crypto_impl::RsaPrivateKey {
            n: one(),
            d: one(),
            e: one(),
            p: None,
            q: None,
            dp: None,
            dq: None,
            qinv: None,
        },
    }
}

/// `init(key); key = null; gc; sign()`: the key object is dead, the
/// `Signature` (whose real-JDK form keeps no reference to it) still names
/// the handle, so the handle stays until the `Signature` lets go.
#[test]
fn a_signature_keeps_the_handle_its_dead_key_object_named() {
    const VM: usize = 0xE18E_0001;
    let _forget = Forget(VM);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    let id = crypto_impl::rsa_key_next_id();
    crypto_impl::rsa_key_store(id, placeholder_pair());
    let (key, sig) = (obj(0x1_E196_0010), obj(0x1_E197_0010));
    {
        let _hold = crypto_impl::hold_key_handle(crypto_impl::KeyStoreKind::Rsa, id);
        crypto_impl::rsa_realkey_map_set(&c, key, id);
    }
    set_sig_algo(&mut c, sig, SIG_SHA256_RSA);
    set_sig_keyid(&mut c, sig, id);
    let key_addr = key.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|a| a != key_addr);
    assert!(
        crypto_impl::rsa_key_registered(id),
        "the handle went with the key while the Signature still names it"
    );
    // Re-`init` without a handle: the Signature lets go, and it was the
    // last owner.
    set_sig_keyid(&mut c, sig, 0);
    assert!(!crypto_impl::rsa_key_registered(id));
}

#[test]
fn the_owned_store_follows_the_algorithm() {
    use crypto_impl::KeyStoreKind as K;
    assert_eq!(sig_handle_kind(Some(SIG_SHA256_ECDSA)), K::Ecdsa);
    assert_eq!(sig_handle_kind(Some(SIG_SHA384_ECDSA)), K::Ecdsa);
    assert_eq!(sig_handle_kind(Some(SIG_ED25519)), K::Ed25519);
    assert_eq!(sig_handle_kind(Some(SIG_SHA256_RSA)), K::Rsa);
    assert_eq!(sig_handle_kind(None), K::Rsa);
}
