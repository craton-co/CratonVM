#[allow(unused_imports)]
use super::super::*;

thread_local! {
    /// Per-thread override for [`selfrec_direct_enabled`], for tests that must
    /// exercise the self-recursive direct-call path without mutating a
    /// process-global env var (which would race parallel test threads). `None`
    /// ⇒ fall back to the env var. Production never sets this.
    pub(crate) static SELFREC_DIRECT_TEST_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test hook: force the self-recursive direct-call path on (`Some(true)`) / off
/// (`Some(false)`) for the CURRENT thread, or restore env behaviour (`None`).
/// Thread-local so parallel tests don't race. Not part of the stable API.
#[doc(hidden)]
pub fn __set_selfrec_direct_override(v: Option<bool>) {
    SELFREC_DIRECT_TEST_OVERRIDE.with(|c| c.set(v));
}

/// Whether guarded IR self-recursion uses a direct call to the method's own
/// entry. Default-on now that the IR lowering has the same native-stack floor
/// guard as single-pass. `CRATONVM_JIT_IR_SELFREC_DIRECT=0` opts out for
/// diagnosis; a thread-local override takes precedence for tests.
pub(crate) fn selfrec_direct_enabled() -> bool {
    if let Some(v) = SELFREC_DIRECT_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
        Err(_) => true,
    }
}

/// `CRATONVM_JIT_IR_SELFREC_DIRECT_VOID=1` -- JIT round 19 wave 3, lane
/// throws3 (THROWS3-1 of `docs/feature-designs/jit-ir-optimizations.md`).
/// **Default OFF.** Read once per compile, only when
/// [`selfrec_direct_enabled`] is on.
///
/// A self-recursive static VOID method's recursive `invokestatic` was the one
/// self-call shape the optimizing tier left on `jit_invoke_dispatch`: the
/// direct route (`invoke_kind == 4`, `ir_lower::emit_self_recursive_call`) was
/// admitted for every other return type, and a wide return that cannot take
/// it even bails the method to single-pass because the dispatch route measured
/// 8.6x slower (`fib44`). Void had no such exit, so a C2 body of `void
/// walk(Node n) { .. walk(child) .. }` paid the dispatch helper on every
/// recursion where its C1 body made a raw self-CALL -- the optimizing tier
/// was the slower one. Measured on `R18ThrowsShapes pre-up-eight` (host,
/// `CRATONVM_DBG_JIT_DISPATCH=1`): `throwPreDown(I)V`'s C2 body served every
/// one of its seven recursive calls per throw from the dispatch cache.
///
/// Why the direct route serves a void call unchanged: a void call node has a
/// result slot like any other (`alloc_slot` runs for every `Op::Call`, and the
/// generic route's `emit_call_return_check` stores RAX into it for a void
/// callee too), and a void compiled body returns with RAX zeroed on its normal
/// path (`Op::Return`'s `XOR EAX, EAX`), so the sentinel compare reads exactly
/// what the dispatch route's caller reads. Nothing else on the route is typed:
/// the entry fast return / entry fold / caller-side answer need a returned
/// `int` and decline a void body, and the frame-state and liveness rules key on
/// the node's type (`Int`/`Long` only).
///
/// Default ON since the round-19 proposal round (R19Throws3Shapes `new-up-eight`
/// 3789 -> 2577 ns, `pre-up-eight` 2556 -> 1464 ns); `=0` keeps the dispatch helper.
pub(crate) fn selfrec_direct_void_enabled() -> bool {
    true
}

/// Does a VOID self-recursive static call take the direct route? Only with
/// the switch ([`selfrec_direct_void_enabled`]) AND the compile's loader
/// identity proof (`CompileRequest::self_call_identity_stable`) -- the proof
/// the single-pass tier asks before it binds any static self-call raw. The
/// non-void kinds predate that proof in this tier and keep their rule; the new
/// shape takes the stricter of the two.
pub(crate) fn selfrec_direct_void_admits(switch_on: bool, self_call_identity_stable: bool) -> bool {
    switch_on && self_call_identity_stable
}

/// `CRATONVM_JIT_IR_CONTEXT_ABI_OF_INSTALLED` for the VM's optimizing door,
/// which fills `CompileRequest::installed_body_needs_context` only under it
/// (one JIT-cache probe per optimizing compile). t1 wave 10, lane LCAP2.
pub fn ir_context_abi_of_installed_enabled() -> bool {
    r25_w10_2_steady_c1::context_abi_of_installed_enabled()
}

