//! gen r4w5/oomjit5: a leftover native return must not outlive an
//! exception that compiled code catches. See
//! [`drain_native_return_at_compiled_catch`].
use super::*;
use crate::threading::ThreadId;

#[test]
fn a_compiled_catch_drops_a_leftover_native_return_once() {
    let mut thread = JvmThread::new(ThreadId(0), "oomjit5-catch");
    // SAFETY: non-null and 8-byte aligned, which is the whole of
    // `ObjectRef::from_raw`'s contract; the drain never dereferences it.
    let leftover = unsafe { ObjectRef::from_raw(0x10_0000usize as *mut u8) };
    thread.native_pending_return = Some(leftover);
    assert!(drain_native_return_at_compiled_catch(&mut thread, "test"));
    assert!(
        thread.native_pending_return.is_none(),
        "the handoff root must not survive the catch"
    );
    assert!(
        !drain_native_return_at_compiled_catch(&mut thread, "test"),
        "nothing left to drop the second time"
    );
}

/// The body of the first `fn` whose signature starts with `sig` in `src`,
/// up to the first line that is a lone closing brace.
fn body<'a>(src: &'a str, sig: &str) -> &'a str {
    // At a line start: other text pins quote these signatures in strings.
    let Some(start) = src
        .find(&format!("\n{sig}"))
        .or_else(|| src.find(&format!("\npub {sig}")))
        .map(|i| i + 1)
    else {
        panic!("`{sig}` not found");
    };
    let rest = &src[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

/// Source witness: every door where compiled code, or the interpreter on
/// its behalf, enters a handler for an exception that never reaches the
/// interpreter's own throw path (`settle_thrown_exception`) applies the
/// drain. A door that loses it silently re-opens the leak: there is no
/// crash, only a dropped object that stays reachable.
#[test]
fn every_compiled_catch_door_drains_the_leftover_native_return() {
    let helpers = crate::jit::helpers::HELPERS_SRC;
    let bridge = concat!(include_str!("../../runtime/interpreter/jit_bridge/mod.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/jit_verdicts.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/osr_continuation.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/compile_osr_body.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/osr_exception_exit.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/try_osr.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/native_shadow_arms.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/compile_optimizing_artifact.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/never_run_static_exit_pcs.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/try_jit_compile_callee_slow.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/byte_array_field_declarer_admitted.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/folded_handle_member_row.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/callee_never_taken_rows.rs"), "\n", include_str!("../../runtime/interpreter/jit_bridge/mint_ldc_slots_at_offer.rs"));
    let dispatch = include_str!("../../runtime/interpreter/exception_dispatch.rs");
    let doors: &[(&str, &str)] = &[
        (helpers, "unsafe fn jit_local_handler_lookup_body("),
        (helpers, "unsafe fn local_handler_enter_implicit("),
        (
            bridge,
            "pub(in crate::runtime::interpreter) fn route_osr_exception_out_of_artifact(",
        ),
        (dispatch, "pub(crate) fn run_jit_callee_handler("),
        (
            dispatch,
            "pub(super) fn route_jit_exception_through_method(",
        ),
    ];
    for (src, sig) in doors {
        assert!(
            body(src, sig).contains("drain_native_return_at_compiled_catch("),
            "`{sig}` enters a handler without draining native_pending_return"
        );
    }
}
