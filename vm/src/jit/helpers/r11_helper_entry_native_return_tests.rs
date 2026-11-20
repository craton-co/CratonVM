//! r11 (2026-09-25): a leftover native return must not stay a GC root
//! across the collections a compiled loop's later helper calls run. See
//! [`drain_native_return_at_compiled_helper_entry`] and
//! `vm/tests/jit_alloc_oome_clears_soft_refs.rs` for the end-to-end probe.
use super::*;
use crate::threading::ThreadId;

/// Through the real door: `jit_safepoint_flush_satb`, which every
/// GC-capable helper calls first. `vm_ptr == 0` skips only the SATB half.
#[test]
fn a_compiled_helper_entry_drops_a_leftover_native_return() {
    let mut thread = JvmThread::new(ThreadId(0), "r11-helper-entry");
    // SAFETY: non-null and 8-byte aligned, which is the whole of
    // `ObjectRef::from_raw`'s contract; the drain never dereferences it.
    let leftover = unsafe { ObjectRef::from_raw(0x10_0000usize as *mut u8) };
    thread.native_pending_return = Some(leftover);
    let scope = set_jit_thread(&mut thread);
    // SAFETY: the JIT thread pointer is installed for this OS thread and
    // no `JitThreadGuard` is live, the contract a helper body's first
    // line runs under.
    unsafe { jit_safepoint_flush_satb(0) };
    restore_jit_thread(scope);
    assert!(
        thread.native_pending_return.is_none(),
        "a helper entered from compiled code kept the previous native \
             return rooted"
    );
}

/// The body of the first `fn` whose signature starts with `sig` in `src`,
/// up to the first line that is a lone closing brace.
fn body<'a>(src: &'a str, sig: &str) -> &'a str {
    // Anchored at a line start: other tests quote these signatures in
    // string literals, and the first bare match can be one of those.
    let Some(start) = src
        .find(&format!("\n{sig}"))
        .or_else(|| src.find(&format!("\npub {sig}")))
    else {
        panic!("`{sig}` not found");
    };
    let rest = &src[start + 1..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

/// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`): the cooperative poll's
/// slow path lowers this thread's handshake arm before `safepoint_check`
/// drains the handshake queue, and outside the JIT-thread branch (a poll
/// with no JIT thread must not come back at every back edge).
#[test]
fn t1e_the_poll_slow_path_lowers_the_handshake_arm_before_the_drain() {
    let helpers = crate::jit::helpers::HELPERS_SRC;
    let b = body(helpers, "unsafe fn jit_safepoint_slow_path_body(");
    let clear = b
        .find("gc_barrier::take_this_threads_handshake_arm();")
        .expect("the clear");
    let thread = b
        .find("if let Some((thread, _guard)) = jit_thread_mut() {\n        let hit")
        .expect("the JIT-thread branch");
    let drain = b
        .find("crate::runtime::interpreter::safepoint_check(vm.as_ref(), thread);")
        .expect("the drain");
    assert!(clear < thread && thread < drain);
}

/// Source witness. The drain rides the SATB flush, so it reaches every
/// helper that flushes; the doors that matter for the soft-reference rule
/// are pinned by name, plus the cooperative poll, which parks for a PEER's
/// collection without flushing. Losing any of them re-opens the leak
/// silently: nothing crashes, a dropped object just stays reachable.
#[test]
fn every_gc_capable_compiled_door_drains_the_leftover_native_return() {
    let helpers = crate::jit::helpers::HELPERS_SRC;
    assert!(
        body(helpers, "unsafe fn jit_safepoint_flush_satb(")
            .contains("drain_native_return_at_compiled_helper_entry();"),
        "the SATB flush door no longer drains native_pending_return"
    );
    assert!(
        body(helpers, "unsafe fn jit_safepoint_slow_path_body(")
            .contains("drain_native_return_at_compiled_helper_entry();"),
        "the cooperative safepoint poll publishes native_pending_return undrained"
    );
    assert!(body(helpers, "unsafe fn jit_invoke_virtual_mic_body(")
        .contains("jit_invoke_virtual_mic_body_inner("));
    assert!(body(helpers, "unsafe fn jit_invoke_dispatch_body(")
        .contains("jit_invoke_dispatch_body_inner("));
    for sig in [
        "unsafe fn jit_newarray_body(",
        "unsafe fn jit_new_object_body(",
        "unsafe fn jit_anewarray_object_body(",
        "unsafe fn jit_multianewarray_n_body(",
        "unsafe fn jit_invoke_dispatch_body_inner(",
        "unsafe fn jit_invoke_virtual_mic_body_inner(",
    ] {
        assert!(
            body(helpers, sig).contains("jit_safepoint_flush_satb(vm_ptr);"),
            "`{sig}` can collect without passing the helper-entry door"
        );
    }
}
