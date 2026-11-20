use super::{proxy_mirror_or_oome, MethodCallFailed, ObjectRef, RuntimeError, VmError};

/// gch h2/fj (g4oo item 4): a proxy door's mirror mint runs with the
/// unwind permission raised and turns the allocators' refusal into a
/// catchable `OutOfMemoryError`; a mint that succeeds passes through, and
/// the permission is put back afterwards.
#[test]
fn gch_h2fj_a_refused_proxy_mirror_is_an_out_of_memory_error() {
    assert!(!crate::runtime::native_oom::unwind_ok(), "no scope before");
    // SAFETY: never dereferenced; only its address is compared.
    let obj = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
    let ok = proxy_mirror_or_oome(|| {
        assert!(
            crate::runtime::native_oom::unwind_ok(),
            "the mint may unwind"
        );
        obj
    });
    assert!(matches!(ok, Ok(o) if o.as_ptr() as usize == 0x1000));
    let refused = proxy_mirror_or_oome(|| crate::runtime::native_oom::raise(1, "class mirror"));
    assert!(matches!(
        refused,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { .. }
        )))
    ));
    assert!(!crate::runtime::native_oom::unwind_ok(), "no scope after");
}
