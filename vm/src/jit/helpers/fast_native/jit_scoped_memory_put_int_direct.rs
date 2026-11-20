#[allow(unused_imports)]
use super::*;

/// `ScopedMemoryAccess.putIntUnaligned` fast path.
///
/// # The panic sentinel is `i64::MIN`, not `0` — r10 `help`
///
/// These four `put` shells were guarded with sentinel `0` while their own
/// bodies use `0` for **success** (the store was served) and `i64::MIN` for
/// failure, exactly like their `get` twins. The site is emitted with
/// `return_type = b'V'` (`ScopedMemoryOp::return_type`), and a `V` return takes
/// the plain arm of `emit_post_invoke_exception_check` — `CMP RAX, 1; JO bail`.
/// So a contained panic returning `0` told compiled code the store had
/// succeeded: the write was dropped, and the `InternalError` `OnPanic::Throw`
/// stashed was left on the pending channel for some unrelated later drain to
/// find. `i64::MIN` is the value that arm bails on, and is what every other
/// `Throw`-guarded direct helper in this file passes.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_int_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_int_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i32).to_be_bytes()
    } else {
        (val as i32).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_INT_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putShortUnaligned` fast path. Panic sentinel `i64::MIN`;
/// see [`jit_scoped_memory_put_int_direct`] for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_short_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_short_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i16).to_be_bytes()
    } else {
        (val as i16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_SHORT_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putLongUnaligned` fast path. Panic sentinel `i64::MIN`;
/// see [`jit_scoped_memory_put_int_direct`] for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_long_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_long_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        val.to_be_bytes()
    } else {
        val.to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_LONG_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putCharUnaligned` fast path. Panic sentinel `i64::MIN`;
/// see [`jit_scoped_memory_put_int_direct`] for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_char_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_char_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_char_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_char_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as u16).to_be_bytes()
    } else {
        (val as u16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_CHAR_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_CHAR_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.getIntUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_int_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getIntUnalignedInternal` by name, never to the public `getIntUnaligned`,
/// or a compiled `getIntUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_int_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_int_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_int_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_int_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 4];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_INT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i32::from_be_bytes(bytes)
        } else {
            i32::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_INT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.getShortUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_short_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getShortUnalignedInternal` by name, never to the public `getShortUnaligned`,
/// or a compiled `getShortUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_short_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_short_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_short_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_short_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_SHORT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i16::from_be_bytes(bytes)
        } else {
            i16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_SHORT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.getLongUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_long_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getLongUnalignedInternal` by name, never to the public `getLongUnaligned`,
/// or a compiled `getLongUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_long_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_long_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_long_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_long_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 8];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_LONG_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i64::from_be_bytes(bytes)
        } else {
            i64::from_le_bytes(bytes)
        };
        return val;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_LONG_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.getCharUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_char_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getCharUnalignedInternal` by name, never to the public `getCharUnaligned`,
/// or a compiled `getCharUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_char_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_char_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_char_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_char_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_CHAR_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_CHAR_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.putIntUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_int_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_int_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_int_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_int_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_int_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i32).to_be_bytes()
    } else {
        (val as i32).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_INT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_INT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putShortUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_short_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_short_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_short_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_short_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_short_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i16).to_be_bytes()
    } else {
        (val as i16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_SHORT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_SHORT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putLongUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_long_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_long_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_long_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_long_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_long_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val).to_be_bytes()
    } else {
        (val).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_LONG_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_LONG_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putCharUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_char_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_char_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_char_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_char_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_put_char_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as u16).to_be_bytes()
    } else {
        (val as u16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_CHAR_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_CHAR_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

// ============================================================================
// jdk/internal/misc/Unsafe plain scalar accessor thin direct helpers
// (heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918 "What is
// left" item 1)
// ============================================================================
//
// `Unsafe.get{Byte,Short,Int,Long}(Object,long)` / `put*` are genuinely
// `ACC_NATIVE`: the real JDK declares them `public native ... getInt(Object o,
// long offset);` with NO bytecode body. They are the floor underneath every
// `ScopedMemoryAccess.*Unaligned` call the helpers above decline (`session !=
// 0`, or under `--real-jdk`) and every `HeapByteBuffer`/netty
// `PlatformDependent` access that reaches `Unsafe` directly rather than
// through `ScopedMemoryAccess` -- measured at ~267 ns/call in BOTH modes
// (`UnsafeCrossingPerf`), i.e. already the generic-dispatch floor, not a
// `--real-jdk` regression. `raw_accessor_read_bytes`/`raw_accessor_write_bytes`
// (shared with the `ScopedMemoryAccess` family above) give the exact same
// answer a real `getfield`/array-element read would, computed without a call
// -- so, like `jit_buffer_session_direct`'s field read, this needs no
// `jit_direct_helper_refused`/§1.4 policy check: there is no bytecode being
// shadowed, because these methods have none. Declines to the generic
// dispatcher for anything the byte-copy core cannot serve (non-array objects,
// out-of-range offsets, reference arrays), which reaches the real registered
// native unchanged.
//
// No `bigEndian` argument on any of these -- the plain (non-`Unaligned`)
// `Unsafe` accessors always use the platform's native order, little-endian on
// every CratonVM host (see `unsafe_multibyte_get!`'s own doc comment in
// `native-builtins/src/unsafe_natives_ext.rs`).

pub static UNSAFE_GET_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getByte",
    descriptor: "(Ljava/lang/Object;J)B",
    num_jit_args: 3,
    return_type: b'B',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_GET_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getShort",
    descriptor: "(Ljava/lang/Object;J)S",
    num_jit_args: 3,
    return_type: b'S',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_GET_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getInt",
    descriptor: "(Ljava/lang/Object;J)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_GET_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getLong",
    descriptor: "(Ljava/lang/Object;J)J",
    num_jit_args: 3,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_GET_REFERENCE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getReference",
    descriptor: "(Ljava/lang/Object;J)Ljava/lang/Object;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_GET_REFERENCE_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getReferenceVolatile",
    descriptor: "(Ljava/lang/Object;J)Ljava/lang/Object;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_PUT_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putByte",
    descriptor: "(Ljava/lang/Object;JB)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_PUT_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putShort",
    descriptor: "(Ljava/lang/Object;JS)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_PUT_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putInt",
    descriptor: "(Ljava/lang/Object;JI)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_PUT_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putLong",
    descriptor: "(Ljava/lang/Object;JJ)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// NIO19-2: the triples [`UNSAFE_GET_CHAR_GATE`] / [`UNSAFE_PUT_CHAR_GATE`]
/// police. No thin helper stands in front of `getChar`/`putChar`; the gates
/// exist so [`unsafe_inline_admitted_mask`] can ask the same per-call policy
/// question about them that it asks about the other eight accessors before
/// the optimizing tier answers them inline. `const`, not `static`, so the
/// frozen `per_vm_state_statics_ratchet` count does not move (the gate's
/// `&INFO` is a promoted constant, as `OBJECT_GET_CLASS_INFO`'s is).
pub const UNSAFE_GET_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getChar",
    descriptor: "(Ljava/lang/Object;J)C",
    num_jit_args: 3,
    return_type: b'C',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const UNSAFE_PUT_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putChar",
    descriptor: "(Ljava/lang/Object;JC)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// [`raw_accessor_read_bytes`], additionally policed by `gate`'s capability
/// — the read half of what `scoped_memory_read_bytes` does for its own
/// family, minus the scoped-arena liveness check that family alone needs.
pub unsafe fn unsafe_accessor_read_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    obj: i64,
    offset: i64,
    out: &mut [u8],
) -> bool {
    if jit_direct_helper_refused(vm, gate) || !unsafe_accessor_capability_admits(vm, gate) {
        return false;
    }
    raw_accessor_read_bytes(vm, obj, offset, out)
}

/// The funnel's own capability check for `gate`'s classification, run per
/// served call: under `Permissive` it is the per-thread first-use memo plus
/// one record, so the audit a served call bypasses the funnel for still sees
/// the use. `direct_helper_refusal` has already refused every other mode.
pub fn unsafe_accessor_capability_admits(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    match gate.capability {
        Some(kind) => crate::vm::check_native_dispatch_capability_for_slot(vm, kind, None).is_ok(),
        None => true,
    }
}

/// The instance-field arm of the plain `Unsafe.getInt` / `getLong` thin
/// helpers: the field at `(obj, offset)` when its declared kind is `desc`
/// (`I` / `J`), read as the registered native reads it (`get_field`). Netty's
/// `ReferenceCountUpdater.isLiveNonVolatile` makes this read on every
/// `ByteBuf` access, and declining it to the native cost about 600 ns each.
/// A field of another kind (the native's field-view and sub-field arms), an
/// array, a static or synthetic offset, or a refused gate declines, and the
/// native answers as before. `getIntVolatile` has had this arm since nio21.
pub unsafe fn unsafe_plain_field_read(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    obj: i64,
    offset: i64,
    desc: u8,
) -> Option<Value> {
    if jit_direct_helper_refused(vm, gate) || !unsafe_accessor_capability_admits(vm, gate) {
        return None;
    }
    let (target, slot) = unsafe_volatile_field_target(vm, obj, offset)?;
    let (thread, _guard) = jit_thread_mut()?;
    let ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    if ctx.instance_field_descriptor_byte(target, slot)? != desc {
        return None;
    }
    let value = ctx.get_field(target, slot);
    // The compiled arm reads the cell the volatile helpers publish (opt-in).
    crate::vm::vm_exec::note_unsafe_volatile_field_cell(vm, target, slot, desc);
    Some(value)
}

/// Write-side twin of [`unsafe_accessor_read_bytes`].
pub unsafe fn unsafe_accessor_write_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    obj: i64,
    offset: i64,
    data: &[u8],
) -> bool {
    if jit_direct_helper_refused(vm, gate) || !unsafe_accessor_capability_admits(vm, gate) {
        return false;
    }
    raw_accessor_write_bytes(vm, obj, offset, data)
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_byte_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_get_byte_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 1];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_BYTE_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(bytes[0] as i8);
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_short_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_short_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_get_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_SHORT_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(i16::from_le_bytes(bytes));
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_int_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_int_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_get_int_direct_body(vm_ptr: i64, receiver: i64, obj: i64, offset: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 4];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_INT_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(i32::from_le_bytes(bytes));
    }
    if let Some(Value::Int(v)) =
        unsafe_plain_field_read(vm, &UNSAFE_GET_INT_GATE, obj, offset, b'I')
    {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(v);
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_long_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_long_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_get_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 8];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_LONG_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from_le_bytes(bytes);
    }
    if let Some(Value::Long(v)) =
        unsafe_plain_field_read(vm, &UNSAFE_GET_LONG_GATE, obj, offset, b'J')
    {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return v;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// Element `offset` of the REFERENCE ARRAY at `obj`, for the two reference
/// `Unsafe` read helpers (SBR round 2, wave 8, lane `tlocal`).
///
/// `Some(Some(object))` is the element, `Some(None)` a null element, `None`
/// declines (a null or unvalidatable base, a non-array, a primitive or
/// non-reference array, an offset that is not an element-aligned in-bounds byte
/// offset, a slot that does not hold a reference): the registered native then
/// answers, which is also the only thing that knows instance-field, static and
/// synthetic offsets.
///
/// The index decode is `unsafe_array_index_from_offset`'s byte-offset arm, spelled
/// out: `ABASE` 16, element scale 8 for `Reference`. The legacy "offset below
/// ABASE is already an index" arm is NOT served here (it exists for synthetic
/// callers; a real `ConcurrentHashMap.tabAt` offset is `ABASE + (i << 3)`).
///
/// Allocates nothing and cannot reach a safepoint, so the returned reference needs
/// only the object-return handoff root the callers set.
pub unsafe fn unsafe_reference_array_read(
    vm: &SharedVm,
    obj: i64,
    offset: i64,
) -> Option<Option<ObjectRef>> {
    let raw = obj as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    const ABASE: i64 = 16;
    if !cratonvm_types::zgc_read_barrier_armed()
        && offset >= ABASE
        && (offset - ABASE) % 8 == 0
    {
        let addr = raw as usize;
        let flags = unsafe { *((addr + cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *const u8) };
        let kind_tags = unsafe { *((addr + cratonvm_types::KIND_TAGS_BYTE_OFFSET) as *const u8) };
        if (flags & cratonvm_types::GC_FLAG_COMPACT) != 0
            && (kind_tags & 0x3f) == cratonvm_jit::unsafe_ref_array_arm::REF_ARRAY_KIND_TAGS_BYTE
        {
            let index = ((offset - ABASE) / 8) as usize;
            let len = unsafe { *((addr + 8) as *const u32) } as usize;
            if index < len {
                let elem = unsafe { *((addr + 16 + index * 8) as *const usize) };
                return Some((elem != 0).then(|| unsafe { ObjectRef::from_raw(elem as *mut u8) }));
            }
        }
    }
    let array = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(array) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    if vm.mem.heap.array_element_type(array) != Some(cratonvm_types::ArrayElementType::Reference) {
        return None;
    }
    if offset < ABASE || (offset - ABASE) % 8 != 0 {
        return None;
    }
    let index = ((offset - ABASE) / 8) as usize;
    if index >= vm.mem.heap.array_length(array) {
        return None;
    }
    match vm.mem.heap.get_array_element(array, index) {
        Ok(Value::Object(element)) => Some(element),
        _ => None,
    }
}

/// Shared body of the two reference `Unsafe` read helpers.
pub unsafe fn jit_unsafe_get_reference_common(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    gate: &'static DirectHelperGate,
    info: &'static JitInvokeInfo,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, gate) && unsafe_accessor_capability_admits(vm, gate) {
        if let Some(read) = unsafe_reference_array_read(vm, obj, offset) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return match read {
                    Some(object) => {
                        // Object-return handoff root, same contract as every other
                        // thin helper that hands a reference to compiled code.
                        thread.native_pending_return = Some(object);
                        object.as_ptr() as i64
                    }
                    None => 0,
                };
            }
        }
        // JIT round 21 wave 1, lane updaters (UPDATERS21-1, opt-in): a field
        // base publishes its reference cell for the compiled get / put arm,
        // then takes the call exactly as before (this helper serves arrays).
        // Only when a compile also armed the reference get / put arm, which
        // is the one reader of the cell (cvm-r21w1 A/B: without it this cost
        // every rfu-get call a field lookup).
        if vm.jit.unsafe_inline.ref_volatile_publish_armed() {
            if let Some((target, slot)) = unsafe_volatile_field_target(vm, obj, offset) {
                crate::vm::vm_exec::note_unsafe_volatile_field_cell(vm, target, slot, b'L');
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_reference_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || {
        jit_unsafe_get_reference_common(
            vm_ptr,
            receiver,
            obj,
            offset,
            &UNSAFE_GET_REFERENCE_GATE,
            &UNSAFE_GET_REFERENCE_INFO,
        )
    };
    let bits = contain(
        "jit_unsafe_get_reference_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_reference_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || {
        jit_unsafe_get_reference_common(
            vm_ptr,
            receiver,
            obj,
            offset,
            &UNSAFE_GET_REFERENCE_VOLATILE_GATE,
            &UNSAFE_GET_REFERENCE_VOLATILE_INFO,
        )
    };
    let bits = contain(
        "jit_unsafe_get_reference_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Panic sentinel `i64::MIN`, not `0`: `0` is this family's SUCCESS value
/// (`return_type = b'V'` takes the plain arm of
/// `emit_post_invoke_exception_check`), same convention as
/// `jit_scoped_memory_put_int_direct`.
pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_byte_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_put_byte_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = [val as u8];
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_BYTE_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_short_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_short_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_put_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = (val as i16).to_le_bytes();
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_SHORT_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_int_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_int_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_put_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = (val as i32).to_le_bytes();
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_INT_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_long_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_long_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_put_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = val.to_le_bytes();
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_LONG_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub static UNSAFE_CAS_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "compareAndSetInt",
    descriptor: "(Ljava/lang/Object;JII)Z",
    num_jit_args: 5,
    return_type: b'Z',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_CAS_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "compareAndSetLong",
    descriptor: "(Ljava/lang/Object;JJJ)Z",
    num_jit_args: 5,
    return_type: b'Z',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// ---------------------------------------------------------------------------
// nio21 wave 3, lane L4: the `ACC_NATIVE` triples the `HttpClient` census
// (`Nio20HttpHot -Dnio20.rows=hc`, `--real-jdk` top 60) found compiled code
// dispatching generically (page
// `r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md`).
//
// Every fast arm below answers through the SAME `NativeContext` accessor the
// registered native reaches for the same input (`get_field_volatile`,
// `set_field_volatile`, `set_array_element`, `compare_and_swap_field_shared`,
// `get_field`), and declines every other shape -- a null or array base where
// the native would take another arm, a synthetic / arena / sentinel offset, a
// value word that is not a heap reference -- to the registered native through
// `jit_invoke_dispatch`. So memory ordering and GC barriers are the native's
// by construction: a volatile store is the accessor's SeqCst store, a
// reference store fires the collector's SATB pre-barrier
// (`publish_overwritten_field` on the Generational heap, inside the accessor on
// G1/ZGC) and card barrier, and a reference CAS is the VM's hardware
// `LOCK CMPXCHG` with SATB on `expected`. A reference-array volatile store adds
// one `MFENCE` after the accessor's store (the native's element store is a
// plain one; HotSpot's is a volatile one).
//
// The descriptors are `const`, not `static`, so the frozen
// `per_vm_state_statics_ratchet` count does not move (as `UNSAFE_GET_CHAR_INFO`).
// ---------------------------------------------------------------------------

pub const UNSAFE_GET_INT_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getIntVolatile",
    descriptor: "(Ljava/lang/Object;J)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const UNSAFE_GET_LONG_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getLongVolatile",
    descriptor: "(Ljava/lang/Object;J)J",
    num_jit_args: 3,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const UNSAFE_PUT_INT_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putIntVolatile",
    descriptor: "(Ljava/lang/Object;JI)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const UNSAFE_PUT_LONG_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putLongVolatile",
    descriptor: "(Ljava/lang/Object;JJ)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const UNSAFE_PUT_REFERENCE_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putReferenceVolatile",
    descriptor: "(Ljava/lang/Object;JLjava/lang/Object;)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const UNSAFE_CAS_REFERENCE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "compareAndSetReference",
    descriptor: "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    num_jit_args: 5,
    return_type: b'Z',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const UNSAFE_FULL_FENCE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "fullFence",
    descriptor: "()V",
    num_jit_args: 1,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// `refersTo0` is `private` on both classes, so the decline edge dispatches it
/// as `invokespecial` (the `ThreadLocal` private bodies' convention below).
pub const REFERENCE_REFERS_TO0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ref/Reference",
    method_name: "refersTo0",
    descriptor: "(Ljava/lang/Object;)Z",
    num_jit_args: 2,
    return_type: b'Z',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const PHANTOM_REFERENCE_REFERS_TO0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ref/PhantomReference",
    method_name: "refersTo0",
    descriptor: "(Ljava/lang/Object;)Z",
    num_jit_args: 2,
    return_type: b'Z',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const REFERENCE_GET0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ref/Reference",
    method_name: "get0",
    descriptor: "()Ljava/lang/Object;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub const THREAD_CURRENT_CARRIER_THREAD_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Thread",
    method_name: "currentCarrierThread",
    descriptor: "()Ljava/lang/Thread;",
    num_jit_args: 0,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The instance-field arm of the volatile `Unsafe` natives
/// (`native_unsafe_{get,put}_{int,long}_volatile`,
/// `native_unsafe_put_object_volatile`, `native_unsafe_cas_object`): a
/// non-null heap object that is not an array, at an offset that can only be a
/// field slot. `0..1<<16` excludes the synthetic side-store band
/// (`0x2000_0000..`), the `Buffer.address` sentinel and every arena handle,
/// which the native answers from other arms; `unsafe_arena_volatile_load` and
/// `unsafe_wide_raw_*` need a null base or an array, so neither precedes this
/// arm for such an input. The object is returned forwarded.
///
/// # SAFETY
///
/// `obj` is a raw reference a compiled frame is holding (validated here).
pub unsafe fn unsafe_volatile_field_target(
    vm: &SharedVm,
    obj: i64,
    offset: i64,
) -> Option<(ObjectRef, usize)> {
    if !(0..(1 << 16)).contains(&offset) {
        return None;
    }
    let raw = obj as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    let object = vm.mem.heap.load_and_forward(object);
    if vm.mem.heap.kind_of(object) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    // JIT round 20 wave 8c (lane huntDisp): under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` the offset is `16 + slot * 8 + k`
    // (`native-builtins` `unsafe_decode_field_offset`); an offset below the
    // base or inside a field (`k != 0`) declines to the native, which serves
    // the field's bytes.
    if vm.classes.unsafe_byte_field_offsets {
        // Cast: `offset` is in `0..1 << 16` (checked above).
        let offset = offset as usize;
        if offset < 16 || (offset - 16) & 7 != 0 {
            return None;
        }
        return Some((object, (offset - 16) >> 3));
    }
    Some((object, offset as usize))
}

/// The reference-ARRAY arm of `native_unsafe_put_object_volatile` /
/// `native_unsafe_cas_object`: `unsafe_checked_array_index`'s byte-offset
/// decode (`ABASE` 16, scale 8 for `Reference`), served only for an aligned,
/// in-bounds element of a reference array below the synthetic band. The legacy
/// "offset below ABASE is an index" arm is not served (as
/// [`unsafe_reference_array_read`]). Forwarded.
///
/// # SAFETY
///
/// As [`unsafe_volatile_field_target`].
pub unsafe fn unsafe_reference_array_slot(
    vm: &SharedVm,
    obj: i64,
    offset: i64,
) -> Option<(ObjectRef, usize)> {
    const ABASE: i64 = 16;
    if offset < ABASE || offset >= 0x2000_0000 || (offset - ABASE) % 8 != 0 {
        return None;
    }
    let raw = obj as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let array = vm.mem.heap.is_object_address(raw as usize)?;
    let array = vm.mem.heap.load_and_forward(array);
    if vm.mem.heap.kind_of(array) != cratonvm_types::ObjectKind::Array
        || vm.mem.heap.array_element_type(array)
            != Some(cratonvm_types::ArrayElementType::Reference)
    {
        return None;
    }
    let index = ((offset - ABASE) / 8) as usize;
    if index >= vm.mem.heap.array_length(array) {
        return None;
    }
    Some((array, index))
}

/// A reference operand word as the `Value` the funnel would have handed the
/// native: `0` is null, anything else must be a heap object (forwarded, as the
/// funnel's argument barrier forwards it); `None` declines.
///
/// # SAFETY
///
/// `raw` is a word a compiled frame passed as a reference (validated here).
pub unsafe fn unsafe_reference_operand(vm: &SharedVm, raw: i64) -> Option<Value> {
    if raw == 0 {
        return Some(Value::Object(None));
    }
    let bits = raw as u64;
    if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(bits as usize)?;
    Some(Value::Object(Some(vm.mem.heap.load_and_forward(object))))
}

/// The receiver null check every `Unsafe` thin helper makes first.
pub fn unsafe_null_receiver() -> i64 {
    set_jit_pending_npe_action(
        crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
    );
    i64::MIN
}

/// `getIntVolatile` (`wide == false`) / `getLongVolatile` (`wide == true`).
pub unsafe fn jit_unsafe_get_volatile_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    wide: bool,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let (gate, info): (&'static DirectHelperGate, &'static JitInvokeInfo) = if wide {
        (
            &UNSAFE_GET_LONG_VOLATILE_GATE,
            &UNSAFE_GET_LONG_VOLATILE_INFO,
        )
    } else {
        (&UNSAFE_GET_INT_VOLATILE_GATE, &UNSAFE_GET_INT_VOLATILE_INFO)
    };
    if !jit_direct_helper_refused(vm, gate) && unsafe_accessor_capability_admits(vm, gate) {
        if let Some((target, slot)) = unsafe_volatile_field_target(vm, obj, offset) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // A read is idempotent, so a value of another kind (the funnel
                // would coerce it) declines and the native re-reads it.
                match (wide, ctx.get_field_volatile(target, slot)) {
                    (true, Value::Long(v)) => {
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // JIT round 21 wave 1, lane updaters (opt-in).
                        crate::vm::vm_exec::note_unsafe_volatile_field_cell(
                            vm, target, slot, b'J',
                        );
                        return v;
                    }
                    (false, Value::Int(v)) => {
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // JIT round 21 wave 1, lane updaters (opt-in).
                        crate::vm::vm_exec::note_unsafe_volatile_field_cell(
                            vm, target, slot, b'I',
                        );
                        return i64::from(v);
                    }
                    _ => {}
                }
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_int_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_volatile_body(vm_ptr, receiver, obj, offset, false);
    contain(
        "jit_unsafe_get_int_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_long_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_volatile_body(vm_ptr, receiver, obj, offset, true);
    contain(
        "jit_unsafe_get_long_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `putIntVolatile` (`kind == b'I'`), `putLongVolatile` (`b'J'`) and
/// `putReferenceVolatile` (`b'L'`). Returns `0` served, else the dispatcher's
/// word (`i64::MIN` exactly when the native threw).
pub unsafe fn jit_unsafe_put_volatile_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
    kind: u8,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let (gate, info): (&'static DirectHelperGate, &'static JitInvokeInfo) = match kind {
        b'I' => (&UNSAFE_PUT_INT_VOLATILE_GATE, &UNSAFE_PUT_INT_VOLATILE_INFO),
        b'J' => (
            &UNSAFE_PUT_LONG_VOLATILE_GATE,
            &UNSAFE_PUT_LONG_VOLATILE_INFO,
        ),
        _ => (
            &UNSAFE_PUT_REFERENCE_VOLATILE_GATE,
            &UNSAFE_PUT_REFERENCE_VOLATILE_INFO,
        ),
    };
    if !jit_direct_helper_refused(vm, gate) && unsafe_accessor_capability_admits(vm, gate) {
        // An `int` operand arrives in the low 32 bits of its word.
        let value = match kind {
            b'I' => Some(Value::Int(val as i32)),
            b'J' => Some(Value::Long(val)),
            _ => unsafe_reference_operand(vm, val),
        };
        if let Some(value) = value {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // JIT round 21 (lane unsafe): only into a field of the
                // store's own kind; a `putIntVolatile` into a `long` / `float`
                // / `double` field (HotSpot: its low four bytes) and a
                // `putLongVolatile` into a `double` declines to the native,
                // whose lane store keeps the field's other bytes and kind.
                // (A reference store reads nothing here.)
                let same_kind = |target: ObjectRef, slot: usize| {
                    kind == b'L'
                        || !matches!(
                            (kind, ctx.get_field_volatile(target, slot)),
                            (b'I', Value::Long(_) | Value::Float(_) | Value::Double(_))
                                | (b'J', Value::Double(_))
                        )
                };
                if let Some((target, slot)) = unsafe_volatile_field_target(vm, obj, offset)
                    .filter(|&(target, slot)| same_kind(target, slot))
                {
                    // The native's own call: descriptor-aware SeqCst store,
                    // SATB publish of the overwritten reference, card barrier.
                    ctx.set_field_volatile(target, slot, value);
                    cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // JIT round 21 wave 1, lane updaters (opt-in): the int /
                    // long cell only; a reference put publishes nothing.
                    // UPDATERS21-1: the reference cell too.
                    crate::vm::vm_exec::note_unsafe_volatile_field_cell(vm, target, slot, kind);
                    return 0;
                }
                if kind == b'L' {
                    if let Some((array, index)) = unsafe_reference_array_slot(vm, obj, offset) {
                        // The native's element store (barriered), then the
                        // trailing StoreLoad a volatile store owes.
                        ctx.set_array_element(array, index, value);
                        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return 0;
                    }
                }
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_int_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_volatile_body(vm_ptr, receiver, obj, offset, val, b'I');
    contain(
        "jit_unsafe_put_int_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_long_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_volatile_body(vm_ptr, receiver, obj, offset, val, b'J');
    contain(
        "jit_unsafe_put_long_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_reference_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_volatile_body(vm_ptr, receiver, obj, offset, val, b'L');
    contain(
        "jit_unsafe_put_reference_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_reference_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || jit_unsafe_cas_reference_direct_body(vm_ptr, receiver, obj, offset, expected, new);
    contain(
        "jit_unsafe_cas_reference_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `compareAndSetReference`: an instance field or a reference-array element
/// through `compare_and_swap_field_shared` -- the body of the native's
/// `ctx.compare_and_swap_field` -- and nothing else. `CRATONVM_DBG_AQS_TRACE`
/// declines everything, as [`unsafe_cas_serve`] does, so the native's ledger
/// stays complete.
pub unsafe fn jit_unsafe_cas_reference_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if !cratonvm_native_builtins::util_concurrent_ext::aqs_trace_enabled()
        && !jit_direct_helper_refused(vm, &UNSAFE_CAS_REFERENCE_GATE)
        && unsafe_accessor_capability_admits(vm, &UNSAFE_CAS_REFERENCE_GATE)
    {
        if let (Some(exp), Some(nv)) = (
            unsafe_reference_operand(vm, expected),
            unsafe_reference_operand(vm, new),
        ) {
            let target = unsafe_volatile_field_target(vm, obj, offset)
                .or_else(|| unsafe_reference_array_slot(vm, obj, offset));
            if let Some((target, slot)) = target {
                let swapped =
                    crate::vm::vm_exec::compare_and_swap_field_shared(vm, target, slot, exp, nv);
                cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return i64::from(swapped);
            }
        }
    }
    cratonvm_jit::UNSAFE_CAS_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, expected, new];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_CAS_REFERENCE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_full_fence_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_unsafe_full_fence_direct_body(vm_ptr, receiver);
    contain(
        "jit_unsafe_full_fence_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `fullFence`: `std::sync::atomic::fence(SeqCst)`, which is `MFENCE` on
/// x86-64 -- what `native_unsafe_fence` does, minus the funnel.
pub unsafe fn jit_unsafe_full_fence_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &UNSAFE_FULL_FENCE_GATE)
        && unsafe_accessor_capability_admits(vm, &UNSAFE_FULL_FENCE_GATE)
    {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_FULL_FENCE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// `Reference.refersTo0` / `PhantomReference.refersTo0`: exactly
/// `native_reference_refers_to` -- slot 0 read through `get_field_no_keepalive`
/// (NO keep-alive: `refersTo` must not resurrect the referent, so unlike
/// `jit_getfield`'s slot-0 SATB log this publishes nothing, under every
/// collector, and under an armed ZGC read barrier it does not mark either --
/// zt t23/zb8) and a pointer compare against the (forwarded) argument.
pub unsafe fn jit_refers_to0_body(vm_ptr: i64, receiver: i64, other: i64, phantom: bool) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let (gate, info): (&'static DirectHelperGate, &'static JitInvokeInfo) = if phantom {
        (
            &PHANTOM_REFERENCE_REFERS_TO0_GATE,
            &PHANTOM_REFERENCE_REFERS_TO0_INFO,
        )
    } else {
        (&REFERENCE_REFERS_TO0_GATE, &REFERENCE_REFERS_TO0_INFO)
    };
    if !jit_direct_helper_refused(vm, gate) {
        if let (Some(Value::Object(Some(this))), Some(target)) = (
            unsafe_reference_operand(vm, receiver),
            unsafe_reference_operand(vm, other),
        ) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // zt t23/zb8: the non-keep-alive read (no mark under an
                // armed ZGC read barrier either).
                let referent = ctx.get_field_no_keepalive(this, 0);
                let result = match (referent, target) {
                    (Value::Object(Some(a)), Value::Object(Some(b))) => a.as_ptr() == b.as_ptr(),
                    (Value::Object(None), Value::Object(None)) => true,
                    _ => false,
                };
                note_funnel_bypass_hit();
                return i64::from(result);
            }
        }
    }
    let args = [receiver, other];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_reference_refers_to0_direct(
    vm_ptr: i64,
    receiver: i64,
    other: i64,
) -> i64 {
    let f = || jit_refers_to0_body(vm_ptr, receiver, other, false);
    contain(
        "jit_reference_refers_to0_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_phantom_reference_refers_to0_direct(
    vm_ptr: i64,
    receiver: i64,
    other: i64,
) -> i64 {
    let f = || jit_refers_to0_body(vm_ptr, receiver, other, true);
    contain(
        "jit_phantom_reference_refers_to0_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_reference_get0_body(vm_ptr: i64, receiver: i64) -> i64 {
    if receiver == 0 {
        crate::jit::conservative_roots::note_jit_boundary();
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &REFERENCE_GET0_GATE)
        && vm.mem.heap.zgc_referent_gate_state() != cratonvm_gc::zgc::ZGC_REFERENT_GATE_ARMED
    {
        let recv_addr = receiver as usize;
        if (recv_addr & 0x7) == 0 && recv_addr < (1usize << 48) {
            let flags = unsafe { *((recv_addr + cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *const u8) };
            if (flags & cratonvm_types::GC_FLAG_COMPACT) != 0 {
                let cid = unsafe { *(recv_addr as *const u32) };
                let packed = vm
                    .jit
                    .jit_alloc_class_cache
                    .last_ref_get0
                    .load(std::sync::atomic::Ordering::Relaxed);
                if (packed >> 16) as u32 == cid && (packed & 0xffff) == 8 {
                    let raw_val = unsafe { *((recv_addr + 8) as *const usize) };
                    if raw_val == 0 {
                        return 0;
                    }
                    if !cratonvm_types::zgc_read_barrier_armed() {
                        return raw_val as i64;
                    }
                    let obj = vm.mem.heap.load_and_forward(unsafe { ObjectRef::from_raw(raw_val as *mut u8) });
                    return obj.as_ptr() as i64;
                }
            }
        }
        if let Some(raw) = vm.mem.heap.fast_read_compact_ref_slot0(receiver as usize, |cid| {
            let packed = vm
                .jit
                .jit_alloc_class_cache
                .last_ref_get0
                .load(std::sync::atomic::Ordering::Relaxed);
            if (packed >> 16) as u32 == cid && (packed & 0xffff) != 0 {
                return Some((packed & 0xffff) as usize);
            }
            if vm.jit.jit_alloc_class_cache.enabled()
                && vm
                    .jit
                    .jit_alloc_class_cache
                    .get(cid)
                    .is_some_and(|info| info.reference_kind != 1)
            {
                let off = cratonvm_types::with_current_class_layout(cid, |layout| {
                    let off = layout.field_disp(0)?;
                    let storage = layout.field_storage(0)?;
                    (layout.total_size <= 1024 && storage.is_reference() && off > 0 && off < 0xffff)
                        .then_some(off as usize)
                })
                .flatten();
                if let Some(off) = off {
                    vm.jit.jit_alloc_class_cache.last_ref_get0.store(
                        ((cid as u64) << 16) | (off as u64),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                off
            } else {
                None
            }
        }) {
            note_funnel_bypass_hit();
            return raw as i64;
        }
        if let Some(Value::Object(Some(this))) = unsafe_reference_operand(vm, receiver) {
            let header = vm.mem.heap.get_header(this);
            let class_id = header.class_id;
            let cached_kind = if vm.jit.jit_alloc_class_cache.enabled() {
                vm.jit
                    .jit_alloc_class_cache
                    .get(class_id.as_u32())
                    .map(|info| info.reference_kind)
            } else {
                None
            };
            // SoftReference (kind 1) must update its LRU timestamp on get0;
            // uncached classes (None) fall back to the registered native.
            if cached_kind.is_some_and(|k| k != 1) {
                let referent = vm.mem.heap.get_field_as(this, 0, b'L');
                let out = match referent {
                    Value::Object(Some(r)) => {
                        vm.mem.heap.referent_keep_alive(referent);
                        r.as_ptr() as i64
                    }
                    _ => 0,
                };
                note_funnel_bypass_hit();
                return out;
            }
        }
    }
    crate::jit::conservative_roots::note_jit_boundary();
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &REFERENCE_GET0_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_reference_get0_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_reference_get0_body(vm_ptr, receiver);
    contain(
        "jit_reference_get0_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub static SYSTEM_CURRENT_TIME_MILLIS_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/System",
    method_name: "currentTimeMillis",
    descriptor: "()J",
    num_jit_args: 0,
    return_type: b'J',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SYSTEM_NANO_TIME_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/System",
    method_name: "nanoTime",
    descriptor: "()J",
    num_jit_args: 0,
    return_type: b'J',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static OBJECT_NOTIFY_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "notify",
    descriptor: "()V",
    num_jit_args: 1,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static OBJECT_NOTIFY_ALL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "notifyAll",
    descriptor: "()V",
    num_jit_args: 1,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_UNPARK_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "unpark",
    descriptor: "(Ljava/lang/Object;)V",
    num_jit_args: 2,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static UNSAFE_PARK_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "park",
    descriptor: "(ZJ)V",
    num_jit_args: 3,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub(in crate::jit) unsafe extern "C" fn jit_system_current_time_millis_direct(vm_ptr: i64) -> i64 {
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &SYSTEM_CURRENT_TIME_MILLIS_GATE) {
        note_funnel_bypass_hit();
        return cratonvm_native_builtins::lang_system::current_time_millis_raw();
    }
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        jit_invoke_dispatch(
            vm_ptr,
            &SYSTEM_CURRENT_TIME_MILLIS_INFO as *const JitInvokeInfo as i64,
            0,
            0,
        )
    };
    contain(
        "jit_system_current_time_millis_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_system_nano_time_direct(vm_ptr: i64) -> i64 {
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &SYSTEM_NANO_TIME_GATE) {
        note_funnel_bypass_hit();
        return cratonvm_native_builtins::lang_system::java_time_nanos();
    }
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        jit_invoke_dispatch(
            vm_ptr,
            &SYSTEM_NANO_TIME_INFO as *const JitInvokeInfo as i64,
            0,
            0,
        )
    };
    contain(
        "jit_system_nano_time_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_object_notify_body(vm_ptr: i64, receiver: i64, all: bool) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let vm = &*(vm_ptr as *const SharedVm);
    let (gate, info) = if all {
        (&OBJECT_NOTIFY_ALL_GATE, &OBJECT_NOTIFY_ALL_INFO)
    } else {
        (&OBJECT_NOTIFY_GATE, &OBJECT_NOTIFY_INFO)
    };
    if receiver != 0 && !jit_direct_helper_refused(vm, gate) {
        if let Some(Value::Object(Some(obj))) = unsafe_reference_operand(vm, receiver) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let mut moved = Vec::new();
                let vtm = vm.threads.virtual_thread_manager.may_have_virtual_threads();
                let mon = &vm.threads.monitors;
                let res = if all { mon.notify_all_reporting(obj, thread.thread_id, vtm.then_some(&mut moved)) } else { mon.notify_reporting(obj, thread.thread_id, vtm.then_some(&mut moved)) };
                if res.is_ok() {
                    if !moved.is_empty() {
                        crate::vm::vm_exec::mark_notified_virtual_waiters(vm, &moved);
                    }
                    note_funnel_bypass_hit();
                    return 0;
                }
            }
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_object_notify_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_object_notify_body(vm_ptr, receiver, false);
    contain(
        "jit_object_notify_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_object_notify_all_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_object_notify_body(vm_ptr, receiver, true);
    contain(
        "jit_object_notify_all_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_unpark_body(vm_ptr: i64, receiver: i64, thread_word: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let vm = &*(vm_ptr as *const SharedVm);
    if receiver != 0
        && !jit_direct_helper_refused(vm, &UNSAFE_UNPARK_GATE)
        && unsafe_accessor_capability_admits(vm, &UNSAFE_UNPARK_GATE)
    {
        if vm.mem.heap.is_heap_addr(receiver as usize).is_some() {
            if thread_word == 0 {
                cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return 0;
            }
            match unsafe_reference_operand(vm, thread_word) {
                Some(Value::Object(None)) => {
                    cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return 0;
                }
                Some(Value::Object(Some(thread_obj))) => {
                    if vm.threads.thread_registry.unpark_eetop()
                        && crate::vm::vm_exec::unpark_platform_through_eetop(vm, thread_obj)
                    {
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return 0;
                    }
                    if let Some((thread, _guard)) = jit_thread_mut() {
                        let ctx = crate::vm::NativeContextImpl {
                            shared: vm,
                            thread: &mut *thread,
                        };
                        cratonvm_native_api::NativeThreadAccess::unpark(&ctx, thread_obj);
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return 0;
                    }
                }
                _ => {}
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, thread_word];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_UNPARK_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_unpark_direct(
    vm_ptr: i64,
    receiver: i64,
    thread_word: i64,
) -> i64 {
    let f = || jit_unsafe_unpark_body(vm_ptr, receiver, thread_word);
    contain(
        "jit_unsafe_unpark_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_park_body(
    vm_ptr: i64,
    receiver: i64,
    is_absolute: i64,
    time: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let vm = &*(vm_ptr as *const SharedVm);
    if receiver != 0
        && !jit_direct_helper_refused(vm, &UNSAFE_PARK_GATE)
        && unsafe_accessor_capability_admits(vm, &UNSAFE_PARK_GATE)
    {
        if let Some(Value::Object(Some(this))) = unsafe_reference_operand(vm, receiver) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                let args = [
                    Value::Object(Some(this)),
                    Value::Int(i32::from(is_absolute != 0)),
                    Value::Long(time),
                ];
                if cratonvm_native_builtins::unsafe_natives_ext::native_unsafe_park(&mut ctx, &args)
                    .is_ok()
                {
                    cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return 0;
                }
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, is_absolute, time];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PARK_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_park_direct(
    vm_ptr: i64,
    receiver: i64,
    is_absolute: i64,
    time: i64,
) -> i64 {
    let f = || jit_unsafe_park_body(vm_ptr, receiver, is_absolute, time);
    contain(
        "jit_unsafe_park_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `Thread.currentCarrierThread()`: the registered native is
/// `native_thread_current_thread`, the one `Thread.currentThread()` runs, so
/// this is [`jit_thread_current_thread_direct`]'s body with this triple's gate
/// and decline edge. (A mounted virtual thread is answered as the native
/// answers it; that the native then differs from HotSpot is the native's
/// page, not this helper's.)
pub(in crate::jit) unsafe extern "C" fn jit_thread_current_carrier_thread_direct(
    vm_ptr: i64,
) -> i64 {
    let f = || jit_thread_current_carrier_thread_direct_body(vm_ptr);
    let bits = contain(
        "jit_thread_current_carrier_thread_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}


pub unsafe fn jit_thread_current_carrier_thread_direct_body(vm_ptr: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &THREAD_CURRENT_CARRIER_THREAD_GATE) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            // A carrier whose `currentThread()` was re-pointed at a mounted
            // virtual thread (`Thread.setCurrentThread`, the VM's
            // `jdk_continuation` module) answers its own thread: the native.
            if thread.jdk_continuation.carrier() == 0 {
                if let Some(obj) = thread.java_thread_obj {
                    thread.native_pending_return = Some(obj);
                    note_funnel_bypass_hit();
                    return obj.as_ptr() as i64;
                }
            }
        }
    }
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_CURRENT_CARRIER_THREAD_INFO as *const JitInvokeInfo as i64,
        0,
        0,
    )
}

/// The fast arm of the two `Unsafe.compareAndSet{Int,Long}` thin helpers:
/// `Some(swapped)` when this call is one the registered native would answer
/// with the hardware CAS, `None` to decline to it.
///
/// `native_unsafe_cas_int`/`_long` (`native-builtins/src/unsafe_natives_ext.rs`)
/// have five arms: a null base (static field, arena or side store), a synthetic
/// offset (side store), an array (locked element CAS), and an instance field,
/// which is `compare_and_swap_field(obj, offset, ..)` and, for a slot
/// `hw_atomic_addr` can address, one `LOCK CMPXCHG`. This serves ONLY that last
/// case, through [`crate::vm::vm_exec::hw_cas_primitive_field`] -- the same
/// function the native reaches, so an interpreted and a compiled CAS address
/// the same word in the same atomicity domain. Every other shape, including an
/// instance field whose LEGACY cell is not yet tagged with this width, is a
/// `None` and runs the native unchanged.
///
/// * A synthetic offset (`0x2000_0000..`) and the high `Buffer.address`
///   sentinel are far above any field index, so `hw_atomic_addr`'s own proof
///   -- the index must resolve to a DECLARED field of this width on the
///   receiver's class -- already excludes them. The explicit bound below only
///   saves the descriptor lookup.
/// * `CRATONVM_DBG_AQS_TRACE` declines everything, so the native's AQS ledger
///   stays complete while it is on.
/// * The capability gate is the funnel's own
///   (`check_native_dispatch_capability_for_slot`), via [`jit_direct_helper_refused`].
///
/// # SAFETY
///
/// `obj_raw` is a raw reference a compiled frame is holding.
pub unsafe fn unsafe_cas_serve(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    obj_raw: i64,
    offset: i64,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    if obj_raw == 0 || !(0..(1 << 16)).contains(&offset) {
        return None;
    }
    if cratonvm_native_builtins::util_concurrent_ext::aqs_trace_enabled()
        || jit_direct_helper_refused(vm, gate)
    {
        return None;
    }
    let obj = vm.mem.heap.is_object_address(obj_raw as usize)?;
    let fwd = vm.mem.heap.load_and_forward(obj);
    // JIT round 20 wave 8c (lane huntDisp): the slot of a byte field offset
    // (`unsafe_volatile_field_target`'s decode); inside a field declines to
    // the native.
    let index = if vm.classes.unsafe_byte_field_offsets {
        // Cast: `offset` is in `0..1 << 16` (checked above).
        let offset = offset as usize;
        if offset < 16 || (offset - 16) & 7 != 0 {
            return None;
        }
        (offset - 16) >> 3
    } else {
        offset as usize
    };
    crate::vm::vm_exec::hw_cas_primitive_field(vm, fwd, index, expected, new_val)
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_int_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || jit_unsafe_cas_int_direct_body(vm_ptr, receiver, obj, offset, expected, new);
    contain(
        "jit_unsafe_cas_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_cas_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // An `int` operand arrives in the low 32 bits of its word.
    if let Some(swapped) = unsafe_cas_serve(
        vm,
        &UNSAFE_CAS_INT_GATE,
        obj,
        offset,
        Value::Int(expected as i32),
        Value::Int(new as i32),
    ) {
        cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(swapped);
    }
    cratonvm_jit::UNSAFE_CAS_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, expected, new];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_CAS_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_long_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || jit_unsafe_cas_long_direct_body(vm_ptr, receiver, obj, offset, expected, new);
    contain(
        "jit_unsafe_cas_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_unsafe_cas_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(swapped) = unsafe_cas_serve(
        vm,
        &UNSAFE_CAS_LONG_GATE,
        obj,
        offset,
        Value::Long(expected),
        Value::Long(new),
    ) {
        cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(swapped);
    }
    cratonvm_jit::UNSAFE_CAS_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, expected, new];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_CAS_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, opt-in): the LEAF TWIN of
/// [`jit_unsafe_cas_int_direct`] (`wide == false`) and
/// [`jit_unsafe_cas_long_direct`] (`wide == true`): the helper's fast arm
/// (`unsafe_cas_serve`: one `LOCK CMPXCHG` on a proven instance field) and `-1`
/// -- never a `boolean` -- where the helper would raise the null receiver's
/// NPE or dispatch the registered native. The contract is
/// [`jit_preconditions_check_index_leaf`]'s: no allocation, no raise (a panic
/// is RECORDED and declines), no Java, no dispatch. A served CAS counts into
/// `UNSAFE_CAS_SERVED` as the helper's does; a decline counts nothing (the
/// helper it falls to counts it). The capability / `--real-jdk` gate is asked
/// again by the helper on a decline, so `direct_helper_jdk_only_*` may count a
/// declined call twice (as for the `checkIndex` twin).
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn unsafe_cas_leaf(
    wide: bool,
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || {
        if receiver == 0 {
            return -1;
        }
        // SAFETY: as in `jit_unsafe_cas_int_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        let (gate, expected, new_val) = if wide {
            (
                &UNSAFE_CAS_LONG_GATE,
                Value::Long(expected),
                Value::Long(new),
            )
        } else {
            // An `int` operand arrives in the low 32 bits of its word.
            (
                &UNSAFE_CAS_INT_GATE,
                Value::Int(expected as i32),
                Value::Int(new as i32),
            )
        };
        match unsafe_cas_serve(vm, gate, obj, offset, expected, new_val) {
            Some(swapped) => {
                cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                i64::from(swapped)
            }
            None => -1,
        }
    };
    contain("jit_unsafe_cas_leaf", OnPanic::RecordFor { vm_ptr }, -1, f)
}

/// [`unsafe_cas_leaf`] for `compareAndSetInt`.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_int_leaf(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    unsafe_cas_leaf(false, vm_ptr, receiver, obj, offset, expected, new)
}

/// [`unsafe_cas_leaf`] for `compareAndSetLong`.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_long_leaf(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    unsafe_cas_leaf(true, vm_ptr, receiver, obj, offset, expected, new)
}

/// Synthetic call-site info for [`jit_thread_current_thread_direct`]'s
/// cold arm (the first call on a thread whose mirror has not been built).
pub static THREAD_CURRENT_THREAD_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Thread",
    method_name: "currentThread",
    descriptor: "()Ljava/lang/Thread;",
    num_jit_args: 0,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Process-wide count of compiled-code native-funnel bypasses.
///
/// The counter exists for the same reason
/// `dispatch_static::INTRINSIC_HITS` does, and the source document says why
/// in as many words: *"the counter exists because two earlier attempts at
/// this shape were **inert**, and timings alone cannot tell 'never installed'
/// from 'installed but no faster'."* A JIT-side bypass has one extra way to
/// be inert that the interpreter one does not — `try_compile` may simply
/// never recognise the site — so a counter incremented in the emitted call's
/// own target is the only evidence that compiled code took this path.
///
/// Reported by `CRATONVM_INTRINSIC_STATS=1` alongside the interpreter count.
///
/// Was the process-global `JIT_FUNNEL_BYPASS_HITS`; now summed over the
/// per-thread [`JitCounterBlock`]s (r9-vmside), because every served
/// `VarHandle` direct call bumped it on top of its own counter.
///
/// Number of compiled-code native-funnel bypasses since process start.
pub fn jit_funnel_bypass_count() -> u64 {
    jit_counter_total(|b| {
        b.funnel_bypass_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// Thin direct-call target for JIT `invokestatic Thread.currentThread()`
/// sites (registered into `cratonvm_jit::THREAD_CURRENT_THREAD_DIRECT_FN` by
/// `build_helpers`; the recognition lives in `jit::try_compile`).
///
/// The JIT half of the bypass the interpreter got in
/// `dispatch_static.rs`'s `InterpIntrinsic::ThreadCurrentThread` arm. Once a
/// thread's `java_thread_obj` mirror exists, `currentThread()` is one field
/// read of a value that is **already a per-thread GC root**: there is no
/// argument to pin, nothing here allocates, collects or throws, and no Java
/// code runs. Everything `safe_native_call` does for it — the argument copy
/// and GC-forwarding barrier, `native_pin_roots`, the STW probe, the two
/// GC-pressure probes, two `thread_state::record_transition` calls, the
/// native ring, `catch_unwind`, the `memwatch`/`ec_watch` polls — is
/// bookkeeping for hazards this operation does not have.
///
/// What the compiled site paid instead was *worse* than the funnel: an
/// `invokestatic` whose callee is a registered native binds no compiled body,
/// so it fell through `jit_invoke_dispatch` to `vm_exec::invoke_or_native`,
/// which re-resolves the callee **by name** on every call and only then
/// enters the funnel.
///
/// The cold arm — a thread whose mirror has not been built yet, i.e. at most
/// once per thread — routes to `jit_invoke_dispatch` with the synthetic
/// call-site info above, which is byte-for-byte the route the site took
/// before this helper existed. That keeps `current_thread_object`'s
/// allocating slow path in exactly one place.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_current_thread_direct(vm_ptr: i64) -> i64 {
    let f = || jit_thread_current_thread_direct_body(vm_ptr);
    let bits = contain(
        "jit_thread_current_thread_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_thread_current_thread_direct`], run under its panic guard.
pub unsafe fn jit_thread_current_thread_direct_body(vm_ptr: i64) -> i64 {
    // Same Rust<->JIT boundary bookkeeping as every other direct helper: the
    // per-thread conservative-scan cache must be invalidated. No SATB flush —
    // the fast arm below cannot allocate or enter the GC barrier, and the
    // cold arm's `jit_invoke_dispatch` performs its own.
    crate::jit::conservative_roots::note_jit_boundary();
    // H12-1. `java/lang/Thread.currentThread()Ljava/lang/Thread;` is `bridge`,
    // and this is the helper the OSR door was MEASURED binding under
    // `--real-jdk` (298 000 calls, `OSR 1`, single-pass `0/7`). Declining here
    // takes the cold arm below — `jit_invoke_dispatch` with the synthetic
    // call-site info — which this function's doc already describes as
    // "byte-for-byte the route the site took before this helper existed", and
    // which is policy-checked.
    //
    // Checked up front rather than beside the `java_thread_obj` read: in strict
    // mode this helper must never serve, so every call IS a refusal, and the
    // only calls the counter over-reports relative to "fast arm suppressed" are
    // the ones with no thread mirror yet — at most one per thread.
    //
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the cold arm below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &THREAD_CURRENT_THREAD_GATE) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(obj) = thread.java_thread_obj {
                // Mirror `safe_native_call`'s object-return handoff root,
                // exactly as `call_integer_native_raw` does. The mirror is
                // already rooted per-thread, so this is the diagnostic/contract
                // half rather than a liveness requirement; the wrapper drops it
                // again at the hand-off (handoff-w36b).
                thread.native_pending_return = Some(obj);
                note_funnel_bypass_hit();
                return obj.as_ptr() as i64;
            }
        }
    }
    // No mirror yet, no JIT thread context, or `JdkOnly` declined the fast arm:
    // the ordinary route, which is what this site did on every call before the
    // fast arm existed.
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_CURRENT_THREAD_INFO as *const JitInvokeInfo as i64,
        0,
        0,
    )
}

/// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`, opt-in): the LEAF TWIN of
/// [`jit_thread_current_thread_direct`]: its fast arm, and `0` (never a
/// thread mirror) where the helper would take its `jit_invoke_dispatch` cold
/// arm. See [`jit_preconditions_check_index_leaf`] for the contract.
///
/// The helper sets `native_pending_return` and its wrapper drops it again at
/// the hand-off (`native_return_handed_to_compiled_code`); the twin does
/// neither, which leaves the same state behind: the mirror is rooted
/// per-thread already, and the handoff root was never a liveness requirement.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_current_thread_leaf(vm_ptr: i64) -> i64 {
    let f = || {
        // SAFETY: as in `jit_thread_current_thread_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        if jit_direct_helper_refused(vm, &THREAD_CURRENT_THREAD_GATE) {
            return 0;
        }
        match jit_thread_mut() {
            Some((thread, _guard)) => match thread.java_thread_obj {
                Some(obj) => {
                    note_funnel_bypass_hit();
                    obj.as_ptr() as i64
                }
                None => 0,
            },
            None => 0,
        }
    };
    contain("jit_thread_current_thread_leaf", OnPanic::RecordFor { vm_ptr }, 0, f)
}

// ---------------------------------------------------------------------------
// SBR round 2, wave 8 (lane `tlocal`): `ThreadLocal.get()` thin direct call.
//
// Under `--real-jdk` `ThreadLocal.get()` is a reviewed `Intrinsic`
// (`native_tl_real_get`, `native-builtins/src/phases_early.rs`) that reads the
// REAL `Thread.threadLocals` table and runs the JDK's own `get(Thread)`
// bytecode for every case it does not answer. From compiled code that native
// crossing still costs a dispatch round trip; this helper answers the same hot
// case (the key found by reading the probe sequence, no stale entry met) with
// no dispatch at all and DECLINES everything else, to the same dispatch the
// site had before, which then runs the native (and through it the bytecode).
//
// The helper never decides anything the native would decide differently:
//   * `--compatible` and any mode where the real-field natives are not the
//     registered ones (`CRATONVM_TL_REAL_FAST=0`, `CRATONVM_TL_INTRINSIC=1`)
//     always decline (`thread_local_real_fast_active`);
//   * the receiver's class must be plain `ThreadLocal`, `InheritableThreadLocal`
//     or a subclass of either that neither overrides `get()` nor is a
//     `TerminatingThreadLocal` (the memo below, stamped with the class-set epoch);
//   * a missing map or table, a key not found, a cleared key on the probe path
//     (`expungeStaleEntry` is the bytecode's business) all decline.
// ---------------------------------------------------------------------------

/// The JDK's own private bodies, which the two helpers dispatch to DIRECTLY when
/// the receiver is a class the helper may answer for and the key is simply not
/// in the table (the first `get` of a thread, a `set` after `remove`, a cleared
/// key on the probe path): `ThreadLocal.get(Thread)` / `ThreadLocal.set(Thread,
/// Object)`. They have no registered native, so the dispatch runs the bytecode
/// without the round trip through the public method's reviewed `Intrinsic`
/// (which found nothing either and then invoked the same bytecode from inside a
/// native: measured, a `set` of a new key cost 2-3 us more that way).
pub static THREAD_LOCAL_GET_PRIVATE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "get",
    descriptor: "(Ljava/lang/Thread;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static THREAD_LOCAL_SET_PRIVATE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "set",
    descriptor: "(Ljava/lang/Thread;Ljava/lang/Object;)V",
    num_jit_args: 3,
    return_type: b'V',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// May the private `get(Thread)` / `set(Thread, Object)` bodies run for `this`
/// with no re-dispatch on its class? Only when no class between `this` and
/// `ThreadLocal` declares its own `get()` (and, for `set`, tracing is off, since
/// the public body then prints a stack trace); `TerminatingThreadLocal` keeps the
/// generic dispatch. Asked only on the miss path.
pub unsafe fn tl_miss_private_ok(vm: &SharedVm, this: ObjectRef, for_set: bool) -> bool {
    let Some(l) = tl_get_layout(vm) else {
        return false;
    };
    if for_set && l.trace {
        return false;
    }
    let cid = vm.mem.heap.class_id_of_validated(this);
    // The private `set(Thread, T)` is `ThreadLocal.set`'s body: a receiver
    // whose class overrides `set(Object)` must run its override instead.
    if for_set && !tl_set_receiver_ok(vm, &l, cid) {
        return false;
    }
    tl_get_receiver_kind(vm, &l, cid) != TL_MAP_BYTECODE
}

/// Synthetic call-site info for [`jit_thread_local_set_direct`]'s decline edge.
pub static THREAD_LOCAL_SET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "set",
    descriptor: "(Ljava/lang/Object;)V",
    num_jit_args: 2,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Synthetic call-site info for [`jit_thread_local_get_direct`]'s decline edge.
pub static THREAD_LOCAL_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "get",
    descriptor: "()Ljava/lang/Object;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Which `Thread` field `ThreadLocal.getMap` reads for a receiver class.
pub const TL_MAP_PLAIN: u8 = 0;
pub const TL_MAP_INHERITABLE: u8 = 1;
pub const TL_MAP_BYTECODE: u8 = 2;

/// Class ids and slot indices the fast path reads, resolved once per VM per
/// thread. Never a process global: another VM in the process may lay these
/// classes out differently (`vm` is the key).
#[derive(Clone, Copy)]
pub struct TlGetLayout {
    pub vm: usize,
    pub tl: ClassId,
    pub itl: ClassId,
    pub term: ClassId,
    /// `ThreadLocal$ThreadLocalMap$Entry`: the raw-read arm
    /// ([`tl_load_ref_field`]) believes a table element only when its header
    /// names this class (nio21 wave 17 L2).
    pub entry: ClassId,
    pub hash_slot: usize,
    pub thread_tl_slot: usize,
    pub thread_itl_slot: usize,
    pub table_slot: usize,
    pub value_slot: usize,
    /// `jdk.traceVirtualThreadLocals` was `true` when this was resolved: `set` then
    /// runs the JDK body (it prints a stack trace for virtual threads).
    pub trace: bool,
}

thread_local! {
    pub static TL_GET_LAYOUT: Cell<Option<TlGetLayout>> = const { Cell::new(None) };
    /// The receiver-kind memo ([`TlKindMemo`]): class-set epoch, then
    /// `(receiver class id, TL_MAP_* verdict)` rows.
    pub static TL_GET_KIND_MEMO: std::cell::RefCell<TlKindMemo> =
        const { std::cell::RefCell::new(TlKindMemo::new()) };
}

/// Is `jdk.traceVirtualThreadLocals` on, as the JDK's
/// `ThreadLocal.traceVirtualThreadLocals()` reads it (JDK 21 bytecode, checked
/// with `javap`: `value != null && (value.isEmpty() || Boolean.parseBoolean(value))`)?
/// `-Djdk.traceVirtualThreadLocals` with no value is an EMPTY string, which the
/// JDK counts as `true`; the layout used to read only the literal `true`, so
/// that spelling expanded and helper-served `set` without the JDK's
/// `dumpStackIfVirtualThread()` (tl21).
pub fn tl_trace_property_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.is_empty() || v.eq_ignore_ascii_case("true"))
}

/// [`tl_trace_property_value`] for this VM's system properties.
pub fn tl_trace_property(vm: &SharedVm) -> bool {
    tl_trace_property_value(
        vm.system_properties
            .read()
            .get("jdk.traceVirtualThreadLocals")
            .map(String::as_str),
    )
}

/// Resolve [`TlGetLayout`], or `None` while any of the classes is not loaded
/// (nothing is cached then, so the next call retries).
pub fn tl_get_layout(vm: &SharedVm) -> Option<TlGetLayout> {
    if let Some(l) = TL_GET_LAYOUT.with(Cell::get) {
        if l.vm == vm.vm_identity {
            return Some(l);
        }
    }
    let cm = vm.classes.class_manager.read();
    let tl = cm.get_loaded_class_id("java/lang/ThreadLocal")?;
    let itl = cm.get_loaded_class_id("java/lang/InheritableThreadLocal")?;
    let term = cm.get_loaded_class_id("jdk/internal/misc/TerminatingThreadLocal")?;
    let thread = cm.get_loaded_class_id("java/lang/Thread")?;
    let map = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap")?;
    let entry = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap$Entry")?;
    let store = &cm.class_store;
    let layout = TlGetLayout {
        vm: vm.vm_identity,
        tl,
        itl,
        term,
        entry,
        hash_slot: crate::vm::resolve_field_index_in_hierarchy(tl, "threadLocalHashCode", store)?,
        thread_tl_slot: crate::vm::resolve_field_index_in_hierarchy(thread, "threadLocals", store)?,
        thread_itl_slot: crate::vm::resolve_field_index_in_hierarchy(
            thread,
            "inheritableThreadLocals",
            store,
        )?,
        table_slot: crate::vm::resolve_field_index_in_hierarchy(map, "table", store)?,
        value_slot: crate::vm::resolve_field_index_in_hierarchy(entry, "value", store)?,
        trace: tl_trace_property(vm),
    };
    drop(cm);
    TL_GET_LAYOUT.with(|c| c.set(Some(layout)));
    Some(layout)
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_EARLY_LAYOUT` (page
/// `jr21-tl3-threadlocal-subclass-get-takes-the-thin-helper-FIXED-20261014.md`, the
/// `R21TlsubRows` / `R21Tl3Rows` shape), **default ON**, `=0` kill: read per
/// compile, never cached.
pub fn tl_early_layout_enabled() -> bool {
    true
}

/// The COMPILER's view of [`TlGetLayout`] (`jit_thread_local_ir_layout`):
/// the full layout when [`tl_get_layout`] has it (`true`), else -- under
/// [`tl_early_layout_enabled`] -- the same resolution with
/// `InheritableThreadLocal` and `jdk/internal/misc/TerminatingThreadLocal`
/// allowed to be unloaded (`false`; each missing one is class id `0`, which
/// names no class). Never cached: the helpers' per-thread layout must name
/// both, or their kind walk would answer `TL_MAP_PLAIN` for an
/// `InheritableThreadLocal` once it loads.
///
/// Why a compile may bake it: a class is loaded after its superclass, so while
/// either is unloaded no loaded class descends from it. Every class id the
/// expansion bakes (`ThreadLocal`, an admitted `SuppliedThreadLocal`, a
/// profiled subclass whose kind [`tl_ir_receiver_kind`] computed now) is
/// tested EXACTLY, and an object never changes class, so the verdict baked for
/// it stays true after either class loads. An `InheritableThreadLocal` site
/// keeps its call (layout word 15 is `0`). Before this, a method compiled
/// before the program's first `ThreadLocal.set` -- which loads
/// `TerminatingThreadLocal` -- refused every expansion for the life of its
/// code (`R21TlsubRows`, row `arith` first: ~150 ns a get in every arm).
pub fn tl_ir_layout(vm: &SharedVm) -> Option<(TlGetLayout, bool)> {
    if let Some(l) = tl_get_layout(vm) {
        return Some((l, true));
    }
    if !tl_early_layout_enabled() {
        return None;
    }
    let cm = vm.classes.class_manager.read();
    let tl = cm.get_loaded_class_id("java/lang/ThreadLocal")?;
    let none = ClassId::new(0);
    let itl = cm
        .get_loaded_class_id("java/lang/InheritableThreadLocal")
        .unwrap_or(none);
    let term = cm
        .get_loaded_class_id("jdk/internal/misc/TerminatingThreadLocal")
        .unwrap_or(none);
    let thread = cm.get_loaded_class_id("java/lang/Thread")?;
    let map = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap")?;
    let entry = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap$Entry")?;
    let store = &cm.class_store;
    let layout = TlGetLayout {
        vm: vm.vm_identity,
        tl,
        itl,
        term,
        entry,
        hash_slot: crate::vm::resolve_field_index_in_hierarchy(tl, "threadLocalHashCode", store)?,
        thread_tl_slot: crate::vm::resolve_field_index_in_hierarchy(thread, "threadLocals", store)?,
        thread_itl_slot: crate::vm::resolve_field_index_in_hierarchy(
            thread,
            "inheritableThreadLocals",
            store,
        )?,
        table_slot: crate::vm::resolve_field_index_in_hierarchy(map, "table", store)?,
        value_slot: crate::vm::resolve_field_index_in_hierarchy(entry, "value", store)?,
        trace: tl_trace_property(vm),
    };
    Some((layout, false))}

/// [`tl_get_receiver_kind`] for a layout from [`tl_ir_layout`]: a partial one
/// (`complete == false`) walks the chain afresh, past the per-thread memo,
/// which the helpers fill under the full layout only.
pub fn tl_ir_receiver_kind(vm: &SharedVm, l: &TlGetLayout, complete: bool, cid: ClassId) -> u8 {
    if complete {
        tl_get_receiver_kind(vm, l, cid)
    } else if cid == l.tl {
        TL_MAP_PLAIN
    } else {
        tl_get_compute_kind(vm, l, cid)
    }
}

/// The superclass walk behind [`tl_get_receiver_kind`]: `getMap` for the class
/// chain of `cid`, `TL_MAP_BYTECODE` for anything the helper must not answer.
pub fn tl_get_compute_kind(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> u8 {
    let cm = vm.classes.class_manager.read();
    let mut c = cid;
    let mut inheritable = false;
    for _ in 0..32 {
        if c == l.term {
            return TL_MAP_BYTECODE;
        }
        let Some(class) = cm.class_store.get(c) else {
            return TL_MAP_BYTECODE;
        };
        // A subclass that declares its own `get()` is dispatched to by the
        // site's virtual call; the helper bound against `ThreadLocal.get` must
        // not answer for it. JIT round 21 (lane tlsub, wave 21; correctness,
        // unconditional): a BRIDGE `get()Object` counts too. javac emits one
        // exactly when the class overrides `get` with a narrower return type
        // (`public int[] get()` in a `ThreadLocal<int[]>` subclass: the real
        // body is `get()[I`, which this descriptor never names), so skipping
        // bridges served such a receiver the raw map value, past its override
        // (`R21TlsubRows tl-get-override`, `fact override-gets`). `ThreadLocal`
        // is public, so no visibility bridge forwards to its own `get`.
        if c != l.tl && class.find_method("get", "()Ljava/lang/Object;").is_some() {
            return TL_MAP_BYTECODE;
        }
        if c == l.itl {
            inheritable = true;
        }
        if c == l.tl {
            return if inheritable {
                TL_MAP_INHERITABLE
            } else {
                TL_MAP_PLAIN
            };
        }
        match class.superclass {
            Some(parent) => c = parent,
            None => return TL_MAP_BYTECODE,
        }
    }
    TL_MAP_BYTECODE
}

/// JIT round 21 (lane tlsub, TL321-4): does a class on `cid`'s superclass
/// chain below `ThreadLocal` declare its own `set(Object)` (a bridge too)? Such
/// a receiver's `set` is dispatched to that body, so the optimizing tier's
/// `set` expansion must not answer for it. A chain that does not reach
/// `ThreadLocal` (or an unknown class) answers `true`, the refusing answer.
/// Compile time only (`jit_thread_local_ir_layout`'s one-class query).
pub fn tl_class_declares_set(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> bool {
    let cm = vm.classes.class_manager.read();
    let mut c = cid;
    for _ in 0..32 {
        if c == l.tl {
            return false;
        }
        let Some(class) = cm.class_store.get(c) else {
            return true;
        };
        // A bridge counts (JIT round 21, lane tlsub, wave 21; correctness,
        // unconditional): `set(int[])` in a `ThreadLocal<int[]>` subclass is
        // reached through its bridge `set(Object)`, as `tl_get_compute_kind`'s
        // `get` override.
        if class.find_method("set", "(Ljava/lang/Object;)V").is_some() {
            return true;
        }
        match class.superclass {
            Some(parent) => c = parent,
            None => return true,
        }
    }
    true
}

/// The `Thread` field `ThreadLocal.getMap` reads for a receiver of class `cid`,
/// memoised per thread under the class-set epoch (an unload frees a `ClassId`
/// for reuse, so an entry is believed only under the epoch it was proven in).
pub fn tl_get_receiver_kind(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> u8 {
    if cid == l.tl {
        return TL_MAP_PLAIN;
    }
    if cid == l.itl {
        return TL_MAP_INHERITABLE;
    }
    tl_kind_memo(vm, cid.as_u32(), || tl_get_compute_kind(vm, l, cid))
}

/// JIT round 21 (lane tlsub, wave 21; correctness, unconditional): may the
/// `set` helper (`jit_thread_local_set_direct`, both its found-key store and
/// its private `set(Thread, T)` miss arm) answer for a receiver of `cid`? Only
/// when no class below `ThreadLocal` declares `set(Object)`
/// ([`tl_class_declares_set`], a bridge included): the site's virtual call
/// dispatches to such an override, which the helper used to skip whenever the
/// key was present (it screened only `get` overrides, through the kind).
/// `ThreadLocal` and `InheritableThreadLocal` declare no override. Memoised
/// beside the kinds under the bit-31-tagged key (class ids are dense indices,
/// far below `1 << 31`; an id with that bit set is refused).
pub fn tl_set_receiver_ok(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> bool {
    if cid == l.tl || cid == l.itl {
        return true;
    }
    let raw = cid.as_u32();
    if raw & TL_SET_MEMO_TAG != 0 {
        return false;
    }
    tl_kind_memo(vm, raw | TL_SET_MEMO_TAG, || {
        u8::from(!tl_class_declares_set(vm, l, cid))
    }) == 1
}

/// The key bit [`tl_set_receiver_ok`]'s rows carry in `TL_GET_KIND_MEMO`.
pub const TL_SET_MEMO_TAG: u32 = 1 << 31;

/// Rows of [`TlKindMemo`] (a power of two).
pub const TL_KIND_MEMO_SLOTS: usize = 64;

/// tl21 (TLSUB21-7): the per-thread receiver-kind memo, direct-mapped. It was a
/// 16-row FIFO list shared by the `get` kinds and the `set` screens, so an
/// application rotating more than about eight `ThreadLocal` subclasses
/// through the helpers (Spring and Hibernate holders) evicted every row before
/// it was asked again and re-walked the class chain under the class-manager
/// read lock on every call. Here a key lives in one slot, derived from its
/// class id with the set tag as the low bit (a class's `get` and `set` rows
/// never share a slot), so only classes whose ids collide in 32 evict one
/// another, and a lookup is one compare. Key `0` names no class and is never
/// stored. The rows are believed only under the class-set epoch they were
/// proven in (an unload frees a `ClassId` for reuse).
pub struct TlKindMemo {
    pub epoch: u64,
    /// `(key, TL_MAP_* verdict or the set screen's 0 / 1)`; key `0` is an empty row.
    pub slots: [(u32, u8); TL_KIND_MEMO_SLOTS],
}

impl TlKindMemo {
    pub const fn new() -> Self {
        TlKindMemo {
            epoch: 0,
            slots: [(0, 0); TL_KIND_MEMO_SLOTS],
        }
    }

    /// The slot of `key`: the class id shifted up one, the set tag below it.
    pub fn slot(key: u32) -> usize {
        let id = usize::try_from(key & !TL_SET_MEMO_TAG).unwrap_or(0);
        let tag = usize::from(key & TL_SET_MEMO_TAG != 0);
        ((id << 1) | tag) & (TL_KIND_MEMO_SLOTS - 1)
    }

    /// The verdict stored for `key` under `epoch`. A memo stamped with another
    /// epoch is emptied first, so a stale verdict is never served.
    pub fn lookup(&mut self, epoch: u64, key: u32) -> Option<u8> {
        if self.epoch != epoch {
            self.slots = [(0, 0); TL_KIND_MEMO_SLOTS];
            self.epoch = epoch;
            return None;
        }
        if key == 0 {
            return None;
        }
        let (stored, kind) = self.slots[Self::slot(key)];
        (stored == key).then_some(kind)
    }

    /// Store `kind` for `key`, only under the epoch the memo holds.
    pub fn insert(&mut self, epoch: u64, key: u32, kind: u8) {
        if self.epoch == epoch && key != 0 {
            self.slots[Self::slot(key)] = (key, kind);
        }
    }
}

/// `TL_GET_KIND_MEMO`'s lookup-or-compute for `key`, under the class-set epoch.
pub fn tl_kind_memo(vm: &SharedVm, key: u32, compute: impl FnOnce() -> u8) -> u8 {
    let epoch = typecheck_memo_epoch(vm);
    if let Some(kind) = TL_GET_KIND_MEMO.with(|m| m.borrow_mut().lookup(epoch, key)) {
        return kind;
    }
    let kind = compute();
    if typecheck_memo_epoch(vm) == epoch {
        TL_GET_KIND_MEMO.with(|m| m.borrow_mut().insert(epoch, key, kind));
    }
    kind
}


/// The entry of the current thread's real table whose referent IS `this`, or
/// `None` (no map or table, key absent, a cleared key on the probe path, a
/// receiver class the helper must not answer for). Allocates nothing and cannot
/// reach a safepoint.
pub unsafe fn tl_find_entry(
    vm: &SharedVm,
    l: &TlGetLayout,
    mirror: ObjectRef,
    this: ObjectRef,
) -> Option<ObjectRef> {
    let heap = &vm.mem.heap;
    let cid = heap.class_id_of_validated(this);
    let field = match tl_get_receiver_kind(vm, l, cid) {
        TL_MAP_PLAIN => l.thread_tl_slot,
        TL_MAP_INHERITABLE => l.thread_itl_slot,
        _ => return None,
    };
    // nio21 wave 17 L2: every read below goes through the raw-displacement
    // readers (`CRATONVM_JIT_TL_RAW_FIELDS`, default ON), which fall back to
    // the generic accessor per read when the switch is off or the body is not
    // compact. A `None` from any of them declines, exactly like a miss.
    let raw = crate::runtime::env_cache::jit_tl_raw_fields();
    let map = tl_load_ref_field(heap, raw, mirror, field)??;
    let table = tl_load_ref_field(heap, raw, map, l.table_slot)??;
    // The element reads below index the body directly, so the table must
    // really be a reference array (the generic `get_array_element` checked
    // this per element; once per call is the same question).
    let table_header = &*(table.as_ptr() as *const cratonvm_types::ObjectHeader);
    if table_header.kind() != cratonvm_types::ObjectKind::Array
        || table_header.element_type() != ArrayElementType::Reference
    {
        return None;
    }
    let len = table_header.array_length() as usize;
    if len == 0 || !len.is_power_of_two() {
        return None;
    }
    let hash = tl_load_int_field(heap, raw, this, l.hash_slot)?;
    let mut i = (hash as u32 as usize) & (len - 1);
    for _ in 0..len {
        // An empty slot: the key is not in the table.
        let entry = tl_load_ref_element(heap, raw, table, i)??;
        // An element that is not an `Entry` is nothing this helper may read
        // slot 0 of as a referent (an autobox wrapper, a corrupt word): the
        // bytecode decides.
        if heap.class_id_of(entry) != l.entry {
            return None;
        }
        // `Reference.referent` is slot 0 (`REF_FIELD_REFERENT`). Compared,
        // never handed out: this is `refersTo(key)`, so no SATB keep-alive
        // is owed (`jit_getfield_body` logs the referent only because it
        // RETURNS it to compiled code).
        match tl_load_ref_field(heap, raw, entry, 0)? {
            Some(referent) => {
                if referent.as_ptr() == this.as_ptr() {
                    return Some(entry);
                }
            }
            // A cleared key: `expungeStaleEntry` is the bytecode's business.
            None => return None,
        }
        i = if i + 1 < len { i + 1 } else { 0 };
    }
    None
}

/// The raw-displacement reference-field read of the `ThreadLocal` helpers
/// (nio21 wave 17 L2, `CRATONVM_JIT_TL_RAW_FIELDS`): `Some(None)` a null
/// field, `Some(Some(r))` a reference, `None` decline.
///
/// With `raw` on and a COMPACT body, the field's displacement comes from
/// [`jit_compact_field_slot`] -- the proof cache the JIT's own `getfield`
/// helper reads, keyed by the receiver's class id, the field index and the
/// layout generation, so a redefinition or a re-registered layout is never
/// believed -- and the slot is read through
/// [`VmHeap::load_ref_slot_barriered`], the collector's load barrier (plain
/// `read_ref_slot` on Generational, G1 and an unarmed ZGC; the healing barrier
/// on an armed ZGC heap; the compressed-oop width either way). That is the
/// read `jit_getfield`'s compact-reference arm makes. What the generic
/// `get_field` additionally does for a reference slot is unbox an
/// `AUTOBOX_CLASS_ID` wrapper; this declines on one instead.
///
/// Otherwise (switch off, a legacy body) it is the generic accessor, as before.
///
/// # Safety
///
/// `obj` is a live object of this heap, and no safepoint can intervene
/// before the caller stops using what this returns (the helpers allocate
/// nothing and never reach one).
#[inline(always)]
pub unsafe fn tl_load_ref_field(
    heap: &VmHeap,
    raw: bool,
    obj: ObjectRef,
    slot: usize,
) -> Option<Option<ObjectRef>> {
    if raw {
        if let Some((off, storage)) = jit_compact_field_slot(obj.as_ptr() as i64, slot as i64) {
            if !storage.is_reference() {
                return None;
            }
            // SAFETY: `off` is the compact displacement of reference field
            // `slot` in `obj`'s own registered layout, so the slot lies inside
            // the live body at the current reference width.
            let word = heap.load_ref_slot_barriered(obj.as_ptr().add(off));
            return tl_checked_ref(word);
        }
        // r23w1 (R23W1L4-4): a second compact reader used to follow here
        // (`VmHeap::load_ref_field_compact`, "when the proof cache has no
        // entry yet"). It never answered: `jit_compact_field_slot` fills the
        // cache on a miss itself, so it returns `None` only for a legacy
        // body or a slot with no compact layout row, and the heap reader
        // (compact flag, then the same layout lookup behind a domain screen
        // that can only narrow it) declines exactly those. It cost a header
        // read and a layout lookup per legacy receiver for nothing.
    }
    match heap.get_field(obj, slot) {
        Value::Object(v) => Some(v),
        _ => None,
    }
}

/// A loaded reference word as the `ThreadLocal` helpers may use it: null,
/// a plausible non-wrapper object, or `None` (decline).
#[inline(always)]
pub unsafe fn tl_checked_ref(word: u64) -> Option<Option<ObjectRef>> {
    if word == 0 {
        return Some(None);
    }
    if !cratonvm_types::plausible_heap_pointer(word) {
        return None;
    }
    // SAFETY: a plausible non-null word loaded from a live reference slot is
    // the address of a live object, whose first bytes are its header.
    if cratonvm_gc::autobox::header_is_wrapper(&*(word as *const cratonvm_types::ObjectHeader)) {
        return None;
    }
    Some(Some(ObjectRef::from_raw(word as *mut u8)))
}

/// The `int` twin of [`tl_load_ref_field`], for
/// `ThreadLocal.threadLocalHashCode`: `None` declines.
///
/// # Safety
///
/// As [`tl_load_ref_field`].
#[inline(always)]
pub unsafe fn tl_load_int_field(heap: &VmHeap, raw: bool, obj: ObjectRef, slot: usize) -> Option<i32> {
    if raw {
        if let Some((off, storage)) = jit_compact_field_slot(obj.as_ptr() as i64, slot as i64) {
            if !matches!(storage, cratonvm_types::FieldStorageKind::Int) {
                return None;
            }
            // SAFETY: a 4-byte `Int` field at its compact displacement inside
            // the live body; `Relaxed` through an atomic of the field's own
            // width, as `field_fast`'s `load_u32` and `read_compact_field` do.
            let p = obj.as_ptr().add(off) as *const std::sync::atomic::AtomicU32;
            return Some((*p).load(std::sync::atomic::Ordering::Relaxed) as i32);
        }
    }
    match heap.get_field(obj, slot) {
        Value::Int(h) => Some(h),
        _ => None,
    }
}

/// Element `i` of the reference array `table` (already checked to be one,
/// with `i < length`), through the collector's load barrier when `raw` is on,
/// else through the generic accessor. As [`tl_load_ref_field`] for the answer.
///
/// # Safety
///
/// `table` is a live reference array of this heap and `i` is below its length.
#[inline(always)]
pub unsafe fn tl_load_ref_element(
    heap: &VmHeap,
    raw: bool,
    table: ObjectRef,
    i: usize,
) -> Option<Option<ObjectRef>> {
    if raw {
        // SAFETY: the caller checked the kind, the element type and the bound;
        // the element lies at `ARRAY_DATA_OFFSET + i * ref_element_size()`,
        // the address `jit_aaload_body` reads (it spells the offset
        // `HEADER_SIZE`, which `ARRAY_DATA_OFFSET` equals).
        let slot = table
            .as_ptr()
            .add(cratonvm_types::ARRAY_DATA_OFFSET + i * ref_element_size());
        return tl_checked_ref(heap.load_ref_slot_barriered(slot));
    }
    match heap.get_array_element(table, i) {
        Ok(Value::Object(v)) => Some(v),
        _ => None,
    }
}

/// `Some(Some(value))` the value, `Some(None)` a null value, `None` declines.
pub unsafe fn tl_get_fast(
    vm: &SharedVm,
    mirror: ObjectRef,
    this: ObjectRef,
) -> Option<Option<ObjectRef>> {
    let l = tl_get_layout(vm)?;
    let entry = tl_find_entry(vm, &l, mirror, this)?;
    tl_load_ref_field(
        &vm.mem.heap,
        crate::runtime::env_cache::jit_tl_raw_fields(),
        entry,
        l.value_slot,
    )
}


/// Whether the real-field `ThreadLocal` natives are the registered ones in this
/// VM. Env-derived and process-stable (the registrar reads the same switches
/// once at registration), so one memoised bool; the policy half is per VM.
pub fn thread_local_real_fast_serves(vm: &SharedVm) -> bool {
    pub static ENV_ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    let env_on = cratonvm_jit::memo_flag::memo_bool(&ENV_ON, || {
        cratonvm_native_builtins::phases_early::thread_local_real_fast_active(true)
    });
    env_on && crate::vm::dispatch_policy(vm).is_jdk_only()
}

/// Thin direct-call target for JIT `invokevirtual ThreadLocal.set(Object)` /
/// `InheritableThreadLocal.set(Object)` sites (SBR round 2, wave 8, lane
/// `tlocal`): overwrites the value of an entry that is already in the current
/// thread's real table, exactly as `ThreadLocalMap.set`'s found-key arm does
/// (`e.value = value`), with the SATB pre-barrier on the overwritten reference
/// and the heap's own card-marking store. Everything else (no map, key absent
/// or stale, a class the helper must not answer for, tracing on, a value that
/// does not validate) declines to the generic dispatch.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_local_set_direct(
    vm_ptr: i64,
    receiver: i64,
    value: i64,
) -> i64 {
    let f = || jit_thread_local_set_direct_body(vm_ptr, receiver, value);
    contain(
        "jit_thread_local_set_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_thread_local_set_direct`], run under its panic guard.
pub unsafe fn jit_thread_local_set_direct_body(vm_ptr: i64, receiver: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    // `(receiver, thread mirror)` once both validated: the miss path below runs the
    // JDK's private body with them.
    let mut miss: Option<(ObjectRef, ObjectRef, Value)> = None;
    'fast: {
        if (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
            break 'fast;
        }
        if !thread_local_real_fast_serves(vm) {
            break 'fast;
        }
        let Some(this) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let new_value = if value == 0 {
            Value::Object(None)
        } else {
            if (value as u64 & 0x7) != 0 || (value as u64) >= (1u64 << 48) {
                break 'fast;
            }
            let Some(object) = vm.mem.heap.is_object_address(value as usize) else {
                break 'fast;
            };
            Value::Object(Some(object))
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let Some(mirror) = thread.java_thread_obj else {
            break 'fast;
        };
        let Some(l) = tl_get_layout(vm) else {
            break 'fast;
        };
        if l.trace {
            break 'fast;
        }
        // JIT round 21 (lane tlsub, wave 21; correctness, unconditional): a
        // receiver whose class overrides `set(Object)` takes the generic
        // (virtual) dispatch, never the store nor the private miss arm.
        if !tl_set_receiver_ok(vm, &l, vm.mem.heap.class_id_of_validated(this)) {
            break 'fast;
        }
        let Some(entry) = tl_find_entry(vm, &l, mirror, this) else {
            miss = Some((this, mirror, new_value));
            break 'fast;
        };
        let heap = &vm.mem.heap;
        // `ThreadLocalMap.set`'s found-key arm: `e.value = value`. The SATB
        // pre-barrier names the OLD reference first, as `jit_putfield_object`
        // does; `set_field` is the compact-aware store with the card barrier.
        //
        // JIT round 23 wave 1, lane L4 (w17-L2 item 3,
        // `CRATONVM_JIT_TL_SET_SINGLE_SATB`): only where the store accessor
        // does not log the old value itself -- Generational, while its queue
        // is active. G1's and ZGC's `set_field` read and log it while marking,
        // so the explicit pre-read was a second log (and a field read on every
        // call while nothing marks), as the interpreter's `aastore` already
        // skips it on G1.
        let explicit_pre = !crate::runtime::env_cache::jit_tl_set_single_satb()
            || (heap.is_generational() && heap.satb_armed());
        if explicit_pre {
            let old = heap.get_field(entry, l.value_slot);
            if let Value::Object(Some(_)) = old {
                heap.satb_barrier(old);
            }
        }
        heap.set_field(entry, l.value_slot, new_value);
        return 0;
    }
    if let Some((this, mirror, _new_value)) = miss {
        if tl_miss_private_ok(vm, this, true) {
            let args = [receiver, mirror.as_ptr() as i64, value];
            return jit_invoke_dispatch(
                vm_ptr,
                &THREAD_LOCAL_SET_PRIVATE_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                3,
            );
        }
    }
    let args = [receiver, value];
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_LOCAL_SET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Thin direct-call target for JIT `invokevirtual ThreadLocal.get()` /
/// `InheritableThreadLocal.get()` sites (SBR round 2, wave 8, lane `tlocal`).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_local_get_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_thread_local_get_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_thread_local_get_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_thread_local_get_direct`], run under its panic guard.
pub unsafe fn jit_thread_local_get_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // The JEP-358 action the dispatch this helper stands in for records.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let mut miss: Option<(ObjectRef, ObjectRef)> = None;
    'fast: {
        if (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
            break 'fast;
        }
        if !thread_local_real_fast_serves(vm) {
            break 'fast;
        }
        let Some(this) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let Some(mirror) = thread.java_thread_obj else {
            break 'fast;
        };
        match tl_get_fast(vm, mirror, this) {
            Some(Some(object)) => {
                // Object-return handoff root, same contract as every other thin
                // helper that hands a reference to compiled code.
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(None) => return 0,
            None => miss = Some((this, mirror)),
        }
    }
    if let Some((this, mirror)) = miss {
        if tl_miss_private_ok(vm, this, false) {
            let args = [receiver, mirror.as_ptr() as i64];
            return jit_invoke_dispatch(
                vm_ptr,
                &THREAD_LOCAL_GET_PRIVATE_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                2,
            );
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_LOCAL_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, opt-in): the LEAF TWIN of
/// [`jit_thread_local_get_direct`]: its `'fast` arm verbatim (`tl_get_fast`
/// over the current thread's REAL table: field and array reads, the per-thread
/// layout memo, the class store under a read lock -- "allocates nothing and
/// cannot reach a safepoint"), and `-1` -- never an aligned reference -- where
/// the helper would raise the null receiver's NPE or dispatch (a miss, the
/// private-get route, a receiver class it must not answer for). The contract
/// is [`jit_preconditions_check_index_leaf`]'s.
///
/// The helper also sets the object-return handoff root
/// (`native_pending_return`) and its wrapper drops it at once
/// (`native_return_handed_to_compiled_code`); the twin does neither, which
/// leaves the same state behind: the value is reachable from the thread's own
/// `threadLocals` table, and the tail site returns it with no safepoint between.
///
/// Published through the well-known channel only where the fast arm can serve
/// at all (`thread_local_real_fast_serves`: `--real-jdk` with the real-field
/// natives), so a `--compatible` site keeps its ordinary call instead of a twin
/// that would always decline.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_local_get_leaf(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || {
        if receiver == 0 || (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
            return -1;
        }
        // SAFETY: as in `jit_thread_local_get_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        if !thread_local_real_fast_serves(vm) {
            return -1;
        }
        let Some(this) = vm.mem.heap.is_object_address(receiver as usize) else {
            return -1;
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            return -1;
        };
        let Some(mirror) = thread.java_thread_obj else {
            return -1;
        };
        match tl_get_fast(vm, mirror, this) {
            Some(Some(object)) => object.as_ptr() as i64,
            Some(None) => 0,
            None => -1,
        }
    };
    contain("jit_thread_local_get_leaf", OnPanic::RecordFor { vm_ptr }, -1, f)
}

/// Synthetic call-site infos for the exact-HashMap thin direct-call helpers'
/// fallback/dispatch paths (perf/halfgap-20260717).
pub static CTOR_NEW_INSTANCE_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/reflect/Constructor",
    method_name: "newInstance",
    descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub static HASHMAP_PUT_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/HashMap",
    method_name: "put",
    descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub static HASHMAP_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/HashMap",
    method_name: "get",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub static CONCURRENT_HASHMAP_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/concurrent/ConcurrentMap",
    method_name: "get",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 2,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static STRING_LATIN1_LOWER_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/StringLatin1",
    method_name: "toLowerCase",
    descriptor: "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Every registry triple this file can reach through a **thin direct-call
/// helper**, i.e. every slot whose `invocations` column is a floor rather than
/// a total once the JIT is wired in this VM.
///
/// A site `jit::try_compile` binds to one of these helpers reaches the native's
/// semantics without ever redeeming a `NativeMethodId`, so
/// [`NativeMethodRegistry::record_invocation`] is never called for it. Only the
/// helpers' own `jit_invoke_dispatch` fallbacks count, and a warm site does not
/// take them. MEASURED
/// (`docs/feature-designs/real-jdk-mode.md`
/// §2): 100,000 `HashMap.put` calls report **2,000** with the JIT on against
/// **100,000** under `--nojit`, and the JIT figure does not move with the
/// workload size or with `CRATONVM_JIT_THRESHOLD` — it is frozen at the count
/// reached before the enclosing loop was compiled.
///
/// **This list is the source-verified sweep of this file, and it is longer than
/// the two helpers `G33-1` §8 N1 nominated.** `jit_integer_value_of_direct`,
/// `jit_integer_int_value_direct` and `jit_thread_current_thread_direct` return
/// from their fast arms without counting too. `G33-1` §2's remark that "the
/// `Integer` siblings in the same file route through `call_integer_native_raw`"
/// is true of the `jit_invoke_dispatch` route ONLY: the standalone direct
/// helpers are separate bodies that never enter that wrapper. The same section
/// read `StringLatin1.toLowerCase` counting exactly and concluded "the
/// direct-call family is not uniformly broken"; the body of
/// `jit_string_latin1_to_lower_direct` contains no census call at all, so that
/// probe's exact figure must have come from sites that never bound the helper,
/// not from the helper counting.
///
/// Both `ConcurrentMap.get` and `ConcurrentHashMap.get` are listed. They are
/// two registry slots holding the same `native_chm_get`;
/// `CONCURRENT_HASHMAP_GET_DIRECT_INFO` names the interface while
/// `jit_concurrent_hashmap_get_direct`'s fast arm only fires for an exact
/// `ConcurrentHashMap` receiver, so the row that loses the call is whichever of
/// the two the fallback would have resolved. A triple that is not registered in
/// this VM simply does not resolve and is not marked.
///
/// [`NativeMethodRegistry::record_invocation`]: cratonvm_native_api::NativeMethodRegistry::record_invocation
pub const DIRECT_CALL_HELPER_NATIVES: [(&str, &str, &str); 78] = [
    // Typed VarHandle and primitive-copy helpers bypass these erased native slots.
    (
        "java/lang/invoke/VarHandle",
        "get",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "getVolatile",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "getOpaque",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "getAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "set",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "setVolatile",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "setRelease",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "setOpaque",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "compareAndSet",
        "([Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    ),
    // `jit_integer_value_of_direct` — TLAB-allocated wrapper, and the cold arm
    // that calls `intrinsic_integer_value_of` through `safe_native_call`
    // directly. Neither counts.
    ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;"),
    // `jit_integer_int_value_direct` — heap-validated field-0 read.
    ("java/lang/Integer", "intValue", "()I"),
    // `jit_long_value_of_direct` / `jit_long_long_value_direct` — the `Long`
    // twins of the two rows above, on the same terms.
    ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;"),
    ("java/lang/Long", "longValue", "()J"),
    // `jit_hashmap_put_direct` / `jit_hashmap_get_direct` — the overlay probe
    // and the `safe_native_call_prevalidated_objects` arm below it.
    (
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    // `jit_string_latin1_to_lower_direct` — delegates to
    // `lang_string::jit_string_to_lower_case` without touching the registry.
    (
        "java/lang/StringLatin1",
        "toLowerCase",
        "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;",
    ),
    // `jit_concurrent_hashmap_get_direct` — reaches `native_chm_get` through
    // `safe_native_call_prevalidated_objects` (H7-1), which restores the funnel
    // but still carries no `NativeMethodId`, so `record_invocation` never fires
    // and this row's count stays a floor.
    (
        "java/util/concurrent/ConcurrentMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    // `jit_thread_current_thread_direct` — returns the per-thread mirror. It
    // bumps `JIT_FUNNEL_BYPASS_HITS`, which is a different instrument: that
    // counter answers "did compiled code take this path", not "how many
    // Java-level calls did this registry slot serve".
    ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
    // SBR2 W8 `jit_thread_local_get_direct` -- the hot arm answers from the real
    // `Thread.threadLocals` table with no dispatch, so no registry call counts it.
    ("java/lang/ThreadLocal", "get", "()Ljava/lang/Object;"),
    (
        "java/lang/InheritableThreadLocal",
        "get",
        "()Ljava/lang/Object;",
    ),
    ("java/lang/ThreadLocal", "set", "(Ljava/lang/Object;)V"),
    (
        "java/lang/InheritableThreadLocal",
        "set",
        "(Ljava/lang/Object;)V",
    ),
    // ---------------------------------------------------------------
    // 2026-09-22. The nine rows above were this file's population when
    // the list was written; eight thin helpers have been added since and
    // none of them added a row, so eight families of census counts have
    // been reported as exact totals while a compiled site bypassed them.
    //
    // Swept from the `JitInvokeInfo` statics in this file, which is what
    // `every_thin_direct_call_helper_is_on_the_census_bypass_list` now
    // enumerates, so a ninth helper with a new info and no row here is a
    // red test rather than a silently exact-looking row.
    //
    // The `--real-jdk` half is new and is why this was noticed. Before
    // 2026-09-22 `jit_direct_helper_refused` declined every one of the
    // guarded helpers in strict mode, so the call took
    // `jit_invoke_dispatch` and the tail counted it -- a strict run's
    // census for these rows really was exact, which is how
    // `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
    // could read `MessageDigest.update (B)V` at one dispatch per call and
    // prove the refusal ran the native anyway. Now that the helpers serve
    // in both modes, both modes need the floor.
    // `jit_md_update_byte_direct` -- the triple that page measures.
    ("java/security/MessageDigest", "update", "(B)V"),
    // `jit_dbb_get_byte_direct` / `jit_dbb_put_byte_direct`.
    ("java/nio/ByteBuffer", "get", "(I)B"),
    ("java/nio/ByteBuffer", "put", "(IB)Ljava/nio/ByteBuffer;"),
    // `jit_buffer_session_direct`. Its FIELD-READ arm answers
    // without the registry at all, which is a bypass of exactly the
    // shape this list exists for.
    (
        "java/nio/Buffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    // `jit_preconditions_check_index_direct` -- the in-range arm
    // returns without dispatching; the throwing arm does dispatch, so
    // this row is a floor rather than a zero.
    (
        "jdk/internal/util/Preconditions",
        "checkIndex",
        "(IILjava/util/function/BiFunction;)I",
    ),
    // `jit_reachability_fence_direct` -- a `black_box` and nothing
    // else, so it never reaches the registry.
    (
        "java/lang/ref/Reference",
        "reachabilityFence",
        "(Ljava/lang/Object;)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getByteInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putByteInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V",
    ),
    // The eight `ScopedMemoryAccess.*Unaligned` helpers and their
    // eight `*Internal` twins, all served by
    // `scoped_memory_{read,write}_bytes`.
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getIntUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getShortUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getLongUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getCharUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putIntUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putShortUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putLongUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putCharUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getIntUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getShortUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getLongUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getCharUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putIntUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putShortUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putLongUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putCharUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    ),
    // The eight `jit_unsafe_*_direct` scalar accessors. Served
    // unconditionally in both modes since 2026-09-18 -- these methods
    // have no bytecode to shadow -- and never counted.
    (
        "jdk/internal/misc/Unsafe",
        "getByte",
        "(Ljava/lang/Object;J)B",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getShort",
        "(Ljava/lang/Object;J)S",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getInt",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLong",
        "(Ljava/lang/Object;J)J",
    ),
    // SBR2 W8: the two reference-array reads.
    (
        "jdk/internal/misc/Unsafe",
        "getReference",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getReferenceVolatile",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putByte",
        "(Ljava/lang/Object;JB)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putShort",
        "(Ljava/lang/Object;JS)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putInt",
        "(Ljava/lang/Object;JI)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLong",
        "(Ljava/lang/Object;JJ)V",
    ),
    // 2026-09-24: `jit_unsafe_cas_{int,long}_direct` -- the served arm is a
    // hardware CAS with no registry call.
    (
        "jdk/internal/misc/Unsafe",
        "compareAndSetInt",
        "(Ljava/lang/Object;JII)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndSetLong",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    // JIT round 17 wave 1, lane nio: `jit_object_get_class_direct` -- the
    // served arm reads the mirror table with no registry call (opt-in bind).
    ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
    // nio21 wave 3, lane L4: the ten `HttpClient`-census rows. Their served
    // arms call the `NativeContext` accessors directly, never the registry.
    (
        "jdk/internal/misc/Unsafe",
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putReferenceVolatile",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("jdk/internal/misc/Unsafe", "fullFence", "()V"),
    (
        "java/lang/ref/Reference",
        "refersTo0",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/ref/PhantomReference",
        "refersTo0",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/ref/Reference",
        "get0",
        "()Ljava/lang/Object;",
    ),
    (
        "java/lang/Thread",
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
    ),
    ("java/lang/System", "currentTimeMillis", "()J"),
    ("java/lang/System", "nanoTime", "()J"),
    ("java/lang/Object", "notify", "()V"),
    ("java/lang/Object", "notifyAll", "()V"),
    ("jdk/internal/misc/Unsafe", "unpark", "(Ljava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "park", "(ZJ)V"),
];

/// Declare every [`DIRECT_CALL_HELPER_NATIVES`] slot's `invocations` count
/// incomplete for `shared`, so the census labels those rows a floor instead of
/// leaving the reader to know this file exists.
///
/// # Why here, and why not a per-call counter
///
/// This runs from [`build_helpers_opt`], at the point the helper addresses are
/// published into `cratonvm_jit`'s `*_DIRECT_FN` cells — the closest thing this
/// side of the boundary has to a bind event, and the last moment before a
/// compiled site can be pointed at one of these bodies. It is cold by
/// construction: a JIT compile is orders of magnitude more expensive than the
/// forty triple hashes below, and the latch makes even those once per (VM,
/// registry generation).
///
/// The alternative — `record_invocation` inside each helper — was measured and
/// rejected. `G33-1` §5: **+9.2 ns/call** against a 1.25 ns baseline, ~6% of the
/// ~141 ns Rust native-call boundary but plausibly *the entire margin* a thin
/// direct-call helper exists to buy. Paying it here would partly undo
/// `perf/halfgap-20260717`. A bit set once and read only at report time costs
/// nothing per call and turns a number that lies into a number that says "at
/// least".
///
/// # What the bit claims
///
/// That a bypassing path is **wired** for the slot, not that a bypassing
/// dispatch has **happened**. That is the statically true statement, and it is
/// the one that survives: whether a given compile binds a given site is a fact
/// no report-time reader can recover. Under `--nojit` nothing calls
/// `build_helpers*` at all, so those runs keep an unmarked — and, for these
/// slots, exact — census.
///
/// # The latch
///
/// Keyed on `(vm_identity, registry generation)`, the same pair
/// `jit_invoke_dispatch`'s site cache uses, so a `RegisterNatives` that adds one
/// of these triples after the first compile is picked up on the next one. Races
/// between two compiling threads can only cause the marking to be *repeated*,
/// never skipped for a VM that was never marked: the VM's latch word is stored
/// after the marks, so observing it means some thread has already marked that VM.
/// `mark_invocations_incomplete` is idempotent and sticky, so a repeat is a
/// no-op.
///
/// nio21 wave 8 L2: the latch was three process statics (`MARKED_ANY`,
/// `MARKED_VM`, `MARKED_GENERATION`) keyed on the LAST VM marked, so two VMs
/// compiling in turn re-marked on every compile. It is now one word per VM,
/// `JitRealm::direct_helper_gates`' marked-generation latch.
pub fn mark_direct_call_helper_natives_incomplete(shared: &crate::vm::SharedVm) {
    let registry = &shared.natives.native_methods;
    let generation = registry.generation();
    let latch = &shared.jit.direct_helper_gates;
    if latch.helper_natives_marked_at(generation) {
        return;
    }
    mark_direct_call_helper_natives_incomplete_in(registry);
    latch.note_helper_natives_marked(generation);
}

/// The registry half of [`mark_direct_call_helper_natives_incomplete`], split
/// out so the marking can be driven against a registry built in a test rather
/// than only through a live `SharedVm` and a JIT compile.
///
/// Returns how many of [`DIRECT_CALL_HELPER_NATIVES`] resolved in this registry.
/// A triple that does not resolve is not an error: `register_collections_natives`
/// is a separate registrar from `register_essential_natives`, and a VM that
/// registered neither still gets a correct — empty — set of marks.
pub fn mark_direct_call_helper_natives_incomplete_in(
    registry: &cratonvm_native_api::NativeMethodRegistry,
) -> usize {
    let mut marked = 0usize;
    for &(class_name, method_name, descriptor) in DIRECT_CALL_HELPER_NATIVES.iter() {
        if let Some(id) = registry.resolve_id(class_name, method_name, descriptor) {
            registry.mark_invocations_incomplete(id);
            marked += 1;
        }
    }
    marked
}

thread_local! {
    pub static CONCURRENT_HASHMAP_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
    /// `(vm_key, class_id)` of the EXACT `java/util/HashMap` class, learned
    /// on the first direct-call fallback resolution — same pattern as
    /// `MATCHER_CLASS_CACHE`/`INTEGER_WRAPPER_CLASS_CACHE`.
    pub static HASHMAP_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Shared receiver/key screening for the two HashMap thin helpers. Returns
/// the validated `(thread-independent)` pieces or `None` → caller falls back
/// to the full dispatcher. The receiver must be EXACTLY `java/util/HashMap`
/// (a LinkedHashMap receiver at a HashMap-declared site takes the fallback,
/// which performs real virtual dispatch), and no class redefine may be in
/// flight (the overlay + class-id cache assume stable identity).
///
/// SAFETY: `receiver` must be non-null and 8-aligned below 2^48 (checked by
/// callers before the raw header read).
pub unsafe fn jit_hashmap_receiver_is_exact(vm: &SharedVm, receiver: i64) -> bool {
    let cid = std::ptr::read(receiver as usize as *const u32);
    let vm_key = vm.vm_identity;
    if HASHMAP_CLASS_CACHE.with(|c| c.get() == Some((vm_key, cid))) {
        return !class_was_redefined(vm, ClassId::new(cid));
    }
    let is_exact = vm
        .classes
        .class_manager
        .read()
        .get_class(ClassId::new(cid))
        .map(|class| class.name.as_ref() == "java/util/HashMap")
        .unwrap_or(false);
    if is_exact {
        HASHMAP_CLASS_CACHE.with(|c| c.set(Some((vm_key, cid))));
        return !class_was_redefined(vm, ClassId::new(cid));
    }
    false
}

// SAFETY: `receiver` must be a non-null pointer to a live object header, and
// `vm` must be live. The first statement reads the class id straight out of
// header+0 with `ptr::read`, so a null or stale receiver is undefined
// behaviour. Callers are JIT fast paths that have already null-checked the
// receiver at the dispatch site.
pub unsafe fn jit_concurrent_hashmap_receiver_is_exact(vm: &SharedVm, receiver: i64) -> bool {
    let cid = std::ptr::read(receiver as usize as *const u32);
    let vm_key = vm.vm_identity;
    if CONCURRENT_HASHMAP_CLASS_CACHE.with(|c| c.get() == Some((vm_key, cid))) {
        return !class_was_redefined(vm, ClassId::new(cid));
    }
    let exact = vm
        .classes
        .class_manager
        .read()
        .get_class(ClassId::new(cid))
        .map(|class| class.name.as_ref() == "java/util/concurrent/ConcurrentHashMap")
        .unwrap_or(false);
    if exact {
        CONCURRENT_HASHMAP_CLASS_CACHE.with(|c| c.set(Some((vm_key, cid))));
    }
    exact && !class_was_redefined(vm, ClassId::new(cid))
}

/// Guarded direct path for `ConcurrentMap.get(Object)` when the runtime
/// receiver is exactly ConcurrentHashMap. All other receivers retain the
/// canonical interface dispatcher.
///
/// # H7-1: one entry, one wrapper, one fallback
///
/// Two things in this body used to differ from its own sibling
/// [`jit_hashmap_get_direct`], for no stated reason, and both differences were
/// invisible to every arm because nothing runs a map lookup cold and warm and
/// diffs the two answers (`docs/known-issues/jdk-only/H7-1-*`):
///
/// 1. **An unrecognised key address answered `null` instead of falling back.**
///    The old `else { return 0 }` below turned "this argument is not an object
///    I can find in the heap" into the Java-visible answer `null`. Interpreted,
///    the identical call goes through the full dispatcher — so the same source
///    line could answer differently once its caller tiered up. The sibling
///    already took the other branch (`None => break 'fast`) and says why:
///    the arena-membership probe "is the only check between a stale/fabricated
///    key argument and that dereference". It is now `break 'fast` here too, so
///    the two helpers give one answer to one question. This is also the shape
///    `chm-get-misses-stored-key-in-process-RETIRED-20260804.md` records:
///    reporting ABSENT for a pair that was never compared.
/// 2. **`native_chm_get` was called with no funnel at all.** The comment that
///    stood here argued the wrapper was "second, redundant" because the native
///    pins its own receiver and key. Pinning is a *service of the funnel* —
///    `pin_native_root` writes into the ring `safe_native_call_impl` sets a
///    watermark on and truncates — and the funnel also supplies the
///    `NativeRunning` transition (which is what makes the STW census WAIT for a
///    native holding raw `ObjectRef`s in Rust locals), the argument-forwarding
///    barrier, the JNI pending-exception drain and `catch_unwind`.
///    `native_chm_get` is not a leaf by any of `set_leaf`'s four criteria: it
///    reaches `chm_key_hash`, which dispatches the key's real `hashCode()`.
///    Its sibling `native_chm_contains_key` carries a comment about exactly
///    this hazard being caught live by `RMapGcStress`. So the call now goes
///    through `safe_native_call_prevalidated_objects`, which is what the
///    sibling helper already used for its own non-leaf fallback
///    (`native_hashmap_get_exact`) three hundred lines below.
///
/// **Cost of (2), stated rather than measured** — no binary carrying this has
/// been built. It adds one native funnel entry per compiled `ConcurrentMap.get`
/// that reaches the native. The in-tree figure for the funnel-plus-by-name
/// round trip is ~400 ns/call
/// (`native-call-funnel-per-call-floor-item2-20260805.md`); this restores only
/// the funnel half, since the callback is still resolved at compile time. The
/// native it fronts is itself measured at 6920 cycles/call on the path that
/// misses its string-node cache (see `native_chm_get`'s own comment in
/// `native-collections/src/lib.rs`), so the wrapper is a small addition to a
/// large number — and the wrapper-free variant only ever appeared on the JIT
/// side, meaning every interpreted `ConcurrentHashMap.get` in this VM has
/// always paid it.
pub(in crate::jit) unsafe extern "C" fn jit_concurrent_hashmap_get_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
) -> i64 {
    let f = || jit_concurrent_hashmap_get_direct_body(vm_ptr, receiver, key);
    let bits = contain(
        "jit_concurrent_hashmap_get_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

const SCOPED_COPY_MEMORY_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "copyMemoryInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V",
    num_jit_args: 8,
    return_type: b'V',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_COPY_MEMORY0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "copyMemory0",
    descriptor: "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

#[inline(always)]
unsafe fn try_fast_off_heap_copy(
    src_base: i64,
    src_offset: i64,
    dst_base: i64,
    dst_offset: i64,
    bytes: i64,
) -> bool {
    if src_base != 0 || dst_base != 0 || bytes < 0 || bytes > (256 * 1024 * 1024) {
        return false;
    }
    if bytes == 0 {
        return true;
    }
    if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(src_offset)
        || cratonvm_native_builtins::unsafe_arena_addr_is_tagged(dst_offset)
        || src_offset == 0
        || dst_offset == 0
    {
        return false;
    }
    std::ptr::copy(
        src_offset as usize as *const u8,
        dst_offset as usize as *mut u8,
        bytes as usize,
    );
    true
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_copy_memory_direct(
    vm_ptr: i64,
    receiver: i64,
    src_session: i64,
    dst_session: i64,
    src_base: i64,
    src_offset: i64,
    dst_base: i64,
    dst_offset: i64,
    bytes: i64,
) -> i64 {
    contain(
        "jit_scoped_memory_copy_memory_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            if receiver != 0
                && src_session == 0
                && dst_session == 0
                && try_fast_off_heap_copy(src_base, src_offset, dst_base, dst_offset, bytes)
            {
                return 0;
            }
            let args = [
                receiver,
                src_session,
                dst_session,
                src_base,
                src_offset,
                dst_base,
                dst_offset,
                bytes,
            ];
            let bits = jit_invoke_dispatch(
                vm_ptr,
                &SCOPED_COPY_MEMORY_INTERNAL_INFO as *const _ as i64,
                args.as_ptr() as i64,
                8,
            );
            native_return_handed_to_compiled_code();
            bits
        },
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_copy_memory_direct(
    vm_ptr: i64,
    receiver: i64,
    src_base: i64,
    src_offset: i64,
    dst_base: i64,
    dst_offset: i64,
    bytes: i64,
) -> i64 {
    contain(
        "jit_unsafe_copy_memory_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            if receiver != 0
                && try_fast_off_heap_copy(src_base, src_offset, dst_base, dst_offset, bytes)
            {
                return 0;
            }
            let args = [receiver, src_base, src_offset, dst_base, dst_offset, bytes];
            let bits = jit_invoke_dispatch(
                vm_ptr,
                &UNSAFE_COPY_MEMORY0_INFO as *const _ as i64,
                args.as_ptr() as i64,
                6,
            );
            native_return_handed_to_compiled_code();
            bits
        },
    )
}
